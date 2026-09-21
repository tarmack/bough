//! Application state: the diff/tree model view plus the panes' view state.
//!
//! `App` owns the mutable state of the viewer (selection, scroll positions,
//! collapsed hunks, focus) and the operations that drive it. Immutable diff
//! data arrives as `ArcSnapshot`s from the worker thread; `App` swaps them in
//! via `apply_snapshot` and never blocks on git. Input routing lives in
//! `crate::input` and rendering in `crate::view`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::diff::{Diff, LineKind};
use crate::highlight::TokenOwned;
use crate::preview;
use crate::snapshot::{ArcSnapshot, Request, Seq};
use crate::text::max_scroll_for;
use crate::theme::Focus;
use crate::tree::{self, Tree};

/// Hard cap for horizontal scroll (cells), applied in `diff_scroll_right`.
pub const MAX_HSCROLL: usize = 10_000;
/// Upper bound for the `-U<N>` context window.
pub const MAX_CONTEXT: u32 = 200;
/// Context-line delta applied by the `[`/`]` keys.
pub const CONTEXT_STEP: i32 = 3;
/// Cells scrolled horizontally per `h`/`l` press.
pub const HSCROLL_STEP: usize = 2;

/// Which changes the app shows. Named for the user's mental model, not git
/// plumbing: "what work am I looking at?" for users keeping oversight of
/// changes (e.g. made by AI coding agents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The branch's work: everything since an origin — the branch point on
    /// the trunk by default, or any ref the user picked — plus any
    /// uncommitted edits.
    Branch(Origin),
    /// Only uncommitted working-tree edits.
    Uncommitted,
}

/// Where a [`Mode::Branch`] comparison is measured from. The default is the
/// startup comparison (usually the trunk's branch point, e.g. `main`); a
/// picked origin is any ref chosen from the base picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The default comparison the app started with.
    Default,
    /// A ref the user picked: a branch, tag, or commit.
    Picked(String),
}

/// One row in the base-picker popup: `rev` is the revision spec the diff is
/// computed against (`<rev>...HEAD`), `label` is what the row displays.
pub struct PickerEntry {
    pub rev: String,
    pub label: String,
}

/// Base-picker popup state (opened with `b` for branches, `t` for
/// commits/tags).
pub struct BasePicker {
    /// Popup title, e.g. ` compare against a branch `.
    pub title: String,
    /// Selectable rows.
    pub entries: Vec<PickerEntry>,
    /// Selected row index.
    pub selection: usize,
}

/// Cache for [`App::visible_line_indices`]. The visibility set depends only on
/// the current snapshot's diff and the `collapsed_hunks` set; both are cheap to
/// fingerprint (`Arc::ptr_eq` on an owned clone of the snapshot Arc + a
/// `HashSet` compare), so the expensive O(n) line scan runs only when one of
/// them actually changes instead of every frame. Holding the old `Arc` also
/// stops the allocator from reusing its address for a different snapshot,
/// which the old pointer-based fingerprint could mistake for a cache hit.
struct VisibleCache {
    /// Clone of the snapshot Arc the cached lines were computed for.
    snap: ArcSnapshot,
    /// `collapsed_hunks` the cached lines were computed for.
    collapsed: HashSet<usize>,
    lines: Rc<Vec<usize>>,
    /// Max `chars().count()` over all `diff.lines` at the time `lines` was
    /// computed (used for horizontal-scroll clamping).
    max_width: usize,
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
    /// Pinned sticky file-header line (click-row mapping), if any.
    pub sticky_file: Option<usize>,
    /// Pinned sticky hunk-header line (click-row mapping), if any.
    pub sticky_hunk: Option<usize>,
    /// Cursor for the current-chunk highlight: the chunk selected by chunk
    /// navigation (`j`/`k`), folding, or clicks. `None` before a chunk is
    /// selected.
    pub active_chunk: Option<usize>,

    /// Channel to the worker thread.
    pub request_tx: Sender<Request>,
    /// Monotonic counter stamped onto each `Request::Diff`/`Highlight`.
    pub seq: Seq,
    /// Last `(from, to)` highlight range (raw `diff.lines` indices, exclusive
    /// end) sent for the selected file; `maybe_request_highlight` only sends
    /// when the visible range moves outside it.
    last_highlight_range: Option<(usize, usize)>,
    /// True when the file watcher degraded to poll mode (for the status line).
    pub poll_mode: bool,
    /// Current diff base: the default branch, a picked origin, or `HEAD`.
    pub mode: Mode,
    /// Revision for `Mode::Branch(Origin::Default)` (the startup revision).
    main_rev: String,
    /// Revision for `Mode::Uncommitted` (`HEAD`).
    uncommitted_rev: String,
    /// Origin of the last `Mode::Branch` view, remembered across an
    /// `Uncommitted` toggle.
    last_origin: Option<Origin>,
    /// Open base-picker popup, or `None` when closed.
    pub base_picker: Option<BasePicker>,
    /// Memoized `visible_line_indices` (see [`VisibleCache`]).
    visible_cache: RefCell<VisibleCache>,
    /// Kitty-protocol image preview state for the selected file.
    pub preview: crate::preview::Preview,
}

