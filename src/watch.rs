//! Filesystem watching for live updates.
//!
//! Produces a stream of `WorkerEvent`s: either the workdir changed (a
//! tracked/untracked file moved, carrying the changed paths) or the git refs
//! moved (`HEAD`, `packed-refs`, or any ref under `.git/refs/`). The worker
//! thread consumes these and recomputes. Two layers:
//!
//! - **`notify` (recommended watcher)** watches the repo root recursively.
//!   Paths under `.git/` are ignored except `.git/HEAD`, `.git/packed-refs`,
//!   and anything under `.git/refs/` or `.git/reftable/`, which become
//!   `RefsChanged`, and `.git/index` + `.git/info/exclude`, which force an
//!   unattributable recompute; everything else under `.git` is ignored.
//! - **Backstop poll**: every 2s, compare a `(mtime, size)` signature over
//!   this worktree's `HEAD`, the shared `packed-refs` and reftable
//!   `tables.list`, every file under the loose ref trees, the worktree index,
//!   and the shared `info/exclude`; no symref resolution (every ref file is
//!   probed on its own, so a moved base ref is caught even though a commit
//!   doesn't touch `HEAD`'s mtime). If `notify` missed a ref change (some
//!   filesystems are flaky), the poll catches it.
//!
//! Ref state is not always inside the worktree root: the main checkout keeps it
//! under `<root>/.git`, but a linked worktree's `.git` is a *file* pointing at
//! `<common>/worktrees/<id>` (per-worktree `HEAD` and `index`) while the shared
//! refs live in `<common>/refs` and `<common>/packed-refs` — all outside the
//! root. [`GitAdmin`] resolves both dirs once at watcher start; the notify loop
//! adds watches for them and the backstop poll stats their ref files. The
//! worktree index and the shared `info/exclude` are watched too: staging or
//! unstaging, or an exclude rewrite, changes the view without moving a file
//! under the root.
//!
//! When `notify` init or its recursive `watch()` fails (unsupported
//! filesystem, watch-table exhaustion), the watcher degrades to a workdir poll
//! and flips the caller-shared poll-mode flag (the `Arc` passed into
//! `start_with_flag`, of which the worker/UI already hold a clone), so the
//! footer hints at degradation the moment it happens rather than at spawn time.
//! The poll walks workdir metadata (path/mtime/size, `.git` skipped,
//! git-ignored directories pruned) and emits `WorkdirChanged` only when an
//! entry appeared, disappeared, or changed, so an idle repository costs no
//! recomputes. Pruning is a self-contained, std-fs-only subset of gitignore:
//! each directory's `.gitignore` is read as the walk enters it, and a directory
//! is pruned only by patterns that contain no `!`, `*`, `?`, or `[`. Once any
//! active layer contains a `!` line, pruning is disabled for the whole subtree
//! beneath that layer: a bare name from an ancestor could otherwise prune a
//! directory that a deeper or sibling negation re-includes (git applies
//! negations last, across every layer), so the walk errs by walking more rather
//! than skipping a subtree git would have kept. A layer's own negation
//! therefore also suppresses that layer's positive bare names. Poll mode is a
//! degraded fallback, and this subset still catches the real-world bulk
//! (`node_modules/`, `target`, `.venv/`, `dist/`). No gix or git access is
//! involved, so a wedged repository can never stall the walk.
//! The poll interval is adaptive: the walk's duration bounds the duty cycle, so
//! a slow tree polls less often instead of pegging a core. Workdir events carry
//! their changed paths so the worker can drop a burst that only touched ignored
//! files. `Watcher::drop` waits a bounded time for the thread and then leaks it
//! rather than blocking process exit on a wedged walk.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::RecursiveMode;

use crate::git;
use crate::perf::{self, Counter};

/// Classification of what moved. A workdir event carries the repo-relative
/// paths that changed so the worker can ignore-filter a recompute; refs are
/// coarse (any ref movement triggers a rescan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    /// A path under the worktree changed (tracked or untracked file), with the
    /// repo-relative paths that changed. An empty list means "unattributable,
    /// recompute": a watcher batch overflow, or the worktree index changed.
    WorkdirChanged(Vec<PathBuf>),
    /// Any git ref moved (`HEAD`, `packed-refs`, or a local/remote/tag ref).
    RefsChanged,
}

/// Failure to start the watcher thread.
#[derive(Debug)]
pub struct WatchError {
    pub message: String,
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WatchError {}

/// Backstop poll interval for the git refs.
const BACKSTOP_INTERVAL: Duration = Duration::from_secs(2);
/// Workdir poll interval in degraded (poll) mode.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Multiplier applied to the last walk's duration to bound the poll's duty
/// cycle: a slow walk means a proportionally longer interval, never a
/// constant-CPU 1s cadence.
const POLL_ADAPTIVE_FACTOR: u32 = 2;
/// Loop tick used so shutdown stays responsive between sleeps.
const SHUTDOWN_TICK: Duration = Duration::from_millis(100);
/// Shutdown-check cadence inside the poll walk: the walk aborts promptly on
/// quit without stat-ing the flag for every entry.
const WALK_SHUTDOWN_CHECK: usize = 1024;
/// Bound on `Watcher::drop`'s wait for the watch thread to report completion
/// before the thread is leaked instead of joined.
const DROP_TIMEOUT: Duration = Duration::from_secs(2);
/// Cap on workdir paths retained while the bounded out channel is full. Past
/// this, the batch collapses to an empty-path `WorkdirChanged` (downstream
/// reads empty as "unattributable, recompute") and further paths are dropped
/// until the next successful send, so a sustained burst under a stalled worker
/// can never grow the retry buffer without bound.
const PENDING_CAP: usize = 4096;
/// Cap on loose-ref entries the backstop signature stats. Past this a
/// pathological ref count is truncated; notify is still the primary trigger.
const MAX_REF_ENTRIES: usize = 4096;
/// Cap on a `.gitignore` read in the degraded poll walk: a committed symlink to
/// a device or a huge file must not hang or exhaust memory every interval.
const MAX_GITIGNORE_BYTES: u64 = 1024 * 1024;

/// Owns the filesystem watcher thread. Dropping it signals shutdown and waits
/// a bounded time for the thread to finish.
pub struct Watcher {
    rx: Receiver<WorkerEvent>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Completion signal from the watch thread, so `drop` can tell a finished
    /// thread from one wedged in a syscall.
    done_rx: Receiver<()>,
}

impl Watcher {
    /// Start watching `repo_root`, sharing a caller-owned poll-mode flag. The
    /// worker owns one process-lifetime flag (the UI clones it once and reads
    /// it every frame), so re-arming the watcher on a worktree switch must
    /// reuse that `Arc` rather than mint a new one the UI would never see.
    /// A fresh watcher always starts un-degraded: the flag may still read
    /// `true` from a previous root's degradation, and the reset happens here
    /// before the thread spawns, so no race can re-set it first.
    pub fn start_with_flag(
        repo_root: PathBuf,
        poll_mode: Arc<AtomicBool>,
    ) -> Result<Watcher, WatchError> {
        Self::start(repo_root, poll_mode, BACKSTOP_INTERVAL)
    }

    /// Test-only: run with a caller-supplied backstop cadence so a test can
    /// disable the poll and prove an event came from `notify`.
    #[cfg(test)]
    fn start_with_backstop(
        repo_root: PathBuf,
        poll_mode: Arc<AtomicBool>,
        backstop: Duration,
    ) -> Result<Watcher, WatchError> {
        Self::start(repo_root, poll_mode, backstop)
    }

    fn start(
        repo_root: PathBuf,
        poll_mode: Arc<AtomicBool>,
        backstop: Duration,
    ) -> Result<Watcher, WatchError> {
        poll_mode.store(false, Ordering::Relaxed);
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = bounded::<WorkerEvent>(128);
        let (done_tx, done_rx) = bounded::<()>(1);

        let poll_mode_t = poll_mode.clone();
        let shutdown_t = shutdown.clone();
        // Resolve the git admin dirs once on the caller's thread; the watch
        // thread and both poll loops then use only paths and stats.
        let refs = GitAdmin::discover(&repo_root);
        let handle = thread::Builder::new()
            .name("bough-watch".into())
            .spawn(move || {
                let io = WatchIo {
                    repo_root,
                    refs,
                    backstop,
                    out_tx: tx,
                    shutdown: shutdown_t,
                };
                run_loop(io, poll_mode_t);
                // Report completion on every exit path so `Watcher::drop` can
                // distinguish a finished thread from a wedged one.
                let _ = done_tx.send(());
            })
            .map_err(|e| WatchError {
                message: format!("failed to spawn watch thread: {e}"),
            })?;

        Ok(Watcher {
            rx,
            shutdown,
            handle: Some(handle),
            done_rx,
        })
    }

