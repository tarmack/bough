//! Diff-pane rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{App, HeaderPositions};
use crate::diff::{
    Diff, DiffLine, FileHeader, FileStatus, HunkMeta, LineKind, SectionKind, WordRun,
};
use crate::highlight::TokenOwned;
use crate::text::{elide, max_scroll_for, render_centered};
use crate::theme;
use crate::view::tree::file_icon;

/// Render the diff pane: the locked header, optional rename line, and the
/// scrollable body (sticky pinned hunk header + hunk rows).
pub fn render_diff(f: &mut Frame, app: &mut App, area: Rect) {
    // Click-row mapping must never point at a previous diff's headers; reset
    // before any early return (loading/error/empty/preview).
    app.sticky_file = None;
    app.sticky_hunk = None;

    // No file selected: placeholder, no locked header.
    if app.current_file().is_none() {
        let style = Style::default().fg(Color::DarkGray);
        render_centered(f, area, "select a file to preview its diff", style);
        return;
    }
    let Some(diff) = app.snapshot.selected_diff.as_ref() else {
        // No diff yet and no recorded error: the snapshot is still loading, so
        // render a neutral placeholder rather than mislabel it as a failure.
        let Some(err) = app.diff_error() else {
            let style = Style::default().fg(Color::DarkGray);
            render_centered(f, area, "loading…", style);
            return;
        };
        let line = Line::styled(format!("error: {err}"), Style::default().fg(Color::Red));
        f.buffer_mut().set_line(area.x, area.y, &line, area.width);
        return;
    };

    let multi = diff.files.len() > 1;
    let has_rename = !multi && diff.header.old_path.is_some();
    let header_height = 1 + has_rename as u16;
    app.last_header_height = header_height;

    let body_height = area.height.saturating_sub(header_height);
    // Degenerate pane (header fills it): nothing below the header to draw.
    if body_height == 0 {
        return;
    }

    let visible = app.visible_line_indices();
    let headers = app.header_positions();
    // Clamp in two passes: the sticky count depends on the scroll position,
    // which the first clamp may have moved.
    let sticky_rows = app.sticky_row_count();
    let max_scroll = max_scroll_for(visible.len(), body_height as usize, sticky_rows);
    if app.diff_scroll > max_scroll {
        app.diff_scroll = max_scroll;
    }
    let (file_sticky, hunk_sticky, flow_height) =
        sticky_headers(&visible, &headers, app.diff_scroll, body_height);
    let actual_rows = usize::from(file_sticky.is_some()) + usize::from(hunk_sticky.is_some());
    let (file_sticky, hunk_sticky, flow_height) = if actual_rows != sticky_rows {
        let max_scroll = max_scroll_for(visible.len(), body_height as usize, actual_rows);
        if app.diff_scroll > max_scroll {
            app.diff_scroll = max_scroll;
        }
        sticky_headers(&visible, &headers, app.diff_scroll, body_height)
    } else {
        (file_sticky, hunk_sticky, flow_height)
    };
    // Pinned sticky lines, for click-row mapping.
    app.sticky_file = file_sticky;
    app.sticky_hunk = hunk_sticky;

    let active = app.active_chunk;
    let current = active.map(|s| s + 1).unwrap_or(0);

    // Locked header: the single file's header, or a folder summary carrying
    // the path and total file count. Line 1 always, line 2 rename. The rows
    // form a DarkGray rail (matching the tree pane's header strip), with the
    // last column of each reserved for a rounded cap.
    f.buffer_mut().set_style(
        Rect::new(area.x, area.y, area.width, header_height),
        Style::default().bg(Color::DarkGray),
    );
    let header_w = area.width.saturating_sub(1) as usize;
    let header_spans = if multi {
        build_summary_header(diff, header_w)
    } else {
        build_header_line(&diff.header, current, header_w)
    };
    draw_locked_header_row(f, app, area, area.y, header_spans);
    let mut y = area.y + 1;
    if has_rename {
        let rename = build_rename_line(&diff.header);
        draw_locked_header_row(f, app, area, y, rename);
        y += 1;
    }

    // Empty body: centered messages below the header.
    if diff.lines.is_empty() {
        let msg = if diff.header.binary {
            "binary file"
        } else if !multi && diff.header.status == FileStatus::Renamed {
            "rename — no content changes"
        } else {
            "no content changes"
        };
        let body = Rect::new(
            area.x,
            y,
            area.width,
            area.height.saturating_sub(header_height),
        );
        let style = Style::default().fg(Color::DarkGray);
        render_centered(f, body, msg, style);
        return;
    }

    // Render pinned sticky rows, then the visible flow rows below them. Guard
    // each sticky write against running past the pane bottom (ratatui's
    // `set_line` panics otherwise).
    let bottom = area.y.saturating_add(area.height);
    if let Some(idx) = file_sticky {
        if y < bottom {
            y = render_section_header(f, app, area, y, idx, diff);
        }
    }
    if let Some(idx) = hunk_sticky {
        if y < bottom {
            y = render_section_header(f, app, area, y, idx, diff);
        }
    }
    let start = app.diff_scroll;
    let end = (start + flow_height as usize).min(visible.len());
    emit_visible_rows(f, app, area, y, diff, &visible[start..end]);
}

