//! The `ratatui` terminal UI (PRD §11).
//!
//! [`run`] owns terminal setup/teardown and the input loop; all model logic
//! lives in [`app`] and all drawing in [`ui`], so the interesting parts are
//! testable with a `ratatui::TestBackend` and without a real terminal.

mod agent;
mod app;
mod highlight;
mod keymap;
#[cfg(test)]
mod svg;
mod theme;
mod ui;

pub use app::App;
pub use theme::ThemeMode;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use futures_lite::{StreamExt, future};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::crossterm::event::{Event, EventStream, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::vcs::{Backend, Base, Vcs};
use agent::AgentEvent;
use highlight::Highlighter;

/// Launch the TUI against `backend`, listing commits per `base`. `theme` is the
/// explicit `--theme`/config override, if any; otherwise the terminal is queried.
pub fn run(backend: Backend, base: Base, theme: Option<ThemeMode>) -> Result<()> {
    // Resolve the theme before the alternate screen: some terminals (e.g.
    // WezTerm) only answer the OSC 11 background query on the normal screen.
    // Raw mode is needed to read the reply.
    let theme = resolve_theme(theme);

    let mut terminal = ratatui::init();
    let result = future::block_on(build_and_run(&mut terminal, backend, base, theme));
    ratatui::restore();
    result
}

/// Resolve the theme, enabling raw mode for the terminal query when no explicit
/// choice short-circuits it. Runs before [`ratatui::init`] enters the alternate
/// screen.
fn resolve_theme(explicit: Option<ThemeMode>) -> ThemeMode {
    if explicit.is_some() {
        return ThemeMode::resolve(explicit);
    }

    let raw_enabled = enable_raw_mode().is_ok();
    let theme = ThemeMode::resolve(explicit);

    if raw_enabled {
        let _ = disable_raw_mode();
    }

    theme
}

async fn build_and_run(
    terminal: &mut ratatui::DefaultTerminal,
    backend: Backend,
    base: Base,
    theme: ThemeMode,
) -> Result<()> {
    let repo_root = backend.root().to_path_buf();
    let mut app = App::new(backend, base, theme)?;
    let highlighter = Highlighter::new(theme, app.palette.default_fg);

    // Watch the annotation log so an agent's out-of-band writes reload live. The
    // watcher must outlive the loop; dropping it stops the watch. A failed setup
    // degrades to manual reload (`R`) rather than aborting the TUI.
    let (sender, receiver) = async_channel::unbounded::<()>();
    let _watcher = match watch_store(&repo_root, sender) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            app.status_message = Some(format!("live reload off: {error}"));
            None
        }
    };

    // Channel the headless agent's streamed events flow through. The app keeps
    // the sender for the loop's lifetime, so the receiver never closes.
    let (agent_sender, agent_receiver) = async_channel::unbounded::<AgentEvent>();
    app.set_agent_channel(agent_sender);

    event_loop(terminal, &mut app, &highlighter, receiver, agent_receiver).await
}

/// Watch `.margin/` for changes to the annotation log, sending `()` on each so
/// the event loop can reload. Watches the directory (the log itself may not
/// exist yet) and filters to the log filename.
fn watch_store(repo_root: &Path, sender: async_channel::Sender<()>) -> Result<RecommendedWatcher> {
    let dir = repo_root.join(".margin");
    std::fs::create_dir_all(&dir)?;

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && event
                .paths
                .iter()
                .any(|p| p.ends_with("annotations.ndjson"))
        {
            let _ = sender.try_send(());
        }
    })?;

    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Whichever event source fired the wake-up.
enum Wake {
    Terminal(Option<std::io::Result<Event>>),
    File,
    Agent(AgentEvent),
}

/// Drive the UI: redraw, then await the next terminal event or filesystem
/// notification, whichever comes first. No polling — both sources wake the task
/// via their own wakers.
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    highlighter: &Highlighter,
    file_changes: async_channel::Receiver<()>,
    agent_events: async_channel::Receiver<AgentEvent>,
) -> Result<()> {
    let mut reader = EventStream::new();

    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, app, highlighter))?;

        // `EventStream::next` is cancel-safe, so dropping the losing future when
        // the race resolves loses no input. `future::race` is binary, so the file
        // and agent sources are nested into the second arm.
        let wake = future::race(
            async { Wake::Terminal(reader.next().await) },
            future::race(
                async {
                    let _ = file_changes.recv().await;
                    Wake::File
                },
                async {
                    match agent_events.recv().await {
                        Ok(event) => Wake::Agent(event),
                        // The sender lives in `app` for the loop's lifetime, so
                        // this only happens at shutdown; idle out the race.
                        Err(_) => future::pending().await,
                    }
                },
            ),
        )
        .await;

        match wake {
            Wake::Terminal(Some(Ok(Event::Key(key)))) => {
                if key.kind == KeyEventKind::Press
                    && let Some(action) = keymap::map(key, app.is_editing())
                {
                    app.apply(action);

                    if app.take_external_edit_request() {
                        edit_in_external(terminal, app);
                    }
                }
            }
            // Resize and other events fall through to the redraw at the loop top.
            Wake::Terminal(Some(Ok(_))) => {}
            Wake::Terminal(Some(Err(error))) => return Err(error.into()),
            Wake::Terminal(None) => break,
            Wake::File => {
                app.reload_if_changed();
            }
            Wake::Agent(event) => app.on_agent_event(event),
        }
    }

    Ok(())
}

