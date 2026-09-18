//! Span/text helpers shared by the render panes.
//!
//! Cell widths come from `Span::width`, so wide (CJK) glyphs are measured in
//! their true display-cell counts.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

/// Elide `spans` to `width` cells, starting at horizontal offset `col`.
/// Cell counts are `Span::width` display cells, so wide (CJK) chars are
/// honoured. The `bg`, when given, is applied to the `…` markers so they
/// inherit the background of the line they sit on.
pub fn elide(
    spans: Vec<Span<'static>>,
    col: usize,
    width: u16,
    bg: Option<Color>,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let marker = match bg {
        Some(bg) => Span::styled("…".to_string(), Style::default().fg(Color::DarkGray).bg(bg)),
        None => Span::styled("…".to_string(), Style::default().fg(Color::DarkGray)),
    };
    let avail = width as usize;
    let mut result: Vec<Span<'static>> = Vec::new();

    if col > 0 {
        // Left marker eats one cell, then skip `col` cells from the left.
        result.push(marker.clone());
        result.extend(drop_left(spans, col));
    } else {
        result.extend(spans);
    }

    if result.iter().map(|s| s.width()).sum::<usize>() <= avail {
        return Line::from(result);
    }

    // Leave one cell for the right marker.
    let target = avail.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in result {
        let w = span.width();
        if used + w <= target {
            used += w;
            out.push(span);
        } else if used < target {
            out.push(keep_left(&span, target - used));
            break;
        } else {
            break;
        }
    }
    out.push(marker);
    Line::from(out)
}

/// Display width of a single char, via ratatui's `Span::width` (which uses
/// `unicode-width`). `encode_utf8` keeps this allocation-free.
fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    let s: &str = c.encode_utf8(&mut buf);
    Span::raw(s).width()
}

/// Display width of `s` in terminal cells (wide glyphs count as 2).
pub(crate) fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Drop up to `col` display cells from the front of `spans`, splitting the
/// partial span. Wide (CJK) chars count as their display cells, so the tail
/// never exceeds the requested cell budget.
fn drop_left(spans: Vec<Span<'static>>, col: usize) -> Vec<Span<'static>> {
    let mut skip = col;
    let mut i = 0usize;
    while i < spans.len() && skip > 0 {
        let w = spans[i].width();
        if w <= skip {
            skip -= w;
            i += 1;
        } else {
            let mut kept = Vec::with_capacity(spans.len() - i);
            kept.push(span_tail(&spans[i], skip));
            kept.extend(spans.iter().skip(i + 1).cloned());
            return kept;
        }
    }
    spans.into_iter().skip(i).collect()
}

/// Keep the leading chars of `span` whose combined display width does not
/// exceed `cells`.
fn keep_left(span: &Span<'static>, cells: usize) -> Span<'static> {
    let s = span.content.as_ref();
    let mut kept = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > cells {
            break;
        }
        used += w;
        kept.push(c);
    }
    Span::styled(kept, span.style)
}

/// Keep the tail of `span` once at least `cells` display cells have been
/// dropped from the front. A wide char straddling the boundary is dropped
/// whole, so the tail width never exceeds the remaining budget.
fn span_tail(span: &Span<'static>, cells: usize) -> Span<'static> {
    let s = span.content.as_ref();
    let mut dropped = 0usize;
    let mut used = 0usize;
    for c in s.chars() {
        if used >= cells {
            break;
        }
        used += char_width(c);
        dropped += 1;
    }
    Span::styled(s.chars().skip(dropped).collect::<String>(), span.style)
}

/// Highest `diff_scroll` that keeps the last visible line on screen, accounting
/// for the `sticky_rows` header rows pinned above the flow (`1` for a lone
/// hunk sticky, up to `2` when a file and a hunk sticky are both pinned).
pub fn max_scroll_for(visible_len: usize, win: usize, sticky_rows: usize) -> usize {
    if visible_len <= win {
        0
    } else {
        visible_len - win + sticky_rows.max(1)
    }
}

/// Draw `text` centered within `area`, using a single `style`.
pub fn render_centered(f: &mut Frame, area: Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let w = text.chars().count() as u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(1) / 2;
    let line = Line::styled(text.to_string(), style);
    f.buffer_mut().set_line(x, y, &line, area.width);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_counts_wide_glyphs_as_two_cells() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("a日c"), 4);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn elide_truncates_right() {
        let line = elide(vec![Span::raw("abcdefghijklm")], 0, 10, None);
        let spans = line.spans;
        assert_eq!(spans.last().unwrap().content.as_ref(), "…");
        let total: usize = spans.iter().map(|s| s.width()).sum();
        assert!(total <= 10);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with('…'));
        assert!(text.chars().count() <= 10);
    }

    #[test]
    fn elide_left_marker() {
        let line = elide(vec![Span::raw("abcdefghijklm")], 5, 10, None);
        let spans = line.spans;
        assert_eq!(spans[0].content.as_ref(), "…");
        assert_eq!(spans[0].style.fg, Some(Color::DarkGray));
        let rest: String = spans.iter().skip(1).map(|s| s.content.as_ref()).collect();
        assert!(rest.starts_with('f'));
        let total: usize = spans.iter().map(|s| s.width()).sum();
        assert!(total <= 10);
    }

    #[test]
    fn max_scroll_accounts_for_sticky_row() {
        assert_eq!(max_scroll_for(21, 6, 1), 16); // len > win: len - win + 1
        assert_eq!(max_scroll_for(21, 6, 2), 17); // two pinned stickies
        assert_eq!(max_scroll_for(6, 6, 1), 0); // exactly fits
        assert_eq!(max_scroll_for(3, 6, 2), 0); // fits, sticky count ignored
    }

    #[test]
    fn elide_wide_chars_within_width() {
        // Each CJK glyph is two cells; the kept prefix plus `…` must fit.
        let line = elide(vec![Span::raw("你好世界")], 0, 5, None);
        let total: usize = line.spans.iter().map(|s| s.width()).sum();
        assert!(total <= 5, "wide line exceeded width: {total}");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "你好…");
    }

    #[test]
    fn drop_left_wide_chars_uses_cells() {
        // Dropping 3 cells from `你好世界` (2 cells each) lands after `你好`.
        let line = elide(vec![Span::raw("你好世界")], 3, 5, None);
        let total: usize = line.spans.iter().map(|s| s.width()).sum();
        assert!(total <= 5, "wide line exceeded width: {total}");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "…世界");
    }
}
