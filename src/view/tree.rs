//! Tree-pane rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{App, Mode};
use crate::diff::FileStatus;
use crate::theme;
use crate::theme::Focus;
use crate::tree::{self, TreeNode};

/// Render the file tree with the selection highlighted.
pub fn render_tree(f: &mut Frame, app: &mut App, area: Rect) {
    // Keep the selection within the visible window. Drawing clamps the
    // selection and scroll into range as a side effect of layout.
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

        draw_aligned(f, area.x, y, area.width, &left, stat.as_ref());
    }
}

/// Render the branch-state footer row at the bottom of the tree pane. Shows
/// the current branch, its divergence from the base ref, and unpushed commits
/// vs the upstream. Renders nothing when the area is too narrow or the branch
/// state is empty (initial frame before the first worker snapshot).
pub fn render_branch_footer(f: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height < 1 {
        return;
    }
    let state = app.branch_state();
    if state.name.is_none() && state.unpushed.is_none() && state.base.is_none() {
        return;
    }

    let mut left: Vec<Span<'static>> = Vec::new();

    match &state.name {
        Some(name) => {
            left.push(Span::styled(
                format!("\u{e0a5} {name} "),
                Style::default().fg(theme::DIR_FG),
            ));
        }
        None => {
            left.push(Span::styled(
                "(detached) ",
                Style::default().fg(theme::DIR_FG),
            ));
        }
    }

    if let (Some(a), Some(b)) = (state.ahead, state.behind) {
        if a > 0 {
            left.push(Span::styled(
                format!("\u{2191}{a} "),
                Style::default().fg(Color::Green),
            ));
        }
        if b > 0 {
            left.push(Span::styled(
                format!("\u{2193}{b} "),
                Style::default().fg(Color::Red),
            ));
        }
    }

    if let Some(u) = state.unpushed {
        left.push(Span::styled(
            format!("\u{f06a}{u} "),
            Style::default().fg(Color::Yellow),
        ));
    }

    let left = Line::from(left);
    let mut right: Vec<Span<'static>> = Vec::new();
    match app.mode {
        Mode::DiffToMain => {
            if let Some(base) = state.base.as_deref() {
                right.push(Span::styled(
                    base.to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        Mode::Uncommitted => {
            right.push(Span::styled("wc", Style::default().fg(Color::DarkGray)));
        }
    }
    let right = Line::from(right);

    draw_aligned(f, area.x, area.y, area.width, &left, Some(&right));
}

/// Draw `left` left-aligned at the pane origin and `right` right-aligned at
/// the pane's right edge, leaving one separating cell, over `width` columns.
/// When `right` is `None`, `left` spans the full width.
fn draw_aligned(f: &mut Frame, x: u16, y: u16, width: u16, left: &Line, right: Option<&Line>) {
    match right {
        Some(r) => {
            let rw = r.width() as u16;
            let left_w = width.saturating_sub(rw).saturating_sub(1);
            f.buffer_mut().set_line(x, y, left, left_w);
            f.buffer_mut()
                .set_line(x + width.saturating_sub(rw), y, r, rw);
        }
        None => {
            f.buffer_mut().set_line(x, y, left, width);
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
            .and_then(FileStatus::from_char)
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
/// Unknown extensions fall back to a generic file icon. Shared with the diff
/// header rows, which show the same icon for a file and its hunks.
pub(crate) fn file_icon(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("") {
        "rs" => "\u{e7a8}",                             // rust
        "toml" | "yaml" | "yml" | "json" => "\u{e615}", // config gear
        "py" => "\u{e795}",                             // python
        "js" | "jsx" | "mjs" | "cjs" => "\u{e8c8}",     // javascript
        "ts" | "tsx" => "\u{e628}",                     // typescript (seti)
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
    use std::sync::Arc;

    #[test]
    fn tree_row_focus_colors() {
        let tree = crate::tree::Tree::build(vec![('M', "x.rs".to_string())]);
        let mut app = App::new(
            sample_snapshot(tree),
            crossbeam_channel::bounded::<crate::snapshot::Request>(4).0,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
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

    #[test]
    fn branch_footer_renders() {
        let mut snap = sample_snapshot(crate::tree::Tree::build(vec![('M', "x.rs".to_string())]));
        Arc::get_mut(&mut snap).unwrap().branch_state = crate::git::BranchState {
            name: Some("feature/x".into()),
            base: Some("main".into()),
            ahead: Some(3),
            behind: Some(0),
            upstream: Some("origin/feature/x".into()),
            unpushed: Some(2),
        };
        let mut app = App::new(
            snap,
            crossbeam_channel::bounded::<crate::snapshot::Request>(4).0,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 23)).unwrap().symbol(), "\u{e0a5}");
        assert_eq!(buf.cell((57, 23)).unwrap().symbol(), "e");
    }

    #[test]
    fn file_icon_extensionless_files_get_generic() {
        let generic = file_icon("");
        // An extension-less name must not be treated as its own extension.
        assert_eq!(file_icon("rs"), generic);
        assert_eq!(file_icon("go"), generic);
        assert_eq!(file_icon("c"), generic);
        assert_eq!(file_icon("README"), generic);
        // A real extension still selects the specific icon.
        assert_ne!(file_icon("a.rs"), generic);
        assert_ne!(file_icon("a.go"), generic);
    }
}
