//! Syntax highlighting layered over diff colors (PRD §11.1).
//!
//! Highlighting is per-line and cached: the diff view only ever asks for the
//! visible (and annotated) rows, so work is bounded by the viewport. Lines past
//! a length cap, and files past a size cap, are returned unhighlighted to keep
//! large diffs responsive. Per-line highlighting trades some accuracy on
//! multi-line constructs (strings/comments) for simplicity; tree-sitter is the
//! noted later upgrade.

use std::cell::RefCell;
use std::collections::HashMap;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::theme::ThemeMode;

/// Longest line that will be highlighted; longer lines render plain.
const MAX_LINE_LEN: usize = 4096;

/// A foreground color span over a slice of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub color: Color,
    pub text: String,
}

/// Caching syntect highlighter bound to one [`ThemeMode`].
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    default_fg: Color,
    cache: RefCell<HashMap<(String, String), Vec<Span>>>,
}

impl Highlighter {
    /// Build a highlighter using syntect's bundled syntaxes and a theme paired
    /// to `mode`.
    pub fn new(mode: ThemeMode, default_fg: Color) -> Self {
        let themes = ThemeSet::load_defaults();
        let name = match mode {
            ThemeMode::Dark => "base16-ocean.dark",
            ThemeMode::Light => "InspiredGitHub",
        };
        let theme = themes.themes[name].clone();

        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
            default_fg,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Highlight one line of a file with the given extension, returning colored
    /// spans. Falls back to a single default-colored span when the language is
    /// unknown or the line is too long.
    pub fn spans(&self, extension: &str, line: &str) -> Vec<Span> {
        let plain = || {
            vec![Span {
                color: self.default_fg,
                text: line.to_string(),
            }]
        };

        if line.len() > MAX_LINE_LEN {
            return plain();
        }

        let Some(syntax) = self.syntax_for(extension) else {
            return plain();
        };

        let key = (syntax.name.clone(), line.to_string());

        if let Some(cached) = self.cache.borrow().get(&key) {
            return cached.clone();
        }

        let spans = self.highlight(syntax, line).unwrap_or_else(plain);
        self.cache.borrow_mut().insert(key, spans.clone());
        spans
    }

    fn syntax_for(&self, extension: &str) -> Option<&SyntaxReference> {
        self.syntaxes.find_syntax_by_extension(extension)
    }

    fn highlight(&self, syntax: &SyntaxReference, line: &str) -> Option<Vec<Span>> {
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let ranges = highlighter.highlight_line(line, &self.syntaxes).ok()?;

        Some(
            ranges
                .into_iter()
                .map(|(style, text)| Span {
                    color: Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b),
                    text: text.to_string(),
                })
                .collect(),
        )
    }
}
