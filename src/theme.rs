//! Monokai color palette and per-kind color helpers shared by the panes.

use ratatui::style::Color;

use crate::app::Focus;
use crate::diff::LineKind;

pub const BG_ADDED: Color = Color::Rgb(0x1a, 0x33, 0x26);
pub const BG_REMOVED: Color = Color::Rgb(0x3a, 0x1f, 0x1f);
// Monokai chunk headers: inactive/folded uses the inactive-tab color
// (#34352f), the active chunk uses the brighter selection shade (#464741).
// Both are darker than the #272822 terminal background (see docs/monokai-classic.md).
pub const BG_HUNK: Color = Color::Rgb(0x34, 0x35, 0x2f);
pub const BG_HUNK_ACTIVE: Color = Color::Rgb(0x46, 0x47, 0x41);
// Dimmed/unfocused active color: midpoint of active #464741 and inactive
// #34352f. Used when a pane's active item sits in the unfocused pane.
pub const BG_HUNK_DIM: Color = Color::Rgb(0x3d, 0x3e, 0x38);
pub const BG_RUN_ADDED: Color = Color::Rgb(0x2f, 0x55, 0x44);

pub const BG_RUN_REMOVED: Color = Color::Rgb(0x5a, 0x2f, 0x2f);

/// Monokai blue (#66d9ef) for directory nodes in the tree.
pub const DIR_FG: Color = Color::Rgb(0x66, 0xd9, 0xef);

/// Foreground color for a diff/file status letter.
pub fn status_color(ch: char) -> Color {
    match ch {
        'A' => Color::Green,
        'M' => Color::Yellow,
        'D' => Color::Red,
        'R' => Color::Magenta,
        'C' => Color::Cyan,
        'T' => Color::Blue,
        _ => Color::Reset,
    }
}

/// Background tint for a body line, depending on its diff kind.
pub fn bg_for(kind: LineKind) -> Color {
    match kind {
        LineKind::Added => BG_ADDED,
        LineKind::Removed => BG_REMOVED,
        LineKind::HunkHeader => BG_HUNK,
        LineKind::FileHeader => Color::Reset,
        LineKind::Context => Color::Reset,
    }
}

/// Lighter background for a changed word run.
pub fn run_bg(kind: LineKind) -> Color {
    match kind {
        LineKind::Added => BG_RUN_ADDED,
        LineKind::Removed => BG_RUN_REMOVED,
        _ => Color::Reset,
    }
}

/// Background for a chunk header: active is the selection color (light) when
/// the diff pane is focused, the dimmed midpoint when unfocused, and the
/// inactive-tab color otherwise.
pub fn hunk_header_bg(is_active: bool, focus: Focus) -> Color {
    if is_active {
        if focus == Focus::Diff {
            BG_HUNK_ACTIVE
        } else {
            BG_HUNK_DIM
        }
    } else {
        BG_HUNK
    }
}
