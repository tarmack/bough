//! Filesystem watching for live updates.
//!
//! Produces a stream of coarse `WorkerEvent`s: either the workdir changed (a
//! tracked/untracked file moved) or the git refs moved (`HEAD`, `packed-refs`,
//! or any ref under `.git/refs/`). The worker thread consumes these and
//! recomputes. Two layers:
//!
//! - **`notify` (recommended watcher)** watches the repo root recursively.
//!   Paths under `.git/` are ignored except `.git/HEAD`, `.git/packed-refs`,
//!   and anything under `.git/refs/`, which become `RefsChanged`.
//! - **Backstop poll**: every 2s, stat `.git/HEAD`. If `notify` missed a ref
//!   change (some filesystems are flaky), the poll catches it.
//!
//! When `notify` init fails (unsupported filesystem), the watcher degrades to
//! a 1s workdir poll and surfaces `is_poll_mode()` so the UI can show a hint.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use notify::{recommended_watcher, RecursiveMode};

/// Coarse classification of what moved. The worker treats both the same way
/// (full recompute) but distinguishing lets it skip git ref lookups when only
/// the workdir changed, if needed later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEvent {
    /// Any non-`.git` path changed (tracked or untracked file).
    WorkdirChanged,
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
/// Loop tick used so shutdown stays responsive between sleeps.
const SHUTDOWN_TICK: Duration = Duration::from_millis(100);

/// Owns the filesystem watcher thread. Dropping it signals shutdown and joins.
pub struct Watcher {
    rx: Receiver<WorkerEvent>,
    poll_mode: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Start watching `repo_root`.
    pub fn start(repo_root: PathBuf) -> Result<Watcher, WatchError> {
        let poll_mode = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = bounded::<WorkerEvent>(128);

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
            })
            .map_err(|e| WatchError {
                message: format!("failed to spawn watch thread: {e}"),
            })?;

        Ok(Watcher {
            rx,
            poll_mode,
            shutdown,
            handle: Some(handle),
        })
    }

    /// Borrow the event receiver. The worker drains this.
    pub fn receiver(&self) -> &Receiver<WorkerEvent> {
        &self.rx
    }

    /// True when `notify` init failed and the watcher degraded to 1s polling.
    pub fn is_poll_mode(&self) -> bool {
        self.poll_mode.load(Ordering::Relaxed)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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
            if notify::Watcher::watch(&mut watcher, &io.repo_root, RecursiveMode::Recursive)
                .is_err()
            {
                poll_mode.store(true, Ordering::Relaxed);
                drop(watcher);
                run_poll_loop(&io);
                return;
            }
            run_notify_loop(&io, watcher, raw_rx);
        }
        Err(_err) => {
            // `notify` silently failed on this platform; degrade to the 1s poll
            // loop. The reason (`_err`) is intentionally not surfaced: there is
            // no logging channel, so only the poll-mode flag the UI reads is
            // set. Binding it here documents why it is dropped.
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

    let head_path = io.repo_root.join(".git/HEAD");
    let mut last_head = stat_mtime(&head_path);
    let mut last_poll = Instant::now();
    // Set when a bounded send dropped an event under backpressure; retried as a
    // coalesced `WorkdirChanged` so the worker can't miss the final change in a
    // burst (only refs have a poll backstop, not the workdir).
    let mut dirty = false;

    loop {
        if io.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if dirty && io.out_tx.try_send(WorkerEvent::WorkdirChanged).is_ok() {
            dirty = false;
        }
        match raw_rx.recv_timeout(SHUTDOWN_TICK) {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    let rel = path.strip_prefix(&io.repo_root).unwrap_or(path);
                    if let Some(ev) = classify(rel) {
                        match io.out_tx.try_send(ev) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => dirty = true,
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                }
            }
            Ok(Err(_)) => { /* notify internal error; keep going */ }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        poll_refs(&head_path, &mut last_head, &io.out_tx, &mut last_poll);
    }
}

/// Degraded loop when `notify` is unavailable: emit `WorkdirChanged` every
/// second and run the same 2s backstop ref poll.
fn run_poll_loop(io: &WatchIo) {
    let head_path = io.repo_root.join(".git/HEAD");
    let mut last_head = stat_mtime(&head_path);
    let mut last_workdir = Instant::now();
    let mut last_refs = Instant::now();

    loop {
        if io.shutdown.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(SHUTDOWN_TICK);

        if last_workdir.elapsed() >= POLL_INTERVAL {
            last_workdir = Instant::now();
            let _ = io.out_tx.send(WorkerEvent::WorkdirChanged);
        }
        poll_refs(&head_path, &mut last_head, &io.out_tx, &mut last_refs);
    }
}

/// Backstop ref poll shared by both loops: on the cadence gated by
/// `last_poll`, stat `HEAD` and emit `RefsChanged` when it moved.
fn poll_refs(
    head_path: &Path,
    last_head: &mut Option<SystemTime>,
    out_tx: &Sender<WorkerEvent>,
    last_poll: &mut Instant,
) {
    if last_poll.elapsed() >= BACKSTOP_INTERVAL {
        *last_poll = Instant::now();
        let h = stat_mtime(head_path);
        if mtime_changed(*last_head, h) {
            *last_head = h;
            let _ = out_tx.send(WorkerEvent::RefsChanged);
        }
    }
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
/// freshly picked base stays live). Other `.git/` internals are ignored;
/// everything else is `WorkdirChanged`.
fn classify(rel: &Path) -> Option<WorkerEvent> {
    // Normalise separators so the string comparisons work on Windows too.
    let s = rel.to_string_lossy().replace('\\', "/");
    if s == ".git/HEAD" || s == ".git/packed-refs" || s.starts_with(".git/refs/") {
        return Some(WorkerEvent::RefsChanged);
    }
    if s.starts_with(".git/") {
        return None;
    }
    Some(WorkerEvent::WorkdirChanged)
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
    fn classify_other_git_internals_ignored() {
        assert_eq!(classify(Path::new(".git/index")), None);
        assert_eq!(classify(Path::new(".git/objects/xx/yyyy")), None);
    }

    #[test]
    fn classify_workdir_paths_are_workdir_changed() {
        assert_eq!(
            classify(Path::new("src/foo.rs")),
            Some(WorkerEvent::WorkdirChanged)
        );
        assert_eq!(
            classify(Path::new("Cargo.toml")),
            Some(WorkerEvent::WorkdirChanged)
        );
        assert_eq!(
            classify(Path::new("a/b/c/d.txt")),
            Some(WorkerEvent::WorkdirChanged)
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
}
