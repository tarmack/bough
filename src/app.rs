//! Application state: the diff/tree model view plus the panes' view state.
//!
//! `App` owns the mutable state of the viewer (selection, scroll positions,
//! collapsed hunks, focus) and the operations that drive it. Immutable diff
//! data arrives as `ArcSnapshot`s from the worker thread; `App` swaps them in
//! via `apply_snapshot` and never blocks on git. Input routing lives in
//! `crate::input` and rendering in `crate::view`.

use std::collections::HashSet;

use crossbeam_channel::Sender;

use crate::diff::{self, LineKind, ParsedDiff};
use crate::highlight::TokenOwned;
use crate::snapshot::{ArcSnapshot, Request, Seq, Snapshot};
use crate::text::max_scroll_for;
use crate::tree::{self, Tree};

/// Hard cap for horizontal scroll (cells), applied in `diff_scroll_right`.
pub const MAX_HSCROLL: usize = 10_000;
/// Upper bound for the `-U<N>` context window.
pub const MAX_CONTEXT: u32 = 200;
/// Context-line delta applied by the `[`/`]` keys.
pub const CONTEXT_STEP: i32 = 3;
/// Cells scrolled horizontally per `h`/`l` press.
pub const HSCROLL_STEP: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Diff,
}

pub struct App {
    /// Immutable data from the worker; swapped atomically between frames.
    pub snapshot: ArcSnapshot,
    /// Derived tree: cloned from `snapshot.tree` with `expanded_dirs` applied.
    pub tree: Tree,
    /// Flattened visible node indices for `tree`.
    pub flat: Vec<usize>,
    /// Index into `flat` (the selected row).
    pub selection: usize,
    /// Path of the selected file, or `None` when a dir / nothing is selected.
    pub selected_path: Option<String>,
    /// Set of expanded dir paths; survives tree rebuilds.
    pub expanded_dirs: HashSet<String>,

    pub tree_scroll: usize,
    pub diff_scroll: usize,
    pub diff_col: usize,
    pub diff_width: u16,
    pub height: usize,
    pub last_header_height: u16,
    pub collapsed_hunks: HashSet<usize>,
    pub context_lines: u32,
    pub word_diff_on: bool,
    pub focus: Focus,
    /// Pending `z`-prefix fold chord (vim fold controls).
    pub fold_key: bool,
    /// Pending `g`-prefix navigation chord (vim `gg` -> first chunk).
    pub g_key: bool,
    /// `parsed.lines` index of the sticky pinned hunk header (the one scrolled
    /// past the top of the view, rendered at body row 0). `None` when none.
    pub sticky_line: Option<usize>,
    /// Cursor for the current-chunk highlight: the chunk selected by chunk
    /// navigation (`j`/`k`), folding, or clicks. `None` before a chunk is
    /// selected.
    pub active_chunk: Option<usize>,

    /// Channel to the worker thread.
    pub request_tx: Sender<Request>,
    /// Monotonic counter stamped onto each `Request::Diff`.
    pub seq: Seq,
    /// True when the file watcher degraded to poll mode (for the status line).
    pub poll_mode: bool,
}

impl App {
    pub fn new(
        snapshot: ArcSnapshot,
        request_tx: Sender<Request>,
        poll_mode: bool,
    ) -> Self {
        let expanded_dirs = collect_expanded(&snapshot.tree);
        let mut tree = snapshot.tree.clone();
        tree.expand_paths(&expanded_dirs);
        let flat = tree.flatten();
        let selected_path = flat
            .first()
            .and_then(|&idx| {
                let n = &tree.nodes[idx];
                (n.kind == tree::Kind::File).then_some(n.path.clone())
            });
        let mut app = App {
            snapshot,
            tree,
            flat,
            selection: 0,
            selected_path,
            expanded_dirs,
            tree_scroll: 0,
            diff_scroll: 0,
            diff_col: 0,
            diff_width: 0,
            height: 0,
            last_header_height: 1,
            collapsed_hunks: HashSet::new(),
            context_lines: 3,
            word_diff_on: true,
            focus: Focus::Tree,
            fold_key: false,
            g_key: false,
            sticky_line: None,
            active_chunk: None,
            request_tx,
            seq: 0,
            poll_mode,
        };
        app.request_diff();
        app
    }