    /// Borrow the event receiver. The worker drains this.
    pub fn receiver(&self) -> &Receiver<WorkerEvent> {
        &self.rx
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Wait briefly for the thread to confirm it finished. A thread wedged in
        // a pathological filesystem syscall must never block process exit: on
        // timeout we leak the join handle and return. When `main` returns the
        // process exits and the OS tears the thread down regardless.
        if wait_done(&self.done_rx, DROP_TIMEOUT).is_ok() {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// Wait up to `timeout` for the watch thread's done signal. `Ok` when the
/// thread reported completion (or its sender vanished); `Err` on timeout, when
/// the caller must not join.
fn wait_done(done_rx: &Receiver<()>, timeout: Duration) -> Result<(), ()> {
    match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => Ok(()),
        Err(RecvTimeoutError::Timeout) => Err(()),
    }
}

/// The two git administration directories a repository's ref state lives under,
/// resolved once at watcher start. In the main checkout both point at
/// `<root>/.git` (already covered by the recursive workdir watch); in a linked
/// worktree they are `<common>/worktrees/<id>` and `<common>`, both outside the
/// worktree root. Only paths are kept, so the watch and poll loops stay
/// gix-free after this discovery.
struct GitAdmin {
    /// Per-worktree admin dir: this worktree's `HEAD` and `index`.
    git_dir: PathBuf,
    /// Shared admin dir: `refs/`, `packed-refs`, and the reftable store.
    common_dir: PathBuf,
}

impl GitAdmin {
    /// Resolve from `repo_root`. `None` when it is not a repository or git
    /// discovery failed (the workdir watch still works; there is nothing to
    /// probe).
    fn discover(repo_root: &Path) -> Option<Self> {
        match git::open_repo_at(repo_root) {
            Ok(repo) => Some(GitAdmin {
                git_dir: repo.git_dir().to_path_buf(),
                common_dir: repo.common_dir().to_path_buf(),
            }),
            Err(e) => {
                // Without the admin dirs the ref and index backstop is blind;
                // the workdir watch keeps working, so log rather than fail.
                crate::perf_evt!("watch git_admin err={e}");
                None
            }
        }
    }

    /// Extra watches for the admin dirs, skipping anything already under the
    /// recursive workdir watch. The admin dirs are watched non-recursively
    /// (`HEAD` and `index` sit directly in them; recursing would pull in
    /// `objects/` and `logs/` churn); the shared ref trees are watched
    /// recursively.
    fn extra_watches(&self, repo_root: &Path) -> Vec<(PathBuf, RecursiveMode)> {
        let candidates = [
            (self.git_dir.clone(), RecursiveMode::NonRecursive),
            (self.common_dir.clone(), RecursiveMode::NonRecursive),
            (self.common_dir.join("info"), RecursiveMode::NonRecursive),
            (self.common_dir.join("refs"), RecursiveMode::Recursive),
            (self.git_dir.join("refs"), RecursiveMode::Recursive),
            (self.common_dir.join("reftable"), RecursiveMode::Recursive),
        ];
        let mut out: Vec<(PathBuf, RecursiveMode)> = Vec::new();
        for (path, mode) in candidates {
            if path.starts_with(repo_root) || !path.exists() {
                continue;
            }
            if !out.iter().any(|(p, _)| p == &path) {
                out.push((path, mode));
            }
        }
        out
    }

    /// True when `path` is a ref path: this worktree's `HEAD`, the shared
    /// `packed-refs`, anything under either `refs/` tree, or the reftable store.
    fn is_ref_path(&self, path: &Path) -> bool {
        let named = |dir: &Path, name: &str| {
            path.parent() == Some(dir) && path.file_name() == Some(std::ffi::OsStr::new(name))
        };
        named(&self.git_dir, "HEAD")
            || named(&self.common_dir, "packed-refs")
            || path.starts_with(self.common_dir.join("refs"))
            || path.starts_with(self.common_dir.join("reftable"))
            || path.starts_with(self.git_dir.join("refs"))
    }

    /// True for the worktree's index: staging or unstaging rewrites it, which
    /// changes the view (the staged side of every diff) without moving a file
    /// under the worktree root.
    fn is_index_path(&self, path: &Path) -> bool {
        path == self.git_dir.join("index")
    }

    /// True for the shared `info/exclude` file: it changes which untracked
    /// paths count without moving a file under the worktree root.
    fn is_exclude_path(&self, path: &Path) -> bool {
        path == self.common_dir.join("info").join("exclude")
    }
}

/// Shared thread inputs threaded through the orchestration: where to watch,
/// where to emit events, and the shutdown flag.
struct WatchIo {
    repo_root: PathBuf,
    /// Git admin dirs whose refs the notify loop extra-watches and the poll
    /// backstop stats. `None` when the root is not a repository or git
    /// discovery failed.
    refs: Option<GitAdmin>,
    /// Backstop poll cadence; [`BACKSTOP_INTERVAL`] in production, effectively
    /// disabled by the live-watch test.
    backstop: Duration,
    out_tx: Sender<WorkerEvent>,
    shutdown: Arc<AtomicBool>,
}

/// Thread entry: try `notify`; on success run the forward+backstop loop, else
/// fall back to the 1s poll loop.
fn run_loop(io: WatchIo, poll_mode: Arc<AtomicBool>) {
    let (raw_tx, raw_rx) = bounded::<notify::Result<notify::Event>>(128);
    // A committed `link -> /` must not pull inotify watches outside the repo
    // (watch exhaustion + event flood); the poll walk already never follows
    // links.
    // `<... as notify::Watcher>::new`: the trait isn't imported (it would
    // collide with this file's `Watcher` struct), so UFCS pins `Self`.
    match <notify::RecommendedWatcher as notify::Watcher>::new(
        move |res: notify::Result<notify::Event>| {
            let _ = raw_tx.send(res);
        },
        notify::Config::default().with_follow_symlinks(false),
    ) {
        Ok(mut watcher) => {
            if let Err(e) =
                notify::Watcher::watch(&mut watcher, &io.repo_root, RecursiveMode::Recursive)
            {
                crate::perf_evt!("watch poll_mode watch err={e}");
                poll_mode.store(true, Ordering::Relaxed);
                // Just drop: closing the inotify fd frees every kernel watch
                // it held. `unwatch` on a recursive root re-walks the whole
                // tree to remove watches one by one and wedges on unreadable
                // subtrees (observed on a 148k-file repo with a
                // permission-denied dir) — the thread would never reach the
                // poll loop.
                drop(watcher);
                run_poll_loop(&io);
                return;
            }
            watch_ref_locations(&mut watcher, &io);
            run_notify_loop(&io, watcher, raw_rx);
        }
        Err(e) => {
            // `notify` could not even be constructed on this platform;
            // degrade to the poll loop and record why.
            crate::perf_evt!("watch poll_mode init err={e}");
            poll_mode.store(true, Ordering::Relaxed);
            run_poll_loop(&io);
        }
    }
}

/// Arm the extra watches for the git admin dirs a linked worktree's refs live
/// under. A failure is logged, not fatal: the 2s ref poll still backstops it,
/// and degrading the whole watcher to poll mode would cost far more.
fn watch_ref_locations(watcher: &mut notify::RecommendedWatcher, io: &WatchIo) {
    let Some(admin) = io.refs.as_ref() else {
        return;
    };
    for (path, mode) in admin.extra_watches(&io.repo_root) {
        if let Err(e) = notify::Watcher::watch(watcher, &path, mode) {
            crate::perf_evt!("watch refs err={e} path={}", path.display());
        }
    }
}

/// `notify`-driven forward loop plus the 2s backstop ref poll.
fn run_notify_loop(
    io: &WatchIo,
    watcher: notify::RecommendedWatcher,
    raw_rx: Receiver<notify::Result<notify::Event>>,
) {
    // Keep the watcher alive for the life of the loop.
    let _watcher = watcher;

    let mut refs = io.refs.as_ref().map(RefProbe::new);
    let mut last_poll = Instant::now();
    // Workdir paths observed since the last successful send. A send dropped
    // under backpressure restores them here so the worker can't miss the final
    // change in a burst (only refs have a poll backstop, not the workdir).
    let mut pending = PendingPaths::default();
    let mut refs_pending = false;
    let mut last_beat = Instant::now();

    loop {
        if !watch_tick(io, &mut last_beat) {
            break;
        }
        if !flush_pending(io, &mut pending, &mut refs_pending) {
            return;
        }
        match raw_rx.recv_timeout(SHUTDOWN_TICK) {
            Ok(Ok(event)) => {
                perf::tally(Counter::WatchRawEvents);
                if event.need_rescan() {
                    // notify dropped events (e.g. an inotify queue overflow) and
                    // cannot say what changed: recompute unconditionally instead
                    // of silently losing them.
                    pending.push_unattributable();
                } else if is_read_access(&event.kind) {
                    perf::tally(Counter::WatchReadFiltered);
                } else {
                    absorb_event(io, &event.paths, &mut pending, &mut refs_pending);
                }
            }
            Ok(Err(e)) => crate::perf_evt!("watch notify_err err={e}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        poll_refs(
            &mut refs,
            &mut pending,
            &mut last_poll,
            &mut refs_pending,
            io.backstop,
        );
    }
}

/// True for `Access` events that carry no content change: a bare read, an open
/// in a read-only mode, or a close of a read-only (or unknown-mode) handle.
/// `notify` emits these
/// for every git read of `.git/HEAD`/refs (and of workdir files), so treating
/// them as changes would close a recompute -> read -> recompute loop. Writes
/// still surface as `Modify`/`Create` events, and `Open`/`Close` in a writable
/// mode (`Write`, `Other`, `Execute`) is not skipped.
fn is_read_access(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Read) => true,
        // `Any` is the unknown-mode catch-all; treat it like a read to avoid
        // the recompute loop, matching the previous behaviour.
        EventKind::Access(AccessKind::Open(mode)) => {
            matches!(mode, AccessMode::Read | AccessMode::Any)
        }
        EventKind::Access(AccessKind::Close(mode)) => {
            matches!(mode, AccessMode::Read | AccessMode::Any)
        }
        _ => false,
    }
}

/// Classify a raw notify event's `paths` into accumulated `pending` workdir
/// paths and the `refs` flag. Paths under the worktree root are made relative
/// and classified by [`classify_into`]; paths outside it can only come from the
/// [`GitAdmin`] watches, so they are matched against it directly (a ref move
/// latches `refs`; an index or exclude rewrite forces an unattributable recompute).
fn absorb_event(io: &WatchIo, paths: &[PathBuf], pending: &mut PendingPaths, refs: &mut bool) {
    for path in paths {
        match path.strip_prefix(&io.repo_root) {
            Ok(rel) => classify_into(rel, pending, refs),
            Err(_) => {
                let Some(admin) = io.refs.as_ref() else {
                    continue;
                };
                if admin.is_ref_path(path) {
                    *refs = true;
                } else if admin.is_index_path(path) || admin.is_exclude_path(path) {
                    pending.push_unattributable();
                }
            }
        }
    }
}

/// Sort and remove duplicate paths from an accumulated workdir batch.
fn dedup_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

/// Accumulated workdir paths awaiting a send, plus the unattributable latch.
/// While the out channel is full the batch is retried; past [`PENDING_CAP`] it
/// collapses to the empty-path form (an empty `Vec`, i.e. "unattributable,
/// recompute") and new paths are dropped until the next successful send, so a
/// sustained burst can't grow the retry buffer without bound.
#[derive(Default)]
struct PendingPaths {
    paths: Vec<PathBuf>,
    /// True once the batch is unattributable (overflow collapse or an index
    /// rewrite): it sends no paths, and further paths are dropped until it is
    /// sent.
    unattributable: bool,
}

impl PendingPaths {
    /// Record changed paths, collapsing to the unattributable form on overflow.
    fn push_all(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.unattributable {
            return;
        }
        for path in paths {
            if self.paths.len() >= PENDING_CAP {
                self.paths.clear();
                self.unattributable = true;
                return;
            }
            self.paths.push(path);
        }
    }