/// Draw one locked-header row: elide the spans into every column but the
/// last, then close the DarkGray rail with a rounded cap whose fingers flow
/// out into the terminal background.
fn draw_locked_header_row(f: &mut Frame, app: &App, area: Rect, y: u16, spans: Vec<Span<'static>>) {
    let w = area.width.saturating_sub(1);
    f.buffer_mut()
        .set_line(area.x, y, &elide(spans, app.diff_col, w, None), w);
    if area.width > 0 {
        let cap = Span::styled(
            theme::CAP_RIGHT,
            Style::default().fg(Color::DarkGray).bg(Color::Reset),
        );
        f.buffer_mut()
            .set_line(area.x + area.width - 1, y, &Line::from(cap), 1);
    }
}

/// Compute the sticky file/hunk header layout for the body. Returns the two
/// sticky indices plus the flow height left below them. The caller writes
/// `app.sticky_file`/`app.sticky_hunk` (the pinned sticky lines, for click-row
/// mapping).
///
/// `headers` holds the precomputed visible-list positions of the section
/// headers, so the lookups are O(log n) rather than scanning back from `start`.
/// The hunk sticky is confined to the pinned file's own lines, so scrolling
/// into a new file replaces the previous file's hunk rather than pinning it
/// under the new header. Rows are capped by `body_height`, preferring the file
/// sticky over the hunk sticky.
fn sticky_headers(
    visible: &[usize],
    headers: &HeaderPositions,
    start: usize,
    body_height: u16,
) -> (Option<usize>, Option<usize>, u16) {
    let start = start.min(visible.len());
    let (file_pos, hunk_pos) = headers.sticky_pair(start);
    let file_sticky = file_pos.map(|p| visible[p]);
    let hunk_sticky = hunk_pos.map(|p| visible[p]);

    // Cap pinned rows: at most one file sticky, and the hunk sticky only when
    // a row remains for it.
    let mut file_sticky = file_sticky;
    let mut hunk_sticky = hunk_sticky;
    if body_height == 0 {
        file_sticky = None;
        hunk_sticky = None;
    } else if body_height < 2 && file_sticky.is_some() {
        hunk_sticky = None;
    }
    let sticky_rows =
        file_sticky.map(|_| 1u16).unwrap_or(0) + hunk_sticky.map(|_| 1u16).unwrap_or(0);
    (
        file_sticky,
        hunk_sticky,
        body_height.saturating_sub(sticky_rows),
    )
}

/// Render the visible flow rows (`rows`), drawing section headers via
/// `render_section_header` and body lines via `diff_body_line`. Returns the
/// `y` below the last rendered row.
fn emit_visible_rows(
    f: &mut Frame,
    app: &App,
    area: Rect,
    mut y: u16,
    diff: &Diff,
    rows: &[usize],
) -> u16 {
    for &line_idx in rows {
        let line = &diff.lines[line_idx];
        if line.kind.is_header() {
            y = render_section_header(f, app, area, y, line_idx, diff);
        } else {
            let base_bg = theme::bg_for(line.kind);
            let tokens = app
                .highlighted()
                .get(line_idx)
                .and_then(|opt| opt.as_deref())
                .unwrap_or(&[]);
            let spans = diff_body_line(tokens, line, base_bg, app.word_diff_on);
            let l = elide(spans, app.diff_col, area.width, None);
            f.buffer_mut().set_line(area.x, y, &l, area.width);
            y += 1;
        }
    }
    y
}

/// Locked header for a multi-file (folder) diff: `<path>  N files` with the
/// aggregate diffstat right-aligned.
fn build_summary_header(diff: &Diff, width: usize) -> Vec<Span<'static>> {
    // The folder's icon tab in the tree's dark directory tint, then the path
    // and file count.
    let mut left = icon_chip(
        file_icon(&diff.header.path),
        theme::DIR_FG,
        Color::DarkGray,
        0,
    );
    left.push(Span::styled(" ", Style::default()));
    left.push(Span::styled(
        diff.header.path.clone(),
        Style::default().fg(Color::White),
    ));
    left.push(Span::raw("  "));
    left.push(Span::styled(
        format!("{} files", diff.files.len()),
        Style::default().fg(Color::Gray),
    ));
    let right = stat_spans(diff.header.added, diff.header.deleted, false, None);
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    right_align(&mut left, right_w, width);
    left.extend(right);
    left
}

