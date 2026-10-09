//! Semantic syntax highlighting engine.
//!
//! Wraps syntect + two-face behind an owned-token API so the rest of `bough`
//! never touches syntect types. Grammar resolution is by file extension;
//! themes come from two-face's embedded collection, which already includes
//! syntect's bundled defaults under matching names.

use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::Color;
use syntect::highlighting::{self, HighlightIterator, HighlightState, Style};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use two_face::theme::LazyThemeSet;

/// One colored span within a single highlighted line.
#[derive(Debug, Clone)]
pub struct TokenOwned {
    /// The literal text of the span.
    pub text: String,
    /// The foreground color to render the span with.
    pub color: Color,
}

/// A checkpointed parse/scope state for one file, resumable at any line.
///
/// Cloning and restarting from a checkpoint is cheap, so the worker can jump
/// to an arbitrary raw-line range of a diff without re-parsing from line 0.
///
/// # Contiguity assumption
///
/// A `ResumeState` is only valid for the exact line sequence that produced it.
/// The consumer must feed lines *contiguously* from the checkpoint onward:
/// syntect's parser state carries multi-line constructs (block comments,
/// template strings, heredocs), so skipping lines — as feeding hunk bodies
/// with gaps between them does — drops the intervening text and drifts the
/// scope stack, coloring later lines under the wrong grammar context.
#[derive(Clone)]
pub struct ResumeState {
    /// The live syntect parser state.
    parse: ParseState,
    /// The scope stack at the end of the last parsed line.
    scope: ScopeStack,
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
/// Keeps a reusable syntect [`highlighting::Highlighter`] (theme-driven style
/// resolution) and lets callers drive the per-file [`ParseState`]/`ScopeStack`
/// themselves via [`fresh_state`](Self::fresh_state),
/// [`parse_only`](Self::parse_only), and [`highlight`](Self::highlight). The
/// file's grammar is resolved once per fresh state, so multi-line constructs
/// (comments, strings) render correctly across arbitrary line ranges. It is
/// reused per render and is not borrow-locked between calls.
pub struct Highlighter {
    syntaxes: &'static SyntaxSet,
    hl: highlighting::Highlighter<'static>,
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
            hl: highlighting::Highlighter::new(theme),
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

    /// A fresh, empty [`ResumeState`] for the file at `path`, or `None` when no
    /// grammar matches its extension (the caller renders those lines
    /// uncolored).
    pub fn fresh_state(&self, path: &str) -> Option<ResumeState> {
        let syntax = self.syntax_for_path(path)?;
        Some(ResumeState {
            parse: ParseState::new(syntax),
            scope: ScopeStack::default(),
        })
    }

    /// Advance `resume` past each of `texts` without producing any tokens.
    ///
    /// This is the cheap path used to bridge a checkpoint to an arbitrary body
    /// line: it mutates only the parse state and scope stack, never allocating
    /// styles.
    ///
    /// Returns `true` once every line parsed, or `false` when a parse error
    /// aborted the advance (the remaining lines were skipped, leaving `resume`
    /// only partially advanced). Callers that cache the mutated state as a
    /// complete checkpoint must treat `false` as "do not resume from this":
    /// continuing from a partial state highlights later lines from the wrong
    /// scope.
    pub fn parse_only(&self, resume: &mut ResumeState, texts: &[&str]) -> bool {
        for text in texts {
            let Ok(ops) = resume.parse.parse_line(text, self.syntaxes) else {
                return false;
            };
            for (_, op) in ops {
                let _ = resume.scope.apply(&op);
            }
        }
        true
    }

    /// Highlight one line, advancing `resume` to the end of that line's scope.
    ///
    /// Mirrors `syntect::easy::HighlightLines::highlight_line`: parse the line,
    /// build a [`HighlightState`] from the previous scope stack, run a
    /// [`HighlightIterator`], then capture its end-of-line `path` back into
    /// `resume`. Returns owned token spans, or an empty `Vec` on a parse error.
    ///
    /// An empty `Vec` is the same failed-parse signal as [`parse_only`]'s
    /// `false`: the line renders uncolored, and callers must not treat the
    /// mutated `resume` as a trustworthy checkpoint afterward.
    ///
    /// [`parse_only`]: Self::parse_only
    pub fn highlight(&self, resume: &mut ResumeState, text: &str) -> Vec<TokenOwned> {
        let ops = match resume.parse.parse_line(text, self.syntaxes) {
            Ok(ops) => ops,
            Err(_) => return Vec::new(),
        };
        let mut hs = HighlightState::new(&self.hl, resume.scope.clone());
        let iter = HighlightIterator::new(&mut hs, &ops, text, &self.hl);
        let tokens = iter
            .map(|(style, s)| TokenOwned {
                text: s.to_string(),
                color: ratatui_color(style),
            })
            .collect();
        resume.scope = hs.path;
        tokens
    }

    /// Highlight one line of `text` for a file at `path`, starting from a fresh
    /// [`ResumeState`]. Returns owned token spans, or an empty `Vec` when no
    /// grammar matches the file extension. Never panics.
    #[cfg(test)]
    pub fn line(&self, path: &str, text: &str) -> Vec<TokenOwned> {
        let Some(mut resume) = self.fresh_state(path) else {
            return Vec::new();
        };
        self.highlight(&mut resume, text)
    }

    /// Look up the grammar for `path` by its file extension.
    fn syntax_for_path(&self, path: &str) -> Option<&'static SyntaxReference> {
        let ext = path_ext(path)?.to_ascii_lowercase();
        self.syntaxes.find_syntax_by_extension(&ext)
    }
}

/// Extract a path's file extension, or `None` for stemless names like `.png`.
///
/// `Path::extension` reports an extension for a bare dotfile (`.png` → `png`)
/// even though its stem is empty; callers treat those as extensionless, so
/// they are rejected here too. Shared with `preview::is_image_path`.
pub(crate) fn path_ext(path: &str) -> Option<&str> {
    let p = Path::new(path);
    let ext = p.extension()?.to_str()?;
    if p.file_stem().map(|s| s.is_empty()).unwrap_or(true) {
        return None;
    }
    Some(ext)
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
        let hl = Highlighter::new("Monokai Extended").unwrap();
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
        let hl = Highlighter::new("Monokai Extended").unwrap();
        assert!(hl.line("x.unknownext", "anything").is_empty());
    }

    #[test]
    fn themes_includes_monokai() {
        assert!(Highlighter::themes().contains(&"Monokai Extended".to_string()));
    }

    #[test]
    fn resume_state_roundtrips() {
        let hl = Highlighter::new("Monokai Extended").unwrap();
        assert!(hl.fresh_state("x.rs").is_some());
        assert!(hl.fresh_state("x.unknownext").is_none());
    }
}
