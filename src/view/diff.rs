//! Diff-pane rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::App;
use crate::diff::{DiffLine, FileHeader, HunkMeta, LineKind, ParsedDiff, SectionKind, WordRun};
use crate::highlight::TokenOwned;
use crate::text::{elide, max_scroll_for, render_centered};
use crate::theme;
use crate::view::tree::file_icon;

/// Render the diff pane: the locked header, optional rename line, and the
/// scrollable body (sticky pinned hunk header + hunk rows).
pub fn render_diff(f: &mut Frame, app: &mut App, area: Rect) {
    // No file selected: placeholder, no locked header.
    if app.current_file().is_none() {
        let style = Style::default().fg(Color::DarkGray);
        render_centered(f, area, "select a file to preview its diff", style);
        return;
    }
    let Some(parsed) = app.snapshot.selected_diff.as_ref() else {
        let msg = app.diff_error().unwrap_or("diff failed");
        let line = Line::styled(format!("error: {msg}"), Style::default().fg(Color::Red));
        f.buffer_mut().set_line(area.x, area.y, &line, area.width);
        return;
    };

    let multi = parsed.files.len() > 1;
    let has_rename = !multi && parsed.header.old_path.is_some();
    let header_height = 1 + has_rename as u16;
    app.last_header_height = header_height;

    let visible = app.visible_line_indices();
    let body_height = area.height.saturating_sub(header_height);
    let max_scroll = max_scroll_for(visible.len(), body_height as usize);
    if app.diff_scroll > max_scroll {
        app.diff_scroll = max_scroll;
    }

    let active = app.active_chunk;
    let current = active.map(|s| s + 1).unwrap_or(0);

    // Locked header: the single file's header, or a folder summary carrying
    // the path and total file count. Line 1 always, line 2 rename.
    let header_spans = if multi {
        build_summary_header(parsed, area.width as usize)
    } else {
        build_header_line(&parsed.header, current, area.width as usize)
    };
    f.buffer_mut().set_line(
        area.x,
        area.y,
        &elide(header_spans, app.diff_col, area.width, None),
        area.width,
    );
    let mut y = area.y + 1;
    if has_rename {
        let rename = build_rename_line(&parsed.header);
        f.buffer_mut().set_line(
            area.x,
            y,
            &elide(rename, app.diff_col, area.width, None),
            area.width,
        );
        y += 1;
    }

    // Empty body: centered messages below the header.
    if parsed.lines.is_empty() {
        let msg = if parsed.header.binary {
            "binary file"
        } else if !multi && parsed.header.status == 'R' {
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

    // Scrollable body. A sticky file header and a sticky hunk header are
    // pinned independently: scrolling into the next file replaces the file
    // sticky, while the hunk sticky tracks the last hunk within it.
    let start = app.diff_scroll;

    let file_sticky = if start > 0 {
        visible
            .iter()
            .take(start)
            .rev()
            .find(|&&li| parsed.lines[li].kind == LineKind::FileHeader)
            .copied()
    } else {
        None
    };
    let hunk_sticky = if start > 0 {
        visible
            .iter()
            .take(start)
            .rev()
            .find(|&&li| parsed.lines[li].kind == LineKind::HunkHeader)
            .copied()
    } else {
        None
    };
    // Topmost pinned sticky line, for click-row mapping.
    app.sticky_line = file_sticky.or(hunk_sticky);
    let sticky_rows = file_sticky.map(|_| 1u16).unwrap_or(0) + hunk_sticky.map(|_| 1u16).unwrap_or(0);
    let flow_height = body_height.saturating_sub(sticky_rows);

    if let Some(idx) = file_sticky {
        y = render_section_header(f, app, area, y, idx, parsed);
    }
    if let Some(idx) = hunk_sticky {
        y = render_section_header(f, app, area, y, idx, parsed);
    }

    let end = (start + flow_height as usize).min(visible.len());
    for &line_idx in &visible[start..end] {
        let line = &parsed.lines[line_idx];
        if line.kind.is_header() {
            y = render_section_header(f, app, area, y, line_idx, parsed);
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
}

/// Locked header for a multi-file (folder) diff: `<path>  N files` with the
/// aggregate diffstat right-aligned.
fn build_summary_header(parsed: &ParsedDiff, width: usize) -> Vec<Span<'static>> {
    let mut left = vec![
        Span::styled(
            parsed.header.path.clone(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} files", parsed.files.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    let mut right = vec![Span::styled(
        format!("+{}", parsed.header.added),
        Style::default().fg(Color::Green),
    )];
    right.push(Span::styled(
        format!(" -{}", parsed.header.deleted),
        Style::default().fg(Color::Red),
    ));
    let left_w: usize = left.iter().map(|s| s.width()).sum();
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    if width > left_w + 1 + right_w {
        left.push(Span::raw(" ".repeat(width - left_w - right_w)));
    }
    left.extend(right);
    left
}

/// Locked header line 1: `<path>  +A -D  <status>  [cur/total]`, plus a
/// right-aligned mode change (`old → new`, octal) when the modes differ.
fn build_header_line(header: &FileHeader, current_hunk: usize, width: usize) -> Vec<Span<'static>> {
    // Left content: path and hunk position.
    let mut left = vec![Span::styled(
        header.path.clone(),
        Style::default().fg(Color::DarkGray),
    )];
    if !header.hunks.is_empty() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            format!("[{current_hunk}/{}]", header.hunks.len()),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right block: optional mode change, then the diff stat at the far right
    // so it aligns with the chunk-header diffstat below. `binary` moves with
    // the diff stat.
    let mut right: Vec<Span<'static>> = Vec::new();
    if let (Some(old), Some(new)) = (header.mode_old, header.mode_new) {
        if old != new {
            right.push(Span::styled(
                format!("{old:o} → {new:o}"),
                Style::default().fg(Color::DarkGray),
            ));
            right.push(Span::raw("  "));
        }
    }
    if header.binary {
        right.push(Span::styled("binary", Style::default().fg(Color::DarkGray)));
    } else {
        right.push(Span::styled(
            format!("+{}", header.added),
            Style::default().fg(Color::Green),
        ));
        right.push(Span::styled(
            format!(" -{}", header.deleted),
            Style::default().fg(Color::Red),
        ));
    }

    let left_w: usize = left.iter().map(|s| s.width()).sum();
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    // Right-align the block when there is room (at least one space of pad);
    // otherwise append it and let elide truncate the overflow.
    if width > left_w + 1 + right_w {
        left.push(Span::raw(" ".repeat(width - left_w - right_w)));
    }
    left.extend(right);
    left
}

/// Locked header line 2: rename `old → new  R <percent>%`.
fn build_rename_line(header: &FileHeader) -> Vec<Span<'static>> {
    let old = header.old_path.clone().unwrap_or_default();
    let mut spans = vec![
        Span::styled(old, Style::default().fg(Color::DarkGray)),
        Span::styled(" → ", Style::default().fg(Color::DarkGray)),
        Span::styled(header.path.clone(), Style::default().fg(Color::DarkGray)),
    ];
    if let Some(p) = header.rename_percent {
        spans.push(Span::styled(
            format!("  R {p}%"),
            Style::default().fg(Color::Magenta),
        ));
    }
    spans
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

/// The owning file's icon and the syntax-highlighted section name; scrolls
/// horizontally.
fn hunk_name_spans(meta: &HunkMeta, name_tokens: &[TokenOwned], bg: Color) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        // One leading space indents hunks beneath their file header.
        format!(" {} ", HUNK_ICON),
        Style::default().fg(Color::Blue).bg(bg),
    )];
    if let Some(section) = &meta.section {
        if name_tokens.is_empty() {
            spans.push(Span::styled(
                section.clone(),
                Style::default().fg(Color::DarkGray).bg(bg),
            ));
        } else {
            for t in name_tokens {
                spans.push(Span::styled(
                    t.text.clone(),
                    Style::default().fg(t.color).bg(bg),
                ));
            }
        }
    }
    spans
}