    // ── Snapshot plumbing ───────────────────────────────────────────────

    /// Swap in a fresh `ArcSnapshot` from the worker. Re-applies `expanded_dirs`
    /// to the new tree, re-locates the selection by path (nearest-sibling
    /// fallback when the path is gone), and re-anchors the diff scroll by hunk
    /// index so the view doesn't jump to the top on every save.
    pub fn apply_snapshot(&mut self, snap: ArcSnapshot) {
        let old_hunk = self.top_visible_hunk_index();
        let old_path = self.selected_path.clone();

        self.snapshot = snap;
        let mut tree = self.snapshot.tree.clone();
        tree.expand_paths(&self.expanded_dirs);
        self.flat = tree.flatten();

        // Re-locate selection by path; nearest-sibling fallback.
        self.selection = relocate_selection(&self.flat, &tree, &old_path);
        self.tree = tree;
        self.selected_path = self
            .flat
            .get(self.selection)
            .map(|&idx| self.tree.nodes[idx].path.clone());

        // Re-anchor diff scroll by hunk index.
        if let Some(hunk) = old_hunk {
            self.reanchor_diff_scroll(hunk);
        } else {
            self.clamp_diff_scroll();
        }
    }

    /// `hunk_index` of the topmost visible `@@` hunk header, or the first
    /// visible line's hunk index — recorded before a snapshot swap so the view
    /// can follow it.
    fn top_visible_hunk_index(&self) -> Option<usize> {
        let parsed = self.snapshot.selected_diff.as_ref()?;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        // Sticky pinned header first.
        if start > 0 {
            if let Some(&li) = visible.iter().take(start).rev().find(|&&li| {
                parsed.lines[li].kind == LineKind::HunkHeader
            }) {
                return Some(parsed.lines[li].hunk_index);
            }
        }
        visible.get(start).map(|&li| parsed.lines[li].hunk_index)
    }

    /// Scroll so the hunk header for `hunk` is at the top of the diff pane
    /// after a snapshot swap; fall back to clamping when the hunk is gone.
    fn reanchor_diff_scroll(&mut self, hunk: usize) {
        let Some(parsed) = self.snapshot.selected_diff.as_ref() else {
            self.diff_scroll = 0;
            return;
        };
        let visible = self.visible_line_indices();
        if let Some(pos) = visible.iter().position(|&li| {
            parsed.lines[li].kind == LineKind::HunkHeader && parsed.lines[li].hunk_index == hunk
        }) {
            self.diff_scroll = pos + 1;
        }
        self.clamp_diff_scroll();
    }

    /// Bump `seq` and send `Request::Diff` for the currently-selected file at
    /// the current context level. No-op when no file is selected.
    fn request_diff(&mut self) {
        let Some(path) = self.selected_path.clone() else {
            return;
        };
        self.seq = self.seq.saturating_add(1);
        let _ = self.request_tx.send(Request::Diff {
            path,
            ctx: self.context_lines,
            seq: self.seq,
        });
    }

    // ── Convenience accessors for snapshot data ─────────────────────────

    pub fn parsed(&self) -> Option<&ParsedDiff> {
        self.snapshot.selected_diff.as_ref()
    }

    pub fn highlighted(&self) -> &[Option<Vec<TokenOwned>>] {
        &self.snapshot.selected_highlight
    }

    pub fn current_file(&self) -> Option<&str> {
        self.selected_path.as_deref()
    }

    pub fn diff_error(&self) -> Option<&str> {
        self.snapshot.error.as_deref()
    }

    pub fn rev(&self) -> &str {
        &self.snapshot.rev
    }

    pub fn empty(&self) -> bool {
        self.snapshot.empty
    }

    // ── Selection / navigation ──────────────────────────────────────────

    pub fn selected_node(&self) -> Option<usize> {
        self.flat.get(self.selection).copied()
    }

