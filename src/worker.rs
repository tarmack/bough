//! Live-update worker thread: owns git I/O, parsing, highlighting, and the
//! debounce/coalesce loop. Produces immutable `ArcSnapshot`s on a channel; the
//! UI thread swaps them in between frames. The worker holds the `Highlighter`
//! (syntect is `&mut self` and single-threaded) and a small LRU of recently
//! viewed `(path, ctx)` diffs so rapid `j`-mashing fills the pane instantly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender, TryRecvError};

use crate::diff::Diff;
use crate::git;
use crate::highlight::{Highlighter, ResumeState};
use crate::perf::{self, Counter};
use crate::preview::PreviewImage;
use crate::snapshot::{ArcSnapshot, HighlightLines, Request, Seq, Snapshot};
use crate::tree::Tree;
use crate::watch::{Watcher, WorkerEvent};

/// Debounce floor: minimum quiet period before a recompute fires.
const DEBOUNCE_FLOOR: Duration = Duration::from_millis(150);
/// Adaptive backoff floor, raised after a slow git call or a huge diff.
const DEBOUNCE_CEILING: Duration = Duration::from_millis(400);
/// How long the raised floor stays in effect.
const FLOOR_RAISED_FOR: Duration = Duration::from_secs(10);
/// Factor applied to the last recompute's duration for the duty-cycle floor:
/// under continuous churn the worker rescans at most ~1/3 of the time.
const RECOMPUTE_FLOOR_FACTOR: u32 = 3;
/// Cap on the duty-cycle floor: worst-case staleness stays under half a minute.
const ADAPTIVE_FLOOR_MAX: Duration = Duration::from_secs(30);
/// Never recompute more often than this, even with perfectly spaced events.
const HARD_CEILING: Duration = Duration::from_millis(100);
/// Above this many changed entries the floor raises to the ceiling.
const LARGE_DIFF_THRESHOLD: usize = 2000;
/// Above this git wall time the floor raises to the ceiling.
const SLOW_GIT_THRESHOLD: Duration = Duration::from_millis(200);
/// Maximum number of `(path, ctx)` diffs kept in the worker's LRU.
const CACHE_CAP: usize = 32;
/// Child diffs built per emitted chunk while streaming a directory aggregate.
const DIR_CHUNK: usize = 25;
/// Hard cap on how many bytes of a preview image are read from disk.
const MAX_PREVIEW_BYTES: u64 = 32 * 1024 * 1024;
/// Hard cap on decoded preview pixels, mirrored on the `ImageReader`.
const MAX_PREVIEW_DIM: u32 = 16384;
/// Hard cap on decoder allocation while building a preview.
const MAX_PREVIEW_ALLOC: u64 = 64 * 1024 * 1024;

/// Compile-time proof that the worker's cross-thread types are `Send`. The
/// `Highlighter` is intentionally NOT asserted here: syntect's onig backend
/// holds raw pointers, so it is not `Send`. Instead the worker constructs its
/// `Highlighter` on the worker thread and never moves it across threads —
/// only `Snapshot`/`ArcSnapshot` cross the boundary.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Snapshot>();
    assert_send::<ArcSnapshot>();
};