    /// Latch the unattributable form without recording a path: the view changed
    /// (the worktree index was rewritten) but no path under the root did, so
    /// the ignore gate must never skip the next recompute.
    fn push_unattributable(&mut self) {
        self.paths.clear();
        self.unattributable = true;
    }

    /// True when there is a batch (paths or the unattributable latch) to send.
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.unattributable
    }

    /// Take the batch for a send: the deduped paths, or an empty `Vec` for an
    /// unattributable batch.
    fn take(&mut self) -> Vec<PathBuf> {
        if self.unattributable {
            self.unattributable = false;
            self.paths.clear();
            Vec::new()
        } else {
            dedup_paths(&mut self.paths);
            std::mem::take(&mut self.paths)
        }
    }

    /// Restore a batch rejected by `try_send`: an empty batch re-arms the
    /// unattributable latch, otherwise the paths are kept for the next retry.
    fn restore(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.unattributable = true;
        } else {
            self.paths = paths;
        }
    }
}

/// Degraded loop when `notify` is unavailable: walk the workdir on an adaptive
/// interval and emit `WorkdirChanged` only for paths whose metadata signature
/// changed, plus the same 2s backstop ref poll. The walk prunes directories
/// matched by a conservative, std-only subset of the active `.gitignore`
/// layers, so it covers the tracked tree without any gix/git dependency.
fn run_poll_loop(io: &WatchIo) {
    let mut refs = io.refs.as_ref().map(RefProbe::new);
    let mut last_refs = Instant::now();
    // Baseline taken before the first interval so a change landing right after
    // startup is not folded into the baseline unseen.
    let (mut last_signature, mut last_unreadable) = walk_signature(&io.repo_root, &io.shutdown);
    let mut last_workdir = Instant::now();
    let mut interval = POLL_INTERVAL;
    let mut last_beat = Instant::now();
    // Same retained-batch / refs-latch backpressure as the notify loop: a
    // stalled worker must never wedge the watcher, so sends are non-blocking.
    let mut pending = PendingPaths::default();
    let mut refs_pending = false;

    loop {
        if !watch_tick(io, &mut last_beat) {
            break;
        }
        if !flush_pending(io, &mut pending, &mut refs_pending) {
            return;
        }
        thread::sleep(SHUTDOWN_TICK);

        if last_workdir.elapsed() >= interval {
            let started = Instant::now();
            let (signature, unreadable) = walk_signature(&io.repo_root, &io.shutdown);
            interval = next_poll_interval(started.elapsed());
            last_workdir = Instant::now();
            // Surface walk blindness once per change, not once per poll.
            if unreadable != last_unreadable {
                crate::perf_evt!("watch poll_walk unreadable={unreadable}");
                last_unreadable = unreadable;
            }
            let changed = signature_diff(&last_signature, &signature);
            last_signature = signature;
            if !changed.is_empty() {
                pending.push_all(changed);
            }
        }
        poll_refs(
            &mut refs,
            &mut pending,
            &mut last_refs,
            &mut refs_pending,
            io.backstop,
        );
    }
}

/// Next poll interval: the last walk's duration must bound the poll's duty
/// cycle, so the interval is at least [`POLL_INTERVAL`] and at least twice the
/// walk. A slow tree polls less often instead of burning a core.
fn next_poll_interval(walk: Duration) -> Duration {
    POLL_INTERVAL.max(walk.saturating_mul(POLL_ADAPTIVE_FACTOR))
}

/// Metadata signature of the workdir: repo-relative path -> `(mtime, size)`.
/// Symlink metadata only, so the walk reads no file content and never follows
/// links. `.git` is skipped entirely (ref and index movement is the backstop
/// poll's job).
type Signature = HashMap<PathBuf, (SystemTime, u64)>;

/// One parsed `.gitignore` file, reduced to the patterns that can prune a
/// directory without risking an over-prune. `base` is the directory the file
/// lives in, relative to the walk root (`""` for the root).
struct IgnoreLayer {
    base: PathBuf,
    /// Bare names (`target`, `node_modules/`) matching a directory of that name
    /// at any depth beneath `base`.
    names: Vec<String>,
    /// Anchored, layer-relative directory paths (`/build`, `tests/cache`)
    /// matching a directory whose path under `base` is exactly this.
    anchored: Vec<PathBuf>,
    /// True when the layer's body contains a `!` line. Any active negation
    /// disables pruning in the whole subtree beneath the layer, because git
    /// applies negations last across every layer.
    negated: bool,
}

