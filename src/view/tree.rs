//! Tree-pane rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::App;
use crate::diff::FileStatus;
use crate::text::add_del_spans;
use crate::theme;
use crate::theme::Focus;
use crate::tree::{self, TreeNode};

use super::footer::{bar_style, chip};

/// Render the tree pane's header row: a DarkGray strip (the visual bookend
/// of the bottom status bar) summarizing the *shape* of what the list
/// beneath it shows — changed-file count with per-status breakdown. The
/// magnitude (diff totals) lives on the full-width status bar, which has the
/// room this third-width pane lacks. The strip opens with a rounded cap on
/// its left edge and runs flush to the screen edge on the right — a closing
/// cap against the terminal edge would float.
pub fn render_tree_header(f: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height < 1 {
        return;
    }
    f.buffer_mut()
        .set_style(area, Style::default().bg(Color::DarkGray));

    let root = &app.snapshot.tree.nodes[app.snapshot.tree.root];

    // Left: the changed-file count. Right: the per-status breakdown — the
    // shape of the change, in the tree's own status colors. Each status
    // becomes a compact rounded pill (status-color fill, bold black text) in
    // the diff pane's language; the count keeps its plain White-on-DarkGray
    // span.
    let mut left: Vec<Span<'static>> = Vec::new();
    let mut right: Vec<Span<'static>> = Vec::new();
    if root.file_count > 0 {
        left.push(Span::styled(
            format!(
                " {} file{} ",
                root.file_count,
                if root.file_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ));
        let order = tree::STATUS_ORDER;
        // One bar-colored cell between pills; the pills separate
        // themselves, so no `· ` marker.
        let mut first = true;
        for (idx, &letter) in order.iter().enumerate() {
            let count = root.status_counts[idx];
            if count == 0 {
                continue;
            }
            if !first {
                right.push(Span::styled(" ", bar_style()));
            }
            first = false;
            let color = FileStatus::from_char(letter)
                .map(theme::status_color)
                .unwrap_or(Color::White);
            if !first {
                right.push(Span::styled(" ", bar_style()));
            }
            first = false;
            right.extend(chip(&format!("{count}{letter}"), color));
        }
    } else if app.is_busy() {
        // The list below shows `scanning changes…`; keep the header in the
        // same state instead of claiming `no changes` before the scan settles.
        left.push(Span::styled(
            " scanning changes… ",
            Style::default().fg(Color::Gray).bg(Color::DarkGray),
        ));
    } else {
        left.push(Span::styled(
            " no changes ",
            Style::default().fg(Color::DarkGray).bg(Color::DarkGray),
        ));
    }
    let left = Line::from(left);
    let right = if right.is_empty() {
        None
    } else {
        Some(Line::from(right))
    };

    // The strip starts one column in, leaving room for a rounded opening
    // cap, and runs flush to the screen's right edge with no trailing cap.
    // Let `set_line` clip when the pane is narrow.
    let opening = Line::from(vec![Span::styled(
        crate::theme::CAP_LEFT,
        Style::default().fg(Color::DarkGray).bg(Color::Reset),
    )]);
    f.buffer_mut().set_line(area.x, area.y, &opening, 1);
    let strip_w = area.width.saturating_sub(1);
    draw_aligned(f, area.x + 1, area.y, strip_w, &left, right.as_ref());
}

/// Render the file tree with the selection highlighted.
pub fn render_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let visible = area.height as usize;
    if app.flat.is_empty() {
        let text = if app.is_busy() {
            "scanning changes…"
        } else {
            "no changes"
        };
        let line = Line::styled(text, Style::default().fg(Color::DarkGray));
        f.buffer_mut().set_line(area.x, area.y, &line, area.width);
        return;
    }

    // Keep the selection within the visible window via the app's shared
    // clamp owner, so render-side follow-selection cannot drift from
    // navigation-side clamping (`App::reveal_tree_selection`).
    app.clamp_tree_view(area.height as usize);

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

/// Draw `left` left-aligned at the pane origin and `right` right-aligned at
/// the pane's right edge, leaving one separating cell, over `width` columns.
/// When `right` is `None`, `left` spans the full width.
pub(super) fn draw_aligned(
    f: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    left: &Line,
    right: Option<&Line>,
) {
    match right {
        Some(r) => {
            // Clamp so an over-wide right line clips inside the pane instead
            // of spilling past it.
            let rw = (r.width() as u16).min(width);
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
    let spans = add_del_spans(node.added, node.deleted, Some(bg), true);
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.selected_path = Some("x.rs".into());
        app.selection = 0;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // The tree pane's header occupies row 0; the first list row is row 1.
        assert_eq!(buf.cell((54, 1)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));

        app.focus = Focus::Diff;
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 1)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
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

    /// An `App` over `tree` with its `busy` flag set explicitly.
    fn app_with_busy(tree: crate::tree::Tree, busy: bool) -> App {
        let mut snap = sample_snapshot(tree);
        Arc::get_mut(&mut snap).unwrap().busy = busy;
        App::new(
            snap,
            crossbeam_channel::bounded::<crate::snapshot::Request>(4).0,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        )
    }

    fn render(app: &mut App) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn empty_tree_busy_placeholder() {
        let mut app = app_with_busy(crate::tree::Tree::build(Vec::new()), true);
        let buf = render(&mut app);
        // The list row (the header strip on row 0 also reads `no changes`).
        let row: String = (54..80)
            .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("scanning changes…"), "{row}");
        assert!(!row.contains("no changes"), "{row}");
        // The header strip agrees with the list: no `no changes` while busy.
        let header: String = (54..80)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(header.contains("scanning changes…"), "{header}");
        assert!(!header.contains("no changes"), "{header}");
    }

    #[test]
    fn empty_tree_settled_placeholder() {
        let mut app = app_with_busy(crate::tree::Tree::build(Vec::new()), false);
        let buf = render(&mut app);
        let row: String = (54..80)
            .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("no changes"), "{row}");
        assert!(!row.contains("scanning"), "{row}");
        // Settled empty header keeps its original `no changes` text.
        let header: String = (54..80)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(header.contains("no changes"), "{header}");
        assert!(!header.contains("scanning"), "{header}");
    }
}