/// Failure to spawn the worker.
#[derive(Debug)]
pub struct WorkerError {
    /// Human-readable failure reason.
    pub message: String,
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WorkerError {}

/// Handle to a running worker. Dropping it signals shutdown and joins.
pub struct Worker {
    /// UI -> worker request channel.
    pub request_tx: Sender<Request>,
    /// Worker -> UI snapshot channel.
    pub snapshot_rx: Receiver<ArcSnapshot>,
    /// Worker -> UI decoded preview-image channel.
    pub image_rx: Receiver<PreviewImage>,
    /// Watcher's degraded-to-poll flag, for the status line.
    pub poll_mode: Arc<AtomicBool>,
    /// Set before `Shutdown` is queued so a backlogged loop exits promptly.
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawn the worker thread. `initial_theme` selects the syntect theme;
    /// an unknown theme degrades to no-color (the worker still builds structured
    /// diffs). `repo_root` is where `notify` watches.
    pub fn spawn(
        rev: String,
        empty: bool,
        repo_root: PathBuf,
        initial_theme: String,
        initial_branch: git::BranchState,
    ) -> Result<Worker, WorkerError> {
        // One process-lifetime poll-mode flag: the UI clones it once at spawn
        // and reads it every frame, so re-arming the watcher on a worktree
        // switch must reuse this Arc, never mint a new one the UI can't see.
        let poll_flag = Arc::new(AtomicBool::new(false));
        let watcher = match Watcher::start_with_flag(repo_root.clone(), poll_flag.clone()) {
            Ok(w) => w,
            Err(e) => {
                // No watcher: surface via the first snapshot's error and run
                // request-only. poll_mode stays false (watcher is off, not
                // degraded to a workdir poll).
                return spawn_no_watcher(
                    rev,
                    empty,
                    repo_root,
                    initial_theme,
                    initial_branch,
                    e.to_string(),
                    poll_flag,
                );
            }
        };
        let (request_tx, request_rx) = crossbeam_channel::unbounded::<Request>();
        let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded::<ArcSnapshot>();
        let (image_tx, image_rx) = crossbeam_channel::unbounded::<PreviewImage>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_t = shutdown.clone();
        let poll_flag_t = poll_flag.clone();

        let theme = initial_theme;
        let handle = thread::Builder::new()
            .name("bough-worker".into())
            .spawn(move || {
                let highlighter = Highlighter::new(&theme).ok();
                run_loop(
                    rev,
                    empty,
                    highlighter,
                    repo_root,
                    initial_branch,
                    poll_flag_t,
                    watcher,
                    shutdown_t,
                    LoopChannels {
                        request_rx,
                        snapshot_tx,
                        image_tx,
                    },
                );
            })
            .map_err(|e| WorkerError {
                message: format!("failed to spawn worker thread: {e}"),
            })?;

        Ok(Worker {
            request_tx,
            snapshot_rx,
            image_rx,
            poll_mode: poll_flag,
            shutdown,
            handle: Some(handle),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Flag first so a busy loop stops before draining queued requests.
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.request_tx.send(Request::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Degraded spawn path when the watcher failed to start.
fn spawn_no_watcher(
    rev: String,
    empty: bool,
    repo_root: PathBuf,
    theme: String,
    initial_branch: git::BranchState,
    watch_err: String,
    poll_flag: Arc<AtomicBool>,
) -> Result<Worker, WorkerError> {
    let (request_tx, request_rx) = crossbeam_channel::unbounded::<Request>();
    let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded::<ArcSnapshot>();
    let (image_tx, image_rx) = crossbeam_channel::unbounded::<PreviewImage>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_t = shutdown.clone();
    let poll_flag_t = poll_flag.clone();

    let first_error = watch_err.clone();
    let handle = thread::Builder::new()
        .name("bough-worker".into())
        .spawn(move || {
            let highlighter = Highlighter::new(&theme).ok();
            // Emit one initial snapshot carrying the watcher error so the UI
            // status line surfaces it, then serve Diff requests only.
            let mut state =
                WorkerState::new(rev.clone(), empty, highlighter, repo_root, initial_branch);
            state.last_error = Some(first_error);
            state.send_initial(&snapshot_tx);

            // Request-only until a worktree switch successfully arms a
            // watcher; the never-ready receiver keeps the select arm silent
            // until then.
            let mut watch = WatchHandle {
                watcher: None,
                event_rx: crossbeam_channel::never(),
                poll_flag: poll_flag_t,
                switched: false,
            };
            let mut last_event: Option<Instant> = None;
            // Set when the watcher channel closes; the receiver is swapped for
            // a never-ready one so `select!` blocks on requests/timeout instead
            // of spinning on the permanently-ready disconnected arm.
            let mut event_closed = false;

            loop {
                if shutdown_t.load(Ordering::Relaxed) {
                    break;
                }
                crossbeam_channel::select! {
                    recv(watch.event_rx) -> ev => {
                        if ev.is_ok() {
                            last_event = Some(Instant::now());
                        } else {
                            // Watcher closed; stop selecting the
                            // permanently-ready disconnected arm.
                            event_closed = true;
                        }
                    }
                    recv(request_rx) -> msg => {
                        match msg {
                            Ok(req) => {
                                if handle_request(
                                    req,
                                    &mut state,
                                    &snapshot_tx,
                                    &image_tx,
                                    &shutdown_t,
                                    &mut watch,
                                ) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    default(HARD_CEILING) => {}
                }
                if event_closed {
                    watch.event_rx = crossbeam_channel::never();
                    event_closed = false;
                }
                // Minimal debounce for the re-armed watcher path: batch the
                // burst for one floor, then recompute. The main loop's
                // adaptive/ignore gating is an optimisation, not a
                // correctness need, and this path is a degraded fallback.
                if let Some(le) = last_event {
                    if le.elapsed() >= DEBOUNCE_FLOOR {
                        recompute(&mut state, &snapshot_tx, &image_tx, &shutdown_t, true);
                        last_event = None;
                    }
                }
            }
        })
        .map_err(|e| WorkerError {
            message: format!("failed to spawn worker thread: {e}"),
        })?;

    Ok(Worker {
        request_tx,
        snapshot_rx,
        image_rx,
        poll_mode: poll_flag,
        shutdown,
        handle: Some(handle),
    })
}

/// Cached diff plus its sparse viewport-driven highlight, keyed by `(path,
/// ctx)`. Highlight entries are `Some` once the line has been highlighted;
/// `None` otherwise. The UI only ever asks for the visible region, so `hl`
/// stays sparse and each request colors exactly the new viewport.
struct CachedDiff {
    diff: Arc<Diff>,
    hl: Arc<HighlightLines>,
    /// Global line -> (file index, body offset of that file), `None` for
    /// headers.
    body_idx: Vec<Option<(usize, usize)>>,
    files: Vec<FileCache>,
}

/// Body lines between parse-state checkpoints. Smaller = cheaper jumps, larger
/// = less memory; 256 keeps a jump to any line under ~256 parse-only lines
/// while a 100k-line file costs only ~400 checkpoints.
const CKPT_STRIDE: usize = 256;

/// A parse-state checkpoint within one file's body-line stream.
struct Checkpoint {
    /// Body offset (within its file's body-line stream) this state precedes.
    body: usize,
    resume: ResumeState,
}

/// Per-file body-line and checkpoint bookkeeping within a [`CachedDiff`].
struct FileCache {
    /// Global `diff.lines` indices of this file's body lines, in diff order.
    body_lines: Vec<usize>,
    /// Checkpoints at body offsets 0, CKPT_STRIDE, 2*CKPT_STRIDE, ... built
    /// lazily.
    checkpoints: Vec<Checkpoint>,
}

/// Build sparse highlight + per-file body/checkpoint bookkeeping for `diff`.
fn new_cached_diff(diff: Arc<Diff>, highlighter: Option<&Highlighter>) -> CachedDiff {
    let mut body_idx = vec![None; diff.lines.len()];
    let mut files: Vec<FileCache> = diff
        .files
        .iter()
        .map(|_| FileCache {
            body_lines: Vec::new(),
            checkpoints: Vec::new(),
        })
        .collect();
    for (i, line) in diff.lines.iter().enumerate() {
        if line.kind.is_header() {
            continue;
        }
        let file = diff.sections[line.section].file;
        let body = files[file].body_lines.len();
        body_idx[i] = Some((file, body));
        files[file].body_lines.push(i);
    }
    if let Some(h) = highlighter {
        for (fi, fc) in files.iter_mut().enumerate() {
            if let Some(resume) = h.fresh_state(&diff.files[fi].path) {
                fc.checkpoints.push(Checkpoint { body: 0, resume });
            }
        }
    }
    let lines = diff.lines.len();
    CachedDiff {
        diff,
        hl: Arc::new(vec![None; lines]),
        body_idx,
        files,
    }
}

/// Mutable worker-thread state.
struct WorkerState {
    rev: String,
    empty: bool,
    highlighter: Option<Highlighter>,
    /// Repository root the worker resolves preview image paths against.
    repo_root: PathBuf,
    selected_path: Option<String>,
    ctx: u32,
    latest_request_seq: Seq,
    last_tree: Option<Arc<Tree>>,
    /// Last diff/highlight sent, retained across a two-phase recompute so the
    /// busy phase can re-send it without a deep clone.
    last_diff: Option<Arc<Diff>>,
    last_highlight: Arc<HighlightLines>,
    last_error: Option<String>,
    cache: LruCache<(String, u32), CachedDiff>,
    /// When the floor was last raised; `None` when floor is at the baseline.
    floor_raised_at: Option<Instant>,
    /// Wall time of the last `git::changed_files` call, for adaptive floor.
    last_git_wall: Duration,
    /// Wall time of the last completed recompute, driving the duty-cycle floor
    /// while the raised window is active.
    last_recompute: Duration,
    /// Instant the last recompute completed. The single clock behind both the
    /// churn debounce and the hard ceiling, so a `SetRev`-triggered recompute
    /// (which bypasses the debounce) still respects the ceiling.
    last_recompute_at: Option<Instant>,
    /// Cached branch state (ahead/behind vs base, unpushed vs upstream).
    last_branch: git::BranchState,
    /// Full per-file metadata from the last recompute, keyed by path. Lets a
    /// single-file diff reuse the scan instead of re-scanning the repository.
    last_changes: HashMap<String, git::ChangeInfo>,
    /// True once the UI has sent a `LoadImage` request: recomputes then also
    /// refresh the selected image preview when it changed on disk.
    preview_active: bool,
    /// Uncommitted-only `(added, deleted)` totals for branch comparisons —
    /// how much of the view is mid-flight work. `None` in the uncommitted
    /// view (its own totals already are uncommitted-only) or when the extra
    /// scan failed.
    uncommitted_stats: Option<(usize, usize)>,
    /// Cached git exclude chain, used to skip a recompute when every changed
    /// path is ignored. `None` when the repository/excludes are unavailable,
    /// which always falls back to recomputing.
    ignore: Option<IgnoreMatcher>,
    /// `busy` flag of the last snapshot sent, so instrumentation only logs a
    /// `busy` transition rather than every snapshot.
    last_sent_busy: bool,
}

impl WorkerState {
    /// `initial_branch` is resolved by the caller (main) so the startup
    /// rev-walk runs once; later recomputes refresh it in place.
    fn new(
        rev: String,
        empty: bool,
        highlighter: Option<Highlighter>,
        repo_root: PathBuf,
        initial_branch: git::BranchState,
    ) -> Self {
        WorkerState {
            rev,
            empty,
            highlighter,
            repo_root,
            selected_path: None,
            ctx: 3,
            latest_request_seq: 0,
            last_tree: None,
            last_diff: None,
            last_highlight: Arc::new(Vec::new()),
            last_error: None,
            cache: LruCache::new(CACHE_CAP),
            floor_raised_at: None,
            last_git_wall: Duration::ZERO,
            last_recompute: Duration::ZERO,
            last_recompute_at: None,
            last_branch: initial_branch,
            last_changes: HashMap::new(),
            preview_active: false,
            uncommitted_stats: None,
            ignore: IgnoreMatcher::open(),
            last_sent_busy: false,
        }
    }

    /// Send an initial empty-ish snapshot so the UI's first frame isn't blank
    /// while the first recompute is debouncing.
    fn send_initial(&mut self, tx: &Sender<ArcSnapshot>) {
        let tree = self.last_tree.clone().unwrap_or_else(empty_tree);
        let snap = self.snapshot(
            tree,
            None,
            Arc::new(Vec::new()),
            self.last_error.clone(),
            false,
        );
        let _ = tx.send(Arc::new(snap));
    }

    /// Assemble a `Snapshot` from the current worker state plus the caller's
    /// tree/diff/highlight/error specifics.
    fn snapshot(
        &mut self,
        tree: Arc<Tree>,
        selected_diff: Option<Arc<Diff>>,
        selected_highlight: Arc<HighlightLines>,
        error: Option<String>,
        busy: bool,
    ) -> Snapshot {
        Snapshot {
            seq: self.latest_request_seq,
            busy,
            empty: self.empty,
            tree,
            selected_diff,
            selected_highlight,
            error,
            branch_state: self.last_branch.clone(),
            uncommitted_stats: self.uncommitted_stats,
        }
    }
}

/// Cached git exclude chain: decides whether a changed path is git-ignored so
/// an all-ignored watcher burst can skip its recompute. The `Stack` owns its
/// parsed pattern state, so the chain is built once (in [`IgnoreMatcher::open`]
/// / [`IgnoreMatcher::open_at`]) and reused across fires.
pub(crate) struct IgnoreMatcher {
    repo: gix::Repository,
    stack: gix::worktree::Stack,
}

impl IgnoreMatcher {
    /// Build from the repository at the process working directory. `None` when
    /// the repository or its index/excludes are unavailable.
    fn open() -> Option<Self> {
        Self::from_repo(git::open_repo().ok()?)
    }

    /// Build the exclude chain for `repo`. `None` on any failure; the caller
    /// then always recomputes.
    fn from_repo(repo: gix::Repository) -> Option<Self> {
        let index = repo.index_or_empty().ok()?;
        let stack = repo
            .excludes(
                &index,
                None,
                gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
            )
            .ok()?
            .detach();
        Some(IgnoreMatcher { repo, stack })
    }

    /// True only when `rel` is confidently git-ignored. Any lookup error is
    /// treated as not-ignored so the caller recomputes rather than skipping.
    fn is_ignored(&mut self, rel: &Path) -> bool {
        match self.stack.at_path(rel, None, &self.repo.objects) {
            Ok(platform) => platform.is_excluded(),
            Err(_) => false,
        }
    }

    /// True only when `rel` is confidently git-ignored *and* untracked. An
    /// exclude lookup never consults the index, so a tracked file matching an
    /// ignore pattern (force-added, or a pattern added after tracking) must
    /// still trigger a recompute. An unreadable index (`index` is `None`) is
    /// treated as not-skippable.
    fn is_ignored_untracked(&mut self, rel: &Path, index: Option<&gix::worktree::Index>) -> bool {
        self.is_ignored(rel) && matches!(is_tracked(index, rel), Some(false))
    }
}

/// `Some(true)` when `rel` has an index entry, `Some(false)` when the index is
/// readable and has none, `None` when the index can't be read. `None` must
/// never be treated as untracked: an unreadable index cannot prove a file
/// (e.g. a force-added ignored `Cargo.lock`) is absent, and wrongly skipping
/// its recompute would hide a real change.
fn is_tracked(index: Option<&gix::worktree::Index>, rel: &Path) -> Option<bool> {
    let index = index?;
    let path = gix::path::into_bstr(rel);
    Some(index.entry_by_path(path.as_ref()).is_some())
}

/// True when `changed` is non-empty and every path satisfies `is_ignored`. The
/// decision core of [`skip_ignored`], split out so it is testable with an
/// injected matcher.
fn all_ignored<F: FnMut(&Path) -> bool>(changed: &[PathBuf], mut is_ignored: F) -> bool {
    !changed.is_empty() && changed.iter().all(|p| is_ignored(p))
}

/// True when a debounced workdir recompute can be skipped because every changed
/// path is both git-ignored and untracked. Conservative: an empty path list, an
/// unavailable matcher, or any path that is not confidently ignored-untracked
/// returns `false`.
fn skip_ignored(state: &mut WorkerState, changed: &[PathBuf]) -> bool {
    let Some(matcher) = state.ignore.as_mut() else {
        return false;
    };
    // Open the index once for the whole burst (not per path) and fallibly:
    // `None` means it couldn't be read, so nothing is confidently untracked.
    let index = matcher.repo.index().ok();
    all_ignored(changed, |p| matcher.is_ignored_untracked(p, index.as_ref()))
}

/// The channels the worker loop reads from and writes to: UI requests in,
/// snapshots and preview images out. Watcher events live in [`WatchHandle`],
/// which a worktree switch can re-arm.
struct LoopChannels {
    request_rx: Receiver<Request>,
    snapshot_tx: Sender<ArcSnapshot>,
    image_tx: Sender<PreviewImage>,
}

/// The watcher a loop can re-arm on a worktree switch, plus the event
/// receiver its select arm drains and the process-lifetime poll-mode flag.
/// In request-only mode `watcher` starts as `None` with a never-ready
/// receiver; a successful switch arms live updates in place.
struct WatchHandle {
    watcher: Option<Watcher>,
    event_rx: Receiver<WorkerEvent>,
    poll_flag: Arc<AtomicBool>,
    /// One-shot flag: set when a worktree switch completed, consumed (and
    /// cleared) by the owning loop so it can drop the old root's stale
    /// debounce-window locals.
    switched: bool,
}

/// Main loop with watcher events + requests + debounce timer.
#[allow(clippy::too_many_arguments)]
fn run_loop(
    rev: String,
    empty: bool,
    highlighter: Option<Highlighter>,
    repo_root: PathBuf,
    initial_branch: git::BranchState,
    poll_flag: Arc<AtomicBool>,
    watcher: Watcher,
    shutdown: Arc<AtomicBool>,
    channels: LoopChannels,
) {
    let LoopChannels {
        request_rx,
        snapshot_tx,
        image_tx,
    } = channels;
    let event_rx = watcher.receiver().clone();
    let mut watch = WatchHandle {
        watcher: Some(watcher),
        event_rx,
        poll_flag,
        switched: false,
    };
    let mut state = WorkerState::new(rev, empty, highlighter, repo_root, initial_branch);
    state.send_initial(&snapshot_tx);
    // First real snapshot comes from the initial recompute, so startup never
    // blocks the main thread on the git scan (whose cost scales with the total
    // diff size).
    recompute(&mut state, &snapshot_tx, &image_tx, &shutdown, true);

    let mut last_event: Option<Instant> = None;
    // Set when the watcher channel closes; the receiver is swapped for a
    // never-ready one so `select!` blocks on requests/timeout instead of
    // spinning on the permanently-ready disconnected arm.
    let mut event_closed = false;
    // Repo-relative paths from workdir events since the last recompute, used
    // to skip a recompute when every one is git-ignored. Deduped at fire time.
    let mut changed_paths: Vec<PathBuf> = Vec::new();
    // True when a ref moved in the current debounce window; a ref change must
    // always recompute, even alongside only-ignored workdir paths, and it is
    // what makes branch state worth re-walking.
    let mut saw_refs = false;
    // An unattributable burst (watcher pending-overflow collapse) must always
    // recompute: its empty path batch must never be swallowed by the
    // ignore-gate even when the window also collected only-ignored paths.
    let mut unattributable = false;
    let mut last_beat = Instant::now();

    loop {
        if last_beat.elapsed() >= Duration::from_secs(1) {
            perf::beat();
            last_beat = Instant::now();
        }
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let floor = current_floor(&state, now);
        let debounce_due = last_event
            .map(|le| should_fire(le, now, floor))
            .unwrap_or(false);
        let tick = next_tick(last_event, state.last_recompute_at, now, floor);

        let timeout = tick.min(HARD_CEILING);

        select! {
            recv(watch.event_rx) -> ev => match ev {
                Ok(we) => {
                    last_event = Some(Instant::now());
                    match &we {
                        WorkerEvent::WorkdirChanged(paths) => {
                            crate::perf_evt!("watch ev=workdir paths={}", paths.len());
                        }
                        WorkerEvent::RefsChanged => crate::perf_evt!("watch ev=refs"),
                    }
                    match we {
                        WorkerEvent::WorkdirChanged(paths) => {
                            if paths.is_empty() {
                                unattributable = true;
                            }
                            changed_paths.extend(paths);
                        }
                        WorkerEvent::RefsChanged => saw_refs = true,
                    }
                }
                Err(_) => {
                    // Watcher closed; stop watching the never-ready channel.
                    crate::perf_evt!("watch closed");
                    event_closed = true;
                }
            },
            recv(request_rx) -> req => match req {
                Ok(r) => {
                    perf::tally(Counter::RequestsSeen);
                    crate::perf_evt!("request kind={} seq={}", request_kind(&r), request_seq(&r));
                    // Fold any queued requests so a burst (rapid `j`) runs at
                    // most the latest of each kind.
                    let survivors =
                        coalesce(r, &request_rx, &shutdown, &state.rev, &state.repo_root);
                    let mut quit = false;
                    for survivor in survivors {
                        if handle_request(
                            survivor,
                            &mut state,
                            &snapshot_tx,
                            &image_tx,
                            &shutdown,
                            &mut watch,
                        ) {
                            quit = true;
                            break;
                        }
                    }
                    if quit {
                        break;
                    }
                    if watch.switched {
                        // A switch dropped everything keyed by the old root;
                        // these locals still hold old-root paths and flags,
                        // so reset them instead of feeding `skip_ignored`
                        // across roots or forcing a redundant recompute via
                        // a stale `saw_refs`.
                        watch.switched = false;
                        last_event = None;
                        saw_refs = false;
                        unattributable = false;
                        changed_paths.clear();
                    }
                }
                Err(_) => break,
            },
            default(timeout) => {}
        }
        if event_closed {
            watch.event_rx = crossbeam_channel::never();
            event_closed = false;
        }

        let now = Instant::now();
        let quiet_due = debounce_due
            && last_event
                .map(|le| should_fire(le, now, floor))
                .unwrap_or(false);
        if quiet_due || churn_due(last_event, state.last_recompute_at, now, floor) {
            if state
                .last_recompute_at
                .map(|lr| now.duration_since(lr) < HARD_CEILING)
                .unwrap_or(false)
            {
                // Hard ceiling: too soon since the last recompute. Reschedule.
                continue;
            }
            changed_paths.sort();
            changed_paths.dedup();
            // A burst that only touched git-ignored paths cannot change the
            // scan result, so skip the recompute entirely — unless a ref moved
            // in the same window.
            let skipped = !saw_refs && !unattributable && skip_ignored(&mut state, &changed_paths);
            if skipped {
                perf::tally(Counter::IgnoreSkips);
                crate::perf_evt!("ignore skip paths={}", changed_paths.len());
            } else {
                recompute(&mut state, &snapshot_tx, &image_tx, &shutdown, saw_refs);
            }
            // A `.gitignore` edit invalidates the cached exclude chain; rebuild
            // it so a path newly ignored or unignored is judged correctly next.
            if changed_paths
                .iter()
                .any(|p| p.ends_with(Path::new(".gitignore")))
            {
                state.ignore = IgnoreMatcher::open();
            }
            last_event = None;
            saw_refs = false;
            unattributable = false;
            changed_paths.clear();
        }
    }
}

/// Fold a burst of queued requests: the latest `SetRev` dominates everything
/// queued before it; the latest `Diff` survives, a `Highlight` survives only
/// when it targets that Diff (or no Diff is queued), and the latest `LoadImage`
/// survives. A surviving `Diff`/`Highlight`/`LoadImage` is executed after the
/// `SetRev` and in ascending request-seq order. An empty result means
/// everything was superseded.
///
/// `current_rev` is the base revision already in effect and `current_root` the
/// worktree already in effect. A `SetRev` naming the current rev, or a
/// `SwitchWorktree` naming the current root, is a no-op that
/// `handle_request` will not recompute for, so it must not discard requests
/// queued alongside it — otherwise a selection sent with an equal-rev
/// `SetRev` (or an accidental re-pick of the current worktree) would be
/// dropped and never served.
fn coalesce(
    first: Request,
    rx: &Receiver<Request>,
    shutdown: &AtomicBool,
    current_rev: &str,
    current_root: &Path,
) -> Vec<Request> {
    if matches!(first, Request::Shutdown) {
        return vec![Request::Shutdown];
    }
    let mut folded = Folded::default();
    folded.absorb(first, current_rev, current_root);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return vec![Request::Shutdown];
        }
        match rx.try_recv() {
            Ok(Request::Shutdown) => return vec![Request::Shutdown],
            Ok(r) => folded.absorb(r, current_rev, current_root),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    let absorbed = folded.absorbed;
    let out = folded.into_requests();
    if absorbed > out.len() {
        perf::tally(Counter::RequestsCoalescedOut);
        crate::perf_evt!("coalesce absorbed={absorbed} out={}", out.len());
    }
    out
}

/// Accumulator for [`coalesce`]: retains the latest request of each kind,
/// dropping any that a newer rev switch supersedes.
#[derive(Default)]
struct Folded {
    set_rev: Option<String>,
    /// Worktree switch, when one was absorbed: dominates everything queued
    /// before it, including `SetRev` (refs are per-worktree-HEAD here).
    switch: Option<PathBuf>,
    diff: Option<Request>,
    highlight: Option<Request>,
    image: Option<Request>,
    /// Requests absorbed, including the first: the fold's input count.
    absorbed: usize,
}

impl Folded {
    fn absorb(&mut self, r: Request, current_rev: &str, current_root: &Path) {
        self.absorbed += 1;
        match r {
            // A rev switch discards everything queued before it, but requests
            // queued after it survive (a new selection alongside a base switch
            // must still be served once the recompute lands). A switch to the
            // rev already in effect changes nothing, so it must not discard
            // the requests around it.
            Request::SetRev { rev } => {
                let effective = self.set_rev.as_deref().unwrap_or(current_rev);
                if rev != effective {
                    self.diff = None;
                    self.highlight = None;
                    self.image = None;
                }
                self.set_rev = Some(rev);
            }
            Request::SwitchWorktree { root } => {
                // A real switch discards everything queued before it — diffs,
                // highlights, images, and any `SetRev` (the post-switch
                // recompute derives the rev from the new worktree's HEAD) —
                // because all of them are keyed by paths/refs from the old
                // worktree. Requests queued after it survive and execute once
                // the switch lands. A switch to the root already in effect is
                // a no-op for `handle_request`, so it must not discard the
                // requests around it.
                let effective = self.switch.as_deref().unwrap_or(current_root);
                if root != effective {
                    self.set_rev = None;
                    self.diff = None;
                    self.highlight = None;
                    self.image = None;
                }
                self.switch = Some(root);
            }
            Request::Diff { .. } => self.diff = Some(r),
            Request::Highlight { .. } => self.highlight = Some(r),
            Request::LoadImage { .. } => self.image = Some(r),
            // `coalesce` intercepts `Shutdown` before it ever reaches the fold.
            Request::Shutdown => unreachable!("coalesce handles Shutdown"),
        }
    }

    fn into_requests(self) -> Vec<Request> {
        let diff_path = match &self.diff {
            Some(Request::Diff { path, .. }) => Some(path.clone()),
            _ => None,
        };
        let highlight = match self.highlight {
            Some(h) => {
                let keep = match diff_path {
                    Some(p) => matches!(&h, Request::Highlight { path, .. } if *path == p),
                    None => true,
                };
                keep.then_some(h)
            }
            None => None,
        };
        let mut survivors = Vec::new();
        if let Some(d) = self.diff {
            survivors.push(d);
        }
        if let Some(h) = highlight {
            survivors.push(h);
        }
        if let Some(i) = self.image {
            survivors.push(i);
        }
        // `handle_request` gates every kind on the shared `latest_request_seq`,
        // so a higher-seq survivor emitted first would stale-drop a lower-seq
        // one. Emit in ascending seq instead.
        survivors.sort_by_key(request_seq);
        // `absorb` clears any `SetRev` queued before a real switch, so a
        // surviving one was queued after it: the post-switch recompute
        // derives the rev from the new worktree's HEAD, and this `SetRev`
        // re-points it explicitly.
        match self.switch {
            Some(root) => {
                let mut out = Vec::with_capacity(survivors.len() + 2);
                out.push(Request::SwitchWorktree { root });
                if let Some(rev) = self.set_rev {
                    out.push(Request::SetRev { rev });
                }
                out.extend(survivors);
                out
            }
            None => match self.set_rev {
                Some(rev) => {
                    let mut out = Vec::with_capacity(survivors.len() + 1);
                    out.push(Request::SetRev { rev });
                    out.extend(survivors);
                    out
                }
                None => survivors,
            },
        }
    }
}

/// Request sequence number used to order coalesced survivors. Rev switches and
/// shutdown carry no seq and never appear among the ordered survivors.
fn request_seq(r: &Request) -> Seq {
    match r {
        Request::Diff { seq, .. }
        | Request::Highlight { seq, .. }
        | Request::LoadImage { seq, .. } => *seq,
        Request::SetRev { .. } | Request::SwitchWorktree { .. } | Request::Shutdown => 0,
    }
}

/// Short kind label for instrumentation lines.
fn request_kind(r: &Request) -> &'static str {
    match r {
        Request::Diff { .. } => "diff",
        Request::Highlight { .. } => "highlight",
        Request::LoadImage { .. } => "image",
        Request::SetRev { .. } => "setrev",
        Request::SwitchWorktree { .. } => "switch",
        Request::Shutdown => "shutdown",
    }
}

/// Duty-cycle floor for watcher-triggered recomputes. Under continuous churn
/// (an agent writing non-ignored files every few seconds) a pure quiet-period
/// debounce fires back-to-back with no idle time, pegging a core on rescans.
/// Holding the floor at a multiple of the last recompute's duration lets a
/// rescan finish and sit idle before the next, so rescanning occupies at most
/// ~1/3 of wall time. Capped so the view never goes stale longer than
/// [`ADAPTIVE_FLOOR_MAX`]. A recompute fast enough that its duty-cycle value
/// does not exceed `base` leaves the floor unchanged.
fn adaptive_floor(base: Duration, last_recompute: Duration) -> Duration {
    let duty = last_recompute
        .saturating_mul(RECOMPUTE_FLOOR_FACTOR)
        .min(ADAPTIVE_FLOOR_MAX);
    base.max(duty)
}

/// Compute the current debounce floor. The base floor rises to
/// [`DEBOUNCE_CEILING`] for [`FLOOR_RAISED_FOR`] after a slow git call or a
/// huge diff; while that window is active the duty-cycle component from the
/// last recompute's duration is layered on. With no recompute refreshing the
/// window for ten seconds both decay back to [`DEBOUNCE_FLOOR`].
fn current_floor(state: &WorkerState, now: Instant) -> Duration {
    match state.floor_raised_at {
        Some(raised) if now.duration_since(raised) < FLOOR_RAISED_FOR => {
            adaptive_floor(DEBOUNCE_CEILING, state.last_recompute)
        }
        _ => DEBOUNCE_FLOOR,
    }
}

/// True when continuous churn has starved the quiet-period debounce: events
/// keep arriving inside the floor, so the watcher would never reach a quiet
/// window. Fire on elapsed time since the last recompute instead, bounding
/// staleness by the current floor — at the baseline floor too, otherwise a
/// fast repo whose events always arrive inside the quiet period would never
/// recompute. Only the watcher path consults this — user requests bypass the
/// floor entirely.
fn churn_due(
    last_event: Option<Instant>,
    last_recompute: Option<Instant>,
    now: Instant,
    floor: Duration,
) -> bool {
    last_event.is_some()
        && last_recompute
            .map(|lr| now.duration_since(lr) >= floor)
            .unwrap_or(false)
}

/// Next tick length so the debounce deadline wakes the loop.
fn next_tick(
    last_event: Option<Instant>,
    last_recompute: Option<Instant>,
    now: Instant,
    floor: Duration,
) -> Duration {
    let tick = if let Some(le) = last_event {
        let remaining = floor.saturating_sub(now.duration_since(le));
        if let Some(lr) = last_recompute {
            let since_recompute = now.duration_since(lr);
            if since_recompute < HARD_CEILING {
                let ceiling_remaining = HARD_CEILING - since_recompute;
                remaining.min(ceiling_remaining)
            } else {
                remaining
            }
        } else {
            remaining
        }
    } else {
        HARD_CEILING
    };
    // A due-but-ceiling-deferred recompute can yield zero; a zero `default`
    // returns immediately and busy-spins until `HARD_CEILING` elapses.
    tick.max(Duration::from_millis(1))
}

/// True when `now - last_event >= floor`.
fn should_fire(last_event: Instant, now: Instant, floor: Duration) -> bool {
    now.duration_since(last_event) >= floor
}

/// True when an incoming `Request::Diff` seq is stale (older than the latest
/// we've already accepted). Equal seq is not stale (the UI may resend).
fn is_stale(latest: Seq, incoming: Seq) -> bool {
    incoming < latest
}

/// Handle a UI request. Returns `true` if the loop should exit (`Shutdown`).
fn handle_request(
    req: Request,
    state: &mut WorkerState,
    tx: &Sender<ArcSnapshot>,
    image_tx: &Sender<PreviewImage>,
    shutdown: &AtomicBool,
    watch: &mut WatchHandle,
) -> bool {
    // Don't start expensive work once the UI has begun tearing us down.
    if shutdown.load(Ordering::Relaxed) {
        return true;
    }
    match req {
        Request::Shutdown => true,
        Request::SwitchWorktree { root } => {
            if root == state.repo_root {
                // Same-root switch: a no-op here, so it must not have
                // discarded the requests coalesced around it.
                return false;
            }
            if let Err(e) = switch_worktree(state, &root, tx, image_tx, shutdown, watch) {
                // Validation/re-aim failure: surface the error over the
                // current (unchanged) view; `switch_worktree` left every bit
                // of state untouched.
                state.last_error = Some(e);
                send_error_snapshot(state, tx);
            }
            false
        }
        Request::SetRev { rev } => {
            if rev != state.rev {
                state.rev = rev;
                // Diffs and highlights are cached by `(path, ctx)`; they no
                // longer apply once the base changes, so drop them.
                state.cache = LruCache::new(CACHE_CAP);
                state.last_error = None;
                recompute(state, tx, image_tx, shutdown, true);
            }
            false
        }
        Request::Diff { path, ctx, seq } => {
            if is_stale(state.latest_request_seq, seq) {
                return false;
            }
            state.latest_request_seq = seq;
            state.selected_path = Some(path.clone());
            state.ctx = ctx;
            serve_diff(state, &path, ctx, tx, shutdown);
            false
        }
        Request::Highlight {
            path,
            ctx,
            from,
            to,
            seq,
        } => {
            if is_stale(state.latest_request_seq, seq) {
                return false;
            }
            // Only highlight the currently-selected file at the current context;
            // a highlight for a stale selection would clobber the live view.
            if state.selected_path.as_deref() != Some(path.as_str()) || state.ctx != ctx {
                return false;
            }
            state.latest_request_seq = seq;
            fill_highlight_range(state, &path, ctx, from, to, tx);
            false
        }
        Request::LoadImage { path, seq } => {
            if is_stale(state.latest_request_seq, seq) {
                return false;
            }
            state.latest_request_seq = seq;
            state.preview_active = true;
            load_preview(state, &path, image_tx, tx);
            false
        }
    }
}

/// Re-aim the worker at another worktree of the same repository: validate
/// (same-repository identity, then the git layer's own root validation),
/// re-aim the git layer, drop everything keyed by the old worktree's paths,
/// emit a busy placeholder, re-arm the watcher, then run a full
/// debounce-bypassing recompute. `Err(message)` is returned only before any
/// mutation, so the caller can surface the error over the untouched current
/// view.
#[allow(clippy::too_many_arguments)]
fn switch_worktree(
    state: &mut WorkerState,
    root: &Path,
    tx: &Sender<ArcSnapshot>,
    image_tx: &Sender<PreviewImage>,
    shutdown: &AtomicBool,
    watch: &mut WatchHandle,
) -> Result<(), String> {
    // Same-repository identity first: a switch stays within the current
    // repository's worktree set (`Request::SwitchWorktree` promises "same
    // repository, different workdir"). Both sides resolve to their worktree
    // set's shared common dir; canonicalize each so symlinked paths can't
    // fake a mismatch, falling back to the raw paths if either side fails.
    let current = git::open_repo_at(&state.repo_root).map_err(|e| e.to_string())?;
    let target = git::open_repo_at(root).map_err(|e| e.to_string())?;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(current.common_dir()) != canon(target.common_dir()) {
        return Err(format!(
            "worktree belongs to a different repository: {}",
            root.display()
        ));
    }
    // Validate and re-aim the git layer in one step; its messages cover both
    // "not a git repository" and "not inside a checked-out worktree"
    // (`gix::discover` otherwise happily accepts a *parent* repository for a
    // stray directory inside one).
    git::set_repo_root(root.to_path_buf()).map_err(|e| e.to_string())?;

    // Paths collide across worktrees, so stale diffs are the correctness
    // risk: nothing keyed by old-worktree paths may survive the switch. The
    // rev label stays (it re-resolves against the new worktree's HEAD), the
    // branch state recomputes.
    state.cache = LruCache::new(CACHE_CAP);
    state.last_changes = HashMap::new();
    state.last_diff = None;
    state.last_highlight = Arc::new(Vec::new());
    state.last_tree = None;
    state.last_branch = git::BranchState::default();
    state.last_error = None;
    state.selected_path = None;
    state.ctx = 3;
    state.preview_active = false;
    state.uncommitted_stats = None;
    // The old worktree's raised floor must not suppress the new root's
    // watcher-driven recomputes for the raised window's duration.
    state.floor_raised_at = None;
    // The exclude chain was parsed from the old worktree's repo; rebuild it.
    state.ignore = IgnoreMatcher::open();
    state.repo_root = root.to_path_buf();
    // Switch committed: the loop must drop the old root's debounce-window
    // locals (paths and flags that no longer apply).
    watch.switched = true;

    // Busy placeholder with an empty tree, so the UI never flashes the old
    // worktree's tree (or a stale diff) while the new scan runs. Sent before
    // the watcher drop/re-arm below: `Watcher::drop` can block up to its
    // timeout on a wedged watch thread, and this placeholder is the UI's
    // only immediate switch feedback.
    send_snapshot(
        state,
        tx,
        empty_tree(),
        None,
        Arc::new(Vec::new()),
        None,
        true,
    );

    // Re-arm the watcher on the new root, resetting the shared poll flag so
    // the UI's process-lifetime Arc reflects the new watcher, not the old
    // root's degradation. Drop the old watcher first: its thread shuts down
    // with a bounded timeout here on the worker thread, acceptable at switch
    // time. In request-only mode this (re)arms live updates; a failed start
    // continues request-only, mirroring the spawn-time degradation (a missing
    // watcher is not an error the user can fix, so it is logged, not
    // surfaced over the freshly recomputed view).
    drop(watch.watcher.take());
    match Watcher::start_with_flag(root.to_path_buf(), watch.poll_flag.clone()) {
        Ok(w) => {
            watch.event_rx = w.receiver().clone();
            watch.watcher = Some(w);
        }
        Err(e) => {
            watch.watcher = None;
            watch.event_rx = crossbeam_channel::never();
            crate::perf_evt!("switch watcher re-arm failed err={e}");
        }
    }

    // Full recompute, bypassing the debounce. `refs_moved` re-derives branch
    // state from the new worktree's HEAD.
    recompute(state, tx, image_tx, shutdown, true);
    Ok(())
}

/// Read and decode the image at `path` (relative to the repo root) and send the
/// finished preview on `tx`. A read/decode failure is surfaced in the status
/// line via a snapshot that keeps the cached diff for `path` intact, so the
/// pane never blanks on a bad image.
fn load_preview(
    state: &mut WorkerState,
    path: &str,
    tx: &Sender<PreviewImage>,
    snapshot_tx: &Sender<ArcSnapshot>,
) {
    let result = read_preview_bytes(&state.repo_root, path)
        .and_then(|bytes| build_preview_bytes(&bytes, path.to_string()));
    match result {
        Ok(img) => {
            let _ = tx.send(img);
        }
        Err(e) => {
            state.last_error = Some(format!("preview {path}: {e}"));
            let key = (path.to_string(), state.ctx);
            let cached = state
                .cache
                .get(&key)
                .map(|entry| (entry.diff.clone(), entry.hl.clone()));
            match cached {
                Some((diff, hl)) => send_diff_snapshot(state, Some(diff), hl, snapshot_tx),
                None => send_error_snapshot(state, snapshot_tx),
            }
        }
    }
}

/// Read a preview image from disk, rejecting symlinks/non-regular files and
/// capping the read so a device file (e.g. `/dev/zero`) can't hang or OOM the
/// worker.
///
/// `path` is repo-derived and untrusted: absolute paths and lexical escapes
/// (`..`, root, prefix components) are rejected before joining so `path`
/// cannot read outside `repo_root`. Residual risk: an intermediate directory
/// symlink inside the repo is not resolved away, so a symlinked directory can
/// still redirect the final component outside the root; full canonicalization
/// is deliberately not done here.
fn read_preview_bytes(
    repo_root: &Path,
    path: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err("preview path escapes the repository".into());
    }
    let full = repo_root.join(rel);
    let meta = std::fs::symlink_metadata(&full)?;
    if !meta.file_type().is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_PREVIEW_BYTES {
        return Err(format!("{} bytes exceeds cap", meta.len()).into());
    }
    let file = std::fs::File::open(&full)?;
    let mut bytes = Vec::new();
    file.take(MAX_PREVIEW_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Decode image bytes and re-encode them as a PNG preview, downscaling when the
/// long edge exceeds 1024px to keep the escape payload small.
pub(crate) fn build_preview_bytes(
    bytes: &[u8],
    path: String,
) -> Result<PreviewImage, Box<dyn std::error::Error + Send + Sync>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PREVIEW_DIM);
    limits.max_image_height = Some(MAX_PREVIEW_DIM);
    limits.max_alloc = Some(MAX_PREVIEW_ALLOC);
    reader.limits(limits);
    let img = reader.decode()?;
    let rgba = if img.width().max(img.height()) > 1024 {
        let long = img.width().max(img.height()) as f64;
        let scale = 1024.0 / long;
        let w = ((img.width() as f64 * scale).round() as u32).max(1);
        let h = ((img.height() as f64 * scale).round() as u32).max(1);
        image::imageops::resize(&img.to_rgba8(), w, h, image::imageops::FilterType::Triangle)
    } else {
        img.to_rgba8()
    };
    let (width, height) = rgba.dimensions();
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(PreviewImage {
        path,
        png: out,
        width,
        height,
    })
}

/// A `(status, path)` listing entry.
type Entry = (char, String);
/// Per-path `(added, deleted)` line stats.
type LineStats = HashMap<String, (usize, usize)>;

/// Scan the working tree into `(status, path)` entries plus per-path
/// `(added, deleted)` stats. Empty repos read the untracked listing; otherwise
/// the full change set is read and cached for per-file diff reuse.
fn scan_entries(
    state: &mut WorkerState,
    shutdown: &AtomicBool,
) -> Result<(Vec<Entry>, LineStats), String> {
    if shutdown.load(Ordering::Relaxed) {
        return Err("shutdown".into());
    }
    perf::tally(Counter::Scans);
    if state.empty {
        let entries = git::initial_listing().map_err(|e| e.to_string())?;
        let paths: Vec<String> = entries.iter().map(|(_, p)| p.clone()).collect();
        let stats = git::initial_stats(&paths);
        // Cache the listing so `is_directory` and directory aggregates work in
        // an empty repo too (every entry is a pure addition).
        state.last_changes = entries
            .iter()
            .map(|(_, p)| {
                let (added, deleted) = stats.get(p).copied().unwrap_or((0, 0));
                (
                    p.clone(),
                    git::ChangeInfo {
                        status: crate::diff::FileStatus::Added,
                        path: p.clone(),
                        old_path: None,
                        rename_percent: None,
                        mode_old: None,
                        mode_new: None,
                        added,
                        deleted,
                        binary: false,
                    },
                )
            })
            .collect();
        Ok((entries, stats))
    } else {
        let changes = git::all_changes(&state.rev).map_err(|e| e.to_string())?;
        state.last_changes = changes
            .iter()
            .map(|c| (c.path.clone(), c.clone()))
            .collect();
        let entries: Vec<(char, String)> = changes
            .iter()
            .map(|c| (c.status.as_char(), c.path.clone()))
            .collect();
        let stats: HashMap<_, _> = changes
            .iter()
            .map(|c| (c.path.clone(), (c.added, c.deleted)))
            .collect();
        Ok((entries, stats))
    }
}

/// Full recompute: changed-files, tree, stats, selected file's diff.
///
/// Runs one scan of the working tree and derives the entries, per-file stats,
/// and per-file metadata (for the diff header) from it, so nothing is rescanned
/// on file selection. The uncommitted-only scan always runs (every watcher
/// fire carries at least one event, and rev switches need it too);
/// `refs_moved` controls whether branch state (which only refs can change) is
/// re-walked.
fn recompute(
    state: &mut WorkerState,
    tx: &Sender<ArcSnapshot>,
    image_tx: &Sender<PreviewImage>,
    shutdown: &AtomicBool,
    refs_moved: bool,
) {
    let t0 = Instant::now();
    perf::tally(Counter::Recomputes);
    let mut span = perf::span("recompute");
    let (entries, stats) = {
        let _scan = perf::span("git_scan");
        match scan_entries(state, shutdown) {
            Ok(x) => x,
            Err(err) => {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                state.last_error = Some(err);
                send_error_snapshot(state, tx);
                return;
            }
        }
    };
    span.detail(format_args!("entries={}", entries.len()));
    recompute_from_scan(
        state, tx, image_tx, shutdown, refs_moved, entries, stats, t0,
    );
    state.last_recompute = t0.elapsed();
    state.last_recompute_at = Some(Instant::now());
}

/// The post-scan half of [`recompute`]: evict, refresh branch stats, build the
/// tree, then stream the two phases (busy tree, settled diff). Split out so the
/// two-phase sequencing is testable without a live repository.
#[allow(clippy::too_many_arguments)]
fn recompute_from_scan(
    state: &mut WorkerState,
    tx: &Sender<ArcSnapshot>,
    image_tx: &Sender<PreviewImage>,
    shutdown: &AtomicBool,
    refs_moved: bool,
    entries: Vec<Entry>,
    stats: LineStats,
    t0: Instant,
) {
    // A watcher recompute may have changed files whose old diffs are cached;
    // evict just those so the pane shows fresh content without discarding
    // highlight progress for every other file.
    evict_changed(state);

    state.last_git_wall = t0.elapsed();
    // Branch state (ahead/behind, unpushed) can only change when refs move, so
    // a workdir-only window reuses the cached value instead of re-walking.
    if refs_moved {
        state.last_branch = git::branch_state(&state.rev);
    }

    // In branch comparisons, how much of the view is uncommitted mid-flight
    // work: a second, uncommitted-only scan. The uncommitted view's own
    // totals already are uncommitted-only, so skip it there. A failure just
    // hides the split from the footer.
    state.uncommitted_stats = if state.rev.contains("...") {
        let _scan = perf::span("uncommitted_scan");
        git::all_changes("HEAD").ok().map(|changes| {
            changes
                .iter()
                .fold((0, 0), |(a, d), c| (a + c.added, d + c.deleted))
        })
    } else {
        None
    };

    // Adaptive floor: slow git or huge diff raises the floor for 10s.
    if state.last_git_wall > SLOW_GIT_THRESHOLD || entries.len() > LARGE_DIFF_THRESHOLD {
        state.floor_raised_at = Some(Instant::now());
        crate::perf_evt!("debounce floor raised entries={}", entries.len());
    }

    let tree = {
        let mut build = perf::span("tree_build");
        let mut tree = Tree::build(entries);
        tree.annotate_stats(&stats);
        build.detail(format_args!("nodes={}", tree.nodes.len()));
        Arc::new(tree)
    };
    state.last_tree = Some(tree.clone());

    if shutdown.load(Ordering::Relaxed) {
        return;
    }

    // Phase 1: busy snapshot carrying the fresh tree and the previous diff
    // (no flicker; the UI skips diff side effects on a shared Arc).
    state.last_error = None;
    let prev_diff = state.last_diff.clone();
    let prev_hl = state.last_highlight.clone();
    send_snapshot(state, tx, tree.clone(), prev_diff, prev_hl, None, true);

    if shutdown.load(Ordering::Relaxed) {
        return;
    }

    // Phase 2: build (or fill) the selected diff, then send the settled
    // snapshot with `busy = false`.
    let rev = state.rev.clone();
    let ctx = state.ctx;
    let (diff, hl) = build_selected_diff(state, &tree, &rev, ctx, tx, shutdown);

    if shutdown.load(Ordering::Relaxed) {
        return;
    }
    state.last_diff = diff.clone();
    state.last_highlight = hl.clone();
    // A `build_selected_diff` failure set `state.last_error`; surface it on the
    // settled snapshot rather than dropping it behind a hard-coded `None`.
    let error = state.last_error.clone();
    send_snapshot(state, tx, tree, diff, hl, error, false);

    // Refresh an image preview when the selected image changed on disk: its
    // binary diff is content-identical, so nothing else in the snapshot
    // signals the edit. Only fires once the UI has opted in via a
    // `LoadImage` request, and only for files the scan reports as changed.
    if state.preview_active {
        if let Some(path) = state.selected_path.clone() {
            if state.last_changes.contains_key(&path) && crate::preview::is_image_path(&path) {
                load_preview(state, &path, image_tx, tx);
            }
        }
    }
}

/// Drop cached diffs for paths that changed in the latest scan, leaving the
/// rest of the cache (and their highlight progress) intact. Directory diffs
/// are evicted when any file beneath them changed.
fn evict_changed(state: &mut WorkerState) {
    if state.empty {
        // Empty-repo mode is small; clear the cache wholesale rather than
        // diffing directory prefixes.
        state.cache.clear();
        return;
    }
    // Precompute the changed set once: the per-entry `format!("{p}/")` below
    // used to run for every cached entry against every changed key.
    let changed: HashSet<&str> = state.last_changes.keys().map(String::as_str).collect();
    state.cache.retain(|(p, _)| {
        if changed.contains(p.as_str()) {
            return false;
        }
        !changed.iter().any(|c| {
            c.strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        })
    });
}

/// An empty tree, used when no scan has produced one yet.
fn empty_tree() -> Arc<Tree> {
    Arc::new(Tree::build(Vec::new()))
}

/// Send a snapshot that carries `error` but preserves the last known tree/diff.
fn send_error_snapshot(state: &mut WorkerState, tx: &Sender<ArcSnapshot>) {
    let tree = state.last_tree.clone().unwrap_or_else(empty_tree);
    let diff = state.last_diff.clone();
    let hl = state.last_highlight.clone();
    let error = state.last_error.clone();
    send_snapshot(state, tx, tree, diff, hl, error, false);
}

/// Assemble and send a snapshot from the caller's tree/diff/highlight/error
/// specifics. All payloads are `Arc`s, so sends share rather than deep-clone.
#[allow(clippy::too_many_arguments)]
fn send_snapshot(
    state: &mut WorkerState,
    tx: &Sender<ArcSnapshot>,
    tree: Arc<Tree>,
    diff: Option<Arc<Diff>>,
    highlight: Arc<HighlightLines>,
    error: Option<String>,
    busy: bool,
) {
    let snap = state.snapshot(tree, diff, highlight, error, busy);
    if busy != state.last_sent_busy {
        crate::perf_evt!("snapshot busy={busy} seq={}", snap.seq);
        state.last_sent_busy = busy;
    }
    perf::tally(Counter::SnapshotsSent);
    let _ = tx.send(Arc::new(snap));
}

/// Parse + highlight the currently-selected file's diff, if it's in the tree.
/// Returns `(diff, highlight)`. On any error, `(None, vec![])`.
fn build_selected_diff(
    state: &mut WorkerState,
    tree: &Arc<Tree>,
    rev: &str,
    ctx: u32,
    tx: &Sender<ArcSnapshot>,
    shutdown: &AtomicBool,
) -> (Option<Arc<Diff>>, Arc<HighlightLines>) {
    let Some(path) = state.selected_path.clone() else {
        return (None, Arc::new(Vec::new()));
    };
    let in_tree = tree.nodes.iter().any(|n| n.path == path);
    if !in_tree {
        return (None, Arc::new(Vec::new()));
    }
    build_diff_for(&path, state, rev, ctx, tree, tx, shutdown)
}

/// Parse one file's diff. Uses the LRU cache when hit; on miss parses fresh
/// and caches it un-highlighted (highlighting is filled progressively via
/// [`fill_highlight_range`] as the UI scrolls). Mutates `state.cache`.
fn build_diff_for(
    path: &str,
    state: &mut WorkerState,
    rev: &str,
    ctx: u32,
    tree: &Arc<Tree>,
    tx: &Sender<ArcSnapshot>,
    shutdown: &AtomicBool,
) -> (Option<Arc<Diff>>, Arc<HighlightLines>) {
    let key = (path.to_string(), ctx);
    let mut span = perf::span("diff_build");
    if let Some((diff, hl)) = state
        .cache
        .get(&key)
        .map(|e| (e.diff.clone(), e.hl.clone()))
    {
        perf::tally(Counter::CacheHits);
        span.detail(format_args!("cache=hit path={path}"));
        // A cached diff is valid content: drop any stale error so the status
        // line clears while browsing already-built files.
        state.last_error = None;
        return (Some(diff), hl);
    }
    perf::tally(Counter::CacheMisses);
    span.detail(format_args!("cache=miss path={path}"));

    let diff = if is_directory(state, path) {
        build_directory_diff(path, state, rev, ctx, tree, tx, shutdown)
    } else {
        build_single_file_diff(path, state, rev, ctx)
    };

    match diff {
        Ok(diff) => {
            perf::tally(Counter::DiffBuilds);
            // No highlighting yet: every line is `None` until the UI nudges the
            // worker to fill the viewport. The diff itself renders instantly.
            let diff = Arc::new(diff);
            let cached = new_cached_diff(diff.clone(), state.highlighter.as_ref());
            let hl = cached.hl.clone();
            state.cache.put(key, cached);
            state.last_error = None;
            (Some(diff), hl)
        }
        Err(e) => {
            if !shutdown.load(Ordering::Relaxed) {
                state.last_error = Some(e.to_string());
            }
            (None, Arc::new(Vec::new()))
        }
    }
}

/// Whether `path` names a directory in the scan: it has no direct entry but at
/// least one changed file beneath it.
fn is_directory(state: &WorkerState, path: &str) -> bool {
    if state.last_changes.contains_key(path) {
        return false;
    }
    let prefix = format!("{path}/");
    state.last_changes.keys().any(|p| p.starts_with(&prefix))
}

/// Metadata for a single changed path: the scan's cached [`ChangeInfo`] when
/// present, else the untracked lookup.
fn meta_for(path: &str, state: &WorkerState) -> Result<git::FileMeta, git::GitError> {
    if let Some(ci) = state.last_changes.get(path) {
        return Ok(ci.clone().into_meta());
    }
    git::file_meta_untracked(path)
}

/// Build a single file's structured diff.
fn build_single_file_diff(
    path: &str,
    state: &WorkerState,
    rev: &str,
    ctx: u32,
) -> Result<Diff, git::GitError> {
    let meta = meta_for(path, state)?;
    git::diff_file(rev, path, &meta, ctx)
}

/// Build the multi-file diff for a selected directory: every changed file
/// beneath it (including new files), in path order, combined under a summary
/// header. Emits a busy snapshot every [`DIR_CHUNK`] children so the pane fills
/// progressively; the caller sends the settled final snapshot.
fn build_directory_diff(
    path: &str,
    state: &mut WorkerState,
    rev: &str,
    ctx: u32,
    tree: &Arc<Tree>,
    tx: &Sender<ArcSnapshot>,
    shutdown: &AtomicBool,
) -> Result<Diff, git::GitError> {
    let prefix = format!("{path}/");
    let mut span = perf::span("dir_build");
    // Owned copies so `state` stays free for the streaming sends below.
    let mut entries: Vec<(String, git::ChangeInfo)> = state
        .last_changes
        .iter()
        .filter(|(p, _)| p.starts_with(&prefix))
        .map(|(p, ci)| (p.clone(), ci.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    span.detail(format_args!("children={}", entries.len()));

    // Open the repo, resolve the base tree, and build one diff platform and
    // LFS matcher up front instead of per child.
    let repo = git::open_repo().map_err(|e| git::GitError::message(e.to_string()))?;
    let mut context = git::DiffContext::new(&repo, rev)?;

    // One aggregate, extended in place with only the newly built children:
    // rebuilding it from every accumulated child per chunk copied each child's
    // lines O(K/25) times. The aggregate is `Arc`-shared with each streamed
    // chunk so the next extension clones it only if a snapshot is still live.
    let mut aggregate = Arc::new(Diff::directory(path));
    let mut pending: Vec<Arc<Diff>> = Vec::with_capacity(DIR_CHUNK);
    let mut built = 0usize;
    for (p, ci) in entries {
        if shutdown.load(Ordering::Relaxed) {
            return Err(git::GitError::message("shutdown"));
        }
        pending.push(Arc::new(context.diff_file(&p, &ci.into_meta(), ctx)?));
        built += 1;
        if built.is_multiple_of(DIR_CHUNK) {
            Arc::make_mut(&mut aggregate).extend_from_children(&pending);
            pending.clear();
            stream_directory_chunk(state, tree, path, &aggregate, tx);
        }
    }
    if !pending.is_empty() {
        Arc::make_mut(&mut aggregate).extend_from_children(&pending);
    }
    Ok(Arc::try_unwrap(aggregate).unwrap_or_else(|a| (*a).clone()))
}

/// Send a busy snapshot carrying the directory diff built so far. No-op when
/// the selection has moved on; the partial chunk is for the live view only.
/// `aggregate` is already complete for the children built so far and is shared,
/// not copied, into the snapshot.
fn stream_directory_chunk(
    state: &mut WorkerState,
    tree: &Arc<Tree>,
    path: &str,
    aggregate: &Arc<Diff>,
    tx: &Sender<ArcSnapshot>,
) {
    if state.selected_path.as_deref() != Some(path) {
        return;
    }
    perf::tally(Counter::DirChunks);
    let partial = aggregate.clone();
    let hl = Arc::new(vec![None; partial.lines.len()]);
    let error = state.last_error.clone();
    send_snapshot(state, tx, tree.clone(), Some(partial), hl, error, true);
}

/// Serve a `Request::Diff`: fetch the (cached or freshly-built) diff and send
/// a snapshot immediately, un-highlighted on first visit. Highlighting then
/// fills in via `Request::Highlight` nudges from the UI, so a larger file's
/// diff appears right away and colors in as it scrolls. Directory aggregates
/// stream busy chunks from [`build_directory_diff`] before this settled send.
fn serve_diff(
    state: &mut WorkerState,
    path: &str,
    ctx: u32,
    tx: &Sender<ArcSnapshot>,
    shutdown: &AtomicBool,
) {
    let rev = state.rev.clone();
    let tree = state.last_tree.clone().unwrap_or_else(empty_tree);
    let (diff, hl) = build_diff_for(path, state, &rev, ctx, &tree, tx, shutdown);
    send_diff_snapshot(state, diff, hl, tx);
}

/// Assemble and send a settled snapshot whose only changed field vs the last
/// full recompute is the selected diff (plus error).
fn send_diff_snapshot(
    state: &mut WorkerState,
    diff: Option<Arc<Diff>>,
    hl: Arc<HighlightLines>,
    tx: &Sender<ArcSnapshot>,
) {
    state.last_diff = diff.clone();
    state.last_highlight = hl.clone();
    let tree = state.last_tree.clone().unwrap_or_else(empty_tree);
    let error = state.last_error.clone();
    send_snapshot(state, tx, tree, diff, hl, error, false);
}

/// Ensure a checkpoint exists at `(body / CKPT_STRIDE) * CKPT_STRIDE`, then
/// parse-only from it to `body`, returning the exact state preceding that body
/// line. Builds checkpoints forward lazily (parse-only, no styling).
/// Returns `None` when the parse bridging fails mid-way: a partial parse
/// state must not be cached as a complete checkpoint (subsequent lines
/// would highlight from a wrong scope).
fn ensure_resume(
    fc: &mut FileCache,
    diff: &Diff,
    body: usize,
    highlighter: &Highlighter,
) -> Option<ResumeState> {
    let base = (body / CKPT_STRIDE) * CKPT_STRIDE;
    // Advance checkpoints until the last one covers `base`.
    loop {
        let last = fc.checkpoints.last()?;
        if last.body >= base {
            break;
        }
        let end = (last.body + CKPT_STRIDE).min(base);
        let mut resume = last.resume.clone();
        let texts: Vec<&str> = fc.body_lines[last.body..end]
            .iter()
            .map(|&i| diff.lines[i].raw_text.as_str())
            .collect();
        if !highlighter.parse_only(&mut resume, &texts) {
            return None;
        }
        fc.checkpoints.push(Checkpoint {
            body: end,
            resume: resume.clone(),
        });
    }
    let ck = fc.checkpoints.iter().rev().find(|c| c.body == base)?;
    let mut resume = ck.resume.clone();
    if base < body {
        let texts: Vec<&str> = fc.body_lines[base..body]
            .iter()
            .map(|&i| diff.lines[i].raw_text.as_str())
            .collect();
        if !highlighter.parse_only(&mut resume, &texts) {
            return None;
        }
    }
    Some(resume)
}

/// Highlight a cached diff's sparse range `from..to` (the visible viewport plus
/// lookahead), resuming from checkpoints so any range is correct in isolation,
/// then re-sends the snapshot. The cost is proportional to the visible region
/// plus at most one CKPT_STRIDE parse-only bridge, independent of file size, so
/// jumps to any part of a large file color in immediately.
fn fill_highlight_range(
    state: &mut WorkerState,
    path: &str,
    ctx: u32,
    from: usize,
    to: usize,
    tx: &Sender<ArcSnapshot>,
) {
    let key = (path.to_string(), ctx);
    let mut span = perf::span("highlight_fill");
    {
        let WorkerState {
            cache, highlighter, ..
        } = &mut *state;
        // Fill-only: a miss means nothing is cached for the current selection
        // (e.g. a directory aggregate still streaming). Never build a diff
        // here, or every streamed chunk would restart the whole build.
        let Some(entry) = cache.get(&key) else {
            return;
        };
        let total = entry.diff.lines.len();
        let to = to.min(total);
        if from >= to {
            return;
        }
        span.detail(format_args!("range={from}..{to}"));

        let CachedDiff {
            diff,
            hl,
            body_idx,
            files,
        } = entry;
        // Copy-on-write: a previously-sent snapshot may still hold this Arc.
        let hl = Arc::make_mut(hl);
        let mut cur_file: Option<usize> = None;
        let mut cur_resume: Option<ResumeState> = None;

        for i in from..to {
            match body_idx[i] {
                None => hl[i] = Some(Vec::new()),
                Some((file, body)) => {
                    let Some(h) = highlighter.as_ref() else {
                        hl[i] = Some(Vec::new());
                        continue;
                    };
                    if cur_file != Some(file) {
                        cur_resume = ensure_resume(&mut files[file], diff, body, h);
                        cur_file = Some(file);
                    }
                    let tokens = match cur_resume.as_mut() {
                        Some(r) => h.highlight(r, &diff.lines[i].raw_text),
                        None => Vec::new(),
                    };
                    hl[i] = Some(tokens);
                }
            }
        }
    }
    let (diff, hl) = match state.cache.get(&key) {
        Some(e) => (e.diff.clone(), e.hl.clone()),
        None => return,
    };
    perf::tally(Counter::HighlightFills);
    send_diff_snapshot(state, Some(diff), hl, tx);
}

/// Hand-rolled LRU keyed by `(path, ctx)`. `get` promotes the hit to the back;
/// `put` evicts the front (oldest) when full.
struct LruCache<K: Eq + Clone, V> {
    entries: VecDeque<(K, V)>,
    cap: usize,
}

impl<K: Eq + Clone, V> LruCache<K, V> {
    fn new(cap: usize) -> Self {
        LruCache {
            entries: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn get(&mut self, key: &K) -> Option<&mut V> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, v) = self.entries.remove(pos).unwrap();
        self.entries.push_back((k, v));
        Some(&mut self.entries.back_mut().unwrap().1)
    }

    fn put(&mut self, key: K, value: V) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == &key) {
            self.entries.remove(pos);
        }
        if self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }

    /// Drop every entry whose key fails `keep`, preserving order and cap.
    fn retain<F: FnMut(&K) -> bool>(&mut self, mut keep: F) {
        self.entries.retain(|(k, _)| keep(k));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::sample_diff;

    #[test]
    fn lru_evicts_oldest() {
        let mut c: LruCache<u32, &str> = LruCache::new(8);
        for i in 0..10 {
            c.put(i, "x");
        }
        assert!(c.get(&0).is_none());
        assert!(c.get(&1).is_none());
        for i in 2..10 {
            assert!(c.get(&i).is_some());
        }
    }

    #[test]
    fn lru_get_moves_to_back() {
        let mut c: LruCache<u32, &str> = LruCache::new(4);
        c.put(1, "a");
        c.put(2, "b");
        c.put(3, "c");
        // Touch 1: now order is [2, 3, 1].
        assert_eq!(c.get(&1).map(|v| *v), Some("a"));
        // Put 4: still fits (len 4).
        c.put(4, "d");
        // Put 5: evicts the front (2, the oldest untouched).
        c.put(5, "e");
        assert!(c.get(&2).is_none());
        assert!(c.get(&1).is_some());
    }

    #[test]
    fn lru_retain_drops_matching_only() {
        let mut c: LruCache<u32, &str> = LruCache::new(8);
        c.put(1, "a");
        c.put(2, "b");
        c.put(3, "c");
        c.retain(|k| k % 2 == 0);
        assert!(c.get(&1).is_none());
        assert!(c.get(&3).is_none());
        assert!(c.get(&2).is_some());
    }

    #[test]
    fn evict_changed_drops_only_changed_paths() {
        let diff = sample_diff(4);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.cache.put(
            ("a.rs".into(), 3u32),
            new_cached_diff(Arc::new(diff.clone()), None),
        );
        state.cache.put(
            ("b.rs".into(), 3u32),
            new_cached_diff(Arc::new(diff.clone()), None),
        );
        state
            .last_changes
            .insert("a.rs".into(), change_info("a.rs"));

        evict_changed(&mut state);

        assert!(state.cache.get(&("a.rs".to_string(), 3)).is_none());
        assert!(state.cache.get(&("b.rs".to_string(), 3)).is_some());
    }

    #[test]
    fn evict_changed_drops_directory_keys_for_child_paths() {
        let diff = sample_diff(4);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.cache.put(
            ("src".into(), 3u32),
            new_cached_diff(Arc::new(diff.clone()), None),
        );
        state.cache.put(
            ("doc".into(), 3u32),
            new_cached_diff(Arc::new(diff.clone()), None),
        );
        state
            .last_changes
            .insert("src/a.rs".into(), change_info("src/a.rs"));

        evict_changed(&mut state);

        assert!(state.cache.get(&("src".to_string(), 3)).is_none());
        assert!(state.cache.get(&("doc".to_string(), 3)).is_some());
    }

    #[test]
    fn evict_changed_clears_cache_in_empty_repo_mode() {
        let diff = sample_diff(4);
        let mut state = WorkerState::new(
            "HEAD".into(),
            true,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.cache.put(
            ("a.rs".into(), 3u32),
            new_cached_diff(Arc::new(diff.clone()), None),
        );

        evict_changed(&mut state);

        assert!(state.cache.get(&("a.rs".to_string(), 3)).is_none());
    }

    fn change_info(path: &str) -> crate::git::ChangeInfo {
        crate::git::ChangeInfo {
            status: crate::diff::FileStatus::Modified,
            path: path.into(),
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            added: 1,
            deleted: 0,
            binary: false,
        }
    }

    #[test]
    fn next_tick_never_returns_zero() {
        let now = Instant::now();
        // A due event with no recent recompute: remaining is zero.
        let last_event = now - Duration::from_millis(200);
        let tick = next_tick(Some(last_event), None, now, Duration::from_millis(100));
        assert_eq!(tick, Duration::from_millis(1));
    }

    #[test]
    fn should_fire_true_after_floor() {
        let last = Instant::now() - Duration::from_millis(200);
        let now = Instant::now();
        assert!(should_fire(last, now, Duration::from_millis(150)));
    }

    #[test]
    fn should_fire_false_before_floor() {
        let last = Instant::now() - Duration::from_millis(50);
        let now = Instant::now();
        assert!(!should_fire(last, now, Duration::from_millis(150)));
    }

    #[test]
    fn adaptive_floor_triples_slow_recompute() {
        assert_eq!(
            adaptive_floor(DEBOUNCE_CEILING, Duration::from_millis(4000)),
            Duration::from_millis(12_000)
        );
    }

    #[test]
    fn adaptive_floor_leaves_fast_recompute_unchanged() {
        // 3 * 100ms does not exceed the raised base, so the floor is unchanged.
        assert_eq!(
            adaptive_floor(DEBOUNCE_CEILING, Duration::from_millis(100)),
            DEBOUNCE_CEILING
        );
    }

    #[test]
    fn adaptive_floor_caps_at_max() {
        // 60s * 3 would be 180s; the cap bounds staleness instead.
        assert_eq!(
            adaptive_floor(DEBOUNCE_CEILING, Duration::from_secs(60)),
            ADAPTIVE_FLOOR_MAX
        );
    }

    #[test]
    fn current_floor_decays_to_baseline() {
        let now = Instant::now();
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.last_recompute = Duration::from_secs(4);
        // No raised window: the adaptive component does not apply.
        assert_eq!(current_floor(&state, now), DEBOUNCE_FLOOR);

        state.floor_raised_at = Some(now);
        assert_eq!(current_floor(&state, now), Duration::from_millis(12_000));

        // Once the quiet window lapses without a recompute, both decay.
        state.floor_raised_at = Some(now - FLOOR_RAISED_FOR - Duration::from_secs(1));
        assert_eq!(current_floor(&state, now), DEBOUNCE_FLOOR);
    }

    #[test]
    fn churn_due_fires_at_baseline_and_raised_floors() {
        let now = Instant::now();
        let recent = now - Duration::from_millis(100);
        let stale = now - Duration::from_secs(13);
        // Baseline floor, events pending and floor elapsed: churn fires too,
        // so a fast repo with no quiet gap still recomputes.
        assert!(churn_due(Some(stale), Some(stale), now, DEBOUNCE_FLOOR));
        // No pending events since the last fire.
        assert!(!churn_due(None, Some(stale), now, DEBOUNCE_CEILING));
        // Events pending, but a recompute happened recently.
        assert!(!churn_due(
            Some(recent),
            Some(recent),
            now,
            DEBOUNCE_CEILING
        ));
        // Events pending, floor elapsed since the last recompute.
        assert!(churn_due(Some(recent), Some(stale), now, DEBOUNCE_CEILING));
    }

    #[test]
    fn set_rev_request_is_not_gated_by_adaptive_floor() {
        // The adaptive floor is a watcher-path concern only: a user's rev
        // switch must recompute immediately regardless of how high the floor
        // is. Force the tallest adaptive floor, then assert `handle_request`
        // still runs a recompute synchronously (the bogus rev fails its scan,
        // so the only observable is a snapshot already queued on return).
        // Not hermetic: the failed scan still opens the process cwd repo.
        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(8);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "no-such-base...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.last_recompute = Duration::from_secs(60);
        state.floor_raised_at = Some(Instant::now());
        let shutdown = AtomicBool::new(false);
        assert_eq!(current_floor(&state, Instant::now()), ADAPTIVE_FLOOR_MAX);

        let quit = handle_request(
            Request::SetRev {
                rev: "also-no-such-base...HEAD".into(),
            },
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            &mut test_watch(),
        );

        assert!(!quit);
        assert_eq!(state.rev, "also-no-such-base...HEAD");
        assert!(
            rx.try_recv().is_ok(),
            "SetRev must recompute without waiting on the adaptive floor"
        );
    }

    #[test]
    fn is_stale_older_is_stale() {
        assert!(is_stale(3, 2));
    }

    #[test]
    fn is_stale_equal_is_not_stale() {
        assert!(!is_stale(3, 3));
    }

    #[test]
    fn is_stale_newer_is_not_stale() {
        assert!(!is_stale(3, 4));
    }

    #[test]
    fn fill_highlight_range_highlights_only_requested_slice() {
        // Theme resolution failure is tolerated: the assertion is about which
        // lines are filled, not about syntect token output.
        let diff = sample_diff(40);
        let (snap_tx, _snap_rx) = crossbeam_channel::bounded::<ArcSnapshot>(16);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            Highlighter::new("Monokai Extended").ok(),
            PathBuf::from("."),
            git::BranchState::default(),
        );
        let key = ("x.rs".to_string(), 3u32);
        state.cache.put(
            key.clone(),
            new_cached_diff(Arc::new(diff.clone()), state.highlighter.as_ref()),
        );
        state.selected_path = Some("x.rs".into());
        state.ctx = 3;

        fill_highlight_range(&mut state, "x.rs", 3, 10, 20, &snap_tx);

        let entry = state.cache.get(&key).unwrap();
        assert!(entry.hl.iter().take(10).all(|e| e.is_none()));
        assert!(entry.hl[10..20].iter().all(|e| e.is_some()));
        assert!(entry.hl.iter().skip(20).all(|e| e.is_none()));
    }

    #[test]
    fn worker_state_uses_passed_branch() {
        // main resolves branch state once at startup and passes it in; the
        // worker must not redo the rev-walk for its first snapshot.
        let branch = git::BranchState {
            name: Some("main".into()),
            ..Default::default()
        };
        let state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            branch,
        );
        assert_eq!(state.last_branch.name.as_deref(), Some("main"));
    }

    #[test]
    fn build_preview_bytes_decodes_and_reencodes() {
        let mut img = image::RgbaImage::new(4, 3);
        for p in img.pixels_mut() {
            *p = image::Rgba([1, 2, 3, 255]);
        }
        let mut src = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut src), image::ImageFormat::Png)
            .unwrap();

        let pv = build_preview_bytes(&src, "a/b.png".to_string()).unwrap();
        assert_eq!(pv.path, "a/b.png");
        assert_eq!((pv.width, pv.height), (4, 3));
        assert_eq!(&pv.png[..4], b"\x89PNG");
    }

    #[test]
    fn read_preview_rejects_lexical_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.png"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        // Normal repo-relative paths read.
        assert!(read_preview_bytes(dir.path(), "ok.png").is_ok());
        // Absolute and `..`-bearing paths are rejected before joining.
        assert!(read_preview_bytes(dir.path(), "/etc/passwd").is_err());
        assert!(read_preview_bytes(dir.path(), "../secret").is_err());
        assert!(read_preview_bytes(dir.path(), "sub/../../secret").is_err());
    }

    #[test]
    fn cache_hit_clears_stale_error() {
        let (tx, _rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.cache.put(
            ("x.rs".to_string(), 3u32),
            new_cached_diff(Arc::new(sample_diff(3)), None),
        );
        state.last_error = Some("stale".into());
        let shutdown = AtomicBool::new(false);
        let tree = Arc::new(Tree::build(Vec::new()));

        let (diff, _hl) =
            build_diff_for("x.rs", &mut state, "main...HEAD", 3, &tree, &tx, &shutdown);

        assert!(diff.is_some());
        assert!(
            state.last_error.is_none(),
            "a cache hit must clear a stale error"
        );
    }

    #[test]
    fn is_directory_detects_empty_mode_prefix() {
        // In empty-repo mode the listing now populates `last_changes`, so a
        // directory selected by prefix is routed to the aggregate path.
        let mut state = WorkerState::new(
            "HEAD".into(),
            true,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state
            .last_changes
            .insert("src/a.rs".into(), change_info("src/a.rs"));

        assert!(is_directory(&state, "src"));
        assert!(!is_directory(&state, "src/a.rs"));
    }

    /// Enqueue `requests`, receive the first, and fold the rest (as the loop
    /// does) with no shutdown pending. The base already in effect is
    /// `main...HEAD` and the root `/repo`, so a `SetRev` to any other rev or
    /// a `SwitchWorktree` to any other root supersedes queued work.
    fn coalesced(requests: Vec<Request>) -> Vec<Request> {
        coalesced_at(requests, Path::new("/repo"))
    }

    /// [`coalesced`] with an explicit root already in effect.
    fn coalesced_at(requests: Vec<Request>, current_root: &Path) -> Vec<Request> {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        for r in requests {
            tx.send(r).unwrap();
        }
        let first = rx.recv().unwrap();
        let shutdown = AtomicBool::new(false);
        coalesce(first, &rx, &shutdown, "main...HEAD", current_root)
    }

    fn diff_req(path: &str, seq: Seq) -> Request {
        Request::Diff {
            path: path.into(),
            ctx: 3,
            seq,
        }
    }

    fn hl_req(path: &str, seq: Seq) -> Request {
        Request::Highlight {
            path: path.into(),
            ctx: 3,
            from: 0,
            to: 5,
            seq,
        }
    }

    fn switch_req(root: &str) -> Request {
        Request::SwitchWorktree {
            root: PathBuf::from(root),
        }
    }

    /// A watch handle with no live watcher and a never-ready event receiver:
    /// the request-only loop's starting shape.
    fn test_watch() -> WatchHandle {
        WatchHandle {
            watcher: None,
            event_rx: crossbeam_channel::never(),
            poll_flag: Arc::new(AtomicBool::new(false)),
            switched: false,
        }
    }

    #[test]
    fn disconnected_watcher_channel_spins_but_never_does_not() {
        // The loops swap a closed watcher receiver for `never()` because
        // crossbeam's `select!` treats a disconnected channel as permanently
        // ready (a 100% CPU spin), while `never()` never wakes its arm so the
        // loop falls back to its default tick. The loop bodies are not
        // directly testable without a thread; this pins the channel
        // semantics the swap relies on.
        let (tx, rx) = crossbeam_channel::unbounded::<WorkerEvent>();
        drop(tx);
        let mut disconnected = false;
        crossbeam_channel::select! {
            recv(rx) -> ev => {
                assert!(ev.is_err(), "a disconnected channel yields Err");
                disconnected = true;
            }
            default(Duration::from_secs(1)) => {}
        }
        assert!(disconnected, "a disconnected arm is always ready");

        let never_rx: Receiver<WorkerEvent> = crossbeam_channel::never();
        let mut woke = false;
        crossbeam_channel::select! {
            recv(never_rx) -> _ => woke = true,
            default(Duration::ZERO) => {}
        }
        assert!(!woke, "never() must keep the arm silent");
    }

    #[test]
    fn coalesce_keeps_only_latest_diff() {
        let out = coalesced(vec![diff_req("a", 1), diff_req("b", 2), diff_req("c", 3)]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Request::Diff { path, .. } if path == "c"));
    }

    #[test]
    fn coalesce_setrev_drops_requests_before_it() {
        let out = coalesced(vec![
            diff_req("a", 1),
            Request::SetRev {
                rev: "x...HEAD".into(),
            },
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Request::SetRev { rev } if rev == "x...HEAD"));
    }

    #[test]
    fn coalesce_keeps_diff_after_setrev() {
        // A selection queued alongside a base switch must survive the fold and
        // be served after the recompute (SetRev first, then the Diff).
        let out = coalesced(vec![
            diff_req("a", 1),
            Request::SetRev {
                rev: "x...HEAD".into(),
            },
            diff_req("b", 2),
        ]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Request::SetRev { rev } if rev == "x...HEAD"));
        assert!(matches!(&out[1], Request::Diff { path, seq, .. } if path == "b" && *seq == 2));
    }

    #[test]
    fn coalesce_orders_survivors_by_seq() {
        // A lower-seq LoadImage must be emitted before a higher-seq Highlight,
        // or the Highlight advances `latest_request_seq` and stales the image.
        let out = coalesced(vec![
            diff_req("a", 1),
            Request::LoadImage {
                path: "a".into(),
                seq: 2,
            },
            hl_req("a", 3),
        ]);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Request::Diff { seq: 1, .. }));
        assert!(matches!(&out[1], Request::LoadImage { seq: 2, .. }));
        assert!(matches!(&out[2], Request::Highlight { seq: 3, .. }));
    }

    #[test]
    fn coalesce_low_seq_load_image_is_not_stale_dropped() {
        let out = coalesced(vec![
            Request::LoadImage {
                path: "a".into(),
                seq: 2,
            },
            hl_req("a", 3),
        ]);
        let (snap_tx, _snap_rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.selected_path = Some("a".into());
        state.ctx = 3;
        let shutdown = AtomicBool::new(false);
        for survivor in out {
            handle_request(
                survivor,
                &mut state,
                &snap_tx,
                &image_tx,
                &shutdown,
                &mut test_watch(),
            );
        }
        // Executed in seq order, so the image request was accepted (its
        // `preview_active` side effect ran) rather than stale-dropped.
        assert!(state.preview_active);
        assert_eq!(state.latest_request_seq, 3);
    }

    #[test]
    fn coalesce_drops_highlight_for_other_path() {
        let out = coalesced(vec![diff_req("a", 1), hl_req("b", 2)]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Request::Diff { path, .. } if path == "a"));
    }

    #[test]
    fn coalesce_keeps_highlight_for_surviving_diff() {
        let out = coalesced(vec![diff_req("a", 1), hl_req("a", 2)]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Request::Diff { .. }));
        assert!(matches!(&out[1], Request::Highlight { .. }));
    }

