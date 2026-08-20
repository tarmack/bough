//! Span/text helpers shared by the render panes.
//!
//! Cell widths come from `Span::width`; wide (CJK) glyphs are an approximation
//! by design.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

/// Elide `spans` to `width` cells, starting at horizontal offset `col`.
/// Cell count is `Span::width`; wide (CJK) chars are an approximation. The
/// `bg`, when given, is applied to the `…` markers so they inherit the
/// background of the line they sit on.
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

/// Drop up to `col` cells from the front of `spans`, splitting the partial
/// span. Cell width ≈ char count (approximation for wide chars).
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

/// Keep the leading `cells` chars of `span` (approx cell count).
fn keep_left(span: &Span<'static>, cells: usize) -> Span<'static> {
    let s = span.content.as_ref();
    let take = cells.min(s.chars().count());
    Span::styled(s.chars().take(take).collect::<String>(), span.style)
}

/// Keep the tail of `span` after dropping `cells` leading chars (approx).
fn span_tail(span: &Span<'static>, cells: usize) -> Span<'static> {
    let s = span.content.as_ref();
    let drop = cells.min(s.chars().count());
    Span::styled(s.chars().skip(drop).collect::<String>(), span.style)
}

/// Highest `diff_scroll` that keeps the last visible line on screen, accounting
/// for the sticky header consuming one row at any `diff_scroll > 0`.
pub fn max_scroll_for(visible_len: usize, win: usize) -> usize {
    if visible_len <= win {
        0
    } else {
        visible_len - win + 1
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
        assert_eq!(max_scroll_for(21, 6), 16); // len > win: len - win + 1
        assert_eq!(max_scroll_for(6, 6), 0); // exactly fits
        assert_eq!(max_scroll_for(3, 6), 0); // fits
    }
}
