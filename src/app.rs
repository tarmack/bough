//! Application state: the diff/tree model view plus the panes' view state.
//!
//! `App` owns the mutable state of the viewer (selection, scroll positions,
//! collapsed hunks, focus) and the operations that drive it. Immutable diff
//! data arrives as `ArcSnapshot`s from the worker thread; `App` swaps them in
//! via `apply_snapshot` and never blocks on git. Input routing lives in
//! `crate::input` and rendering in `crate::view`.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender};
use ratatui::layout::Rect;

use crate::diff::{Diff, LineKind};
use crate::git::{DirtyStats, WorktreeInfo};
use crate::highlight::TokenOwned;
use crate::perf;
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
/// Worktree id of the main checkout (the synthetic picker entry); the
/// per-worktree view-memory map and `current_worktree`'s `None` both
/// canonicalize to it.
pub(crate) const MAIN_WORKTREE: &str = "main";

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

/// Which flavor of picker popup is open; Enter (and clicks) dispatch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Compare against a rev (branch / commit-tag pickers).
    Base,
    /// Switch the worktree being viewed.
    Worktree,
}

/// One row in the base-picker popup: `label` is what the row displays and
/// `rev` is the revision spec the diff is computed against
/// (`<rev>...HEAD`). Worktree rows ([`PickerKind::Worktree`]) overload
/// `rev` with the worktree id (mirroring `label`); the row's full
/// [`WorktreeInfo`] lives in [`BasePicker::worktrees`].
pub struct PickerEntry {
    pub rev: String,
    pub label: String,
}

/// Base-picker popup state (opened with `b` for branches, `t` for
/// commits/tags, `w` for worktrees).
pub struct BasePicker {
    /// Popup title, e.g. ` compare against a branch `.
    pub title: String,
    /// Selectable rows.
    pub entries: Vec<PickerEntry>,
    /// Selected row index.
    pub selection: usize,
    /// Case-insensitive substring filter typed into the popup's filter row;
    /// empty means unfiltered. Lives on the picker, so it resets on every
    /// open and dies on close.
    pub filter: String,
    /// Which picker flavor is open; Enter dispatches on it.
    pub kind: PickerKind,
    /// Worktree rows for [`PickerKind::Worktree`], index-aligned with
    /// `entries` (empty for the base kinds).
    pub worktrees: Vec<WorktreeInfo>,
}

impl BasePicker {
    /// Case-insensitive substring haystack for row `i`: the revision spec
    /// and label for base rows; the worktree id, branch, and path for
    /// worktree rows (whose `rev`/`label` hold only the id, so building
    /// from them would search the id twice).
    fn haystack(&self, i: usize) -> String {
        let entry = &self.entries[i];
        let mut hay = format!("{} {}", entry.rev, entry.label);
        if self.kind == PickerKind::Worktree {
            if let Some(wt) = self.worktrees.get(i) {
                hay = format!(
                    "{} {} {}",
                    wt.id,
                    wt.branch.as_deref().unwrap_or(""),
                    wt.path.display()
                );
            }
        }
        hay.to_lowercase()
    }

    /// Real indices of the rows matching the filter, in display order. An
    /// empty filter selects every row.
    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        (0..self.entries.len())
            .filter(|&i| self.haystack(i).contains(&needle))
            .collect()
    }

    /// Whether row `i` can hold the selection (`missing` worktree rows
    /// render dimmed and are unselectable).
    fn selectable(&self, i: usize) -> bool {
        self.kind != PickerKind::Worktree || self.worktrees.get(i).is_some_and(|w| !w.missing)
    }
}

/// Sort key for worktree-picker rows: missing worktrees always last, then
/// by uncommitted-change count (descending, unknown = 0), then by tip
/// commit time (descending, unknown last). A stable sort preserves the git
/// listing order (main first, then id) for full ties.
fn worktree_sort_key(
    w: &WorktreeInfo,
    dirty: Option<usize>,
) -> (bool, Reverse<usize>, Reverse<i64>) {
    (
        w.missing,
        Reverse(dirty.unwrap_or(0)),
        Reverse(w.tip_time.unwrap_or(i64::MIN)),
    )
}

/// The view state remembered per worktree so switching back restores what
/// the user was looking at. Deliberately excludes toggles that are session
/// preferences (`word_diff_on`, context width, collapsed hunks): those carry
/// across switches, not per-worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeUiState {
    /// Which comparison the worktree was being viewed in.
    pub mode: Mode,
    /// The origin the worktree's branch view was measured from, so an
    /// `Uncommitted` mode round-trips (`m`) to the worktree's own origin
    /// instead of one inherited from another worktree.
    pub last_origin: Option<Origin>,
    /// The selected file path (dirs select too).
    pub selected_path: Option<String>,
    /// Expanded directory set, as of switch-out.
    pub expanded_dirs: HashSet<String>,
    pub tree_scroll: usize,
    pub diff_scroll: usize,
    /// Horizontal (column) scroll of the diff pane.
    pub diff_col: usize,
}

/// Interactive screen regions recorded by the last render for mouse
/// hit-testing. Defaults are inert so a skipped frame (tiny terminal)
/// never routes a stale click.
#[derive(Debug, Default, Clone)]
pub struct MouseGeometry {
    /// Tree list rows (the area below the header strip).
    pub tree_list: Option<Rect>,
    /// The footer row.
    pub footer: Option<Rect>,
    /// Footer pill hitboxes with their action zones, in draw order.
    pub footer_zones: Vec<(FooterZone, Rect)>,
    /// Base-picker popup geometry while the picker is open.
    pub picker: Option<PickerGeometry>,
}

/// The footer pill a click activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterZone {
    /// The source/origin pill: left-click opens the branch picker,
    /// right-click the commit/tag picker.
    Source,
    /// The mode pill: cycles the view mode.
    Mode,
    /// The `worktree:<id>` pill (linked worktrees only): opens the
    /// worktree picker.
    Worktree,
}

/// Base-picker popup geometry in screen coordinates.
#[derive(Debug, Clone, Copy)]
pub struct PickerGeometry {
    /// Outer popup rect, border included.
    pub popup: Rect,
    /// Inner rect, inside the border — the entry-list area.
    pub inner: Rect,
    /// Real index of the first entry row rendered in `inner` (the top inner
    /// row is the filter box; entry rows follow beneath it).
    pub first_row: usize,
}

/// Cache for [`App::visible_line_indices`]. The visibility set depends only on
/// the current snapshot's diff and the `collapsed_hunks` set; both are cheap to
/// fingerprint (`Arc::ptr_eq` on an owned clone + a `HashSet` compare), so the
/// expensive O(n) line scan runs only when one of them actually changes instead
/// of every frame. Highlight fills swap the snapshot's highlight `Arc` but never
/// the diff, so they reuse the cached lines: the per-row highlight overlay is
/// applied at render time, not baked into the line list. Holding the old `Arc`
/// also stops the allocator from reusing an address for a different diff, which
/// a raw-pointer fingerprint could mistake for a cache hit.
struct VisibleCache {
    /// Selected diff the cached lines were computed for; compared by
    /// `Arc::ptr_eq` (both `None` = unchanged).
    diff: Option<Arc<Diff>>,
    /// `collapsed_hunks` the cached lines were computed for.
    collapsed: HashSet<usize>,
    lines: Rc<Vec<usize>>,
    /// Sorted visible-list positions of the diff's section-header rows,
    /// rebuilt with `lines` whenever the diff or fold set changes.
    headers: Rc<HeaderPositions>,
    /// Max `chars().count()` over all `diff.lines` at the time `lines` was
    /// computed (used for horizontal-scroll clamping).
    max_width: usize,
}

/// Sorted positions within the visible-lines list of a diff's section-header
/// rows, split by kind. Precomputed once per visible-lines rebuild so sticky
/// lookups are O(log n) instead of scanning back from the scroll position.
#[derive(Default)]
pub(crate) struct HeaderPositions {
    pub(crate) file: Vec<usize>,
    pub(crate) hunk: Vec<usize>,
}

impl HeaderPositions {
    /// Position of the last file header before `start`, if any.
    pub(crate) fn file_before(&self, start: usize) -> Option<usize> {
        self.file
            .partition_point(|&p| p < start)
            .checked_sub(1)
            .map(|i| self.file[i])
    }

    /// Position of the last hunk header in `from..start`, if any.
    pub(crate) fn hunk_before(&self, start: usize, from: usize) -> Option<usize> {
        let i = self.hunk.partition_point(|&p| p < start).checked_sub(1)?;
        (self.hunk[i] >= from).then_some(self.hunk[i])
    }

    /// Position of the last header of any kind before `start`, if any.
    pub(crate) fn any_before(&self, start: usize) -> Option<usize> {
        match (self.file_before(start), self.hunk_before(start, 0)) {
            (Some(f), Some(h)) => Some(f.max(h)),
            (f, h) => f.or(h),
        }
    }

    /// Positions of the sticky file and hunk headers at `start`, as
    /// `(file_pos, hunk_pos)`. The hunk is confined to lines after the file
    /// header, so scrolling into a new file does not pin the previous file's
    /// hunk under the new header. Shared by rendering
    /// (`view::diff::sticky_headers`) and scroll math ([`App::sticky_row_count`])
    /// so the two can never desync.
    pub(crate) fn sticky_pair(&self, start: usize) -> (Option<usize>, Option<usize>) {
        let file_pos = self.file_before(start);
        let hunk_from = file_pos.map_or(0, |p| p + 1);
        (file_pos, self.hunk_before(start, hunk_from))
    }

    /// [`sticky_pair`](Self::sticky_pair) with the renderer's row cap applied:
    /// no pinned rows when `body_height == 0`, and the hunk sticky dropped when
    /// only one body row exists and a file sticky already occupies it. Shared
    /// by the render path and [`App::sticky_row_count`] so scroll math and
    /// drawing can never disagree on how many rows are pinned.
    pub(crate) fn sticky_pair_capped(
        &self,
        start: usize,
        body_height: u16,
    ) -> (Option<usize>, Option<usize>) {
        let (file, hunk) = self.sticky_pair(start);
        if body_height == 0 {
            return (None, None);
        }
        if body_height < 2 && file.is_some() {
            return (file, None);
        }
        (file, hunk)
    }
}

