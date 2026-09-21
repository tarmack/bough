//! Tree-pane rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{App, Mode, Origin};
use crate::diff::FileStatus;
use crate::theme;
use crate::theme::Focus;
use crate::tree::{self, TreeNode};

/// Render the tree pane's header row: a DarkGray strip (the visual bookend
/// of the bottom status bar) summarizing the *shape* of what the list
/// beneath it shows — changed-file count with per-status breakdown. The
/// magnitude (diff totals) lives on the full-width status bar, which has the
/// room this third-width pane lacks.
pub fn render_tree_header(f: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height < 1 {
        return;
    }
    f.buffer_mut()
        .set_style(area, Style::default().bg(Color::DarkGray));

    let root = &app.snapshot.tree.nodes[app.snapshot.tree.root];

    // Left: the file count and per-status breakdown — the shape of the
    // change, in the tree's own status colors.
    let mut left: Vec<Span<'static>> = Vec::new();
    if root.file_count > 0 {
        left.push(Span::styled(
            format!(
                " {} file{} ",
                root.file_count,
                if root.file_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ));
        let order = ['M', 'A', 'D', 'R', 'C', 'T'];
        let mut counts = std::collections::HashMap::new();
        for node in &app.snapshot.tree.nodes {
            if node.kind == tree::Kind::File {
                if let Some(letter) = node.status.chars().next() {
                    *counts.entry(letter).or_insert(0usize) += 1;
                }
            }
        }
        let parts: Vec<String> = order
            .iter()
            .filter_map(|l| counts.get(l).map(|n| format!("{n}{l}")))
            .collect();
        if !parts.is_empty() {
            left.push(Span::styled(
                "· ".to_string(),
                Style::default().fg(Color::White).bg(Color::DarkGray),
            ));
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    left.push(Span::styled(
                        " ".to_string(),
                        Style::default().fg(Color::White).bg(Color::DarkGray),
                    ));
                }
                let color = crate::diff::FileStatus::from_char(part.chars().last().unwrap())
                    .map(theme::status_color)
                    .unwrap_or(Color::White);
                left.push(Span::styled(
                    part.clone(),
                    Style::default().fg(color).bg(Color::DarkGray),
                ));
            }
        }
    } else {
        left.push(Span::styled(
            " no changes ",
            Style::default().fg(Color::DarkGray).bg(Color::DarkGray),
        ));
    }
    let left = Line::from(left);

    // Right: nothing — the pane is only a third of the terminal wide; the
    // diff totals live on the full-width status bar, which has the room,
    // while the shape of the change (this header's left side) belongs next
    // to the list it summarizes.
    let right = Line::from(Vec::<Span<'static>>::new());

    draw_aligned(f, area.x, area.y, area.width, &left, Some(&right));
}

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

