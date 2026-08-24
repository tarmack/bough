//! Shared test fixtures for building small `App`s, `Snapshot`s, and `Diff`s.
//!
//! Only compiled under `#[cfg(test)]` (declared in `main.rs`). Import with
//! `use crate::test_support::*;` from any test module.

use std::sync::Arc;

use crossbeam_channel::bounded;

use crate::app::App;
use crate::diff::{
    Diff, DiffLine, FileHeader, FileStatus, HunkMeta, LineKind, Section, SectionKind,
};
use crate::snapshot::{ArcSnapshot, Request, Snapshot};
use crate::tree::Tree;

/// Build a single-file [`Diff`] from its header and body lines. Hunk
/// sections are derived from the header's `hunks`; each line's `section` is
/// its hunk index (single-file diffs have no `File` section).
pub fn single_diff(header: FileHeader, lines: Vec<DiffLine>) -> Diff {
    let hunks_len = header.hunks.len();
    let sections = (0..hunks_len)
        .map(|i| Section {
            kind: SectionKind::Hunk,
            file: 0,
            hunk: Some(i),
        })
        .collect();
    Diff {
        header: header.clone(),
        files: vec![header],
        sections,
        lines,
    }
}

/// A two-file `App` (a.rs, b.rs) backed by a `Snapshot`, ready for further
/// setup. The worker request channel is a throwaway bounded channel.
pub fn sample_app() -> App {
    let tree = Tree::build(vec![('M', "a.rs".to_string()), ('M', "b.rs".to_string())]);
    let snap = sample_snapshot(tree);
    let (tx, _rx) = bounded::<Request>(4);
    App::new(
        snap,
        tx,
        false,
        "main...HEAD".to_string(),
        "HEAD".to_string(),
    )
}

/// A `Snapshot` with the given tree, no selected diff, default fields.
pub fn sample_snapshot(tree: Tree) -> ArcSnapshot {
    Arc::new(Snapshot {
        empty: false,
        selected_diff: None,
        selected_highlight: Vec::new(),
        error: None,
        branch_state: crate::git::BranchState::default(),
        tree,
    })
}

/// A `Snapshot` whose `selected_diff` is `diff` and whose
/// `selected_highlight` is `Some(Vec::new())` per line.
pub fn snapshot_with_diff(diff: Diff) -> ArcSnapshot {
    let hl = std::iter::repeat_with(|| Some(Vec::new()))
        .take(diff.lines.len())
        .collect();
    let tree = Tree::build(vec![('M', "x.rs".to_string())]);
    Arc::new(Snapshot {
        empty: false,
        selected_diff: Some(diff),
        selected_highlight: hl,
        error: None,
        branch_state: crate::git::BranchState::default(),
        tree,
    })
}

/// `n` context lines in a single hunk.
pub fn sample_diff(n: usize) -> Diff {
    single_diff(
        header_full(None, None),
        (0..n)
            .map(|i| DiffLine {
                kind: LineKind::Context,
                raw_text: format!("l{i}"),
                section: 0,
                word_runs: None,
            })
            .collect(),
    )
}

/// A single-hunk file header with optional mode change.
pub fn header_full(mode_old: Option<u32>, mode_new: Option<u32>) -> FileHeader {
    FileHeader {
        path: "x.rs".into(),
        status: FileStatus::Modified,
        added: 1,
        deleted: 1,
        old_path: None,
        rename_percent: None,
        mode_old,
        mode_new,
        binary: false,
        hunks: vec![HunkMeta {
            added: 1,
            deleted: 1,
        }],
    }
}

/// A `@@` hunk-header line for hunk `idx`.
pub fn hunk_header(idx: usize) -> DiffLine {
    DiffLine {
        kind: LineKind::HunkHeader,
        raw_text: format!("@@ -{idx} +{idx} @@"),
        section: idx,
        word_runs: None,
    }
}

/// A context body line in hunk `hunk`.
pub fn ctx_line(idx: usize, hunk: usize) -> DiffLine {
    DiffLine {
        kind: LineKind::Context,
        raw_text: format!("l{idx}"),
        section: hunk,
        word_runs: None,
    }
}

/// A 3-hunk diff whose 6 visible lines all fit in the pane.
pub fn fitting_hunk() -> Diff {
    let header = FileHeader {
        path: "x.rs".into(),
        status: FileStatus::Modified,
        added: 3,
        deleted: 3,
        old_path: None,
        rename_percent: None,
        mode_old: None,
        mode_new: None,
        binary: false,
        hunks: (0..3)
            .map(|_| HunkMeta {
                added: 1,
                deleted: 1,
            })
            .collect(),
    };
    single_diff(
        header,
        vec![
            hunk_header(0),
            ctx_line(0, 0),
            hunk_header(1),
            ctx_line(1, 1),
            hunk_header(2),
            ctx_line(2, 2),
        ],
    )
}
