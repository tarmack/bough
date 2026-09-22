//! Filesystem watching for live updates.
//!
//! Produces a stream of `WorkerEvent`s: either the workdir changed (a
//! tracked/untracked file moved, carrying the changed paths) or the git refs
//! moved (`HEAD`, `packed-refs`, or any ref under `.git/refs/`). The worker
//! thread consumes these and recomputes. Two layers:
//!
//! - **`notify` (recommended watcher)** watches the repo root recursively.
//!   Paths under `.git/` are ignored except `.git/HEAD`, `.git/packed-refs`,
//!   and anything under `.git/refs/`, which become `RefsChanged`.
//! - **Backstop poll**: every 2s, stat `.git/HEAD` *and* the ref file its
//!   symref targets (plus `.git/packed-refs`), because a commit moves the ref
//!   file without touching `HEAD`'s mtime. If `notify` missed a ref change
//!   (some filesystems are flaky), the poll catches it.
//!
//! When `notify` init or its recursive `watch()` fails (unsupported
//! filesystem, watch-table exhaustion), the watcher degrades to a workdir poll
//! and flips the shared `poll_mode_flag()` the UI reads, so the footer hints
//! at degradation the moment it happens rather than at spawn time.
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{recommended_watcher, RecursiveMode};

use crate::perf::{self, Counter};

/// Classification of what moved. A workdir event carries the repo-relative
/// paths that changed so the worker can ignore-filter a recompute; refs are
/// coarse (any ref movement triggers a rescan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    /// Any non-`.git` path changed (tracked or untracked file), with the
    /// repo-relative paths that changed (empty when unattributable).
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

/// Owns the filesystem watcher thread. Dropping it signals shutdown and waits
/// a bounded time for the thread to finish.
pub struct Watcher {
    rx: Receiver<WorkerEvent>,
    poll_mode: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Completion signal from the watch thread, so `drop` can tell a finished
    /// thread from one wedged in a syscall.
    done_rx: Receiver<()>,
}

