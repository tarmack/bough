//! Shared test fixtures for building small `App`s, `Snapshot`s, and `ParsedDiff`s.
//!
//! Only compiled under `#[cfg(test)]` (declared in `main.rs`). Import with
//! `use crate::test_support::*;` from any test module.

use std::sync::Arc;

use crossbeam_channel::bounded;

use crate::app::App;
use crate::diff::{DiffLine, FileHeader, HunkMeta, LineKind, ParsedDiff};
use crate::snapshot::{ArcSnapshot, Request, Snapshot};
use crate::tree::Tree;

/// A two-file `App` (a.rs, b.rs) backed by a `Snapshot`, ready for further
/// setup. The worker request channel is a throwaway bounded channel.
pub fn sample_app() -> App {
    let tree = Tree::build(vec![('M', "a.rs".to_string()), ('M', "b.rs".to_string())]);
    let snap = sample_snapshot_with_tree(tree);
    let (tx, _rx) = bounded::<Request>(4);
    App::new(snap, tx, false)
}

/// A `Snapshot` with the given tree, no selected diff, default fields.
pub fn sample_snapshot(tree: Tree) -> ArcSnapshot {
    sample_snapshot_with_tree(tree)
}

fn sample_snapshot_with_tree(tree: Tree) -> ArcSnapshot {
    let flat = tree.flatten();
    let selected_path = flat
        .first()
        .and_then(|&idx| {
            let n = &tree.nodes[idx];
            (n.kind == crate::tree::Kind::File).then_some(n.path.clone())
        });
    Arc::new(Snapshot {
        rev: "HEAD".into(),
        empty: false,
        flat,
        stats: Default::default(),
        selected_path,
        selected_diff: None,
        selected_highlight: Vec::new(),
        highlight_truncated_at: 0,
        error: None,
        generated_at: std::time::Instant::now(),
        seq: 0,
        tree,
    })
}

/// A `Snapshot` whose `selected_diff` is `parsed` and whose
/// `selected_highlight` is `Some(Vec::new())` per line.
pub fn snapshot_with_diff(parsed: ParsedDiff) -> ArcSnapshot {
    let hl = std::iter::repeat_with(|| Some(Vec::new()))
        .take(parsed.lines.len())
        .collect();
    let tree = Tree::build(vec![('M', "x.rs".to_string())]);
    let flat = tree.flatten();
    Arc::new(Snapshot {
        rev: "HEAD".into(),
        empty: false,
        flat,
        stats: Default::default(),
        selected_path: None,
        selected_diff: Some(parsed),
        selected_highlight: hl,
        highlight_truncated_at: 0,
        error: None,
        generated_at: std::time::Instant::now(),
        seq: 0,
        tree,
    })
}

/// `n` context lines in a single hunk.
pub fn sample_parsed(n: usize) -> ParsedDiff {
    ParsedDiff {
        header: FileHeader {
            path: "x.rs".into(),
            status: 'M',
            added: 1,
            deleted: 1,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: vec![HunkMeta {
                section: None,
                added: 1,
                deleted: 1,
            }],
        },
        lines: (0..n)
            .map(|i| DiffLine {
                kind: LineKind::Context,
                raw_text: format!("l{i}"),
                hunk_index: 0,
                word_runs: None,
            })
            .collect(),
    }
}

/// A single-hunk file header with optional mode change.
pub fn header_full(mode_old: Option<u32>, mode_new: Option<u32>) -> FileHeader {
    FileHeader {
        path: "x.rs".into(),
        status: 'M',
        added: 1,
        deleted: 1,
        old_path: None,
        rename_percent: None,
        mode_old,
        mode_new,
        binary: false,
        hunks: vec![HunkMeta {
            section: None,
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
        hunk_index: idx,
        word_runs: None,
    }
}

/// A context body line in hunk `hunk`.
pub fn ctx_line(idx: usize, hunk: usize) -> DiffLine {
    DiffLine {
        kind: LineKind::Context,
        raw_text: format!("l{idx}"),
        hunk_index: hunk,
        word_runs: None,
    }
}

/// A 3-hunk diff whose 6 visible lines all fit in the pane.
pub fn fitting_hunk() -> ParsedDiff {
    ParsedDiff {
        header: FileHeader {
            path: "x.rs".into(),
            status: 'M',
            added: 3,
            deleted: 3,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: (0..3)
                .map(|_| HunkMeta {
                    section: None,
                    added: 1,
                    deleted: 1,
                })
                .collect(),
        },
        lines: vec![
            hunk_header(0),
            ctx_line(0, 0),
            hunk_header(1),
            ctx_line(1, 1),
            hunk_header(2),
            ctx_line(2, 2),
        ],
    }
}