/// Locked header line 1: `<path>  +A -D  <status>  [cur/total]`, plus a
/// right-aligned mode change (`old → new`, octal) when the modes differ.
fn build_header_line(header: &FileHeader, current_hunk: usize, width: usize) -> Vec<Span<'static>> {
    // Left content: the file's icon tab in its status tint, then the path
    // and hunk position.
    let mut left = icon_chip(
        file_icon(&header.path),
        theme::status_color(header.status),
        Color::DarkGray,
        0,
    );
    left.push(Span::styled(" ", Style::default()));
    left.push(Span::styled(
        header.path.clone(),
        Style::default().fg(Color::White),
    ));
    if !header.hunks.is_empty() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            format!("[{current_hunk}/{}]", header.hunks.len()),
            Style::default().fg(Color::Gray),
        ));
    }

    // Right block: optional mode change, then the diff stat at the far right
    // so it aligns with the chunk-header diffstat below. `binary` moves with
    // the diff stat.
    let mut right: Vec<Span<'static>> = Vec::new();
    if let (Some(old), Some(new)) = (header.mode_old, header.mode_new) {
        if old != new {
            right.push(mode_change_span(old, new, None));
            right.push(Span::raw("  "));
        }
    }
    right.extend(stat_spans(
        header.added,
        header.deleted,
        header.binary,
        None,
    ));

    let right_w: usize = right.iter().map(|s| s.width()).sum();
    // Right-align the block when there is room (at least one space of pad);
    // otherwise append it and let elide truncate the overflow.
    right_align(&mut left, right_w, width);
    left.extend(right);
    left
}

/// Locked header line 2: rename `old → new  R <percent>%`.
fn build_rename_line(header: &FileHeader) -> Vec<Span<'static>> {
    let old = header.old_path.clone().unwrap_or_default();
    let mut spans = vec![
        Span::styled(old, Style::default().fg(Color::Gray)),
        Span::styled(" → ", Style::default().fg(Color::Gray)),
        Span::styled(header.path.clone(), Style::default().fg(Color::Gray)),
    ];
    if let Some(p) = header.rename_percent {
        spans.push(Span::styled(
            format!("  R {p}%"),
            Style::default().fg(Color::Magenta),
        ));
    }
    spans
}

/// The `old → new` mode change (octal) span. On the locked header's DarkGray
/// rail (`bg = None`) it reads `Gray`; over a section strip (`Some(bg)`) it
/// stays `DarkGray`. Caller handles surrounding spacing.
fn mode_change_span(old: u32, new: u32, bg: Option<Color>) -> Span<'static> {
    let style = match bg {
        Some(c) => Style::default().fg(Color::DarkGray).bg(c),
        None => Style::default().fg(Color::Gray),
    };
    Span::styled(format!("{old:o} → {new:o}"), style)
}

/// The `+A -D` diffstat spans (green add / red delete), or a single `binary`
/// span when the file is binary. `bg` tints the spans for a header row with a
/// painted background; `None` leaves them on the plain locked-header style.
fn stat_spans(added: usize, deleted: usize, binary: bool, bg: Option<Color>) -> Vec<Span<'static>> {
    if binary {
        let style = match bg {
            Some(c) => Style::default().fg(Color::DarkGray).bg(c),
            None => Style::default().fg(Color::DarkGray),
        };
        return vec![Span::styled("binary", style)];
    }
    let add_style = match bg {
        Some(c) => Style::default().fg(Color::Green).bg(c),
        None => Style::default().fg(Color::Green),
    };
    let del_style = match bg {
        Some(c) => Style::default().fg(Color::Red).bg(c),
        None => Style::default().fg(Color::Red),
    };
    vec![
        Span::styled(format!("+{added}"), add_style),
        Span::styled(format!(" -{deleted}"), del_style),
    ]
}

/// Right-align a `right` block against a `left` block over `width` columns:
/// pad when there is at least one space of room, otherwise leave `left` alone
/// and let the caller's elide truncate the overflow.
fn right_align(left: &mut Vec<Span<'static>>, right_w: usize, width: usize) {
    let left_w: usize = left.iter().map(|s| s.width()).sum();
    if width >= left_w + 1 + right_w {
        left.push(Span::raw(" ".repeat(width - left_w - right_w)));
    }
}

/// Compose a body line's spans: syntax tokens tinted with the line's
/// background, word-level runs overlaid when enabled. Caller elides.
fn diff_body_line(
    tokens: &[TokenOwned],
    line: &DiffLine,
    base_bg: Color,
    word_diff_on: bool,
) -> Vec<Span<'static>> {
    if !(word_diff_on && line.word_runs.is_some()) {
        return spans_from_tokens(tokens, &line.raw_text, base_bg);
    }
    let runs = line.word_runs.as_deref().unwrap_or(&[]);
    overlay_word_runs(&line.raw_text, tokens, runs, line.kind, base_bg)
}

/// Convert syntax tokens to spans with a uniform background. If no grammar
/// matched (empty tokens), render the raw text as a single uncolored span.
fn spans_from_tokens(tokens: &[TokenOwned], raw: &str, bg: Color) -> Vec<Span<'static>> {
    if tokens.is_empty() {
        return vec![Span::styled(raw.to_string(), Style::default().bg(bg))];
    }
    tokens
        .iter()
        .map(|t| Span::styled(t.text.clone(), Style::default().fg(t.color).bg(bg)))
        .collect()
}