impl Watcher {
    /// Start watching `repo_root`.
    pub fn start(repo_root: PathBuf) -> Result<Watcher, WatchError> {
        let poll_mode = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = bounded::<WorkerEvent>(128);
        let (done_tx, done_rx) = bounded::<()>(1);

        let poll_mode_t = poll_mode.clone();
        let shutdown_t = shutdown.clone();
        let handle = thread::Builder::new()
            .name("bough-watch".into())
            .spawn(move || {
                let io = WatchIo {
                    repo_root,
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
            poll_mode,
            shutdown,
            handle: Some(handle),
            done_rx,
        })
    }

    /// Borrow the event receiver. The worker drains this.
    pub fn receiver(&self) -> &Receiver<WorkerEvent> {
        &self.rx
    }

    /// The shared degradation flag. The worker/UI clone this `Arc` and read it
    /// every frame, so they see a mid-run degradation the moment it happens.
    pub fn poll_mode_flag(&self) -> Arc<AtomicBool> {
        self.poll_mode.clone()
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

/// Shared thread inputs threaded through the orchestration: where to watch,
/// where to emit events, and the shutdown flag.
struct WatchIo {
    repo_root: PathBuf,
    out_tx: Sender<WorkerEvent>,
    shutdown: Arc<AtomicBool>,
}

/// Thread entry: try `notify`; on success run the forward+backstop loop, else
/// fall back to the 1s poll loop.
fn run_loop(io: WatchIo, poll_mode: Arc<AtomicBool>) {
    let (raw_tx, raw_rx) = bounded::<notify::Result<notify::Event>>(128);
    match recommended_watcher(move |res: notify::Result<notify::Event>| {
        let _ = raw_tx.send(res);
    }) {
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

/// `notify`-driven forward loop plus the 2s backstop ref poll.
fn run_notify_loop(
    io: &WatchIo,
    watcher: notify::RecommendedWatcher,
    raw_rx: Receiver<notify::Result<notify::Event>>,
) {
    // Keep the watcher alive for the life of the loop.
    let _watcher = watcher;

    let mut refs = RefProbe::new(&io.repo_root.join(".git"));
    let mut last_poll = Instant::now();
    // Workdir paths observed since the last successful send. A send dropped
    // under backpressure restores them here so the worker can't miss the final
    // change in a burst (only refs have a poll backstop, not the workdir).
    let mut pending = PendingPaths::default();
    let mut refs_pending = false;
    let mut last_beat = Instant::now();

    loop {
        if last_beat.elapsed() >= Duration::from_secs(1) {
            perf::beat();
            last_beat = Instant::now();
        }
        if io.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if refs_pending {
            match io.out_tx.try_send(WorkerEvent::RefsChanged) {
                Ok(()) => {
                    perf::tally(Counter::WatchRefs);
                    refs_pending = false;
                }
                Err(TrySendError::Full(_)) => {
                    crate::perf_evt!("watch backpressure kind=refs");
                }
                Err(TrySendError::Disconnected(_)) => return,
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
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        match raw_rx.recv_timeout(SHUTDOWN_TICK) {
            Ok(Ok(event)) => {
                perf::tally(Counter::WatchRawEvents);
                if is_read_access(&event.kind) {
                    perf::tally(Counter::WatchReadFiltered);
                } else {
                    absorb_event(&io.repo_root, &event.paths, &mut pending, &mut refs_pending);
                }
            }
            Ok(Err(_)) => { /* notify internal error; keep going */ }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        poll_refs(&mut refs, &mut last_poll, &mut refs_pending);
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
/// paths and the `refs` flag. Paths are made relative to `root`; `.git`
/// internals and unrecognised paths are dropped.
fn absorb_event(root: &Path, paths: &[PathBuf], pending: &mut PendingPaths, refs: &mut bool) {
    for path in paths {
        let rel = path.strip_prefix(root).unwrap_or(path);
        match classify(rel) {
            Some(WorkerEvent::WorkdirChanged(p)) => pending.push_all(p),
            Some(WorkerEvent::RefsChanged) => *refs = true,
            None => {}
        }
    }
}

/// Sort and remove duplicate paths from an accumulated workdir batch.
fn dedup_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

/// Accumulated workdir paths awaiting a send, plus the overdue-batch latch.
/// While the out channel is full the batch is retried; past [`PENDING_CAP`] it
/// collapses to the empty-path form (`Vec::new()`, i.e. "unattributable,
/// recompute") and new paths are dropped until the next successful send, so a
/// sustained burst can't grow the retry buffer without bound.
#[derive(Default)]
struct PendingPaths {
    paths: Vec<PathBuf>,
    /// True once the batch overflowed: it is now the empty-path unattributable
    /// form, and further paths are dropped until it is sent.
    overflowed: bool,
}

impl PendingPaths {
    /// Record changed paths, collapsing to the unattributable form on overflow.
    fn push_all(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.overflowed {
            return;
        }
        for path in paths {
            if self.paths.len() >= PENDING_CAP {
                self.paths.clear();
                self.overflowed = true;
                return;
            }
            self.paths.push(path);
        }
    }

    /// True when there is a batch (paths or the overflow latch) to send.
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.overflowed
    }

    /// Take the batch for a send: the deduped paths, or an empty `Vec` for an
    /// overflowed (unattributable) batch.
    fn take(&mut self) -> Vec<PathBuf> {
        if self.overflowed {
            self.overflowed = false;
            self.paths.clear();
            Vec::new()
        } else {
            dedup_paths(&mut self.paths);
            std::mem::take(&mut self.paths)
        }
    }

    /// Restore a batch rejected by `try_send`: an empty batch re-arms the
    /// overflow latch, otherwise the paths are kept for the next retry.
    fn restore(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.overflowed = true;
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
    let mut refs = RefProbe::new(&io.repo_root.join(".git"));
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
        if last_beat.elapsed() >= Duration::from_secs(1) {
            perf::beat();
            last_beat = Instant::now();
        }
        if io.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if refs_pending {
            match io.out_tx.try_send(WorkerEvent::RefsChanged) {
                Ok(()) => {
                    perf::tally(Counter::WatchRefs);
                    refs_pending = false;
                }
                Err(TrySendError::Full(_)) => {
                    crate::perf_evt!("watch backpressure kind=refs");
                }
                Err(TrySendError::Disconnected(_)) => return,
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
                Err(TrySendError::Disconnected(_)) => return,
            }
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
        poll_refs(&mut refs, &mut last_refs, &mut refs_pending);
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
/// links. `.git` is skipped entirely (ref movement is the backstop poll's job).
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
        let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) else {
            return false;
        };
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

/// Backstop ref poll shared by both loops: on the cadence gated by
/// `last_poll`, compare the probed ref files and latch `refs_pending` when any
/// moved. The caller sends (non-blocking) on its next pass, so a stalled worker
/// cannot wedge the watcher here.
fn poll_refs(refs: &mut RefProbe, last_poll: &mut Instant, refs_pending: &mut bool) {
    if last_poll.elapsed() >= BACKSTOP_INTERVAL {
        *last_poll = Instant::now();
        if refs.poll() {
            *refs_pending = true;
        }
    }
}

/// Tracks the git ref files a commit actually moves. `.git/HEAD` is usually a
/// symref (`ref: refs/heads/<branch>`); a commit rewrites the target ref file
/// without touching `HEAD`'s mtime, so stat-ing HEAD alone misses commits. The
/// probe stats HEAD, `packed-refs`, and the resolved symref target, and
/// re-resolves the target whenever HEAD's own mtime moves (a branch switch or
/// detach rewrites HEAD).
struct RefProbe {
    head: PathBuf,
    packed_refs: PathBuf,
    /// The ref file HEAD points at, or `None` for a detached HEAD.
    target: Option<PathBuf>,
    /// HEAD's mtime when `target` was last resolved; meaningful once `resolved`.
    resolved_for: Option<SystemTime>,
    resolved: bool,
    /// Last observed mtimes of `(HEAD, packed-refs, target)`.
    last: (Option<SystemTime>, Option<SystemTime>, Option<SystemTime>),
}

impl RefProbe {
    /// Build a probe for the git dir and take the initial baseline, so the
    /// first poll reports only movement after construction.
    fn new(git_dir: &Path) -> Self {
        let mut probe = RefProbe {
            head: git_dir.join("HEAD"),
            packed_refs: git_dir.join("packed-refs"),
            target: None,
            resolved_for: None,
            resolved: false,
            last: (None, None, None),
        };
        probe.refresh_target();
        probe.last = probe.stat();
        probe
    }

    /// Re-read HEAD's `ref:` line only when HEAD's own mtime moved, so a branch
    /// switch is picked up while the common case reads no file.
    fn refresh_target(&mut self) {
        let head_mtime = stat_mtime(&self.head);
        if self.resolved && self.resolved_for == head_mtime {
            return;
        }
        self.resolved = true;
        self.resolved_for = head_mtime;
        self.target = resolve_head_target(&self.head);
    }

    fn stat(&self) -> (Option<SystemTime>, Option<SystemTime>, Option<SystemTime>) {
        (
            stat_mtime(&self.head),
            stat_mtime(&self.packed_refs),
            self.target.as_deref().and_then(stat_mtime),
        )
    }

    /// Compare the probed ref mtimes against the previous poll, updating the
    /// baseline. True when any moved (created, deleted, or rewritten).
    fn poll(&mut self) -> bool {
        self.refresh_target();
        let cur = self.stat();
        let changed = mtime_changed(self.last.0, cur.0)
            || mtime_changed(self.last.1, cur.1)
            || mtime_changed(self.last.2, cur.2);
        self.last = cur;
        changed
    }
}

/// Resolve `.git/HEAD`'s `ref: <path>` symref to the ref file it points at
/// (relative to HEAD's directory), or `None` for a detached HEAD (bare object
/// id) or an unreadable HEAD.
fn resolve_head_target(head: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(head).ok()?;
    let rest = text.strip_prefix("ref:")?.trim();
    if rest.is_empty() {
        return None;
    }
    Some(head.parent()?.join(rest))
}

/// mtime of `p`, or `None` when it can't be stat-ed.
fn stat_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

/// True when `cur` differs from `prev` (ref moved/created/deleted). Both
/// `None` (never seen, still absent) means no change.
fn mtime_changed(prev: Option<SystemTime>, cur: Option<SystemTime>) -> bool {
    prev != cur
}

/// Classify a path (relative to the repo root) into a `WorkerEvent`, or
/// `None` to ignore. `.git/HEAD`, `.git/packed-refs`, and anything under
/// `.git/refs/` are `RefsChanged` (any ref movement triggers a rescan, so a
/// freshly picked base stays live). Any path containing a `.git` component —
/// including a nested/submodule `.git` — is ignored rather than treated as a
/// workdir change; everything else is `WorkdirChanged` carrying the path.
fn classify(rel: &Path) -> Option<WorkerEvent> {
    // Normalise separators so the string comparisons work on Windows too.
    let s = rel.to_string_lossy().replace('\\', "/");
    if s == ".git/HEAD" || s == ".git/packed-refs" || s.starts_with(".git/refs/") {
        return Some(WorkerEvent::RefsChanged);
    }
    if rel
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
    {
        return None;
    }
    Some(WorkerEvent::WorkdirChanged(vec![rel.to_path_buf()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_head_is_refs_changed() {
        assert_eq!(
            classify(Path::new(".git/HEAD")),
            Some(WorkerEvent::RefsChanged)
        );
    }

    #[test]
    fn classify_packed_refs_is_refs_changed() {
        assert_eq!(
            classify(Path::new(".git/packed-refs")),
            Some(WorkerEvent::RefsChanged)
        );
    }

    #[test]
    fn classify_any_head_ref_is_refs_changed() {
        // Any ref movement (not just the configured base) rescans, so a picked
        // branch stays live.
        assert_eq!(
            classify(Path::new(".git/refs/heads/main")),
            Some(WorkerEvent::RefsChanged)
        );
        assert_eq!(
            classify(Path::new(".git/refs/heads/feature/x")),
            Some(WorkerEvent::RefsChanged)
        );
        assert_eq!(
            classify(Path::new(".git/refs/tags/v1")),
            Some(WorkerEvent::RefsChanged)
        );
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
        assert_eq!(classify(Path::new(".git/index")), None);
        assert_eq!(classify(Path::new(".git/objects/xx/yyyy")), None);
    }

    #[test]
    fn classify_workdir_paths_carry_their_path() {
        assert_eq!(
            classify(Path::new("src/foo.rs")),
            Some(WorkerEvent::WorkdirChanged(vec![PathBuf::from(
                "src/foo.rs"
            )]))
        );
        assert_eq!(
            classify(Path::new("Cargo.toml")),
            Some(WorkerEvent::WorkdirChanged(vec![PathBuf::from(
                "Cargo.toml"
            )]))
        );
        assert_eq!(
            classify(Path::new("a/b/c/d.txt")),
            Some(WorkerEvent::WorkdirChanged(vec![PathBuf::from(
                "a/b/c/d.txt"
            )]))
        );
    }

    #[test]
    fn mtime_changed_detects_creation() {
        assert!(mtime_changed(None, Some(SystemTime::UNIX_EPOCH)));
    }

    #[test]
    fn mtime_changed_detects_deletion() {
        assert!(mtime_changed(Some(SystemTime::UNIX_EPOCH), None));
    }

    #[test]
    fn mtime_changed_detects_movement() {
        let a = SystemTime::UNIX_EPOCH;
        let b = a + Duration::from_secs(60);
        assert!(mtime_changed(Some(a), Some(b)));
    }

    #[test]
    fn mtime_changed_stable_when_equal() {
        assert!(!mtime_changed(None, None));
        assert!(!mtime_changed(
            Some(SystemTime::UNIX_EPOCH),
            Some(SystemTime::UNIX_EPOCH)
        ));
    }

    #[test]
    fn ref_probe_detects_target_movement_with_stable_head() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let ref_file = git.join("refs/heads/main");
        std::fs::write(&ref_file, "0000000000000000000000000000000000000000\n").unwrap();

        let mut probe = RefProbe::new(&git);
        assert!(!probe.poll(), "a stable tree must not report a change");

        // Move the ref file alone (as a commit does), leaving HEAD untouched.
        let moved = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&ref_file)
            .unwrap()
            .set_modified(moved)
            .unwrap();

        assert!(
            probe.poll(),
            "a ref move must fire even when HEAD is unchanged"
        );
    }

    #[test]
    fn resolve_head_target_reads_symref_and_skips_detached() {
        let dir = tempfile::tempdir().unwrap();
        let head = dir.path().join("HEAD");
        std::fs::write(&head, "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(
            resolve_head_target(&head),
            Some(dir.path().join("refs/heads/feature/x"))
        );

        std::fs::write(&head, "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        assert_eq!(resolve_head_target(&head), None);
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
        assert_eq!(classify(Path::new("sub/.git/HEAD")), None);
        assert_eq!(classify(Path::new("sub/.git/index")), None);
    }

    #[test]
    fn absorb_event_accumulates_workdir_paths_and_refs() {
        let root = Path::new("/repo");
        let mut pending = PendingPaths::default();
        let mut refs = false;

        absorb_event(
            root,
            &[
                PathBuf::from("/repo/src/a.rs"),
                PathBuf::from("/repo/src/a.rs"),
                PathBuf::from("/repo/.git/HEAD"),
                PathBuf::from("/repo/.git/index"),
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
    fn poll_mode_flag_is_shared_with_the_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = Watcher::start(dir.path().to_path_buf()).unwrap();
        let flag = watcher.poll_mode_flag();
        // A store through one clone must be visible through another: the UI
        // holds the same allocation the watcher flips on degradation.
        flag.store(true, Ordering::Relaxed);
        assert!(watcher.poll_mode_flag().load(Ordering::Relaxed));
    }
}