/// Recursively collect the [`Signature`] of every file under `root`, pruning
/// directories ignored by the active layer stack. `shutdown` is polled every
/// [`WALK_SHUTDOWN_CHECK`] entries so a quit aborts a long walk promptly.
fn walk_signature(root: &Path, shutdown: &AtomicBool) -> (Signature, usize) {
    let mut span = perf::span("poll_walk");
    let mut walk = PollWalk {
        root,
        shutdown,
        out: Signature::new(),
        layers: Vec::new(),
        seen: 0,
        unreadable: 0,
    };
    walk.collect(root);
    span.detail(format_args!(
        "entries={} unreadable={}",
        walk.out.len(),
        walk.unreadable
    ));
    (walk.out, walk.unreadable)
}

/// State threaded through a poll walk: the root, the shutdown flag, the
/// accumulated signature, and the stack of `.gitignore` layers active on the
/// directory currently being descended.
struct PollWalk<'a> {
    root: &'a Path,
    shutdown: &'a AtomicBool,
    out: Signature,
    layers: Vec<IgnoreLayer>,
    seen: usize,
    /// Directories whose `read_dir` failed this walk. Their contents are unseen;
    /// the poll loop surfaces the count so perf logs show the blindness.
    unreadable: usize,
}

impl PollWalk<'_> {
    /// Descend `dir`, adding its `.gitignore` to the layer stack, pruning
    /// directories matched by an active layer, and recording every file.
    fn collect(&mut self, dir: &Path) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let pushed = self.push_gitignore(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            // Blind this walk (permission denied, vanished, transient error):
            // keep going, but count it so the poll loop can surface it.
            self.unreadable += 1;
            if pushed {
                self.layers.pop();
            }
            return;
        };
        for entry in entries.flatten() {
            // Prune any `.git` directory (root or nested, e.g. a submodule)
            // so ref churn never leaks into the workdir signature.
            if entry.file_name() == ".git" {
                continue;
            }
            self.seen += 1;
            if self.seen.is_multiple_of(WALK_SHUTDOWN_CHECK)
                && self.shutdown.load(Ordering::Relaxed)
            {
                break;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let Ok(rel) = path.strip_prefix(self.root) else {
                continue;
            };
            if meta.is_dir() {
                if is_pruned(rel, &self.layers) {
                    continue;
                }
                self.collect(&path);
            } else {
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                self.out.insert(rel.to_path_buf(), (mtime, meta.len()));
            }
        }
        if pushed {
            self.layers.pop();
        }
    }

    /// Read `dir/.gitignore` and push its simple patterns as a layer. Returns
    /// false when the file is absent or contributes no usable pattern.
    fn push_gitignore(&mut self, dir: &Path) -> bool {
        let Ok(rel) = dir.strip_prefix(self.root) else {
            return false;
        };
        let path = dir.join(".gitignore");
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        // Only a regular file under the cap: a symlink to `/dev/zero` or a
        // multi-gigabyte file would otherwise hang or exhaust memory.
        if !meta.file_type().is_file() || meta.len() > MAX_GITIGNORE_BYTES {
            return false;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            return false;
        };
        let mut text = String::new();
        if file
            .take(MAX_GITIGNORE_BYTES)
            .read_to_string(&mut text)
            .is_err()
        {
            return false;
        }
        let (names, anchored, negated) = parse_gitignore(&text);
        if names.is_empty() && anchored.is_empty() && !negated {
            return false;
        }
        self.layers.push(IgnoreLayer {
            base: rel.to_path_buf(),
            names,
            anchored,
            negated,
        });
        true
    }
}

/// Extract the conservative, wildcard-free patterns from a `.gitignore` body.
/// Patterns containing `!`, `*`, `?`, or `[` are dropped outright: wildcards
/// and character classes are never used to prune, so any subtree they might
/// re-include is walked rather than wrongly skipped. A line starting with `!`
/// also sets the third return value, which disables pruning in the whole
/// subtree beneath the layer (a negation re-includes, so no positive pattern
/// from this or an ancestor layer may prune there).
fn parse_gitignore(text: &str) -> (Vec<String>, Vec<PathBuf>, bool) {
    let mut names = Vec::new();
    let mut anchored = Vec::new();
    let mut negated = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.chars().any(|c| matches!(c, '!' | '*' | '?' | '[')) {
            if line.starts_with('!') {
                negated = true;
            }
            continue;
        }
        let pat = line.strip_suffix('/').unwrap_or(line);
        if pat.is_empty() {
            continue;
        }
        if pat.contains('/') {
            // A slash (other than a trailing one) anchors the pattern to the
            // layer's directory; a leading slash is decorative.
            let anchored_pat = pat.strip_prefix('/').unwrap_or(pat);
            if !anchored_pat.is_empty() {
                anchored.push(PathBuf::from(anchored_pat));
            }
        } else {
            names.push(pat.to_string());
        }
    }
    (names, anchored, negated)
}

/// True when `dir_rel` (root-relative) matches an active ignore layer. Bare
/// names match the directory's own name at any depth; anchored paths match the
/// directory's path under the layer exactly.
fn is_pruned(dir_rel: &Path, layers: &[IgnoreLayer]) -> bool {
    // Any active negation disables all pruning beneath it: git applies
    // negations last across every layer, so a positive bare name from an
    // ancestor could otherwise prune a directory that a deeper negation
    // re-includes. Walking more than git is always safe.
    if layers.iter().any(|l| l.negated) {
        return false;
    }
    let name = dir_rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    for layer in layers {
        let Ok(layer_rel) = dir_rel.strip_prefix(&layer.base) else {
            continue;
        };
        if let Some(name) = &name {
            if layer.names.iter().any(|n| n == name) {
                return true;
            }
        }
        if layer.anchored.iter().any(|p| p.as_path() == layer_rel) {
            return true;
        }
    }
    false
}

/// Repo-relative paths whose signature entry appeared, disappeared, or changed.
fn signature_diff(prev: &Signature, next: &Signature) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, meta) in next {
        if prev.get(path) != Some(meta) {
            changed.push(path.clone());
        }
    }
    for path in prev.keys() {
        if !next.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed
}

/// Shared preamble of both watch loops: beat the perf counter at most once per
/// second, then check shutdown. Returns false when shutdown is set; the caller
/// breaks. The beat runs first, so the exiting pass still ticks it.
fn watch_tick(io: &WatchIo, last_beat: &mut Instant) -> bool {
    if last_beat.elapsed() >= Duration::from_secs(1) {
        perf::beat();
        *last_beat = Instant::now();
    }
    !io.shutdown.load(Ordering::Relaxed)
}