    /// Load a fresh diff for the selected file. Each file switch starts a
    /// clean pane: no horizontal scroll, nothing collapsed, default context.
    pub fn load_diff(&mut self) {
        self.diff_col = 0;
        self.collapsed_hunks.clear();
        self.context_lines = 3;
        self.diff_scroll = 0;
        self.active_chunk = if self
            .parsed()
            .is_some_and(|p| !p.header.hunks.is_empty())
        {
            Some(0)
        } else {
            None
        };
        self.request_diff();
    }

    /// Update `selected_path` from the current flat selection, then send a
    /// `Request::Diff` so the worker fetches the diff.
    fn select_and_request(&mut self) {
        self.selected_path = self
            .flat
            .get(self.selection)
            .map(|&idx| self.tree.nodes[idx].path.clone());
        self.request_diff();
    }

    pub fn move_down(&mut self, n: usize) {
        if self.flat.is_empty() {
            return;
        }
        let last = self.flat.len() - 1;
        self.selection = self.selection.saturating_add(n).min(last);
        self.select_and_request();
    }

    pub fn move_up(&mut self, n: usize) {
        self.selection = self.selection.saturating_sub(n);
        self.select_and_request();
    }

    pub fn collapse(&mut self) {
        let idx = match self.selected_node() {
            Some(i) => i,
            None => return,
        };
        let node = &mut self.tree.nodes[idx];
        if node.kind == tree::Kind::Dir && node.expanded {
            node.expanded = false;
            self.expanded_dirs.remove(&node.path);
        } else {
            return;
        }

        self.flat = self.tree.flatten();
        if let Some(pos) = self.flat.iter().position(|&x| x == idx) {
            self.selection = pos;
        }
        self.diff_scroll = 0;
    }

    pub fn expand_or_refresh(&mut self) {
        let idx = match self.selected_node() {
            Some(i) => i,
            None => return,
        };
        if self.tree.nodes[idx].kind == tree::Kind::Dir {
            self.tree.nodes[idx].expanded = true;
            self.expanded_dirs.insert(self.tree.nodes[idx].path.clone());
            self.flat = self.tree.flatten();
        } else {
            self.load_diff();
        }
    }

    /// Number of body lines visible under the locked header.
    pub fn diff_page_size(&self) -> usize {
        self.height
            .saturating_sub(self.last_header_height as usize)
            .max(1)
    }

    pub fn diff_scroll_down(&mut self, n: usize) {
        if self.diff_fits() {
            self.move_active_chunk(n as i32);
            return;
        }
        let max = max_scroll_for(self.visible_line_indices().len(), self.diff_page_size());
        self.diff_scroll = self.diff_scroll.saturating_add(n).min(max);
    }

    pub fn diff_scroll_up(&mut self, n: usize) {
        if self.diff_fits() {
            self.move_active_chunk(-(n as i32));
            return;
        }
        self.diff_scroll = self.diff_scroll.saturating_sub(n);
    }

    /// True when the whole diff fits and there is nothing to scroll.
    pub fn diff_fits(&self) -> bool {
        self.visible_line_indices()
            .len()
            .saturating_sub(self.diff_page_size())
            == 0
    }