    #[test]
    fn coalesce_keeps_latest_load_image() {
        let out = coalesced(vec![
            diff_req("a", 1),
            Request::LoadImage {
                path: "a".into(),
                seq: 2,
            },
            Request::LoadImage {
                path: "b".into(),
                seq: 3,
            },
        ]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Request::Diff { .. }));
        assert!(matches!(&out[1], Request::LoadImage { path, .. } if path == "b"));
    }

    #[test]
    fn coalesce_same_rev_setrev_keeps_queued_diff() {
        // A SetRev naming the rev already in effect is a no-op for
        // `handle_request`; it must not discard a selection queued around it
        // (which would otherwise never be served).
        let out = coalesced(vec![
            diff_req("a", 1),
            Request::SetRev {
                rev: "main...HEAD".into(),
            },
        ]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Request::SetRev { rev } if rev == "main...HEAD"));
        assert!(matches!(&out[1], Request::Diff { path, seq, .. } if path == "a" && *seq == 1));
    }

    #[test]
    fn coalesce_switch_drops_requests_before_it() {
        let out = coalesced_at(
            vec![diff_req("a", 1), switch_req("/other")],
            Path::new("/repo"),
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Request::SwitchWorktree { root } if root == Path::new("/other")));
    }

    #[test]
    fn coalesce_keeps_requests_after_switch() {
        // Selections queued alongside a worktree switch survive the fold and
        // are served after the switch's recompute (switch first, then the
        // survivors in seq order).
        let out = coalesced_at(
            vec![
                diff_req("a", 1),
                switch_req("/other"),
                diff_req("b", 2),
                hl_req("b", 3),
            ],
            Path::new("/repo"),
        );
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Request::SwitchWorktree { root } if root == Path::new("/other")));
        assert!(matches!(&out[1], Request::Diff { path, seq, .. } if path == "b" && *seq == 2));
        assert!(
            matches!(&out[2], Request::Highlight { path, seq, .. } if path == "b" && *seq == 3)
        );
    }

    #[test]
    fn coalesce_switch_drops_setrev_before_it() {
        // The post-switch recompute derives the rev from the new worktree's
        // HEAD, so a base switch queued before a worktree switch is stale.
        let out = coalesced_at(
            vec![
                Request::SetRev {
                    rev: "x...HEAD".into(),
                },
                switch_req("/other"),
            ],
            Path::new("/repo"),
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Request::SwitchWorktree { .. }));
    }

    #[test]
    fn coalesce_keeps_setrev_after_switch() {
        let out = coalesced_at(
            vec![
                switch_req("/other"),
                Request::SetRev {
                    rev: "x...HEAD".into(),
                },
            ],
            Path::new("/repo"),
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Request::SwitchWorktree { .. }));
        assert!(matches!(&out[1], Request::SetRev { rev } if rev == "x...HEAD"));
    }

    #[test]
    fn coalesce_latest_switch_dominates() {
        let out = coalesced_at(
            vec![switch_req("/one"), diff_req("a", 1), switch_req("/two")],
            Path::new("/repo"),
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Request::SwitchWorktree { root } if root == Path::new("/two")));
    }

    #[test]
    fn coalesce_same_root_switch_keeps_queued_diff() {
        // A SwitchWorktree naming the root already in effect is a no-op for
        // `handle_request`; it must not discard a selection queued around it.
        let out = coalesced_at(
            vec![diff_req("a", 1), switch_req("/repo")],
            Path::new("/repo"),
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Request::SwitchWorktree { root } if root == Path::new("/repo")));
        assert!(matches!(&out[1], Request::Diff { path, seq, .. } if path == "a" && *seq == 1));
    }

    #[test]
    fn same_root_switch_is_a_no_op() {
        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("/repo"),
            git::BranchState::default(),
        );
        state.cache.put(
            ("a.rs".into(), 3u32),
            new_cached_diff(Arc::new(sample_diff(3)), None),
        );
        let shutdown = AtomicBool::new(false);
        let mut watch = test_watch();

        let quit = handle_request(
            switch_req("/repo"),
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            &mut watch,
        );

        assert!(!quit);
        assert!(
            !watch.switched,
            "a same-root no-op must not reset the debounce window"
        );
        assert!(
            rx.try_recv().is_err(),
            "a same-root switch must not emit a snapshot"
        );
        assert_eq!(state.repo_root, PathBuf::from("/repo"));
        assert!(
            state.cache.get(&("a.rs".to_string(), 3)).is_some(),
            "a same-root switch must not drop the cache"
        );
    }

    #[test]
    fn switch_to_non_repo_root_errors_and_keeps_state() {
        // The current root is a real repository; the target is a plain
        // directory that is not a git repository (and not inside one), so
        // validation must fail before any state mutation.
        let repo_dir = tempfile::tempdir().unwrap();
        crate::git::test_util::repo_with_commits(repo_dir.path(), &["one\n"]);
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            repo_dir.path().to_path_buf(),
            git::BranchState::default(),
        );
        state.cache.put(
            ("a.rs".into(), 3u32),
            new_cached_diff(Arc::new(sample_diff(3)), None),
        );
        state.last_tree = Some(Arc::new(Tree::build(vec![('M', "a.rs".to_string())])));
        let shutdown = AtomicBool::new(false);
        let mut watch = test_watch();

        let quit = handle_request(
            switch_req(dir.path().to_str().unwrap()),
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            &mut watch,
        );

        assert!(!quit);
        let snap = rx.recv().unwrap();
        // Name the expected failure modes so a mis-rooted environment (TMPDIR
        // inside some repository) fails with context, not a confusing assert:
        // the target is rejected as "not a git repository", "not inside a
        // checked-out worktree", or — when the environment plants one — as a
        // different repository.
        let err = snap
            .error
            .clone()
            .unwrap_or_else(|| panic!("the failure must reach the status line"));
        assert!(
            err.contains("not a git repository")
                || err.contains("not a worktree root")
                || err.contains("not inside a checked-out worktree")
                || err.contains("different repository"),
            "unexpected failure mode: {err}"
        );
        assert!(!snap.busy);
        assert!(!watch.switched);
        // All state untouched: no switch, no cache drop, rev intact.
        assert_eq!(state.repo_root, repo_dir.path());
        assert_eq!(state.rev, "main...HEAD");
        assert!(state.cache.get(&("a.rs".to_string(), 3)).is_some());
    }

    #[test]
    fn switch_to_other_repository_errors_and_keeps_state() {
        // `SwitchWorktree` promises "same repository, different workdir": a
        // directory belonging to a different repository must be rejected
        // before any state mutation, even though it is a valid worktree root.
        let a_dir = tempfile::tempdir().unwrap();
        crate::git::test_util::repo_with_commits(a_dir.path(), &["one\n"]);
        let b_dir = tempfile::tempdir().unwrap();
        crate::git::test_util::repo_with_commits(b_dir.path(), &["two\n"]);

        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "HEAD".into(),
            false,
            None,
            a_dir.path().to_path_buf(),
            git::BranchState::default(),
        );
        state.cache.put(
            ("a.rs".into(), 3u32),
            new_cached_diff(Arc::new(sample_diff(3)), None),
        );
        let shutdown = AtomicBool::new(false);
        let mut watch = test_watch();

        let quit = handle_request(
            switch_req(b_dir.path().to_str().unwrap()),
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            &mut watch,
        );
        assert!(!quit);
        let snap = rx.recv().unwrap();
        let err = snap
            .error
            .clone()
            .unwrap_or_else(|| panic!("the failure must reach the status line"));
        assert!(
            err.contains("different repository"),
            "a cross-repo switch must be rejected as such: {err}"
        );
        assert!(!watch.switched);
        // All state untouched: no switch, no cache drop.
        assert_eq!(state.repo_root, a_dir.path());
        assert!(state.cache.get(&("a.rs".to_string(), 3)).is_some());
    }

    #[test]
    fn switch_worktree_recomputes_from_new_root() {
        // End-to-end: depends on `git::set_repo_root`, which the git-layer
        // agent implements in parallel (the todo!() stub panics until then).
        // `set_repo_root` mutates the process-global root, so serialize
        // against the git module's own root tests and restore the cwd
        // fallback on drop (cargo runs tests in parallel threads).
        let _root_guard = root_lock();

        let main_dir = tempfile::tempdir().unwrap();
        crate::git::test_util::repo_with_commits(main_dir.path(), &["one\n"]);
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("wt");
        crate::git::test_util::run_git(
            main_dir.path(),
            &["worktree", "add", "-b", "side", wt.to_str().unwrap()],
        );
        // Untracked file existing only in the new worktree.
        std::fs::write(wt.join("wt-only.txt"), "from wt\n").unwrap();

        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(16);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(4);
        let mut state = WorkerState::new(
            "HEAD".into(),
            false,
            None,
            main_dir.path().to_path_buf(),
            git::BranchState::default(),
        );
        state.cache.put(
            ("stale.rs".into(), 3u32),
            new_cached_diff(Arc::new(sample_diff(3)), None),
        );
        // A raised floor from the old worktree's slow scan must not survive
        // the switch.
        state.floor_raised_at = Some(Instant::now());
        let shutdown = AtomicBool::new(false);
        let mut watch = test_watch();

        let quit = handle_request(
            switch_req(wt.to_str().unwrap()),
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            &mut watch,
        );
        assert!(!quit);
        assert!(
            watch.switched,
            "a completed switch must flag the loop to drop stale debounce state"
        );
        assert!(
            state.floor_raised_at.is_none(),
            "the switch must clear the old worktree's raised floor"
        );

        // Placeholder busy snapshot with an empty tree, then the recompute's
        // busy (fresh tree) and settled phases.
        let placeholder = rx.recv().unwrap();
        assert!(
            placeholder.busy,
            "the switch must emit a busy placeholder first"
        );
        assert!(
            placeholder.tree.flatten().is_empty(),
            "the placeholder must not carry the old worktree's tree"
        );
        let busy = rx.recv().unwrap();
        assert!(busy.busy);
        let settled = rx.recv().unwrap();
        assert!(!settled.busy);
        assert!(
            settled.tree.nodes.iter().any(|n| n.path == "wt-only.txt"),
            "the settled tree must come from the new worktree"
        );
        assert_eq!(state.repo_root, wt);
        assert!(
            state.cache.get(&("stale.rs".to_string(), 3)).is_none(),
            "the switch must drop the old worktree's diff cache"
        );

        // A diff for a new-worktree-only path serves content from the new
        // root (no stale old-worktree cache entry can answer it).
        let quit = handle_request(
            diff_req("wt-only.txt", 1),
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            &mut test_watch(),
        );
        assert!(!quit);
        let snap = rx.recv().unwrap();
        let diff = snap.selected_diff.as_ref().unwrap();
        assert!(
            diff.lines.iter().any(|l| l.raw_text.contains("from wt")),
            "the diff must be built from the new worktree's content"
        );
    }

    /// Serialize root-touching tests against every other module's, via the
    /// crate-wide `test_support::GIT_ROOT_LOCK`; `RootGuard::drop` restores
    /// the cwd fallback so the next test starts from clean global state.
    fn root_lock() -> RootGuard {
        let lock = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        RootGuard { _lock: lock }
    }

    struct RootGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for RootGuard {
        fn drop(&mut self) {
            git::clear_repo_root_for_tests();
        }
    }

    #[test]
    fn coalesce_shutdown_flag_returns_shutdown() {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        tx.send(diff_req("a", 1)).unwrap();
        let first = rx.recv().unwrap();
        let shutdown = AtomicBool::new(true);
        let out = coalesce(first, &rx, &shutdown, "main...HEAD", Path::new("/repo"));
        assert!(matches!(out.as_slice(), [Request::Shutdown]));
    }

    #[test]
    fn recompute_emits_busy_then_settled() {
        // Not hermetic: the `main...HEAD` rev runs the uncommitted-only scan
        // against the process cwd repo, so serialize against tests that
        // point the global root elsewhere.
        let _root_guard = root_lock();
        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(8);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        let shutdown = AtomicBool::new(false);
        let entries = vec![('M', "a.rs".to_string())];

        recompute_from_scan(
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            false,
            entries,
            HashMap::new(),
            Instant::now(),
        );

        let first = rx.recv().unwrap();
        assert!(first.busy, "phase 1 must be busy");
        let second = rx.recv().unwrap();
        assert!(!second.busy, "phase 2 must be settled");
    }

    #[test]
    fn settled_snapshot_carries_diff_error() {
        // Not hermetic: building the selected diff resolves the bogus rev in
        // the process cwd repo (which is expected to fail), so serialize
        // against tests that point the global root elsewhere.
        let _root_guard = root_lock();
        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(8);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "no-such-rev".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        // A selected file with cached metadata, so the diff build attempts to
        // resolve the bogus revision and fails.
        state.selected_path = Some("a.rs".into());
        state
            .last_changes
            .insert("a.rs".into(), change_info("a.rs"));
        let shutdown = AtomicBool::new(false);
        let entries = vec![('M', "a.rs".to_string())];

        recompute_from_scan(
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            false,
            entries,
            HashMap::new(),
            Instant::now(),
        );

        let busy = rx.recv().unwrap();
        assert!(busy.busy, "phase 1 must be busy");
        let settled = rx.recv().unwrap();
        assert!(!settled.busy, "phase 2 must be settled");
        assert!(
            settled.error.is_some(),
            "the settled snapshot must surface the diff build error"
        );
    }

    #[test]
    fn workdir_only_recompute_reuses_branch_state() {
        // A workdir-only window cannot move refs, so the cached branch state
        // is reused without re-walking it. Uses a rev without `...` so no
        // git I/O runs (hermetic).
        let (tx, _rx) = crossbeam_channel::bounded::<ArcSnapshot>(8);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "no-such-rev".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.last_branch = git::BranchState {
            name: Some("__stale__".into()),
            ..Default::default()
        };
        let shutdown = AtomicBool::new(false);

        recompute_from_scan(
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            false,
            Vec::new(),
            HashMap::new(),
            Instant::now(),
        );

        assert_eq!(state.last_branch.name.as_deref(), Some("__stale__"));
    }

    #[test]
    fn refs_moved_recompute_refreshes_branch_state() {
        // A ref move must re-walk branch state rather than reuse the cache.
        // Reads the real repo via the global root (cwd fallback), so it must
        // serialize against tests that point the root at temp worktrees.
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (tx, _rx) = crossbeam_channel::bounded::<ArcSnapshot>(8);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.last_branch = git::BranchState {
            name: Some("__stale__".into()),
            ..Default::default()
        };
        let shutdown = AtomicBool::new(false);

        recompute_from_scan(
            &mut state,
            &tx,
            &image_tx,
            &shutdown,
            true,
            Vec::new(),
            HashMap::new(),
            Instant::now(),
        );

        let expected = git::branch_state("main...HEAD");
        assert_eq!(state.last_branch.name, expected.name);
        assert_ne!(state.last_branch.name.as_deref(), Some("__stale__"));
    }

    #[test]
    fn directory_chunk_snapshot_is_busy() {
        let (tx, rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        state.selected_path = Some("src".into());
        let tree = Arc::new(Tree::build(vec![('M', "src/a.rs".to_string())]));
        state.last_tree = Some(tree.clone());

        let mut aggregate = Arc::new(Diff::directory("src"));
        Arc::make_mut(&mut aggregate)
            .extend_from_children(&[Arc::new(sample_diff(3)), Arc::new(sample_diff(3))]);
        stream_directory_chunk(&mut state, &tree, "src", &aggregate, &tx);

        let snap = rx.recv().unwrap();
        assert!(snap.busy);
        assert_eq!(snap.selected_diff.as_ref().unwrap().files.len(), 2);
    }

    #[test]
    fn directory_aggregate_borrows_child_arcs() {
        // A streaming/master list keeps children behind `Arc`; building an
        // aggregate must not deep-clone or retain owned copies of them, so the
        // same children can back every chunk of a growing directory diff.
        let children = vec![Arc::new(sample_diff(3)), Arc::new(sample_diff(3))];
        let aggregate = Diff::from_directory("src", &children);
        let again = Diff::from_directory("src", &children);

        assert_eq!(aggregate.files.len(), 2);
        assert_eq!(again.files.len(), 2);
        assert_eq!(
            aggregate, again,
            "repeated chunks must yield identical content"
        );
        for child in &children {
            assert_eq!(
                Arc::strong_count(child),
                1,
                "the aggregate must borrow, not retain, its child diffs"
            );
        }
    }

    #[test]
    fn highlight_snapshot_shares_diff_arc() {
        let (snap_tx, snap_rx) = crossbeam_channel::bounded::<ArcSnapshot>(8);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        let key = ("x.rs".to_string(), 3u32);
        state.cache.put(
            key.clone(),
            new_cached_diff(Arc::new(sample_diff(40)), None),
        );
        state.selected_path = Some("x.rs".into());
        state.ctx = 3;

        fill_highlight_range(&mut state, "x.rs", 3, 0, 10, &snap_tx);
        fill_highlight_range(&mut state, "x.rs", 3, 10, 20, &snap_tx);

        let a = snap_rx.recv().unwrap();
        let b = snap_rx.recv().unwrap();
        assert!(!a.busy && !b.busy);
        assert!(Arc::ptr_eq(
            a.selected_diff.as_ref().unwrap(),
            b.selected_diff.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(&a.selected_highlight, &b.selected_highlight));
    }

    #[test]
    fn highlight_cache_miss_does_not_build_or_send() {
        let (snap_tx, snap_rx) = crossbeam_channel::bounded::<ArcSnapshot>(4);
        let (image_tx, _image_rx) = crossbeam_channel::bounded::<PreviewImage>(1);
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        // The selection is current, but nothing is cached for it yet (e.g. a
        // directory aggregate mid-stream). A highlight nudge must degrade to a
        // no-op, never restart a build.
        state.selected_path = Some("x.rs".into());
        state.ctx = 3;
        let shutdown = AtomicBool::new(false);

        let quit = handle_request(
            hl_req("x.rs", 1),
            &mut state,
            &snap_tx,
            &image_tx,
            &shutdown,
            &mut test_watch(),
        );

        assert!(!quit);
        assert!(
            snap_rx.try_recv().is_err(),
            "a cache-miss highlight must not send a snapshot"
        );
        assert!(state.last_diff.is_none(), "no build was triggered");
    }

    #[test]
    fn lru_cap32_evicts_least_recently_used() {
        let mut c: LruCache<u32, u32> = LruCache::new(CACHE_CAP);
        for i in 0..CACHE_CAP as u32 {
            c.put(i, i);
        }
        // Touch 0 so it is no longer the least recently used.
        assert!(c.get(&0).is_some());
        c.put(99, 99);
        assert!(c.get(&1).is_none(), "least recently used evicted");
        assert!(c.get(&0).is_some(), "recently used kept");
    }

    #[test]
    fn shutdown_aborts_scan() {
        let mut state = WorkerState::new(
            "main...HEAD".into(),
            false,
            None,
            PathBuf::from("."),
            git::BranchState::default(),
        );
        let shutdown = AtomicBool::new(true);
        assert!(scan_entries(&mut state, &shutdown).is_err());
    }

    #[test]
    fn all_ignored_empty_list_never_skips() {
        assert!(!all_ignored(&[], |_| true));
    }

    #[test]
    fn all_ignored_all_paths_ignored_skips() {
        let paths = vec![PathBuf::from("target/a"), PathBuf::from("target/b")];
        assert!(all_ignored(&paths, |p| p.starts_with("target")));
    }

    #[test]
    fn all_ignored_mixed_list_recomputes() {
        let paths = vec![PathBuf::from("target/a"), PathBuf::from("src/b")];
        assert!(!all_ignored(&paths, |p| p.starts_with("target")));
    }

    #[test]
    fn ignore_matcher_uses_repo_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/foo"), "x\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let mut matcher = IgnoreMatcher::from_repo(repo).unwrap();

        assert!(matcher.is_ignored(Path::new("target/debug/foo")));
        assert!(!matcher.is_ignored(Path::new("src/main.rs")));
    }

    #[test]
    fn ignore_gate_does_not_skip_tracked_ignored_path() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "Cargo.lock\n").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "x\n").unwrap();
        // Force-add a file that an ignore pattern also matches.
        crate::git::test_util::run_git(dir.path(), &["add", "-f", "Cargo.lock"]);

        let repo = gix::open(dir.path()).unwrap();
        let mut matcher = IgnoreMatcher::from_repo(repo).unwrap();
        let index = matcher.repo.index().ok();

        // The exclude pattern matches, but the path is tracked, so the gate must
        // not skip its recompute.
        assert!(matcher.is_ignored(Path::new("Cargo.lock")));
        assert!(!matcher.is_ignored_untracked(Path::new("Cargo.lock"), index.as_ref()));
    }

    #[test]
    fn ignore_gate_unreadable_index_does_not_skip() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "Cargo.lock\n").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "x\n").unwrap();
        // Force-add an ignored file, then remove the index so it can't be read.
        crate::git::test_util::run_git(dir.path(), &["add", "-f", "Cargo.lock"]);
        std::fs::remove_file(dir.path().join(".git/index")).unwrap();

        let repo = gix::open(dir.path()).unwrap();
        let mut matcher = IgnoreMatcher::from_repo(repo).unwrap();
        let index = matcher.repo.index().ok();
        assert!(index.is_none(), "index must be unreadable for this test");

        // The path is ignored and the index can't prove it untracked, so the
        // gate must not skip (a force-added tracked file would be hidden).
        assert!(matcher.is_ignored(Path::new("Cargo.lock")));
        assert!(!matcher.is_ignored_untracked(Path::new("Cargo.lock"), index.as_ref()));
    }

    #[test]
    fn ignore_gate_skips_ignored_untracked_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/foo"), "x\n").unwrap();
        // Materialize a readable index (entry for .gitignore only), so the
        // absent target/foo is confidently untracked.
        crate::git::test_util::run_git(dir.path(), &["add", ".gitignore"]);

        let mut matcher = IgnoreMatcher::from_repo(repo).unwrap();
        let index = matcher.repo.index().ok();
        assert!(index.is_some(), "index must be readable for this test");

        assert!(matcher.is_ignored_untracked(Path::new("target/foo"), index.as_ref()));
    }
}
