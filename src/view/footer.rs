//! Footer (status bar) rendering: the powerline row spanning the terminal
//! width, carrying state (unpushed counts, diff totals, uncommitted split)
//! on the left and the identity pill (source vs origin, plus the worktree
//! pill) on the right.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{App, FooterZone, Mode, Origin};
use crate::text::spans_width;

use super::tree::draw_aligned;

/// Powerline rounded chip caps, shared with the tree pane's header strip.
const CAP_LEFT: &str = crate::theme::CAP_LEFT;
const CAP_RIGHT: &str = crate::theme::CAP_RIGHT;

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
pub fn render_footer(f: &mut Frame, app: &mut App, area: Rect, message: Option<&str>) {
    // Rebuilt from scratch every frame so a hidden pill leaves no stale hitbox.
    app.mouse.footer_zones.clear();
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
    // each reads as its own chip. While the worker is busy, a `scanning…`
    // chip leads the stats side so live recomputes are visible.
    let mut left: Vec<Span<'static>> = Vec::new();
    let mut first = true;
    if app.is_busy() {
        left.extend(chip("scanning\u{2026}", Color::Yellow));
        first = false;
    }
    for (text, bg, fg) in &segments {
        if !first {
            left.push(Span::styled(" ", bar_style()));
        }
        // The segments use a variable fg (Black on Green, White on Red for
        // contrast) and no bold, so they do not fit `chip`'s bold-black
        // convention; keep their explicit three-span form here.
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
        first = false;
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
    // backgrounds with bold black text — the fills are the recognition
    // signal. The source is always purple; the origin part carries the
    // mode's color (blue default origin, magenta picked, green
    // `Uncommitted`), mirroring vim's mode colors.
    let (mode_bg, label) = match &app.mode {
        Mode::Uncommitted => (Color::Green, "\u{f040} uncommitted".to_string()),
        Mode::Branch(Origin::Default) => (Color::Blue, app.mode_label()),
        Mode::Branch(Origin::Picked(_)) => (Color::Magenta, app.mode_label()),
    };
    // Bold black reads on every one of the pill's colored fills.
    let on_pill = |bg: Color| {
        Style::default()
            .fg(Color::Black)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    };
    let pill_cap = |end: &'static str, color: Color| {
        Span::styled(end, Style::default().fg(color).bg(Color::DarkGray))
    };
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
    let mut source: Vec<Span<'static>> = Vec::new();
    if let Some(name) = &state.name {
        source.push(pill_cap(CAP_LEFT, Color::Magenta));
        source.push(Span::styled(
            format!(" \u{f418} {name} "),
            on_pill(Color::Magenta),
        ));
        source = divergence(source);
        // The source's purple flows into the origin's mode color.
        source.push(Span::styled(
            CAP_RIGHT,
            Style::default().fg(Color::Magenta).bg(mode_bg),
        ));
    } else if state.unpushed.is_some() || state.base.is_some() {
        // Detached HEAD: a source part without a branch name.
        source.push(pill_cap(CAP_LEFT, Color::Magenta));
        source.push(Span::styled(" (detached) ", on_pill(Color::Magenta)));
        source = divergence(source);
        source.push(Span::styled(
            CAP_RIGHT,
            Style::default().fg(Color::Magenta).bg(mode_bg),
        ));
    }

    let mut mode: Vec<Span<'static>> = Vec::new();
    if source.is_empty() {
        // No branch state yet (initial frame): the origin part alone.
        mode.push(pill_cap(CAP_LEFT, mode_bg));
    }
    mode.push(Span::styled(format!(" {label} "), on_pill(mode_bg)));
    mode.push(pill_cap(CAP_RIGHT, mode_bg));

    // The worktree pill, left of the identity pill and only when a linked
    // worktree is being viewed (`None` = main checkout, no pill). Cyan —
    // unused by the other pills — marks "where am I".
    let mut worktree: Vec<Span<'static>> = Vec::new();
    if let Some(id) = &app.current_worktree {
        worktree.push(pill_cap(CAP_LEFT, Color::Cyan));
        worktree.push(Span::styled(
            format!(" worktree:{id} "),
            on_pill(Color::Cyan),
        ));
        worktree.push(pill_cap(CAP_RIGHT, Color::Cyan));
        worktree.push(Span::styled(" ", bar_style()));
    }

    app.mouse.footer_zones = footer_pill_zones(area, &worktree, &source, &mode);

    let right = Line::from(
        worktree
            .into_iter()
            .chain(source)
            .chain(mode)
            .collect::<Vec<_>>(),
    );
    draw_aligned(f, area.x, area.y, area.width, &left, Some(&right));
}

/// The bar's base style: plain text on the DarkGray bar background.
/// Shared with the tree pane's header strip, which paints the same bar.
pub(super) fn bar_style() -> Style {
    Style::default().fg(Color::DarkGray).bg(Color::DarkGray)
}