/// The `+A -D` diffstat, a spacer, and the fold arrow. Pinned to the pane's
/// right edge (does not scroll).
fn hunk_stat_spans(meta: &HunkMeta, bg: Color, collapsed: bool) -> Vec<Span<'static>> {
    let glyph = if collapsed { "▶" } else { "▼" };
    vec![
        Span::styled(
            format!("+{}", meta.added),
            Style::default().fg(Color::Green).bg(bg),
        ),
        Span::styled(
            format!(" -{}", meta.deleted),
            Style::default().fg(Color::Red).bg(bg),
        ),
        Span::styled(" ".to_string(), Style::default().bg(bg)),
        Span::styled(glyph.to_string(), Style::default().fg(Color::Blue).bg(bg)),
    ]
}

/// Content of a navigable section header: a hunk (`@@ …`) or a file header
/// inside a multi-file diff. Both render through the same layout — a name that
/// scrolls/elides on the left and a right-pinned diffstat + fold arrow.
enum HeaderContent<'a> {
    File { file: &'a FileHeader },
    Hunk {
        meta: &'a HunkMeta,
        name_tokens: &'a [TokenOwned],
    },
}

impl<'a> HeaderContent<'a> {
    fn name_spans(&self, bg: Color) -> Vec<Span<'static>> {
        match self {
            HeaderContent::File { file } => file_header_name_spans(file, bg),
            HeaderContent::Hunk { meta, name_tokens } => hunk_name_spans(meta, name_tokens, bg),
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
    parsed: &ParsedDiff,
) -> u16 {
    let line = &parsed.lines[line_idx];
    let collapsed = app.collapsed_hunks.contains(&line.section);
    let bg = theme::hunk_header_bg(app.active_chunk == Some(line.section), app.focus);

    match header_content(app, line_idx, parsed, line) {
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
fn header_content<'a>(
    app: &'a App,
    line_idx: usize,
    parsed: &'a ParsedDiff,
    line: &DiffLine,
) -> Option<HeaderContent<'a>> {
    let section = parsed.sections.get(line.section)?;
    let file = parsed.files.get(section.file)?;
    match section.kind {
        SectionKind::File => Some(HeaderContent::File { file }),
        SectionKind::Hunk => {
            let meta = file.hunks.get(section.hunk?)?;
            let name_tokens = app
                .highlighted()
                .get(line_idx)
                .and_then(|opt| opt.as_deref())
                .unwrap_or(&[]);
            Some(HeaderContent::Hunk { meta, name_tokens })
        }
    }
}

/// Shared layout for any section header: elide the name, pin the diffstat +
/// fold arrow to the right edge, and pad the name's background to the full
/// region so the whole row is one highlighted strip.
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
    let name_w = area.width.saturating_sub(stat_w);
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
    if name_w > 0 {
        f.buffer_mut()
            .set_line(area.x + name_w, y, &Line::from(stat), area.width - name_w);
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
            spans.push(Span::styled(
                format!("  {old:o} → {new:o}"),
                Style::default().fg(Color::DarkGray).bg(bg),
            ));
        }
    }
    spans
}