/// Suspend the TUI, compose the open editor's body in `$EDITOR`, and feed the
/// result back. The terminal is restored and repainted on every path so a failed
/// launch never wedges it.
fn edit_in_external(terminal: &mut ratatui::DefaultTerminal, app: &mut App) {
    let Some(seed) = app.editor_seed() else {
        return;
    };

    let outcome = suspend_and_edit(&seed);

    // Re-enter the alternate screen and repaint regardless of the outcome.
    let _ = enable_raw_mode();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = terminal.clear();

    match outcome {
        Ok(edited) => app.apply_external_edit(edited),
        Err(error) => app.status_message = Some(format!("editor failed: {error}")),
    }
}

/// Leave the alternate screen, run `$EDITOR` on a temp file seeded from `seed`,
/// and return the stripped result. Re-entering the screen is the caller's job.
fn suspend_and_edit(seed: &app::EditorSeed) -> std::io::Result<String> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;

    let path = std::env::temp_dir().join(format!("margin-annotation-{}.md", std::process::id()));
    std::fs::write(&path, app::editor_template(seed))?;

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    // `sh -c '<editor> "$1"' sh <path>` passes the path as a single argument,
    // so editors carrying flags (e.g. `code -w`) still work.
    Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()?;

    let edited = std::fs::read_to_string(&path).map(|content| app::strip_template(&content));
    let _ = std::fs::remove_file(&path);
    edited
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    fn fixture() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@example.com"]);
        git(path, &["config", "user.name", "T"]);
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "base"]);

        git(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(path.join("lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "Add lib"]);

        repo
    }

    /// Commit the staged tree with author/committer identity *and dates* pinned,
    /// so the demo repo's commit hashes are byte-stable run to run.
    fn commit_pinned(dir: &Path, message: &str) {
        assert!(
            Command::new("git")
                .current_dir(dir)
                .args(["commit", "-q", "-m", message])
                .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05 +0000")
                .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05 +0000")
                .status()
                .unwrap()
                .success()
        );
    }

    /// A realistic single-commit change (a token-bucket limiter gaining
    /// time-based refill) used to render the README screenshots. Larger and more
    /// representative than [`fixture`], with a visible add/remove hunk for syntax
    /// highlighting and diff tints.
    fn demo_fixture() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "reviewer@example.com"]);
        git(path, &["config", "user.name", "Reviewer"]);

        let base = "\
use std::time::Instant;

/// A token-bucket rate limiter.
pub struct Limiter {
    capacity: u32,
    tokens: u32,
    last: Instant,
}

impl Limiter {
    pub fn new(capacity: u32) -> Self {
        Self { capacity, tokens: capacity, last: Instant::now() }
    }

    /// Take one token, returning whether the call is allowed.
    pub fn try_acquire(&mut self) -> bool {
        if self.tokens == 0 {
            return false;
        }

        self.tokens -= 1;
        true
    }
}
";
        std::fs::write(path.join("rate.rs"), base).unwrap();
        git(path, &["add", "-A"]);
        commit_pinned(path, "Add a token-bucket limiter");

        git(path, &["checkout", "-q", "-b", "feature"]);
        let feature = "\
use std::time::Instant;

/// A token-bucket rate limiter.
pub struct Limiter {
    capacity: u32,
    tokens: u32,
    last: Instant,
}

impl Limiter {
    pub fn new(capacity: u32) -> Self {
        Self { capacity, tokens: capacity, last: Instant::now() }
    }

    /// Take one token, returning whether the call is allowed.
    pub fn try_acquire(&mut self) -> bool {
        self.refill();

        if self.tokens == 0 {
            return false;
        }

        self.tokens -= 1;
        true
    }

