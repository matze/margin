//! Syntax highlighting layered over diff colors (PRD §11.1).
//!
//! Highlighting is per-line and cached: the diff view only ever asks for the
//! visible (and annotated) rows, so work is bounded by the viewport. Lines past
//! a length cap, and files past a size cap, are returned unhighlighted to keep
//! large diffs responsive. Per-line highlighting trades some accuracy on
//! multi-line constructs (strings/comments) for simplicity; tree-sitter is the
//! noted later upgrade.
//!
//! Highlighting runs off the render path: [`spans`](Highlighter::spans) returns
//! a cached result or plain text immediately, recording each miss. After the
//! draw, [`dispatch`](Highlighter::dispatch) hands the misses to a background
//! worker thread; a finished [`Batch`] arrives on [`results`](Highlighter::results)
//! and is folded back in by [`merge`](Highlighter::merge), so the next draw
//! paints those lines colored. A line is computed at most once.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::theme::ThemeMode;

/// Longest line that will be highlighted; longer lines render plain.
const MAX_LINE_LEN: usize = 4096;

/// Lines per message handed to the worker. Bounds how much a bulk prewarm can
/// delay an interleaved viewport miss, and makes coloring fill in progressively.
const CHUNK: usize = 128;

/// A foreground color span over a slice of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub color: Color,
    pub text: String,
}

/// Cache key: the resolved syntax name paired with the verbatim line.
type Key = (String, String);

/// A worker-computed set of highlighted lines, folded into the cache by
/// [`Highlighter::merge`].
pub type Batch = HashMap<Key, Vec<Span>>;

/// Caching syntect highlighter bound to one [`ThemeMode`], computing misses on a
/// background thread.
pub struct Highlighter {
    syntaxes: Arc<SyntaxSet>,
    default_fg: Color,
    cache: RefCell<HashMap<Key, Vec<Span>>>,
    /// Keys already handed to the worker (in flight or merged), so each line is
    /// requested at most once.
    requested: RefCell<HashSet<Key>>,
    /// `(extension, line)` misses gathered during the current draw, drained by
    /// [`dispatch`](Self::dispatch).
    queue: RefCell<Vec<(String, String)>>,
    work_tx: async_channel::Sender<Vec<(String, String)>>,
    results: async_channel::Receiver<Batch>,
}

impl Highlighter {
    /// Build a highlighter using syntect's bundled syntaxes and a theme paired
    /// to `mode`, spawning the worker thread that computes highlights.
    pub fn new(mode: ThemeMode, default_fg: Color) -> Self {
        let themes = ThemeSet::load_defaults();
        let name = match mode {
            ThemeMode::Dark => "base16-ocean.dark",
            ThemeMode::Light => "InspiredGitHub",
        };
        let theme = themes.themes[name].clone();
        let syntaxes = Arc::new(SyntaxSet::load_defaults_newlines());

        let (work_tx, work_rx) = async_channel::unbounded::<Vec<(String, String)>>();
        let (result_tx, results) = async_channel::unbounded::<Batch>();

        {
            let syntaxes = syntaxes.clone();
            let theme = theme.clone();
            std::thread::spawn(move || worker(syntaxes, theme, default_fg, work_rx, result_tx));
        }

        Self {
            syntaxes,
            default_fg,
            cache: RefCell::new(HashMap::new()),
            requested: RefCell::new(HashSet::new()),
            queue: RefCell::new(Vec::new()),
            work_tx,
            results,
        }
    }

    /// Highlight one line of a file with the given extension. Returns the cached
    /// colored spans, or a single default-colored span when the result is not
    /// ready yet (recording the miss), the language is unknown, or the line is
    /// too long.
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

