//! Live-update data types: the immutable `Snapshot` the worker builds and the
//! UI swaps atomically, plus the `Request` enum the UI sends to drive the
//! worker.
//!
//! Pure data — no logic. The worker (`crate::worker`) produces
//! `ArcSnapshot`s; the UI thread holds one and never mutates it in place.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::diff::ParsedDiff;
use crate::highlight::TokenOwned;
use crate::tree::Tree;

/// Monotonic sequence number stamped onto each `Request::Diff` and `Snapshot`.
/// The UI bumps its own `seq` on every selection move and ignores worker
/// arrivals whose `seq` is stale.
pub type Seq = u64;

/// An immutable snapshot of the whole tree + currently-selected diff, built
/// off the UI thread and swapped in atomically between frames.
pub struct Snapshot {
    /// Revision the diff is against (e.g. `main...HEAD`, `HEAD`).
    pub rev: String,
    /// Whether the repo has no commits yet (`empty` mode): every file is new.
    pub empty: bool,
    /// Changed-file tree (worker-side default expansion; the UI re-applies its
    /// own `expanded_dirs` on swap).
    pub tree: Tree,
    /// Flattened visible node indices for `tree` with worker-default expansion.
    pub flat: Vec<usize>,
    /// Per-file `(added, deleted)` line counts, keyed by path.
    pub stats: HashMap<String, (usize, usize)>,
    /// Path of the file the worker last parsed a diff for.
    pub selected_path: Option<String>,
    /// Parsed diff for `selected_path`, if any.
    pub selected_diff: Option<ParsedDiff>,
    /// Per-body-line syntax tokens, parallel to `selected_diff.lines`.
    /// `None`  = not yet highlighted (e.g. past the >10k-line cap).
    /// `Some([])` = highlighted but uncolored (grammar miss / no highlighter).
    pub selected_highlight: Vec<Option<Vec<TokenOwned>>>,
    /// 0 = highlight covers the whole diff; else the first line index that was
    /// not highlighted (truncation cap). Drives a `truncated: N/M` status note.
    pub highlight_truncated_at: usize,
    /// Last git error text, surfaced in the status line. `None` when healthy.
    pub error: Option<String>,
    /// When the snapshot was built (worker-side `Instant::now()`).
    pub generated_at: Instant,
    /// Monotonic sequence; the UI ignores arrivals with `seq` older than the
    /// latest request it sent.
    pub seq: Seq,
}

/// Cheaply-clonable handle to a `Snapshot`.
pub type ArcSnapshot = Arc<Snapshot>;

/// Request the UI sends to the worker thread.
#[derive(Debug, Clone)]
pub enum Request {
    /// The selection moved to `path`; parse + highlight its diff at context
    /// `ctx`. `seq` lets the worker drop stale asks.
    Diff {
        path: String,
        ctx: u32,
        seq: Seq,
    },
    /// Tear the worker down cleanly (UI is exiting).
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_fields() {
        let tree = Tree::build(vec![('M', "src/a.rs".to_string())]);
        let flat = tree.flatten();
        let mut stats = HashMap::new();
        stats.insert("src/a.rs".to_string(), (3, 1));
        let snap = Snapshot {
            rev: "main...HEAD".into(),
            empty: false,
            flat,
            stats,
            selected_path: Some("src/a.rs".into()),
            selected_diff: None,
            selected_highlight: Vec::new(),
            highlight_truncated_at: 0,
            error: None,
            generated_at: Instant::now(),
            seq: 1,
            tree,
        };
        assert_eq!(snap.rev, "main...HEAD");
        assert!(!snap.empty);
        assert_eq!(snap.selected_path.as_deref(), Some("src/a.rs"));
        assert_eq!(snap.stats.get("src/a.rs"), Some(&(3, 1)));
        assert_eq!(snap.seq, 1);
    }

    #[test]
    fn arc_snapshot_clones_cheaply() {
        let tree = Tree::build(Vec::new());
        let snap = Arc::new(Snapshot {
            rev: String::new(),
            empty: false,
            flat: Vec::new(),
            stats: HashMap::new(),
            selected_path: None,
            selected_diff: None,
            selected_highlight: Vec::new(),
            highlight_truncated_at: 0,
            error: None,
            generated_at: Instant::now(),
            seq: 0,
            tree,
        });
        let clone: ArcSnapshot = snap.clone();
        assert!(Arc::ptr_eq(&snap, &clone));
    }

    #[test]
    fn request_diff_carries_ctx() {
        let req = Request::Diff {
            path: "x.rs".into(),
            ctx: 6,
            seq: 42,
        };
        match req {
            Request::Diff { path, ctx, seq } => {
                assert_eq!(path, "x.rs");
                assert_eq!(ctx, 6);
                assert_eq!(seq, 42);
            }
            _ => panic!("expected Diff"),
        }
    }
}
