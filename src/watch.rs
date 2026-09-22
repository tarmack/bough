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
//! - **Backstop poll**: every 2s, stat `.git/HEAD`. If `notify` missed a ref
//!   change (some filesystems are flaky), the poll catches it.
//!
//! When `notify` init fails (unsupported filesystem), the watcher degrades to
//! a 1s workdir poll and surfaces `is_poll_mode()` so the UI can show a hint.
//! The poll walks workdir metadata (path/mtime/size, `.git` skipped) and emits
//! `WorkdirChanged` only when an entry appeared, disappeared, or changed, so an
//! idle repository costs no recomputes. Workdir events carry their changed
//! paths so the worker can drop a burst that only touched ignored files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{recommended_watcher, RecursiveMode};

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
    // Workdir paths observed since the last successful send. A send dropped
    // under backpressure restores them here so the worker can't miss the final
    // change in a burst (only refs have a poll backstop, not the workdir).
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut refs_pending = false;

    loop {
        if io.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if refs_pending {
            match io.out_tx.try_send(WorkerEvent::RefsChanged) {
                Ok(()) => refs_pending = false,
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        if !pending.is_empty() {
            dedup_paths(&mut pending);
            let ev = WorkerEvent::WorkdirChanged(std::mem::take(&mut pending));
            match io.out_tx.try_send(ev) {
                Ok(()) => {}
                Err(TrySendError::Full(ev)) => {
                    if let WorkerEvent::WorkdirChanged(p) = ev {
                        pending = p;
                    }
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        match raw_rx.recv_timeout(SHUTDOWN_TICK) {
            Ok(Ok(event)) => {
                if !is_read_access(&event.kind) {
                    absorb_event(&io.repo_root, &event.paths, &mut pending, &mut refs_pending);
                }
            }
            Ok(Err(_)) => { /* notify internal error; keep going */ }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        poll_refs(&head_path, &mut last_head, &io.out_tx, &mut last_poll);
    }
}

/// True for `Access` events that carry no content change: a bare read, an open
/// in a read-only mode, or a close of a read-only handle. `notify` emits these
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
        EventKind::Access(AccessKind::Close(mode)) => matches!(mode, AccessMode::Read),
        _ => false,
    }
}

/// Classify a raw notify event's `paths` into accumulated `pending` workdir
/// paths and the `refs` flag. Paths are made relative to `root`; `.git`
/// internals and unrecognised paths are dropped.
fn absorb_event(root: &Path, paths: &[PathBuf], pending: &mut Vec<PathBuf>, refs: &mut bool) {
    for path in paths {
        let rel = path.strip_prefix(root).unwrap_or(path);
        match classify(rel) {
            Some(WorkerEvent::WorkdirChanged(p)) => pending.extend(p),
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

/// Degraded loop when `notify` is unavailable: walk the workdir every second
/// and emit `WorkdirChanged` only for paths whose metadata signature changed,
/// plus the same 2s backstop ref poll.
fn run_poll_loop(io: &WatchIo) {
    let head_path = io.repo_root.join(".git/HEAD");
    let mut last_head = stat_mtime(&head_path);
    let mut last_refs = Instant::now();
    // Baseline taken before the first interval so a change landing right after
    // startup is not folded into the baseline unseen.
    let mut last_signature = walk_signature(&io.repo_root);
    let mut last_workdir = Instant::now();

    loop {
        if io.shutdown.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(SHUTDOWN_TICK);

        if last_workdir.elapsed() >= POLL_INTERVAL {
            last_workdir = Instant::now();
            let signature = walk_signature(&io.repo_root);
            let changed = signature_diff(&last_signature, &signature);
            last_signature = signature;
            if !changed.is_empty() {
                let _ = io.out_tx.send(WorkerEvent::WorkdirChanged(changed));
            }
        }
        poll_refs(&head_path, &mut last_head, &io.out_tx, &mut last_refs);
    }
}

/// Metadata signature of the workdir: repo-relative path -> `(mtime, size)`.
/// Symlink metadata only, so the walk reads no file content and never follows
/// links. `.git` is skipped entirely (ref movement is the backstop poll's job).
type Signature = HashMap<PathBuf, (SystemTime, u64)>;

/// Recursively collect the [`Signature`] of every file under `root`.
fn walk_signature(root: &Path) -> Signature {
    let mut signature = Signature::new();
    collect_signature(root, root, &mut signature);
    signature
}

/// Descend `dir` (a subdirectory of `root`), inserting each regular entry.
fn collect_signature(root: &Path, dir: &Path, out: &mut Signature) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Prune any `.git` directory (root or nested, e.g. a submodule) so ref
        // churn never leaks into the workdir signature.
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_signature(root, &path, out);
        } else {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.insert(rel.to_path_buf(), (mtime, meta.len()));
        }
    }
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
    fn signature_unchanged_tree_has_empty_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "two\n").unwrap();

        let first = walk_signature(dir.path());
        let second = walk_signature(dir.path());

        assert!(signature_diff(&first, &second).is_empty());
    }

    #[test]
    fn signature_detects_create_modify_delete() {
        let dir = tempfile::tempdir().unwrap();
        let touched = dir.path().join("a.txt");
        std::fs::write(&touched, "one\n").unwrap();
        let mut prev = walk_signature(dir.path());

        // Create: the new path appears.
        std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
        let next = walk_signature(dir.path());
        assert_eq!(signature_diff(&prev, &next), vec![PathBuf::from("b.txt")]);
        prev = next;

        // Modify (size differs, so the change is visible even at coarse mtime
        // resolution).
        std::fs::write(&touched, "one\ntwo\n").unwrap();
        let next = walk_signature(dir.path());
        assert_eq!(signature_diff(&prev, &next), vec![PathBuf::from("a.txt")]);
        prev = next;

        // Delete: the vanished path is reported.
        std::fs::remove_file(&touched).unwrap();
        let next = walk_signature(dir.path());
        assert_eq!(signature_diff(&prev, &next), vec![PathBuf::from("a.txt")]);
    }

    #[test]
    fn signature_skips_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "x\n").unwrap();

        let sig = walk_signature(dir.path());

        assert!(sig.contains_key(Path::new("tracked.txt")));
        assert!(!sig.keys().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn signature_skips_nested_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/.git")).unwrap();
        std::fs::write(dir.path().join("sub/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.path().join("sub/tracked.txt"), "x\n").unwrap();

        let sig = walk_signature(dir.path());

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
        let mut pending = Vec::new();
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
            pending,
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
}