/// The `[icon] ` prefix of a header row: the owning file's type glyph colored
/// `color`, on the header background `bg`.
fn header_icon(file: &FileHeader, color: Color, bg: Color) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("{} ", file_icon(&file.path)),
        Style::default().fg(color).bg(bg),
    )]
}

/// Right-pinned stat + fold arrow for a file-header row.
fn file_header_stat_spans(file: &FileHeader, bg: Color, collapsed: bool) -> Vec<Span<'static>> {
    let glyph = if collapsed { "▶" } else { "▼" };
    let mut spans = if file.binary {
        vec![Span::styled("binary", Style::default().fg(Color::DarkGray).bg(bg))]
    } else {
        vec![
            Span::styled(
                format!("+{}", file.added),
                Style::default().fg(Color::Green).bg(bg),
            ),
            Span::styled(
                format!(" -{}", file.deleted),
                Style::default().fg(Color::Red).bg(bg),
            ),
        ]
    };
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
    use crate::diff::{DiffLine, FileHeader, HunkMeta, LineKind, ParsedDiff, Section, SectionKind};
    use crate::test_support::*;
    use crate::theme::BG_HUNK;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn file_header(path: &str, added: usize, deleted: usize) -> FileHeader {
        FileHeader {
            path: path.into(),
            status: 'M',
            added,
            deleted,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            hunks: vec![HunkMeta {
                section: None,
                added,
                deleted,
            }],
        }
    }

    #[test]
    fn renders_file_headers_in_folder_diff() {
        let fa = file_header("src/a.rs", 1, 0);
        let fb = file_header("src/b.rs", 2, 0);
        let parsed = ParsedDiff {
            header: FileHeader {
                path: "src".into(),
                old_path: None,
                status: 'M',
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
        app.snapshot = snapshot_with_diff(parsed);

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
        assert!(text.contains("2 files"), "file count in locked header: {text}");
        assert!(text.contains("src/a.rs"), "file header row a: {text}");
        assert!(text.contains("src/b.rs"), "file header row b: {text}");
    }

    #[test]
    fn render_diff_smoke() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_parsed(
            FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
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
        let parsed = single_parsed(
            FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 2,
                deleted: 2,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
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
        app.snapshot = snapshot_with_diff(parsed);

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let row: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(row.contains("x.rs"));

        // Word-diff off: the non-overlay path still renders.
        app.word_diff_on = false;
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();

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
        app.snapshot = snapshot_with_diff(single_parsed(
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
        app.snapshot = snapshot_with_diff(single_parsed(
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
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            lines,
        ));
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
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            lines,
        ));
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
            section: Some("fn main()".into()),
            added: 3,
            deleted: 1,
        };
        let name_tokens = vec![TokenOwned {
            text: "fn".into(),
            color: Color::Rgb(1, 2, 3),
        }];
        let name = hunk_name_spans(&meta, &name_tokens, BG_HUNK);
        assert!(name.iter().any(|s| s.content.as_ref() == "fn")); // syntax-highlighted name
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
        app.snapshot = snapshot_with_diff(single_parsed(
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
        app.focus = crate::app::Focus::Diff;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Top visible chunk (hunk 0's header, ACTIVE = brighter selection
        // #464741) at buffer row 1; hunk 1 is INACTIVE (inactive-tab #34352f)
        // at row 4.
        assert_eq!(buf.cell((0, 1)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));
        assert_eq!(buf.cell((0, 4)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn focus_dims_active_chunk() {
        let mut app = sample_app();
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_parsed(
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
        app.focus = crate::app::Focus::Tree;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::view::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((0, 1)).unwrap().bg, Color::Rgb(0x3d, 0x3e, 0x38));
        assert_eq!(buf.cell((0, 4)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn render_leaves_active_chunk_alone() {
        let mut app = sample_app();
        app.focus = crate::app::Focus::Diff;
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
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            lines,
        ));
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
}
