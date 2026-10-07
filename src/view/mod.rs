//! Rendering layer: lays out the diff and tree panes and draws them.
//!
//! This is the presentation layer. It reads `crate::app::App` state and writes
//! only the transient view-derived fields (`diff_width`, `height`,
//! `last_header_height`, `sticky_file`/`sticky_hunk`). During draw it also clamps the
//! selection/cursor state used to lay out the panes as a side effect
//! (`app.selection` and `app.tree_scroll` in the tree pane; `app.diff_scroll`
//! in the diff pane). No application logic lives here.

mod diff;
mod tree;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Frame;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::app::{App, MouseGeometry, PickerGeometry, PickerKind};
use crate::git::WorktreeInfo;
use crate::preview;

/// How long a transient copy confirmation (`App::flash`) overrides the
/// persistent status messages.
pub(crate) const FLASH_TTL: Duration = Duration::from_secs(3);

/// Compute the pane split and draw both panes plus a full-width footer row
/// (vim-style status line). Returns early on degenerate terminal sizes.
pub fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 6 || area.height < 2 {
        // A skipped frame must not leave last frame's hitboxes live.
        app.mouse = MouseGeometry::default();
        return;
    }

    // The bottom row is always the footer; both panes share the rest.
    let body_height = area.height.saturating_sub(1);

    let tree_w = (area.width / 3).clamp(1, area.width.saturating_sub(2));
    let diff_w = area.width.saturating_sub(tree_w).saturating_sub(1);

    if app.diff_width != diff_w {
        app.diff_width = diff_w;
        // A resize can move the pane's right edge left; re-clamp the
        // horizontal scroll so it is never past the new maximum.
        app.clamp_diff_col();
    }
    app.height = body_height as usize;

    let diff = ratatui::layout::Rect::new(area.x, area.y, diff_w, body_height);
    // The tree pane carries a one-row header summarizing what it lists
    // (file count, per-status breakdown, diff totals); the list follows
    // beneath it.
    let tree_header = ratatui::layout::Rect::new(area.x + diff_w + 1, area.y, tree_w, 1);
    let tree = ratatui::layout::Rect::new(
        area.x + diff_w + 1,
        area.y + 1,
        tree_w,
        body_height.saturating_sub(1),
    );
    let footer = ratatui::layout::Rect::new(area.x, area.y + body_height, area.width, 1);
    app.mouse.tree_list = Some(tree);
    app.mouse.footer = Some(footer);

    if app.preview.active() {
        // The diff is not rendered: no sticky rows are pinned, so clear any
        // stale click-row mapping from the previous frame.
        app.sticky_file = None;
        app.sticky_hunk = None;
        preview::render(f, app, diff);
    } else {
        app.preview.area = None;
        diff::render_diff(f, app, diff);
    }
    tree::render_tree_header(f, app, tree_header);
    tree::render_tree(f, app, tree);
    let status = status_line(app);
    tree::render_footer(f, app, footer, status.as_deref());

    if app.base_picker.is_some() {
        render_branch_picker(f, app);
    } else {
        app.mouse.picker = None;
    }
}