impl App {
    pub fn new(
        snapshot: ArcSnapshot,
        request_tx: Sender<Request>,
        poll_mode: bool,
        main_rev: String,
        uncommitted_rev: String,
    ) -> Self {
        let expanded_dirs = collect_expanded(&snapshot.tree);
        let mut tree = snapshot.tree.clone();
        tree.expand_paths(&expanded_dirs);
        let flat = tree.flatten();
        // Select the first tree node (a directory or a file) so the diff pane
        // renders something immediately on open. Empty repos have no base to
        // diff a folder against, so pick the first file there.
        let selected_path = flat.first().and_then(|&idx| {
            let n = &tree.nodes[idx];
            if n.kind == tree::Kind::Dir && snapshot.empty {
                None
            } else {
                Some(n.path.clone())
            }
        });
        // The initial snapshot Arc; also seed the visibility cache's fingerprint
        // (an owned clone, compared by `Arc::ptr_eq`).
        let init_snap = snapshot.clone();
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
            sticky_file: None,
            sticky_hunk: None,
            active_chunk: None,
            request_tx,
            seq: 0,
            last_highlight_range: None,
            poll_mode,
            mode: Mode::Branch(Origin::Default),
            main_rev,
            uncommitted_rev,
            last_origin: None,
            base_picker: None,
            visible_cache: RefCell::new(VisibleCache {
                snap: init_snap,
                collapsed: HashSet::new(),
                lines: Rc::new(Vec::new()),
                max_width: 0,
            }),
            preview: crate::preview::Preview::new(),
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
        let content_changed = self.snapshot.selected_diff != snap.selected_diff;
        let old_section = self.top_visible_section();
        let old_path = self.selected_path.clone();

        self.snapshot = snap;
        // Trees are born collapsed; only dirs the user has expanded live in
        // `expanded_dirs`, so a fresh snapshot can never open new folders.
        let mut tree = self.snapshot.tree.clone();
        tree.expand_paths(&self.expanded_dirs);
        self.flat = tree.flatten();

        // Re-locate selection by path; nearest-sibling fallback.
        self.selection = relocate_selection(&self.flat, &tree, &old_path);
        self.tree = tree;
        let new_selected = self
            .flat
            .get(self.selection)
            .map(|&idx| self.tree.nodes[idx].path.clone());
        let relocated = new_selected != old_path;
        self.selected_path = new_selected;

        // Re-anchor the scroll only when the diff's content actually changed
        // (a recompute, context change, or rev switch). Progress-highlight
        // snapshots carry the same `Diff`, so re-anchoring on them would snap a
        // mid-section scroll back to that section's header instead of
        // preserving the user's position.
        match old_section {
            Some(section) if content_changed => self.reanchor_diff_scroll(section),
            _ => self.clamp_diff_scroll(),
        }

        // A relocation (the selected path vanished and a sibling was chosen)
        // changes the selection without an input event; ask the worker for its
        // diff so the pane never shows stale content.
        if relocated {
            self.request_diff();
        }
    }

    /// Section of the topmost visible header (file or hunk), or the first
    /// visible line's section — recorded before a snapshot swap so the view
    /// can follow it.
    fn top_visible_section(&self) -> Option<usize> {
        let diff = self.snapshot.selected_diff.as_ref()?;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        // Sticky pinned header first.
        if start > 0 {
            if let Some(&li) = visible
                .iter()
                .take(start)
                .rev()
                .find(|&&li| diff.lines[li].kind.is_header())
            {
                return Some(diff.lines[li].section);
            }
        }
        visible.get(start).map(|&li| diff.lines[li].section)
    }

