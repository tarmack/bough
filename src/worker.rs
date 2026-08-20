//! Live-update worker thread: owns git I/O, parsing, highlighting, and the
//! debounce/coalesce loop. Produces immutable `ArcSnapshot`s on a channel; the
//! UI thread swaps them in between frames. The worker holds the `Highlighter`
//! (syntect is `&mut self` and single-threaded) and a small LRU of recently
//! viewed `(path, ctx)` diffs so rapid `j`-mashing fills the pane instantly.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender};

use crate::diff::{self, LineKind, ParsedDiff};
use crate::git;
use crate::highlight::{Highlighter, TokenOwned};
use crate::snapshot::{ArcSnapshot, Request, Seq, Snapshot};
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
/// Highlight at most this many body lines; the rest render uncolored.
pub const HIGHLIGHT_CAP: usize = 2000;

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
    pub poll_mode: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawn the worker thread. `initial_theme` selects the syntect theme;
    /// an unknown theme degrades to no-color (the worker still parses diffs).
    /// `repo_root` is where `notify` watches; `base_ref` is the local branch
    /// name whose ref file is watched for movement (e.g. `main`), or `None`.
    pub fn spawn(
        rev: String,
        empty: bool,
        repo_root: PathBuf,
        base_ref: Option<String>,
        initial_theme: String,
        initial_tree: Tree,
    ) -> Result<Worker, WorkerError> {
        let watcher = match Watcher::start(repo_root.clone(), base_ref.clone()) {
            Ok(w) => w,
            Err(e) => {
                // No watcher: surface via the first snapshot's error and run
                // request-only. poll_mode stays false (watcher is off, not
                // degraded to a workdir poll).
                return spawn_no_watcher(rev, empty, initial_theme, e.to_string(), initial_tree);
            }
        };
        let poll_mode = watcher_poll_mode(&watcher);
        let event_rx = watcher.receiver().clone();

        let (request_tx, request_rx) = crossbeam_channel::bounded::<Request>(64);
        let (snapshot_tx, snapshot_rx) = crossbeam_channel::bounded::<ArcSnapshot>(16);

        let theme = initial_theme;
        let handle = thread::Builder::new()
            .name("bough-worker".into())
            .spawn(move || {
                let highlighter = Highlighter::new(&theme).ok();
                run_loop(
                    rev,
                    empty,
                    highlighter,
                    watcher,
                    event_rx,
                    request_rx,
                    snapshot_tx,
                    initial_tree,
                );
            })
            .map_err(|e| WorkerError {
                message: format!("failed to spawn worker thread: {e}"),
            })?;

        Ok(Worker {
            request_tx,
            snapshot_rx,
            poll_mode,
            handle: Some(handle),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
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
    theme: String,
    watch_err: String,
    initial_tree: Tree,
) -> Result<Worker, WorkerError> {
    let (request_tx, request_rx) = crossbeam_channel::bounded::<Request>(64);
    let (snapshot_tx, snapshot_rx) = crossbeam_channel::bounded::<ArcSnapshot>(16);

    let first_error = watch_err.clone();
    let handle = thread::Builder::new()
        .name("bough-worker".into())
        .spawn(move || {
            let highlighter = Highlighter::new(&theme).ok();
            // Emit one initial snapshot carrying the watcher error so the UI
            // status line surfaces it, then serve Diff requests only.
            let mut state = WorkerState::new(rev.clone(), empty, highlighter, initial_tree);
            state.last_error = Some(first_error);
            state.send_initial(&snapshot_tx);

            loop {
                crossbeam_channel::select! {
                    recv(request_rx) -> msg => {
                        match msg {
                            Ok(req) => {
                                if handle_request(req, &mut state, &snapshot_tx) {
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
        poll_mode: Arc::new(AtomicBool::new(false)),
        handle: Some(handle),
    })
}

/// Cached parsed diff + its highlight, keyed by `(path, ctx)`.
type CachedDiff = (ParsedDiff, Vec<Option<Vec<TokenOwned>>>);

/// Mutable worker-thread state.
struct WorkerState {
    rev: String,
    empty: bool,
    highlighter: Option<Highlighter>,
    selected_path: Option<String>,
    ctx: u32,
    latest_request_seq: Seq,
    next_seq: Seq,
    last_tree: Option<Tree>,
    last_error: Option<String>,
    last_stats: HashMap<String, (usize, usize)>,
    cache: LruCache<(String, u32), CachedDiff>,
    /// When the floor was last raised; `None` when floor is at the baseline.
    floor_raised_at: Option<Instant>,
    /// Wall time of the last `git::changed_files` call, for adaptive floor.
    last_git_wall: Duration,
}

impl WorkerState {
    fn new(
        rev: String,
        empty: bool,
        highlighter: Option<Highlighter>,
        initial_tree: Tree,
    ) -> Self {
        WorkerState {
            rev,
            empty,
            highlighter,
            selected_path: None,
            ctx: 3,
            latest_request_seq: 0,
            next_seq: 1,
            last_tree: Some(initial_tree),
            last_error: None,
            last_stats: HashMap::new(),
            cache: LruCache::new(8),
            floor_raised_at: None,
            last_git_wall: Duration::ZERO,
        }
    }

    fn bump_seq(&mut self) -> Seq {
        let s = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        s
    }

    /// Send an initial empty-ish snapshot so the UI's first frame isn't blank
    /// while the first recompute is debouncing.
    fn send_initial(&mut self, tx: &Sender<ArcSnapshot>) {
        let tree = self.last_tree.clone().unwrap_or_else(|| Tree::build(Vec::new()));
        let snap = self.snapshot(tree, None, Vec::new(), 0, self.last_error.clone());
        let _ = tx.send(Arc::new(snap));
    }

    /// Assemble a `Snapshot` from the current worker state plus the caller's
    /// diff/highlight/error specifics.
    fn snapshot(
        &mut self,
        tree: Tree,
        selected_diff: Option<ParsedDiff>,
        selected_highlight: Vec<Option<Vec<TokenOwned>>>,
        highlight_truncated_at: usize,
        error: Option<String>,
    ) -> Snapshot {
        Snapshot {
            rev: self.rev.clone(),
            empty: self.empty,
            flat: tree.flatten(),
            stats: self.last_stats.clone(),
            selected_path: self.selected_path.clone(),
            selected_diff,
            selected_highlight,
            highlight_truncated_at,
            error,
            generated_at: Instant::now(),
            seq: self.bump_seq(),
            tree,
        }
    }
}

/// Main loop with watcher events + requests + debounce timer.
fn run_loop(
    rev: String,
    empty: bool,
    highlighter: Option<Highlighter>,
    watcher: Watcher,
    event_rx: Receiver<WorkerEvent>,
    request_rx: Receiver<Request>,
    snapshot_tx: Sender<ArcSnapshot>,
    initial_tree: Tree,
) {
    let _watcher = watcher; // keep alive for the life of the loop
    let mut state = WorkerState::new(rev, empty, highlighter, initial_tree);
    state.send_initial(&snapshot_tx);

    let mut last_event: Option<Instant> = None;
    let mut last_recompute: Option<Instant> = None;

    loop {
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
                Err(_) => { /* watcher closed; keep serving requests */ }
            },
            recv(request_rx) -> req => match req {
                Ok(r) => {
                    if handle_request(r, &mut state, &snapshot_tx) {
                        break;
                    }
                }
                Err(_) => break,
            },
            default(timeout) => {}
        }

        let now = Instant::now();
        if debounce_due && last_event.map(|le| should_fire(le, now, floor)).unwrap_or(false) {
            if last_recompute
                .map(|lr| now.duration_since(lr) < HARD_CEILING)
                .unwrap_or(false)
            {
                // Hard ceiling: too soon since the last recompute. Reschedule.
                continue;
            }
            recompute(&mut state, &snapshot_tx);
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
    if let Some(le) = last_event {
        let remaining = floor.saturating_sub(now.duration_since(le));
        if let Some(lr) = last_recompute {
            let since_recompute = now.duration_since(lr);
            if since_recompute < HARD_CEILING {
                let ceiling_remaining = HARD_CEILING - since_recompute;
                return remaining.min(ceiling_remaining);
            }
        }
        return remaining;
    }
    HARD_CEILING
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
fn handle_request(req: Request, state: &mut WorkerState, tx: &Sender<ArcSnapshot>) -> bool {
    match req {
        Request::Shutdown => true,
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
    }
}

/// Full recompute: changed-files, tree, stats, selected file's diff.
fn recompute(state: &mut WorkerState, tx: &Sender<ArcSnapshot>) {
    let t0 = Instant::now();
    let entries = if state.empty {
        git::listing_empty()
    } else {
        git::changed_files(&state.rev)
    };
    state.last_git_wall = t0.elapsed();

    let entries = match entries {
        Ok(e) => e,
        Err(err) => {
            state.last_error = Some(err.to_string());
            send_error_snapshot(state, tx);
            return;
        }
    };

    // Adaptive floor: slow git or huge diff raises the floor for 10s.
    if state.last_git_wall > SLOW_GIT_THRESHOLD || entries.len() > LARGE_DIFF_THRESHOLD {
        state.floor_raised_at = Some(Instant::now());
    }

    let mut tree = Tree::build(entries);
    let stats = if state.empty {
        let paths: Vec<String> = tree
            .nodes
            .iter()
            .filter(|n| n.kind == crate::tree::Kind::File)
            .map(|n| n.path.clone())
            .collect();
        git::empty_stats(&paths)
    } else {
        git::diff_stats(&state.rev).unwrap_or_default()
    };
    tree.annotate_stats(&stats);
    state.last_tree = Some(clone_tree(&tree));

    let (selected_diff, selected_highlight, truncated_at) = {
        let rev = state.rev.clone();
        let empty = state.empty;
        let ctx = state.ctx;
        build_selected_diff(state, &tree, &rev, empty, ctx)
    };

    state.last_stats = stats;
    state.last_error = None;
    let snap = state.snapshot(tree, selected_diff, selected_highlight, truncated_at, None);
    let _ = tx.send(Arc::new(snap));
}

/// Send a snapshot that carries `error` but preserves the last known tree.
fn send_error_snapshot(state: &mut WorkerState, tx: &Sender<ArcSnapshot>) {
    let tree = state
        .last_tree
        .clone()
        .unwrap_or_else(|| Tree::build(Vec::new()));
    let snap = state.snapshot(tree, None, Vec::new(), 0, state.last_error.clone());
    let _ = tx.send(Arc::new(snap));
}

/// Parse + highlight the currently-selected file's diff, if it's in the tree.
/// Returns `(diff, highlight, truncated_at)`. On any error, `(None, vec![], 0)`.
fn build_selected_diff(
    state: &mut WorkerState,
    tree: &Tree,
    rev: &str,
    empty: bool,
    ctx: u32,
) -> (Option<ParsedDiff>, Vec<Option<Vec<TokenOwned>>>, usize) {
    let Some(path) = state.selected_path.clone() else {
        return (None, Vec::new(), 0);
    };
    let in_tree = tree.nodes.iter().any(|n| n.path == path);
    if !in_tree {
        return (None, Vec::new(), 0);
    }
    build_diff_for(&path, state, rev, empty, ctx)
}

/// Parse + highlight one file's diff. Uses the LRU cache when hit; on miss
/// parses fresh and caches. Mutates `state.cache` and borrows
/// `state.highlighter`.
fn build_diff_for(
    path: &str,
    state: &mut WorkerState,
    rev: &str,
    empty: bool,
    ctx: u32,
) -> (Option<ParsedDiff>, Vec<Option<Vec<TokenOwned>>>, usize) {
    let key = (path.to_string(), ctx);
    if let Some((parsed, hl)) = state.cache.get(&key) {
        return (
            Some(parsed.clone()),
            hl.clone(),
            0, // cached entries are always complete (never truncated)
        );
    }

    let new_file = empty || git::is_untracked(path);
    let raw = if new_file {
        git::file_diff_no_index(path, ctx)
    } else {
        git::file_diff_ctx(rev, path, ctx)
    };
    let meta = if new_file {
        git::file_meta_untracked(path)
    } else {
        git::file_meta(rev, path)
    };
    match (raw, meta) {
        (Ok(raw), Ok(meta)) => {
            let mut parsed = diff::parse_diff(&raw, path, meta.status);
            parsed.header.path = path.to_string();
            parsed.header.added = meta.added;
            parsed.header.deleted = meta.deleted;
            parsed.header.old_path = meta.old_path;
            parsed.header.rename_percent = meta.rename_percent;
            parsed.header.mode_old = meta.mode_old;
            parsed.header.mode_new = meta.mode_new;
            parsed.header.binary = meta.binary;
            let (hl, truncated_at) =
                build_highlight(&parsed, path, &mut state.highlighter);
            state.cache.put(key, (parsed.clone(), hl.clone()));
            (Some(parsed), hl, truncated_at)
        }
        (Err(e), _) | (_, Err(e)) => {
            state.last_error = Some(e.to_string());
            (None, Vec::new(), 0)
        }
    }
}

/// Serve a `Request::Diff`: LRU hit → send a snapshot with the cached diff
/// immediately, then refresh in the background and resend. Miss → fetch fresh.
fn serve_diff(state: &mut WorkerState, path: &str, ctx: u32, tx: &Sender<ArcSnapshot>) {
    let key = (path.to_string(), ctx);
    let cached = state.cache.get(&key).map(|(p, h)| (p.clone(), h.clone()));

    if let Some((parsed, hl)) = cached {
        send_diff_snapshot(state, Some(parsed), hl, 0, tx);
    }

    // Fresh fetch (also repopulates the cache). If we sent a cached entry
    // above, this resend carries the up-to-date content ~150ms later.
    let rev = state.rev.clone();
    let empty = state.empty;
    let (parsed, hl, truncated_at) = build_diff_for(path, state, &rev, empty, ctx);
    send_diff_snapshot(state, parsed, hl, truncated_at, tx);
}

/// Assemble and send a snapshot whose only changed field vs the last full
/// recompute is the selected diff (plus seq + error).
fn send_diff_snapshot(
    state: &mut WorkerState,
    parsed: Option<ParsedDiff>,
    hl: Vec<Option<Vec<TokenOwned>>>,
    truncated_at: usize,
    tx: &Sender<ArcSnapshot>,
) {
    let tree = state
        .last_tree
        .clone()
        .unwrap_or_else(|| Tree::build(Vec::new()));
    let snap = state.snapshot(tree, parsed, hl, truncated_at, state.last_error.clone());
    let _ = tx.send(Arc::new(snap));
}

/// Highlight a parsed diff's body lines up to `HIGHLIGHT_CAP`; lines past the
/// cap get `None` (rendered uncolored). Returns `(highlight, truncated_at)`.
fn build_highlight(
    parsed: &ParsedDiff,
    path: &str,
    highlighter: &mut Option<Highlighter>,
) -> (Vec<Option<Vec<TokenOwned>>>, usize) {
    let total = parsed.lines.len();
    let cap = total.min(HIGHLIGHT_CAP);
    let mut out: Vec<Option<Vec<TokenOwned>>> = Vec::with_capacity(total);

    for line in parsed.lines.iter().take(cap) {
        if line.kind == LineKind::HunkHeader {
            let section = parsed
                .header
                .hunks
                .get(line.hunk_index)
                .and_then(|m| m.section.as_deref());
            match highlighter.as_mut() {
                Some(h) => {
                    h.reset();
                    let tokens = match section {
                        Some(sec) => h.line(path, sec),
                        None => Vec::new(),
                    };
                    out.push(Some(tokens));
                }
                None => out.push(Some(Vec::new())),
            }
            continue;
        }
        match highlighter.as_mut() {
            Some(h) => out.push(Some(h.line(path, &line.raw_text))),
            None => out.push(Some(Vec::new())),
        }
    }

    let truncated_at = if cap < total { cap } else { 0 };
    out.resize(total, None);
    (out, truncated_at)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::sample_parsed;

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
    fn build_highlight_wraps_each_line_in_some() {
        let parsed = sample_parsed(3);
        let mut hl = Highlighter::new("Monokai Extended").ok();
        let (out, truncated) = build_highlight(&parsed, "x.rs", &mut hl);
        assert_eq!(out.len(), parsed.lines.len());
        assert!(out.iter().all(|e| e.is_some()));
        assert_eq!(truncated, 0);
    }

    #[test]
    fn build_highlight_caps_at_highlight_cap() {
        // Build a parsed diff with 2 * HIGHLIGHT_CAP context lines.
        let lines = (0..(2 * HIGHLIGHT_CAP))
            .map(|_| crate::diff::DiffLine {
                kind: LineKind::Context,
                raw_text: "x".into(),
                hunk_index: 0,
                word_runs: None,
            })
            .collect();
        let parsed = ParsedDiff {
            header: crate::diff::FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![crate::diff::HunkMeta {
                    section: None,
                    added: 1,
                    deleted: 1,
                }],
            },
            lines,
        };
        let mut hl = Highlighter::new("Monokai Extended").ok();
        let (out, truncated) = build_highlight(&parsed, "x.rs", &mut hl);
        assert_eq!(out.len(), 2 * HIGHLIGHT_CAP);
        // First HIGHLIGHT_CAP entries are Some(...).
        assert!(out.iter().take(HIGHLIGHT_CAP).all(|e| e.is_some()));
        // The rest are None (truncated).
        assert!(out
            .iter()
            .skip(HIGHLIGHT_CAP)
            .all(|e| e.is_none()));
        assert_eq!(truncated, HIGHLIGHT_CAP);
    }
}