/// Linewise mouse selection in the diff pane: `anchor` holds the line where
/// the drag started while a click/drag is in progress; `range` is the
/// highlighted span (inclusive line indices) that survives after release.
#[derive(Debug, Default, Clone)]
pub struct DiffSelection {
    pub anchor: Option<usize>,
    pub range: Option<(usize, usize)>,
    pub dragged: bool,
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
    /// Shared watcher degradation flag (for the status line). `main` swaps in
    /// the worker's real `Arc`, so the footer reflects a mid-run degradation.
    pub poll_mode: Arc<AtomicBool>,
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
    /// Transient picker-open failure (e.g. the repository could not be
    /// opened), surfaced on the status line. Cleared on the next attempt.
    picker_error: Option<String>,
    /// Memoized `visible_line_indices` (see [`VisibleCache`]).
    visible_cache: RefCell<VisibleCache>,
    /// Interactive screen regions recorded by the last render (see
    /// [`MouseGeometry`]).
    pub mouse: MouseGeometry,
    /// Kitty-protocol image preview state for the selected file.
    pub preview: crate::preview::Preview,
    /// Linewise mouse selection in the diff pane.
    pub diff_selection: DiffSelection,
    /// Transient status-line message ("copied N lines", "copied path") with its
    /// expiry; shown for ~3 seconds.
    pub flash: Option<(String, Instant)>,
    /// Id of the linked worktree being viewed (`worktree:<id>` footer pill);
    /// `None` in the main checkout. UI agent owns the full switching UX.
    pub current_worktree: Option<String>,
    /// Per-worktree view memory, keyed by worktree id (`main` for the main
    /// checkout). Written on switch-out, consumed on switch-back.
    pub worktree_ui: HashMap<String, WorktreeUiState>,
    /// View state of the worktree just switched to, awaiting its first tree
    /// snapshot (consumed once in `apply_snapshot`).
    pending_restore: Option<WorktreeUiState>,
    /// Set while a worktree switch is in flight, until the switch's BUSY
    /// placeholder snapshot arrives in `apply_snapshot`. Keeps stale
    /// settled snapshots from the old root queued ahead of the placeholder
    /// (channel FIFO) from consuming `pending_restore`.
    awaiting_switch: bool,
    /// Results of the picker's background dirty-count scan: `(id, stats)`
    /// pairs streamed in by the scan thread. Dropped when the picker closes.
    pub dirty_rx: Option<Receiver<(String, DirtyStats)>>,
    /// Dirty counts seen so far, keyed by worktree id; feeds the picker's
    /// `±N` chips and survives picker close (a cheap cache).
    pub dirty_counts: HashMap<String, usize>,
    /// Cancel flag for the dirty-count scan thread: set when the receiver is
    /// dropped in `close_picker` so the thread stops before spending another
    /// git scan on a picker that is already gone (crossbeam's `Sender` has
    /// no disconnect check).
    dirty_scan_cancel: Arc<AtomicBool>,
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
        let mut tree = (*snapshot.tree).clone();
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
        // The initial snapshot Arc; the visibility cache starts empty and
        // recomputes on first query.
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
            poll_mode: Arc::new(AtomicBool::new(poll_mode)),
            mode: Mode::Branch(Origin::Default),
            main_rev,
            uncommitted_rev,
            last_origin: None,
            base_picker: None,
            picker_error: None,
            visible_cache: RefCell::new(VisibleCache {
                diff: None,
                collapsed: HashSet::new(),
                lines: Rc::new(Vec::new()),
                headers: Rc::new(HeaderPositions::default()),
                max_width: 0,
            }),
            mouse: MouseGeometry::default(),
            preview: crate::preview::Preview::new(),
            diff_selection: DiffSelection::default(),
            flash: None,
            current_worktree: None,
            worktree_ui: HashMap::new(),
            pending_restore: None,
            awaiting_switch: false,
            dirty_rx: None,
            dirty_counts: HashMap::new(),
            dirty_scan_cancel: Arc::new(AtomicBool::new(false)),
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
        // Pointer comparison: a highlight-only snapshot shares the same `Diff`
        // Arc, so it must not be treated as a content change.
        let content_changed = match (&self.snapshot.selected_diff, &snap.selected_diff) {
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            (None, None) => false,
            _ => true,
        };
        let tree_changed = !Arc::ptr_eq(&self.snapshot.tree, &snap.tree);
        let highlight_changed =
            !Arc::ptr_eq(&self.snapshot.selected_highlight, &snap.selected_highlight);
        let old_section = content_changed
            .then(|| self.top_visible_section())
            .flatten();
        let old_path = self.selected_path.clone();
        let mut relocated = false;

        crate::perf_evt!(
            "snapshot seq={} tree={} diff={} hl={} busy={}",
            snap.seq,
            tree_changed,
            content_changed,
            highlight_changed,
            snap.busy
        );

        self.snapshot = snap;

        // A BUSY placeholder (e.g. the worktree switch's own marker
        // snapshot) clears the awaiting-switch gate; it must never consume
        // the pending restore (guarded below).
        if self.snapshot.busy {
            self.awaiting_switch = false;
        }

        // A settled snapshot whose tree did NOT change can only mean the
        // switch never landed (the worker rejected it — its error snapshot
        // replays the old view's tree Arc verbatim). A real switch always
        // changes the tree Arc (the new root's tree is freshly built).
        // Drop the stale restore instead of letting a later unrelated
        // recompute consume it onto the old worktree's tree.
        if !self.snapshot.busy && !tree_changed && self.awaiting_switch {
            self.awaiting_switch = false;
            self.pending_restore = None;
        }

        if content_changed {
            // A recompute can deliver a new `Diff` Arc over the same visible
            // raw range; without re-arming, `maybe_request_highlight` would
            // suppress the request and the fresh diff would render bare.
            self.last_highlight_range = None;
            // The old selection's indices refer to the replaced diff's lines.
            self.diff_selection = DiffSelection::default();
        }

        if tree_changed {
            // Trees are born collapsed; only dirs the user has expanded live in
            // `expanded_dirs`, so a fresh snapshot can never open new folders.
            // A pending worktree-restore overrides the usual relocation
            // (consumed once, so later snapshots never re-trigger it). It may
            // only be consumed by the settled snapshot of the switched-to
            // worktree: the switch's BUSY placeholder cleared the
            // `awaiting_switch` gate above, while stale settled snapshots
            // from the old root queued ahead of it (channel FIFO) are
            // skipped while the gate is still set.
            let pending = if !self.snapshot.busy && !self.awaiting_switch {
                self.pending_restore.take()
            } else {
                None
            };
            if let Some(p) = pending {
                relocated = self.apply_pending_restore(&p);
            } else {
                let mut tree = (*self.snapshot.tree).clone();
                tree.expand_paths(&self.expanded_dirs);
                self.flat = tree.flatten();

                // Re-locate selection by path; nearest-sibling fallback.
                self.selection = relocate_selection(&self.flat, &tree, &old_path);
                self.tree = tree;
                let new_selected = self
                    .flat
                    .get(self.selection)
                    .map(|&idx| self.tree.nodes[idx].path.clone());
                relocated = new_selected != old_path;
                self.selected_path = new_selected;

                // A relocation (the selected path vanished and a sibling was chosen)
                // changes the selection without an input event; reset the per-file
                // view state and ask the worker for its diff so the pane never
                // shows stale content (or the dead file's section indices).
                if relocated {
                    self.reset_diff_view();
                    self.request_diff();
                }
            }
        }

