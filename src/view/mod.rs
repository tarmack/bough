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
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Frame;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::app::{App, MouseGeometry, PickerGeometry};
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
/// selectable bases with the current branch marked and the selected row
/// reversed. The list scrolls so the selection is always visible.
fn render_branch_picker(f: &mut Frame, app: &mut App) {
    let Some(picker) = app.base_picker.as_ref() else {
        return;
    };
    let area = f.area();
    if area.width < 6 || area.height == 0 || picker.entries.is_empty() {
        app.mouse.picker = None;
        return;
    }

    let longest = picker
        .entries
        .iter()
        .map(|e| e.label.chars().count())
        .max()
        .unwrap_or(0);
    let max_w = (area.width.saturating_mul(3) / 5).max(1);
    let width =
        (u16::try_from(longest).unwrap_or(u16::MAX).saturating_add(6)).clamp(28.min(max_w), max_w);
    let max_h = (area.height.saturating_mul(4) / 5).max(1);
    let height = (u16::try_from(picker.entries.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2))
    .clamp(5.min(max_h), max_h);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(picker.title.as_str());
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let rows = inner.height as usize;
    // Scroll so the selection is visible, anchoring the window's bottom to the
    // selection once the list outgrows the popup.
    let start = picker.selection.saturating_sub(rows.saturating_sub(1));
    app.mouse.picker = Some(PickerGeometry {
        popup: rect,
        inner,
        first_row: start,
    });
    let current = app.snapshot.branch_state.name.as_deref();
    for (i, entry) in picker.entries.iter().enumerate().skip(start).take(rows) {
        let mut text = entry.label.clone();
        if current == Some(entry.label.as_str()) {
            text.push_str(" *");
        }
        let style = if i == picker.selection {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let line = Line::styled(text, style);
        f.buffer_mut()
            .set_line(inner.x, inner.y + (i - start) as u16, &line, inner.width);
    }
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
        let rows = g.inner.height as usize;
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
}
