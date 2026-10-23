//! Picker popup infrastructure: the shared popup state machine (entries,
//! filter, selection, worktree rows) extracted from `app` so the 4.6k-line
//! App module reads as view-state + tree/diff navigation with pickers as
//! one cohesive subsystem beside it.

use std::cmp::Reverse;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ratatui::layout::Rect;

use crate::app::{rect_contains, App, Mode, Origin};
use crate::git::{DirtyStats, WorktreeInfo};
use crate::snapshot::Request;

/// Worktree id of the main checkout (the synthetic picker entry); the
/// per-worktree view-memory map and `current_worktree`'s `None` both
/// canonicalize to it.
pub(crate) const MAIN_WORKTREE: &str = "main";

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

    /// Real indices of the filtered, selectable rows, in display order.
    /// Shared by `picker_step` (j/k navigation) and
    /// `reanchor_picker_selection` (filter re-anchor), which both need the
    /// filtered ∩ selectable subset.
    pub fn selectable_rows(&self) -> Vec<usize> {
        self.filtered_indices()
            .into_iter()
            .filter(|&i| self.selectable(i))
            .collect()
    }
}

/// Build picker entries from worktree infos: each row's `rev`/`label` is
/// the worktree id (the full [`WorktreeInfo`] lives in
/// [`BasePicker::worktrees`]). Shared by `open_worktree_picker_with` (initial
/// build) and `poll_dirty_counts` (rebuild after re-sort), so the
/// index-alignment invariant (entries[i] ↔ worktrees[i]) is structural.
fn worktree_entries(worktrees: &[WorktreeInfo]) -> Vec<PickerEntry> {
    worktrees
        .iter()
        .map(|w| PickerEntry {
            rev: w.id.clone(),
            label: w.id.clone(),
        })
        .collect()
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

impl App {
    /// Open the base picker populated from the repository's local branches.
    pub fn open_branch_picker(&mut self) {
        self.picker_error = None;
        let entries = match crate::git::local_branches() {
            Ok(branches) => branches
                .into_iter()
                .map(|name| PickerEntry {
                    rev: name.clone(),
                    label: name,
                })
                .collect(),
            Err(e) => {
                self.picker_error = Some(format!("branches unavailable: {e}"));
                Vec::new()
            }
        };
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
        // thread exits after this fixed list either way. The guard below
        // only prevents two receivers on App, NOT two scan threads — a
        // previous scan whose receiver was dropped (a rapid close/reopen)
        // can still be running when the next open spawns. One extra
        // bounded scan may overlap briefly; the fresh cancel flag stops
        // the old thread before its next git scan (the old flag is not
        // reset, so it dies with its own Arc).
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
            let cancel = Arc::new(AtomicBool::new(false));
            self.dirty_scan_cancel = Some(Arc::clone(&cancel));
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
        let picker_entries = worktree_entries(&entries);
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
        // Signal the scan thread to stop, then drop the receiver. The cancel
        // flag stops the thread before its next git scan; dropping the
        // receiver makes its remaining sends fail harmlessly. The old flag
        // dies with its own Arc (the field is `Option`, so a fresh one is
        // created on the next open — the old thread's flag is never reset).
        if let Some(cancel) = self.dirty_scan_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.dirty_rx = None;
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
            self.flash("worktree directory is gone");
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
        if self.awaiting_switch {
            // A second switch would arm over an unanswered epoch: the first
            // attempt's answer would settle the second gate. The window is one
            // worker round-trip, so just refuse the extra pick.
            self.close_picker();
            self.flash("switch already in progress");
            return;
        }
        self.worktree_ui
            .insert(from.clone(), self.capture_ui_state());
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
        // The gate stays set until a snapshot answering the switch (a newer
        // `switch_epoch`) arrives, so stale old-root snapshots queued ahead of
        // it (channel FIFO) can never consume the pending restore in
        // `apply_snapshot`. The epoch observed now is what marks later
        // snapshots as stale (`settle_switch_gates`).
        self.awaiting_switch = true;
        self.switch_epoch = self.snapshot.switch_epoch;
        self.pending_restore = Some((from, wt.id.clone(), saved));
    }

    /// Move the picker selection one row within the filtered subset, skipping
    /// `missing` worktree rows (they render dimmed and are unselectable).
    /// Clamped at both ends of the filtered subset.
    pub fn picker_step(&mut self, down: bool) {
        let Some(picker) = self.base_picker.as_mut() else {
            return;
        };
        let rows = picker.selectable_rows();
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
        let selectable = picker.selectable_rows();
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
            picker.entries = worktree_entries(&picker.worktrees);
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
        let in_rect = |r: Rect| rect_contains(r, column, row);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Mode, Origin, PickerGeometry, WorktreeUiState};
    use crate::snapshot::Request;
    use crate::test_support::*;
    use crate::tree::Tree;
    use ratatui::layout::Rect;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    #[test]
    fn click_picker_row_selects_confirms_and_closes() {
        let (tx, rx) = crossbeam_channel::bounded::<Request>(8);
        let mut app = App::new(
            sample_snapshot(Tree::build(Vec::new())),
            tx,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

        // The switch's busy placeholder answers the A → B switch (fresh epoch)
        // and clears the gate; without it a second pick would be refused as
        // "switch already in progress". It must not consume the restore.
        let mut placeholder = sample_snapshot(Tree::build(Vec::new()));
        {
            let s = Arc::get_mut(&mut placeholder).unwrap();
            s.busy = true;
            s.switch_epoch = app.switch_epoch + 1;
        }
        app.apply_snapshot(placeholder);
        assert!(
            !app.awaiting_switch,
            "the placeholder settles the first switch's gate"
        );
        assert!(
            app.pending_restore.is_some(),
            "the placeholder must not consume the restore"
        );
        let _ = rx.try_recv(); // Diff requested by the placeholder's relocation

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
        // switch's answer (channel FIFO) and still carrying the epoch observed
        // at request time, must not settle the gate or consume the pending
        // restore.
        app.apply_snapshot(sample_snapshot(Tree::build(vec![
            ('M', "top.rs".to_string()),
            ('M', "old-root-only.rs".to_string()),
        ])));
        assert!(
            app.awaiting_switch,
            "a stale old-root snapshot must not settle the gate"
        );
        assert!(
            app.pending_restore.is_some(),
            "a settled pre-placeholder snapshot must not consume the restore"
        );
        // The stale snapshot legitimately relocated the (still B-phase)
        // selection and asked for its diff; drain it so the restore's own
        // Diff request is unambiguous below.
        let _ = rx.try_recv();

        // The switch's busy placeholder answers the switch (fresh epoch) and
        // clears the awaiting-switch gate, but must not consume the restore.
        let mut placeholder = sample_snapshot(Tree::build(Vec::new()));
        {
            let s = Arc::get_mut(&mut placeholder).unwrap();
            s.busy = true;
            s.switch_epoch = app.switch_epoch + 1;
        }
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
        // unrelated recompute to apply onto the old worktree's tree. The
        // target's saved state is re-stashed, and the footer/mode revert to
        // the from worktree's saved state.
        let tree = Tree::build(vec![('M', "old.rs".to_string())]);
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let mut app = App::new(
            sample_snapshot(tree),
            tx,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv(); // initial diff

        // Simulate the from worktree having saved state (as `switch_worktree`
        // does at switch-out), so the revert has something to read.
        app.worktree_ui.insert(
            "main".to_string(),
            WorktreeUiState {
                mode: Mode::Branch(Origin::Picked("feature".to_string())),
                last_origin: Some(Origin::Picked("feature".to_string())),
                selected_path: Some("old.rs".to_string()),
                expanded_dirs: HashSet::new(),
                tree_scroll: 0,
                diff_scroll: 0,
                diff_col: 0,
            },
        );
        app.pending_restore = Some((
            "main".to_string(),
            "wt-b".to_string(),
            Some(WorktreeUiState {
                mode: Mode::Uncommitted,
                last_origin: None,
                selected_path: Some("target.rs".to_string()),
                expanded_dirs: HashSet::new(),
                tree_scroll: 0,
                diff_scroll: 0,
                diff_col: 0,
            }),
        ));
        app.awaiting_switch = true;

        // The rejected switch's settled error snapshot: every inner Arc is
        // shared with the current snapshot, so nothing changed — but it
        // answers the switch attempt with a fresh epoch.
        Arc::get_mut(&mut app.snapshot).unwrap().switch_epoch = app.switch_epoch + 1;
        let replay = app.snapshot.clone();
        app.apply_snapshot(replay);
        assert!(app.pending_restore.is_none(), "stale restore dropped");
        assert!(!app.awaiting_switch, "gate cleared");
        assert!(
            rx.try_recv().is_err(),
            "no diff requested from the stale restore"
        );
        // The target's saved state is re-stashed under its id.
        assert!(
            app.worktree_ui.contains_key("wt-b"),
            "target state re-stashed"
        );
        let re_stashed = app.worktree_ui.get("wt-b").unwrap();
        assert_eq!(re_stashed.mode, Mode::Uncommitted);
        assert_eq!(
            re_stashed.selected_path.as_deref(),
            Some("target.rs"),
            "the re-stashed state is the target's saved state"
        );
        // The footer/mode revert to the from worktree's saved state.
        assert_eq!(
            app.current_worktree, None,
            "footer pill reverts to the from worktree (main)"
        );
        assert_eq!(
            app.mode,
            Mode::Branch(Origin::Picked("feature".to_string())),
            "mode reverts to the from worktree's saved mode"
        );
        assert_eq!(
            app.last_origin,
            Some(Origin::Picked("feature".to_string())),
            "last_origin reverts to the from worktree's saved origin"
        );
    }

    #[test]
    fn worktree_switch_to_unvisited_worktree_starts_fresh() {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let mut app = App::new(
            sample_snapshot(Tree::build(vec![('M', "a.rs".to_string())])),
            tx,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

        // The new tree arrives: selection lands on the first row. The settled
        // snapshot answers the switch (fresh epoch), releasing the gate.
        let mut settled = sample_snapshot(Tree::build(vec![('M', "z.rs".to_string())]));
        Arc::get_mut(&mut settled).unwrap().switch_epoch = app.switch_epoch + 1;
        app.apply_snapshot(settled);
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