/// Render the vim-airline-style footer row spanning the full terminal width:
/// a powerline bar with rounded chip caps and a full-row background, using
/// only the terminal's 16 default colors so the user's terminal theme stays
/// in charge.
///
/// The bar carries state and magnitude: the branch's remote-sync state
/// (`2 unpushed`), repo-wide diff totals (`+123 -45`) with the uncommitted
/// split (`✎ +40 -12`) as their plain-text qualifier, and transient status
/// messages on the left; on the far right, one two-part pill carrying the
/// identity — ` feature/x ↑3 ↓1  vs main `: the source branch with its
/// divergence from the origin (white on blue), and the origin itself in the
/// mode's color (blue default origin, magenta picked origin, green
/// `Uncommitted`), mirroring vim's mode colors. The *shape* of the change
/// (file count, per-status breakdown) lives in the tree pane's header, next
/// to the list it summarizes.
pub fn render_footer(f: &mut Frame, app: &App, area: Rect, message: Option<&str>) {
    if area.width < 8 || area.height < 1 {
        return;
    }
    let state = app.branch_state();

    // The bar's base background fills the whole row; segments and messages
    // are drawn on top.
    f.buffer_mut()
        .set_style(area, Style::default().bg(Color::DarkGray));

    // Left-side chips, left to right: `(text, bg, fg)`. Worded in GitHub
    // vocabulary so the counts name their relation: `unpushed` commits are
    // ones not yet on the upstream.
    let mut segments: Vec<(String, Color, Color)> = Vec::new();
    if let Some(u) = state.unpushed {
        if u > 0 {
            segments.push((format!("{u} unpushed"), Color::Yellow, Color::Black));
        }
    }

    // Repo-wide diff totals, aggregated at the tree root — the magnitude of
    // the change, in the same green/red language as the per-file stats.
    let root = &app.snapshot.tree.nodes[app.snapshot.tree.root];
    if root.added > 0 {
        segments.push((format!("+{}", root.added), Color::Green, Color::Black));
    }
    if root.deleted > 0 {
        segments.push((format!("-{}", root.deleted), Color::Red, Color::White));
    }

    // Rounded-cap chip per segment, separated by a plain bar-colored cell so
    // each reads as its own chip.
    let mut left: Vec<Span<'static>> = Vec::new();
    for (i, (text, bg, fg)) in segments.iter().enumerate() {
        if i > 0 {
            left.push(Span::styled(" ", bar_style()));
        }
        left.push(Span::styled(
            CAP_LEFT,
            Style::default().fg(*bg).bg(Color::DarkGray),
        ));
        left.push(Span::styled(
            format!(" {text} "),
            Style::default().fg(*fg).bg(*bg),
        ));
        left.push(Span::styled(
            CAP_RIGHT,
            Style::default().fg(*bg).bg(Color::DarkGray),
        ));
    }

    // How much of the view is uncommitted mid-flight work (branch views
    // only). Plain text on the bar, so it reads as a qualifier of the
    // totals chips rather than another chip.
    if let Some((ua, ud)) = app.snapshot.uncommitted_stats {
        if ua > 0 || ud > 0 {
            let on_bar = Style::default().bg(Color::DarkGray);
            left.push(Span::styled(" \u{f040} ", on_bar.fg(Color::White)));
            if ua > 0 {
                left.push(Span::styled(format!("+{ua}"), on_bar.fg(Color::Green)));
            }
            if ud > 0 {
                left.push(Span::styled(format!(" -{ud}"), on_bar.fg(Color::Red)));
            }
            left.push(Span::styled(" ", on_bar));
        }
    }

    // Transient status (git error, poll mode) rides along the bar.
    if let Some(msg) = message {
        left.push(Span::styled("  ", bar_style()));
        left.push(Span::styled(msg.to_string(), bar_style().fg(Color::White)));
    }
    let left = Line::from(left);

    // The far right: one two-part pill, `source  origin`, on colored
    // backgrounds with bold white text — the fills are the recognition
    // signal. The source is always purple; the origin part carries the
    // mode's color (blue default origin, magenta picked, green
    // `Uncommitted`), mirroring vim's mode colors.
    let (mode_bg, label) = match &app.mode {
        Mode::Uncommitted => (Color::Green, "\u{f040} uncommitted".to_string()),
        branch => {
            let bg = match branch {
                Mode::Branch(Origin::Default) => Color::Blue,
                Mode::Branch(Origin::Picked(_)) => Color::Magenta,
                Mode::Uncommitted => unreachable!(),
            };
            (bg, app.mode_label())
        }
    };
    // Bold white reads on every one of the pill's colored fills.
    let on_pill = |bg: Color| {
        Style::default()
            .fg(Color::White)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    };
    let pill_cap = |end: &'static str, color: Color| {
        Span::styled(end, Style::default().fg(color).bg(Color::DarkGray))
    };
    let mut right: Vec<Span<'static>> = Vec::new();
    // The branch's divergence from the origin, shown inside the source part
    // between the two branches it compares — `feature/x 3 ⇄ 1  vs main`:
    // the source-side count first (next to its name), the origin-side count
    // last (next to `vs`), joined by a fat bold double arrow (Material
    // Design's `swap-horizontal-bold`, U+F0BCD — the heaviest double arrow
    // in the font, chosen for its visual weight next to the bold numbers).
    // A one-sided divergence shows its lone count.
    let divergence = |mut out: Vec<Span<'static>>| {
        if let (Some(a), Some(b)) = (state.ahead, state.behind) {
            let text = match (a > 0, b > 0) {
                (true, true) => format!(" {a} \u{f0bcd} {b}"),
                (true, false) => format!(" {a}"),
                (false, true) => format!(" {b}"),
                (false, false) => String::new(),
            };
            if !text.is_empty() {
                out.push(Span::styled(text, on_pill(Color::Magenta)));
            }
        }
        out
    };
    if let Some(name) = &state.name {
        right.push(pill_cap(CAP_LEFT, Color::Magenta));
        right.push(Span::styled(
            format!(" \u{f418} {name} "),
            on_pill(Color::Magenta),
        ));
        right = divergence(right);
        // The source's purple flows into the origin's mode color.
        right.push(Span::styled(
            CAP_RIGHT,
            Style::default().fg(Color::Magenta).bg(mode_bg),
        ));
    } else if state.unpushed.is_some() || state.base.is_some() {
        // Detached HEAD: a source part without a branch name.
        right.push(pill_cap(CAP_LEFT, Color::Magenta));
        right.push(Span::styled(" (detached) ", on_pill(Color::Magenta)));
        right = divergence(right);
        right.push(Span::styled(
            CAP_RIGHT,
            Style::default().fg(Color::Magenta).bg(mode_bg),
        ));
    } else {
        // No branch state yet (initial frame): the origin part alone.
        right.push(pill_cap(CAP_LEFT, mode_bg));
    }
    right.push(Span::styled(format!(" {label} "), on_pill(mode_bg)));
    right.push(pill_cap(CAP_RIGHT, mode_bg));
    let right = Line::from(right);

    draw_aligned(f, area.x, area.y, area.width, &left, Some(&right));
}