    /// Move the active-chunk selection by `delta` chunks (clamped). Used when the
    /// diff fits in the pane (no scrolling possible) so selection still moves.
    pub fn move_active_chunk(&mut self, delta: i32) {
        let max = self
            .parsed()
            .map_or(0usize, |p| p.header.hunks.len().saturating_sub(1));
        let cur = self.active_chunk.unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, max as i32) as usize;
        self.active_chunk = Some(next);
    }

    /// Move the chunk cursor by `delta`: jump to the adjacent chunk when its
    /// header is already in view, otherwise scroll one line in the direction.
    /// Scrolling activates a chunk as soon as its header line enters the view.
    pub fn navigate_chunk(&mut self, delta: i32) {
        if let Some(cur) = self.active_chunk {
            let dest = cur as i32 + delta;
            let in_range = self
                .parsed()
                .is_some_and(|p| dest >= 0 && (dest as usize) < p.header.hunks.len());
            if in_range && self.chunk_header_in_view(dest as usize) {
                self.active_chunk = Some(dest as usize);
                return;
            }
        }
        let before = self.diff_scroll;
        if delta > 0 {
            self.diff_scroll_down(1);
        } else {
            self.diff_scroll_up(1);
        }
        if self.diff_scroll == before {
            return;
        }
        let entered = if delta > 0 {
            self.bottom_body_line()
        } else {
            self.top_body_line()
        };
        if let Some(li) = entered {
            if let Some(parsed) = self.parsed() {
                if parsed.lines[li].kind == LineKind::HunkHeader {
                    self.active_chunk = Some(parsed.lines[li].hunk_index);
                }
            }
        }
    }

    /// Whether `chunk`'s hunk header is currently visible (in-flow window or the
    /// pinned sticky top header).
    fn chunk_header_in_view(&self, chunk: usize) -> bool {
        let Some(parsed) = self.parsed() else {
            return false;
        };
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let end = start.saturating_add(self.diff_page_size());
        let sticky = visible.iter().take(start).rev().find_map(|&li| {
            (parsed.lines[li].kind == LineKind::HunkHeader).then_some(parsed.lines[li].hunk_index)
        });
        if sticky == Some(chunk) {
            return true;
        }
        let in_flow = visible[start..end.min(visible.len())].iter().any(|&li| {
            parsed.lines[li].kind == LineKind::HunkHeader && parsed.lines[li].hunk_index == chunk
        });
        in_flow
    }

    /// `parsed.lines` index of the top rendered body line: the sticky header if
    /// present, else the first in-flow line.
    fn top_body_line(&self) -> Option<usize> {
        let parsed = self.parsed()?;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        if start > 0 {
            if let Some(li) = visible
                .iter()
                .take(start)
                .rev()
                .find(|&&li| parsed.lines[li].kind == LineKind::HunkHeader)
            {
                return Some(*li);
            }
        }
        visible.get(start).copied()
    }

    /// `parsed.lines` index of the last rendered body line.
    fn bottom_body_line(&self) -> Option<usize> {
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let win = self.diff_page_size();
        let rows = win.saturating_sub(if start > 0 { 1 } else { 0 });
        let last = start
            .saturating_add(rows)
            .min(visible.len())
            .saturating_sub(1);
        visible.get(last).copied()
    }

    pub fn diff_scroll_left(&mut self, n: usize) {
        self.diff_col = self.diff_col.saturating_sub(n);
    }

    pub fn diff_scroll_right(&mut self, n: usize) {
        let max = self
            .max_visible_line_width()
            .saturating_sub(self.diff_width as usize)
            .min(MAX_HSCROLL);
        self.diff_col = self.diff_col.saturating_add(n).min(max);
    }

    fn max_visible_line_width(&self) -> usize {
        self.parsed().map_or(0, |p| {
            p.lines
                .iter()
                .map(|l| l.raw_text.chars().count())
                .max()
                .unwrap_or(0)
        })
    }

    pub fn visible_line_indices(&self) -> Vec<usize> {
        let Some(parsed) = self.parsed() else {
            return Vec::new();
        };
        parsed
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.kind == LineKind::HunkHeader || !self.collapsed_hunks.contains(&l.hunk_index)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn clamp_diff_scroll(&mut self) {
        let max = max_scroll_for(self.visible_line_indices().len(), self.diff_page_size());
        self.diff_scroll = self.diff_scroll.min(max);
    }

    pub fn collapse_hunk(&mut self, hunk: usize) {
        self.collapsed_hunks.insert(hunk);
        self.snap_view_to_chunk(hunk);
    }

    pub fn expand_hunk(&mut self, hunk: usize) {
        self.collapsed_hunks.remove(&hunk);
        self.clamp_diff_scroll();
    }

    pub fn toggle_hunk(&mut self, hunk: usize) {
        if self.collapsed_hunks.contains(&hunk) {
            self.expand_hunk(hunk);
        } else {
            self.collapse_hunk(hunk);
        }
    }

    fn snap_view_to_chunk(&mut self, hunk: usize) {
        let Some(parsed) = self.parsed() else {
            return;
        };
        let visible = self.visible_line_indices();
        if let Some(pos) = visible.iter().position(|&li| {
            parsed.lines[li].kind == LineKind::HunkHeader && parsed.lines[li].hunk_index == hunk
        }) {
            self.diff_scroll = pos + 1;
        }
        self.active_chunk = Some(hunk);
        self.clamp_diff_scroll();
    }

    fn enter_chunk_from_bottom(&mut self, dest: usize) {
        let Some(parsed) = self.parsed() else {
            return;
        };
        let visible = self.visible_line_indices();
        let win = self.diff_page_size();
        if let Some(last) = visible
            .iter()
            .rposition(|&li| parsed.lines[li].hunk_index == dest)
        {
            self.diff_scroll = last.saturating_sub(win.saturating_sub(1));
        }
        self.active_chunk = Some(dest);
        self.clamp_diff_scroll();
    }

    pub fn go_first_chunk(&mut self) {
        self.snap_view_to_chunk(0);
    }

    pub fn go_last_chunk(&mut self) {
        let max = self
            .parsed()
            .map_or(0, |p| p.header.hunks.len().saturating_sub(1));
        self.snap_view_to_chunk(max);
    }

    pub fn go_chunk_start(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.snap_view_to_chunk(hunk);
        }
    }

    pub fn go_chunk_end(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.enter_chunk_from_bottom(hunk);
        }
    }

    pub fn toggle_hunk_at_cursor(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.toggle_hunk(hunk);
        }
    }

    pub fn hunk_at_screen_row(&self, row: u16) -> Option<usize> {
        let parsed = self.parsed()?;
        let header_h = self.last_header_height as usize;
        if (row as usize) < header_h {
            return None;
        }
        let body_row = (row as usize) - header_h;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let line_idx = if let Some(sticky) = self.sticky_line {
            if body_row == 0 {
                return parsed.lines.get(sticky).map(|l| l.hunk_index);
            }
            visible.get(start + body_row - 1).copied()
        } else {
            visible.get(start + body_row).copied()
        }?;
        let line = parsed.lines.get(line_idx)?;
        (line.kind == LineKind::HunkHeader).then_some(line.hunk_index)
    }

    pub fn open_hunk_at_cursor(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.expand_hunk(hunk);
        }
    }

    pub fn close_hunk_at_cursor(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.collapse_hunk(hunk);
        }
    }

    pub fn expand_all_hunks(&mut self) {
        self.collapsed_hunks.clear();
        self.clamp_diff_scroll();
    }

    pub fn collapse_all_hunks(&mut self) {
        if let Some(parsed) = self.parsed() {
            self.collapsed_hunks = (0..parsed.header.hunks.len()).collect();
        }
        self.clamp_diff_scroll();
    }

    /// Toggle expand/collapse of the focused tree dir (Space on a dir).
    pub fn toggle_dir(&mut self) {
        let idx = match self.selected_node() {
            Some(i) => i,
            None => return,
        };
        if self.tree.nodes[idx].kind != tree::Kind::Dir {
            return;
        }
        let now_expanded = !self.tree.nodes[idx].expanded;
        self.tree.nodes[idx].expanded = now_expanded;
        if now_expanded {
            self.expanded_dirs.insert(self.tree.nodes[idx].path.clone());
        } else {
            self.expanded_dirs.remove(&self.tree.nodes[idx].path);
        }
        self.flat = self.tree.flatten();
        if let Some(pos) = self.flat.iter().position(|&x| x == idx) {
            self.selection = pos;
        }
        self.diff_scroll = 0;
    }

    /// Space: toggle the selected tree dir / load the selected file diff, or
    /// toggle the active hunk when diff has focus.
    pub fn space_action(&mut self) {
        match self.focus {
            Focus::Tree => match self.selected_node() {
                Some(idx) if self.tree.nodes[idx].kind == tree::Kind::Dir => self.toggle_dir(),
                Some(_) => self.load_diff(),
                None => {}
            },
            Focus::Diff => self.toggle_hunk_at_cursor(),
        }
    }

    /// Grow (`delta > 0`) or shrink (`delta < 0`) context, clamped to
    /// `[0, MAX_CONTEXT]`. Sends a fresh `Request::Diff` at the new context
    /// level when the value changes.
    pub fn change_context(&mut self, delta: i32) {
        let new_ctx = (self.context_lines as i32 + delta).clamp(0, MAX_CONTEXT as i32) as u32;
        if new_ctx == self.context_lines {
            return;
        }
        self.context_lines = new_ctx;
        self.request_diff();
        self.clamp_diff_scroll();
    }
}