    /// Add back the tokens accrued since the last call.
    fn refill(&mut self) {
        let elapsed = self.last.elapsed().as_secs() as u32;
        self.tokens = (self.tokens + elapsed).min(self.capacity);
        self.last = Instant::now();
    }
}
";
        std::fs::write(path.join("rate.rs"), feature).unwrap();
        git(path, &["add", "-A"]);
        commit_pinned(path, "Refill the token bucket as time passes");

        repo
    }

    /// The `app.rows` index of the added diff line whose content contains
    /// `needle`.
    fn added_row(app: &App, needle: &str) -> usize {
        app.rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    super::app::Row::Line { line, .. }
                        if line.kind == crate::vcs::DiffLineKind::Added
                            && line.content.contains(needle)
                )
            })
            .unwrap_or_else(|| panic!("no added line matching {needle:?}"))
    }

    /// Drive the editor to annotate the added line matching `needle`, pressing
    /// `cycle_type` times to pick a type (None→Fix→Question→Suggestion→…).
    fn annotate(app: &mut App, needle: &str, body: &str, cycle_type: usize) {
        app.diff_cursor = added_row(app, needle);
        app.apply(keymap::Action::Annotate);

        for ch in body.chars() {
            app.apply(keymap::Action::EditorChar(ch));
        }

        for _ in 0..cycle_type {
            app.apply(keymap::Action::EditorCycleType);
        }

        app.apply(keymap::Action::EditorSave);
    }

    /// Like [`annotate`], but over a visual selection that starts at `needle` and
    /// extends `extra_lines` rows down, so the annotation spans a line range.
    fn annotate_range(
        app: &mut App,
        needle: &str,
        extra_lines: usize,
        body: &str,
        cycle_type: usize,
    ) {
        app.diff_cursor = added_row(app, needle);
        app.apply(keymap::Action::StartSelection);

        for _ in 0..extra_lines {
            app.apply(keymap::Action::Down);
        }

        app.apply(keymap::Action::Annotate);

        for ch in body.chars() {
            app.apply(keymap::Action::EditorChar(ch));
        }

        for _ in 0..cycle_type {
            app.apply(keymap::Action::EditorCycleType);
        }

        app.apply(keymap::Action::EditorSave);
    }

    /// Draw `app` to a fresh `width`×`height` backend and return the buffer.
    fn draw(
        app: &mut App,
        highlighter: &Highlighter,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        app.diff_top = 0;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, app, highlighter))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The last row carrying any content (a glyph or a tinted background),
    /// ignoring the help row pinned to the bottom — so the canvas can be trimmed
    /// to exactly the rendered scene with no empty rows.
    fn last_content_row(buffer: &ratatui::buffer::Buffer) -> usize {
        let area = buffer.area;

        (0..area.height.saturating_sub(1))
            .rev()
            .find(|&y| {
                (0..area.width).any(|x| {
                    buffer.cell((area.x + x, area.y + y)).is_some_and(|c| {
                        (c.symbol() != " " && !c.symbol().is_empty())
                            || c.bg != ratatui::style::Color::Reset
                    })
                })
            })
            .map(usize::from)
            .unwrap_or(0)
    }

    /// Build the screenshot scene: the demo diff with two annotations — a
    /// multi-line question and a fix already resolved by the agent — and the band
    /// showing the annotation overview.
    fn render_demo_svg(mode: ThemeMode) -> String {
        use crate::model::{Actor, EventKind};
        use crate::store::Store;

        let repo = demo_fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), mode).unwrap();

        annotate(
            &mut app,
            "self.refill();",
            "Run refill() before the early return",
            1, // Fix
        );
        annotate_range(
            &mut app,
            "as_secs() as u32",
            2, // three lines: the whole refill body
            "Whole-second truncation drops sub-second refills",
            2, // Question
        );

        // Mark the fix resolved by the agent, the way a handoff would.
        let fix = app
            .annotations()
            .iter()
            .find(|resolved| resolved.annotation.body.starts_with("Run refill()"))
            .map(crate::review::ResolvedAnnotation::id)
            .expect("the fix annotation");
        let store = Store::open(repo.path());
        store
            .append(&crate::model::Event::now(
                fix,
                Actor::Agent,
                EventKind::AgentResolved {
                    reply: Some("Moved refill() above the early return.".into()),
                },
            ))
            .unwrap();
        app.reload();

        // Focus the band on the annotation overview, framed from the file header
        // so both annotations are in shot.
        app.apply(keymap::Action::ViewAnnotations);

        // Open the editor on the top annotation so the note-entry box is in shot
        // alongside the resolved fix and the range question below it.
        app.diff_cursor = added_row(&app, "self.refill();");
        app.apply(keymap::Action::Annotate);

        // Render once tall to measure the scene, then trim the height so the diff
        // fills the frame with no empty rows below it.
        let highlighter = Highlighter::new(mode, app.palette.default_fg);
        let probe = draw(&mut app, &highlighter, 100, 60);
        let height = last_content_row(&probe) as u16 + 2;

        svg::buffer_to_svg(&draw(&mut app, &highlighter, 100, height), mode)
    }

    #[test]
    #[ignore = "regenerates the README screenshots; run with --ignored"]
    fn dump_screenshot() {
        for (mode, file) in [
            (ThemeMode::Dark, "docs/screenshot-dark.svg"),
            (ThemeMode::Light, "docs/screenshot-light.svg"),
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
            std::fs::write(path, render_demo_svg(mode)).unwrap();
        }
    }

    #[test]
    fn renders_without_panicking() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Add lib"),
            "sidebar should list the commit"
        );
    }

    #[test]
    fn empty_revision_shows_no_changes_note() {
        let repo = fixture();
        let path = repo.path();
        git(
            path,
            &["commit", "-q", "--allow-empty", "-m", "empty change"],
        );

        let backend = Backend::discover(path, Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("no changes in this revision"),
            "empty revision should show the no-changes note:\n{rendered}"
        );
    }

    #[test]
    #[ignore = "visual preview; run with --ignored --nocapture"]
    fn dump_preview() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        for c in "this fn needs a doc comment".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorCycleType);
        app.apply(keymap::Action::EditorSave);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        println!("\n{}", terminal.backend());
    }

    #[test]
    fn cursor_paints_background_on_a_non_code_row() {
        // The first diff row is a File header; with the diff focused and the
        // cursor on it, that visual row must carry the cursor background so the
        // cursor stays visible on non-code lines.
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit); // focus diff, cursor on row 0 (File)

        let cursor_bg = app.palette.cursor_bg;
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        // The diff spans the full width directly below the band rule (the `┴`
        // row), with no header of its own; its first row is the File header
        // holding the cursor.
        let rule_y = terminal
            .backend()
            .to_string()
            .lines()
            .position(|line| line.contains('┴'))
            .expect("band rule row") as u16;
        let cursor_y = rule_y + 1;
        let buffer = terminal.backend().buffer();
        let painted = (0..120).any(|x| buffer.cell((x, cursor_y)).map(|c| c.bg) == Some(cursor_bg));
        assert!(
            painted,
            "cursor row should be highlighted:\n{}",
            terminal.backend()
        );
    }

    #[test]
    fn expanding_context_reveals_surrounding_lines() {
        use super::app::Row;

        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@example.com"]);
        git(path, &["config", "user.name", "T"]);
        let original: String = (1..=12).map(|n| format!("line{n}\n")).collect();
        std::fs::write(path.join("code.rs"), &original).unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "base"]);

        git(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            path.join("code.rs"),
            original.replace("line6\n", "line6_changed\n"),
        )
        .unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "change"]);

        let backend = Backend::discover(path, Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::NextChange);

        let has = |app: &App, content: &str| {
            app.rows
                .iter()
                .any(|r| matches!(r, Row::Line { line, .. } if line.content == content))
        };

        let before = app.rows.len();
        assert!(
            !has(&app, "line1") && !has(&app, "line12"),
            "file edges hidden by default"
        );

        app.apply(keymap::Action::ExpandContext);
        assert!(app.rows.len() > before, "expansion adds rows");
        assert!(
            has(&app, "line1") && has(&app, "line12"),
            "expansion reveals the file edges"
        );

        app.apply(keymap::Action::CollapseContext);
        assert_eq!(
            app.rows.len(),
            before,
            "collapse restores the original rows"
        );
    }

    #[test]
    fn overview_navigation_reveals_without_taking_focus() {
        use super::app::Focus;

        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        // Two annotations on different lines of the same commit.
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        app.apply(keymap::Action::EditorChar('a'));
        app.apply(keymap::Action::EditorSave);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        app.apply(keymap::Action::EditorChar('b'));
        app.apply(keymap::Action::EditorSave);

        app.apply(keymap::Action::ViewAnnotations);
        let first = app.diff_cursor;

        app.apply(keymap::Action::Down);
        assert!(
            matches!(app.focus, Focus::Band),
            "overview keeps focus in the band"
        );
        assert_ne!(
            app.diff_cursor, first,
            "moving the overview row moves the diff cursor"
        );
    }

    #[test]
    fn next_change_lands_on_a_changed_line() {
        use super::app::Row;
        use crate::vcs::DiffLineKind;

        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);

        app.apply(keymap::Action::NextChange);
        let cursor = app.diff_cursor;
        assert!(
            matches!(&app.rows[cursor], Row::Line { line, .. } if !matches!(line.kind, DiffLineKind::Context)),
            "should land on an added or removed line",
        );
    }

    #[test]
    fn half_page_uses_the_recorded_viewport_height() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);

        // A render records the viewport height; without it paging cannot move.
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        assert!(app.diff_viewport_height > 0);
        app.apply(keymap::Action::HalfPageDown);
        assert!(
            app.diff_cursor > 0,
            "half-page down should advance the cursor"
        );
    }

    #[test]
    fn select_and_annotate_writes_an_event() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        // Focus the diff, move onto an added line, annotate it.
        app.apply(keymap::Action::SelectCommit);
        for _ in 0..3 {
            app.apply(keymap::Action::Down);
        }
        app.apply(keymap::Action::Annotate);
        assert!(app.is_editing(), "annotate should open the editor");

        for c in "needs a doc comment".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorCycleType);
        app.apply(keymap::Action::EditorSave);

        assert!(!app.is_editing(), "save should close the editor");
        assert_eq!(app.annotations().len(), 1);
        assert_eq!(app.annotations()[0].annotation.body, "needs a doc comment");
    }

    #[test]
    fn annotating_an_already_annotated_line_edits_in_place() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        app.apply(keymap::Action::SelectCommit);
        for _ in 0..3 {
            app.apply(keymap::Action::Down);
        }
        app.apply(keymap::Action::Annotate);
        for c in "first".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorSave);
        assert_eq!(app.annotations().len(), 1);

        // Annotating the same line again edits the existing annotation rather than
        // stacking a duplicate.
        app.apply(keymap::Action::Annotate);
        assert!(
            app.is_editing(),
            "should reopen the editor on the existing annotation"
        );
        app.apply(keymap::Action::EditorChar('!'));
        app.apply(keymap::Action::EditorSave);

        assert_eq!(
            app.annotations().len(),
            1,
            "no duplicate annotation is created"
        );
        assert_eq!(app.annotations()[0].annotation.body, "first!");
    }

    /// Build an app on the fixture with a single annotation on the first added
    /// line of the feature commit.
    fn app_with_annotation(repo: &Path) -> App {
        let backend = Backend::discover(repo, Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        for c in "needs docs".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorSave);
        app
    }

    #[test]
    fn reload_reflects_an_out_of_band_status_write() {
        use super::app::Marker;
        use crate::model::{Actor, Event, EventKind, Status};
        use crate::store::Store;

        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        let resolved = &app.annotations()[0];
        let id = resolved.id();
        let anchor = &resolved.annotation.anchor;
        let line = anchor.start_line.get();
        let file_index = app.file_index_of(&anchor.file).unwrap();
        let side = anchor.side;

        assert_eq!(app.annotations()[0].status, Status::Open);
        assert_eq!(
            app.line_marker(file_index, side, line).map(|m| m.marker),
            Some(Marker::Open)
        );

        // The agent resolves it in a separate process.
        Store::open(repo.path())
            .append(&Event::now(
                id,
                Actor::Agent,
                EventKind::AgentResolved { reply: None },
            ))
            .unwrap();

        app.reload();

        assert_eq!(app.annotations()[0].status, Status::Resolved);
        assert_eq!(
            app.line_marker(file_index, side, line).map(|m| m.marker),
            Some(Marker::Resolved),
            "the gutter marker reflects the out-of-band resolution"
        );
    }

    #[test]
    fn reload_keeps_the_cursor_on_the_same_commit() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        let before = app.current_revision().unwrap().id.clone();

        app.reload();

        assert_eq!(
            app.current_revision().unwrap().id,
            before,
            "reload re-lists without moving off the selected commit"
        );
    }

    #[test]
    fn reload_if_changed_only_fires_on_a_write() {
        use crate::model::{Actor, Event, EventKind};
        use crate::store::Store;

        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        assert!(!app.reload_if_changed(), "no write means no reload");

        Store::open(repo.path())
            .append(&Event::now(
                app.annotations()[0].id(),
                Actor::Agent,
                EventKind::AgentResolved { reply: None },
            ))
            .unwrap();

        assert!(
            app.reload_if_changed(),
            "an out-of-band write triggers a reload"
        );
        assert!(
            !app.reload_if_changed(),
            "a second check with no further write does not reload again"
        );
    }

    #[test]
    fn agent_events_build_the_log_and_clear_running() {
        use super::agent::{AgentEvent, Outcome};

        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        app.agent.running = true;

        app.on_agent_event(AgentEvent::Started);
        app.on_agent_event(AgentEvent::ToolUse {
            name: "Edit".into(),
            summary: "lib.rs".into(),
        });

        assert!(app.agent.running, "still running mid-session");
        assert!(
            app.agent.log.iter().any(|line| line.contains("Edit")),
            "tool use is logged: {:?}",
            app.agent.log
        );

        app.on_agent_event(AgentEvent::Finished {
            outcome: Outcome::Ok,
            summary: "done".into(),
        });

        assert!(!app.agent.running, "a finish clears the running flag");
        assert_eq!(app.status_message.as_deref(), Some("agent finished"));
    }

    #[test]
    fn toggling_the_agent_log_flips_visibility() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        assert!(!app.agent.log_visible);
        app.apply(keymap::Action::ToggleAgentLog);
        assert!(app.agent.log_visible);
        app.apply(keymap::Action::ToggleAgentLog);
        assert!(!app.agent.log_visible);
    }

    #[test]
    fn spawning_an_agent_without_a_channel_reports_unavailable() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        // No event loop wired the channel, so the trigger is rejected cleanly
        // rather than launching a process.
        app.apply(keymap::Action::SpawnAgentForOpen);

        assert!(!app.agent.running);
        assert_eq!(
            app.status_message.as_deref(),
            Some("agent unavailable in this context")
        );
    }

    #[test]
    fn reviewer_reopens_an_agent_resolution() {
        use crate::model::Status;
        use crate::model::{Actor, AnnotationId, Event, EventKind};
        use crate::store::Store;

        let repo = fixture();
        let id: AnnotationId = app_with_annotation(repo.path()).annotations()[0].id();

        // The agent resolves it out of band.
        Store::open(repo.path())
            .append(&Event::now(
                id,
                Actor::Agent,
                EventKind::AgentResolved { reply: None },
            ))
            .unwrap();

        // Reopen via the overview, which focuses the selected annotation.
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        assert_eq!(app.annotations()[0].status, Status::Resolved);

        app.apply(keymap::Action::ViewAnnotations);
        app.apply(keymap::Action::Reopen);

        assert_eq!(app.annotations()[0].status, Status::Open);
        assert_eq!(app.annotations()[0].annotation.timeline.len(), 3);
    }

    #[test]
    fn annotation_and_editor_render_inline_in_the_diff() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();

        // The saved annotation shows inline beneath its line, not in a footer box.
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();
        assert!(
            terminal.backend().to_string().contains("needs docs"),
            "inline annotation"
        );

        // Opening the editor renders it inline too (its save hint is visible).
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        assert!(app.is_editing());
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();
        assert!(
            terminal.backend().to_string().contains("ctrl-s save"),
            "inline editor"
        );
    }

    #[test]
    fn editor_paints_the_cursor_under_a_mid_line_character() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        for c in "abZ".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorLeft); // cursor lands on 'Z'

        let cursor_bg = app.palette.text_cursor_bg;
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let painted = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                buffer
                    .cell((x, y))
                    .is_some_and(|cell| cell.bg == cursor_bg && cell.symbol() == "Z")
            })
        });
        assert!(
            painted,
            "the editor cursor cell carries the cursor background:\n{}",
            terminal.backend()
        );
    }

    #[test]
    fn editor_seed_quotes_the_annotated_source_line() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::NextChange); // land on an added line
        app.apply(keymap::Action::Annotate);

        let seed = app.editor_seed().expect("editor is open");
        assert!(
            seed.source_lines
                .iter()
                .any(|line| line.contains("fn a()") || line.contains("fn b()")),
            "the seed quotes the annotated source: {:?}",
            seed.source_lines
        );
    }

    #[test]
    fn deleting_removes_the_annotation() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        assert_eq!(app.annotations().len(), 1);

        // Open the sidebar overview and delete the selected annotation.
        app.apply(keymap::Action::ViewAnnotations);
        app.apply(keymap::Action::Delete);

        assert!(app.annotations().is_empty(), "annotation should fold away");

        let ndjson =
            std::fs::read_to_string(repo.path().join(".margin/annotations.ndjson")).unwrap();
        assert!(ndjson.contains("annotation_deleted"), "{ndjson}");
    }

    #[test]
    fn deleting_from_the_diff_cursor_removes_the_annotation() {
        let repo = fixture();
        // The helper leaves the diff focused with the cursor on the annotated line.
        let mut app = app_with_annotation(repo.path());
        assert_eq!(app.annotations().len(), 1);

        app.apply(keymap::Action::Delete);

        assert!(
            app.annotations().is_empty(),
            "delete should fold the annotation away"
        );
    }

    #[test]
    fn undo_restores_a_deleted_annotation() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        app.apply(keymap::Action::Delete);
        assert!(
            app.annotations().is_empty(),
            "delete folds the annotation away"
        );

        app.apply(keymap::Action::Undo);
        assert_eq!(app.annotations().len(), 1, "undo brings it back");

        let ndjson =
            std::fs::read_to_string(repo.path().join(".margin/annotations.ndjson")).unwrap();
        assert!(ndjson.contains("annotation_restored"), "{ndjson}");
    }

    #[test]
    fn jumping_from_the_overview_focuses_the_diff_line() {
        use super::app::{Focus, Row};

        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        let anchored = app.annotations()[0].annotation.anchor.start_line.get();

        // Move off the commit, open the overview, and jump to the annotation.
        app.apply(keymap::Action::ViewAnnotations);
        app.apply(keymap::Action::Confirm);

        assert!(
            matches!(app.focus, Focus::Diff),
            "jump should focus the diff"
        );
        let cursor_line = match &app.rows[app.diff_cursor] {
            Row::Line { line, .. } => line.new_no.map(|n| n.get()),
            _ => None,
        };
        assert_eq!(
            cursor_line,
            Some(anchored),
            "cursor should land on the anchor line"
        );
    }

    #[test]
    fn line_markers_do_not_bleed_across_files_on_the_same_line() {
        use crate::anchor::{CONTEXT_LINES, capture};
        use crate::model::{Actor, AnnotationId, Event, EventKind, LineNumber, RepoRelPath, Side};
        use crate::store::Store;
        use crate::vcs::Vcs;

        // A commit touching two files whose line 1 is identical: a file-blind
        // marker keyed only by line number would light up both.
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@example.com"]);
        git(path, &["config", "user.name", "T"]);
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "base"]);

        git(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(path.join("a.rs"), "fn shared() {}\n").unwrap();
        std::fs::write(path.join("b.rs"), "fn shared() {}\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "Add two files"]);

        let backend = Backend::discover(path, Some(crate::vcs::Kind::Git)).unwrap();
        let revision = backend
            .revisions(&Base::Branch("main".into()))
            .unwrap()
            .revisions[0]
            .id
            .clone();
        let b_path = RepoRelPath(std::path::PathBuf::from("b.rs"));
        let source = backend.file_at(&revision, &b_path).unwrap();
        let commit = backend.commit_of(&revision).unwrap();
        let anchor = capture(
            b_path.clone(),
            revision,
            commit,
            Side::New,
            &source,
            LineNumber::new(1).unwrap(),
            LineNumber::new(1).unwrap(),
            CONTEXT_LINES,
        )
        .unwrap();
        Store::open(path)
            .append(&Event::now(
                AnnotationId::new(),
                Actor::Reviewer,
                EventKind::AnnotationCreated {
                    anchor,
                    body: "on b only".into(),
                    annotation_type: None,
                },
            ))
            .unwrap();

        let backend = Backend::discover(path, Some(crate::vcs::Kind::Git)).unwrap();
        let app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        let a_index = app
            .file_index_of(&RepoRelPath(std::path::PathBuf::from("a.rs")))
            .unwrap();
        let b_index = app.file_index_of(&b_path).unwrap();

        assert!(
            app.line_marker(b_index, Side::New, 1).is_some(),
            "b.rs:1 carries the marker"
        );
        assert!(
            app.line_marker(a_index, Side::New, 1).is_none(),
            "a.rs:1 must not inherit it"
        );
    }

    #[test]
    fn editing_appends_an_edit_event() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        app.apply(keymap::Action::ViewAnnotations);
        app.apply(keymap::Action::Edit);
        assert!(app.is_editing());

        for _ in 0.."needs docs".len() {
            app.apply(keymap::Action::EditorBackspace);
        }
        for c in "updated text".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorSave);

        assert_eq!(app.annotations()[0].annotation.body, "updated text");
        assert_eq!(app.annotations()[0].annotation.timeline.len(), 2);
    }

    #[test]
    fn timeline_overlay_renders_event_history() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        app.apply(keymap::Action::ViewAnnotations);
        app.apply(keymap::Action::Timeline);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("timeline"), "{rendered}");
        assert!(rendered.contains("created"), "{rendered}");
    }

    #[test]
    fn timeline_popup_does_not_cover_the_annotation() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        // The diff cursor still sits on the annotated line after saving.
        app.apply(keymap::Action::Timeline);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("timeline"), "{rendered}");
        // The popup anchors beside the annotation, so the annotated source line
        // stays visible rather than being covered.
        assert!(rendered.contains("fn a() {}"), "{rendered}");
    }

    #[test]
    fn timeline_renders_a_multiline_body() {
        let repo = fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();

        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        for c in "first line".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorNewline);
        for c in "second line".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorSave);
        app.apply(keymap::Action::Timeline);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        // The type label sits in the heading and the second body line renders as
        // its own connector row.
        assert!(rendered.contains("note"), "{rendered}");
        assert!(rendered.contains("second line"), "{rendered}");
    }

    /// A repo whose feature commit modifies one line (apple -> banana) and adds
    /// a brand-new line, so the diff has a paired change and a pure addition.
    fn modification_fixture() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@example.com"]);
        git(path, &["config", "user.name", "T"]);
        let original: String = (1..=12)
            .map(|n| {
                if n == 6 {
                    "apple\n".into()
                } else {
                    format!("line{n}\n")
                }
            })
            .collect();
        std::fs::write(path.join("code.rs"), &original).unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "base"]);

        git(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            path.join("code.rs"),
            original.replace("apple\n", "banana\ncherry\n"),
        )
        .unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "change"]);

        repo
    }

    /// Find the rendered row (as a string) that contains `needle`.
    fn row_with(terminal: &Terminal<TestBackend>, needle: &str) -> String {
        terminal
            .backend()
            .to_string()
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no rendered row contains {needle:?}"))
            .to_string()
    }

    #[test]
    fn split_pairs_removed_and_added_on_one_row() {
        let repo = modification_fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::ToggleSplit);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        // The diff spans the full width, so the only `│` on a diff line is the
        // split cell divider.

        // The removed "apple" and its paired added "banana" share one screen row,
        // old on the left of the divider, new on the right.
        let paired = row_with(&terminal, "apple");
        let divider = paired.find('│').expect("split row has a cell divider");
        assert!(
            paired.find("apple").unwrap() < divider,
            "old text left of divider:\n{paired}"
        );
        assert!(
            paired.find("banana").unwrap() > divider,
            "new text right of divider:\n{paired}"
        );

        // The pure addition "cherry" renders right-only: nothing but blanks left
        // of the divider on its row.
        let added = row_with(&terminal, "cherry");
        let divider = added.find('│').unwrap();
        assert!(
            !added[..divider].contains(char::is_alphabetic),
            "pure addition has a blank left cell:\n{added}"
        );
    }

    #[test]
    fn split_divider_runs_unbroken_through_headers_and_empty_space() {
        let repo = modification_fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::ToggleSplit);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        // The diff is full width, so the cell divider is the first `│` on a row.
        let cell_column = |line: &str| line.chars().position(|c| c == '│');

        let rendered = terminal.backend().to_string();
        let content = cell_column(rendered.lines().find(|l| l.contains("apple")).unwrap())
            .expect("a content row has the cell divider");

        // The file header, the hunk header, and a blank row below the diff all
        // carry the divider at the same column.
        for needle in ["code.rs", "@@", "  "] {
            let row = rendered
                .lines()
                .rev()
                .find(|l| l.contains(needle) && l.contains('│'))
                .unwrap();
            assert_eq!(
                cell_column(row),
                Some(content),
                "divider should align on row {needle:?}:\n{row}"
            );
        }
    }

    #[test]
    fn split_toggles_back_to_an_identical_unified_view() {
        let repo = modification_fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let render = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|frame| ui::render(frame, app, &highlighter))
                .unwrap();
            terminal.backend().to_string()
        };

        let unified = render(&mut app);
        app.apply(keymap::Action::ToggleSplit);
        let split = render(&mut app);
        app.apply(keymap::Action::ToggleSplit);
        let back = render(&mut app);

        assert_ne!(unified, split, "split view should differ from unified");
        assert_eq!(unified, back, "toggling back reproduces the unified view");
    }

    #[test]
    fn split_view_still_renders_annotation_blocks() {
        let repo = modification_fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::NextChange); // lands on the removed "apple" line
        app.apply(keymap::Action::Down); // step onto the added "banana" line (new side)
        app.apply(keymap::Action::Annotate);
        for c in "needs a test".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorSave);
        app.apply(keymap::Action::ToggleSplit);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("needs a test"),
            "annotation body shows in split view:\n{rendered}"
        );
    }

    #[test]
    fn annotating_a_removed_line_renders_inline() {
        use crate::model::Side;

        let repo = modification_fixture();
        let backend = Backend::discover(repo.path(), Some(crate::vcs::Kind::Git)).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::NextChange); // lands on the removed "apple" line
        app.apply(keymap::Action::Annotate);
        for c in "deleted on purpose".chars() {
            app.apply(keymap::Action::EditorChar(c));
        }
        app.apply(keymap::Action::EditorSave);

        // The annotation anchors the old side; it must still render inline in both
        // views, not vanish once the editor closes.
        assert_eq!(app.annotations()[0].annotation.anchor.side, Side::Old);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let render = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|frame| ui::render(frame, app, &highlighter))
                .unwrap();
            terminal.backend().to_string()
        };

        assert!(
            render(&mut app).contains("deleted on purpose"),
            "old-side annotation shows in unified view"
        );

        app.apply(keymap::Action::ToggleSplit);
        let split = render(&mut app);
        assert!(
            split.contains("deleted on purpose"),
            "old-side annotation shows in split view:\n{split}"
        );

        // In split the block hangs under the left (old) cell, leaving the divider
        // and right cell intact.
        let block = split
            .lines()
            .find(|l| l.contains("deleted on purpose"))
            .unwrap();
        let divider = block.find('│').expect("block row keeps the cell divider");
        assert!(
            block.find("deleted on purpose").unwrap() < divider,
            "old-side block sits left of the divider:\n{block}"
        );
    }

    /// A feature commit touching two files (`alpha.rs`, `beta.rs`), so the file
    /// panel has more than one entry to navigate.
    fn multi_file_fixture() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@example.com"]);
        git(path, &["config", "user.name", "T"]);
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "base"]);

        git(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(path.join("alpha.rs"), "fn a() {}\n").unwrap();
        std::fs::write(path.join("beta.rs"), "fn b() {}\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "two files"]);
        repo
    }

    fn multi_file_app(repo: &Path) -> App {
        let backend = Backend::discover(repo, Some(crate::vcs::Kind::Git)).unwrap();
        App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap()
    }

    /// Show the file list in the band (which focuses it).
    fn focus_file_panel(app: &mut App) {
        app.apply(keymap::Action::ViewFiles);
    }

    #[test]
    fn tab_toggles_between_the_band_and_the_diff() {
        use super::app::Focus;

        let repo = multi_file_fixture();
        let mut app = multi_file_app(repo.path());

        // Review starts in the diff, where annotating happens.
        assert!(matches!(app.focus, Focus::Diff));
        app.apply(keymap::Action::FocusToggle);
        assert!(matches!(app.focus, Focus::Band), "tab moves to the band");
        app.apply(keymap::Action::FocusToggle);
        assert!(
            matches!(app.focus, Focus::Diff),
            "tab moves back to the diff"
        );
    }

    #[test]
    fn shift_tab_cycles_the_band_views() {
        use super::app::{BandView, Focus};

        let repo = multi_file_fixture();
        let mut app = multi_file_app(repo.path());

        app.apply(keymap::Action::ViewCommits);
        assert!(matches!(app.band, BandView::Commits));
        assert!(
            matches!(app.focus, Focus::Band),
            "showing a view focuses it"
        );

        app.apply(keymap::Action::CycleView);
        assert!(matches!(app.band, BandView::Files));
        app.apply(keymap::Action::CycleView);
        assert!(matches!(app.band, BandView::Annotations));
        app.apply(keymap::Action::CycleView);
        assert!(matches!(app.band, BandView::Commits), "cycle wraps around");
    }

    #[test]
    fn moving_the_file_panel_reveals_the_file_in_the_diff() {
        use super::app::{Focus, Row};

        let repo = multi_file_fixture();
        let mut app = multi_file_app(repo.path());

        focus_file_panel(&mut app);
        app.apply(keymap::Action::Down);

        assert!(
            matches!(app.focus, Focus::Band),
            "moving keeps focus in the band"
        );
        assert!(
            matches!(app.rows[app.diff_cursor], Row::File { .. }),
            "the diff cursor lands on a file header"
        );
        let headers_before = app.rows[..app.diff_cursor]
            .iter()
            .filter(|row| matches!(row, Row::File { .. }))
            .count();
        assert_eq!(
            headers_before, 1,
            "the cursor sits on the second file's header"
        );
    }

    #[test]
    fn scrolling_the_diff_highlights_the_file_in_the_panel() {
        use super::app::{Focus, Row};

        let repo = multi_file_fixture();
        let mut app = multi_file_app(repo.path());

        while !matches!(app.focus, Focus::Diff) {
            app.apply(keymap::Action::FocusToggle);
        }
        assert_eq!(app.file_cursor, 0, "the diff starts in the first file");

        // Scroll down until the diff cursor reaches the second file's header.
        let second_header = app
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, Row::File { .. }))
            .nth(1)
            .map(|(index, _)| index)
            .unwrap();

        while app.diff_cursor < second_header {
            app.apply(keymap::Action::Down);
        }

        assert_eq!(
            app.file_cursor, 1,
            "reaching the second file highlights it in the panel"
        );
    }

    #[test]
    fn enter_in_the_file_panel_focuses_the_diff() {
        use super::app::{Focus, Row};

        let repo = multi_file_fixture();
        let mut app = multi_file_app(repo.path());

        focus_file_panel(&mut app);
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Confirm);

        assert!(
            matches!(app.focus, Focus::Diff),
            "enter drops into the diff"
        );
        assert!(matches!(app.rows[app.diff_cursor], Row::File { .. }));
    }

    #[test]
    fn file_panel_lists_changed_paths() {
        let repo = multi_file_fixture();
        let mut app = multi_file_app(repo.path());
        app.apply(keymap::Action::ViewFiles);

        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("files ·"),
            "file panel header is shown:\n{rendered}"
        );
        assert!(
            rendered.contains("alpha.rs") && rendered.contains("beta.rs"),
            "file panel lists both changed files:\n{rendered}"
        );
    }
}