    /// Scroll so the header for `section` is at the top of the diff pane after
    /// a snapshot swap; fall back to clamping when the section is gone.
    fn reanchor_diff_scroll(&mut self, section: usize) {
        let Some(diff) = self.snapshot.selected_diff.as_ref() else {
            self.diff_scroll = 0;
            return;
        };
        let visible = self.visible_line_indices();
        if let Some(pos) = visible
            .iter()
            .position(|&li| diff.lines[li].kind.is_header() && diff.lines[li].section == section)
        {
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
        // A fresh diff starts un-highlighted; the highlight nudge re-arms below.
        self.last_highlight_range = None;
        let _ = self.request_tx.send(Request::Diff {
            path,
            ctx: self.context_lines,
            seq: self.seq,
        });
        self.request_preview();
    }

    /// Ask the worker to decode the selected file as an image when the preview
    /// is active and the selection is an image file; otherwise hide any preview.
    fn request_preview(&mut self) {
        let is_file = self
            .selected_node()
            .map(|idx| self.tree.nodes[idx].kind == tree::Kind::File)
            .unwrap_or(false);
        let path = self.selected_path.clone();
        let wanted = self.preview.enabled
            && is_file
            && path.as_deref().map(preview::is_image_path).unwrap_or(false);
        if !wanted {
            self.preview.current = None;
            self.preview.requested = None;
            return;
        }
        let Some(path) = path else {
            return;
        };
        self.seq = self.seq.saturating_add(1);
        let _ = self.request_tx.send(Request::LoadImage {
            path: path.clone(),
            seq: self.seq,
        });
        self.preview.requested = Some(path.clone());
        // Keep the old image on screen while the same file re-decodes (its
        // `accept` bumps the generation so `sync` re-places on arrival); a
        // different file hides it immediately.
        if self.preview.current.as_ref().map(|c| c.path.as_str()) != Some(path.as_str()) {
            self.preview.current = None;
        }
    }

    /// Enable or disable image previews (called once at startup after the kitty
    /// capability probe) and refresh the current selection accordingly.
    pub fn set_preview_enabled(&mut self, on: bool) {
        self.preview.set_enabled(on);
        self.request_preview();
    }

    /// Ask the worker to highlight the currently-visible diff region (viewport
    /// plus one screen of lookahead) whenever it changes. Called once per frame;
    /// the request is keyed off the viewport's actual line range, so scrolling,
    /// jumping (`G`/`g`), and resize all trigger it, not just per-scroll events.
    pub fn maybe_request_highlight(&mut self) {
        let Some(diff) = self.snapshot.selected_diff.as_ref() else {
            return;
        };
        let Some(path) = self.selected_path.clone() else {
            return;
        };
        // Only highlight the diff that actually matches the selection; a stale
        // (old-file) diff means the new one is still loading.
        if diff.header.path != path {
            return;
        }
        let visible = self.visible_line_indices();
        if visible.is_empty() {
            return;
        }
        let page = self.diff_page_size();
        let len = visible.len();
        let start = self.diff_scroll.min(len.saturating_sub(1));
        let end = (start + page).min(len);
        let with_lookahead = (end + page).min(len);
        // Raw line range (exclusive end) covering the viewport plus one screen
        // ahead.
        let from = visible[start];
        let to = visible[with_lookahead - 1] + 1;
        if self.last_highlight_range == Some((from, to)) {
            return;
        }
        let seq = self.seq.saturating_add(1);
        let req = Request::Highlight {
            path,
            ctx: self.context_lines,
            from,
            to,
            seq,
        };
        if self.request_tx.try_send(req).is_ok() {
            self.seq = seq;
            self.last_highlight_range = Some((from, to));
        }
    }

    /// The active diff base revision.
    fn current_rev(&self) -> String {
        match &self.mode {
            Mode::Branch(Origin::Default) => self.main_rev.clone(),
            Mode::Branch(Origin::Picked(rev)) => format!("{rev}...HEAD"),
            Mode::Uncommitted => self.uncommitted_rev.clone(),
        }
    }

    /// Short name of the default origin (the left side of the startup
    /// revision, e.g. `main` in `main...HEAD`), or `None` when the startup
    /// revision compares against `HEAD` itself.
    fn default_origin(&self) -> Option<String> {
        self.main_rev
            .split_once("...")
            .map(|(base, _)| base.trim().to_string())
    }

    /// Switch between the branch view and uncommitted-only. The branch view's
    /// origin is remembered so the round trip returns to it.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode.clone() {
            Mode::Branch(origin) => {
                self.last_origin = Some(origin);
                Mode::Uncommitted
            }
            Mode::Uncommitted => Mode::Branch(self.last_origin.clone().unwrap_or(Origin::Default)),
        };
        self.apply_base_switch();
    }

    /// Reset the diff view and ask the worker to recompute at the current rev.
    /// Shared by every base switch (`toggle_mode`, `set_base`).
    fn apply_base_switch(&mut self) {
        self.diff_scroll = 0;
        self.diff_col = 0;
        self.active_chunk = None;
        self.collapsed_hunks.clear();
        // The worker drops its cache on SetRev, so highlights must be re-armed.
        self.last_highlight_range = None;
        let _ = self.request_tx.send(Request::SetRev {
            rev: self.current_rev(),
        });
    }

    /// Open the base picker populated from the repository's local branches.
    pub fn open_branch_picker(&mut self) {
        let entries = crate::git::local_branches()
            .into_iter()
            .map(|name| PickerEntry {
                rev: name.clone(),
                label: name,
            })
            .collect();
        self.open_picker_with(" compare against a branch ", entries);
    }

    /// Open the base picker populated with recent commits followed by tags.
    /// Commits diff against the full oid, tags against the tag name.
    pub fn open_commit_picker(&mut self) {
        let commits = crate::git::recent_commits(50)
            .into_iter()
            .map(|(oid, subject)| {
                let short = oid.get(..7).unwrap_or(&oid).to_string();
                PickerEntry {
                    rev: oid,
                    label: format!("{short} {subject}"),
                }
            });
        let tags = crate::git::tags().into_iter().map(|name| PickerEntry {
            rev: name.clone(),
            label: name,
        });
        self.open_commit_picker_with(commits.chain(tags).collect());
    }

    /// Testable core of [`App::open_commit_picker`]: open the commit/tag picker
    /// with an explicit entry list.
    pub fn open_commit_picker_with(&mut self, entries: Vec<PickerEntry>) {
        self.open_picker_with(" compare against a commit or tag ", entries);
    }

    /// Testable core of the picker openers: open the popup with an explicit
    /// title and entry list. No-op when the list is empty; the selection starts
    /// on the current mode's base when it is present.
    pub fn open_picker_with(&mut self, title: &str, entries: Vec<PickerEntry>) {
        if entries.is_empty() {
            return;
        }
        let current = self.base_name();
        let selection = current
            .as_deref()
            .and_then(|b| entries.iter().position(|e| e.rev == b))
            .unwrap_or(0);
        self.base_picker = Some(BasePicker {
            title: title.to_string(),
            entries,
            selection,
        });
    }

    /// Close the base picker (Esc/q, or after a pick).
    pub fn close_picker(&mut self) {
        self.base_picker = None;
    }

    /// Enter in the picker: switch the diff base to the selected entry's rev.
    pub fn select_picked_base(&mut self) {
        let Some(rev) = self
            .base_picker
            .as_ref()
            .and_then(|p| p.entries.get(p.selection).map(|e| e.rev.clone()))
        else {
            self.base_picker = None;
            return;
        };
        self.base_picker = None;
        self.set_base(rev);
    }

    /// Switch the comparison origin to `base...HEAD`. Picking the default
    /// origin (e.g. `main`) canonicalizes back to [`Origin::Default`], so it
    /// is presented exactly like the app's startup view rather than as a
    /// custom pick. Picking the branch you are on compares against `HEAD`
    /// itself — that is the uncommitted view, so switch to it.
    pub fn set_base(&mut self, base: String) {
        if self.snapshot.branch_state.name.as_deref() == Some(base.as_str()) {
            self.mode = Mode::Uncommitted;
            self.apply_base_switch();
            return;
        }
        let origin = if self.default_origin().as_deref() == Some(base.as_str()) {
            Origin::Default
        } else {
            Origin::Picked(base)
        };
        self.mode = Mode::Branch(origin.clone());
        self.last_origin = Some(origin);
        self.apply_base_switch();
    }

    /// Short name of the current view's origin, when one applies.
    fn base_name(&self) -> Option<String> {
        match &self.mode {
            Mode::Branch(Origin::Default) => self.default_origin(),
            Mode::Branch(Origin::Picked(rev)) => Some(rev.clone()),
            Mode::Uncommitted => self.last_origin.as_ref().map(|o| match o {
                Origin::Default => self.default_origin(),
                Origin::Picked(rev) => Some(rev.clone()),
            })?,
        }
    }

    /// The footer's always-present mode label: `vs <origin>` for a branch
    /// comparison, or `uncommitted`. Never empty, so the footer shows the
    /// active mode even before the first worker snapshot arrives.
    pub fn mode_label(&self) -> String {
        match &self.mode {
            Mode::Uncommitted => "uncommitted".to_string(),
            Mode::Branch(origin) => {
                let base = match origin {
                    Origin::Default => self
                        .default_origin()
                        .unwrap_or_else(|| self.main_rev.trim().to_string()),
                    Origin::Picked(rev) => short_origin(rev),
                };
                format!("vs {base}")
            }
        }
    }

    // ── Convenience accessors for snapshot data ─────────────────────────

    pub fn diff(&self) -> Option<&Diff> {
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

    pub fn branch_state(&self) -> &crate::git::BranchState {
        &self.snapshot.branch_state
    }

    // ── Selection / navigation ──────────────────────────────────────────

    pub fn selected_node(&self) -> Option<usize> {
        self.flat.get(self.selection).copied()
    }

    /// Load a fresh diff for the selected file. Each file switch starts a
    /// clean pane: no horizontal scroll, nothing collapsed, default context.
    pub fn load_diff(&mut self) {
        self.reset_diff_view();
        self.active_chunk = if self.diff().is_some_and(|p| !p.sections.is_empty()) {
            Some(0)
        } else {
            None
        };
        self.request_diff();
    }

    /// Reset per-file view state (`collapsed_hunks`, `diff_col`,
    /// `context_lines`, `diff_scroll`, `active_chunk`) so a file switch never
    /// inherits stale section indices from the previous file.
    fn reset_diff_view(&mut self) {
        self.diff_col = 0;
        self.collapsed_hunks.clear();
        self.context_lines = 3;
        self.diff_scroll = 0;
        self.active_chunk = None;
    }

    /// Update `selected_path` from the current flat selection, then send a
    /// `Request::Diff` so the worker fetches the diff.
    fn select_and_request(&mut self) {
        self.selected_path = self
            .flat
            .get(self.selection)
            .map(|&idx| self.tree.nodes[idx].path.clone());
        self.reset_diff_view();
        self.request_diff();
    }

    pub fn move_down(&mut self, n: usize) {
        if self.flat.is_empty() {
            return;
        }
        let last = self.flat.len() - 1;
        let new = self.selection.saturating_add(n).min(last);
        if new == self.selection {
            return;
        }
        self.selection = new;
        self.select_and_request();
    }

    pub fn move_up(&mut self, n: usize) {
        let new = self.selection.saturating_sub(n);
        if new == self.selection {
            return;
        }
        self.selection = new;
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

        self.rebuild_flat_and_select(idx);
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
        let max = max_scroll_for(
            self.visible_line_indices().len(),
            self.diff_page_size(),
            self.sticky_row_count(),
        );
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

    /// Move the active-section selection by `delta` sections (clamped to the
    /// currently visible headers). Used when the diff fits in the pane (no
    /// scrolling possible) so selection still moves across files and hunks,
    /// skipping any folded-away sections.
    pub fn move_active_chunk(&mut self, delta: i32) {
        let headers = self.visible_headers();
        if headers.is_empty() {
            return;
        }
        let cur_pos = self
            .active_chunk
            .and_then(|s| headers.iter().position(|&h| h == s));
        let next_pos = match cur_pos {
            Some(p) => (p as i32 + delta).clamp(0, headers.len() as i32 - 1) as usize,
            None => {
                if delta > 0 {
                    0
                } else {
                    headers.len() - 1
                }
            }
        };
        self.active_chunk = Some(headers[next_pos]);
    }

    /// Move the section cursor by `delta`: jump to the adjacent section (file
    /// or hunk) when its header is already in view, otherwise scroll one line
    /// in the direction. Scrolling activates a section as soon as its header
    /// line enters the view. Sections folded away (a collapsed file's hunks)
    /// are skipped so the cursor moves to the next actually-visible header.
    pub fn navigate_chunk(&mut self, delta: i32) {
        if let Some(cur) = self.active_chunk {
            let dest = cur as i32 + delta;
            let in_range = self
                .diff()
                .is_some_and(|p| dest >= 0 && (dest as usize) < p.sections.len());
            if in_range {
                let dest = dest as usize;
                if self.chunk_header_in_view(dest) {
                    self.active_chunk = Some(dest);
                    return;
                }
                // The adjacent section may be hidden by a folded file. Jump to
                // the nearest visible header rather than scrolling (which would
                // do nothing), and snap it into view so the selection is always
                // visible.
                if self.snap_nearest(dest, delta, cur) {
                    return;
                }
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
        self.activate_entered_header(delta);
    }

    /// Snap to the nearest visible header from `dest` in direction `delta`,
    /// skipping over any folded-away section, so the selection stays visible.
    /// Returns true when it snapped (and the caller should return early).
    fn snap_nearest(&mut self, dest: usize, delta: i32, cur: usize) -> bool {
        let headers = self.visible_headers();
        if headers.contains(&dest) {
            return false;
        }
        if let Some(next) = Self::nearest_header(&headers, dest, delta) {
            if next != cur {
                self.snap_view_to_chunk(next);
                return true;
            }
        }
        false
    }

    /// After a scroll, activate the header section that just entered the view
    /// (the bottom body line when scrolling down, the top one when up).
    fn activate_entered_header(&mut self, delta: i32) {
        let entered = if delta > 0 {
            self.bottom_body_line()
        } else {
            self.top_body_line()
        };
        if let Some(li) = entered {
            if let Some(diff) = self.diff() {
                if diff.lines[li].kind.is_header() {
                    self.active_chunk = Some(diff.lines[li].section);
                }
            }
        }
    }

    /// Sections of the header lines currently visible (in document order).
    fn visible_headers(&self) -> Vec<usize> {
        let Some(diff) = self.diff() else {
            return Vec::new();
        };
        self.visible_line_indices()
            .iter()
            .filter(|&&li| diff.lines[li].kind.is_header())
            .map(|&li| diff.lines[li].section)
            .collect()
    }

    /// Nearest header section in `headers` in direction `delta` from `from`
    /// (the first after it going down, before it going up).
    fn nearest_header(headers: &[usize], from: usize, delta: i32) -> Option<usize> {
        if delta > 0 {
            headers
                .iter()
                .find(|&&h| h > from)
                .copied()
                .or_else(|| headers.last().copied())
        } else {
            headers
                .iter()
                .rev()
                .find(|&&h| h < from)
                .copied()
                .or_else(|| headers.first().copied())
        }
    }

    /// Whether `section`'s header is currently visible (in-flow window or the
    /// pinned sticky top header).
    fn chunk_header_in_view(&self, section: usize) -> bool {
        let Some(diff) = self.diff() else {
            return false;
        };
        let visible = self.visible_line_indices();
        if visible.is_empty() {
            return false;
        }
        let start = self.diff_scroll.min(visible.len());
        let end = start
            .saturating_add(self.diff_page_size())
            .min(visible.len());
        let sticky =
            visible.iter().take(start).rev().find_map(|&li| {
                (diff.lines[li].kind.is_header()).then_some(diff.lines[li].section)
            });
        if sticky == Some(section) {
            return true;
        }
        visible[start..end]
            .iter()
            .any(|&li| diff.lines[li].kind.is_header() && diff.lines[li].section == section)
    }

    /// `diff.lines` index of the top rendered body line: the nearest scrolled-off
    /// header (the bottom sticky) if present, else the first in-flow line.
    fn top_body_line(&self) -> Option<usize> {
        let diff = self.diff()?;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        if start > 0 {
            if let Some(&li) = visible
                .iter()
                .take(start)
                .rev()
                .find(|&&li| diff.lines[li].kind.is_header())
            {
                return Some(li);
            }
        }
        visible.get(start).copied()
    }

    /// Number of pinned sticky rows at the current scroll position, mirroring
    /// `view::diff::sticky_headers`: at most one file header, plus one hunk
    /// header only when it falls after the pinned file header.
    pub(crate) fn sticky_row_count(&self) -> usize {
        let Some(diff) = self.diff() else {
            return 0;
        };
        let visible = self.visible_line_indices();
        let start = self.diff_scroll.min(visible.len());
        if start == 0 {
            return 0;
        }
        let file_pos = (0..start)
            .rev()
            .find(|&p| diff.lines[visible[p]].kind == LineKind::FileHeader);
        let hunk_start = file_pos.map_or(0, |p| p + 1);
        let hunk = (hunk_start..start)
            .rev()
            .any(|p| diff.lines[visible[p]].kind == LineKind::HunkHeader);
        usize::from(file_pos.is_some()) + usize::from(hunk)
    }

    /// `diff.lines` index of the last rendered body line, subtracting the
    /// pinned sticky rows (mirroring the render path's `flow_height`).
    fn bottom_body_line(&self) -> Option<usize> {
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let rows = self
            .diff_page_size()
            .saturating_sub(self.sticky_row_count());
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
        // Populate the shared cache (only rescanned on a snapshot/collapse
        // change), then reuse its stored max width instead of re-scanning every
        // `h`/`l` press.
        self.visible_line_indices();
        self.visible_cache.borrow().max_width
    }

    /// Indices of the body lines that are not folded away, in document order.
    ///
    /// Memoized: the set depends only on the current snapshot's diff and the
    /// `collapsed_hunks` set, so it is recomputed only when those change
    /// (not on every frame). The caller gets a cheap `Rc` clone; the O(n) scan
    /// happens only on cache misses.
    pub fn visible_line_indices(&self) -> Rc<Vec<usize>> {
        let mut cache = self.visible_cache.borrow_mut();
        let same_snapshot = Arc::ptr_eq(&cache.snap, &self.snapshot);
        if !same_snapshot || cache.collapsed != self.collapsed_hunks {
            // `max_width` depends only on the snapshot, not on folding, so a
            // collapse-only change can reuse the cached value.
            if !same_snapshot {
                cache.max_width = self.compute_max_width();
            }
            cache.snap = self.snapshot.clone();
            cache.collapsed = self.collapsed_hunks.clone();
            cache.lines = Rc::new(self.compute_visible_lines());
        }
        Rc::clone(&cache.lines)
    }

    /// Max display width over all `diff.lines` in terminal cells (wide
    /// glyphs count as 2), regardless of folding. 0 when there is no diff.
    fn compute_max_width(&self) -> usize {
        self.diff().map_or(0, |p| {
            p.lines
                .iter()
                .map(|l| crate::text::display_width(&l.raw_text))
                .max()
                .unwrap_or(0)
        })
    }

    /// The memoized body of `visible_line_indices`: a single O(n) pass over
    /// `diff.lines` collecting the not-folded indices.
    fn compute_visible_lines(&self) -> Vec<usize> {
        let Some(diff) = self.diff() else {
            return Vec::new();
        };
        diff.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| self.line_visible(diff, l))
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether a body line is hidden because its section (or owning file
    /// section) is collapsed. Header rows stay visible — a collapsed file or
    /// hunk shows its header with a fold arrow — while their bodies fold.
    fn line_visible(&self, diff: &Diff, l: &crate::diff::DiffLine) -> bool {
        let s = l.section;
        let file_sec = diff.file_section_for(s);
        let file_hidden = file_sec.is_some_and(|fs| self.collapsed_hunks.contains(&fs));
        if file_hidden && !(l.kind.is_header() && file_sec == Some(s)) {
            // The file is folded; keep only its own header row.
            return false;
        }
        if l.kind.is_header() {
            return true;
        }
        !self.collapsed_hunks.contains(&s)
    }

    pub fn clamp_diff_scroll(&mut self) {
        let max = max_scroll_for(
            self.visible_line_indices().len(),
            self.diff_page_size(),
            self.sticky_row_count(),
        );
        self.diff_scroll = self.diff_scroll.min(max);
    }

    pub fn collapse_hunk(&mut self, section: usize) {
        self.collapsed_hunks.insert(section);
        self.snap_view_to_chunk(section);
    }

    pub fn expand_hunk(&mut self, section: usize) {
        self.collapsed_hunks.remove(&section);
        self.clamp_diff_scroll();
    }

    pub fn toggle_hunk(&mut self, section: usize) {
        if self.collapsed_hunks.contains(&section) {
            self.expand_hunk(section);
        } else {
            self.collapse_hunk(section);
        }
    }

    fn snap_view_to_chunk(&mut self, section: usize) {
        let Some(diff) = self.diff() else {
            return;
        };
        let visible = self.visible_line_indices();
        if let Some(pos) = visible
            .iter()
            .position(|&li| diff.lines[li].kind.is_header() && diff.lines[li].section == section)
        {
            self.diff_scroll = pos + 1;
        }
        self.active_chunk = Some(section);
        self.clamp_diff_scroll();
    }

    fn enter_chunk_from_bottom(&mut self, dest: usize) {
        let Some(diff) = self.diff() else {
            return;
        };
        let visible = self.visible_line_indices();
        let win = self.diff_page_size();
        if let Some(last) = visible
            .iter()
            .rposition(|&li| diff.lines[li].section == dest)
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
            .diff()
            .map_or(0, |p| p.sections.len().saturating_sub(1));
        self.snap_view_to_chunk(max);
    }

    pub fn go_chunk_start(&mut self) {
        if let Some(section) = self.active_chunk {
            self.snap_view_to_chunk(section);
        }
    }

    pub fn go_chunk_end(&mut self) {
        if let Some(section) = self.active_chunk {
            self.enter_chunk_from_bottom(section);
        }
    }

    pub fn toggle_hunk_at_cursor(&mut self) {
        if let Some(section) = self.active_chunk {
            self.toggle_hunk(section);
        }
    }

    pub fn hunk_at_screen_row(&self, row: u16) -> Option<usize> {
        let diff = self.diff()?;
        let header_h = self.last_header_height as usize;
        if (row as usize) < header_h {
            return None;
        }
        let body_row = (row as usize) - header_h;
        let file = self.sticky_file;
        let hunk = self.sticky_hunk;
        let sticky_rows = usize::from(file.is_some()) + usize::from(hunk.is_some());

        // A click on a pinned sticky row maps to that header's section.
        if body_row < sticky_rows {
            let idx = if body_row == 0 { file.or(hunk) } else { hunk };
            return idx.and_then(|i| diff.lines.get(i).map(|l| l.section));
        }

        let start = self.diff_scroll;
        let line_idx = self
            .visible_line_indices()
            .get(start + body_row - sticky_rows)
            .copied()?;
        let line = diff.lines.get(line_idx)?;
        (line.kind.is_header()).then_some(line.section)
    }

    pub fn open_hunk_at_cursor(&mut self) {
        if let Some(section) = self.active_chunk {
            self.expand_hunk(section);
        }
    }

    pub fn close_hunk_at_cursor(&mut self) {
        if let Some(section) = self.active_chunk {
            self.collapse_hunk(section);
        }
    }

    pub fn expand_all_hunks(&mut self) {
        self.collapsed_hunks.clear();
        self.clamp_diff_scroll();
    }

    pub fn collapse_all_hunks(&mut self) {
        if let Some(diff) = self.diff() {
            self.collapsed_hunks = (0..diff.sections.len()).collect();
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
        self.rebuild_flat_and_select(idx);
    }

    /// Re-flatten `self.tree`, reposition `self.selection` onto the flat
    /// position of node `idx`, and reset the diff scroll to the top.
    fn rebuild_flat_and_select(&mut self, idx: usize) {
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
        // A context change renumbers sections, so the old fold/cursor indices
        // no longer refer to the same hunks.
        self.collapsed_hunks.clear();
        self.active_chunk = None;
        self.request_diff();
        self.clamp_diff_scroll();
    }
}

/// Shortens a full 40-hex commit oid to 7 characters for the footer; branch
/// and tag names pass through unchanged.
fn short_origin(rev: &str) -> String {
    if rev.len() == 40 && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        rev[..7].to_string()
    } else {
        rev.to_string()
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
    if let Some((_, pos)) = candidates
        .iter()
        .rev()
        .find(|(name, _)| name.as_str() < old_name)
    {
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
    use crate::diff::{Diff, FileHeader, FileStatus, HunkMeta};
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

    #[test]
    fn toggle_mode_cycles_and_sends_setrev() {
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        assert_eq!(app.mode, Mode::Branch(Origin::Default));

        app.toggle_mode();
        assert_eq!(app.mode, Mode::Uncommitted);
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }

        app.toggle_mode();
        assert_eq!(app.mode, Mode::Branch(Origin::Default));
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "main...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn set_base_sends_setrev_and_resets_view() {
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![('M', "a.rs".to_string())])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        // Drain the initial Diff request sent by `App::new`.
        let _ = rx.try_recv();

        app.diff_scroll = 5;
        app.diff_col = 3;
        app.active_chunk = Some(2);
        app.collapsed_hunks = [1].into_iter().collect();

        app.set_base("feature".to_string());

        assert_eq!(
            app.mode,
            Mode::Branch(Origin::Picked("feature".to_string()))
        );
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.diff_col, 0);
        assert_eq!(app.active_chunk, None);
        assert!(app.collapsed_hunks.is_empty());
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "feature...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn set_base_default_branch_canonicalizes_to_default_origin() {
        // Picking `main` from the base picker must present exactly like the
        // app's startup view (default origin, blue), not as a custom pick
        // (magenta): the two are the same comparison.
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();

        app.set_base("main".to_string());

        assert_eq!(app.mode, Mode::Branch(Origin::Default));
        assert_eq!(app.mode_label(), "vs main");
        // The startup revision is sent verbatim, not `main...HEAD` rebuilt
        // from the picked name.
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "main...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn set_base_own_branch_is_the_uncommitted_view() {
        // Picking the branch you are on compares against `HEAD` itself:
        // present that as the uncommitted view, not a branch view.
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut snap = sample_snapshot(Tree::build(Vec::new()));
        std::sync::Arc::get_mut(&mut snap).unwrap().branch_state = crate::git::BranchState {
            name: Some("feature".into()),
            ..Default::default()
        };
        let mut app = App::new(
            snap,
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();

        app.set_base("feature".to_string());

        assert_eq!(app.mode, Mode::Uncommitted);
        assert_eq!(app.mode_label(), "uncommitted");
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn toggle_round_trip_returns_to_branch() {
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![('M', "a.rs".to_string())])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();

        app.toggle_mode();
        assert_eq!(app.mode, Mode::Uncommitted);
        app.toggle_mode();
        assert_eq!(app.mode, Mode::Branch(Origin::Default));
    }

    #[test]
    fn toggle_round_trip_returns_to_picked_branch() {
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![('M', "a.rs".to_string())])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();

        app.set_base("feature".to_string());
        let _ = rx.try_recv();
        app.toggle_mode();
        assert_eq!(app.mode, Mode::Uncommitted);
        app.toggle_mode();
        assert_eq!(
            app.mode,
            Mode::Branch(Origin::Picked("feature".to_string()))
        );
    }

    #[test]
    fn mode_label_covers_all_modes() {
        let (tx, _rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        assert_eq!(app.mode_label(), "vs main");

        app.set_base("feature/x".to_string());
        assert_eq!(app.mode_label(), "vs feature/x");

        app.toggle_mode();
        assert_eq!(app.mode_label(), "uncommitted");
    }

    #[test]
    fn mode_label_shortens_full_commit_oid() {
        let (tx, _rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.set_base("abcdef1234567890abcdef1234567890abcdef12".to_string());
        assert_eq!(app.mode_label(), "vs abcdef1");
    }

    #[test]
    fn mode_label_branch_without_origin_rev() {
        let (tx, _rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "HEAD".to_string(),
            "HEAD".to_string(),
        );
        assert_eq!(app.mode_label(), "vs HEAD");
    }

    #[test]
    fn open_picker_prefills_current_base() {
        let mut app = sample_app();
        app.set_base("feature".to_string());
        app.open_picker_with(
            " compare against a branch ",
            vec![
                PickerEntry {
                    rev: "develop".to_string(),
                    label: "develop".to_string(),
                },
                PickerEntry {
                    rev: "feature".to_string(),
                    label: "feature".to_string(),
                },
                PickerEntry {
                    rev: "main".to_string(),
                    label: "main".to_string(),
                },
            ],
        );
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.selection, 1);

        app.close_picker();
        assert!(app.base_picker.is_none());

        // An empty list is a no-op: no popup opens.
        app.open_picker_with(" compare against a branch ", Vec::new());
        assert!(app.base_picker.is_none());
    }

    fn app_with_diff(diff: Diff) -> App {
        let mut app = sample_app();
        app.snapshot = snapshot_with_diff(diff);
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
        let diff = single_diff(
            FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: (0..2)
                    .map(|_| HunkMeta {
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            std::iter::once(hunk_header(0))
                .chain((0..10).map(|i| ctx_line(i, 0)))
                .chain(std::iter::once(hunk_header(1)))
                .chain(std::iter::once(ctx_line(10, 1)))
                .collect(),
        );
        let mut app = app_with_diff(diff);
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
        let diff = single_diff(
            FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 1,
                deleted: 1,
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
            },
            vec![
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
        );
        let mut app = app_with_diff(diff);
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
        let diff = single_diff(
            FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: (0..2)
                    .map(|_| HunkMeta {
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        );
        let mut app = app_with_diff(diff);
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
    fn apply_snapshot_preserves_scroll_when_diff_unchanged() {
        let diff = single_diff(
            header_full(None, None),
            std::iter::once(hunk_header(0))
                .chain((0..30).map(|i| ctx_line(i, 0)))
                .collect(),
        );
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(diff.clone());
        app.height = 8;
        app.last_header_height = 1;
        app.diff_scroll = 7;

        // A highlight-only snapshot carries the same diff content; applying it
        // must preserve the scroll position rather than snapping to the top of
        // the sticky section.
        app.apply_snapshot(snapshot_with_diff(diff));

        assert_eq!(app.diff_scroll, 7);
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
        let mut app = App::new(
            sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
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

    #[test]
    fn navigate_skips_collapsed_file_sections() {
        // A two-file folder diff: File sections 0 and 2, hunk sections 1 and 3.
        let mk = |path: &str| crate::diff::FileHeader {
            path: path.into(),
            status: FileStatus::Modified,
            added: 1,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: vec![HunkMeta {
                added: 1,
                deleted: 0,
            }],
        };
        let mut app = sample_app();
        app.snapshot = snapshot_with_diff(Diff {
            header: mk("src"),
            files: vec![mk("src/a.rs"), mk("src/b.rs")],
            sections: vec![
                crate::diff::Section {
                    kind: crate::diff::SectionKind::File,
                    file: 0,
                    hunk: None,
                },
                crate::diff::Section {
                    kind: crate::diff::SectionKind::Hunk,
                    file: 0,
                    hunk: Some(0),
                },
                crate::diff::Section {
                    kind: crate::diff::SectionKind::File,
                    file: 1,
                    hunk: None,
                },
                crate::diff::Section {
                    kind: crate::diff::SectionKind::Hunk,
                    file: 1,
                    hunk: Some(0),
                },
            ],
            lines: vec![
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::FileHeader,
                    raw_text: "src/a.rs".into(),
                    section: 0,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::HunkHeader,
                    raw_text: "@@".into(),
                    section: 1,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::FileHeader,
                    raw_text: "src/b.rs".into(),
                    section: 2,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::HunkHeader,
                    raw_text: "@@".into(),
                    section: 3,
                    word_runs: None,
                },
            ],
        });
        app.active_chunk = Some(0); // File 0's header
        app.height = 20;
        app.last_header_height = 1;

        // Collapse file 0: its hunk (section 1) disappears.
        app.collapse_hunk(0);

        // Moving down from the collapsed file must land on the next visible
        // section (file 1, section 2), not the folded-away hunk.
        app.navigate_chunk(1);
        assert_eq!(app.active_chunk, Some(2));

        // The same holds for the no-scroll path.
        app.active_chunk = Some(0);
        app.move_active_chunk(1);
        assert_eq!(app.active_chunk, Some(2));
    }

    #[test]
    fn navigate_up_brings_collapsed_file_into_view() {
        // Two-file folder diff; file 0 (sections 0, 1) is collapsed.
        let mk = |path: &str| crate::diff::FileHeader {
            path: path.into(),
            status: FileStatus::Modified,
            added: 1,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: vec![HunkMeta {
                added: 1,
                deleted: 0,
            }],
        };
        let mut app = sample_app();
        app.snapshot = snapshot_with_diff(Diff {
            header: mk("src"),
            files: vec![mk("src/a.rs"), mk("src/b.rs")],
            sections: vec![
                crate::diff::Section {
                    kind: crate::diff::SectionKind::File,
                    file: 0,
                    hunk: None,
                },
                crate::diff::Section {
                    kind: crate::diff::SectionKind::Hunk,
                    file: 0,
                    hunk: Some(0),
                },
                crate::diff::Section {
                    kind: crate::diff::SectionKind::File,
                    file: 1,
                    hunk: None,
                },
                crate::diff::Section {
                    kind: crate::diff::SectionKind::Hunk,
                    file: 1,
                    hunk: Some(0),
                },
            ],
            lines: vec![
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::FileHeader,
                    raw_text: "src/a.rs".into(),
                    section: 0,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::HunkHeader,
                    raw_text: "@@".into(),
                    section: 1,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::FileHeader,
                    raw_text: "src/b.rs".into(),
                    section: 2,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::HunkHeader,
                    raw_text: "@@".into(),
                    section: 3,
                    word_runs: None,
                },
            ],
        });
        app.height = 2;
        app.last_header_height = 1;
        app.collapse_hunk(0); // Fold file 0; its header stays visible.
                              // Select file 1 and scroll it to the bottom so file 0's header is
                              // above the viewport.
        app.active_chunk = Some(2);
        app.diff_scroll = usize::MAX;

        // Moving up must land on file 0 and bring its header into view.
        app.navigate_chunk(-1);
        assert_eq!(app.active_chunk, Some(0));
        assert!(app.chunk_header_in_view(0), "file 0 header must be visible");
    }

    #[test]
    fn move_between_files_resets_view_state() {
        let mut app = sample_app();
        app.selected_path = Some("a.rs".into());
        app.collapsed_hunks = [3].into_iter().collect();
        app.diff_col = 7;
        app.context_lines = 10;
        app.active_chunk = Some(5);
        app.diff_scroll = 4;

        app.move_down(1);

        assert_eq!(app.selected_path.as_deref(), Some("b.rs"));
        assert!(app.collapsed_hunks.is_empty());
        assert_eq!(app.diff_col, 0);
        assert_eq!(app.context_lines, 3);
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.active_chunk, None);
    }
}