/// Collect the set of expanded dir paths from a freshly-built `Tree` (its
/// default expansion state).
fn collect_expanded(tree: &Tree) -> HashSet<String> {
    tree.nodes
        .iter()
        .filter(|n| n.kind == tree::Kind::Dir && n.expanded)
        .map(|n| n.path.clone())
        .collect()
}

/// Find the flat-list position of `old_path` in a new `flat`/`tree`; if the
/// exact path is gone, pick the nearest surviving sibling by lexicographic
/// name in the same parent dir, else the parent dir, else the first entry.
fn relocate_selection(flat: &[usize], tree: &Tree, old_path: &Option<String>) -> usize {
    let Some(old) = old_path else {
        return 0;
    };
    // Exact match.
    for (pos, &idx) in flat.iter().enumerate() {
        if tree.nodes[idx].path == *old {
            return pos;
        }
    }
    // Nearest sibling by name in the same parent dir.
    let parent = old.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let old_name = old.rsplit('/').next().unwrap_or("");
    let mut candidates: Vec<(String, usize)> = flat
        .iter()
        .enumerate()
        .filter_map(|(pos, &idx)| {
            let n = &tree.nodes[idx];
            let node_parent = n.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            (node_parent == parent).then(|| (n.name.clone(), pos))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    // Nearest surviving sibling: first file after `old_name` in sort order,
    // else the last file before it.
    if let Some((_, pos)) = candidates.iter().find(|(name, _)| name.as_str() > old_name) {
        return *pos;
    }
    if let Some((_, pos)) = candidates.iter().rev().find(|(name, _)| name.as_str() < old_name) {
        return *pos;
    }
    // Parent dir, else first.
    for (pos, &idx) in flat.iter().enumerate() {
        if tree.nodes[idx].path == parent && tree.nodes[idx].kind == tree::Kind::Dir {
            return pos;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileHeader, HunkMeta, ParsedDiff};
    use crate::test_support::*;

    #[test]
    fn toggle_hunk_helper() {
        let mut app = sample_app();
        assert!(app.collapsed_hunks.is_empty());
        app.toggle_hunk(3);
        assert!(app.collapsed_hunks.contains(&3));
        app.toggle_hunk(3);
        assert!(!app.collapsed_hunks.contains(&3));
    }

    fn app_with_diff(parsed: ParsedDiff) -> App {
        let mut app = sample_app();
        app.snapshot = snapshot_with_diff(parsed);
        app
    }

    #[test]
    fn navigate_chunk_jumps_when_on_screen() {
        let mut app = app_with_diff(fitting_hunk());
        app.focus = Focus::Diff;
        app.height = 20;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(0);

        app.navigate_chunk(1);
        assert_eq!(app.active_chunk, Some(1));
        assert_eq!(app.diff_scroll, 0);
    }

    #[test]
    fn navigate_chunk_scrolls_when_off_screen() {
        let parsed = ParsedDiff {
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
                hunks: (0..2)
                    .map(|_| HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            lines: std::iter::once(hunk_header(0))
                .chain((0..10).map(|i| ctx_line(i, 0)))
                .chain(std::iter::once(hunk_header(1)))
                .chain(std::iter::once(ctx_line(10, 1)))
                .collect(),
        };
        let mut app = app_with_diff(parsed);
        app.focus = Focus::Diff;
        app.height = 3;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(0);

        app.navigate_chunk(1);
        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.active_chunk, Some(0));
    }

    #[test]
    fn collapse_snaps_view_to_chunk() {
        let mut app = app_with_diff(fitting_hunk());
        app.focus = Focus::Diff;
        app.height = 3;
        app.last_header_height = 1;
        app.diff_scroll = 3;
        app.active_chunk = Some(1);

        app.collapse_hunk(1);

        assert_eq!(app.diff_scroll, 3);
        assert_eq!(app.active_chunk, Some(1));
    }

    #[test]
    fn up_scrolls_smoothly() {
        let parsed = ParsedDiff {
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
                ctx_line(2, 1),
                ctx_line(3, 1),
                ctx_line(4, 1),
                ctx_line(5, 1),
                hunk_header(2),
                ctx_line(6, 2),
                ctx_line(7, 2),
                ctx_line(8, 2),
                ctx_line(9, 2),
                ctx_line(10, 2),
            ],
        };
        let mut app = app_with_diff(parsed);
        app.focus = Focus::Diff;
        app.height = 3;
        app.last_header_height = 1;
        app.diff_scroll = 10;
        app.active_chunk = Some(2);

        app.navigate_chunk(-1);

        assert_eq!(app.diff_scroll, 9);
        assert_eq!(app.active_chunk, Some(2));
    }

    #[test]
    fn up_selects_and_reveals_in_one_move() {
        let mut app = app_with_diff(fitting_hunk());
        app.focus = Focus::Diff;
        app.collapsed_hunks = [0, 1].into_iter().collect();
        app.height = 2;
        app.last_header_height = 1;
        app.diff_scroll = 3;
        app.active_chunk = Some(2);

        app.navigate_chunk(-1);

        assert_eq!(app.active_chunk, Some(1));
        assert_eq!(app.diff_scroll, 2);
    }

    #[test]
    fn scroll_activates_header_when_enters() {
        let parsed = ParsedDiff {
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
                hunks: (0..2)
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
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        };
        let mut app = app_with_diff(parsed);
        app.focus = Focus::Diff;
        app.height = 5;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = None;

        app.navigate_chunk(1);

        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.active_chunk, Some(1));
    }

    #[test]
    fn current_hunk_is_the_cursor() {
        let mut app = sample_app();
        app.active_chunk = Some(2);
        assert_eq!(app.active_chunk, Some(2));
        app.active_chunk = None;
        assert_eq!(app.active_chunk, None);
    }

    #[test]
    fn apply_snapshot_relocates_selection_by_path() {
        let mut app = sample_app();
        // Select b.rs (flat index 1 in the two-file tree).
        app.move_down(1);
        assert_eq!(app.selected_path.as_deref(), Some("b.rs"));

        // New snapshot where b.rs is gone and c.rs appears: selection should
        // jump to the nearest sibling (c.rs).
        let tree = Tree::build(vec![('M', "a.rs".to_string()), ('M', "c.rs".to_string())]);
        app.apply_snapshot(sample_snapshot(tree));
        assert_eq!(app.selected_path.as_deref(), Some("c.rs"));
    }

    #[test]
    fn apply_snapshot_keeps_expansion_state() {
        let tree = Tree::build(vec![
            ('M', "src/a.rs".to_string()),
            ('M', "src/b.rs".to_string()),
        ]);
        let (tx, _rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = App::new(sample_snapshot(tree), tx, false);
        // Collapse src/.
        app.collapse();
        assert!(app.tree.nodes[app.flat[0]].kind == tree::Kind::Dir);
        assert!(!app.tree.nodes[app.flat[0]].expanded);

        // New snapshot with the same tree structure: src/ should stay collapsed.
        let tree2 = Tree::build(vec![
            ('M', "src/a.rs".to_string()),
            ('M', "src/b.rs".to_string()),
        ]);
        app.apply_snapshot(sample_snapshot(tree2));
        let src_idx = app.flat[0];
        assert!(!app.tree.nodes[src_idx].expanded);
    }
}