/// Draw the base-picker popup centered over the panes: a bordered list of
/// selectable entries with the current one marked and the selected row
/// reversed. The list scrolls so the selection is always visible. Worktree
/// pickers render richer rows (branch, path, dirty chip) and dim missing
/// worktrees.
fn render_branch_picker(f: &mut Frame, app: &mut App) {
    let Some(picker) = app.base_picker.as_ref() else {
        return;
    };
    let area = f.area();
    if area.width < 6 || area.height == 0 || picker.entries.is_empty() {
        app.mouse.picker = None;
        return;
    }

    let worktree_kind = picker.kind == PickerKind::Worktree;
    let current_wt = app
        .current_worktree
        .as_deref()
        .unwrap_or(crate::app::MAIN_WORKTREE);
    // The filtered subset drives every row surface: the list below, the
    // match count, and `first_row` (which `click_picker` maps clicks back
    // through). Selection surfaces in `App` recompute the same subset.
    let visible = picker.filtered_indices();
    // Worktree rows are built once per draw: the same texts feed the width
    // measurement and the render loop (each build allocates the row, its
    // truncated path, and its chip).
    let mut row_texts: Vec<String> = if worktree_kind {
        visible
            .iter()
            .map(|&i| {
                let wt = &picker.worktrees[i];
                worktree_row_text(
                    wt,
                    app.dirty_counts.get(&wt.id).copied(),
                    wt.id == current_wt,
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    // Popup width tracks display columns (what the terminal paints), not
    // chars — a wide-char row would otherwise clip mid-glyph.
    let longest = if worktree_kind {
        row_texts
            .iter()
            .map(|t| display_width(t))
            .max()
            .unwrap_or(0)
    } else {
        picker
            .entries
            .iter()
            .map(|e| display_width(&e.label))
            .max()
            .unwrap_or(0)
    };
    let max_w = (area.width.saturating_mul(3) / 5).max(1);
    let width =
        (u16::try_from(longest).unwrap_or(u16::MAX).saturating_add(6)).clamp(28.min(max_w), max_w);
    // +1 inner row for the shared filter row above the entry list.
    let max_h = (area.height.saturating_mul(4) / 5).max(1);
    let height = (u16::try_from(picker.entries.len())
        .unwrap_or(u16::MAX)
        .saturating_add(3))
    .clamp(6.min(max_h), max_h);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(picker.title.as_str());
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let list_rows = (inner.height as usize).saturating_sub(1);
    let sel_pos = visible
        .iter()
        .position(|&i| i == picker.selection)
        .unwrap_or(0);
    let start = sel_pos.saturating_sub(list_rows.saturating_sub(1));
    app.mouse.picker = Some(PickerGeometry {
        popup: rect,
        inner,
        first_row: visible.get(start).copied().unwrap_or(0),
    });

    // Shared filter row: dim prompt, typed text, block cursor, and a match
    // count once the filter is non-empty.
    if inner.height > 0 {
        let dim = Style::default().fg(Color::DarkGray);
        let count = if picker.filter.is_empty() {
            String::new()
        } else {
            format!("  {}/{}", visible.len(), picker.entries.len())
        };
        let line = Line::from(vec![
            Span::styled("filter: ", dim),
            Span::raw(picker.filter.clone()),
            Span::raw("█"),
            Span::styled(count, dim),
        ]);
        f.buffer_mut()
            .set_line(inner.x, inner.y, &line, inner.width);
    }

    let current_branch = app.snapshot.branch_state.name.as_deref();
    for (offset, &i) in visible.iter().skip(start).take(list_rows).enumerate() {
        let row_y = inner.y + 1 + offset as u16;
        let (text, style) = if worktree_kind {
            let wt = &picker.worktrees[i];
            let mut style = if wt.missing {
                // A pruned worktree reads as unavailable.
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            if i == picker.selection && !wt.missing {
                style = style.add_modifier(Modifier::REVERSED);
            }
            (std::mem::take(&mut row_texts[start + offset]), style)
        } else {
            let mut text = picker.entries[i].label.clone();
            if current_branch == Some(text.as_str()) {
                text.push_str(" *");
            }
            let style = if i == picker.selection {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            (text, style)
        };
        let line = Line::styled(text, style);
        f.buffer_mut().set_line(inner.x, row_y, &line, inner.width);
    }
}

/// Text of one worktree-picker row: the worktree id (the current one marked
/// with `*`), its branch (or `(detached)`), the path capped from the right,
/// and the `±N` dirty chip once that worktree's count has arrived.
fn worktree_row_text(wt: &WorktreeInfo, dirty: Option<usize>, is_current: bool) -> String {
    let branch = wt.branch.as_deref().unwrap_or("(detached)");
    let mut text = format!(
        "{}{}  {}  {}",
        wt.id,
        if is_current { " *" } else { "" },
        branch,
        truncate_right(&wt.path.display().to_string(), 40),
    );
    if let Some(n) = dirty {
        text.push_str(&format!("  ±{n}"));
    }
    text
}

/// Display width of `s` in terminal cells. ratatui 0.30 does not re-export
/// `unicode-width`, so measure through `Span::width`, which uses the same
/// crate internally as `Line::width()` (the footer zones' measurement).
fn display_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Keep the head of `s` within `max` display columns (terminal cells, not
/// chars — CJK/emoji glyphs span more than one column), marking the cut
/// with `…`. The kept head plus the `…` always fit in `max` columns.
fn truncate_right(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    // One column is reserved for the `…`; zero-width chars at the cut edge
    // still fit (width 0 never exceeds the budget).
    let budget = max - 1;
    let mut kept = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let w = display_width(ch.encode_utf8(&mut buf));
        if used + w > budget {
            break;
        }
        kept.push(ch);
        used += w;
    }
    format!("{kept}…")
}

fn status_line(app: &App) -> Option<String> {
    // Deliberate precedence: a fresh copy confirmation outranks the persistent
    // `diff_error`/`picker_error` for its brief window. Reversing this would let
    // a lingering `picker_error` permanently mask later copy confirmations.
    if let Some((msg, at)) = &app.flash {
        if at.elapsed() < FLASH_TTL {
            return Some(msg.clone());
        }
    }
    if let Some(err) = app.diff_error() {
        return Some(format!("git: {err}"));
    }
    if let Some(msg) = app.picker_error() {
        return Some(msg.to_string());
    }
    if app.poll_mode.load(Ordering::Relaxed) {
        return Some("live update: poll mode".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PickerEntry;
    use crate::test_support::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        terminal
    }

    #[test]
    fn ui_records_tree_list_and_footer() {
        let mut app = sample_app();
        render(&mut app, 80, 24);
        assert_eq!(app.mouse.tree_list, Some(Rect::new(54, 1, 26, 22)));
        assert_eq!(app.mouse.footer, Some(Rect::new(0, 23, 80, 1)));
    }

    #[test]
    fn ui_records_and_clears_picker() {
        let mut app = sample_app();
        app.open_picker_with(
            " t ",
            (0..10)
                .map(|i| PickerEntry {
                    rev: format!("r{i}"),
                    label: format!("r{i}"),
                })
                .collect(),
        );
        app.base_picker.as_mut().unwrap().selection = 9;

        render(&mut app, 80, 10);
        let g = app.mouse.picker.expect("picker geometry");
        assert!(g.inner.x >= g.popup.x && g.inner.y >= g.popup.y);
        assert!(g.inner.x + g.inner.width <= g.popup.x + g.popup.width);
        assert!(g.inner.y + g.inner.height <= g.popup.y + g.popup.height);
        // The top inner row is the filter box; entry rows follow beneath it.
        let rows = g.inner.height as usize - 1;
        assert_eq!(g.first_row, 9usize.saturating_sub(rows.saturating_sub(1)));
        assert!(g.first_row > 0, "scrolled picker: {g:?}");

        app.base_picker = None;
        render(&mut app, 80, 10);
        assert!(app.mouse.picker.is_none(), "closed picker leaves no hitbox");
    }

    #[test]
    fn tiny_terminal_resets_mouse_geometry() {
        let mut app = sample_app();
        app.mouse.tree_list = Some(Rect::new(1, 2, 3, 4));
        app.mouse.footer = Some(Rect::new(0, 9, 8, 1));
        app.mouse.footer_zones = vec![(crate::app::FooterZone::Mode, Rect::new(0, 9, 3, 1))];

        render(&mut app, 5, 24);
        assert_eq!(app.mouse.tree_list, None);
        assert_eq!(app.mouse.footer, None);
        assert!(app.mouse.picker.is_none());
        assert!(app.mouse.footer_zones.is_empty());
    }

    #[test]
    fn status_line_shows_fresh_flash_only() {
        let mut app = sample_app();
        app.flash = Some(("copied 2 lines".into(), std::time::Instant::now()));
        assert_eq!(status_line(&app).as_deref(), Some("copied 2 lines"));

        app.flash = Some((
            "stale".into(),
            std::time::Instant::now() - std::time::Duration::from_secs(4),
        ));
        assert_eq!(status_line(&app), None);
    }

    /// A worktree entry for the picker-render test.
    fn wtinfo(id: &str, path: &str, branch: Option<&str>, missing: bool) -> WorktreeInfo {
        WorktreeInfo {
            id: id.to_string(),
            path: std::path::PathBuf::from(path),
            branch: branch.map(Into::into),
            main: id == "main",
            missing,
            tip_time: None,
        }
    }

    #[test]
    fn worktree_picker_rows_mark_current_and_dim_missing() {
        let mut app = sample_app();
        app.set_current_worktree(Some("wt-b".into()));
        app.open_worktree_picker_with(vec![
            wtinfo("main", "/repos/bough", Some("main"), false),
            wtinfo("wt-b", "/repos/.wt/b", Some("feature"), false),
            wtinfo("gone", "/repos/.wt/gone", None, true),
        ]);
        app.dirty_counts.insert("wt-b".into(), 3);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let g = app.mouse.picker.expect("picker geometry");
        let buf = terminal.backend().buffer();
        let row_text = |row: u16| {
            (g.inner.x..g.inner.x + g.inner.width)
                .map(|x| buf.cell((x, row)).unwrap().symbol().to_string())
                .collect::<String>()
        };

        // Selection starts on the current worktree (wt-b), so all three rows
        // are visible in order below the filter row: main, wt-b (marked, with
        // its dirty chip), and the dimmed missing one.
        let filter_row = row_text(g.inner.y);
        assert!(filter_row.contains("filter:"), "{filter_row}");
        let row0 = row_text(g.inner.y + 1);
        let row1 = row_text(g.inner.y + 2);
        let row2 = row_text(g.inner.y + 3);
        assert!(row0.contains("main"), "{row0}");
        assert!(!row0.contains('*'), "non-current is unmarked: {row0}");
        assert!(row1.contains("wt-b *"), "current is marked: {row1}");
        assert!(row1.contains("feature"), "{row1}");
        assert!(row1.contains("/repos/.wt/b"), "{row1}");
        assert!(row1.contains("±3"), "dirty chip: {row1}");
        assert!(row2.contains("gone"), "{row2}");
        assert!(row2.contains("(detached)"), "{row2}");
        let missing_cell = buf.cell((g.inner.x, g.inner.y + 3)).unwrap();
        assert_eq!(missing_cell.fg, Color::DarkGray, "missing row is dimmed");
    }

    #[test]
    fn filter_row_shows_prompt_cursor_and_match_count() {
        let mut app = sample_app();
        app.open_picker_with(
            " b ",
            vec![
                PickerEntry {
                    rev: "alpha".into(),
                    label: "alpha".into(),
                },
                PickerEntry {
                    rev: "beta".into(),
                    label: "beta".into(),
                },
            ],
        );
        app.picker_push_filter('a');
        app.picker_push_filter('l');

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let g = app.mouse.picker.expect("picker geometry");
        let row_text = |buf: &ratatui::buffer::Buffer, row: u16| {
            (g.inner.x..g.inner.x + g.inner.width)
                .map(|x| buf.cell((x, row)).unwrap().symbol().to_string())
                .collect::<String>()
        };
        let buf = terminal.backend().buffer();

        // Prompt, typed text, block cursor, and the 1/2 match count.
        let filter_row = row_text(buf, g.inner.y);
        assert!(filter_row.contains("filter: al█"), "{filter_row}");
        assert!(filter_row.contains("1/2"), "{filter_row}");
        // Entry rows start beneath the filter row and only show the match.
        let first_entry = row_text(buf, g.inner.y + 1);
        assert!(first_entry.contains("alpha"));
        assert!(!first_entry.contains("beta"));

        // An empty filter shows the bare prompt with no count.
        app.picker_clear_filter();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let empty_row = row_text(buf, g.inner.y);
        assert!(empty_row.contains("filter: █"), "{empty_row}");
        assert!(!empty_row.contains("/2"), "{empty_row}");
    }

    #[test]
    fn truncate_right_caps_with_ellipsis() {
        assert_eq!(truncate_right("short", 40), "short");
        let long = truncate_right("/a/very/long/path/that/keeps/going/on", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
        assert_eq!(truncate_right("anything", 0), "");
    }

    #[test]
    fn truncate_right_budgets_display_columns() {
        // CJK glyphs span two terminal columns each: 10 chars paint 14
        // columns, so char-count truncation would overflow the budget.
        let path = "/git/仓库/项目";
        assert_eq!(display_width(path), 14);
        let cut = truncate_right(path, 10);
        assert!(cut.ends_with('…'));
        assert_eq!(display_width(&cut), 10);
        // ASCII rows keep the old behaviour: chars and columns agree.
        let ascii = truncate_right("/a/very/long/path", 10);
        assert_eq!(display_width(&ascii), 10);
        assert_eq!(ascii.chars().count(), 10);
    }
}
