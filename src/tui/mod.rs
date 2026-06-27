//! The `ratatui` terminal UI (PRD §11).
//!
//! [`run`] owns terminal setup/teardown and the input loop; all model logic
//! lives in [`app`] and all drawing in [`ui`], so the interesting parts are
//! testable with a `ratatui::TestBackend` and without a real terminal.

mod app;
mod highlight;
mod keymap;
mod theme;
mod ui;

pub use app::App;
pub use theme::ThemeMode;

use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::vcs::{Base, GitBackend};
use highlight::Highlighter;

/// Poll interval; bounds how long a draw can lag a terminal resize.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Launch the TUI against `backend`, listing commits per `base`. `theme` is the
/// explicit `--theme`/config override, if any; otherwise the terminal is queried.
pub fn run(backend: GitBackend, base: Base, theme: Option<ThemeMode>) -> Result<()> {
    let mut terminal = ratatui::init();
    // Resolve the theme with the terminal in raw mode so the OSC 11 query works.
    let theme = ThemeMode::resolve(theme);

    let result = build_and_run(&mut terminal, backend, base, theme);
    ratatui::restore();
    result
}

fn build_and_run(
    terminal: &mut ratatui::DefaultTerminal,
    backend: GitBackend,
    base: Base,
    theme: ThemeMode,
) -> Result<()> {
    let mut app = App::new(backend, base, theme)?;
    let highlighter = Highlighter::new(theme, app.palette.default_fg);
    event_loop(terminal, &mut app, &highlighter)
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    highlighter: &Highlighter,
) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, app, highlighter))?;

        if !event::poll(POLL_INTERVAL)? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if let Some(action) = keymap::map(key, app.is_editing()) {
                app.apply(action);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success());
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

    #[test]
    fn renders_without_panicking() {
        let repo = fixture();
        let backend = GitBackend::discover(repo.path()).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Add lib"), "sidebar should list the commit");
    }

    #[test]
    #[ignore = "visual preview; run with --ignored --nocapture"]
    fn dump_preview() {
        let repo = fixture();
        let backend = GitBackend::discover(repo.path()).unwrap();
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
        let backend = GitBackend::discover(repo.path()).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit); // focus diff, cursor on row 0 (File)

        let cursor_bg = app.palette.cursor_bg;
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut app, &highlighter))
            .unwrap();

        // Diff pane body starts at x=32 (sidebar width) and y=1 (under header).
        let buffer = terminal.backend().buffer();
        let painted = (33..120).any(|x| buffer.cell((x, 1)).map(|c| c.bg) == Some(cursor_bg));
        assert!(painted, "cursor row should be highlighted:\n{}", terminal.backend());
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
        std::fs::write(path.join("code.rs"), original.replace("line6\n", "line6_changed\n")).unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "change"]);

        let backend = GitBackend::discover(path).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);
        app.apply(keymap::Action::NextChange);

        let has = |app: &App, content: &str| {
            app.rows.iter().any(|r| matches!(r, Row::Line { line, .. } if line.content == content))
        };

        let before = app.rows.len();
        assert!(!has(&app, "line1") && !has(&app, "line12"), "file edges hidden by default");

        app.apply(keymap::Action::ExpandContext);
        assert!(app.rows.len() > before, "expansion adds rows");
        assert!(has(&app, "line1") && has(&app, "line12"), "expansion reveals the file edges");

        app.apply(keymap::Action::CollapseContext);
        assert_eq!(app.rows.len(), before, "collapse restores the original rows");
    }

    #[test]
    fn overview_navigation_reveals_without_taking_focus() {
        use super::app::Focus;

        let repo = fixture();
        let backend = GitBackend::discover(repo.path()).unwrap();
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

        app.apply(keymap::Action::ToggleOverview);
        let first = app.diff_cursor;

        app.apply(keymap::Action::Down);
        assert!(matches!(app.focus, Focus::Sidebar), "overview keeps focus in the sidebar");
        assert_ne!(app.diff_cursor, first, "moving the overview row moves the diff cursor");
    }

    #[test]
    fn next_change_lands_on_a_changed_line() {
        use super::app::Row;
        use crate::vcs::DiffLineKind;

        let repo = fixture();
        let backend = GitBackend::discover(repo.path()).unwrap();
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
        let backend = GitBackend::discover(repo.path()).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        app.apply(keymap::Action::SelectCommit);

        // A render records the viewport height; without it paging cannot move.
        let highlighter = Highlighter::new(ThemeMode::Dark, app.palette.default_fg);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| ui::render(frame, &mut app, &highlighter)).unwrap();

        assert!(app.diff_viewport_height > 0);
        app.apply(keymap::Action::HalfPageDown);
        assert!(app.diff_cursor > 0, "half-page down should advance the cursor");
    }

    #[test]
    fn select_and_annotate_writes_an_event() {
        let repo = fixture();
        let backend = GitBackend::discover(repo.path()).unwrap();
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
        let backend = GitBackend::discover(repo.path()).unwrap();
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
        assert!(app.is_editing(), "should reopen the editor on the existing annotation");
        app.apply(keymap::Action::EditorChar('!'));
        app.apply(keymap::Action::EditorSave);

        assert_eq!(app.annotations().len(), 1, "no duplicate annotation is created");
        assert_eq!(app.annotations()[0].annotation.body, "first!");
    }

    /// Build an app on the fixture with a single annotation on the first added
    /// line of the feature commit.
    fn app_with_annotation(repo: &Path) -> App {
        let backend = GitBackend::discover(repo).unwrap();
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
    fn reviewer_reopens_an_agent_resolution() {
        use crate::model::{Actor, AnnotationId, Event, EventKind};
        use crate::model::Status;
        use crate::store::Store;

        let repo = fixture();
        let id: AnnotationId = app_with_annotation(repo.path()).annotations()[0].id();

        // The agent resolves it out of band.
        Store::open(repo.path())
            .append(&Event::now(id, Actor::Agent, EventKind::AgentResolved { reply: None }))
            .unwrap();

        // Reopen via the overview, which focuses the selected annotation.
        let backend = GitBackend::discover(repo.path()).unwrap();
        let mut app = App::new(backend, Base::Branch("main".into()), ThemeMode::Dark).unwrap();
        assert_eq!(app.annotations()[0].status, Status::Resolved);

        app.apply(keymap::Action::ToggleOverview);
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
        terminal.draw(|frame| ui::render(frame, &mut app, &highlighter)).unwrap();
        assert!(terminal.backend().to_string().contains("needs docs"), "inline annotation");

        // Opening the editor renders it inline too (its save hint is visible).
        app.apply(keymap::Action::Down);
        app.apply(keymap::Action::Annotate);
        assert!(app.is_editing());
        terminal.draw(|frame| ui::render(frame, &mut app, &highlighter)).unwrap();
        assert!(terminal.backend().to_string().contains("ctrl-s save"), "inline editor");
    }

    #[test]
    fn deleting_removes_the_annotation() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        assert_eq!(app.annotations().len(), 1);

        // Open the sidebar overview and delete the selected annotation.
        app.apply(keymap::Action::ToggleOverview);
        app.apply(keymap::Action::Delete);

        assert!(app.annotations().is_empty(), "annotation should fold away");

        let ndjson = std::fs::read_to_string(repo.path().join(".margin/annotations.ndjson")).unwrap();
        assert!(ndjson.contains("annotation_deleted"), "{ndjson}");
    }

    #[test]
    fn deleting_from_the_diff_cursor_removes_the_annotation() {
        let repo = fixture();
        // The helper leaves the diff focused with the cursor on the annotated line.
        let mut app = app_with_annotation(repo.path());
        assert_eq!(app.annotations().len(), 1);

        app.apply(keymap::Action::Delete);

        assert!(app.annotations().is_empty(), "delete should fold the annotation away");
    }

    #[test]
    fn undo_restores_a_deleted_annotation() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        app.apply(keymap::Action::Delete);
        assert!(app.annotations().is_empty(), "delete folds the annotation away");

        app.apply(keymap::Action::Undo);
        assert_eq!(app.annotations().len(), 1, "undo brings it back");

        let ndjson = std::fs::read_to_string(repo.path().join(".margin/annotations.ndjson")).unwrap();
        assert!(ndjson.contains("annotation_restored"), "{ndjson}");
    }

    #[test]
    fn jumping_from_the_overview_focuses_the_diff_line() {
        use super::app::{Focus, Row};

        let repo = fixture();
        let mut app = app_with_annotation(repo.path());
        let anchored = app.annotations()[0].annotation.anchor.start_line.get();

        // Move off the commit, open the overview, and jump to the annotation.
        app.apply(keymap::Action::ToggleOverview);
        app.apply(keymap::Action::Confirm);

        assert!(matches!(app.focus, Focus::Diff), "jump should focus the diff");
        let cursor_line = match &app.rows[app.diff_cursor] {
            Row::Line { line, .. } => line.new_no.map(|n| n.get()),
            _ => None,
        };
        assert_eq!(cursor_line, Some(anchored), "cursor should land on the anchor line");
    }

    #[test]
    fn editing_appends_an_edit_event() {
        let repo = fixture();
        let mut app = app_with_annotation(repo.path());

        app.apply(keymap::Action::ToggleOverview);
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

        app.apply(keymap::Action::ToggleOverview);
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
}