/// The bar's base style: plain text on the DarkGray bar background.
fn bar_style() -> Style {
    Style::default().fg(Color::DarkGray).bg(Color::DarkGray)
}

/// Powerline rounded chip caps (nerd font): thick half-circles — the left
/// cap's rounded edge faces left (U+E0B6), the right cap's faces right
/// (U+E0B4). Not U+E0B7/E0B5: those are the thin arc variants, decorative
/// crescents rather than filled caps.
const CAP_LEFT: &str = "\u{e0b6}";
const CAP_RIGHT: &str = "\u{e0b4}";

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
        // The tree pane's header occupies row 0; the first list row is row 1.
        assert_eq!(buf.cell((54, 1)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));

        app.focus = Focus::Diff;
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 1)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn branch_footer_renders() {
        let mut snap = sample_snapshot(crate::tree::Tree::build(vec![('M', "x.rs".to_string())]));
        Arc::get_mut(&mut snap).unwrap().branch_state = crate::git::BranchState {
            name: Some("feature/x".into()),
            base: Some("main".into()),
            ahead: Some(3),
            behind: Some(1),
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
        // The bar's left side carries only the remote-sync chip; the
        // branch's divergence moved into the pill.
        let text: String = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(text.contains("2 unpushed"), "row: {text}");
        assert!(!text.contains("ahead"), "row: {text}");
        // The two-part source-vs-origin pill is anchored at the far right:
        // a purple source part and the mode-colored origin part (blue =
        // Branch), closed by rounded caps in their fill colors.
        assert_eq!(buf.cell((79, 23)).unwrap().symbol(), "\u{e0b4}");
        assert_eq!(buf.cell((79, 23)).unwrap().fg, Color::Blue);
        assert_eq!(buf.cell((79, 23)).unwrap().bg, Color::DarkGray);
        // The pill reads `feature/x` with its divergence, before `vs main`:
        // source-side count, switch glyph, origin-side count.
        assert!(text.contains("feature/x"), "row: {text}");
        assert!(text.contains("3 \u{f0bcd} 1"), "row: {text}");
        // The divergence is bold white on purple, like the rest of the
        // source part.
        let switch = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap())
            .find(|c| c.symbol() == "\u{f0bcd}")
            .expect("double arrow in the pill");
        assert_eq!(switch.fg, Color::White);
        assert_eq!(switch.bg, Color::Magenta);
        assert!(text.contains("vs main"), "row: {text}");
        assert!(
            text.find("feature/x").unwrap() < text.find("vs main").unwrap(),
            "source before origin: {text}"
        );
        assert_eq!(buf.cell((20, 23)).unwrap().bg, Color::DarkGray);
    }

    #[test]
    fn footer_shows_diff_statistics() {
        let mut tree = crate::tree::Tree::build(vec![
            ('M', "a.rs".to_string()),
            ('M', "b.rs".to_string()),
            ('A', "c.rs".to_string()),
            ('D', "d.rs".to_string()),
        ]);
        let mut stats = std::collections::HashMap::new();
        stats.insert("a.rs".to_string(), (3, 1));
        stats.insert("b.rs".to_string(), (2, 0));
        tree.annotate_stats(&stats);
        let mut snap = sample_snapshot(tree);
        std::sync::Arc::get_mut(&mut snap)
            .unwrap()
            .uncommitted_stats = Some((2, 1));
        let mut app = App::new(
            snap,
            crossbeam_channel::bounded::<crate::snapshot::Request>(4).0,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // The shape of the change lives in the tree pane's header (x =
        // 67..100 on a 100-col terminal), directly above the list it
        // describes.
        let header: String = (67..100)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(header.contains("4 files"), "header: {header}");
        assert!(header.contains("2M 1A 1D"), "header: {header}");
        // The magnitude lives on the full-width status bar.
        let bar: String = (0..100)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(bar.contains("+5"), "bar: {bar}");
        assert!(bar.contains("-1"), "bar: {bar}");
        assert!(bar.contains("\u{f040} +2 -1"), "bar: {bar}");
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
