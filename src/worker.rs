//! Live-update worker thread: owns git I/O, parsing, highlighting, and the
//! debounce/coalesce loop. Produces immutable `ArcSnapshot`s on a channel; the
//! UI thread swaps them in between frames. The worker holds the `Highlighter`
//! (syntect is `&mut self` and single-threaded) and a small LRU of recently
//! viewed `(path, ctx)` diffs so rapid `j`-mashing fills the pane instantly.

use std::collections::{HashMap, VecDeque};
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
    pub request_tx: Sender<Request>,
    pub snapshot_rx: Receiver<ArcSnapshot>,
    pub image_rx: Receiver<PreviewImage>,
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
        let watcher = match Watcher::start(repo_root.clone()) {
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
                );
            }
        };
        let poll_mode = watcher_poll_mode(&watcher);
        let event_rx = watcher.receiver().clone();

        let (request_tx, request_rx) = crossbeam_channel::unbounded::<Request>();
        let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded::<ArcSnapshot>();
        let (image_tx, image_rx) = crossbeam_channel::unbounded::<PreviewImage>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_t = shutdown.clone();

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
                    watcher,
                    shutdown_t,
                    LoopChannels {
                        event_rx,
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
            poll_mode,
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

/// `poll_mode` lives on the `Watcher`; clone its `Arc` out so the UI can read
/// it without holding the watcher.
fn watcher_poll_mode(w: &Watcher) -> Arc<AtomicBool> {
    // The Watcher does not expose its internal Arc; mirror via is_poll_mode at
    // spawn time. The flag is stable for the life of the watcher.
    Arc::new(AtomicBool::new(w.is_poll_mode()))
}

/// Degraded spawn path when the watcher failed to start.
fn spawn_no_watcher(
    rev: String,
    empty: bool,
    repo_root: PathBuf,
    theme: String,
    initial_branch: git::BranchState,
    watch_err: String,
) -> Result<Worker, WorkerError> {
    let (request_tx, request_rx) = crossbeam_channel::unbounded::<Request>();
    let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded::<ArcSnapshot>();
    let (image_tx, image_rx) = crossbeam_channel::unbounded::<PreviewImage>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_t = shutdown.clone();

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

            loop {
                if shutdown_t.load(Ordering::Relaxed) {
                    break;
                }
                crossbeam_channel::select! {
                    recv(request_rx) -> msg => {
                        match msg {
                            Ok(req) => {
                                if handle_request(req, &mut state, &snapshot_tx, &image_tx, &shutdown_t) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    default(HARD_CEILING) => {}
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
        poll_mode: Arc::new(AtomicBool::new(false)),
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
            last_branch: initial_branch,
            last_changes: HashMap::new(),
            preview_active: false,
            uncommitted_stats: None,
            ignore: IgnoreMatcher::open(),
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
/// parsed pattern state, so the chain is built once (in [`IgnoreMatcher::open`])
/// and reused across fires.
struct IgnoreMatcher {
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
    /// still trigger a recompute. Any index lookup failure is treated as
    /// not-skippable.
    fn is_ignored_untracked(&mut self, rel: &Path) -> bool {
        self.is_ignored(rel) && matches!(self.is_tracked(rel), Some(false))
    }

    /// `Some(true)` when `rel` has an index entry, `Some(false)` when the index
    /// is readable and has none, `None` when the index can't be read.
    fn is_tracked(&self, rel: &Path) -> Option<bool> {
        let index = self.repo.index_or_empty().ok()?;
        let path = gix::path::into_bstr(rel);
        Some(index.entry_by_path(path.as_ref()).is_some())
    }
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
    all_ignored(changed, |p| matcher.is_ignored_untracked(p))
}

/// The channels the worker loop reads from and writes to: watcher events,
/// UI requests in, snapshots and preview images out.
struct LoopChannels {
    event_rx: Receiver<WorkerEvent>,
    request_rx: Receiver<Request>,
    snapshot_tx: Sender<ArcSnapshot>,
    image_tx: Sender<PreviewImage>,
}

/// Main loop with watcher events + requests + debounce timer.
#[allow(clippy::too_many_arguments)]
fn run_loop(
    rev: String,
    empty: bool,
    highlighter: Option<Highlighter>,
    repo_root: PathBuf,
    initial_branch: git::BranchState,
    watcher: Watcher,
    shutdown: Arc<AtomicBool>,
    channels: LoopChannels,
) {
    let LoopChannels {
        mut event_rx,
        request_rx,
        snapshot_tx,
        image_tx,
    } = channels;
    let _watcher = watcher; // keep alive for the life of the loop
    let mut state = WorkerState::new(rev, empty, highlighter, repo_root, initial_branch);
    state.send_initial(&snapshot_tx);
    // First real snapshot comes from the initial recompute, so startup never
    // blocks the main thread on the git scan (whose cost scales with the total
    // diff size).
    recompute(&mut state, &snapshot_tx, &image_tx, &shutdown, true);

    let mut last_event: Option<Instant> = None;
    let mut last_recompute: Option<Instant> = None;
    // Set when the watcher channel closes; the receiver is swapped for a
    // never-ready one so `select!` blocks on requests/timeout instead of
    // spinning on the permanently-ready disconnected arm.
    let mut event_closed = false;
    // True when a workdir change (not just ref churn) arrived since the last
    // recompute, so the uncommitted-only scan is worth redoing.
    let mut saw_workdir = false;
    // Repo-relative paths from workdir events since the last recompute, used
    // to skip a recompute when every one is git-ignored. Deduped at fire time.
    let mut changed_paths: Vec<PathBuf> = Vec::new();
    // True when a ref moved in the current debounce window; a ref change must
    // always recompute, even alongside only-ignored workdir paths.
    let mut saw_refs = false;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let floor = current_floor(&state, now);
        let debounce_due = last_event
            .map(|le| should_fire(le, now, floor))
            .unwrap_or(false);
        let tick = next_tick(last_event, last_recompute, now, floor);

        let timeout = tick.min(HARD_CEILING);

        select! {
            recv(event_rx) -> ev => match ev {
                Ok(we) => {
                    last_event = Some(Instant::now());
                    match we {
                        WorkerEvent::WorkdirChanged(paths) => {
                            saw_workdir = true;
                            changed_paths.extend(paths);
                        }
                        WorkerEvent::RefsChanged => saw_refs = true,
                    }
                }
                Err(_) => {
                    // Watcher closed; stop watching the never-ready channel.
                    event_closed = true;
                }
            },
            recv(request_rx) -> req => match req {
                Ok(r) => {
                    // Fold any queued requests so a burst (rapid `j`) runs at
                    // most the latest of each kind.
                    let survivors = coalesce(r, &request_rx, &shutdown);
                    let mut quit = false;
                    for survivor in survivors {
                        if handle_request(survivor, &mut state, &snapshot_tx, &image_tx, &shutdown) {
                            quit = true;
                            break;
                        }
                    }
                    if quit {
                        break;
                    }
                }
                Err(_) => break,
            },
            default(timeout) => {}
        }
        if event_closed {
            event_rx = crossbeam_channel::never();
            event_closed = false;
        }

        let now = Instant::now();
        if debounce_due
            && last_event
                .map(|le| should_fire(le, now, floor))
                .unwrap_or(false)
        {
            if last_recompute
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
            if saw_refs || !skip_ignored(&mut state, &changed_paths) {
                recompute(
                    &mut state,
                    &snapshot_tx,
                    &image_tx,
                    &shutdown,
                    refresh_uncommitted_for(saw_workdir, saw_refs),
                );
                last_recompute = Some(Instant::now());
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
            saw_workdir = false;
            saw_refs = false;
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
fn coalesce(first: Request, rx: &Receiver<Request>, shutdown: &AtomicBool) -> Vec<Request> {
    if matches!(first, Request::Shutdown) {
        return vec![Request::Shutdown];
    }
    let mut folded = Folded::default();
    folded.absorb(first);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return vec![Request::Shutdown];
        }
        match rx.try_recv() {
            Ok(Request::Shutdown) => return vec![Request::Shutdown],
            Ok(r) => folded.absorb(r),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    folded.into_requests()
}

/// Accumulator for [`coalesce`]: retains the latest request of each kind,
/// dropping any that a newer rev switch supersedes.
#[derive(Default)]
struct Folded {
    set_rev: Option<String>,
    diff: Option<Request>,
    highlight: Option<Request>,
    image: Option<Request>,
}

impl Folded {
    fn absorb(&mut self, r: Request) {
        match r {
            // A rev switch discards everything queued before it, but requests
            // queued after it survive (a new selection alongside a base switch
            // must still be served once the recompute lands).
            Request::SetRev { rev } => {
                self.set_rev = Some(rev);
                self.diff = None;
                self.highlight = None;
                self.image = None;
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
        match self.set_rev {
            Some(rev) => {
                let mut out = Vec::with_capacity(survivors.len() + 1);
                out.push(Request::SetRev { rev });
                out.extend(survivors);
                out
            }
            None => survivors,
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
        Request::SetRev { .. } | Request::Shutdown => 0,
    }
}

/// Compute the current debounce floor, raising it for 10s after a slow/huge
/// recompute.
fn current_floor(state: &WorkerState, now: Instant) -> Duration {
    match state.floor_raised_at {
        Some(raised) if now.duration_since(raised) < FLOOR_RAISED_FOR => DEBOUNCE_CEILING,
        _ => DEBOUNCE_FLOOR,
    }
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

/// Whether a debounced recompute must redo the uncommitted-only scan. A commit
/// moves HEAD (refs-only) and changes `all_changes("HEAD")`, so a ref move
/// refreshes just like a workdir edit.
fn refresh_uncommitted_for(saw_workdir: bool, saw_refs: bool) -> bool {
    saw_workdir || saw_refs
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
) -> bool {
    // Don't start expensive work once the UI has begun tearing us down.
    if shutdown.load(Ordering::Relaxed) {
        return true;
    }
    match req {
        Request::Shutdown => true,
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
fn read_preview_bytes(
    repo_root: &Path,
    path: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let full = repo_root.join(path);
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
pub fn build_preview_bytes(
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
    if state.empty {
        let entries = git::initial_listing().map_err(|e| e.to_string())?;
        let paths: Vec<String> = entries.iter().map(|(_, p)| p.clone()).collect();
        let stats = git::initial_stats(&paths);
        state.last_changes.clear();
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
/// on file selection. `refresh_uncommitted` controls whether the extra
/// uncommitted-only scan runs: workdir edits, ref movement (a commit changes
/// `all_changes("HEAD")`), and rev switches all need it.
fn recompute(
    state: &mut WorkerState,
    tx: &Sender<ArcSnapshot>,
    image_tx: &Sender<PreviewImage>,
    shutdown: &AtomicBool,
    refresh_uncommitted: bool,
) {
    let t0 = Instant::now();
    let (entries, stats) = match scan_entries(state, shutdown) {
        Ok(x) => x,
        Err(err) => {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            state.last_error = Some(err);
            send_error_snapshot(state, tx);
            return;
        }
    };
    recompute_from_scan(
        state,
        tx,
        image_tx,
        shutdown,
        refresh_uncommitted,
        entries,
        stats,
        t0,
    );
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
    refresh_uncommitted: bool,
    entries: Vec<Entry>,
    stats: LineStats,
    t0: Instant,
) {
    // A watcher recompute may have changed files whose old diffs are cached;
    // evict just those so the pane shows fresh content without discarding
    // highlight progress for every other file.
    evict_changed(state);

    state.last_git_wall = t0.elapsed();
    state.last_branch = git::branch_state(&state.rev);

    // In branch comparisons, how much of the view is uncommitted mid-flight
    // work: a second, uncommitted-only scan. The uncommitted view's own
    // totals already are uncommitted-only, so skip it there. A failure just
    // hides the split from the footer.
    if refresh_uncommitted {
        state.uncommitted_stats = if state.rev.contains("...") {
            git::all_changes("HEAD").ok().map(|changes| {
                changes
                    .iter()
                    .fold((0, 0), |(a, d), c| (a + c.added, d + c.deleted))
            })
        } else {
            None
        };
    }

    // Adaptive floor: slow git or huge diff raises the floor for 10s.
    if state.last_git_wall > SLOW_GIT_THRESHOLD || entries.len() > LARGE_DIFF_THRESHOLD {
        state.floor_raised_at = Some(Instant::now());
    }

    let mut tree = Tree::build(entries);
    tree.annotate_stats(&stats);
    let tree = Arc::new(tree);
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
        // Empty-repo mode never populates `last_changes`; every file is new,
        // so nothing cached can be trusted.
        state.cache.clear();
        return;
    }
    let changed = &state.last_changes;
    state.cache.retain(|(p, _)| {
        !changed.contains_key(p) && !changed.keys().any(|c| c.starts_with(&format!("{p}/")))
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
    if let Some(e) = state.cache.get(&key) {
        return (Some(e.diff.clone()), e.hl.clone());
    }

    let diff = if is_directory(state, path) {
        build_directory_diff(path, state, rev, ctx, tree, tx, shutdown)
    } else {
        build_single_file_diff(path, state, rev, ctx)
    };

    match diff {
        Ok(diff) => {
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
    // Owned copies so `state` stays free for the streaming sends below.
    let mut entries: Vec<(String, git::ChangeInfo)> = state
        .last_changes
        .iter()
        .filter(|(p, _)| p.starts_with(&prefix))
        .map(|(p, ci)| (p.clone(), ci.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Open the repo once up front instead of re-discovering it per file.
    let repo = git::open_repo().map_err(|e| git::GitError::message(e.to_string()))?;

    // Children are `Arc`ed so each streamed chunk borrows them instead of
    // deep-cloning every diff built so far.
    let mut diffs: Vec<Arc<Diff>> = Vec::with_capacity(entries.len());
    for (p, ci) in entries {
        if shutdown.load(Ordering::Relaxed) {
            return Err(git::GitError::message("shutdown"));
        }
        diffs.push(Arc::new(git::diff_file_in(
            &repo,
            rev,
            &p,
            &ci.into_meta(),
            ctx,
        )?));
        if diffs.len().is_multiple_of(DIR_CHUNK) {
            stream_directory_chunk(state, tree, path, &diffs, tx);
        }
    }
    Ok(Diff::from_directory(path, &diffs))
}

/// Send a busy snapshot carrying the directory diff built so far. No-op when
/// the selection has moved on; the partial chunk is for the live view only.
fn stream_directory_chunk(
    state: &mut WorkerState,
    tree: &Arc<Tree>,
    path: &str,
    diffs: &[Arc<Diff>],
    tx: &Sender<ArcSnapshot>,
) {
    if state.selected_path.as_deref() != Some(path) {
        return;
    }
    let partial = Diff::from_directory(path, diffs);
    let hl = Arc::new(vec![None; partial.lines.len()]);
    let error = state.last_error.clone();
    send_snapshot(
        state,
        tx,
        tree.clone(),
        Some(Arc::new(partial)),
        hl,
        error,
        true,
    );
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
        highlighter.parse_only(&mut resume, &texts);
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
        highlighter.parse_only(&mut resume, &texts);
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

    /// Enqueue `requests`, receive the first, and fold the rest (as the loop
    /// does) with no shutdown pending.
    fn coalesced(requests: Vec<Request>) -> Vec<Request> {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        for r in requests {
            tx.send(r).unwrap();
        }
        let first = rx.recv().unwrap();
        let shutdown = AtomicBool::new(false);
        coalesce(first, &rx, &shutdown)
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
            handle_request(survivor, &mut state, &snap_tx, &image_tx, &shutdown);
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
    fn coalesce_shutdown_flag_returns_shutdown() {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        tx.send(diff_req("a", 1)).unwrap();
        let first = rx.recv().unwrap();
        let shutdown = AtomicBool::new(true);
        let out = coalesce(first, &rx, &shutdown);
        assert!(matches!(out.as_slice(), [Request::Shutdown]));
    }

    #[test]
    fn recompute_emits_busy_then_settled() {
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
    fn refresh_uncommitted_predicate_includes_refs() {
        assert!(!refresh_uncommitted_for(false, false));
        assert!(refresh_uncommitted_for(true, false));
        // A refs-only window (e.g. a commit moved HEAD) must refresh too.
        assert!(refresh_uncommitted_for(false, true));
        assert!(refresh_uncommitted_for(true, true));
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

        stream_directory_chunk(
            &mut state,
            &tree,
            "src",
            &[Arc::new(sample_diff(3)), Arc::new(sample_diff(3))],
            &tx,
        );

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

        // The exclude pattern matches, but the path is tracked, so the gate must
        // not skip its recompute.
        assert!(matcher.is_ignored(Path::new("Cargo.lock")));
        assert!(!matcher.is_ignored_untracked(Path::new("Cargo.lock")));
    }

    #[test]
    fn ignore_gate_skips_ignored_untracked_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/foo"), "x\n").unwrap();

        let mut matcher = IgnoreMatcher::from_repo(repo).unwrap();

        assert!(matcher.is_ignored_untracked(Path::new("target/foo")));
    }
}
