//! Tree-pane rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{App, Focus};
use crate::theme;
use crate::tree::{self, TreeNode};

/// Render the file tree with the selection highlighted.
pub fn render_tree(f: &mut Frame, app: &mut App, area: Rect) {
    // Keep the selection within the visible window.
    let visible = area.height as usize;
    if app.flat.is_empty() {
        let line = Line::styled("no changes", Style::default().fg(Color::DarkGray));
        f.buffer_mut().set_line(area.x, area.y, &line, area.width);
        return;
    }

    if app.selection > app.flat.len() - 1 {
        app.selection = app.flat.len() - 1;
    }
    if app.tree_scroll > app.selection {
        app.tree_scroll = app.selection;
    }
    if app.selection >= app.tree_scroll + visible && visible > 0 {
        app.tree_scroll = app.selection - visible + 1;
    }

    let end = (app.tree_scroll + visible).min(app.flat.len());
    for i in app.tree_scroll..end {
        let node_idx = app.flat[i];
        let selected = i == app.selection;
        let node = &app.tree.nodes[node_idx];
        let row_bg = if selected {
            if app.focus == Focus::Tree {
                theme::BG_HUNK_ACTIVE
            } else {
                // Unfocused tree selection matches the inactive chunk header.
                theme::BG_HUNK
            }
        } else {
            Color::Reset
        };
        let left = tree_left(app, node_idx, row_bg);
        let stat = stat_line(node, row_bg);
        let y = area.y + (i - app.tree_scroll) as u16;

        // Fill the whole row with the row background so the selection spans the
        // full line width, then draw the text on top.
        f.buffer_mut().set_style(
            Rect::new(area.x, y, area.width, 1),
            Style::default().bg(row_bg),
        );

        match &stat {
            Some(s) => {
                let sw = s.width() as u16;
                let left_w = area.width.saturating_sub(sw).saturating_sub(1);
                f.buffer_mut().set_line(area.x, y, &left, left_w);
                let sx = area.x + area.width.saturating_sub(sw);
                f.buffer_mut().set_line(sx, y, s, sw);
            }
            None => {
                f.buffer_mut().set_line(area.x, y, &left, area.width);
            }
        }
    }
}

fn tree_left(app: &App, node_idx: usize, bg: Color) -> Line<'static> {
    let node = &app.tree.nodes[node_idx];

    let indent = "  ".repeat(node.depth.saturating_sub(1));

    let name_color = match node.kind {
        tree::Kind::Dir => theme::DIR_FG,
        tree::Kind::File => node
            .status
            .chars()
            .next()
            .map(theme::status_color)
            .unwrap_or(Color::Reset),
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(indent, Style::default().bg(bg)));

    match node.kind {
        tree::Kind::Dir => {
            // Open/closed folder icon; colored like the name.
            let glyph = if node.expanded {
                "\u{f07c}"
            } else {
                "\u{f07b}"
            };
            spans.push(Span::styled(
                format!("{glyph} "),
                Style::default().fg(theme::DIR_FG).bg(bg),
            ));
        }
        tree::Kind::File => {
            // File-type icon plus a space keep names aligned with the dir
            // glyph + space width.
            spans.push(Span::styled(
                format!("{} ", file_icon(&node.name)),
                Style::default().fg(name_color).bg(bg),
            ));
        }
    }

    spans.push(Span::styled(
        node.name.clone(),
        Style::default().fg(name_color).bg(bg),
    ));

    if node.kind == tree::Kind::Dir {
        spans.push(Span::styled(
            format!(" {}", node.file_count),
            Style::default().fg(Color::DarkGray).bg(bg),
        ));
    }

    Line::from(spans)
}

/// File-type glyph for a tree row, chosen by extension (Nerd Font glyphs).
/// Unknown extensions fall back to a generic file icon.
fn file_icon(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "rs" => "\u{e7a8}",                             // rust
        "toml" | "yaml" | "yml" | "json" => "\u{e615}", // config gear
        "py" => "\u{e795}",                             // python
        "js" | "jsx" | "mjs" | "cjs" => "\u{e8c8}",     // javascript
        "ts" | "tsx" => "\u{e9a0}",                     // typescript
        "c" | "h" | "cpp" | "hpp" => "\u{e8a0}",        // c/c++
        "go" => "\u{e8bd}",                             // go
        "md" | "markdown" => "\u{e609}",                // markdown
        "sh" | "bash" | "zsh" | "fish" => "\u{f245}",   // shell
        _ => "\u{e612}",                                // generic file
    }
}

fn stat_line(node: &TreeNode, bg: Color) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if node.added > 0 {
        spans.push(Span::styled(
            format!("+{}", node.added),
            Style::default().fg(Color::Green).bg(bg),
        ));
    }
    if node.deleted > 0 {
        if !spans.is_empty() {
            spans.push(Span::styled(" ", Style::default().bg(bg)));
        }
        spans.push(Span::styled(
            format!("-{}", node.deleted),
            Style::default().fg(Color::Red).bg(bg),
        ));
    }
    if spans.is_empty() {
        None
    } else {
        Some(Line::from(spans))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::test_support::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn tree_row_focus_colors() {
        let tree = crate::tree::Tree::build(vec![('M', "x.rs".to_string())]);
        let mut app = App::new(sample_snapshot(tree), crossbeam_channel::bounded::<crate::snapshot::Request>(4).0, false);
        app.selected_path = Some("x.rs".into());
        app.selection = 0;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 0)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));

        app.focus = Focus::Diff;
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 0)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }
}
