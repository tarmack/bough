//! Live-update data types: the immutable `Snapshot` the worker builds and the
//! UI swaps atomically, plus the `Request` enum the UI sends to drive the
//! worker.
//!
//! Pure data — no logic. The worker (`crate::worker`) produces
//! `ArcSnapshot`s; the UI thread holds one and never mutates it in place.

use std::path::PathBuf;
use std::sync::Arc;

use crate::diff::Diff;
use crate::highlight::TokenOwned;
use crate::tree::Tree;

/// Per-line highlight tokens for the selected diff; `None` means a line was
/// not highlighted (binary line, or beyond the highlight cap). Parallel to
/// `Diff::lines`.
pub type HighlightLines = Vec<Option<Vec<TokenOwned>>>;

/// Monotonic sequence number stamped onto each `Request::Diff`,
/// `Request::Highlight`, and `Request::LoadImage` so the worker can drop stale
/// asks.
pub type Seq = u64;

/// An immutable snapshot of the whole tree + currently-selected diff, built
/// off the UI thread and swapped in atomically between frames.
pub struct Snapshot {
    /// Request sequence this snapshot answers (the latest request the worker
    /// had accepted when it was built).
    pub seq: Seq,
    /// Worker-side counter of worktree-switch attempts. The UI records the
    /// value observed when it requests a switch and only settles the switch gate
    /// on a snapshot carrying a newer value, so stale old-root snapshots can
    /// never consume a pending restore or fake a rejection.
    pub switch_epoch: u64,
    /// True when the worker still has in-flight or pending work for this state
    /// ("more coming"). A settled snapshot (`busy == false`) always follows.
    /// The tree/diff carried are the best known at send time.
    pub busy: bool,
    /// Whether the repo has no commits yet (`empty` mode): every file is new.
    pub empty: bool,
    /// Changed-file tree. Directories are built collapsed; the UI re-applies
    /// its own `expanded_dirs` on swap.
    pub tree: Arc<Tree>,
    /// Diff for the selected path, if any.
    pub selected_diff: Option<Arc<Diff>>,
    /// Per-body-line syntax tokens, parallel to `selected_diff.lines`.
    /// `None` = not yet highlighted (filled in as the viewport moves).
    /// `Some([])` = highlighted but uncolored (grammar miss / no highlighter).
    pub selected_highlight: Arc<HighlightLines>,
    /// Last git error text, surfaced in the status line. `None` when healthy.
    pub error: Option<String>,
    /// Current branch state (branch name, ahead/behind vs base, unpushed vs
    /// upstream), computed by the worker alongside the diff.
    pub branch_state: crate::git::BranchState,
    /// Uncommitted-only totals `(added, deleted)`: how much of the view is
    /// mid-flight work not yet committed. `None` when the view itself is the
    /// uncommitted comparison, or when the extra scan failed (degrades to
    /// hiding the split rather than blanking the bar).
    pub uncommitted_stats: Option<(usize, usize)>,
}

/// Cheaply-clonable handle to a `Snapshot`.
pub type ArcSnapshot = Arc<Snapshot>;

/// Request the UI sends to the worker thread.
#[derive(Debug, Clone)]
pub enum Request {
    /// The selection moved to `path`; build + highlight its diff at context
    /// `ctx`. `seq` lets the worker drop stale asks.
    Diff { path: String, ctx: u32, seq: Seq },
    /// Highlight the selected diff's sparse line range `from..to` (exclusive,
    /// raw `diff.lines` indices). Sent when the viewport shifts so the worker
    /// colors exactly the visible region plus lookahead.
    Highlight {
        path: String,
        ctx: u32,
        from: usize,
        to: usize,
        seq: Seq,
    },
    /// Decode + downscale the image at `path` for the preview pane. `seq` lets
    /// the worker drop stale asks.
    LoadImage { path: String, seq: Seq },
    /// Switch the base the diffs are computed against (e.g. between `HEAD`
    /// and `main...HEAD`) and recompute the whole tree + diff.
    SetRev { rev: String },
    /// Switch the worktree being viewed (same repository, different
    /// workdir). The worker re-aims the git layer, drops caches, re-arms the
    /// watcher, and recomputes everything. A root equal to the current one
    /// is a no-op passed through by coalescing and ignored by the worker
    /// (mirroring same-rev `SetRev`).
    SwitchWorktree { root: PathBuf },
    /// Tear the worker down cleanly (UI is exiting).
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_fields() {
        // Non-default `selected_diff`/`tree`, built via the shared fixture
        // helper, must round-trip intact onto the carried `ArcSnapshot`.
        let diff = crate::test_support::sample_diff(2);
        let snap = crate::test_support::snapshot_with_diff(diff.clone());

        assert!(!snap.empty);
        assert_eq!(snap.selected_diff.as_deref(), Some(&diff));
        assert_eq!(snap.selected_diff.as_ref().unwrap().header.path, "x.rs");
        // Per-line highlight slots mirror the diff's lines.
        assert_eq!(snap.selected_highlight.len(), 2);
        assert!(snap.selected_highlight.iter().all(Option::is_some));
        // The tree carries the fixture's single file.
        assert_eq!(snap.tree.flatten().len(), 1);
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
