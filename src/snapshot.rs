//! Live-update data types: the immutable `Snapshot` the worker builds and the
//! UI swaps atomically, plus the `Request` enum the UI sends to drive the
//! worker.
//!
//! Pure data — no logic. The worker (`crate::worker`) produces
//! `ArcSnapshot`s; the UI thread holds one and never mutates it in place.

use std::sync::Arc;

use crate::diff::ParsedDiff;
use crate::highlight::TokenOwned;
use crate::tree::Tree;

/// Monotonic sequence number stamped onto each `Request::Diff` so the worker
/// can drop stale asks.
pub type Seq = u64;

/// An immutable snapshot of the whole tree + currently-selected diff, built
/// off the UI thread and swapped in atomically between frames.
pub struct Snapshot {
    /// Whether the repo has no commits yet (`empty` mode): every file is new.
    pub empty: bool,
    /// Changed-file tree (worker-side default expansion; the UI re-applies its
    /// own `expanded_dirs` on swap).
    pub tree: Tree,
    /// Parsed diff for the selected path, if any.
    pub selected_diff: Option<ParsedDiff>,
    /// Per-body-line syntax tokens, parallel to `selected_diff.lines`.
    /// `None`  = not yet highlighted (e.g. past the >10k-line cap).
    /// `Some([])` = highlighted but uncolored (grammar miss / no highlighter).
    pub selected_highlight: Vec<Option<Vec<TokenOwned>>>,
    /// Last git error text, surfaced in the status line. `None` when healthy.
    pub error: Option<String>,
    /// Current branch state (branch name, ahead/behind vs base, unpushed vs
    /// upstream), computed by the worker alongside the diff.
    pub branch_state: crate::git::BranchState,
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
        let snap = Snapshot {
            empty: false,
            selected_diff: None,
            selected_highlight: Vec::new(),
            error: None,
            branch_state: crate::git::BranchState::default(),
            tree,
        };
        assert!(!snap.empty);
        assert!(snap.selected_diff.is_none());
    }

    #[test]
    fn arc_snapshot_clones_cheaply() {
        let tree = Tree::build(Vec::new());
        let snap = Arc::new(Snapshot {
            empty: false,
            selected_diff: None,
            selected_highlight: Vec::new(),
            error: None,
            branch_state: crate::git::BranchState::default(),
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