/// Shared preamble of both watch loops: flush the latched refs event and the
/// retained workdir batch with non-blocking sends, restoring a full batch for
/// the next pass. Returns false when the worker hung up (`Disconnected`) —
/// watching is over, and the caller returns.
fn flush_pending(io: &WatchIo, pending: &mut PendingPaths, refs_pending: &mut bool) -> bool {
    if *refs_pending {
        match io.out_tx.try_send(WorkerEvent::RefsChanged) {
            Ok(()) => {
                perf::tally(Counter::WatchRefs);
                *refs_pending = false;
            }
            Err(TrySendError::Full(_)) => {
                crate::perf_evt!("watch backpressure kind=refs");
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
    if !pending.is_empty() {
        let ev = WorkerEvent::WorkdirChanged(pending.take());
        match io.out_tx.try_send(ev) {
            Ok(()) => perf::tally(Counter::WatchWorkdir),
            Err(TrySendError::Full(ev)) => {
                crate::perf_evt!("watch backpressure kind=workdir");
                if let WorkerEvent::WorkdirChanged(p) = ev {
                    pending.restore(p);
                }
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
    true
}

/// Backstop ref poll shared by both loops: on the cadence gated by
/// `last_poll`, compare the probed ref files and latch `refs_pending` when any
/// moved, and latch an unattributable recompute in `pending` when the worktree
/// index or the shared exclude file was rewritten (view changes that are not
/// ref moves). The caller sends (non-blocking) on its next pass, so a stalled
/// worker cannot wedge the watcher here.
fn poll_refs(
    refs: &mut Option<RefProbe>,
    pending: &mut PendingPaths,
    last_poll: &mut Instant,
    refs_pending: &mut bool,
    backstop: Duration,
) {
    if last_poll.elapsed() >= backstop {
        *last_poll = Instant::now();
        let Some(probe) = refs.as_mut() else {
            return;
        };
        let change = probe.poll();
        if change.refs {
            *refs_pending = true;
        }
        if change.index || change.exclude {
            pending.push_unattributable();
        }
    }
}

/// What the backstop probe observed since the previous poll.
struct ProbeChange {
    /// A ref (HEAD, packed-refs, a loose ref, or the reftable store) moved.
    refs: bool,
    /// The worktree index was rewritten: a view change that is not a ref move.
    index: bool,
    /// The shared `info/exclude` file changed, which changes which untracked
    /// paths count but is not a ref move.
    exclude: bool,
}

/// Tracks the ref state a commit, fetch, pull, or branch switch moves. Takes a
/// stat-only signature of this worktree's `HEAD`, the shared `packed-refs` and
/// reftable table list, and every file under the loose ref trees, and reports
/// movement when anything differs. It also stats the worktree index and the
/// shared `info/exclude` file, which the degraded poll's workdir walk skips. It
/// deliberately does not resolve HEAD's symref: the base ref (e.g. `main`) is
/// usually not the checked-out branch, and its movement must be caught too.
///
/// A linked worktree's `HEAD` lives in its per-worktree admin dir and all
/// shared refs in the common dir, which is why [`GitAdmin`] carries both.
struct RefProbe {
    /// Files whose `(mtime, size)` alone signals ref movement.
    fixed: Vec<PathBuf>,
    /// Loose ref trees, walked for `(path, (mtime, size))` entries.
    trees: Vec<PathBuf>,
    /// The worktree index: a rewrite here changes the staged side of the view
    /// without moving any ref.
    index: PathBuf,
    /// The shared exclude file: a rewrite changes which untracked paths count.
    exclude: PathBuf,
    /// Last observed signature, sorted by path: `(mtime, size)` per ref file.
    last: Vec<(PathBuf, (Option<SystemTime>, u64))>,
    /// Last observed `(mtime, size)` of the worktree index.
    last_index: (Option<SystemTime>, u64),
    /// Last observed `(mtime, size)` of the exclude file.
    last_exclude: (Option<SystemTime>, u64),
}

impl RefProbe {
    /// Build a probe for the admin dirs and take the initial baseline, so the
    /// first poll reports only movement after construction.
    fn new(admin: &GitAdmin) -> Self {
        let mut trees = vec![admin.common_dir.join("refs")];
        if admin.git_dir != admin.common_dir {
            trees.push(admin.git_dir.join("refs"));
        }
        let mut probe = RefProbe {
            fixed: vec![
                admin.git_dir.join("HEAD"),
                admin.common_dir.join("packed-refs"),
                admin.common_dir.join("reftable").join("tables.list"),
            ],
            trees,
            index: admin.git_dir.join("index"),
            exclude: admin.common_dir.join("info").join("exclude"),
            last: Vec::new(),
            last_index: (None, 0),
            last_exclude: (None, 0),
        };
        probe.last = probe.signature();
        probe.last_index = stat_signature(&probe.index);
        probe.last_exclude = stat_signature(&probe.exclude);
        probe
    }

    /// Stat-only signature: every probe file plus every file under the loose
    /// ref trees, sorted by path so the comparison is order-independent. Each
    /// entry is `(mtime, size)`: the size disambiguates same-tick rewrites on
    /// coarse-mtime filesystems. A ref rewrite with `rename` changes the file's
    /// mtime; creation or deletion changes the set.
    fn signature(&self) -> Vec<(PathBuf, (Option<SystemTime>, u64))> {
        let mut sig = Vec::new();
        for path in &self.fixed {
            sig.push((path.clone(), stat_signature(path)));
        }
        for tree in &self.trees {
            collect_ref_files(tree, &mut sig);
        }
        sig.sort_by(|a, b| a.0.cmp(&b.0));
        sig
    }

    /// Compare the current signature and the index and exclude `(mtime, size)`
    /// against the previous poll, updating the baselines. Reports what moved.
    fn poll(&mut self) -> ProbeChange {
        let index = stat_signature(&self.index);
        let exclude = stat_signature(&self.exclude);
        let cur = self.signature();
        let change = ProbeChange {
            refs: cur != self.last,
            index: index != self.last_index,
            exclude: exclude != self.last_exclude,
        };
        self.last = cur;
        self.last_index = index;
        self.last_exclude = exclude;
        change
    }
}

/// Collect `(path, (mtime, size))` for every entry under a loose-ref tree,
/// bounded so a pathological ref count cannot make the 2s backstop unbounded.
/// Symlinked directories are not followed.
fn collect_ref_files(dir: &Path, out: &mut Vec<(PathBuf, (Option<SystemTime>, u64))>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Sort before the cap: `read_dir` order is not guaranteed, and flipping the
    // retained subset between polls would latch a spurious ref change.
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if out.len() >= MAX_REF_ENTRIES {
            return;
        }
        let Ok(link_meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if link_meta.is_dir() {
            collect_ref_files(&path, out);
        } else {
            // A symlinked ref is followed (git permits them); a symlinked
            // directory is not, matching the walk's no-follow stance.
            out.push((path.clone(), stat_signature(&path)));
        }
    }
}

/// `(mtime, size)` of `p`, following symlinks, or `(None, 0)` when it can't be
/// stat-ed. The size disambiguates rewrites that share a timestamp on
/// coarse-mtime filesystems, mirroring the workdir poll's signature.
fn stat_signature(p: &Path) -> (Option<SystemTime>, u64) {
    match std::fs::metadata(p) {
        Ok(meta) => (meta.modified().ok(), meta.len()),
        Err(_) => (None, 0),
    }
}

/// Classify a path (relative to the repo root) into the pending workdir batch
/// and the refs flag. `.git/HEAD`, `.git/packed-refs`, and anything under
/// `.git/refs/` or `.git/reftable/` set `refs` (any ref movement triggers a
/// rescan, so a freshly picked base stays live). `.git/index` and
/// `.git/info/exclude` force an unattributable recompute: staging or unstaging
/// changes the staged side of the view and the exclude file changes which
/// untracked paths count, without moving a file under the root. Any other path
/// containing a `.git` component — including a nested/submodule `.git` — is
/// ignored rather than treated as a workdir change; everything else is
/// accumulated as a workdir path. Components are compared directly, so no
/// string is built and the platform separator is handled by `Path` itself.
fn classify_into(rel: &Path, pending: &mut PendingPaths, refs: &mut bool) {
    // The index decides the staged side of the view and the local exclude file
    // changes which untracked paths count: both change what is shown without
    // touching a file under the root. Force an unattributable recompute so the
    // worker re-reads them.
    if rel == Path::new(".git/index") || rel == Path::new(".git/info/exclude") {
        pending.push_unattributable();
        return;
    }
    let git = std::ffi::OsStr::new(".git");
    let mut comps = rel.components();
    let (first, second, third) = (comps.next(), comps.next(), comps.next());
    let is_ref = match (first, second, third) {
        (Some(dir), Some(entry), _) if dir.as_os_str() == git => {
            let name = entry.as_os_str();
            if name == std::ffi::OsStr::new("HEAD") || name == std::ffi::OsStr::new("packed-refs") {
                third.is_none()
            } else {
                (name == std::ffi::OsStr::new("refs") || name == std::ffi::OsStr::new("reftable"))
                    && third.is_some()
            }
        }
        _ => false,
    };
    if is_ref {
        *refs = true;
        return;
    }
    if rel.components().any(|c| c.as_os_str() == git) {
        return;
    }
    pending.push_all(std::iter::once(rel.to_path_buf()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `classify_into` with fresh pending state; returns the pending
    /// batch and the refs latch.
    fn classify(rel: &Path) -> (PendingPaths, bool) {
        let mut pending = PendingPaths::default();
        let mut refs = false;
        classify_into(rel, &mut pending, &mut refs);
        (pending, refs)
    }

    /// A `WatchIo` for `absorb_event` tests, with an optional admin layout.
    fn test_io(root: &Path, admin: Option<GitAdmin>) -> WatchIo {
        let (tx, _rx) = bounded(1);
        WatchIo {
            repo_root: root.to_path_buf(),
            refs: admin,
            backstop: BACKSTOP_INTERVAL,
            out_tx: tx,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn classify_head_is_refs_changed() {
        let (pending, refs) = classify(Path::new(".git/HEAD"));
        assert!(refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn classify_packed_refs_is_refs_changed() {
        let (pending, refs) = classify(Path::new(".git/packed-refs"));
        assert!(refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn classify_any_head_ref_is_refs_changed() {
        // Any ref movement (not just the configured base) rescans, so a picked
        // branch stays live.
        let (pending, refs) = classify(Path::new(".git/refs/heads/main"));
        assert!(refs);
        assert!(pending.paths.is_empty());
        let (pending, refs) = classify(Path::new(".git/refs/heads/feature/x"));
        assert!(refs);
        assert!(pending.paths.is_empty());
        let (pending, refs) = classify(Path::new(".git/refs/tags/v1"));
        assert!(refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn read_access_events_are_ignored() {
        use notify::event::{AccessMode, CreateKind, ModifyKind};

        // Reads and read-only opens/closes carry no content change and must
        // not trigger a recompute.
        assert!(is_read_access(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(is_read_access(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(is_read_access(&EventKind::Access(AccessKind::Read)));
        assert!(is_read_access(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        // Unknown-mode closes are treated as reads too, matching Open(Any).
        assert!(is_read_access(&EventKind::Access(AccessKind::Close(
            AccessMode::Any
        ))));
        // A completed write, a write-mode open, or an actual mutation counts.
        assert!(!is_read_access(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!is_read_access(&EventKind::Access(AccessKind::Open(
            AccessMode::Write
        ))));
        assert!(!is_read_access(&EventKind::Access(AccessKind::Open(
            AccessMode::Other
        ))));
        assert!(!is_read_access(&EventKind::Modify(ModifyKind::Any)));
        assert!(!is_read_access(&EventKind::Create(CreateKind::File)));
    }

    #[test]
    fn classify_other_git_internals_ignored() {
        let (pending, refs) = classify(Path::new(".git/objects/xx/yyyy"));
        assert!(!refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn classify_index_forces_a_recompute() {
        let (mut pending, refs) = classify(Path::new(".git/index"));
        assert!(!refs);
        assert!(
            !pending.is_empty(),
            "an index change must force a recompute"
        );
        assert_eq!(pending.take(), Vec::<PathBuf>::new());
    }

    #[test]
    fn classify_reftable_is_refs_changed() {
        let (pending, refs) = classify(Path::new(".git/reftable/tables.list"));
        assert!(refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn classify_local_exclude_forces_a_recompute() {
        let (mut pending, refs) = classify(Path::new(".git/info/exclude"));
        assert!(!refs);
        assert!(!pending.is_empty());
        assert_eq!(pending.take(), Vec::<PathBuf>::new());
    }

    #[test]
    fn classify_nested_git_index_is_ignored() {
        let (pending, refs) = classify(Path::new("sub/.git/index"));
        assert!(!refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn classify_workdir_paths_carry_their_path() {
        let (pending, refs) = classify(Path::new("src/foo.rs"));
        assert!(!refs);
        assert_eq!(pending.paths, vec![PathBuf::from("src/foo.rs")]);
        let (pending, refs) = classify(Path::new("Cargo.toml"));
        assert!(!refs);
        assert_eq!(pending.paths, vec![PathBuf::from("Cargo.toml")]);
        let (pending, refs) = classify(Path::new("a/b/c/d.txt"));
        assert!(!refs);
        assert_eq!(pending.paths, vec![PathBuf::from("a/b/c/d.txt")]);
    }

    #[test]
    fn absorb_event_classifies_admin_paths_outside_the_root() {
        let admin = GitAdmin {
            git_dir: PathBuf::from("/common/worktrees/wt"),
            common_dir: PathBuf::from("/common"),
        };
        let io = test_io(Path::new("/wt"), Some(admin));
        let mut pending = PendingPaths::default();
        let mut refs = false;
        absorb_event(
            &io,
            &[
                PathBuf::from("/common/worktrees/wt/HEAD"),
                PathBuf::from("/common/refs/heads/main"),
                PathBuf::from("/common/packed-refs"),
                PathBuf::from("/common/objects/ab/cdef"),
            ],
            &mut pending,
            &mut refs,
        );
        assert!(refs, "a shared ref move outside the root must refire");
        assert!(
            pending.is_empty(),
            "objects churn must not force a recompute"
        );

        let mut pending = PendingPaths::default();
        let mut refs = false;
        absorb_event(
            &io,
            &[PathBuf::from("/common/worktrees/wt/index")],
            &mut pending,
            &mut refs,
        );
        assert!(!refs);
        assert!(
            !pending.is_empty(),
            "an index rewrite outside the root must force a recompute"
        );
        assert_eq!(pending.take(), Vec::<PathBuf>::new());

        let mut pending = PendingPaths::default();
        let mut refs = false;
        absorb_event(
            &io,
            &[PathBuf::from("/common/info/exclude")],
            &mut pending,
            &mut refs,
        );
        assert!(!refs);
        assert!(
            !pending.is_empty(),
            "a shared info/exclude rewrite must force a recompute"
        );
        assert_eq!(pending.take(), Vec::<PathBuf>::new());
    }

    #[test]
    fn ref_probe_detects_a_base_ref_move_when_head_stays() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(git.join("refs/heads/feature"), "0000\n").unwrap();
        let base = git.join("refs/heads/main");
        std::fs::write(&base, "1111\n").unwrap();

        let admin = GitAdmin {
            git_dir: git.clone(),
            common_dir: git.clone(),
        };
        let mut probe = RefProbe::new(&admin);
        let change = probe.poll();
        assert!(
            !change.refs && !change.index,
            "a stable repo must not report a change"
        );

        // Pulling `main` rewrites its ref file while HEAD stays on `feature`;
        // the base is not HEAD's target, so it must be probed on its own.
        std::fs::File::options()
            .write(true)
            .open(&base)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        assert!(probe.poll().refs, "a base ref move must fire");
        let change = probe.poll();
        assert!(
            !change.refs && !change.index,
            "the new baseline must be stable"
        );
    }

    #[test]
    fn ref_probe_detects_a_same_timestamp_rewrite_by_size() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let base = git.join("refs/heads/main");
        std::fs::write(&base, "0000\n").unwrap();
        let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);
        std::fs::File::options()
            .write(true)
            .open(&base)
            .unwrap()
            .set_modified(stamp)
            .unwrap();

        let admin = GitAdmin {
            git_dir: git.clone(),
            common_dir: git,
        };
        let mut probe = RefProbe::new(&admin);
        assert!(!probe.poll().refs);

        // Same timestamp, different content length: only the size half of the
        // signature can catch this on a coarse-mtime filesystem.
        std::fs::write(&base, "0000\n1111\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&base)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert!(probe.poll().refs, "a same-timestamp rewrite must be caught");
    }

    #[test]
    fn ref_probe_tracks_a_linked_worktrees_admin_and_common_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("main/.git");
        let admin_dir = common.join("worktrees/wt");
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        std::fs::create_dir_all(&admin_dir).unwrap();
        std::fs::write(admin_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(admin_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(admin_dir.join("index"), b"index").unwrap();
        let base = common.join("refs/heads/main");
        std::fs::write(&base, "1111\n").unwrap();

        let admin = GitAdmin {
            git_dir: admin_dir.clone(),
            common_dir: common.clone(),
        };
        let mut probe = RefProbe::new(&admin);
        let change = probe.poll();
        assert!(
            !change.refs && !change.index,
            "a stable linked worktree must be quiet"
        );

        std::fs::File::options()
            .write(true)
            .open(&base)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        assert!(probe.poll().refs, "a common-dir base ref move must fire");

        std::fs::write(admin_dir.join("HEAD"), "ref: refs/heads/other\n").unwrap();
        assert!(probe.poll().refs, "a per-worktree HEAD rewrite must fire");

        std::fs::write(admin_dir.join("index"), b"index2").unwrap();
        let change = probe.poll();
        assert!(change.index, "an index rewrite must be reported");
        assert!(!change.refs, "an index rewrite is not a ref move");
    }

    #[test]
    fn ref_probe_detects_packed_refs_and_reftable_movement() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::create_dir_all(git.join("reftable")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs/heads/main"), "0000\n").unwrap();
        std::fs::write(git.join("packed-refs"), "# pack-refs\n").unwrap();
        std::fs::write(git.join("reftable/tables.list"), "table\n").unwrap();

        let admin = GitAdmin {
            git_dir: git.clone(),
            common_dir: git.clone(),
        };
        let mut probe = RefProbe::new(&admin);
        let change = probe.poll();
        assert!(!change.refs && !change.index);

        let bump = |path: &Path| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000))
                .unwrap();
        };
        bump(&git.join("packed-refs"));
        assert!(probe.poll().refs, "a packed-refs rewrite must fire");

        bump(&git.join("reftable/tables.list"));
        assert!(probe.poll().refs, "a reftable table-list rewrite must fire");
    }

    #[test]
    fn ref_probe_reports_the_exclude_file_separately() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::create_dir_all(git.join("info")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs/heads/main"), "0000\n").unwrap();
        std::fs::write(git.join("info/exclude"), "target/\n").unwrap();

        let admin = GitAdmin {
            git_dir: git.clone(),
            common_dir: git.clone(),
        };
        let mut probe = RefProbe::new(&admin);
        assert!(!probe.poll().exclude);

        std::fs::write(git.join("info/exclude"), "target/\nbuild/\n").unwrap();
        let change = probe.poll();
        assert!(change.exclude, "an exclude rewrite must be reported");
        assert!(
            !change.refs,
            "an exclude rewrite is not a ref move (the worker rebuilds the \
             exclude chain only for unattributable windows)"
        );
    }

    #[test]
    fn extra_watches_skip_the_worktree_root_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wt");
        let common = dir.path().join("repo/.git");
        let admin_dir = common.join("worktrees/wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&admin_dir).unwrap();
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::write(admin_dir.join("HEAD"), "ref: refs/heads/f\n").unwrap();

        let admin = GitAdmin {
            git_dir: admin_dir.clone(),
            common_dir: common.clone(),
        };
        let watches = admin.extra_watches(&root);
        assert!(watches.contains(&(admin_dir, RecursiveMode::NonRecursive)));
        assert!(watches.contains(&(common.clone(), RecursiveMode::NonRecursive)));
        assert!(watches.contains(&(common.join("refs"), RecursiveMode::Recursive)));
        assert!(watches.contains(&(common.join("info"), RecursiveMode::NonRecursive)));
        assert!(
            watches.iter().all(|(p, _)| !p.starts_with(&root)),
            "nothing under the worktree root may be watched twice"
        );
    }

    #[test]
    fn extra_watches_empty_for_the_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::create_dir_all(git.join("info")).unwrap();
        let admin = GitAdmin {
            git_dir: git.clone(),
            common_dir: git,
        };
        assert!(
            admin.extra_watches(&root).is_empty(),
            "the recursive workdir watch already covers <root>/.git"
        );
    }

    #[test]
    fn watcher_sees_a_commit_outside_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        crate::git::test_util::repo_with_commits(dir.path(), &["one\n"]);
        crate::git::test_util::run_git(dir.path(), &["worktree", "add", "-b", "feature", "wt"]);
        let wt = std::fs::canonicalize(dir.path().join("wt")).unwrap();
        assert!(
            wt.join(".git").is_file(),
            "a linked worktree's .git must be a file"
        );

        let poll_flag = Arc::new(AtomicBool::new(false));
        let watcher =
            Watcher::start_with_backstop(wt.clone(), poll_flag.clone(), Duration::from_secs(3600))
                .unwrap();
        let rx = watcher.receiver().clone();
        // The watch thread arms its watches asynchronously; a workdir event
        // proves the loop is live (all watches are installed before the loop starts).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            std::fs::write(wt.join("ping.txt"), "ping\n").unwrap();
            if rx.recv_timeout(Duration::from_millis(100)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "watcher never armed");
        }
        while rx.try_recv().is_ok() {}

        // Advancing `main` in the main checkout moves only a shared ref, in the
        // common dir outside this worktree's root. The backstop is effectively
        // disabled, so only the notify admin watches can deliver the event.
        crate::git::test_util::run_git(dir.path(), &["commit", "--allow-empty", "-m", "advance"]);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::RefsChanged) => break,
                Ok(WorkerEvent::WorkdirChanged(_)) => {}
                Err(RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "a commit in the main worktree must reach the linked worktree's watcher"
                    );
                }
                Err(RecvTimeoutError::Disconnected) => panic!("watcher closed"),
            }
        }
        assert!(
            !poll_flag.load(Ordering::Relaxed),
            "notify watches must deliver this, not a poll-mode fallback"
        );
    }

    #[test]
    fn signature_unchanged_tree_has_empty_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "two\n").unwrap();

        let (first, _) = walk_signature(dir.path(), &AtomicBool::new(false));
        let (second, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(signature_diff(&first, &second).is_empty());
    }

    #[test]
    fn signature_detects_create_modify_delete() {
        let dir = tempfile::tempdir().unwrap();
        let touched = dir.path().join("a.txt");
        std::fs::write(&touched, "one\n").unwrap();
        let (mut prev, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        // Create: the new path appears.
        std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
        let (next, _) = walk_signature(dir.path(), &AtomicBool::new(false));
        assert_eq!(signature_diff(&prev, &next), vec![PathBuf::from("b.txt")]);
        prev = next;

        // Modify (size differs, so the change is visible even at coarse mtime
        // resolution).
        std::fs::write(&touched, "one\ntwo\n").unwrap();
        let (next, _) = walk_signature(dir.path(), &AtomicBool::new(false));
        assert_eq!(signature_diff(&prev, &next), vec![PathBuf::from("a.txt")]);
        prev = next;

        // Delete: the vanished path is reported.
        std::fs::remove_file(&touched).unwrap();
        let (next, _) = walk_signature(dir.path(), &AtomicBool::new(false));
        assert_eq!(signature_diff(&prev, &next), vec![PathBuf::from("a.txt")]);
    }

    #[test]
    fn signature_skips_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "x\n").unwrap();

        let (sig, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(sig.contains_key(Path::new("tracked.txt")));
        assert!(!sig.keys().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn signature_skips_nested_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/.git")).unwrap();
        std::fs::write(dir.path().join("sub/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.path().join("sub/tracked.txt"), "x\n").unwrap();

        let (sig, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(sig.contains_key(Path::new("sub/tracked.txt")));
        assert!(
            !sig.keys()
                .any(|p| p.components().any(|c| c.as_os_str() == ".git")),
            "a nested .git must be pruned from the walk"
        );
    }

    #[test]
    fn classify_nested_git_is_not_workdir() {
        // A submodule's `.git` internals must not surface as workdir changes.
        let (pending, refs) = classify(Path::new("sub/.git/HEAD"));
        assert!(!refs);
        assert!(pending.paths.is_empty());
        let (pending, refs) = classify(Path::new("sub/.git/index"));
        assert!(!refs);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn absorb_event_accumulates_workdir_paths_and_refs() {
        let root = Path::new("/repo");
        let mut pending = PendingPaths::default();
        let mut refs = false;

        absorb_event(
            &test_io(root, None),
            &[
                PathBuf::from("/repo/src/a.rs"),
                PathBuf::from("/repo/src/a.rs"),
                PathBuf::from("/repo/.git/HEAD"),
            ],
            &mut pending,
            &mut refs,
        );

        assert_eq!(
            pending.paths,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/a.rs")]
        );
        assert!(refs, "HEAD must be classified as a ref change");
    }

    #[test]
    fn absorb_event_index_collapses_to_unattributable() {
        // An index rewrite forces a recompute; the already-accumulated paths
        // are superseded by the unattributable batch.
        let mut pending = PendingPaths::default();
        let mut refs = false;

        absorb_event(
            &test_io(Path::new("/repo"), None),
            &[
                PathBuf::from("/repo/src/a.rs"),
                PathBuf::from("/repo/.git/index"),
            ],
            &mut pending,
            &mut refs,
        );

        assert!(!refs);
        assert!(!pending.is_empty());
        assert_eq!(pending.take(), Vec::<PathBuf>::new());
    }

    #[test]
    fn dedup_paths_removes_duplicates() {
        let mut paths = vec![PathBuf::from("b"), PathBuf::from("a"), PathBuf::from("b")];
        dedup_paths(&mut paths);
        assert_eq!(paths, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn pending_paths_take_dedups() {
        let mut pending = PendingPaths::default();
        pending.push_all(vec![
            PathBuf::from("b"),
            PathBuf::from("a"),
            PathBuf::from("b"),
        ]);
        assert_eq!(pending.take(), vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert!(pending.is_empty());
    }

    #[test]
    fn pending_paths_overflow_collapses_to_unattributable() {
        let mut pending = PendingPaths::default();
        pending.push_all((0..PENDING_CAP).map(|i| PathBuf::from(format!("p{i}"))));
        pending.push_all(vec![PathBuf::from("later")]);
        // Overflowed: the batch is the empty-path form and new paths drop.
        assert_eq!(pending.take(), Vec::<PathBuf>::new());
        assert!(pending.is_empty());
    }

    #[test]
    fn pending_paths_restore_rearms_overflow() {
        let mut pending = PendingPaths::default();
        pending.push_all((0..PENDING_CAP).map(|i| PathBuf::from(format!("p{i}"))));
        pending.push_all(vec![PathBuf::from("later")]);
        let batch = pending.take();
        assert!(batch.is_empty());
        pending.restore(batch);
        assert!(!pending.is_empty(), "a rejected batch must be retried");
    }

    /// A tempdir with a root `.gitignore` (`node_modules/`, `/build`), a nested
    /// `.gitignore` (`cache/`) and files both under and outside those trees.
    fn ignored_dir_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n/build\n").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        std::fs::write(dir.path().join("node_modules/x/y"), "dep\n").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/z"), "obj\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        // A `cache/` at the root is *not* ignored: only the nested `.gitignore`
        // establishes that name, and only beneath `sub/`.
        std::fs::create_dir_all(dir.path().join("cache")).unwrap();
        std::fs::write(dir.path().join("cache/keep"), "keep\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub/cache")).unwrap();
        std::fs::write(dir.path().join("sub/cache/f"), "ignored\n").unwrap();
        std::fs::write(dir.path().join("sub/.gitignore"), "cache/\n").unwrap();
        std::fs::write(dir.path().join("sub/keep.rs"), "fn keep() {}\n").unwrap();
        dir
    }

    #[test]
    fn parse_gitignore_drops_wildcards_and_negations() {
        let (names, anchored, negated) = parse_gitignore("!important\n*.log\nfoo?\nbar[0]\n");
        assert!(names.is_empty(), "non-simple patterns must never be parsed");
        assert!(anchored.is_empty());
        assert!(negated, "a `!` line must disable pruning for the subtree");

        let (names, anchored, negated) = parse_gitignore("# comment\n\ntarget\nnode_modules/\n");
        assert_eq!(
            names,
            vec!["target".to_string(), "node_modules".to_string()]
        );
        assert!(anchored.is_empty());
        assert!(!negated);
    }

    #[test]
    fn parse_gitignore_anchors_patterns_with_a_slash() {
        let (names, anchored, negated) = parse_gitignore("/build\ntests/system/cache/\n");
        assert!(names.is_empty());
        assert!(!negated);
        assert_eq!(
            anchored,
            vec![PathBuf::from("build"), PathBuf::from("tests/system/cache")]
        );
    }

    #[test]
    fn bare_name_prunes_a_dir_at_any_depth() {
        let layers = vec![IgnoreLayer {
            base: PathBuf::new(),
            names: vec!["node_modules".to_string()],
            anchored: Vec::new(),
            negated: false,
        }];
        assert!(is_pruned(Path::new("node_modules"), &layers));
        assert!(is_pruned(Path::new("a/b/node_modules"), &layers));
        assert!(!is_pruned(Path::new("a/b/src"), &layers));
    }

    #[test]
    fn anchored_pattern_prunes_only_at_the_layer_root() {
        let layers = vec![IgnoreLayer {
            base: PathBuf::new(),
            names: Vec::new(),
            anchored: vec![PathBuf::from("build")],
            negated: false,
        }];
        assert!(is_pruned(Path::new("build"), &layers));
        assert!(!is_pruned(Path::new("sub/build"), &layers));
    }

    #[test]
    fn nested_layer_prunes_only_under_it() {
        let layers = vec![IgnoreLayer {
            base: PathBuf::from("sub"),
            names: vec!["cache".to_string()],
            anchored: Vec::new(),
            negated: false,
        }];
        assert!(is_pruned(Path::new("sub/cache"), &layers));
        assert!(!is_pruned(Path::new("cache"), &layers));
        assert!(!is_pruned(Path::new("other/cache"), &layers));
    }

    #[test]
    fn dir_matching_nothing_is_walked() {
        let layers = vec![IgnoreLayer {
            base: PathBuf::new(),
            names: vec!["target".to_string()],
            anchored: vec![PathBuf::from("build")],
            negated: false,
        }];
        assert!(!is_pruned(Path::new("src"), &layers));
        assert!(!is_pruned(Path::new("src/targets"), &layers));
    }

    #[test]
    fn walk_prunes_ignored_dirs() {
        let dir = ignored_dir_repo();
        let (sig, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(sig.contains_key(Path::new("src/main.rs")));
        assert!(sig.contains_key(Path::new("sub/keep.rs")));
        assert!(sig.contains_key(Path::new("cache/keep")));
        assert!(
            !sig.keys().any(|p| p.starts_with("node_modules")),
            "an ignored directory must be pruned whole, not just its files"
        );
        assert!(
            !sig.keys().any(|p| p.starts_with("build")),
            "an anchored ignore pattern must prune its directory"
        );
        assert!(
            !sig.keys().any(|p| p.starts_with("sub/cache")),
            "a nested .gitignore must prune only under its own directory"
        );
    }

    #[test]
    fn walk_without_gitignore_does_not_prune() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        std::fs::write(dir.path().join("node_modules/x/y"), "dep\n").unwrap();

        let (sig, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(sig.contains_key(Path::new("node_modules/x/y")));
    }

    #[test]
    fn walk_keeps_negated_bare_name_in_same_layer() {
        // Root `.gitignore` has `foo` but also `!foo`: git keeps `foo`, so the
        // walk must not prune it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "foo\n!foo\n").unwrap();
        std::fs::create_dir_all(dir.path().join("foo")).unwrap();
        std::fs::write(dir.path().join("foo/x"), "kept\n").unwrap();

        let (sig, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(
            sig.contains_key(Path::new("foo/x")),
            "a negation re-includes foo"
        );
    }

    #[test]
    fn walk_keeps_ancestor_pruned_name_negated_deeper() {
        // Root ignores `foo`; `sub/.gitignore` negates it. git keeps
        // `sub/foo`, so the ancestor bare name must not prune that subtree.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "foo\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub/foo")).unwrap();
        std::fs::write(dir.path().join("sub/.gitignore"), "!foo\n").unwrap();
        std::fs::write(dir.path().join("sub/foo/x"), "kept\n").unwrap();
        std::fs::create_dir_all(dir.path().join("foo")).unwrap();
        std::fs::write(dir.path().join("foo/y"), "ignored\n").unwrap();

        let (sig, _) = walk_signature(dir.path(), &AtomicBool::new(false));

        assert!(
            sig.contains_key(Path::new("sub/foo/x")),
            "a deeper negation must veto the ancestor prune"
        );
        // The negation lives under `sub`, so it only disables pruning in that
        // subtree: the root `foo` stays pruned, exactly as git ignores it.
        assert!(!sig.contains_key(Path::new("foo/y")));
    }

    #[test]
    fn negation_in_layer_disables_its_own_positives() {
        // A layer with any negation disables pruning beneath it entirely; the
        // documented conservative rule. Pinned so the behavior can't drift.
        let layers = vec![IgnoreLayer {
            base: PathBuf::new(),
            names: vec!["target".to_string()],
            anchored: Vec::new(),
            negated: true,
        }];
        assert!(!is_pruned(Path::new("target"), &layers));
    }

    #[test]
    fn walk_counts_unreadable_root() {
        let (sig, unreadable) = walk_signature(
            Path::new("/nonexistent-bough-walk-root"),
            &AtomicBool::new(false),
        );
        assert!(sig.is_empty());
        assert_eq!(
            unreadable, 1,
            "an unreadable root must be counted, not hidden"
        );
    }

    #[test]
    fn walk_aborts_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let shutdown = AtomicBool::new(true);

        let (sig, _) = walk_signature(dir.path(), &shutdown);

        assert!(sig.is_empty(), "a set shutdown flag must abort the walk");
    }

    #[test]
    fn wait_done_times_out_on_an_empty_channel() {
        let (_tx, rx) = bounded::<()>(1);
        assert!(wait_done(&rx, Duration::ZERO).is_err());
    }

    #[test]
    fn wait_done_observes_completion() {
        let (tx, rx) = bounded::<()>(1);
        tx.send(()).unwrap();
        assert!(wait_done(&rx, Duration::ZERO).is_ok());
    }

    #[test]
    fn wait_done_treats_a_dropped_sender_as_done() {
        let (tx, rx) = bounded::<()>(1);
        drop(tx);
        assert!(wait_done(&rx, Duration::ZERO).is_ok());
    }

    #[test]
    fn next_poll_interval_bounds_duty_cycle_by_walk() {
        // A 600ms walk must push the next poll out to at least 2x the walk.
        assert_eq!(
            next_poll_interval(Duration::from_millis(600)),
            Duration::from_millis(1200)
        );
        // A fast walk leaves the floor interval unchanged.
        assert_eq!(next_poll_interval(Duration::from_millis(10)), POLL_INTERVAL);
    }

    #[test]
    fn start_with_flag_resets_caller_flag() {
        let dir = tempfile::tempdir().unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        let _watcher = Watcher::start_with_flag(dir.path().to_path_buf(), flag.clone()).unwrap();

        // A new watcher starts un-degraded, even when the shared flag still
        // reads true from a previous root's degradation.
        assert!(!flag.load(Ordering::Relaxed));
    }
}