        self.enqueue(extension, line);
        plain()
    }

    /// Schedule every line for highlighting, so scrolling past the viewport never
    /// flashes plain. Already-cached or already-requested lines are skipped, so
    /// repeat calls (e.g. after expanding context) only add the new lines.
    pub fn prewarm(&self, lines: impl IntoIterator<Item = (String, String)>) {
        for (extension, line) in lines {
            self.enqueue(&extension, &line);
        }
    }

    /// Record a miss once, to be highlighted on the worker. Lines too long or in
    /// an unknown language never enqueue — [`spans`](Self::spans) renders them
    /// plain outright.
    fn enqueue(&self, extension: &str, line: &str) {
        if line.len() > MAX_LINE_LEN {
            return;
        }

        let Some(syntax) = self.syntax_for(extension) else {
            return;
        };

        // `requested` gates both queueing and (via the worker) the cache, so a
        // line is computed at most once across viewport misses and prewarms.
        if self
            .requested
            .borrow_mut()
            .insert((syntax.name.clone(), line.to_string()))
        {
            self.queue
                .borrow_mut()
                .push((extension.to_string(), line.to_string()));
        }
    }

    /// Hand the queued misses to the worker in [`CHUNK`]-sized messages. Called
    /// once after each draw; a no-op when nothing missed. Chunking keeps a bulk
    /// prewarm from blocking later viewport misses behind one giant compute.
    pub fn dispatch(&self) {
        let queued: Vec<_> = self.queue.borrow_mut().drain(..).collect();

        for chunk in queued.chunks(CHUNK) {
            let _ = self.work_tx.try_send(chunk.to_vec());
        }
    }

    /// The channel finished [`Batch`]es arrive on; the event loop awaits it.
    pub fn results(&self) -> &async_channel::Receiver<Batch> {
        &self.results
    }

    /// Fold a finished batch into the cache; the next draw paints these colored.
    pub fn merge(&self, batch: Batch) {
        self.cache.borrow_mut().extend(batch);
    }

    /// Drain the queued misses through the worker and block until they merge, so
    /// a follow-up draw is fully colored. For synchronous rendering (the
    /// screenshot dump) where there is no event loop to receive a [`Batch`].
    #[cfg(test)]
    pub fn warm_blocking(&self) {
        let queued: Vec<_> = self.queue.borrow_mut().drain(..).collect();

        if queued.is_empty() {
            return;
        }

        let _ = self.work_tx.try_send(queued);

        if let Ok(batch) = self.results.recv_blocking() {
            self.merge(batch);
        }
    }

    fn syntax_for(&self, extension: &str) -> Option<&SyntaxReference> {
        self.syntaxes.find_syntax_by_extension(extension)
    }
}

/// Drain batches of `(extension, line)` misses, highlight each line, and send the
/// results back. Exits when the [`Highlighter`] drops and the work channel closes.
fn worker(
    syntaxes: Arc<SyntaxSet>,
    theme: Theme,
    default_fg: Color,
    work: async_channel::Receiver<Vec<(String, String)>>,
    results: async_channel::Sender<Batch>,
) {
    while let Ok(items) = work.recv_blocking() {
        let mut batch = Batch::new();

        for (extension, line) in items {
            let Some(syntax) = syntaxes.find_syntax_by_extension(&extension) else {
                continue;
            };
            let key = (syntax.name.clone(), line.clone());
            batch
                .entry(key)
                .or_insert_with(|| compute(&syntaxes, &theme, default_fg, syntax, &line));
        }

        if results.send_blocking(batch).is_err() {
            break;
        }
    }
}

/// Highlight one line into colored spans, falling back to a single
/// `default_fg` span when syntect errors.
fn compute(
    syntaxes: &SyntaxSet,
    theme: &Theme,
    default_fg: Color,
    syntax: &SyntaxReference,
    line: &str,
) -> Vec<Span> {
    let mut highlighter = HighlightLines::new(syntax, theme);

    match highlighter.highlight_line(line, syntaxes) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| Span {
                color: Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b),
                text: text.to_string(),
            })
            .collect(),
        Err(_) => vec![Span {
            color: default_fg,
            text: line.to_string(),
        }],
    }
}