/// Overlay word-level run backgrounds on top of syntax tokens.
///
/// The kept runs (context runs plus runs matching this line's own kind) tile
/// the line's `raw_text` exactly, as do the syntax tokens, so we merge them by
/// byte offset: split tokens at run boundaries and repaint each segment's
/// background. Runs from the paired opposite line (e.g. Added runs on a
/// Removed line) are skipped — they are not part of this line's text.
fn overlay_word_runs(
    raw: &str,
    tokens: &[TokenOwned],
    runs: &[WordRun],
    line_kind: LineKind,
    base_bg: Color,
) -> Vec<Span<'static>> {
    let owned;
    let tokens = if tokens.is_empty() {
        owned = vec![TokenOwned {
            text: raw.to_string(),
            color: Color::Reset,
        }];
        owned.as_slice()
    } else {
        tokens
    };

    // Byte spans of the runs that belong to this line, with their bg color.
    // The shared run vector holds runs from both the removed and added sides;
    // advance the offset only over runs this line actually contains (Context or
    // its own kind), so the kept spans tile `raw` exactly. Counting the skipped
    // opposite-side runs would push the byte offsets past the end of `raw`.
    let mut kept: Vec<(usize, usize, Color)> = Vec::new();
    let mut run_pos = 0usize;
    for run in runs {
        if run.kind == LineKind::Context || run.kind == line_kind {
            let rs = run_pos;
            let bg = if run.changed && run.kind == line_kind {
                theme::run_bg(run.kind)
            } else {
                base_bg
            };
            run_pos += run.text.len();
            kept.push((rs, run_pos, bg));
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut tok_start = 0usize;
    let mut ti = 0usize;
    for &(rs, re, bg) in &kept {
        let mut seg = rs;
        while seg < re {
            while ti + 1 < tokens.len() && tok_start + tokens[ti].text.len() <= seg {
                tok_start += tokens[ti].text.len();
                ti += 1;
            }
            if ti >= tokens.len() {
                // Tokens ran out before this kept run ended; render the rest of
                // the run with a fallback foreground so the line still tiles
                // `raw` exactly. Never indexes out of bounds.
                spans.push(Span::styled(
                    raw[seg..re].to_string(),
                    Style::default().fg(Color::Reset).bg(bg),
                ));
                seg = re;
                continue;
            }
            let te = tok_start + tokens[ti].text.len();
            let s = seg.max(tok_start);
            let e = re.min(te);
            if s >= e {
                tok_start = te;
                ti += 1;
                continue;
            }
            spans.push(Span::styled(
                raw[s..e].to_string(),
                Style::default().fg(tokens[ti].color).bg(bg),
            ));
            seg = e;
            if e == te {
                tok_start = te;
                ti += 1;
            }
        }
    }
    spans
}

/// Nerd Font glyph for a hunk (`@@`) header row — a block of changed lines.
const HUNK_ICON: &str = "\u{f0c9}";

/// The hunk icon glyph; scrolls horizontally. The tab anchors at column 0,
/// flush with the file and main headers, while the one-cell indent lives
/// inside the dark-blue fill — so hunk icons sit one cell further in than
/// file-header icons without the tab itself being offset.
fn hunk_name_spans(bg: Color) -> Vec<Span<'static>> {
    icon_chip(HUNK_ICON, Color::Blue, bg, 1)
}

/// The `+A -D` diffstat, a spacer, and the fold arrow. Pinned to the pane's
/// right edge (does not scroll).
fn hunk_stat_spans(meta: &HunkMeta, bg: Color, collapsed: bool) -> Vec<Span<'static>> {
    let glyph = if collapsed { "▶" } else { "▼" };
    let mut spans = stat_spans(meta.added, meta.deleted, false, Some(bg));
    spans.push(Span::styled(" ".to_string(), Style::default().bg(bg)));
    spans.push(Span::styled(
        glyph.to_string(),
        Style::default().fg(Color::Blue).bg(bg),
    ));
    spans
}

/// Content of a navigable section header: a hunk (`@@ …`) or a file header
/// inside a multi-file diff. Both render through the same layout — a name that
/// scrolls/elides on the left and a right-pinned diffstat + fold arrow.
enum HeaderContent<'a> {
    File { file: &'a FileHeader },
    Hunk { meta: &'a HunkMeta },
}

impl<'a> HeaderContent<'a> {
    fn name_spans(&self, bg: Color) -> Vec<Span<'static>> {
        match self {
            HeaderContent::File { file } => file_header_name_spans(file, bg),
            HeaderContent::Hunk { .. } => hunk_name_spans(bg),
        }
    }

    fn stat_spans(&self, bg: Color, collapsed: bool) -> Vec<Span<'static>> {
        match self {
            HeaderContent::File { file } => file_header_stat_spans(file, bg, collapsed),
            HeaderContent::Hunk { meta, .. } => hunk_stat_spans(meta, bg, collapsed),
        }
    }
}

/// Render one section header row. The name/path scrolls/elides on the left;
/// the diffstat and fold arrow stay pinned to the right edge. Returns the `y`
/// directly below the rendered row.
fn render_section_header(
    f: &mut Frame,
    app: &App,
    area: Rect,
    y: u16,
    line_idx: usize,
    diff: &Diff,
) -> u16 {
    let line = &diff.lines[line_idx];
    let collapsed = app.collapsed_hunks.contains(&line.section);
    let bg = theme::hunk_header_bg(app.active_chunk == Some(line.section), app.focus);

    match header_content(diff, line) {
        Some(content) => draw_section_header(f, app, area, y, bg, collapsed, &content),
        None => {
            let l = Line::styled(
                line.raw_text.clone(),
                Style::default().fg(Color::Blue).bg(bg),
            );
            f.buffer_mut().set_line(area.x, y, &l, area.width);
            y + 1
        }
    }
}

