//! Rendering layer: lays out the diff and tree panes and draws them.
//!
//! This is the presentation layer. It reads `crate::app::App` state and writes
//! only the transient view-derived fields (`diff_width`, `height`,
//! `last_header_height`, `sticky_line`). No application logic lives here.

mod diff;
mod tree;

use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::Frame;

use crate::app::App;

/// Compute the pane split and draw both panes plus an optional status line.
/// Returns early on degenerate terminal sizes.
pub fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 6 || area.height == 0 {
        return;
    }

    let status = status_line(app);
    let body_height = if status.is_some() {
        area.height.saturating_sub(1)
    } else {
        area.height
    };

    let tree_w = (area.width / 3).clamp(1, area.width.saturating_sub(2));
    let diff_w = area.width.saturating_sub(tree_w).saturating_sub(1);

    app.diff_width = diff_w;
    app.height = body_height as usize;

    let diff = ratatui::layout::Rect::new(area.x, area.y, diff_w, body_height);
    let tree = ratatui::layout::Rect::new(area.x + diff_w + 1, area.y, tree_w, body_height);

    diff::render_diff(f, app, diff);
    tree::render_tree(f, app, tree);

    if let Some(msg) = status {
        let y = area.y + body_height;
        let line = Line::styled(msg, Style::default().fg(Color::Yellow));
        f.buffer_mut().set_line(area.x, y, &line, area.width);
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
