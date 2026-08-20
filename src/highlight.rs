//! Semantic syntax highlighting engine.
//!
//! Wraps syntect + two-face behind an owned-token API so the rest of `bough`
//! never touches syntect types. Grammar resolution is by file extension;
// themes come from two-face's embedded collection, which already includes
// syntect's bundled defaults under matching names.

use std::sync::OnceLock;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::theme::LazyThemeSet;

/// One colored span within a single highlighted line.
#[derive(Debug, Clone)]
pub struct TokenOwned {
    /// The literal text of the span.
    pub text: String,
    /// The foreground color to render the span with.
    pub color: Color,
}

/// A failure during highlighter construction.
#[derive(Debug)]
pub enum HighlightError {
    /// The requested theme name does not exist in the available set.
    UnknownTheme {
        /// The theme name that was requested.
        requested: String,
        /// Every valid theme name, sorted.
        available: Vec<String>,
    },
}

impl std::fmt::Display for HighlightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HighlightError::UnknownTheme {
                requested,
                available,
            } => write!(
                f,
                "unknown theme \"{requested}\"; available: {}",
                available.join(", ")
            ),
        }
    }
}

impl std::error::Error for HighlightError {}

/// Leak the two-face syntax set once so grammar references can be `'static`.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<&'static SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(|| Box::leak(Box::new(two_face::syntax::extra_no_newlines())))
}

/// Leak the two-face theme set once so theme references can be `'static`.
fn theme_set() -> &'static LazyThemeSet {
    static THEMES: OnceLock<&'static LazyThemeSet> = OnceLock::new();
    THEMES.get_or_init(|| Box::leak(Box::new(LazyThemeSet::from(two_face::theme::extra()))))
}

/// Highlights lines of code into owned ratatui color tokens.
///
/// Keeps the underlying syntect highlighter across calls so multi-line
/// constructs (comments, strings) keep their state within a file, and
/// recreates it when the grammar changes. It is reused per render and is not
/// borrow-locked between calls.
pub struct Highlighter {
    syntaxes: &'static SyntaxSet,
    theme: &'static syntect::highlighting::Theme,
    /// The grammar currently loaded, plus its live highlighter. `None` until
    /// the first line is highlighted.
    current: Option<(&'static SyntaxReference, HighlightLines<'static>)>,
}

impl Highlighter {
    /// Init with two-face syntaxes and the named theme. Returns `Err` on an
    /// unknown theme name.
    pub fn new(theme_name: &str) -> Result<Self, HighlightError> {
        let theme = theme_set()
            .get(theme_name)
            .ok_or_else(|| HighlightError::UnknownTheme {
                requested: theme_name.to_string(),
                available: Self::themes(),
            })?;
        Ok(Highlighter {
            syntaxes: syntaxes(),
            theme,
            current: None,
        })
    }

    /// All available theme names, sorted and deduped, covering both the
    /// two-face extras and the syntect built-ins two-face already bundles.
    pub fn themes() -> Vec<String> {
        let mut names: Vec<String> = two_face::theme::EmbeddedLazyThemeSet::theme_names()
            .iter()
            .map(|name| name.as_name().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Drop the current per-file syntect state so the next line starts fresh.
    pub fn reset(&mut self) {
        self.current = None;
    }

    /// Highlight one line of `text` for a file at `path`. Returns owned token
    /// spans, or an empty `Vec` when no grammar matches the file extension
    /// (the caller renders the line uncolored). Never panics.
    pub fn line(&mut self, path: &str, text: &str) -> Vec<TokenOwned> {
        let Some(syntax) = self.syntax_for_path(path) else {
            return Vec::new();
        };

        // Recreate the underlying highlighter when the grammar changes (a new
        // file). Reusing it keeps multi-line state intact within one file.
        if self
            .current
            .as_ref()
            .is_none_or(|tup| !std::ptr::eq(tup.0, syntax))
        {
            self.current = Some((syntax, HighlightLines::new(syntax, self.theme)));
        }

        // `current` was just set to `Some` above, so the unwrap is safe.
        let (_, hl) = self.current.as_mut().unwrap();
        let ranges = match hl.highlight_line(text, self.syntaxes) {
            Ok(ranges) => ranges,
            // Degraded mode by design: swallow the error and render uncolored.
            Err(_) => return Vec::new(),
        };

        ranges
            .into_iter()
            .map(|(style, text)| TokenOwned {
                text: text.to_string(),
                color: ratatui_color(style),
            })
            .collect()
    }

    /// Look up the grammar for `path` by its file extension.
    fn syntax_for_path(&self, path: &str) -> Option<&'static SyntaxReference> {
        let ext = extension(path)?.to_ascii_lowercase();
        self.syntaxes.find_syntax_by_extension(&ext)
    }
}

/// Extract the lowercase-hint extension from a path, if any.
fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

/// Translate a syntect style's sRGB foreground into a ratatui color.
fn ratatui_color(style: Style) -> Color {
    let fg = style.foreground;
    Color::Rgb(fg.r, fg.g, fg.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_default_theme() {
        let mut hl = Highlighter::new("Monokai Extended").unwrap();
        let tokens = hl.line("x.rs", "fn main() {}");
        assert!(!tokens.is_empty());
        assert!(tokens.iter().any(|t| matches!(t.color, Color::Rgb(..))));
    }

    #[test]
    fn new_rejects_unknown_theme() {
        let err = match Highlighter::new("does-not-exist") {
            Ok(_) => panic!("expected Err"),
            Err(e) => e,
        };
        match err {
            HighlightError::UnknownTheme {
                requested,
                available,
            } => {
                assert_eq!(requested, "does-not-exist");
                assert!(available.contains(&"Monokai Extended".to_string()));
            }
        }
    }

    #[test]
    fn line_returns_empty_on_grammar_miss() {
        let mut hl = Highlighter::new("Monokai Extended").unwrap();
        assert!(hl.line("x.unknownext", "anything").is_empty());
    }

    #[test]
    fn themes_includes_monokai() {
        assert!(Highlighter::themes().contains(&"Monokai Extended".to_string()));
    }
}