/// Resolve the content a header line's section describes.
fn header_content<'a>(diff: &'a Diff, line: &DiffLine) -> Option<HeaderContent<'a>> {
    let section = diff.sections.get(line.section)?;
    let file = diff.files.get(section.file)?;
    match section.kind {
        SectionKind::File => Some(HeaderContent::File { file }),
        SectionKind::Hunk => {
            let meta = file.hunks.get(section.hunk?)?;
            Some(HeaderContent::Hunk { meta })
        }
    }
}

/// Shared layout for any section header: elide the name, pad its background
/// to the name region, pin the diffstat + fold arrow flush against the cap
/// column, and close the strip's right edge with a rounded cap — leaving the
/// last column for it so the whole row reads as a rounded strip over the
/// terminal background.
fn draw_section_header(
    f: &mut Frame,
    app: &App,
    area: Rect,
    y: u16,
    bg: Color,
    collapsed: bool,
    content: &HeaderContent<'_>,
) -> u16 {
    let stat = content.stat_spans(bg, collapsed);
    let stat_w: u16 = stat.iter().map(|s| s.width()).sum::<usize>() as u16;
    // The strip (name region + diffstat + fold arrow) spans every column but
    // the last, which is reserved for the cap.
    let name_w = area.width.saturating_sub(stat_w).saturating_sub(1);
    let mut name_line = elide(
        content.name_spans(bg),
        app.diff_col,
        name_w.saturating_sub(1),
        Some(bg),
    );
    // Pad so the name's background spans the full name region (ending one cell
    // short of the diffstat).
    let used: usize = name_line.spans.iter().map(|s| s.width()).sum();
    let pad = (name_w as usize).saturating_sub(used);
    if pad > 0 {
        name_line
            .spans
            .push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    f.buffer_mut().set_line(area.x, y, &name_line, name_w);
    // Draw the stat even when the name region collapsed to zero width so a
    // too-narrow pane still shows the (truncated) diffstat instead of a blank
    // row. It ends flush against the cap column.
    let stat_w_avail = area.width.saturating_sub(name_w).saturating_sub(1);
    f.buffer_mut()
        .set_line(area.x + name_w, y, &Line::from(stat), stat_w_avail);
    // Close the strip's right edge: the cap's fg is the strip color, so the
    // strip flows out into the terminal background.
    if area.width > 0 {
        let cap = Span::styled(theme::CAP_RIGHT, Style::default().fg(bg).bg(Color::Reset));
        f.buffer_mut()
            .set_line(area.x + area.width - 1, y, &Line::from(cap), 1);
    }
    y + 1
}

/// Left span for a file-header row: the file-type icon, path, and a mode
/// change when the file's modes differ.
fn file_header_name_spans(file: &FileHeader, bg: Color) -> Vec<Span<'static>> {
    let mut spans = header_icon(file, theme::status_color(file.status), bg);
    spans.push(Span::styled(
        file.path.clone(),
        Style::default().fg(Color::DarkGray).bg(bg),
    ));
    if let (Some(old), Some(new)) = (file.mode_old, file.mode_new) {
        if old != new {
            spans.push(Span::styled("  ".to_string(), Style::default().bg(bg)));
            spans.push(mode_change_span(old, new, Some(bg)));
        }
    }
    spans
}

/// A leading icon tab: the glyph on a colored fill, closed by a rounded
/// right cap over the row's background — a tab, not a full pill (no left
/// cap), matching the footer's chip language. `chip_bg` carries the meaning
/// (file status color, structure blue, folder cyan); the glyph is bold
/// black, like the footer pill's text.
///
/// The tab always anchors at column 0. `indent` prepends spaces *inside* the
/// colored fill, so indented rows (hunk headers) read as padding within the
/// anchored tab rather than an offset before it.
fn icon_chip(icon: &str, chip_bg: Color, row_bg: Color, indent: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{} {icon} ", " ".repeat(indent)),
            Style::default()
                .fg(Color::Black)
                .bg(chip_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(theme::CAP_RIGHT, Style::default().fg(chip_bg).bg(row_bg)),
    ]
}

/// The `[icon] ` prefix of a header row: the file-type glyph as an icon tab
/// in the file's status color.
fn header_icon(file: &FileHeader, color: Color, bg: Color) -> Vec<Span<'static>> {
    icon_chip(file_icon(&file.path), color, bg, 0)
}

