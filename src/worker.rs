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

use crossbeam_channel::{select, Receiver, Sender};

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
const CACHE_CAP: usize = 8;
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
    ) -> Result<Worker, WorkerError> {
        let watcher = match Watcher::start(repo_root.clone()) {
            Ok(w) => w,
            Err(e) => {
                // No watcher: surface via the first snapshot's error and run
                // request-only. poll_mode stays false (watcher is off, not
                // degraded to a workdir poll).
                return spawn_no_watcher(rev, empty, repo_root, initial_theme, e.to_string());
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
            let mut state = WorkerState::new(rev.clone(), empty, highlighter, repo_root);
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
    diff: Diff,
    hl: HighlightLines,
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
fn new_cached_diff(diff: &Diff, highlighter: Option<&Highlighter>) -> CachedDiff {
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
    CachedDiff {
        diff: diff.clone(),
        hl: vec![None; diff.lines.len()],
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
    last_tree: Option<Tree>,
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
}

impl WorkerState {
    fn new(
        rev: String,
        empty: bool,
        highlighter: Option<Highlighter>,
        repo_root: PathBuf,
    ) -> Self {
        // Compute once up front so the first worker snapshot already shows
        // real branch state; later recomputes refresh it.
        let initial_branch = git::branch_state(&rev);
        WorkerState {
            rev,
            empty,
            highlighter,
            repo_root,
            selected_path: None,
            ctx: 3,
            latest_request_seq: 0,
            last_tree: None,
            last_error: None,
            cache: LruCache::new(CACHE_CAP),
            floor_raised_at: None,
            last_git_wall: Duration::ZERO,
            last_branch: initial_branch,
            last_changes: HashMap::new(),
            preview_active: false,
        }
    }

    /// Send an initial empty-ish snapshot so the UI's first frame isn't blank
    /// while the first recompute is debouncing.
    fn send_initial(&mut self, tx: &Sender<ArcSnapshot>) {
        send_snapshot(self, tx, None, Vec::new(), self.last_error.clone());
    }

    /// Assemble a `Snapshot` from the current worker state plus the caller's
    /// diff/highlight/error specifics.
    fn snapshot(
        &mut self,
        tree: Tree,
        selected_diff: Option<Diff>,
        selected_highlight: HighlightLines,
        error: Option<String>,
    ) -> Snapshot {
        Snapshot {
            empty: self.empty,
            tree,
            selected_diff,
            selected_highlight,
            error,
            branch_state: self.last_branch.clone(),
        }
    }
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
fn run_loop(
    rev: String,
    empty: bool,
    highlighter: Option<Highlighter>,
    repo_root: PathBuf,
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
    let mut state = WorkerState::new(rev, empty, highlighter, repo_root);
    state.send_initial(&snapshot_tx);
    // First real snapshot comes from the initial recompute, so startup never
    // blocks the main thread on the git scan (whose cost scales with the total
    // diff size).
    recompute(&mut state, &snapshot_tx, &image_tx);

    let mut last_event: Option<Instant> = None;
    let mut last_recompute: Option<Instant> = None;
    // Set when the watcher channel closes; the receiver is swapped for a
    // never-ready one so `select!` blocks on requests/timeout instead of
    // spinning on the permanently-ready disconnected arm.
    let mut event_closed = false;

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
                Ok(_we) => {
                    last_event = Some(Instant::now());
                }
                Err(_) => {
                    // Watcher closed; stop watching the never-ready channel.
                    event_closed = true;
                }
            },
            recv(request_rx) -> req => match req {
                Ok(r) => {
                    if handle_request(r, &mut state, &snapshot_tx, &image_tx, &shutdown) {
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
            recompute(&mut state, &snapshot_tx, &image_tx);
            last_recompute = Some(Instant::now());
            last_event = None;
        }
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
                recompute(state, tx, image_tx);
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
            serve_diff(state, &path, ctx, tx);
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
        image::imageops::resize(
            &img.to_rgba8(),
            w,
            h,
            image::imageops::FilterType::Triangle,
        )
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
fn scan_entries(state: &mut WorkerState) -> Result<(Vec<Entry>, LineStats), String> {
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
/// on file selection.
fn recompute(state: &mut WorkerState, tx: &Sender<ArcSnapshot>, image_tx: &Sender<PreviewImage>) {
    let t0 = Instant::now();

    let (entries, stats) = match scan_entries(state) {
        Ok(x) => x,
        Err(err) => {
            state.last_error = Some(err);
            send_error_snapshot(state, tx);
            return;
        }
    };

    // A watcher recompute may have changed files whose old diffs are cached;
    // evict just those so the pane shows fresh content without discarding
    // highlight progress for every other file.
    evict_changed(state);

    state.last_git_wall = t0.elapsed();
    state.last_branch = git::branch_state(&state.rev);

    // Adaptive floor: slow git or huge diff raises the floor for 10s.
    if state.last_git_wall > SLOW_GIT_THRESHOLD || entries.len() > LARGE_DIFF_THRESHOLD {
        state.floor_raised_at = Some(Instant::now());
    }

    let mut tree = Tree::build(entries);
    tree.annotate_stats(&stats);
    state.last_tree = Some(clone_tree(&tree));

    let (selected_diff, selected_highlight) = {
        let rev = state.rev.clone();
        let ctx = state.ctx;
        build_selected_diff(state, &tree, &rev, ctx)
    };

    state.last_error = None;
    let snap = state.snapshot(tree, selected_diff, selected_highlight, None);
    let _ = tx.send(Arc::new(snap));

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
        !changed.contains_key(p)
            && !changed.keys().any(|c| c.starts_with(&format!("{p}/")))
    });
}

/// Send a snapshot that carries `error` but preserves the last known tree.
fn send_error_snapshot(state: &mut WorkerState, tx: &Sender<ArcSnapshot>) {
    send_snapshot(state, tx, None, Vec::new(), state.last_error.clone());
}

/// Assemble and send a snapshot from the current worker state plus the
/// caller's diff/highlight/error specifics. Builds from the last known tree
/// (an empty one if none) so the UI keeps showing the previous state.
fn send_snapshot(
    state: &mut WorkerState,
    tx: &Sender<ArcSnapshot>,
    diff: Option<Diff>,
    highlight: HighlightLines,
    error: Option<String>,
) {
    let tree = state
        .last_tree
        .clone()
        .unwrap_or_else(|| Tree::build(Vec::new()));
    let snap = state.snapshot(tree, diff, highlight, error);
    let _ = tx.send(Arc::new(snap));
}

/// Parse + highlight the currently-selected file's diff, if it's in the tree.
/// Returns `(diff, highlight)`. On any error, `(None, vec![])`.
fn build_selected_diff(
    state: &mut WorkerState,
    tree: &Tree,
    rev: &str,
    ctx: u32,
) -> (Option<Diff>, HighlightLines) {
    let Some(path) = state.selected_path.clone() else {
        return (None, Vec::new());
    };
    let in_tree = tree.nodes.iter().any(|n| n.path == path);
    if !in_tree {
        return (None, Vec::new());
    }
    build_diff_for(&path, state, rev, ctx)
}

/// Parse one file's diff. Uses the LRU cache when hit; on miss parses fresh
/// and caches it un-highlighted (highlighting is filled progressively via
/// [`fill_highlight`] as the UI scrolls). Mutates `state.cache`.
fn build_diff_for(
    path: &str,
    state: &mut WorkerState,
    rev: &str,
    ctx: u32,
) -> (Option<Diff>, HighlightLines) {
    let key = (path.to_string(), ctx);
    if let Some(e) = state.cache.get(&key) {
        return (Some(e.diff.clone()), e.hl.clone());
    }

    let diff = if is_directory(state, path) {
        build_directory_diff(path, state, rev, ctx)
    } else {
        build_single_file_diff(path, state, rev, ctx)
    };

    match diff {
        Ok(diff) => {
            // No highlighting yet: every line is `None` until the UI nudges the
            // worker to fill the viewport. The diff itself renders instantly.
            let cached = new_cached_diff(&diff, state.highlighter.as_ref());
            let hl = cached.hl.clone();
            state.cache.put(key, cached);
            state.last_error = None;
            (Some(diff), hl)
        }
        Err(e) => {
            state.last_error = Some(e.to_string());
            (None, Vec::new())
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
/// header.
fn build_directory_diff(
    path: &str,
    state: &WorkerState,
    rev: &str,
    ctx: u32,
) -> Result<Diff, git::GitError> {
    let prefix = format!("{path}/");
    let mut entries: Vec<(&String, &git::ChangeInfo)> = state
        .last_changes
        .iter()
        .filter(|(p, _)| p.starts_with(&prefix))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    // Open the repo once up front instead of re-discovering it per file.
    let repo = git::open_repo().map_err(|e| git::GitError::message(e.to_string()))?;

    let mut diffs = Vec::with_capacity(entries.len());
    for (p, ci) in entries {
        diffs.push(git::diff_file_in(
            &repo,
            rev,
            p,
            &ci.clone().into_meta(),
            ctx,
        )?);
    }
    Ok(Diff::from_directory(path, diffs))
}

/// Serve a `Request::Diff`: fetch the (cached or freshly-built) diff and send
/// a snapshot immediately, un-highlighted on first visit. Highlighting then
/// fills in via `Request::Highlight` nudges from the UI, so a larger file's
/// diff appears right away and colors in as it scrolls.
fn serve_diff(state: &mut WorkerState, path: &str, ctx: u32, tx: &Sender<ArcSnapshot>) {
    let rev = state.rev.clone();
    let (diff, hl) = build_diff_for(path, state, &rev, ctx);
    send_diff_snapshot(state, diff, hl, tx);
}

/// Assemble and send a snapshot whose only changed field vs the last full
/// recompute is the selected diff (plus error).
fn send_diff_snapshot(
    state: &mut WorkerState,
    diff: Option<Diff>,
    hl: HighlightLines,
    tx: &Sender<ArcSnapshot>,
) {
    send_snapshot(state, tx, diff, hl, state.last_error.clone());
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

/// Deep-clone a `Tree` (its nodes hold owned `Vec`s and `String`s).
fn clone_tree(t: &Tree) -> Tree {
    Tree {
        nodes: t.nodes.clone(),
        root: t.root,
    }
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
        let mut state = WorkerState::new("main...HEAD".into(), false, None, PathBuf::from("."));
        state
            .cache
            .put(("a.rs".into(), 3u32), new_cached_diff(&diff, None));
        state
            .cache
            .put(("b.rs".into(), 3u32), new_cached_diff(&diff, None));
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
        let mut state = WorkerState::new("main...HEAD".into(), false, None, PathBuf::from("."));
        state
            .cache
            .put(("src".into(), 3u32), new_cached_diff(&diff, None));
        state
            .cache
            .put(("doc".into(), 3u32), new_cached_diff(&diff, None));
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
        let mut state = WorkerState::new("HEAD".into(), true, None, PathBuf::from("."));
        state
            .cache
            .put(("a.rs".into(), 3u32), new_cached_diff(&diff, None));

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
        );
        let key = ("x.rs".to_string(), 3u32);
        state.cache.put(
            key.clone(),
            new_cached_diff(&diff, state.highlighter.as_ref()),
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
    fn worker_state_new_populates_branch() {
        // This repo is a git repo, so the first worker state must already
        // carry a branch name (the footer depends on it before any recompute).
        let state = WorkerState::new("main...HEAD".into(), false, None, PathBuf::from("."));
        assert!(state.last_branch.name.is_some());
    }

    #[test]
    fn build_preview_bytes_decodes_and_reencodes() {
        let mut img = image::RgbaImage::new(4, 3);
        for p in img.pixels_mut() {
            *p = image::Rgba([1, 2, 3, 255]);
        }
        let mut src = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut src),
                image::ImageFormat::Png,
            )
            .unwrap();

        let pv = build_preview_bytes(&src, "a/b.png".to_string()).unwrap();
        assert_eq!(pv.path, "a/b.png");
        assert_eq!((pv.width, pv.height), (4, 3));
        assert_eq!(&pv.png[..4], b"\x89PNG");
    }
}