/// A rounded-cap chip: `CAP_LEFT` (cap color on the bar) + bold black text on
/// `bg` + `CAP_RIGHT` (cap color on the bar). Shared by the footer's busy
/// chip and the tree header's status pills, which both use bold black text
/// on a colored fill. The segments loop keeps its own three-span form
/// because its delete chip uses White-on-Red for contrast (no bold).
pub(super) fn chip(text: &str, bg: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled(CAP_LEFT, Style::default().fg(bg).bg(Color::DarkGray)),
        Span::styled(
            format!(" {text} "),
            Style::default()
                .fg(Color::Black)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(CAP_RIGHT, Style::default().fg(bg).bg(Color::DarkGray)),
    ]
}

/// Saturating `usize → u16` cast: the `u16::try_from(..).unwrap_or(u16::MAX)`
/// idiom, named once for the footer's zone measurement.
fn sat_u16(w: usize) -> u16 {
    u16::try_from(w).unwrap_or(u16::MAX)
}

/// Hitboxes for the footer's right pill row, one per part in draw order.
/// `worktree` precedes `source` precedes `mode`; any may be empty. The pill
/// is right-aligned like [`draw_aligned`], so when it overflows the pane only
/// its leading columns survive: zones clip to `area`, and a fully-clipped
/// part is dropped.
fn footer_pill_zones(
    area: Rect,
    worktree: &[Span],
    source: &[Span],
    mode: &[Span],
) -> Vec<(FooterZone, Rect)> {
    if area.height == 0 {
        return Vec::new();
    }
    let wt_w = spans_width(worktree);
    let source_w = spans_width(source);
    let mode_w = spans_width(mode);
    let total = wt_w + source_w + mode_w;
    if total == 0 {
        return Vec::new();
    }
    let drawn = sat_u16(total.min(area.width as usize));
    let edge = area.x.saturating_add(area.width);
    let x = area.x + area.width.saturating_sub(drawn);

    let mut zones = Vec::new();
    let mut push = |zone: FooterZone, x0: u16, w: usize| {
        let end = x0.saturating_add(sat_u16(w));
        if x0 >= edge || end <= x0 {
            return;
        }
        zones.push((
            zone,
            Rect::new(x0, area.y, end.min(edge).saturating_sub(x0), 1),
        ));
    };
    let mut offset = x;
    if wt_w > 0 {
        push(FooterZone::Worktree, offset, wt_w);
        offset = offset.saturating_add(sat_u16(wt_w));
    }
    if source_w > 0 {
        push(FooterZone::Source, offset, source_w);
        offset = offset.saturating_add(sat_u16(source_w));
    }
    if mode_w > 0 {
        push(FooterZone::Mode, offset, mode_w);
    }
    zones
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::test_support::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    /// A snapshot carrying a `feature/x` branch tracking `main`, ready for
    /// footer/pill assertions.
    fn branch_snapshot() -> crate::snapshot::ArcSnapshot {
        let mut snap = sample_snapshot(crate::tree::Tree::build(vec![('M', "x.rs".to_string())]));
        Arc::get_mut(&mut snap).unwrap().branch_state = crate::git::BranchState {
            name: Some("feature/x".into()),
            base: Some("main".into()),
            ahead: Some(3),
            behind: Some(1),
            upstream: Some("origin/feature/x".into()),
            unpushed: Some(2),
        };
        snap
    }

    /// An `App` over [`branch_snapshot`].
    fn branch_app() -> App {
        App::new(
            branch_snapshot(),
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
    fn branch_footer_renders() {
        let mut app = branch_app();

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
        // The divergence is bold black on purple, like the rest of the
        // source part.
        let switch = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap())
            .find(|c| c.symbol() == "\u{f0bcd}")
            .expect("double arrow in the pill");
        assert_eq!(switch.fg, Color::Black);
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
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        // Each status is a compact pill, so the parts are no longer a
        // contiguous plain-text run.
        assert!(header.contains("2M"), "header: {header}");
        assert!(header.contains("1A"), "header: {header}");
        assert!(header.contains("1D"), "header: {header}");
        // A pill's closing cap carries its status color on the strip.
        let modified_color = crate::theme::status_color(crate::diff::FileStatus::Modified);
        assert!(
            (67..100).any(|x| {
                let c = buf.cell((x, 0)).unwrap();
                c.symbol() == crate::theme::CAP_RIGHT && c.fg == modified_color
            }),
            "no modified-colored cap in header: {header}"
        );
        // The magnitude lives on the full-width status bar.
        let bar: String = (0..100)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(bar.contains("+5"), "bar: {bar}");
        assert!(bar.contains("-1"), "bar: {bar}");
        assert!(bar.contains("\u{f040} +2 -1"), "bar: {bar}");
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

    #[test]
    fn footer_shows_scanning_chip_when_busy() {
        let mut app = app_with_busy(
            crate::tree::Tree::build(vec![('M', "x.rs".to_string())]),
            true,
        );
        let buf = render(&mut app);
        let bar: String = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(bar.contains("scanning…"), "bar: {bar}");
        // The chip leads the stats side, before any `+N`/`-N` chip.
        assert!(bar.find("scanning…").unwrap() < 6, "bar: {bar}");
    }

    #[test]
    fn footer_omits_scanning_chip_when_settled() {
        let mut app = app_with_busy(
            crate::tree::Tree::build(vec![('M', "x.rs".to_string())]),
            false,
        );
        let buf = render(&mut app);
        let bar: String = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(!bar.contains("scanning"), "bar: {bar}");
    }

    #[test]
    fn footer_records_pill_zones() {
        let mut app = branch_app();
        render(&mut app);
        let zones = &app.mouse.footer_zones;
        assert_eq!(zones.len(), 2, "{zones:?}");
        let source = zones
            .iter()
            .find(|(z, _)| *z == FooterZone::Source)
            .expect("source zone")
            .1;
        let mode = zones
            .iter()
            .find(|(z, _)| *z == FooterZone::Mode)
            .expect("mode zone")
            .1;

        assert_eq!(source.y, 23);
        assert_eq!(mode.y, 23);
        assert_eq!(source.height, 1);
        assert_eq!(mode.height, 1);
        // Source sits left of Mode with no overlap, both inside the terminal.
        assert!(source.x + source.width <= mode.x, "{zones:?}");
        assert!(mode.x + mode.width <= 80, "{zones:?}");
        // Mode = leading space + label + trailing space + closing cap, in
        // display cells (`Line::width`), not `chars()` — wide glyphs diverge.
        let expected = Line::from(vec![
            Span::raw(format!(" {} ", app.mode_label())),
            Span::raw(CAP_RIGHT),
        ])
        .width();
        assert_eq!(mode.width as usize, expected, "{zones:?}");
    }

    #[test]
    fn footer_shows_worktree_pill_only_when_linked() {
        let mut app = branch_app();
        let buf = render(&mut app);
        let bar: String = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(
            !bar.contains("worktree:"),
            "main checkout has no pill: {bar}"
        );
        assert!(
            !app.mouse
                .footer_zones
                .iter()
                .any(|(z, _)| *z == FooterZone::Worktree),
            "{:?}",
            app.mouse.footer_zones
        );

        app.set_current_worktree(Some("wt-b".into()));
        let buf = render(&mut app);
        let bar: String = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(
            bar.contains("worktree:wt-b"),
            "pill names the worktree: {bar}"
        );
        // The pill sits left of the identity pill, in draw order.
        assert!(
            bar.find("worktree:wt-b").unwrap() < bar.find("feature/x").unwrap(),
            "worktree pill precedes the source pill: {bar}"
        );
        let zones = &app.mouse.footer_zones;
        let wt = zones
            .iter()
            .find(|(z, _)| *z == FooterZone::Worktree)
            .expect("worktree zone")
            .1;
        let source = zones
            .iter()
            .find(|(z, _)| *z == FooterZone::Source)
            .expect("source zone")
            .1;
        assert!(wt.x + wt.width <= source.x, "{zones:?}");
    }

    #[test]
    fn footer_zones_empty_when_footer_does_not_draw() {
        let mut app = branch_app();
        // Below the footer's own 8-column minimum: nothing draws, no hitboxes.
        let mut terminal = Terminal::new(TestBackend::new(7, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        assert!(app.mouse.footer_zones.is_empty());
    }

    #[test]
    fn footer_zones_clip_when_pill_overflows() {
        let mut app = branch_app();
        // Pill wider than the pane: it draws from the left edge and clips, so
        // the mode part falls off-screen and only a clipped source survives.
        let mut terminal = Terminal::new(TestBackend::new(12, 24)).unwrap();
        terminal.draw(|f| super::super::ui(f, &mut app)).unwrap();
        let zones = &app.mouse.footer_zones;
        assert_eq!(zones.len(), 1, "{zones:?}");
        assert_eq!(zones[0].0, FooterZone::Source);
        assert_eq!(zones[0].1, Rect::new(0, 23, 12, 1));
    }

    #[test]
    fn footer_zones_record_with_busy_and_message() {
        let mut snap = branch_snapshot();
        {
            let s = Arc::get_mut(&mut snap).unwrap();
            s.busy = true;
            s.error = Some("boom".into());
        }
        let mut app = App::new(
            snap,
            crossbeam_channel::bounded::<crate::snapshot::Request>(4).0,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let buf = render(&mut app);
        assert_eq!(app.mouse.footer_zones.len(), 2, "pill draws while busy");
        let bar: String = (0..80)
            .map(|x| buf.cell((x, 23)).unwrap().symbol().to_string())
            .collect();
        assert!(bar.contains("boom"), "bar: {bar}");
    }

    #[test]
    fn chip_is_bold_black_on_fill_with_caps() {
        let spans = chip("hi", Color::Yellow);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), CAP_LEFT);
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(spans[0].style.bg, Some(Color::DarkGray));
        assert_eq!(spans[1].content.as_ref(), " hi ");
        assert_eq!(spans[1].style.fg, Some(Color::Black));
        assert_eq!(spans[1].style.bg, Some(Color::Yellow));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content.as_ref(), CAP_RIGHT);
    }
}