        // Re-anchor the scroll only when the diff's content actually changed
        // (a recompute, context change, or rev switch). Progress-highlight
        // snapshots carry the same `Diff` Arc, so re-anchoring on them would
        // snap a mid-section scroll back that section's header instead of
        // preserving the user's position. A relocation already reset the view,
        // so it only needs a clamp.
        if relocated {
            self.clamp_diff_scroll();
        } else {
            match old_section {
                Some(section) => self.reanchor_diff_scroll(section),
                None => self.clamp_diff_scroll(),
            }
        }
    }

    /// Apply a pending per-worktree view restore against the tree that just
    /// arrived in `apply_snapshot`: expansion is the saved set ∩ the new
    /// tree, the selection is the saved path only when it survived (else
    /// reset), and the per-pane scrolls come back verbatim. Session
    /// preferences (context width, folded hunks) are preserved across the
    /// restore, so the re-requested diff uses the user's context level.
    /// Always returns `true`: a restore already relocated the view, so the
    /// caller only needs to clamp the diff scroll.
    fn apply_pending_restore(&mut self, state: &WorktreeUiState) -> bool {
        let mut tree = (*self.snapshot.tree).clone();
        let dirs: HashSet<String> = state
            .expanded_dirs
            .iter()
            .filter(|d| {
                tree.nodes
                    .iter()
                    .any(|n| n.kind == tree::Kind::Dir && n.path == **d)
            })
            .cloned()
            .collect();
        self.expanded_dirs = dirs.clone();
        tree.expand_paths(&dirs);
        self.flat = tree.flatten();
        self.selection = state
            .selected_path
            .as_deref()
            .and_then(|path| {
                self.flat
                    .iter()
                    .position(|&idx| tree.nodes[idx].path == path)
            })
            .unwrap_or(0);
        self.tree = tree;
        self.selected_path = self
            .flat
            .get(self.selection)
            .map(|&idx| self.tree.nodes[idx].path.clone());
        self.tree_scroll = state.tree_scroll;
        self.diff_col = state.diff_col;
        // `reset_diff_view` clears the collapsed set and resets the context
        // width; both are session preferences that carry across switches
        // (see `WorktreeUiState`), so save and re-apply them.
        let context_lines = self.context_lines;
        let collapsed_hunks = self.collapsed_hunks.clone();
        self.reset_diff_view();
        self.context_lines = context_lines;
        self.collapsed_hunks = collapsed_hunks;
        self.diff_scroll = state.diff_scroll;
        self.request_diff();
        true
    }

    /// True when the worker still has in-flight or pending work for the shown
    /// state (a settled snapshot follows). Drives wave-2 "scanning…" affordances.
    pub fn is_busy(&self) -> bool {
        self.snapshot.busy
    }

    /// Section of the topmost visible header (file or hunk), or the first
    /// visible line's section — recorded before a snapshot swap so the view
    /// can follow it.
    fn top_visible_section(&self) -> Option<usize> {
        let diff = self.snapshot.selected_diff.as_ref()?;
        let visible = self.visible_line_indices();
        let headers = self.header_positions();
        let start = self.diff_scroll;
        // Sticky pinned header first.
        if start > 0 {
            if let Some(pos) = headers.any_before(start) {
                return Some(diff.lines[visible[pos]].section);
            }
        }
        visible.get(start).map(|&li| diff.lines[li].section)
    }

    /// Scroll so the header for `section` is at the top of the diff pane after
    /// a snapshot swap; fall back to clamping when the section is gone.
    fn reanchor_diff_scroll(&mut self, section: usize) {
        if self.snapshot.selected_diff.is_none() {
            self.diff_scroll = 0;
            return;
        }
        self.scroll_to_section_header(section);
        self.clamp_diff_scroll();
    }

    /// Scroll `diff_scroll` to the line just below `section`'s header when that
    /// header is visible. Returns whether the section was found. Shared by
    /// re-anchoring after a snapshot swap and by cursor navigation.
    fn scroll_to_section_header(&mut self, section: usize) -> bool {
        let Some(diff) = self.diff() else {
            return false;
        };
        let visible = self.visible_line_indices();
        if let Some(pos) = visible
            .iter()
            .position(|&li| diff.lines[li].kind.is_header() && diff.lines[li].section == section)
        {
            self.diff_scroll = pos + 1;
            true
        } else {
            false
        }
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
        // While the worker is still streaming (e.g. a directory aggregate
        // arriving in chunks), the visible range churns on every snapshot; hold
        // highlights until the settled snapshot lands with the final range. The
        // settled pass then requests once, and a `last_highlight_range` left
        // untouched here never swallows that request.
        if self.snapshot.busy {
            return;
        }
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

    /// Track which worktree the app is viewing (`None` = main checkout);
    /// called at startup and on every in-app worktree switch.
    pub fn set_current_worktree(&mut self, id: Option<String>) {
        self.current_worktree = id;
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
        // The swapped diff retires the old line indices.
        self.diff_selection = DiffSelection::default();
        let _ = self.request_tx.send(Request::SetRev {
            rev: self.current_rev(),
        });
    }

    /// Open the base picker populated from the repository's local branches.
    pub fn open_branch_picker(&mut self) {
        self.picker_error = None;
        // `local_branches` swallows the repo-open error into an empty list;
        // probe once so the status line can explain why `b` did nothing.
        if let Err(e) = crate::git::open_repo() {
            self.picker_error = Some(format!("branches unavailable: {e}"));
        }
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
    /// Commits diff against the full oid, tags against the tag name. The repo is
    /// opened once and shared by both listings.
    pub fn open_commit_picker(&mut self) {
        self.picker_error = None;
        let entries = match crate::git::open_repo() {
            Ok(repo) => {
                let commits =
                    crate::git::recent_commits_in(&repo, 50)
                        .into_iter()
                        .map(|(oid, subject)| {
                            let short = oid.get(..7).unwrap_or(&oid).to_string();
                            PickerEntry {
                                rev: oid,
                                label: format!("{short} {subject}"),
                            }
                        });
                let tags = crate::git::tags_in(&repo)
                    .into_iter()
                    .map(|name| PickerEntry {
                        rev: name.clone(),
                        label: name,
                    });
                commits.chain(tags).collect()
            }
            Err(e) => {
                self.picker_error = Some(format!("commits unavailable: {e}"));
                Vec::new()
            }
        };
        self.open_commit_picker_with(entries);
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
            filter: String::new(),
            kind: PickerKind::Base,
            worktrees: Vec::new(),
        });
    }

    /// Open the worktree picker, listing every worktree of the repository. A
    /// background thread scans each worktree's uncommitted-change count so
    /// the rows' `±N` chips fill in without ever blocking the UI thread.
    pub fn open_worktree_picker(&mut self) {
        self.picker_error = None;
        // `worktrees` swallows the repo-open error into an empty list; probe
        // once so the status line can explain why `w` did nothing.
        if let Err(e) = crate::git::open_repo() {
            self.picker_error = Some(format!("worktrees unavailable: {e}"));
            return;
        }
        let (entries, worktree_err) = crate::git::worktrees_with_error();
        if let Some(e) = worktree_err {
            self.picker_error = Some(format!("worktrees unavailable: {e}"));
            return;
        }
        if entries.is_empty() {
            self.picker_error = Some("worktrees unavailable: none found".to_string());
            return;
        }
        self.open_worktree_picker_with(entries.clone());
        // Stream dirty counts in from a scan thread: one independent
        // repository open per worktree path, owned data only. Sends fail
        // harmlessly once the receiver is dropped (picker closed), and the
        // thread exits after this fixed list either way. A previous scan can
        // still be running after a rapid close/reopen; don't spawn a second
        // concurrent scanner over the same slot — the existing receiver and
        // the `dirty_counts` cache keep the chips filled until counts land.
        if self.dirty_rx.is_none() {
            // Only live worktrees get scanned: a `missing` row (pruned, or a
            // crafted/untrusted gitdir target) must never feed the scan
            // thread a filesystem path to enumerate.
            let targets: Vec<(String, PathBuf)> = entries
                .iter()
                .filter(|w| !w.missing)
                .map(|w| (w.id.clone(), w.path.clone()))
                .collect();
            let (tx, rx) = crossbeam_channel::unbounded::<(String, DirtyStats)>();
            self.dirty_scan_cancel.store(false, Ordering::Relaxed);
            let cancel = Arc::clone(&self.dirty_scan_cancel);
            let spawned = std::thread::Builder::new()
                .name("bough-dirty".into())
                .spawn(move || {
                    for (id, path) in targets {
                        // The picker can close mid-scan; stop before spending
                        // another git scan on a receiver that is already gone.
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Some(stats) = crate::git::dirty_count(&path) {
                            if tx.send((id, stats)).is_err() {
                                break;
                            }
                        }
                    }
                })
                .is_ok();
            if spawned {
                self.dirty_rx = Some(rx);
            }
        }
    }

    /// Testable core of [`App::open_worktree_picker`]: open the worktree
    /// picker with an explicit entry list (no git calls, no scan thread).
    pub fn open_worktree_picker_with(&mut self, entries: Vec<WorktreeInfo>) {
        if entries.is_empty() {
            return;
        }
        // Activity order (see `worktree_sort_key`); cached dirty counts from
        // a previous picker visit apply immediately.
        let mut entries = entries;
        entries.sort_by_key(|w| worktree_sort_key(w, self.dirty_counts.get(&w.id).copied()));
        let current = self
            .current_worktree
            .clone()
            .unwrap_or_else(|| MAIN_WORKTREE.to_string());
        let mut selection = entries.iter().position(|w| w.id == current).unwrap_or(0);
        if entries[selection].missing {
            selection = entries.iter().position(|w| !w.missing).unwrap_or(selection);
        }
        let picker_entries = entries
            .iter()
            .map(|w| PickerEntry {
                rev: w.id.clone(),
                label: w.id.clone(),
            })
            .collect();
        self.base_picker = Some(BasePicker {
            title: " switch worktree ".to_string(),
            entries: picker_entries,
            selection,
            filter: String::new(),
            kind: PickerKind::Worktree,
            worktrees: entries,
        });
    }

    /// Close the base picker (Esc/q, or after a pick).
    pub fn close_picker(&mut self) {
        self.base_picker = None;
        // The dirty-count scan has no audience anymore; dropping the
        // receiver makes the scan thread's remaining sends fail harmlessly,
        // and the cancel flag stops it before its next git scan.
        self.dirty_rx = None;
        self.dirty_scan_cancel.store(true, Ordering::Relaxed);
    }

    /// Enter in the picker: switch the diff base to the selected entry's rev,
    /// or switch worktrees when the worktree picker is open. A no-op when the
    /// selection fell out of the filtered subset (the filter emptied the
    /// list): never a stale pick from before the rows narrowed.
    pub fn select_picked_base(&mut self) {
        let target = self.base_picker.as_ref().and_then(|p| {
            p.filtered_indices()
                .contains(&p.selection)
                .then_some((p.kind, p.selection))
        });
        let Some((kind, selection)) = target else {
            return;
        };
        match kind {
            PickerKind::Worktree => self.select_picked_worktree(),
            PickerKind::Base => {
                let rev = self.base_picker.as_ref().unwrap().entries[selection]
                    .rev
                    .clone();
                self.base_picker = None;
                self.set_base(rev);
            }
        }
    }

    /// Enter in the worktree picker: switch to the selected worktree, unless
    /// its directory is gone (flash an error instead).
    fn select_picked_worktree(&mut self) {
        let Some(wt) = self
            .base_picker
            .as_ref()
            .and_then(|p| p.worktrees.get(p.selection).cloned())
        else {
            self.base_picker = None;
            return;
        };
        if wt.missing {
            self.flash = Some(("worktree directory is gone".to_string(), Instant::now()));
            return;
        }
        self.switch_worktree(&wt);
    }

    /// Switch the view to worktree `wt`: save the current worktree's view
    /// state, aim the worker at the new root, and stash the target's saved
    /// state for the two-phase restore in `apply_snapshot`. Switching to the
    /// worktree already viewed only closes the picker (the worker would drop
    /// the same-root switch, and the view must not visibly reset).
    fn switch_worktree(&mut self, wt: &WorktreeInfo) {
        let from = self
            .current_worktree
            .clone()
            .unwrap_or_else(|| MAIN_WORKTREE.to_string());
        if from == wt.id {
            self.close_picker();
            return;
        }
        self.worktree_ui.insert(from, self.capture_ui_state());
        let saved = self.worktree_ui.remove(&wt.id);
        self.set_current_worktree((!wt.main).then(|| wt.id.clone()));
        let _ = self.request_tx.send(Request::SwitchWorktree {
            root: wt.path.clone(),
        });
        self.close_picker();
        match &saved {
            Some(state) => {
                self.mode = state.mode.clone();
                // Restore the worktree's own remembered origin so an
                // `Uncommitted` mode round-trips to it instead of to the
                // origin of the worktree switched away from.
                self.last_origin = state.last_origin.clone();
            }
            None => {
                // Never visited: present the startup view rather than
                // carrying the previous worktree's mode/selection across.
                self.mode = Mode::Branch(Origin::Default);
                self.last_origin = None;
                self.selection = 0;
                self.selected_path = None;
                self.expanded_dirs.clear();
            }
        }
        // Both branches drive the worker to the mode's revision, exactly as
        // if the user had re-picked it (`apply_base_switch` sends SetRev).
        self.apply_base_switch();
        // The gate stays set until the switch's BUSY placeholder arrives, so
        // stale old-root snapshots queued ahead of it (channel FIFO) can
        // never consume the pending restore in `apply_snapshot`.
        self.awaiting_switch = true;
        self.pending_restore = saved;
    }

    /// Snapshot the current view state for the per-worktree memory.
    fn capture_ui_state(&self) -> WorktreeUiState {
        WorktreeUiState {
            mode: self.mode.clone(),
            last_origin: self.last_origin.clone(),
            selected_path: self.selected_path.clone(),
            expanded_dirs: self.expanded_dirs.clone(),
            tree_scroll: self.tree_scroll,
            diff_scroll: self.diff_scroll,
            diff_col: self.diff_col,
        }
    }

    /// Move the picker selection one row within the filtered subset, skipping
    /// `missing` worktree rows (they render dimmed and are unselectable).
    /// Clamped at both ends of the filtered subset.
    pub fn picker_step(&mut self, down: bool) {
        let Some(picker) = self.base_picker.as_mut() else {
            return;
        };
        let rows: Vec<usize> = picker
            .filtered_indices()
            .into_iter()
            .filter(|&i| picker.selectable(i))
            .collect();
        if rows.is_empty() {
            return;
        }
        let pos = rows
            .iter()
            .position(|&r| r == picker.selection)
            .unwrap_or(0);
        let next = if down { pos + 1 } else { pos.saturating_sub(1) };
        if next < rows.len() {
            picker.selection = rows[next];
        }
    }

    /// Append a typed character to the picker filter and re-anchor the
    /// selection onto the filtered subset.
    pub fn picker_push_filter(&mut self, c: char) {
        if let Some(picker) = self.base_picker.as_mut() {
            picker.filter.push(c);
            Self::reanchor_picker_selection(picker);
        }
    }

    /// Drop the picker filter's last character (Backspace).
    pub fn picker_filter_backspace(&mut self) {
        if let Some(picker) = self.base_picker.as_mut() {
            picker.filter.pop();
            Self::reanchor_picker_selection(picker);
        }
    }

    /// Clear the picker filter entirely (the first of two Esc presses).
    pub fn picker_clear_filter(&mut self) {
        if let Some(picker) = self.base_picker.as_mut() {
            picker.filter.clear();
            Self::reanchor_picker_selection(picker);
        }
    }

    /// Re-anchor the picker selection after a filter or order change: keep it
    /// when it still points at a selectable row of the filtered subset,
    /// otherwise jump to the first selectable row. When nothing matches, the
    /// selection is left alone and Enter no-ops.
    fn reanchor_picker_selection(picker: &mut BasePicker) {
        let selectable: Vec<usize> = picker
            .filtered_indices()
            .into_iter()
            .filter(|&i| picker.selectable(i))
            .collect();
        if selectable.is_empty() || selectable.contains(&picker.selection) {
            return;
        }
        picker.selection = selectable[0];
    }

    /// Drain the background dirty-count scan into the picker's chip map.
    /// Called once per loop tick; returns `true` when any count landed (the
    /// picker rows changed and a redraw is warranted). Arriving counts also
    /// re-sort the open worktree picker into activity order, with the
    /// selection following its worktree id so rows don't shift under the
    /// user.
    pub fn poll_dirty_counts(&mut self) -> bool {
        let Some(rx) = self.dirty_rx.clone() else {
            return false;
        };
        let mut changed = false;
        while let Ok((id, stats)) = rx.try_recv() {
            self.dirty_counts.insert(id, stats.files);
            changed = true;
        }
        if changed
            && self
                .base_picker
                .as_ref()
                .is_some_and(|p| p.kind == PickerKind::Worktree)
        {
            let mut picker = self.base_picker.take().unwrap();
            let selected_id = picker.worktrees.get(picker.selection).map(|w| w.id.clone());
            picker
                .worktrees
                .sort_by_key(|w| worktree_sort_key(w, self.dirty_counts.get(&w.id).copied()));
            picker.entries = picker
                .worktrees
                .iter()
                .map(|w| PickerEntry {
                    rev: w.id.clone(),
                    label: w.id.clone(),
                })
                .collect();
            if let Some(id) = selected_id {
                if let Some(pos) = picker.worktrees.iter().position(|w| w.id == id) {
                    picker.selection = pos;
                }
            }
            Self::reanchor_picker_selection(&mut picker);
            self.base_picker = Some(picker);
        }
        changed
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

    // ── Mouse ───────────────────────────────────────────────────────────

    /// Resolve a screen `row` in the tree list to its flat index, or `None`
    /// when no list geometry was recorded, the row is outside the list's
    /// vertical span, or it holds no entry.
    fn tree_row_index(&self, row: u16) -> Option<usize> {
        let rect = self.mouse.tree_list?;
        if row < rect.y || row >= rect.y.saturating_add(rect.height) {
            return None;
        }
        let idx = self.tree_scroll + (row - rect.y) as usize;
        self.flat.get(idx)?;
        Some(idx)
    }

    /// A left-click inside the tree list at screen `row`. Selects the entry on
    /// that row (loading its diff) and focuses the tree pane; a directory row
    /// also toggles its expansion in the same click. Returns `true` when the
    /// row held an entry.
    pub fn click_tree_row(&mut self, row: u16) -> bool {
        let Some(idx) = self.tree_row_index(row) else {
            return false;
        };
        let node_idx = self.flat[idx];
        self.focus = Focus::Tree;
        let is_dir = self.tree.nodes[node_idx].kind == tree::Kind::Dir;
        // Select the entry (loading its diff); a directory click also toggles
        // its expansion in the same click — one click opens AND shows, like a
        // file manager's double-click.
        if idx != self.selection {
            self.selection = idx;
            self.select_and_request();
        }
        if is_dir {
            self.toggle_dir();
        }
        true
    }

    /// Right-click on a tree row: copies that entry's repo-relative path to the
    /// clipboard (OSC52) and flashes a confirmation (or a failure). `false`
    /// when the row holds no entry. Directories copy the directory path.
    pub fn copy_tree_path(&mut self, row: u16) -> bool {
        let Some(idx) = self.tree_row_index(row) else {
            return false;
        };
        let path = self.tree.nodes[self.flat[idx]].path.clone();
        match copy_osc52(&path) {
            Ok(_) => self.flash = Some((format!("copied {path}"), Instant::now())),
            Err(_) => self.flash = Some(("copy failed".to_string(), Instant::now())),
        }
        true
    }

    /// Mouse-down on a diff body line: starts a selection anchor there and
    /// focuses the diff pane. `false` when the row maps to a header or nothing.
    pub fn begin_diff_selection(&mut self, row: u16) -> bool {
        let Some(line) = self.line_at_screen_row(row) else {
            return false;
        };
        let Some(diff) = self.diff() else {
            return false;
        };
        if diff.lines[line].kind.is_header() {
            return false;
        }
        self.diff_selection.anchor = Some(line);
        self.diff_selection.range = Some((line, line));
        self.diff_selection.dragged = false;
        self.focus = Focus::Diff;
        true
    }

    /// Drag over a diff body line: extends the selection from the anchor to the
    /// row's line (unchanged when the row maps to a header/off-screen). `true`
    /// when the selected range changed. Any drag marks the selection dragged,
    /// even when the row maps back to an endpoint.
    pub fn extend_diff_selection(&mut self, row: u16) -> bool {
        let Some(anchor) = self.diff_selection.anchor else {
            return false;
        };
        self.diff_selection.dragged = true;
        let Some(line) = self.line_at_screen_row(row) else {
            return false;
        };
        let Some(diff) = self.diff() else {
            return false;
        };
        if diff.lines[line].kind.is_header() {
            return false;
        }
        let range = (anchor.min(line), anchor.max(line));
        if self.diff_selection.range == Some(range) {
            return false;
        }
        self.diff_selection.range = Some(range);
        true
    }

    /// Cancel an in-progress drag without copying (e.g. the base picker
    /// opened mid-drag and consumed the release).
    pub fn cancel_diff_selection(&mut self) {
        self.diff_selection = DiffSelection::default();
    }

    /// Mouse-up: ends the drag. A dragged selection is copied to the clipboard
    /// (OSC52) and stays highlighted; a click without dragging clears it.
    pub fn finish_diff_selection(&mut self) {
        if !self.diff_selection.dragged {
            self.diff_selection = DiffSelection::default();
            return;
        }
        if let Some(text) = self.selection_text() {
            let lines = text.lines().count();
            match copy_osc52(&text) {
                Ok(truncated) => {
                    let suffix = if truncated { " (truncated)" } else { "" };
                    self.flash = Some((format!("copied {lines} lines{suffix}"), Instant::now()));
                }
                Err(_) => self.flash = Some(("copy failed".to_string(), Instant::now())),
            }
        }
        self.diff_selection.anchor = None;
        self.diff_selection.dragged = false;
    }

    /// The active selection range intersected with the live diff: `None` when
    /// no selection is active, the diff is empty/gone, or the range no longer
    /// overlaps it (e.g. the diff shrank after a swap).
    fn clamped_selection_range(&self) -> Option<(usize, usize)> {
        let (min, max) = self.diff_selection.range?;
        let diff = self.diff()?;
        let max = max.min(diff.lines.len().checked_sub(1)?);
        (min <= max).then_some((min, max))
    }

    /// The selected lines' content, joined by '\n' with a trailing newline;
    /// `None` when no selection is active or it holds no body lines.
    /// `raw_text` carries no diff markers (gix consumes them), so lines join
    /// as-is; section-header rows are never highlighted, so they are skipped.
    pub fn selection_text(&self) -> Option<String> {
        let (min, max) = self.clamped_selection_range()?;
        let diff = self.diff()?;
        let mut out = String::new();
        for line in diff.lines[min..=max].iter().filter(|l| !l.kind.is_header()) {
            out.push_str(&line.raw_text);
            out.push('\n');
        }
        (!out.is_empty()).then_some(out)
    }

    /// Footer click at screen `column`. `right` selects the source pill's
    /// right-click action (commit/tag picker) instead of its left-click one.
    /// Returns `true` when a pill zone was hit.
    pub fn click_footer(&mut self, column: u16, right: bool) -> bool {
        let Some(&(zone, _)) = self
            .mouse
            .footer_zones
            .iter()
            .find(|(_, r)| column >= r.x && column < r.x.saturating_add(r.width))
        else {
            return false;
        };
        match zone {
            FooterZone::Source if right => self.open_commit_picker(),
            FooterZone::Source => self.open_branch_picker(),
            FooterZone::Mode => self.toggle_mode(),
            FooterZone::Worktree => self.open_worktree_picker(),
        }
        true
    }

    /// Base-picker click at screen `column, row` — every click while the
    /// picker is open is consumed: inside the entry list selects AND confirms
    /// that entry; on the border/title it does nothing; outside the popup it
    /// closes the picker. Returns `true` when consumed.
    pub fn click_picker(&mut self, column: u16, row: u16) -> bool {
        if self.base_picker.is_none() {
            return false;
        }
        let Some(g) = self.mouse.picker else {
            // No recorded popup geometry (tiny terminal or not yet drawn):
            // dismiss rather than swallow the click forever.
            self.close_picker();
            return true;
        };
        let in_rect = |r: Rect| {
            column >= r.x
                && column < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        };
        if in_rect(g.inner) {
            let offset = (row - g.inner.y) as usize;
            // Row 0 of the inner area is the filter box: a click there is
            // consumed but inert. Entry rows are offset by one below it, and
            // `first_row` is their first rendered real index — map the click
            // through the filtered subset.
            let picked = if offset == 0 {
                None
            } else {
                self.base_picker.as_ref().and_then(|p| {
                    let visible = p.filtered_indices();
                    let start = visible.iter().position(|&i| i == g.first_row)?;
                    visible.get(start + offset - 1).copied()
                })
            };
            if let Some(entry) = picked {
                if let Some(picker) = self.base_picker.as_mut() {
                    picker.selection = entry;
                }
                self.select_picked_base();
            }
        } else if !in_rect(g.popup) {
            self.close_picker();
        }
        true
    }

    /// Wheel inside the base picker: move the selection one entry (skipping
    /// unselectable worktree rows).
    pub fn picker_scroll(&mut self, up: bool) {
        self.picker_step(!up);
    }

    // ── Convenience accessors for snapshot data ─────────────────────────

    pub fn diff(&self) -> Option<&Diff> {
        self.snapshot.selected_diff.as_deref()
    }

    pub fn highlighted(&self) -> &[Option<Vec<TokenOwned>>] {
        self.snapshot.selected_highlight.as_slice()
    }

    pub fn current_file(&self) -> Option<&str> {
        self.selected_path.as_deref()
    }

    pub fn diff_error(&self) -> Option<&str> {
        self.snapshot.error.as_deref()
    }

    /// Transient picker-open failure message for the status line.
    pub fn picker_error(&self) -> Option<&str> {
        self.picker_error.as_deref()
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
        self.diff_selection = DiffSelection::default();
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

    /// Move the tree selection down one viewport, keeping the selected row
    /// visible (the tree's scroll follows the selection).
    pub fn tree_page_down(&mut self) {
        self.move_down(self.tree_page_size());
        self.reveal_tree_selection();
    }

    /// Move the tree selection up one viewport, keeping the selected row visible.
    pub fn tree_page_up(&mut self) {
        self.move_up(self.tree_page_size());
        self.reveal_tree_selection();
    }

    /// Jump the tree selection to the first row.
    pub fn tree_first(&mut self) {
        if self.selection != 0 {
            self.selection = 0;
            self.select_and_request();
        }
        self.reveal_tree_selection();
    }

    /// Jump the tree selection to the last row.
    pub fn tree_last(&mut self) {
        let Some(last) = self.flat.len().checked_sub(1) else {
            return;
        };
        if self.selection != last {
            self.selection = last;
            self.select_and_request();
        }
        self.reveal_tree_selection();
    }

    /// Visible rows in the tree list (the pane minus its one-row header).
    fn tree_page_size(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }

    /// Scroll the tree so the selected row stays inside the visible window
    /// (mirrors the clamp the tree pane applies during layout).
    fn reveal_tree_selection(&mut self) {
        let visible = self.tree_page_size();
        if self.selection < self.tree_scroll {
            self.tree_scroll = self.selection;
        } else if self.selection >= self.tree_scroll.saturating_add(visible) {
            self.tree_scroll = self.selection + 1 - visible;
        }
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
        // Mirror the render path: the pinned headers are the capped sticky pair,
        // so a file+hunk pin can both count as "in view".
        let headers = self.header_positions();
        let pinned =
            headers.sticky_pair_capped(start, self.diff_page_size().min(u16::MAX as usize) as u16);
        let sticky_hit = [pinned.0, pinned.1].into_iter().flatten().any(|p| {
            visible
                .get(p)
                .is_some_and(|&li| diff.lines[li].section == section)
        });
        if sticky_hit {
            return true;
        }
        visible[start..end]
            .iter()
            .any(|&li| diff.lines[li].kind.is_header() && diff.lines[li].section == section)
    }

    /// `diff.lines` index of the top rendered body line: the nearest scrolled-off
    /// header (the bottom sticky) if present, else the first in-flow line.
    fn top_body_line(&self) -> Option<usize> {
        let visible = self.visible_line_indices();
        let headers = self.header_positions();
        let start = self.diff_scroll;
        if start > 0 {
            if let Some(pos) = headers.any_before(start) {
                return Some(visible[pos]);
            }
        }
        visible.get(start).copied()
    }

    /// Number of pinned sticky rows at the current scroll position, mirroring
    /// `view::diff::sticky_headers`: at most one file header, plus one hunk
    /// header only when it falls after the pinned file header.
    pub(crate) fn sticky_row_count(&self) -> usize {
        if self.diff().is_none() {
            return 0;
        }
        let visible = self.visible_line_indices();
        let headers = self.header_positions();
        let start = self.diff_scroll.min(visible.len());
        if start == 0 {
            return 0;
        }
        let body_height = self.diff_page_size().min(u16::MAX as usize) as u16;
        let (file_pos, hunk_pos) = headers.sticky_pair_capped(start, body_height);
        usize::from(file_pos.is_some()) + usize::from(hunk_pos.is_some())
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
        self.diff_col = self.diff_col.saturating_add(n).min(self.max_diff_col());
    }

    /// Re-clamp `diff_col` against the current max. Call when the pane width
    /// changes (a resize otherwise leaves the horizontal scroll past the new
    /// maximum until the next `h`/`l`).
    pub fn clamp_diff_col(&mut self) {
        self.diff_col = self.diff_col.min(self.max_diff_col());
    }

    /// Largest valid horizontal scroll for the current diff and pane width.
    fn max_diff_col(&self) -> usize {
        self.max_visible_line_width()
            .saturating_sub(self.diff_width as usize)
            .min(MAX_HSCROLL)
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
    /// Memoized on content identity: the diff `Arc` (for `max_width` and line
    /// visibility) and the `collapsed_hunks` set. The caller gets a cheap `Rc`
    /// clone; the O(n) scan happens only on cache misses, and a highlight-only
    /// snapshot (which shares the diff) reuses everything.
    pub fn visible_line_indices(&self) -> Rc<Vec<usize>> {
        let mut cache = self.visible_cache.borrow_mut();
        let diff = &self.snapshot.selected_diff;
        let diff_same = match (&cache.diff, diff) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        let collapsed_same = cache.collapsed == self.collapsed_hunks;
        if diff_same && collapsed_same {
            return Rc::clone(&cache.lines);
        }

        // `max_width` depends only on the diff, and the header positions only
        // on the diff + fold set.
        if !diff_same {
            let mut span = perf::span("max_width");
            cache.max_width = self.compute_max_width();
            let lines = self
                .snapshot
                .selected_diff
                .as_ref()
                .map_or(0, |d| d.lines.len());
            span.detail(format_args!("lines={lines}"));
        }
        let mut span = perf::span("visible_lines");
        let lines = self.compute_visible_lines();
        span.detail(format_args!("flat={}", lines.len()));
        if !diff_same || !collapsed_same {
            cache.headers = Rc::new(self.compute_headers(&lines));
        }
        cache.lines = Rc::new(lines);
        cache.diff = diff.clone();
        cache.collapsed = self.collapsed_hunks.clone();
        Rc::clone(&cache.lines)
    }

    /// Header-row positions for the current visible lines, keeping the memoized
    /// cache fresh. Cheap: the value is rebuilt only when the lines are.
    pub(crate) fn header_positions(&self) -> Rc<HeaderPositions> {
        self.visible_line_indices();
        Rc::clone(&self.visible_cache.borrow().headers)
    }

    /// Sorted visible-list positions of the diff's file and hunk header rows.
    fn compute_headers(&self, visible: &[usize]) -> HeaderPositions {
        let Some(diff) = self.diff() else {
            return HeaderPositions::default();
        };
        let mut headers = HeaderPositions::default();
        for (pos, &line_idx) in visible.iter().enumerate() {
            match diff.lines[line_idx].kind {
                LineKind::FileHeader => headers.file.push(pos),
                LineKind::HunkHeader => headers.hunk.push(pos),
                _ => {}
            }
        }
        headers
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
        if self.diff().is_none() {
            return;
        }
        self.scroll_to_section_header(section);
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
        let line_idx = self.line_at_screen_row(row)?;
        (diff.lines[line_idx].kind.is_header()).then_some(diff.lines[line_idx].section)
    }

    /// The diff line at a diff-pane screen row, if it maps to a line at all.
    /// Covers sticky header rows and the visible flow rows; returns `None` for
    /// the locked header, off-screen rows, and when no diff is loaded.
    pub fn line_at_screen_row(&self, row: u16) -> Option<usize> {
        let diff = self.diff()?;
        let header_h = self.last_header_height as usize;
        if (row as usize) < header_h {
            return None;
        }
        let body_row = (row as usize) - header_h;
        let file = self.sticky_file;
        let hunk = self.sticky_hunk;
        let sticky_rows = usize::from(file.is_some()) + usize::from(hunk.is_some());

        // A row on a pinned sticky row maps to that header's line.
        if body_row < sticky_rows {
            let idx = if body_row == 0 { file.or(hunk) } else { hunk };
            return idx.filter(|&i| i < diff.lines.len());
        }

        self.visible_line_indices()
            .get(self.diff_scroll + body_row - sticky_rows)
            .copied()
            .filter(|&i| i < diff.lines.len())
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
        // The reflowed diff invalidates the old selection's line indices.
        self.diff_selection = DiffSelection::default();
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
            (n.kind == tree::Kind::File && node_parent == parent).then(|| (n.name.clone(), pos))
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

/// Copy `text` to the primary selection (the middle-click buffer) via the
/// OSC52 escape sequence (works over SSH; kitty/wezterm/xterm honor the `p`
/// selection). We use OSC52 rather than a clipboard crate so the copy reaches
/// the local terminal from a remote host. Truncates very large payloads
/// rather than overflowing terminal limits.
///
/// Returns `Ok(true)` when the payload was truncated at the 16 KiB cap,
/// `Ok(false)` on a full copy, and `Err` when the write to the terminal failed.
fn copy_osc52(text: &str) -> std::io::Result<bool> {
    const MAX: usize = 16 * 1024;
    let mut end = text.len().min(MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = end < text.len();
    let payload = STANDARD.encode(&text.as_bytes()[..end]);
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;p;{payload}\x07").and_then(|()| out.flush())?;
    Ok(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Diff, DiffLine, FileHeader, FileStatus, HunkMeta, LineKind};
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
    fn highlight_suppressed_while_busy_then_fires_on_settle() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut snap = snapshot_with_diff(sample_diff(20));
        Arc::get_mut(&mut snap).unwrap().busy = true;
        let mut app = App::new(
            snap,
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial `Request::Diff` from `App::new`

        // A busy (still-streaming) snapshot must not fire highlight requests.
        app.maybe_request_highlight();
        assert!(
            rx.try_recv().is_err(),
            "no highlight request while the worker is busy"
        );

        // Once the settled snapshot lands, the normal path fires once.
        Arc::get_mut(&mut app.snapshot).unwrap().busy = false;
        app.maybe_request_highlight();
        match rx.try_recv().unwrap() {
            Request::Highlight { path, .. } => assert_eq!(path, "x.rs"),
            other => panic!("expected Highlight, got {other:?}"),
        }
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
                lfs: None,
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
                lfs: None,
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
                lfs: None,
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
        let mut app = app_with_diff(fitting_hunk());
        app.active_chunk = Some(1);

        // The cursor operation must act on the active hunk, not its neighbours.
        app.toggle_hunk_at_cursor();
        assert!(app.collapsed_hunks.contains(&1));
        assert!(!app.collapsed_hunks.contains(&0));
        assert!(!app.collapsed_hunks.contains(&2));
        assert_eq!(app.active_chunk, Some(1));

        app.toggle_hunk_at_cursor();
        assert!(!app.collapsed_hunks.contains(&1));
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
        let initial = snapshot_with_diff(diff);
        let shared_diff = initial.selected_diff.clone().unwrap();
        let shared_tree = initial.tree.clone();
        app.snapshot = initial;
        app.height = 8;
        app.last_header_height = 1;
        app.diff_scroll = 7;

        // A highlight-only snapshot shares the same diff/tree Arcs (pointer
        // equal), so applying it must preserve the scroll position rather than
        // snapping to the top of the sticky section.
        app.apply_snapshot(Arc::new(crate::snapshot::Snapshot {
            seq: 1,
            busy: false,
            empty: false,
            tree: shared_tree,
            selected_diff: Some(shared_diff),
            selected_highlight: Arc::new(vec![None; 31]),
            error: None,
            branch_state: crate::git::BranchState::default(),
            uncommitted_stats: None,
        }));

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
            lfs: None,
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
            lfs: None,
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

    /// A snapshot sharing `app`'s diff/tree Arcs but with a fresh highlight Arc.
    fn highlight_only_snapshot(app: &App, highlight_len: usize) -> ArcSnapshot {
        Arc::new(crate::snapshot::Snapshot {
            seq: 1,
            busy: false,
            empty: false,
            tree: app.snapshot.tree.clone(),
            selected_diff: app.snapshot.selected_diff.clone(),
            selected_highlight: Arc::new(vec![None; highlight_len]),
            error: None,
            branch_state: crate::git::BranchState::default(),
            uncommitted_stats: None,
        })
    }

    #[test]
    fn highlight_only_snapshot_reuses_lines_and_max_width() {
        let mut app = app_with_diff(sample_diff(4));
        let first = app.visible_line_indices();
        // A retained (non-recomputed) max_width must survive the swap.
        app.visible_cache.borrow_mut().max_width = usize::MAX - 1;

        app.apply_snapshot(highlight_only_snapshot(&app, 4));

        let second = app.visible_line_indices();
        assert!(
            Rc::ptr_eq(&first, &second),
            "highlight-only snapshot must not recompute visible lines"
        );
        assert_eq!(
            app.visible_cache.borrow().max_width,
            usize::MAX - 1,
            "max_width must survive a highlight-only snapshot"
        );
    }

    #[test]
    fn diff_recompute_rearms_highlight_for_unchanged_range() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            snapshot_with_diff(sample_diff(20)),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial `Request::Diff` from `App::new`
        app.height = 10;
        app.last_header_height = 1;

        // Seed the viewport highlight; the raw range is remembered.
        app.maybe_request_highlight();
        match rx.try_recv().unwrap() {
            Request::Highlight { .. } => {}
            other => panic!("expected Highlight, got {other:?}"),
        }

        // A watcher recompute delivers a fresh `Diff` Arc over the same visible
        // raw range. Re-arming must let the nudge fire again, or the fresh diff
        // renders bare.
        let mut snap = snapshot_with_diff(sample_diff(20));
        Arc::get_mut(&mut snap).unwrap().tree = app.snapshot.tree.clone();
        app.apply_snapshot(snap);

        app.maybe_request_highlight();
        match rx.try_recv().unwrap() {
            Request::Highlight { path, .. } => assert_eq!(path, "x.rs"),
            other => panic!("expected Highlight after recompute, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_diff_and_highlight_reuse_cache() {
        let mut app = app_with_diff(sample_diff(4));
        let first = app.visible_line_indices();
        app.visible_cache.borrow_mut().max_width = usize::MAX - 1;

        // Same diff Arc and same highlight Arc: nothing is recomputed.
        app.apply_snapshot(Arc::new(crate::snapshot::Snapshot {
            seq: 1,
            busy: false,
            empty: false,
            tree: app.snapshot.tree.clone(),
            selected_diff: app.snapshot.selected_diff.clone(),
            selected_highlight: app.snapshot.selected_highlight.clone(),
            error: None,
            branch_state: crate::git::BranchState::default(),
            uncommitted_stats: None,
        }));

        let second = app.visible_line_indices();
        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(app.visible_cache.borrow().max_width, usize::MAX - 1);
    }

    /// An `App` with `n` flat tree rows and a known viewport height.
    fn nav_app(n: usize) -> App {
        let entries: Vec<(char, String)> = (0..n).map(|i| ('M', format!("f{i:02}.rs"))).collect();
        let (tx, _rx) = crossbeam_channel::unbounded::<crate::snapshot::Request>();
        let mut app = App::new(
            sample_snapshot(Tree::build(entries)),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.height = 6; // tree page = 5 visible rows
        app
    }

    #[test]
    fn tree_page_keys_move_and_reveal() {
        let mut app = nav_app(20);
        assert_eq!(app.selection, 0);
        assert_eq!(app.tree_scroll, 0);

        app.tree_page_down();
        assert_eq!(app.selection, 5);
        assert_eq!(app.tree_scroll, 1, "scroll follows the selection");

        app.tree_page_up();
        assert_eq!(app.selection, 0);
        assert_eq!(app.tree_scroll, 0);
    }

    #[test]
    fn tree_page_keys_clamp_at_ends() {
        let mut app = nav_app(7);
        app.tree_page_down();
        assert_eq!(app.selection, 5);
        app.tree_page_down();
        assert_eq!(app.selection, 6, "clamped at the last row");

        app.tree_first();
        assert_eq!(app.selection, 0);
        assert_eq!(app.tree_scroll, 0);

        app.tree_last();
        assert_eq!(app.selection, 6);
        assert_eq!(app.tree_scroll, 2, "last row revealed: 6 + 1 - 5");
        app.tree_last();
        assert_eq!(app.selection, 6);
    }

    #[test]
    fn click_footer_mode_zone_cycles_and_sends_setrev() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.mouse.footer_zones = vec![(FooterZone::Mode, Rect::new(10, 0, 8, 1))];
        assert_eq!(app.mode, Mode::Branch(Origin::Default));

        assert!(app.click_footer(12, false));
        assert_eq!(app.mode, Mode::Uncommitted);
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn click_footer_source_left_opens_branch_picker() {
        // Real opener: reads the global repo root (see t_opens_commit_picker).
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (tx, _rx) = crossbeam_channel::bounded::<Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.mouse.footer_zones = vec![(FooterZone::Source, Rect::new(0, 0, 8, 1))];
        assert!(app.click_footer(3, false));
        assert_eq!(
            app.base_picker.as_ref().map(|p| p.title.as_str()),
            Some(" compare against a branch ")
        );
    }

    #[test]
    fn click_footer_source_right_opens_commit_picker() {
        // Real opener: reads the global repo root (see t_opens_commit_picker).
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (tx, _rx) = crossbeam_channel::bounded::<Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.mouse.footer_zones = vec![(FooterZone::Source, Rect::new(0, 0, 8, 1))];
        assert!(app.click_footer(3, true));
        assert_eq!(
            app.base_picker.as_ref().map(|p| p.title.as_str()),
            Some(" compare against a commit or tag ")
        );
    }

    #[test]
    fn click_footer_miss_returns_false() {
        let mut app = sample_app();
        app.mouse.footer_zones = vec![(FooterZone::Mode, Rect::new(10, 0, 8, 1))];
        assert!(!app.click_footer(100, false));
        assert!(!app.click_footer(9, false), "just before the zone");
        assert_eq!(app.mode, Mode::Branch(Origin::Default));
    }

    #[test]
    fn click_picker_row_selects_confirms_and_closes() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
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
            ],
        );
        app.mouse.picker = Some(PickerGeometry {
            popup: Rect::new(0, 0, 20, 6),
            inner: Rect::new(1, 1, 18, 4),
            first_row: 0,
        });

        // inner.y = 1: row 2 is the filter row, row 3 is entry 1 (`feature`).
        assert!(app.click_picker(3, 3));
        assert!(app.base_picker.is_none());
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "feature...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn click_picker_outside_closes_without_request() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.open_picker_with(
            " t ",
            vec![PickerEntry {
                rev: "main".to_string(),
                label: "main".to_string(),
            }],
        );
        app.mouse.picker = Some(PickerGeometry {
            popup: Rect::new(5, 5, 10, 4),
            inner: Rect::new(6, 6, 8, 2),
            first_row: 0,
        });

        assert!(app.click_picker(0, 0));
        assert!(app.base_picker.is_none());
        assert!(rx.try_recv().is_err(), "outside click sends no request");
    }

    #[test]
    fn click_picker_border_is_consumed_but_inert() {
        let mut app = sample_app();
        app.open_picker_with(
            " t ",
            vec![PickerEntry {
                rev: "main".to_string(),
                label: "main".to_string(),
            }],
        );
        app.mouse.picker = Some(PickerGeometry {
            popup: Rect::new(5, 5, 10, 4),
            inner: Rect::new(6, 6, 8, 2),
            first_row: 0,
        });

        // Popup corner sits on the border, outside `inner`.
        assert!(app.click_picker(5, 5));
        assert!(app.base_picker.is_some());
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);
    }

    #[test]
    fn picker_scroll_moves_and_clamps() {
        let mut app = sample_app();
        app.open_picker_with(
            " t ",
            vec![
                PickerEntry {
                    rev: "a".to_string(),
                    label: "a".to_string(),
                },
                PickerEntry {
                    rev: "b".to_string(),
                    label: "b".to_string(),
                },
            ],
        );
        app.picker_scroll(false);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1);
        app.picker_scroll(false);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1, "last");
        app.picker_scroll(true);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);
        app.picker_scroll(true);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0, "first");

        // No picker open: a wheel is a no-op and must not re-open one.
        app.base_picker = None;
        app.picker_scroll(false);
        assert!(app.base_picker.is_none());
    }

    #[test]
    fn click_tree_row_selects_and_expands_directory_in_one_click() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![
                ('M', "src/a.rs".to_string()),
                ('M', "b.rs".to_string()),
            ])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial Diff for the selected dir path
        app.mouse.tree_list = Some(Rect::new(0, 1, 20, 10));
        // Select the file first (row 2 → b.rs) so the dir row is not selected.
        assert!(app.click_tree_row(2));
        let _ = rx.try_recv(); // b.rs diff request
        let dir_idx = app.flat[0];
        assert_eq!(app.tree.nodes[dir_idx].kind, tree::Kind::Dir);
        assert!(!app.tree.nodes[dir_idx].expanded);

        // One click on the dir row (row 1 → flat[0]): selects it, loads its
        // aggregate diff, and expands it.
        assert!(app.click_tree_row(1));
        assert_eq!(app.selection, 0);
        assert!(
            app.tree.nodes[dir_idx].expanded,
            "one click expands the dir"
        );
        assert!(app.expanded_dirs.contains("src"));
        match rx.try_recv().unwrap() {
            Request::Diff { path, .. } => assert_eq!(path, "src"),
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn click_tree_row_selects_file_and_requests_diff() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![
                ('M', "a.rs".to_string()),
                ('M', "b.rs".to_string()),
            ])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial Diff for a.rs
        app.mouse.tree_list = Some(Rect::new(0, 1, 20, 10));

        // row 2 → idx = tree_scroll(0) + (2 - 1) = 1 → b.rs.
        assert!(app.click_tree_row(2));
        assert_eq!(app.selection, 1);
        assert_eq!(app.selected_path.as_deref(), Some("b.rs"));
        assert_eq!(app.focus, Focus::Tree);
        match rx.try_recv().unwrap() {
            Request::Diff { path, .. } => assert_eq!(path, "b.rs"),
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn click_tree_row_toggles_selected_directory() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![('M', "src/a.rs".to_string())])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial Diff for the selected dir path
        app.mouse.tree_list = Some(Rect::new(0, 1, 20, 10));
        let dir_idx = app.flat[app.selection];
        assert_eq!(app.tree.nodes[dir_idx].kind, tree::Kind::Dir);
        assert!(!app.tree.nodes[dir_idx].expanded);

        assert!(app.click_tree_row(1)); // idx == selection → toggle
        assert!(app.tree.nodes[dir_idx].expanded);
        assert!(app.expanded_dirs.contains("src"));
        assert_eq!(app.selection, 0, "selection stays on the toggled dir");
        assert!(
            rx.try_recv().is_err(),
            "toggling a dir sends no diff request"
        );
    }

    #[test]
    fn click_tree_row_beyond_last_entry_returns_false() {
        let mut app = sample_app();
        app.mouse.tree_list = Some(Rect::new(0, 1, 20, 10));

        // Two entries (a.rs, b.rs): row 3 → idx 2, past the end.
        assert!(!app.click_tree_row(3));
        assert_eq!(app.selection, 0);
        // A row above the list is not an entry either.
        assert!(!app.click_tree_row(0));
        // Without recorded geometry the click is never routed.
        app.mouse.tree_list = None;
        assert!(!app.click_tree_row(1));
    }

    /// An `App` whose selected diff is `lines` (hunk 0), with the row-mapping
    /// fields pre-set so `line_at_screen_row(1)` is the hunk header and row
    /// `1 + n` is `lines[n]` (no sticky rows, no scroll).
    fn selectable_app(lines: Vec<DiffLine>) -> App {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(header_full(None, None), lines));
        app.focus = Focus::Diff;
        app.last_header_height = 1;
        app.sticky_file = None;
        app.sticky_hunk = None;
        app.diff_scroll = 0;
        app
    }

    /// A body line with the given kind and raw text, in hunk 0.
    fn marked(kind: LineKind, raw: &str) -> DiffLine {
        DiffLine {
            kind,
            raw_text: raw.into(),
            section: 0,
            word_runs: None,
        }
    }

    #[test]
    fn diff_selection_orders_range_up_and_down() {
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Added, "one"),
            marked(LineKind::Context, "two"),
            marked(LineKind::Removed, "three"),
        ]);

        // Dragging downward: anchor 1, extend to 3.
        assert!(app.begin_diff_selection(2));
        assert_eq!(app.diff_selection.anchor, Some(1));
        assert!(app.extend_diff_selection(4));
        assert_eq!(app.diff_selection.range, Some((1, 3)));

        // Dragging upward from a later anchor: anchor 2, extend to 1.
        assert!(app.begin_diff_selection(3));
        assert!(app.extend_diff_selection(2));
        assert_eq!(app.diff_selection.range, Some((1, 2)));
    }

    #[test]
    fn begin_diff_selection_rejects_header_row() {
        let mut app = selectable_app(vec![hunk_header(0), marked(LineKind::Context, "x")]);
        assert!(
            !app.begin_diff_selection(1),
            "hunk header is not selectable"
        );
        assert!(app.diff_selection.range.is_none());
    }

    #[test]
    fn extend_over_header_keeps_range() {
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Context, "one"),
            hunk_header(1),
            marked(LineKind::Context, "two"),
        ]);
        assert!(app.begin_diff_selection(2));
        assert_eq!(app.diff_selection.range, Some((1, 1)));

        // Row 1 is the hunk header and row 100 is off-screen: both unchanged.
        assert!(!app.extend_diff_selection(1));
        assert_eq!(app.diff_selection.range, Some((1, 1)));
        assert!(!app.extend_diff_selection(100));
        assert_eq!(app.diff_selection.range, Some((1, 1)));
        assert!(app.diff_selection.dragged, "any drag marks it dragged");
    }

    #[test]
    fn finish_after_drag_keeps_range_and_flashes() {
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Added, "one"),
            marked(LineKind::Context, "two"),
            marked(LineKind::Removed, "three"),
        ]);
        assert!(app.begin_diff_selection(2));
        assert!(app.extend_diff_selection(4));
        app.finish_diff_selection();

        assert_eq!(app.diff_selection.range, Some((1, 3)), "highlight survives");
        assert_eq!(app.diff_selection.anchor, None);
        assert!(!app.diff_selection.dragged);
        let (msg, _) = app.flash.clone().expect("flash set");
        assert_eq!(msg, "copied 3 lines");
    }

    #[test]
    fn finish_flashes_count_of_copied_body_lines() {
        // Range spans a header and a line past the diff end; the flash must
        // count only the body lines actually copied.
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Added, "one"),
            hunk_header(1),
            marked(LineKind::Context, "two"),
        ]);
        app.diff_selection.range = Some((1, 99));
        app.diff_selection.anchor = Some(1);
        app.diff_selection.dragged = true;
        app.finish_diff_selection();
        let (msg, _) = app.flash.clone().expect("flash set");
        assert_eq!(msg, "copied 2 lines");
    }

    #[test]
    fn finish_out_of_range_selection_copies_nothing() {
        let mut app = selectable_app(vec![hunk_header(0), marked(LineKind::Context, "one")]);
        app.diff_selection.range = Some((50, 60));
        app.diff_selection.anchor = Some(50);
        app.diff_selection.dragged = true;
        app.finish_diff_selection();
        assert!(app.flash.is_none(), "no false copied flash");
    }

    #[test]
    fn finish_without_drag_clears_selection() {
        let mut app = selectable_app(vec![hunk_header(0), marked(LineKind::Context, "one")]);
        assert!(app.begin_diff_selection(2));
        app.finish_diff_selection();
        assert!(app.diff_selection.range.is_none());
        assert!(app.diff_selection.anchor.is_none());
        assert!(app.flash.is_none(), "a plain click does not copy");
    }

    #[test]
    fn apply_snapshot_content_change_clears_diff_selection() {
        let mut app = app_with_diff(sample_diff(4));
        app.diff_selection.range = Some((1, 2));
        app.diff_selection.anchor = Some(1);
        // A fresh diff Arc is a content change even when the path is unchanged.
        let mut snap = snapshot_with_diff(sample_diff(4));
        Arc::get_mut(&mut snap).unwrap().tree = app.snapshot.tree.clone();
        app.apply_snapshot(snap);
        assert!(app.diff_selection.range.is_none());
        assert!(app.diff_selection.anchor.is_none());
    }

    #[test]
    fn toggle_mode_clears_diff_selection() {
        let (tx, _rx) = crossbeam_channel::bounded::<Request>(4);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.diff_selection.range = Some((0, 2));
        app.diff_selection.anchor = Some(0);
        app.toggle_mode();
        assert!(app.diff_selection.range.is_none());
        assert!(app.diff_selection.anchor.is_none());
    }

    #[test]
    fn change_context_clears_diff_selection() {
        let mut app = sample_app();
        app.diff_selection.range = Some((0, 2));
        app.diff_selection.anchor = Some(0);
        app.change_context(1);
        assert!(app.diff_selection.range.is_none());
        assert!(app.diff_selection.anchor.is_none());
    }

    #[test]
    fn click_picker_without_geometry_closes() {
        let mut app = sample_app();
        app.open_picker_with(
            " t ",
            vec![PickerEntry {
                rev: "main".to_string(),
                label: "main".to_string(),
            }],
        );
        assert!(app.mouse.picker.is_none());
        assert!(app.click_picker(0, 0));
        assert!(
            app.base_picker.is_none(),
            "an unrenderable picker is still dismissable"
        );
    }

    #[test]
    fn selection_text_joins_raw_lines() {
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Added, "added"),
            marked(LineKind::Context, "context keeps leading chars"),
            marked(LineKind::Removed, "+ leading plus survives copy"),
        ]);
        assert!(app.begin_diff_selection(2));
        assert!(app.extend_diff_selection(4));
        assert_eq!(
            app.selection_text().as_deref(),
            Some("added\ncontext keeps leading chars\n+ leading plus survives copy\n")
        );
    }

    #[test]
    fn selection_text_skips_headers_within_range() {
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Added, "one"),
            hunk_header(1),
            marked(LineKind::Context, "two"),
        ]);
        app.diff_selection.range = Some((1, 3));
        assert_eq!(app.selection_text().as_deref(), Some("one\ntwo\n"));
    }

    #[test]
    fn selection_text_clamps_range_to_live_diff() {
        let mut app = selectable_app(vec![
            hunk_header(0),
            marked(LineKind::Context, "one"),
            marked(LineKind::Context, "two"),
        ]);
        app.diff_selection.range = Some((1, 99));
        assert_eq!(app.selection_text().as_deref(), Some("one\ntwo\n"));
    }

    #[test]
    fn selection_text_out_of_range_is_none() {
        let mut app = selectable_app(vec![hunk_header(0), marked(LineKind::Context, "one")]);
        app.diff_selection.range = Some((50, 60));
        assert!(app.selection_text().is_none());
    }

    #[test]
    fn selection_text_header_only_range_is_none() {
        let mut app = selectable_app(vec![hunk_header(0), hunk_header(1)]);
        app.diff_selection.range = Some((0, 1));
        assert!(app.selection_text().is_none());
    }

    #[test]
    fn copy_tree_path_flashes_the_relative_path() {
        let mut app = sample_app();
        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        // row 2 → idx = 0 + (2 - 1) = 1 → b.rs.
        assert!(app.copy_tree_path(2));
        let (msg, _) = app.flash.clone().expect("flash set");
        assert!(msg.contains("b.rs"), "flash names the path: {msg}");
        // Past the last entry and above the list: no copy.
        assert!(!app.copy_tree_path(3));
        assert!(!app.copy_tree_path(0));
    }

    #[test]
    fn selection_clears_when_tree_selection_changes() {
        let mut app = sample_app();
        app.diff_selection.range = Some((1, 3));
        app.diff_selection.anchor = Some(1);
        app.move_down(1);
        assert!(
            app.diff_selection.range.is_none(),
            "switching files drops the diff selection"
        );
        assert!(app.diff_selection.anchor.is_none());
    }

    /// A worktree entry for the picker tests: id, path, branch, main, missing.
    fn wtinfo(
        id: &str,
        path: &str,
        branch: Option<&str>,
        main: bool,
        missing: bool,
    ) -> WorktreeInfo {
        WorktreeInfo {
            id: id.to_string(),
            path: PathBuf::from(path),
            branch: branch.map(Into::into),
            main,
            missing,
            tip_time: None,
        }
    }

    /// `main` + one linked worktree `wt-b` at /r/b.
    fn two_worktrees() -> Vec<WorktreeInfo> {
        vec![
            wtinfo("main", "/r/main", Some("main"), true, false),
            wtinfo("wt-b", "/r/b", Some("feature"), false, false),
        ]
    }

    #[test]
    fn open_worktree_picker_with_selects_current_entry() {
        let mut app = sample_app();
        app.set_current_worktree(Some("wt-b".into()));
        app.open_worktree_picker_with(vec![
            wtinfo("main", "/r/main", Some("main"), true, false),
            wtinfo("wt-b", "/r/b", Some("feature"), false, false),
            wtinfo("gone", "/r/gone", None, false, true),
        ]);
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.kind, PickerKind::Worktree);
        assert_eq!(picker.title, " switch worktree ");
        assert_eq!(picker.selection, 1, "selection starts on the current entry");

        // An empty list is a no-op: the already-open picker stays.
        app.open_worktree_picker_with(Vec::new());
        assert!(app.base_picker.is_some(), "empty list leaves the picker");
    }

    #[test]
    fn worktree_picker_skips_missing_rows() {
        let mut app = sample_app();
        app.open_worktree_picker_with(vec![
            wtinfo("main", "/r/main", Some("main"), true, false),
            wtinfo("gone", "/r/gone", None, false, true),
            wtinfo("wt-c", "/r/c", Some("c"), false, false),
        ]);
        // Missing rows sort last: main, wt-c, gone.
        app.picker_step(true);
        assert_eq!(
            app.base_picker.as_ref().unwrap().selection,
            1,
            "steps onto wt-c"
        );
        app.picker_step(true);
        assert_eq!(
            app.base_picker.as_ref().unwrap().selection,
            1,
            "missing row is unselectable: clamped at wt-c"
        );
        app.picker_step(false);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0, "skips back");
        app.picker_step(false);
        assert_eq!(
            app.base_picker.as_ref().unwrap().selection,
            0,
            "clamped at start"
        );
    }

    #[test]
    fn worktree_enter_on_missing_flashes_without_switching() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_worktree_picker_with(two_worktrees());
        app.base_picker.as_mut().unwrap().worktrees[1].missing = true;
        app.base_picker.as_mut().unwrap().selection = 1;

        app.select_picked_base();

        let (msg, _) = app.flash.clone().expect("flash error set");
        assert_eq!(msg, "worktree directory is gone");
        assert!(
            app.base_picker.is_some(),
            "picker stays open for another pick"
        );
        assert_eq!(app.current_worktree, None);
        assert!(
            rx.try_recv().is_err(),
            "a missing worktree sends no request"
        );
    }

    #[test]
    fn worktree_enter_sends_switch_and_updates_current() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_worktree_picker_with(two_worktrees());
        app.picker_step(true);

        app.select_picked_base();

        assert!(app.base_picker.is_none(), "picker closes on switch");
        assert_eq!(app.current_worktree.as_deref(), Some("wt-b"));
        match rx.try_recv().unwrap() {
            Request::SwitchWorktree { root } => assert_eq!(root, PathBuf::from("/r/b")),
            other => panic!("expected SwitchWorktree, got {other:?}"),
        }
        // An unvisited worktree presents the startup view, so the switch is
        // followed by a SetRev for the startup revision.
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "main...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn worktree_enter_on_current_worktree_only_closes() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_worktree_picker_with(two_worktrees());
        assert_eq!(
            app.base_picker.as_ref().unwrap().selection,
            0,
            "main is current"
        );

        app.select_picked_base();

        assert!(app.base_picker.is_none());
        assert_eq!(app.current_worktree, None);
        assert!(rx.try_recv().is_err(), "same-root switch sends nothing");
    }

    #[test]
    fn worktree_switch_round_trip_restores_view_state() {
        let tree = Tree::build(vec![
            ('M', "src/a.rs".to_string()),
            ('M', "src/b.rs".to_string()),
            ('M', "top.rs".to_string()),
        ]);
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let mut app = App::new(
            sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial diff

        // Shape the main worktree's view: expand src/, select src/b.rs,
        // uncommitted mode against a picked origin. Flat order is
        // [src, src/a.rs, src/b.rs, top.rs].
        app.expand_or_refresh();
        app.move_down(2);
        let _ = rx.try_recv(); // Diff for src/b.rs
        assert_eq!(app.selected_path.as_deref(), Some("src/b.rs"));

        app.set_base("feature".to_string());
        let _ = rx.try_recv(); // SetRev feature...HEAD
        app.toggle_mode();
        let _ = rx.try_recv(); // SetRev HEAD

        // A → B.
        app.open_worktree_picker_with(two_worktrees());
        app.picker_step(true);
        app.select_picked_base();
        assert_eq!(app.current_worktree.as_deref(), Some("wt-b"));
        let saved = app
            .worktree_ui
            .get("main")
            .expect("state saved on switch-out");
        assert_eq!(saved.mode, Mode::Uncommitted);
        assert_eq!(
            saved.last_origin,
            Some(Origin::Picked("feature".to_string())),
            "the saved origin round-trips with the saved mode"
        );
        assert_eq!(saved.selected_path.as_deref(), Some("src/b.rs"));
        assert!(saved.expanded_dirs.contains("src"));
        match rx.try_recv().unwrap() {
            Request::SwitchWorktree { root } => assert_eq!(root, PathBuf::from("/r/b")),
            other => panic!("expected SwitchWorktree, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "main...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }

        // Mutate the view "in" B; the changes must not leak into A's memory.
        app.move_down(1);
        let _ = rx.try_recv();

        // B → A: the picker opens with the selection on the current entry.
        app.open_worktree_picker_with(two_worktrees());
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1);
        app.picker_step(false);
        app.select_picked_base();
        assert_eq!(app.current_worktree, None);
        match rx.try_recv().unwrap() {
            Request::SwitchWorktree { root } => assert_eq!(root, PathBuf::from("/r/main")),
            other => panic!("expected SwitchWorktree, got {other:?}"),
        }
        // The restored uncommitted mode re-asks for its revision.
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }

        // A stale settled snapshot from the old root, queued before the
        // switch's BUSY placeholder (channel FIFO), must not consume the
        // pending restore.
        app.apply_snapshot(sample_snapshot(Tree::build(vec![
            ('M', "top.rs".to_string()),
            ('M', "old-root-only.rs".to_string()),
        ])));
        assert!(
            app.pending_restore.is_some(),
            "a settled pre-placeholder snapshot must not consume the restore"
        );
        // The stale snapshot legitimately relocated the (still B-phase)
        // selection and asked for its diff; drain it so the restore's own
        // Diff request is unambiguous below.
        let _ = rx.try_recv();

        // The switch's BUSY placeholder clears the awaiting-switch gate but
        // must not consume the restore either.
        let mut placeholder = sample_snapshot(Tree::build(Vec::new()));
        Arc::get_mut(&mut placeholder).unwrap().busy = true;
        app.apply_snapshot(placeholder);
        assert!(
            app.pending_restore.is_some(),
            "the BUSY placeholder must not consume the restore"
        );
        assert!(!app.awaiting_switch, "the placeholder clears the gate");

        // Session preferences in effect when the restored tree lands (context
        // width, folded hunks) must survive the restore instead of being
        // reset to defaults.
        app.context_lines = 5;
        app.collapsed_hunks = [2].into_iter().collect();

        // Two-phase restore: A's tree arrives from the worker.
        let tree_a = Tree::build(vec![
            ('M', "src/a.rs".to_string()),
            ('M', "src/b.rs".to_string()),
            ('M', "top.rs".to_string()),
        ]);
        app.apply_snapshot(sample_snapshot(tree_a));
        assert_eq!(app.mode, Mode::Uncommitted);
        assert_eq!(app.selected_path.as_deref(), Some("src/b.rs"));
        assert!(app.expanded_dirs.contains("src"));
        let src_idx = app.flat[0];
        assert!(app.tree.nodes[src_idx].expanded);
        match rx.try_recv().unwrap() {
            Request::Diff { path, ctx, .. } => {
                assert_eq!(path, "src/b.rs");
                assert_eq!(
                    ctx, 5,
                    "the diff is re-requested at the preserved context level"
                );
            }
            other => panic!("expected Diff for the restored selection, got {other:?}"),
        }
        assert_eq!(
            app.context_lines, 5,
            "the restore preserves the context width"
        );
        assert!(
            app.collapsed_hunks.contains(&2),
            "the restore preserves folded hunks"
        );

        // The restored mode round-trips to the worktree's own remembered
        // origin, not one inherited from the worktree switched away from.
        app.toggle_mode();
        assert_eq!(
            app.mode,
            Mode::Branch(Origin::Picked("feature".to_string()))
        );
        let _ = rx.try_recv(); // SetRev feature...HEAD

        // Idempotent: a later unrelated snapshot must not re-trigger restore.
        let tree_a2 = Tree::build(vec![
            ('M', "src/a.rs".to_string()),
            ('M', "src/b.rs".to_string()),
            ('M', "top.rs".to_string()),
        ]);
        app.apply_snapshot(sample_snapshot(tree_a2));
        assert_eq!(app.selected_path.as_deref(), Some("src/b.rs"));
        assert!(rx.try_recv().is_err(), "restore fires once");
    }

    #[test]
    fn failed_switch_drops_its_pending_restore() {
        // A worker-rejected switch replays the OLD view as a settled error
        // snapshot: the SAME tree Arc, so `tree_changed == false`. The
        // stale restore must be dropped there, not left armed for a later
        // unrelated recompute to apply onto the old worktree's tree.
        let tree = Tree::build(vec![('M', "old.rs".to_string())]);
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let mut app = App::new(
            sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial diff

        app.pending_restore = Some(WorktreeUiState {
            mode: Mode::Uncommitted,
            last_origin: None,
            selected_path: Some("target.rs".to_string()),
            expanded_dirs: HashSet::new(),
            tree_scroll: 0,
            diff_scroll: 0,
            diff_col: 0,
        });
        app.awaiting_switch = true;

        // The rejected switch's settled error snapshot: every inner Arc is
        // shared with the current snapshot, so nothing changed.
        let replay = app.snapshot.clone();
        app.apply_snapshot(replay);
        assert!(app.pending_restore.is_none(), "stale restore dropped");
        assert!(!app.awaiting_switch, "gate cleared");
        assert!(
            rx.try_recv().is_err(),
            "no diff requested from the stale restore"
        );
    }

    #[test]
    fn worktree_switch_to_unvisited_worktree_starts_fresh() {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![('M', "a.rs".to_string())])),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.toggle_mode();
        let _ = rx.try_recv();

        app.open_worktree_picker_with(two_worktrees());
        app.picker_step(true);
        app.select_picked_base();

        // Startup defaults, not the previous worktree's mode/selection.
        assert_eq!(app.mode, Mode::Branch(Origin::Default));
        assert_eq!(app.selected_path, None);
        assert!(app.expanded_dirs.is_empty());

        // The new tree arrives: selection lands on the first row.
        app.apply_snapshot(sample_snapshot(Tree::build(vec![(
            'M',
            "z.rs".to_string(),
        )])));
        assert_eq!(app.selected_path.as_deref(), Some("z.rs"));
    }

    #[test]
    fn poll_dirty_counts_stores_arriving_counts() {
        let mut app = sample_app();
        assert!(!app.poll_dirty_counts(), "no scan: nothing to drain");
        let (tx, rx) = crossbeam_channel::unbounded::<(String, DirtyStats)>();
        app.dirty_rx = Some(rx);
        tx.send(("wt-b".to_string(), DirtyStats { files: 3 }))
            .unwrap();
        tx.send(("main".to_string(), DirtyStats { files: 0 }))
            .unwrap();

        assert!(app.poll_dirty_counts());
        assert_eq!(app.dirty_counts.get("wt-b"), Some(&3));
        assert_eq!(app.dirty_counts.get("main"), Some(&0));
        assert!(!app.poll_dirty_counts(), "drained: no change");
    }

    /// A picker entry with `rev` used as both revision spec and label.
    fn pentry(rev: &str) -> PickerEntry {
        PickerEntry {
            rev: rev.to_string(),
            label: rev.to_string(),
        }
    }

    /// A worktree entry with a tip commit time, for the ordering tests.
    fn wt_timed(id: &str, tip_time: Option<i64>) -> WorktreeInfo {
        WorktreeInfo {
            id: id.to_string(),
            path: PathBuf::from(format!("/r/{id}")),
            branch: Some(id.to_string()),
            main: id == "main",
            missing: false,
            tip_time,
        }
    }

    /// Ids of the open worktree picker's rows, in display order.
    fn picker_ids(app: &App) -> Vec<String> {
        app.base_picker
            .as_ref()
            .unwrap()
            .worktrees
            .iter()
            .map(|w| w.id.clone())
            .collect()
    }

    #[test]
    fn worktree_picker_orders_dirty_then_tip_then_listing() {
        let mut app = sample_app();
        let entries = || {
            vec![
                wt_timed("wt-b", Some(100)),
                wt_timed("main", Some(300)),
                wt_timed("wt-a", Some(200)),
            ]
        };

        // Recency only: newest tip first.
        app.open_worktree_picker_with(entries());
        assert_eq!(picker_ids(&app), ["main", "wt-a", "wt-b"]);

        // Dirty beats recency.
        app.close_picker();
        app.dirty_counts.insert("wt-b".to_string(), 3);
        app.open_worktree_picker_with(entries());
        assert_eq!(picker_ids(&app), ["wt-b", "main", "wt-a"]);

        // Missing always last.
        app.close_picker();
        app.open_worktree_picker_with(vec![
            wtinfo("gone", "/r/gone", None, false, true),
            wtinfo("main", "/r/main", Some("main"), true, false),
        ]);
        assert_eq!(picker_ids(&app), ["main", "gone"]);

        // Unknown tip time sorts last within its tier.
        app.close_picker();
        app.open_worktree_picker_with(vec![wt_timed("wt-x", None), wt_timed("main", Some(50))]);
        assert_eq!(picker_ids(&app), ["main", "wt-x"]);
    }

    #[test]
    fn poll_dirty_counts_resorts_picker_and_selection_follows_id() {
        let mut app = sample_app();
        app.open_worktree_picker_with(vec![
            wt_timed("main", Some(300)),
            wt_timed("wt-a", Some(200)),
        ]);
        app.base_picker.as_mut().unwrap().selection = 1; // on wt-a
        let (tx, rx) = crossbeam_channel::unbounded::<(String, DirtyStats)>();
        app.dirty_rx = Some(rx);
        tx.send(("wt-a".to_string(), DirtyStats { files: 5 }))
            .unwrap();

        assert!(app.poll_dirty_counts());

        // wt-a jumped to the top and the selection followed its id, not its
        // (shifted) index; entries stay index-aligned with the worktrees.
        assert_eq!(picker_ids(&app), ["wt-a", "main"]);
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.worktrees[picker.selection].id, "wt-a");
        assert_eq!(picker.entries[picker.selection].rev, "wt-a");
    }

    #[test]
    fn filter_narrows_base_picker_and_enter_picks_the_match() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_picker_with(
            " compare against a branch ",
            vec![pentry("develop"), pentry("feature/x"), pentry("main")],
        );
        // Selection prefills on the current base (main); typing re-anchors.
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 2);
        for c in "fea".chars() {
            app.picker_push_filter(c);
        }
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.filter, "fea");
        assert_eq!(picker.filtered_indices(), vec![1]);
        assert_eq!(picker.selection, 1, "re-anchored onto the surviving row");

        // j/k stay within the filtered subset; Enter picks it.
        app.picker_step(true);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1, "clamped");
        app.select_picked_base();
        assert!(app.base_picker.is_none());
        assert_eq!(
            app.mode,
            Mode::Branch(Origin::Picked("feature/x".to_string()))
        );
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "feature/x...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn filter_matching_is_case_insensitive() {
        let mut app = sample_app();
        app.open_picker_with(" b ", vec![pentry("develop"), pentry("feature/x")]);
        for c in "DEV".chars() {
            app.picker_push_filter(c);
        }
        assert_eq!(
            app.base_picker.as_ref().unwrap().filtered_indices(),
            vec![0]
        );
    }

    #[test]
    fn filter_matches_commit_subject_and_full_oid() {
        let mut app = sample_app();
        app.open_commit_picker_with(vec![
            PickerEntry {
                rev: "abcdef1234567890".to_string(),
                label: "abcdef1 fix the worker loop".to_string(),
            },
            PickerEntry {
                rev: "1111111111111111".to_string(),
                label: "1111111 unrelated".to_string(),
            },
        ]);
        for c in "worker".chars() {
            app.picker_push_filter(c);
        }
        assert_eq!(
            app.base_picker.as_ref().unwrap().filtered_indices(),
            vec![0],
            "subject matches"
        );
        app.picker_clear_filter();
        for c in "cdef1234".chars() {
            app.picker_push_filter(c);
        }
        assert_eq!(
            app.base_picker.as_ref().unwrap().filtered_indices(),
            vec![0],
            "full oid matches (via the rev)"
        );
        app.picker_clear_filter();
        for c in "unrel".chars() {
            app.picker_push_filter(c);
        }
        assert_eq!(
            app.base_picker.as_ref().unwrap().filtered_indices(),
            vec![1]
        );
        app.select_picked_base();
        assert_eq!(
            app.mode,
            Mode::Branch(Origin::Picked("1111111111111111".to_string()))
        );
    }

    #[test]
    fn filter_matches_worktree_branch_path_and_id() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_worktree_picker_with(two_worktrees());
        for c in "feat".chars() {
            app.picker_push_filter(c);
        }
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.filtered_indices(), vec![1], "branch matches");
        assert_eq!(picker.selection, 1);
        app.select_picked_base();
        match rx.try_recv().unwrap() {
            Request::SwitchWorktree { root } => assert_eq!(root, PathBuf::from("/r/b")),
            other => panic!("expected SwitchWorktree, got {other:?}"),
        }

        app.open_worktree_picker_with(two_worktrees());
        for c in "/r/main".chars() {
            app.picker_push_filter(c);
        }
        assert_eq!(
            app.base_picker.as_ref().unwrap().filtered_indices(),
            vec![0],
            "path matches"
        );
        app.picker_clear_filter();
        for c in "b".chars() {
            app.picker_push_filter(c);
        }
        assert_eq!(
            app.base_picker.as_ref().unwrap().filtered_indices(),
            vec![1],
            "id matches"
        );
    }

    #[test]
    fn filter_backspace_edits_and_enter_on_empty_list_is_noop() {
        let mut app = sample_app();
        app.open_picker_with(" b ", vec![pentry("develop"), pentry("feature/x")]);
        for c in "evex".chars() {
            app.picker_push_filter(c);
        }
        assert!(
            app.base_picker
                .as_ref()
                .unwrap()
                .filtered_indices()
                .is_empty(),
            "no row matches"
        );
        app.select_picked_base();
        assert!(
            app.base_picker.is_some(),
            "empty filtered list: Enter is a no-op"
        );

        // Backspace narrows the filter back to a matching prefix; the
        // selection was already on a surviving row and stays put.
        app.picker_filter_backspace();
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.filter, "eve");
        assert_eq!(picker.filtered_indices(), vec![0]);
        assert_eq!(picker.selection, 0);
    }

    #[test]
    fn filter_clear_restores_full_list_and_does_not_survive_reopen() {
        let mut app = sample_app();
        app.open_picker_with(" b ", vec![pentry("develop"), pentry("feature/x")]);
        for c in "fea".chars() {
            app.picker_push_filter(c);
        }
        app.picker_clear_filter();
        let picker = app.base_picker.as_ref().unwrap();
        assert!(picker.filter.is_empty());
        assert_eq!(picker.filtered_indices(), vec![0, 1]);

        // Closing drops the filter; a fresh open starts unfiltered.
        app.picker_push_filter('f');
        app.close_picker();
        app.open_picker_with(" b ", vec![pentry("develop"), pentry("feature/x")]);
        let picker = app.base_picker.as_ref().unwrap();
        assert!(picker.filter.is_empty(), "filter resets on open");
        assert_eq!(picker.filtered_indices(), vec![0, 1]);
    }

    #[test]
    fn click_picker_maps_rows_through_the_filter() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_picker_with(" b ", vec![pentry("develop"), pentry("feature/x")]);
        for c in "fea".chars() {
            app.picker_push_filter(c);
        }
        // The renderer maps the first visible row to real index 1.
        app.mouse.picker = Some(PickerGeometry {
            popup: Rect::new(0, 0, 20, 6),
            inner: Rect::new(1, 1, 18, 4),
            first_row: 1,
        });
        // inner.y = 1: row 2 is the first entry row → the only match.
        assert!(app.click_picker(3, 2));
        assert!(app.base_picker.is_none());
        match rx.try_recv().unwrap() {
            Request::SetRev { rev } => assert_eq!(rev, "feature/x...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }

        // A click on the filter row itself is consumed but inert.
        app.open_picker_with(" b ", vec![pentry("develop"), pentry("feature/x")]);
        app.picker_push_filter('f');
        app.mouse.picker = Some(PickerGeometry {
            popup: Rect::new(0, 0, 20, 6),
            inner: Rect::new(1, 1, 18, 4),
            first_row: 1,
        });
        assert!(app.click_picker(3, 1), "filter-row click consumed");
        assert!(app.base_picker.is_some(), "filter-row click is inert");
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1);
    }
}
