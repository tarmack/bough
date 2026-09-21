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

use crate::app::App;
use crate::preview;

/// Compute the pane split and draw both panes plus a full-width footer row
/// (vim-style status line). Returns early on degenerate terminal sizes.
pub fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 6 || area.height < 2 {
        return;
    }

    // The bottom row is always the footer; both panes share the rest.
    let body_height = area.height.saturating_sub(1);

    let tree_w = (area.width / 3).clamp(1, area.width.saturating_sub(2));
    let diff_w = area.width.saturating_sub(tree_w).saturating_sub(1);

    app.diff_width = diff_w;
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
    tree::render_footer(f, app, footer, status_line(app).as_deref());

    if app.base_picker.is_some() {
        render_branch_picker(f, app);
    }
}

/// Draw the base-picker popup centered over the panes: a bordered list of
/// selectable bases with the current branch marked and the selected row
/// reversed. The list scrolls so the selection is always visible.
fn render_branch_picker(f: &mut Frame, app: &App) {
    let Some(picker) = app.base_picker.as_ref() else {
        return;
    };
    let area = f.area();
    if area.width < 6 || area.height == 0 || picker.entries.is_empty() {
        return;
    }

    let longest = picker
        .entries
        .iter()
        .map(|e| e.label.chars().count())
        .max()
        .unwrap_or(0);
    let max_w = (area.width.saturating_mul(3) / 5).max(1);
    let width = (longest as u16 + 6).clamp(28.min(max_w), max_w);
    let max_h = (area.height.saturating_mul(4) / 5).max(1);
    let height = ((picker.entries.len() as u16) + 2).clamp(5.min(max_h), max_h);
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
    if let Some(err) = app.diff_error() {
        return Some(format!("git: {err}"));
    }
    if app.poll_mode {
        return Some("live update: poll mode".into());
    }
    None
}