/// Right-pinned stat + fold arrow for a file-header row.
fn file_header_stat_spans(file: &FileHeader, bg: Color, collapsed: bool) -> Vec<Span<'static>> {
    let glyph = if collapsed { "▶" } else { "▼" };
    let mut spans = stat_spans(file.added, file.deleted, file.binary, Some(bg));
    spans.push(Span::styled(" ".to_string(), Style::default().bg(bg)));
    spans.push(Span::styled(
        glyph.to_string(),
        Style::default().fg(Color::Blue).bg(bg),
    ));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{
        Diff, DiffLine, FileHeader, FileStatus, HunkMeta, LineKind, Section, SectionKind,
    };
    use crate::test_support::*;
    use crate::theme::BG_HUNK;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn file_header(path: &str, added: usize, deleted: usize) -> FileHeader {
        FileHeader {
            path: path.into(),
            status: FileStatus::Modified,
            added,
            deleted,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: vec![HunkMeta { added, deleted }],
        }
    }

    #[test]
    fn renders_file_headers_in_folder_diff() {
        let fa = file_header("src/a.rs", 1, 0);
        let fb = file_header("src/b.rs", 2, 0);
        let diff = Diff {
            header: FileHeader {
                path: "src".into(),
                old_path: None,
                status: FileStatus::Modified,
                added: 3,
                deleted: 0,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: Vec::new(),
            },
            files: vec![fa.clone(), fb.clone()],
            sections: vec![
                Section {
                    kind: SectionKind::File,
                    file: 0,
                    hunk: None,
                },
                Section {
                    kind: SectionKind::Hunk,
                    file: 0,
                    hunk: Some(0),
                },
                Section {
                    kind: SectionKind::File,
                    file: 1,
                    hunk: None,
                },
                Section {
                    kind: SectionKind::Hunk,
                    file: 1,
                    hunk: Some(0),
                },
            ],
            lines: vec![
                DiffLine {
                    kind: LineKind::FileHeader,
                    raw_text: "src/a.rs".into(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    section: 1,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "a".into(),
                    section: 1,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::FileHeader,
                    raw_text: "src/b.rs".into(),
                    section: 2,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    section: 3,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "b".into(),
                    section: 3,
                    word_runs: None,
                },
            ],
        };
        let mut app = sample_app();
        app.selected_path = Some("src".into());
        app.snapshot = snapshot_with_diff(diff);

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("src"), "folder path in locked header: {text}");
        assert!(
            text.contains("2 files"),
            "file count in locked header: {text}"
        );
        assert!(text.contains("src/a.rs"), "file header row a: {text}");
        assert!(text.contains("src/b.rs"), "file header row b: {text}");
    }

    #[test]
    fn render_diff_smoke() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    added: 1,
                    deleted: 1,
                }],
            },
            vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "new".into(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "old".into(),
                    section: 0,
                    word_runs: None,
                },
            ],
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();

        let cells = terminal.backend().buffer().content();
        let row: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(row.contains("x.rs"));
    }

    #[test]
    fn render_diff_with_word_runs() {
        let diff = single_diff(
            FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 2,
                deleted: 2,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    added: 2,
                    deleted: 2,
                }],
            },
            vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "old".into(),
                    section: 0,
                    // Single changed run covering the whole line.
                    word_runs: Some(vec![WordRun {
                        text: "old".into(),
                        changed: true,
                        kind: LineKind::Removed,
                    }]),
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "newlonger".into(),
                    section: 0,
                    // Pair whose sides differ in length; used to panic when
                    // syntect tokens ran out before the kept runs did.
                    word_runs: Some(vec![WordRun {
                        text: "newlonger".into(),
                        changed: true,
                        kind: LineKind::Added,
                    }]),
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "fn foo()".into(),
                    section: 0,
                    // Mixed Context + changed runs.
                    word_runs: Some(vec![
                        WordRun {
                            text: "fn ".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                        WordRun {
                            text: "foo".into(),
                            changed: true,
                            kind: LineKind::Removed,
                        },
                        WordRun {
                            text: "()".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                    ]),
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "fn bar()".into(),
                    section: 0,
                    word_runs: Some(vec![
                        WordRun {
                            text: "fn ".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                        WordRun {
                            text: "bar".into(),
                            changed: true,
                            kind: LineKind::Added,
                        },
                        WordRun {
                            text: "()".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                    ]),
                },
            ],
        );

        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.word_diff_on = true;
        app.last_header_height = 1;
        app.snapshot = snapshot_with_diff(diff);

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let row: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(row.contains("x.rs"));

        // Word-diff off: the non-overlay path still renders.
        app.word_diff_on = false;
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let row: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(
            row.contains("newlonger"),
            "non-word path must render the added line: {row}"
        );
        assert!(
            row.contains("old"),
            "non-word path must render the removed line: {row}"
        );

        // Directly exercise the guard: tokens that don't tile `raw` exactly
        // must not panic and must still tile `raw` exactly (fallback spans).
        let spans = overlay_word_runs(
            "newlonger",
            &[TokenOwned {
                text: "new".to_string(),
                color: Color::Reset,
            }],
            &[WordRun {
                text: "newlonger".into(),
                changed: true,
                kind: LineKind::Added,
            }],
            LineKind::Added,
            Color::Reset,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "newlonger");
    }

    #[test]
    fn mode_hidden_when_unchanged() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(Some(0o100644), Some(0o100644)),
            vec![hunk_header(0), ctx_line(0, 0)],
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(!text.contains("100644 → 100644"));
        assert_eq!(app.last_header_height, 1);
    }

    #[test]
    fn mode_shown_when_changed() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(Some(0o100644), Some(0o100755)),
            vec![hunk_header(0), ctx_line(0, 0)],
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("x.rs"));
        assert!(text.contains("100644 → 100755"));
        assert_eq!(app.last_header_height, 1);
    }

    #[test]
    fn mode_emitted_when_narrow() {
        let header = header_full(Some(0o100644), Some(0o100755));
        let spans = build_header_line(&header, 1, 20);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // The mode + diff-stat block is always appended; narrow widths rely on
        // elide to truncate the overflow, so both are present in raw spans.
        assert!(text.contains("100644 → 100755"));
        assert!(text.contains("+1"));
        assert!(text.contains("-1"));
        // The diff stat sits at the far right of the line.
        assert!(text.ends_with("-1"));
    }

    #[test]
    fn sticky_header_pinned() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        let n = 30;
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..n).map(|i| ctx_line(i, 0)))
            .collect();
        app.snapshot = snapshot_with_diff(single_diff(header_full(None, None), lines));
        // Scroll the `@@` header off above the body top.
        app.diff_scroll = 25;

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("▼"));
        // The header renders only in the sticky line, not in flow.
        assert_eq!(text.matches("▼").count(), 1);
    }

    #[test]
    fn sticky_disappears_when_header_visible() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        let n = 30;
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..n).map(|i| ctx_line(i, 0)))
            .collect();
        app.snapshot = snapshot_with_diff(single_diff(header_full(None, None), lines));
        app.diff_scroll = 0; // header at top, in flow.

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // In-flow header is expanded, so the arrow renders as `▼`; no
        // sticky duplicate header, and nothing is collapsed (`▶` absent).
        assert!(text.contains("▼"));
        assert!(!text.contains("▶"));
        assert_eq!(text.matches("▼").count(), 1);
    }

    #[test]
    fn hunk_header_restyle() {
        let meta = HunkMeta {
            added: 3,
            deleted: 1,
        };
        let name = hunk_name_spans(BG_HUNK);
        // The `@@` marker is replaced by a dedicated hunk icon.
        assert!(!name.iter().any(|s| s.content.as_ref() == "@@"));
        assert!(name.iter().any(|s| s.content.as_ref().contains(HUNK_ICON)));
        let stat = hunk_stat_spans(&meta, BG_HUNK, false);
        let has = |fg: Color, content: &str| {
            stat.iter()
                .any(|s| s.style.fg == Some(fg) && s.content.as_ref().contains(content))
        };
        assert!(has(Color::Green, "+3"));
        assert!(has(Color::Red, "-1"));
        assert!(has(Color::Blue, "▼"));
        let collapsed = hunk_stat_spans(&meta, BG_HUNK, true);
        assert!(collapsed
            .iter()
            .any(|s| s.style.fg == Some(Color::Blue) && s.content.as_ref() == "▶"));
    }

    #[test]
    fn active_highlight_bg() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        ));
        app.active_chunk = Some(0);
        // Focus the diff pane so the active chunk uses the full selection
        // color (#464741); otherwise focus-based dimming would render it dim.
        app.focus = crate::theme::Focus::Diff;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Top visible chunk (hunk 0's header, ACTIVE = brighter selection
        // #464741) at buffer row 1; hunk 1 is INACTIVE (inactive-tab #34352f)
        // at row 4. Sample past the hunk tab (one indent space, icon, padding,
        // and cap — five cells) to read the row's highlight background.
        assert_eq!(buf.cell((5, 1)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));
        assert_eq!(buf.cell((5, 4)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn focus_dims_active_chunk() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        ));
        app.active_chunk = Some(0);
        // Focus on the tree: the active chunk header is the unfocused pane's
        // active item, so it renders at the dimmed color #3d3e38.
        app.focus = crate::theme::Focus::Tree;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((5, 1)).unwrap().bg, Color::Rgb(0x3d, 0x3e, 0x38));
        assert_eq!(buf.cell((5, 4)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn render_leaves_active_chunk_alone() {
        let mut app = sample_app();
        app.focus = crate::theme::Focus::Diff;
        app.snapshot = snapshot_with_diff(fitting_hunk());
        app.height = 20;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(2);

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        // Rendering never mutates the active-chunk cursor.
        assert_eq!(app.active_chunk, Some(2));
    }

    #[test]
    fn last_line_visible_at_max_scroll_with_sticky() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        let n = 20;
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..n).map(|i| ctx_line(i, 0)))
            .collect();
        app.snapshot = snapshot_with_diff(single_diff(header_full(None, None), lines));
        // Scroll to the absolute bottom; render must clamp so the last line
        // (raw_text `l19`) is visible despite the sticky header row.
        app.diff_scroll = usize::MAX;

        let mut terminal = Terminal::new(TestBackend::new(80, 7)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("l19"), "last line was cut off: {text}");
    }

    #[test]
    fn render_tiny_pane_does_not_panic() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..5).map(|i| ctx_line(i, 0)))
            .collect();
        app.snapshot = snapshot_with_diff(single_diff(header_full(None, None), lines));

        // `body_height == 0`: the pane is only as tall as the locked header.
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        assert_eq!(app.sticky_file, None);
        assert_eq!(app.sticky_hunk, None);
    }

    #[test]
    fn sticky_rows_capped_to_body_height() {
        let mk = |path: &str| FileHeader {
            path: path.into(),
            status: FileStatus::Modified,
            added: 1,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: vec![HunkMeta {
                added: 1,
                deleted: 0,
            }],
        };
        let mut app = sample_app();
        app.selected_path = Some("src".into());
        app.snapshot = snapshot_with_diff(Diff {
            header: mk("src"),
            files: vec![mk("src/a.rs"), mk("src/b.rs")],
            sections: vec![
                Section {
                    kind: SectionKind::File,
                    file: 0,
                    hunk: None,
                },
                Section {
                    kind: SectionKind::Hunk,
                    file: 0,
                    hunk: Some(0),
                },
                Section {
                    kind: SectionKind::File,
                    file: 1,
                    hunk: None,
                },
                Section {
                    kind: SectionKind::Hunk,
                    file: 1,
                    hunk: Some(0),
                },
            ],
            lines: vec![
                DiffLine {
                    kind: LineKind::FileHeader,
                    raw_text: "src/a.rs".into(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@".into(),
                    section: 1,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "a".into(),
                    section: 1,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::FileHeader,
                    raw_text: "src/b.rs".into(),
                    section: 2,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@".into(),
                    section: 3,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "b".into(),
                    section: 3,
                    word_runs: None,
                },
            ],
        });
        // Both a file and a hunk sticky would pin, but only one body row
        // exists; the file sticky wins. (Three terminal rows: two body rows
        // plus the full-width footer.)
        app.diff_scroll = 5;

        let mut terminal = Terminal::new(TestBackend::new(80, 3)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        assert!(app.sticky_file.is_some());
        assert_eq!(app.sticky_hunk, None);
    }

    #[test]
    fn hunk_sticky_stays_within_file() {
        let lines = vec![
            DiffLine {
                kind: LineKind::FileHeader,
                raw_text: "a.rs".into(),
                section: 0,
                word_runs: None,
            },
            hunk_header(1),
            ctx_line(0, 1),
            DiffLine {
                kind: LineKind::FileHeader,
                raw_text: "b.rs".into(),
                section: 2,
                word_runs: None,
            },
            hunk_header(3),
            ctx_line(1, 3),
        ];
        let visible: Vec<usize> = (0..lines.len()).collect();
        let positions = header_positions(&lines);
        // Top scrolled just past b.rs's header: a.rs's hunk must not pin under
        // b.rs's header.
        let (file, hunk, _) = sticky_headers(&visible, &positions, 4, 5);
        assert_eq!(file, Some(3));
        assert_eq!(hunk, None);
        // One line further, b.rs's own hunk becomes sticky.
        let (file, hunk, _) = sticky_headers(&visible, &positions, 5, 5);
        assert_eq!(file, Some(3));
        assert_eq!(hunk, Some(4));
    }

    /// Precomputed header positions for an identity `visible` mapping.
    fn header_positions(lines: &[DiffLine]) -> HeaderPositions {
        let mut positions = HeaderPositions::default();
        for (pos, line) in lines.iter().enumerate() {
            match line.kind {
                LineKind::FileHeader => positions.file.push(pos),
                LineKind::HunkHeader => positions.hunk.push(pos),
                _ => {}
            }
        }
        positions
    }

    #[test]
    fn sticky_pair_confines_hunk_to_pinned_file() {
        let lines = vec![
            DiffLine {
                kind: LineKind::FileHeader,
                raw_text: "a.rs".into(),
                section: 0,
                word_runs: None,
            },
            hunk_header(1),
            ctx_line(0, 1),
            DiffLine {
                kind: LineKind::FileHeader,
                raw_text: "b.rs".into(),
                section: 2,
                word_runs: None,
            },
            hunk_header(3),
            ctx_line(1, 3),
        ];
        let positions = header_positions(&lines);
        assert_eq!(positions.sticky_pair(0), (None, None));
        // At b.rs's header row, a.rs (file + hunk) still pins.
        assert_eq!(positions.sticky_pair(3), (Some(0), Some(1)));
        // One row past it, b.rs pins but a.rs's hunk does not carry over.
        assert_eq!(positions.sticky_pair(4), (Some(3), None));
        assert_eq!(positions.sticky_pair(5), (Some(3), Some(4)));
    }

    #[test]
    fn sticky_headers_large_scroll() {
        // A tall single-hunk file: scroll deep into the body; the hunk header
        // (position 0) stays pinned and no file header exists.
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..500).map(|i| ctx_line(i, 0)))
            .collect();
        let visible: Vec<usize> = (0..lines.len()).collect();
        let positions = header_positions(&lines);

        let (file, hunk, flow) = sticky_headers(&visible, &positions, 400, 20);
        assert_eq!(file, None);
        assert_eq!(hunk, Some(0));
        assert_eq!(flow, 19);

        // At the very top the header is still in flow, not pinned.
        let (_, hunk, _) = sticky_headers(&visible, &positions, 0, 20);
        assert_eq!(hunk, None);
    }
}
