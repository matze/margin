//! TUI application state and update logic (PRD §11).
//!
//! State is kept terminal-free so it can be driven by tests with a
//! `ratatui::TestBackend`: [`App`] holds the model, [`App::apply`] folds an
//! [`Action`] into it, and rendering (in [`super::ui`]) is a pure function of
//! the resulting state.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::anchor::{CONTEXT_LINES, Resolution, capture};
use crate::model::{
    Actor, AnnotationId, AnnotationType, Event, EventKind, LineNumber, RepoRelPath, RevisionId,
    Side, Status,
};
use crate::review::{ResolvedAnnotation, resolve_all};
use crate::store::Store;
use crate::vcs::{
    Backend, Base, ChangeKind, CommitDiff, DiffLine, DiffLineKind, FileDiff, Hunk, ListingSource,
    Revision, Vcs,
};

use super::agent::{self, AgentEvent, AgentScope, Outcome};
use super::emphasis;
use super::keymap::Action;
use super::theme::{Palette, ThemeMode};

/// Lines of source context revealed per expand/collapse step.
const CONTEXT_STEP: u32 = 10;

/// Assumed visible rows of the message column for scroll clamping; longer
/// messages scroll.
pub const COMMIT_MESSAGE_VIEWPORT: usize = 8;

/// Which top-level pane has keyboard focus. `Tab` toggles between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Band,
    Diff,
}

/// What the top band shows: one topic at a time, cycled with `Shift-Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandView {
    /// The commit list beside the selected commit's message.
    Commits,
    /// The changed-file list for the selected commit.
    Files,
    /// The cross-commit annotation overview.
    Annotations,
}

/// How the diff pane lays out changed lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView {
    /// One full-width row per diff line, old and new line numbers together.
    Unified,
    /// Old text on the left, new text on the right, removed lines paired beside
    /// their corresponding added lines.
    Split,
}

/// Direction of a cursor movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

/// A modal overlay drawn over the main screen.
pub enum Overlay {
    None,
    Editor(Editor),
    Timeline(Timeline),
}

/// State of a headless agent session launched from the TUI, plus its streamed
/// log. Kept separate from [`Overlay`] so the log can stay visible while diff
/// navigation continues — the session runs non-blocking.
#[derive(Default)]
pub struct AgentSession {
    /// Whether a session is currently running.
    pub running: bool,
    /// The streamed activity log, oldest first.
    pub log: Vec<String>,
    /// Whether the log overlay is shown.
    pub log_visible: bool,
}

/// A single-cursor text buffer for the annotation editor. `cursor` is a byte
/// offset into `text`, kept on a `char` boundary and within `0..=text.len()` by
/// every method, so it can never index mid-character.
#[derive(Default)]
pub struct TextField {
    text: String,
    cursor: usize,
}

impl TextField {
    /// Seed the buffer with `text`, parking the cursor at the end.
    pub fn seeded(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    pub fn contents(&self) -> &str {
        &self.text
    }

    /// Replace the whole buffer, parking the cursor at the end.
    pub fn set_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    /// The cursor's zero-based `(row, column)`, counting columns in `char`s.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let column = before
            .rsplit('\n')
            .next()
            .map_or(0, |line| line.chars().count());

        (row, column)
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary(self.cursor) {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    pub fn delete_forward(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    pub fn delete_word_back(&mut self) {
        let start = self.word_start_before(self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn left(&mut self) {
        if let Some(prev) = self.prev_boundary(self.cursor) {
            self.cursor = prev;
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.cursor = next;
        }
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start_before(self.cursor);
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_end_after(self.cursor);
    }

    pub fn line_start(&mut self) {
        self.cursor = self.line_start_at(self.cursor);
    }

    pub fn line_end(&mut self) {
        self.cursor = self.line_end_at(self.cursor);
    }

    pub fn up(&mut self) {
        self.move_vertical(Direction::Up);
    }

    pub fn down(&mut self) {
        self.move_vertical(Direction::Down);
    }

    /// Move to the same column on the line above/below, clamped to its end.
    fn move_vertical(&mut self, direction: Direction) {
        let line_start = self.line_start_at(self.cursor);
        let column = self.text[line_start..self.cursor].chars().count();

        let target_line_start = match direction {
            Direction::Up if line_start == 0 => return,
            Direction::Up => self.line_start_at(line_start - 1),
            Direction::Down => {
                let line_end = self.line_end_at(self.cursor);

                if line_end == self.text.len() {
                    return;
                }

                line_end + 1
            }
        };

        self.cursor = self.column_to_offset(target_line_start, column);
    }

    /// The byte offset `column` chars into the line starting at `line_start`,
    /// stopping at the line's end.
    fn column_to_offset(&self, line_start: usize, column: usize) -> usize {
        let line_end = self.line_end_at(line_start);
        let mut offset = line_start;

        for _ in 0..column {
            match self.next_boundary(offset) {
                Some(next) if next <= line_end => offset = next,
                _ => break,
            }
        }

        offset
    }

    fn line_start_at(&self, idx: usize) -> usize {
        self.text[..idx].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end_at(&self, idx: usize) -> usize {
        self.text[idx..]
            .find('\n')
            .map_or(self.text.len(), |i| idx + i)
    }

    fn prev_boundary(&self, idx: usize) -> Option<usize> {
        self.text[..idx].char_indices().next_back().map(|(i, _)| i)
    }

    fn next_boundary(&self, idx: usize) -> Option<usize> {
        self.text[idx..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| idx + i)
            .or(if idx < self.text.len() {
                Some(self.text.len())
            } else {
                None
            })
    }

    /// The start of the word at or before `idx`: skip trailing whitespace, then
    /// the word's characters.
    fn word_start_before(&self, idx: usize) -> usize {
        let mut offset = idx;

        while let Some(prev) = self.prev_boundary(offset) {
            if self.text[prev..offset].chars().all(char::is_whitespace) {
                offset = prev;
            } else {
                break;
            }
        }

        while let Some(prev) = self.prev_boundary(offset) {
            if self.text[prev..offset].chars().any(char::is_whitespace) {
                break;
            }

            offset = prev;
        }

        offset
    }

    /// The end of the word at or after `idx`: skip leading whitespace, then the
    /// word's characters.
    fn word_end_after(&self, idx: usize) -> usize {
        let mut offset = idx;

        while offset < self.text.len() {
            match self.text[offset..].chars().next() {
                Some(c) if c.is_whitespace() => offset = self.next_boundary(offset).unwrap(),
                _ => break,
            }
        }

        while offset < self.text.len() {
            match self.text[offset..].chars().next() {
                Some(c) if !c.is_whitespace() => offset = self.next_boundary(offset).unwrap(),
                _ => break,
            }
        }

        offset
    }
}

/// The marker line separating the ignored instruction block from the editable
/// body in the `$EDITOR` template.
const TEMPLATE_MARKER: &str = "# ----------------------- 8< -----------------------";

/// What an external-editor session needs: the current body plus the annotated
/// location and source lines to quote as read-only context.
pub struct EditorSeed {
    pub body: String,
    pub location: String,
    pub source_lines: Vec<String>,
}

/// Seed an external-editor buffer: an ignored instruction block quoting the
/// annotated source lines, the marker line, then the current body below it.
pub fn editor_template(seed: &EditorSeed) -> String {
    let mut header = String::from(
        "# margin annotation — write the text below the marker line.\n\
         # Everything above the marker (these comments) is ignored on save.\n\
         # Save & quit to apply; an empty body cancels.\n",
    );

    if !seed.source_lines.is_empty() {
        header.push_str(&format!("#\n# Annotating {}:\n", seed.location));

        for line in &seed.source_lines {
            header.push_str("#   ");
            header.push_str(line);
            header.push('\n');
        }
    }

    format!("{header}{TEMPLATE_MARKER}\n{}", seed.body)
}

/// Recover the body from an external-editor buffer: everything after the marker
/// line, trailing blank lines trimmed. Absent the marker, the whole content is
/// the body.
pub fn strip_template(content: &str) -> String {
    let body = match content.find(TEMPLATE_MARKER) {
        Some(idx) => {
            let after = &content[idx + TEMPLATE_MARKER.len()..];
            after.strip_prefix('\n').unwrap_or(after)
        }
        None => content,
    };

    body.trim_end().to_string()
}

/// The annotation editor (PRD §11 annotation editor), used both to create a new
/// annotation and to edit an existing one's body/type.
pub struct Editor {
    pub mode: EditorMode,
    pub text: TextField,
    pub annotation_type: Option<AnnotationType>,
}

/// What an editor session will write on save.
pub enum EditorMode {
    /// Create a new annotation anchored at `Target`.
    Create(Target),
    /// Edit the body/type of an existing annotation.
    Edit(AnnotationId),
}

/// The per-annotation timeline overlay (PRD §11.0): the folded event history.
pub struct Timeline {
    pub annotation_id: AnnotationId,
    pub scroll: usize,
}

/// Where a pending annotation will be anchored.
#[derive(Clone)]
pub struct Target {
    pub path: RepoRelPath,
    pub revision: RevisionId,
    pub side: Side,
    pub start: LineNumber,
    pub end: LineNumber,
}

/// A rendered row of the diff pane.
pub enum Row {
    File {
        label: String,
        change: ChangeKind,
    },
    Hunk {
        /// The file this hunk belongs to, and its original boundaries, so the
        /// hunk can be located and its context expanded.
        file_index: usize,
        old_start: u32,
        old_count: u32,
        new_start: u32,
        new_count: u32,
        section: String,
    },
    Line {
        file_index: usize,
        /// File extension, for syntax highlighting.
        extension: String,
        line: DiffLine,
        /// Word-level changed byte ranges into `line.content`, for a line paired
        /// with a replacement counterpart in the same hunk. Empty for context
        /// lines and for unpaired/too-dissimilar added/removed lines.
        emphasis: Vec<Range<usize>>,
    },
}

impl Row {
    /// A diff line carrying its intraline-change `emphasis` ranges.
    fn line(
        file_index: usize,
        extension: &str,
        line: DiffLine,
        emphasis: Vec<Range<usize>>,
    ) -> Row {
        Row::Line {
            file_index,
            extension: extension.to_string(),
            line,
            emphasis,
        }
    }

    /// A context (unchanged) diff line, which never carries emphasis.
    fn context_line(file_index: usize, extension: &str, line: DiffLine) -> Row {
        Row::line(file_index, extension, line, Vec::new())
    }
}

/// A gutter/sidebar annotation marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Open,
    Resolved,
    Attention,
}

impl Marker {
    /// The glyph shown for this marker (PRD §11.0).
    pub fn glyph(self) -> char {
        match self {
            Marker::Open => '•',
            Marker::Resolved => '✓',
            Marker::Attention => '!',
        }
    }

    /// The marker for an annotation's derived status.
    pub fn from_status(status: Status) -> Marker {
        match status {
            Status::Open => Marker::Open,
            Status::Resolved | Status::WontDo => Marker::Resolved,
            Status::Orphaned => Marker::Attention,
        }
    }

    /// Combine markers, keeping the most attention-worthy (open > attention >
    /// resolved).
    fn merge(self, other: Marker) -> Marker {
        [Marker::Open, Marker::Attention, Marker::Resolved]
            .into_iter()
            .find(|m| *m == self || *m == other)
            .unwrap_or(other)
    }
}

/// Where a diff line sits within an annotation's line range, so the gutter can
/// draw a bracket spanning a multi-line annotation instead of a repeated glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanPosition {
    Single,
    Start,
    Middle,
    End,
}

/// A gutter marker for one diff line: its status glyph plus its place in the
/// annotation's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineMarker {
    pub marker: Marker,
    pub position: SpanPosition,
}

/// The whole application.
pub struct App {
    backend: Backend,
    repo_root: PathBuf,
    store: Store,
    /// The base the revision list was built from, kept so [`App::reload`] can
    /// re-list after the working copy changes out of band.
    base: Base,
    /// Modified-time of the annotation log at the last [`App::reload_if_changed`]
    /// check, used to detect out-of-band writes.
    last_store_stamp: Option<StoreStamp>,

    revisions: Vec<Revision>,
    pub listing_source: ListingSource,
    pub commit_cursor: usize,
    /// First visible row of the commit list column (scrolls to keep the cursor
    /// in view within the bounded band).
    pub commit_top: usize,
    /// Cursor into the changed-file list.
    pub file_cursor: usize,
    /// First visible row of the file list.
    pub file_top: usize,
    /// Cursor into the annotation overview.
    pub annotation_cursor: usize,
    /// First visible row of the annotation overview.
    pub annotation_top: usize,

    diff: Option<CommitDiff>,
    pub rows: Vec<Row>,
    /// Bumped on every [`rebuild_rows`](Self::rebuild_rows) so the event loop can
    /// detect a changed row set and prewarm its highlights.
    pub rows_generation: u64,
    pub diff_cursor: usize,
    pub diff_top: usize,
    selection_anchor: Option<usize>,
    /// Extra context lines revealed on each side of a hunk, keyed by
    /// `(file_index, original new_start)`.
    expansions: HashMap<(usize, u32), u32>,
    /// The most recently deleted annotation, restorable with undo.
    last_deleted: Option<AnnotationId>,

    annotations: Vec<ResolvedAnnotation>,
    commit_markers: HashMap<RevisionId, Marker>,
    line_markers: HashMap<(usize, Side, u32), LineMarker>,

    /// Full message of the selected commit, shown in the band's message column.
    pub current_message: String,
    /// First visible line of the message column (scrolled with ctrl-u/d).
    pub message_scroll: usize,
    /// Height of the diff viewport, recorded each frame for half-page paging.
    pub diff_viewport_height: usize,

    pub focus: Focus,
    pub view: DiffView,
    pub band: BandView,
    pub overlay: Overlay,
    pub theme_mode: ThemeMode,
    pub palette: Palette,
    pub status_message: Option<String>,
    pub should_quit: bool,
    /// Set when the editor asks to hand off to `$EDITOR`; the event loop, which
    /// owns the terminal, performs the suspend/resume and clears it.
    pending_external_edit: bool,

    /// The headless agent session and its log.
    pub agent: AgentSession,
    /// Channel the spawned agent streams events through, wired by the live event
    /// loop. `None` in tests that drive `App` directly.
    agent_tx: Option<async_channel::Sender<AgentEvent>>,
}

impl App {
    /// Build the application: list revisions for `base` and load the first one.
    pub fn new(
        backend: Backend,
        base: Base,
        theme_mode: ThemeMode,
    ) -> Result<Self, crate::vcs::VcsError> {
        let repo_root = backend.root().to_path_buf();
        let store = Store::open(&repo_root);
        let listing = backend.revisions(&base)?;
        let last_store_stamp = store_stamp(&store);

        let mut app = Self {
            backend,
            repo_root,
            store,
            base,
            last_store_stamp,
            revisions: listing.revisions,
            listing_source: listing.source,
            commit_cursor: 0,
            commit_top: 0,
            file_cursor: 0,
            file_top: 0,
            annotation_cursor: 0,
            annotation_top: 0,
            diff: None,
            rows: Vec::new(),
            rows_generation: 0,
            diff_cursor: 0,
            diff_top: 0,
            selection_anchor: None,
            expansions: HashMap::new(),
            last_deleted: None,
            annotations: Vec::new(),
            commit_markers: HashMap::new(),
            line_markers: HashMap::new(),
            current_message: String::new(),
            message_scroll: 0,
            diff_viewport_height: 0,
            focus: Focus::Diff,
            view: DiffView::Unified,
            band: BandView::Commits,
            overlay: Overlay::None,
            theme_mode,
            palette: Palette::for_mode(theme_mode),
            status_message: None,
            should_quit: false,
            pending_external_edit: false,
            agent: AgentSession::default(),
            agent_tx: None,
        };

        app.refresh_annotations();
        app.load_selected_commit();
        Ok(app)
    }

    /// The revisions shown in the sidebar.
    pub fn revisions(&self) -> &[Revision] {
        &self.revisions
    }

    /// The annotations across all commits (for the overview).
    pub fn annotations(&self) -> &[ResolvedAnnotation] {
        &self.annotations
    }

    /// True when any annotation is still open (for the "agent all" hint).
    pub fn has_open_annotations(&self) -> bool {
        self.annotations
            .iter()
            .any(|resolved| resolved.status == Status::Open)
    }

    /// Look up a resolved annotation by id (for the timeline overlay).
    pub fn annotation(&self, id: AnnotationId) -> Option<&ResolvedAnnotation> {
        self.annotations.iter().find(|a| a.id() == id)
    }

    /// Marker for a sidebar commit, if it has annotations.
    pub fn commit_marker(&self, revision: &RevisionId) -> Option<Marker> {
        self.commit_markers.get(revision).copied()
    }

    /// Marker for a diff line by its file, side, and line number, if annotated.
    pub fn line_marker(&self, file_index: usize, side: Side, line: u32) -> Option<LineMarker> {
        self.line_markers.get(&(file_index, side, line)).copied()
    }

    /// The files changed in the loaded commit, for the file panel.
    pub fn changed_files(&self) -> &[FileDiff] {
        self.diff.as_ref().map_or(&[], |diff| &diff.files)
    }

    /// The `app.rows` index of the `file_index`-th file header. File headers are
    /// emitted in `diff.files` order, so this maps a file to its diff row even
    /// when the file has no hunks (a pure rename or mode change).
    fn file_header_row(&self, file_index: usize) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, Row::File { .. }))
            .nth(file_index)
            .map(|(index, _)| index)
    }

    /// The current diff's index for `file`, matched on either side's path so an
    /// old-side (deleted-line) anchor still resolves across a rename.
    pub fn file_index_of(&self, file: &RepoRelPath) -> Option<usize> {
        self.diff.as_ref()?.files.iter().position(|diff| {
            diff.display_path() == Some(file) || diff.old_path.as_ref() == Some(file)
        })
    }

    /// The currently selected revision.
    pub fn current_revision(&self) -> Option<&Revision> {
        self.revisions.get(self.commit_cursor)
    }

    /// The new-side line number under the diff cursor, if any.
    pub fn cursor_new_line(&self) -> Option<u32> {
        match self.rows.get(self.diff_cursor)? {
            Row::Line { line, .. } => line.new_no.map(|no| no.get()),
            _ => None,
        }
    }

    /// The side and line number the diff cursor anchors to: the old-side number
    /// on a removed line, the new-side number otherwise.
    fn cursor_side_line(&self) -> Option<(Side, u32)> {
        match self.rows.get(self.diff_cursor)? {
            Row::Line { line, .. } => {
                let side = line.kind.side();
                let number = match side {
                    Side::New => line.new_no,
                    Side::Old => line.old_no,
                }?;
                Some((side, number.get()))
            }
            _ => None,
        }
    }

    /// The diff file index of the row under the diff cursor, if any.
    fn cursor_file_index(&self) -> Option<usize> {
        match self.rows.get(self.diff_cursor)? {
            Row::Line { file_index, .. } | Row::Hunk { file_index, .. } => Some(*file_index),
            _ => None,
        }
    }

    /// The annotation covering the diff cursor's line on the current commit.
    pub fn annotation_at_cursor(&self) -> Option<&ResolvedAnnotation> {
        let (side, line) = self.cursor_side_line()?;
        let file_index = self.cursor_file_index()?;
        let revision = &self.current_revision()?.id;

        self.annotations.iter().find(|resolved| {
            let anchor = &resolved.annotation.anchor;
            anchor.revision_id == *revision
                && anchor.side == side
                && self.file_index_of(&anchor.file) == Some(file_index)
                && (anchor.start_line.get()..=anchor.end_line.get()).contains(&line)
        })
    }

    /// The inclusive row range currently selected (cursor plus any `v` anchor).
    pub fn selection(&self) -> (usize, usize) {
        match self.selection_anchor {
            Some(anchor) => (anchor.min(self.diff_cursor), anchor.max(self.diff_cursor)),
            None => (self.diff_cursor, self.diff_cursor),
        }
    }

    /// True while a visual selection is active.
    pub fn selecting(&self) -> bool {
        self.selection_anchor.is_some()
    }

    /// True while the annotation editor is capturing text.
    pub fn is_editing(&self) -> bool {
        matches!(self.overlay, Overlay::Editor(_))
    }

    /// Fold an action into the state.
    pub fn apply(&mut self, action: Action) {
        self.status_message = None;

        match action {
            Action::Quit => self.should_quit = true,
            Action::Up => self.move_up(),
            Action::Down => self.move_down(),
            Action::HalfPageUp => self.move_page(Direction::Up),
            Action::HalfPageDown => self.move_page(Direction::Down),
            Action::NextChange => self.jump_change(Direction::Down),
            Action::PrevChange => self.jump_change(Direction::Up),
            Action::NextAnnotation => self.jump_annotation(Direction::Down),
            Action::PrevAnnotation => self.jump_annotation(Direction::Up),
            Action::NextCommit => self.step_commit(Direction::Down),
            Action::PrevCommit => self.step_commit(Direction::Up),
            Action::ExpandContext => self.expand_context(Direction::Down),
            Action::CollapseContext => self.expand_context(Direction::Up),
            Action::FocusToggle => self.toggle_focus(),
            Action::ToggleSplit => self.toggle_view(),
            Action::SelectCommit => self.select_commit(),
            Action::Confirm => self.confirm(),
            Action::StartSelection => self.start_selection(),
            Action::Annotate => self.begin_annotation(),
            Action::ViewCommits => self.show_view(BandView::Commits),
            Action::ViewFiles => self.show_view(BandView::Files),
            Action::ViewAnnotations => self.show_view(BandView::Annotations),
            Action::CycleView => self.cycle_view(),
            Action::Timeline => self.open_timeline(),
            Action::Reopen => self.reopen(),
            Action::Reload => self.reload(),
            Action::Edit => self.begin_edit(),
            Action::Delete => self.delete(),
            Action::Undo => self.undo_delete(),
            Action::Cancel => self.cancel(),
            Action::EditorChar(c) => self.with_editor(|text| text.insert(c)),
            Action::EditorBackspace => self.with_editor(TextField::backspace),
            Action::EditorNewline => self.with_editor(TextField::insert_newline),
            Action::EditorLeft => self.with_editor(TextField::left),
            Action::EditorRight => self.with_editor(TextField::right),
            Action::EditorUp => self.with_editor(TextField::up),
            Action::EditorDown => self.with_editor(TextField::down),
            Action::EditorWordLeft => self.with_editor(TextField::word_left),
            Action::EditorWordRight => self.with_editor(TextField::word_right),
            Action::EditorLineStart => self.with_editor(TextField::line_start),
            Action::EditorLineEnd => self.with_editor(TextField::line_end),
            Action::EditorDeleteForward => self.with_editor(TextField::delete_forward),
            Action::EditorDeleteWordBack => self.with_editor(TextField::delete_word_back),
            Action::EditorOpenExternal => self.request_external_edit(),
            Action::EditorCycleType => self.editor_cycle_type(),
            Action::EditorSave => self.editor_save(),
            Action::SpawnAgentForAnnotation => self.spawn_agent_for_annotation(),
            Action::SpawnAgentForOpen => self.spawn_agent(AgentScope::AllOpen),
            Action::ToggleAgentLog => self.agent.log_visible = !self.agent.log_visible,
        }
    }

    fn move_up(&mut self) {
        match &mut self.overlay {
            Overlay::Timeline(timeline) => timeline.scroll = timeline.scroll.saturating_sub(1),
            Overlay::Editor(_) => {}
            Overlay::None => match self.focus {
                Focus::Band => self.move_band(Direction::Up),
                Focus::Diff => self.set_diff_cursor(self.diff_cursor.saturating_sub(1)),
            },
        }
    }

    fn move_down(&mut self) {
        match &mut self.overlay {
            Overlay::Timeline(timeline) => timeline.scroll += 1,
            Overlay::Editor(_) => {}
            Overlay::None => match self.focus {
                Focus::Band => self.move_band(Direction::Down),
                Focus::Diff => self.set_diff_cursor(self.diff_cursor + 1),
            },
        }
    }

    /// Move the cursor within the band's active view.
    fn move_band(&mut self, direction: Direction) {
        match self.band {
            BandView::Commits => self.move_commits(direction),
            BandView::Files => self.move_files(direction),
            BandView::Annotations => self.move_annotations(direction),
        }
    }

    /// Move the commit-list cursor and load the newly selected commit.
    fn move_commits(&mut self, direction: Direction) {
        let max = self.revisions.len().saturating_sub(1);
        self.commit_cursor = step_index(self.commit_cursor, direction, max);
        self.load_selected_commit();
    }

    /// Move the overview cursor and reveal that annotation in the diff.
    fn move_annotations(&mut self, direction: Direction) {
        let max = self.overview_annotations().len().saturating_sub(1);
        self.annotation_cursor = step_index(self.annotation_cursor, direction, max);
        self.reveal_annotation();
    }

    /// Move the file-list cursor and reveal that file in the diff.
    fn move_files(&mut self, direction: Direction) {
        let max = self.changed_files().len().saturating_sub(1);
        self.file_cursor = step_index(self.file_cursor, direction, max);
        self.reveal_file();
    }

    /// Scroll the diff to the selected file's header without changing focus, so
    /// the file panel can be browsed with the diff following.
    fn reveal_file(&mut self) {
        if let Some(row) = self.file_header_row(self.file_cursor) {
            self.diff_cursor = row;
        }
    }

    /// Jump from the file panel into the diff at the selected file.
    fn jump_to_file(&mut self) {
        self.reveal_file();
        self.focus = Focus::Diff;
    }

    /// Move the diff cursor and keep the file panel pointed at the file the
    /// cursor now sits in, so scrolling the diff highlights the matching file.
    fn set_diff_cursor(&mut self, row: usize) {
        let max = self.rows.len().saturating_sub(1);
        self.diff_cursor = row.min(max);
        self.file_cursor = self
            .rows
            .iter()
            .take(self.diff_cursor + 1)
            .filter(|row| matches!(row, Row::File { .. }))
            .count()
            .saturating_sub(1);
    }

    /// Half-viewport paging: the diff cursor when the diff is focused, or the
    /// message column otherwise. No-op while an overlay is open.
    fn move_page(&mut self, direction: Direction) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }

        match self.focus {
            Focus::Band => {
                if matches!(self.band, BandView::Commits) {
                    self.scroll_message(direction);
                }
            }
            Focus::Diff => {
                let step = (self.diff_viewport_height / 2).max(1);

                self.set_diff_cursor(match direction {
                    Direction::Up => self.diff_cursor.saturating_sub(step),
                    Direction::Down => self.diff_cursor + step,
                });
            }
        }
    }

    /// Scroll the message column, clamped to its content.
    fn scroll_message(&mut self, direction: Direction) {
        let max_scroll = self
            .current_message
            .lines()
            .count()
            .saturating_sub(COMMIT_MESSAGE_VIEWPORT);
        let step = (COMMIT_MESSAGE_VIEWPORT / 2).max(1);

        self.message_scroll = match direction {
            Direction::Up => self.message_scroll.saturating_sub(step),
            Direction::Down => (self.message_scroll + step).min(max_scroll),
        };
    }

    /// Move the diff cursor to the start of the next/previous change section: a
    /// maximal run of added or removed lines. Landing on the first changed line
    /// (rather than a hunk header) keeps `p` useful even when the cursor already
    /// sits on a hunk's first line.
    fn jump_change(&mut self, direction: Direction) {
        if !matches!(self.focus, Focus::Diff) || !matches!(self.overlay, Overlay::None) {
            return;
        }

        let starts = (0..self.rows.len()).filter(|&index| self.is_section_start(index));

        let target = match direction {
            Direction::Down => starts.clone().find(|&index| index > self.diff_cursor),
            Direction::Up => starts.rev().find(|&index| index < self.diff_cursor),
        };

        if let Some(index) = target {
            self.diff_cursor = index;
        }
    }

    /// Move the diff cursor to the first line of the next/previous annotated
    /// span and focus the diff. Within the current diff it steps to the adjacent
    /// span (landing on its first line so repeated presses move between
    /// annotations, not within one); once the diff is exhausted it crosses into
    /// the nearest commit with an anchored annotation and lands on its
    /// first/last span. Available from either pane.
    fn jump_annotation(&mut self, direction: Direction) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }

        if let Some(index) = self.adjacent_annotation_start(direction) {
            self.diff_cursor = index;
            self.focus = Focus::Diff;
            return;
        }

        self.jump_annotation_across_commits(direction);
    }

    /// The next/previous annotated-span start relative to the diff cursor within
    /// the current diff, if any.
    fn adjacent_annotation_start(&self, direction: Direction) -> Option<usize> {
        let is_start = |index: &usize| self.is_annotation_start(*index);

        match direction {
            Direction::Down => (self.diff_cursor + 1..self.rows.len()).find(is_start),
            Direction::Up => (0..self.diff_cursor).rev().find(is_start),
        }
    }

    /// Cross into the nearest commit (in `direction`) with an anchored
    /// annotation and place the cursor on its first (down) or last (up)
    /// annotated span, focusing the diff. Commits whose only annotations are
    /// orphaned are skipped, since they have no gutter span to land on.
    fn jump_annotation_across_commits(&mut self, direction: Direction) {
        let has_anchored = |index: &usize| {
            self.revisions
                .get(*index)
                .is_some_and(|revision| self.commit_has_anchored_annotation(&revision.id))
        };

        let target = match direction {
            Direction::Down => (self.commit_cursor + 1..self.revisions.len()).find(has_anchored),
            Direction::Up => (0..self.commit_cursor).rev().find(has_anchored),
        };

        let Some(index) = target else {
            self.status_message = Some(
                match direction {
                    Direction::Down => "no later annotation",
                    Direction::Up => "no earlier annotation",
                }
                .into(),
            );
            return;
        };

        self.commit_cursor = index;
        self.load_selected_commit();
        self.focus = Focus::Diff;

        let starts = (0..self.rows.len()).filter(|&index| self.is_annotation_start(index));

        if let Some(row) = match direction {
            Direction::Down => starts.min(),
            Direction::Up => starts.max(),
        } {
            self.diff_cursor = row;
        }
    }

    /// True when `revision` has an annotation that anchors to a live diff line
    /// (i.e. is not orphaned), so jumping there lands on a gutter span.
    fn commit_has_anchored_annotation(&self, revision: &RevisionId) -> bool {
        self.annotations.iter().any(|resolved| {
            resolved.annotation.anchor.revision_id == *revision
                && !matches!(resolved.location, Resolution::Orphaned)
        })
    }

    /// True when `index` begins an annotated span: an annotated line whose
    /// predecessor is not annotated.
    fn is_annotation_start(&self, index: usize) -> bool {
        self.is_annotated_line(index) && (index == 0 || !self.is_annotated_line(index - 1))
    }

    /// True when the diff line at `index` carries an annotation gutter marker.
    fn is_annotated_line(&self, index: usize) -> bool {
        let Some(Row::Line {
            file_index, line, ..
        }) = self.rows.get(index)
        else {
            return false;
        };

        let side = line.kind.side();
        let number = match side {
            Side::New => line.new_no,
            Side::Old => line.old_no,
        };

        number.is_some_and(|no| self.line_marker(*file_index, side, no.get()).is_some())
    }

    /// Switch to the next/previous commit while keeping the diff focused, so the
    /// review can move between commits without returning to the sidebar.
    fn step_commit(&mut self, direction: Direction) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }

        let max = self.revisions.len().saturating_sub(1);
        self.commit_cursor = step_index(self.commit_cursor, direction, max);
        self.load_selected_commit();
    }

    /// True when `index` begins a change section: a changed line whose
    /// predecessor is not a changed line.
    fn is_section_start(&self, index: usize) -> bool {
        self.is_change_line(index) && (index == 0 || !self.is_change_line(index - 1))
    }

    /// True when the row at `index` is an added or removed diff line.
    fn is_change_line(&self, index: usize) -> bool {
        matches!(
            self.rows.get(index),
            Some(Row::Line { line, .. }) if !matches!(line.kind, DiffLineKind::Context)
        )
    }

    /// Enter's context action: select a commit (or jump to an annotation) from
    /// the sidebar, or annotate the current line/selection in the diff.
    fn confirm(&mut self) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }

        match (self.focus, self.band) {
            (Focus::Band, BandView::Commits) => self.select_commit(),
            (Focus::Band, BandView::Files) => self.jump_to_file(),
            (Focus::Band, BandView::Annotations) => self.jump_to_annotation(),
            (Focus::Diff, _) => self.begin_annotation(),
        }
    }

    /// Reveal the selected overview annotation in the diff pane — load its commit
    /// (only if different) and place the diff cursor on its anchor line — without
    /// changing focus, so the overview can be navigated with the diff following.
    fn reveal_annotation(&mut self) {
        let Some((revision, file, start_line)) = self.focused_annotation().map(|resolved| {
            let anchor = &resolved.annotation.anchor;
            (
                anchor.revision_id.clone(),
                anchor.file.clone(),
                anchor.start_line.get(),
            )
        }) else {
            return;
        };

        if let Some(index) = self.revisions.iter().position(|r| r.id == revision)
            && index != self.commit_cursor
        {
            self.commit_cursor = index;
            self.load_selected_commit();
        }

        let Some(file_index) = self.file_index_of(&file) else {
            return;
        };

        let row = self.rows.iter().position(|row| {
            matches!(
                row,
                Row::Line { file_index: fi, line, .. }
                    if *fi == file_index && line.new_no.map(|n| n.get()) == Some(start_line)
            )
        });

        if let Some(row) = row {
            self.diff_cursor = row;
        }
    }

    /// Jump from the overview to the selected annotation, moving focus to the diff.
    fn jump_to_annotation(&mut self) {
        self.reveal_annotation();
        self.focus = Focus::Diff;
    }

    /// Toggle focus between the top band and the diff.
    fn toggle_focus(&mut self) {
        if matches!(self.overlay, Overlay::None) {
            self.focus = match self.focus {
                Focus::Band => Focus::Diff,
                Focus::Diff => Focus::Band,
            };
        }
    }

    /// Switch the diff pane between unified and split layouts. Rows are
    /// view-independent, so only the rendering changes.
    fn toggle_view(&mut self) {
        self.view = match self.view {
            DiffView::Unified => DiffView::Split,
            DiffView::Split => DiffView::Unified,
        };
    }

    fn select_commit(&mut self) {
        self.load_selected_commit();
        self.focus = Focus::Diff;
    }

    fn start_selection(&mut self) {
        if matches!(self.focus, Focus::Diff) && matches!(self.overlay, Overlay::None) {
            self.selection_anchor = match self.selection_anchor {
                Some(_) => None,
                None => Some(self.diff_cursor),
            };
        }
    }

    /// Switch the band to `view` and focus it so it can be navigated. Selecting
    /// the files or annotations view reveals the current selection in the diff.
    fn show_view(&mut self, view: BandView) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }

        self.band = view;
        self.focus = Focus::Band;

        match view {
            BandView::Files => self.reveal_file(),
            BandView::Annotations => self.reveal_annotation(),
            BandView::Commits => {}
        }
    }

    /// Cycle the band to the next view (commits → files → annotations → …).
    fn cycle_view(&mut self) {
        let next = match self.band {
            BandView::Commits => BandView::Files,
            BandView::Files => BandView::Annotations,
            BandView::Annotations => BandView::Commits,
        };

        self.show_view(next);
    }

    fn cancel(&mut self) {
        match self.overlay {
            Overlay::None => {
                if self.selection_anchor.take().is_none() {
                    self.focus = Focus::Band;
                }
            }
            _ => self.overlay = Overlay::None,
        }
    }

    /// Begin annotating the current line or selection on the new side. With no
    /// active selection on an already-annotated line, edit that annotation rather
    /// than stacking a duplicate on the same line.
    fn begin_annotation(&mut self) {
        if !self.selecting()
            && let Some((id, body, annotation_type)) = self.annotation_at_cursor().map(|resolved| {
                (
                    resolved.annotation.id,
                    resolved.annotation.body.clone(),
                    resolved.annotation.annotation_type,
                )
            })
        {
            self.overlay = Overlay::Editor(Editor {
                mode: EditorMode::Edit(id),
                text: TextField::seeded(body),
                annotation_type,
            });
            return;
        }

        let Some(target) = self.selection_target() else {
            self.status_message = Some("select an added or context line to annotate".into());
            return;
        };

        self.overlay = Overlay::Editor(Editor {
            mode: EditorMode::Create(target),
            text: TextField::default(),
            annotation_type: None,
        });
    }

    /// Open the timeline for the focused annotation.
    fn open_timeline(&mut self) {
        match self.focused_annotation().map(ResolvedAnnotation::id) {
            Some(annotation_id) => {
                self.overlay = Overlay::Timeline(Timeline {
                    annotation_id,
                    scroll: 0,
                })
            }
            None => self.status_message = Some("no annotation here to show a timeline for".into()),
        }
    }

    /// Edit the focused annotation's body/type. From the sidebar overview this
    /// first jumps to the annotation so the inline editor lands on its line.
    fn begin_edit(&mut self) {
        let from_overview =
            matches!(self.focus, Focus::Band) && matches!(self.band, BandView::Annotations);

        if from_overview {
            self.jump_to_annotation();
        }

        let Some(resolved) = self.focused_annotation() else {
            self.status_message = Some("no annotation here to edit".into());
            return;
        };

        self.overlay = Overlay::Editor(Editor {
            mode: EditorMode::Edit(resolved.annotation.id),
            text: TextField::seeded(resolved.annotation.body.clone()),
            annotation_type: resolved.annotation.annotation_type,
        });
    }

    /// Reviewer reopens the focused annotation, rejecting the agent's
    /// resolution (PRD §10.1 `reviewer_reopened`).
    fn reopen(&mut self) {
        let Some(resolved) = self.focused_annotation() else {
            self.status_message = Some("no annotation here to reopen".into());
            return;
        };

        if !matches!(resolved.status, Status::Resolved | Status::WontDo) {
            self.status_message = Some("only resolved annotations can be reopened".into());
            return;
        }

        let id = resolved.annotation.id;
        let event = Event::now(
            id,
            Actor::Reviewer,
            EventKind::ReviewerReopened { reason: None },
        );

        match self.store.append(&event) {
            Ok(()) => {
                self.refresh_annotations();
                self.recompute_line_markers();
                self.status_message = Some("reopened".into());
            }
            Err(error) => self.status_message = Some(format!("reopen failed: {error}")),
        }
    }

    /// Reviewer deletes the focused annotation; it folds away as a tombstone.
    fn delete(&mut self) {
        let Some(resolved) = self.focused_annotation() else {
            self.status_message = Some("no annotation here to delete".into());
            return;
        };

        let id = resolved.annotation.id;
        let event = Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationDeleted { reason: None },
        );

        match self.store.append(&event) {
            Ok(()) => {
                self.last_deleted = Some(id);
                self.refresh_annotations();
                self.recompute_line_markers();
                self.cancel_overlay_if_orphaned();
                self.status_message = Some("deleted · u to undo".into());
            }
            Err(error) => self.status_message = Some(format!("delete failed: {error}")),
        }
    }

    /// Undo the most recent deletion by appending a compensating restore event.
    fn undo_delete(&mut self) {
        let Some(id) = self.last_deleted else {
            self.status_message = Some("nothing to undo".into());
            return;
        };

        let event = Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationRestored { reason: None },
        );

        match self.store.append(&event) {
            Ok(()) => {
                self.last_deleted = None;
                self.refresh_annotations();
                self.recompute_line_markers();
                self.status_message = Some("restored".into());
            }
            Err(error) => self.status_message = Some(format!("undo failed: {error}")),
        }
    }

    /// Close the timeline overlay if its annotation no longer exists (e.g. after
    /// a delete), so it does not linger pointing at nothing.
    fn cancel_overlay_if_orphaned(&mut self) {
        if let Overlay::Timeline(timeline) = &self.overlay
            && self.annotation(timeline.annotation_id).is_none()
        {
            self.overlay = Overlay::None;
        }
    }

    /// Re-read the revision list, the current diff, and the annotation log from
    /// disk, reflecting changes made out of band (an agent addressing
    /// annotations) without a restart. Focus, band, and diff view are preserved;
    /// the cursor stays on the same commit where it survives the re-listing, and
    /// the diff scrolls back to the top since code edits shift line numbers.
    pub fn reload(&mut self) {
        let current = self.current_revision().map(|r| r.id.clone());

        let listing = match self.backend.revisions(&self.base) {
            Ok(listing) => listing,
            Err(error) => {
                self.status_message = Some(format!("reload failed: {error}"));
                return;
            }
        };

        self.revisions = listing.revisions;
        self.listing_source = listing.source;

        let max = self.revisions.len().saturating_sub(1);
        self.commit_cursor = current
            .and_then(|id| self.revisions.iter().position(|r| r.id == id))
            .unwrap_or(self.commit_cursor)
            .min(max);

        self.refresh_annotations();
        self.annotation_cursor = self
            .annotation_cursor
            .min(self.annotations.len().saturating_sub(1));
        self.load_selected_commit();
        self.cancel_overlay_if_orphaned();

        self.status_message = Some("reloaded".into());
    }

    /// Reload when the annotation log changed on disk since the last check.
    /// Returns whether a reload happened, so the caller can redraw.
    pub fn reload_if_changed(&mut self) -> bool {
        let stamp = store_stamp(&self.store);

        if stamp == self.last_store_stamp {
            return false;
        }

        self.reload();
        true
    }

    /// Wire the channel the live event loop drains for streamed agent events.
    /// Only the running TUI sets this, so tests build an `App` without one.
    pub fn set_agent_channel(&mut self, sender: async_channel::Sender<AgentEvent>) {
        self.agent_tx = Some(sender);
    }

    /// Hand the focused annotation to a headless agent.
    fn spawn_agent_for_annotation(&mut self) {
        let Some(resolved) = self.focused_annotation() else {
            self.status_message = Some("no annotation here to hand to the agent".into());
            return;
        };

        let id = resolved.annotation.id;
        self.spawn_agent(AgentScope::Focused(id));
    }

    /// Launch a headless agent over `scope`, streaming its events into the log.
    /// One session at a time; a second trigger while running is rejected.
    fn spawn_agent(&mut self, scope: AgentScope) {
        if self.agent.running {
            self.status_message = Some("agent already running".into());
            return;
        }

        let Some(sender) = self.agent_tx.clone() else {
            self.status_message = Some("agent unavailable in this context".into());
            return;
        };

        let label = match &scope {
            AgentScope::AllOpen => "all open annotations",
            AgentScope::Focused(_) => "this annotation",
        };

        self.agent.running = true;
        self.agent.log.clear();
        self.agent
            .log
            .push(format!("▶ launching agent for {label}"));
        self.status_message = Some(format!("agent started · {label}"));

        agent::spawn(self.repo_root.clone(), scope, sender);
    }

    /// Fold a streamed agent event into the session log and status line. On the
    /// terminal event the store is reloaded so the agent's final `margin status`
    /// writes show even if the file watcher missed the last one.
    pub fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Started => {
                self.agent.log.push("● session started".into());
                self.status_message = Some("agent: working…".into());
            }
            AgentEvent::Assistant(text) => {
                let first = text.lines().next().unwrap_or("").to_string();
                self.agent.log.push(format!("  {text}"));
                self.status_message = Some(format!("agent: {first}"));
            }
            AgentEvent::ToolUse { name, summary } => {
                let line = match summary.is_empty() {
                    true => format!("⚙ {name}"),
                    false => format!("⚙ {name}: {summary}"),
                };
                self.status_message = Some(format!("agent: {line}"));
                self.agent.log.push(line);
            }
            AgentEvent::Finished { outcome, summary } => {
                self.agent.running = false;
                let mark = match outcome {
                    Outcome::Ok => '✓',
                    Outcome::Error => '✗',
                };
                self.agent.log.push(format!("{mark} {summary}"));
                self.reload();
                self.status_message = Some(match outcome {
                    Outcome::Ok => "agent finished".into(),
                    Outcome::Error => format!("agent finished with errors: {summary}"),
                });
            }
            AgentEvent::Failed(message) => {
                self.agent.running = false;
                self.agent.log.push(format!("✗ {message}"));
                self.reload();
                self.status_message = Some(format!("agent failed: {message}"));
            }
        }
    }

    /// The annotation the next command acts on: the timeline's annotation when
    /// it is open, the sidebar overview's selection when that view is active,
    /// else the one under the diff cursor.
    fn focused_annotation(&self) -> Option<&ResolvedAnnotation> {
        if let Overlay::Timeline(timeline) = &self.overlay {
            return self
                .annotations
                .iter()
                .find(|a| a.id() == timeline.annotation_id);
        }

        match self.band {
            BandView::Annotations if matches!(self.focus, Focus::Band) => self
                .overview_annotations()
                .into_iter()
                .nth(self.annotation_cursor),
            _ => self.annotation_at_cursor(),
        }
    }

    /// Annotations in display order, for the sidebar overview.
    pub fn overview_annotations(&self) -> Vec<&ResolvedAnnotation> {
        self.annotations.iter().collect()
    }

    /// Resolve the current selection to an annotation target. A selection of
    /// purely removed lines anchors the old side (deleted lines); anything with
    /// additions anchors the new side, dropping any removed lines.
    fn selection_target(&self) -> Option<Target> {
        let revision = self.current_revision()?.id.clone();
        let (lo, hi) = self.selection();

        let changes: Vec<(usize, Side, u32)> = (lo..=hi)
            .filter_map(|index| match self.rows.get(index)? {
                Row::Line {
                    file_index, line, ..
                } if line.kind != DiffLineKind::Context => {
                    let side = line.kind.side();
                    let number = match side {
                        Side::New => line.new_no,
                        Side::Old => line.old_no,
                    }?;
                    Some((*file_index, side, number.get()))
                }
                _ => None,
            })
            .collect();

        let (file_index, _, _) = *changes.first()?;
        let side = if changes.iter().all(|(_, s, _)| *s == Side::Old) {
            Side::Old
        } else {
            Side::New
        };

        let same_file: Vec<u32> = changes
            .iter()
            .filter(|(idx, s, _)| *idx == file_index && *s == side)
            .map(|(_, _, no)| *no)
            .collect();

        let start = LineNumber::new(*same_file.iter().min()?)?;
        let end = LineNumber::new(*same_file.iter().max()?)?;
        let path = self.file_path(file_index, side)?;

        Some(Target {
            path,
            revision,
            side,
            start,
            end,
        })
    }

    /// The path of file `file_index` on the given side: the old path anchors a
    /// deleted-line annotation, the displayed (new) path everything else.
    fn file_path(&self, file_index: usize, side: Side) -> Option<RepoRelPath> {
        let file = self.diff.as_ref()?.files.get(file_index)?;

        match side {
            Side::New => file.display_path().cloned(),
            Side::Old => file.old_path.clone(),
        }
    }

    /// Run `edit` against the open editor's text buffer, if any.
    fn with_editor(&mut self, edit: impl FnOnce(&mut TextField)) {
        if let Overlay::Editor(editor) = &mut self.overlay {
            edit(&mut editor.text);
        }
    }

    /// Request that the event loop hand the open editor's body off to `$EDITOR`.
    fn request_external_edit(&mut self) {
        if self.is_editing() {
            self.pending_external_edit = true;
        }
    }

    /// Take the pending `$EDITOR` request (the event loop owns the terminal, so
    /// it performs the suspend/resume).
    pub fn take_external_edit_request(&mut self) -> bool {
        std::mem::take(&mut self.pending_external_edit)
    }

    /// The open editor's body, type, and annotated source lines, for seeding the
    /// `$EDITOR` template.
    pub fn editor_seed(&self) -> Option<EditorSeed> {
        let Overlay::Editor(editor) = &self.overlay else {
            return None;
        };

        let (location, source_lines) = match &editor.mode {
            EditorMode::Create(target) => (
                location_label(&target.path, target.start.get(), target.end.get()),
                self.target_source_lines(target),
            ),
            EditorMode::Edit(id) => self
                .annotations
                .iter()
                .find(|resolved| resolved.id() == *id)
                .map(|resolved| {
                    let anchor = &resolved.annotation.anchor;
                    (
                        location_label(
                            &anchor.file,
                            anchor.start_line.get(),
                            anchor.end_line.get(),
                        ),
                        anchor.anchored_text.clone(),
                    )
                })
                .unwrap_or_default(),
        };

        Some(EditorSeed {
            body: editor.text.contents().to_string(),
            location,
            source_lines,
        })
    }

    /// The annotated source lines for a pending `Create` target, read from the
    /// file at its revision (the new side, or the parent for a deleted line).
    fn target_source_lines(&self, target: &Target) -> Vec<String> {
        let source = match target.side {
            Side::New => self.backend.file_at(&target.revision, &target.path),
            Side::Old => self.backend.file_at_parent(&target.revision, &target.path),
        };

        let Ok(source) = source else {
            return Vec::new();
        };

        let lines: Vec<&str> = source.lines().collect();

        (target.start.get()..=target.end.get())
            .filter_map(|n| lines.get(n as usize - 1).copied())
            .map(str::to_string)
            .collect()
    }

    /// Replace the open editor's body with text returned from `$EDITOR`.
    pub fn apply_external_edit(&mut self, body: String) {
        self.with_editor(|text| text.set_text(body));
    }

    fn editor_cycle_type(&mut self) {
        if let Overlay::Editor(editor) = &mut self.overlay {
            editor.annotation_type = match editor.annotation_type {
                None => Some(AnnotationType::Fix),
                Some(AnnotationType::Fix) => Some(AnnotationType::Question),
                Some(AnnotationType::Question) => Some(AnnotationType::Suggestion),
                Some(AnnotationType::Suggestion) => Some(AnnotationType::Nit),
                Some(AnnotationType::Nit) => Some(AnnotationType::Praise),
                Some(AnnotationType::Praise) => None,
            };
        }
    }

    fn editor_save(&mut self) {
        let Overlay::Editor(editor) = &self.overlay else {
            return;
        };

        if editor.text.contents().trim().is_empty() {
            self.status_message = Some("annotation body is empty".into());
            return;
        }

        let result = match &editor.mode {
            EditorMode::Create(target) => self.persist_created(target, editor),
            EditorMode::Edit(id) => self.persist_edited(*id, editor),
        };

        match result {
            Ok(saved) => {
                self.overlay = Overlay::None;
                self.selection_anchor = None;
                self.refresh_annotations();
                self.recompute_line_markers();
                self.status_message = Some(saved.into());
            }
            Err(message) => self.status_message = Some(message),
        }
    }

    fn persist_created(&self, target: &Target, editor: &Editor) -> Result<&'static str, String> {
        // Old-side anchors capture from the parent revision, where the deleted
        // line still exists; new-side from the revision itself.
        let source = match target.side {
            Side::New => self.backend.file_at(&target.revision, &target.path),
            Side::Old => self.backend.file_at_parent(&target.revision, &target.path),
        }
        .map_err(|error| format!("reading file at revision: {error}"))?;

        let commit_at_capture = self
            .backend
            .commit_of(&target.revision)
            .map_err(|error| format!("resolving commit at revision: {error}"))?;

        let anchor = capture(
            target.path.clone(),
            target.revision.clone(),
            commit_at_capture,
            target.side,
            &source,
            target.start,
            target.end,
            CONTEXT_LINES,
        )
        .ok_or_else(|| "selection is out of range for the file".to_string())?;

        let event = Event::now(
            AnnotationId::new(),
            Actor::Reviewer,
            EventKind::AnnotationCreated {
                anchor,
                body: editor.text.contents().trim().to_string(),
                annotation_type: editor.annotation_type,
            },
        );

        self.store
            .append(&event)
            .map_err(|error| format!("writing annotation: {error}"))?;
        Ok("annotation saved")
    }

    fn persist_edited(&self, id: AnnotationId, editor: &Editor) -> Result<&'static str, String> {
        let event = Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationEdited {
                body: Some(editor.text.contents().trim().to_string()),
                annotation_type: editor.annotation_type,
            },
        );

        self.store
            .append(&event)
            .map_err(|error| format!("writing edit: {error}"))?;
        Ok("annotation updated")
    }

    fn load_selected_commit(&mut self) {
        self.diff_cursor = 0;
        self.diff_top = 0;
        self.file_cursor = 0;
        self.file_top = 0;
        self.message_scroll = 0;
        self.selection_anchor = None;
        self.expansions.clear();

        let Some(revision) = self.current_revision().map(|r| r.id.clone()) else {
            self.diff = None;
            self.rows = Vec::new();
            self.current_message = String::new();
            return;
        };

        self.current_message = self.backend.message(&revision).unwrap_or_default();

        match self.backend.diff(&revision) {
            Ok(diff) => self.diff = Some(diff),
            Err(error) => {
                self.diff = None;
                self.status_message = Some(format!("failed to load diff: {error}"));
            }
        }

        self.rebuild_rows();
        self.recompute_line_markers();
    }

    /// Rebuild the diff rows from the loaded diff, splicing in any expanded
    /// context lines (fetched from the file at the revision).
    fn rebuild_rows(&mut self) {
        self.rows_generation += 1;

        let Some(diff) = &self.diff else {
            self.rows = Vec::new();
            return;
        };

        let mut contents: HashMap<usize, Vec<String>> = HashMap::new();

        for (file_index, file) in diff.files.iter().enumerate() {
            let expanded = file
                .hunks
                .iter()
                .any(|hunk| self.expansion(file_index, hunk.new_start) > 0);

            if !expanded {
                continue;
            }

            if let Some(path) = file.new_path.clone()
                && let Ok(text) = self.backend.file_at(&diff.revision, &path)
            {
                contents.insert(file_index, text.lines().map(str::to_string).collect());
            }
        }

        self.rows = build_rows(diff, &self.expansions, &contents);
    }

    /// Expanded-context count for a hunk.
    fn expansion(&self, file_index: usize, new_start: u32) -> u32 {
        self.expansions
            .get(&(file_index, new_start))
            .copied()
            .unwrap_or(0)
    }

    /// Reveal (or hide) more source context around the hunk under the cursor.
    fn expand_context(&mut self, direction: Direction) {
        if !matches!(self.focus, Focus::Diff) || !matches!(self.overlay, Overlay::None) {
            return;
        }

        let Some(key) = self.focused_hunk_key() else {
            self.status_message = Some("place the cursor in a hunk to expand context".into());
            return;
        };

        let current = self.expansions.get(&key).copied().unwrap_or(0);
        let next = match direction {
            Direction::Down => current + CONTEXT_STEP,
            Direction::Up => current.saturating_sub(CONTEXT_STEP),
        };

        if next == 0 {
            self.expansions.remove(&key);
        } else {
            self.expansions.insert(key, next);
        }

        // Splicing context in shifts row indices, so pin the cursor to its
        // source line rather than its (now stale) row position.
        let cursor_line = self.cursor_line();

        self.rebuild_rows();

        self.diff_cursor = cursor_line
            .and_then(|line| self.locate_line(line))
            .unwrap_or_else(|| self.diff_cursor.min(self.rows.len().saturating_sub(1)));
    }

    /// Source-line identity `(file_index, old_no, new_no)` of the row under the
    /// cursor, for a diff line. `None` for file/hunk headers.
    fn cursor_line(&self) -> Option<(usize, Option<u32>, Option<u32>)> {
        match self.rows.get(self.diff_cursor)? {
            Row::Line {
                file_index, line, ..
            } => Some((
                *file_index,
                line.old_no.map(LineNumber::get),
                line.new_no.map(LineNumber::get),
            )),
            _ => None,
        }
    }

    /// Row index of the diff line matching a `cursor_line` identity.
    fn locate_line(&self, line: (usize, Option<u32>, Option<u32>)) -> Option<usize> {
        let (file_index, old_no, new_no) = line;

        self.rows.iter().position(|row| {
            matches!(
                row,
                Row::Line { file_index: fi, line, .. }
                    if *fi == file_index
                        && line.old_no.map(LineNumber::get) == old_no
                        && line.new_no.map(LineNumber::get) == new_no
            )
        })
    }

    /// The `(file_index, new_start)` key of the hunk the cursor sits in.
    ///
    /// Resolved by line number rather than the nearest header, so that inside a
    /// merged block — where inner headers are dropped — expanding near the top
    /// targets the leading hunk and near the bottom the trailing one.
    fn focused_hunk_key(&self) -> Option<(usize, u32)> {
        let (file_index, new_no, old_no) = match self.rows.get(self.diff_cursor)? {
            Row::Hunk {
                file_index,
                new_start,
                ..
            } => (*file_index, Some(*new_start), None),
            Row::Line {
                file_index, line, ..
            } => (
                *file_index,
                line.new_no.map(LineNumber::get),
                line.old_no.map(LineNumber::get),
            ),
            _ => return None,
        };

        let hunks = &self.diff.as_ref()?.files.get(file_index)?.hunks;
        let hunk = hunks
            .iter()
            .rev()
            .find(|hunk| match (new_no, old_no) {
                (Some(new_no), _) => hunk.new_start <= new_no,
                (None, Some(old_no)) => hunk.old_start <= old_no,
                (None, None) => false,
            })
            .or_else(|| hunks.first())?;

        Some((file_index, hunk.new_start))
    }

    fn refresh_annotations(&mut self) {
        self.annotations =
            resolve_all(&self.store, &self.repo_root, &self.backend).unwrap_or_default();
        self.recompute_commit_markers();
        // Record the log's state as of this read so a later out-of-band write is
        // detectable and our own writes (which run through here) do not look
        // like one.
        self.last_store_stamp = store_stamp(&self.store);
    }

    fn recompute_commit_markers(&mut self) {
        let mut markers: HashMap<RevisionId, Marker> = HashMap::new();

        for resolved in &self.annotations {
            let marker = Marker::from_status(resolved.status);
            markers
                .entry(resolved.annotation.anchor.revision_id.clone())
                .and_modify(|existing| *existing = existing.merge(marker))
                .or_insert(marker);
        }

        self.commit_markers = markers;
    }

    fn recompute_line_markers(&mut self) {
        let mut markers: HashMap<(usize, Side, u32), LineMarker> = HashMap::new();

        if let Some(revision) = self.current_revision().map(|r| r.id.clone()) {
            for resolved in &self.annotations {
                let anchor = &resolved.annotation.anchor;

                if anchor.revision_id != revision {
                    continue;
                }

                let Some(file_index) = self.file_index_of(&anchor.file) else {
                    continue;
                };

                // A vanished anchor has no honest line to mark; the annotation
                // stays in the sidebar but earns no stale diff gutter marker.
                if matches!(resolved.location, Resolution::Orphaned) {
                    continue;
                }

                let marker = Marker::from_status(resolved.status);
                let (start, end) = (anchor.start_line.get(), anchor.end_line.get());

                for line in start..=end {
                    let position = span_position(line, start, end);
                    markers
                        .entry((file_index, anchor.side, line))
                        .and_modify(|existing| {
                            existing.marker = existing.marker.merge(marker);
                            // Overlapping ranges collapse to a plain glyph.
                            existing.position = SpanPosition::Single;
                        })
                        .or_insert(LineMarker { marker, position });
                }
            }
        }

        self.line_markers = markers;
    }
}

/// Modified-time of the annotation log, or `None` when it does not yet exist or
/// cannot be stat-ed.
/// A `path:line` (or `path:start-end`) label for an annotated range.
fn location_label(path: &RepoRelPath, start: u32, end: u32) -> String {
    if start == end {
        format!("{}:{start}", path.0.display())
    } else {
        format!("{}:{start}-{end}", path.0.display())
    }
}

/// A change signature for the annotation log: modification time paired with
/// length. The log is append-only, so its length grows on every write — that
/// catches an append landing within the same coarse mtime tick, which mtime
/// alone would miss.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StoreStamp {
    modified: SystemTime,
    len: u64,
}

fn store_stamp(store: &Store) -> Option<StoreStamp> {
    let meta = std::fs::metadata(store.path()).ok()?;
    let modified = meta.modified().ok()?;

    Some(StoreStamp {
        modified,
        len: meta.len(),
    })
}

/// Step a cursor index up or down, clamped to `[0, max]`.
fn step_index(index: usize, direction: Direction, max: usize) -> usize {
    match direction {
        Direction::Up => index.saturating_sub(1),
        Direction::Down => (index + 1).min(max),
    }
}

/// Advance or pull back the scroll `top` so `cursor` stays within a window of
/// `height` rows starting at `top`.
pub fn keep_in_view(top: usize, cursor: usize, height: usize) -> usize {
    let height = height.max(1);

    if cursor < top {
        cursor
    } else if cursor >= top + height {
        cursor + 1 - height
    } else {
        top
    }
}

/// Where `line` sits within the inclusive range `[start, end]`.
fn span_position(line: u32, start: u32, end: u32) -> SpanPosition {
    match () {
        () if start == end => SpanPosition::Single,
        () if line == start => SpanPosition::Start,
        () if line == end => SpanPosition::End,
        () => SpanPosition::Middle,
    }
}

/// Flatten a commit diff into renderable rows, splicing in expanded context
/// lines from `contents` (the new-side file text) where `expansions` requests.
fn build_rows(
    diff: &CommitDiff,
    expansions: &HashMap<(usize, u32), u32>,
    contents: &HashMap<usize, Vec<String>>,
) -> Vec<Row> {
    let mut rows = Vec::new();

    for (file_index, file) in diff.files.iter().enumerate() {
        let label = file
            .display_path()
            .map(|p| p.0.display().to_string())
            .unwrap_or_else(|| "<unknown>".into());

        let extension = file
            .display_path()
            .and_then(|p| p.0.extension())
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();

        rows.push(Row::File {
            label,
            change: file.change,
        });

        let file_lines = contents.get(&file_index);
        let extra_of = |hunk: &Hunk| {
            expansions
                .get(&(file_index, hunk.new_start))
                .copied()
                .unwrap_or(0)
        };

        let mut start = 0;

        while start < file.hunks.len() {
            // Extend the group while each gap to the next hunk is fully covered
            // by the neighbouring expansions, so they render as one block.
            let mut end = start;

            while end + 1 < file.hunks.len()
                && gap_covered(
                    &file.hunks[end],
                    &file.hunks[end + 1],
                    extra_of(&file.hunks[end]),
                    extra_of(&file.hunks[end + 1]),
                    file_lines,
                )
            {
                end += 1;
            }

            let head = &file.hunks[start];
            let tail = &file.hunks[end];

            rows.push(Row::Hunk {
                file_index,
                old_start: head.old_start,
                old_count: (tail.old_start + tail.old_count).saturating_sub(head.old_start),
                new_start: head.new_start,
                new_count: (tail.new_start + tail.new_count).saturating_sub(head.new_start),
                section: head.section.clone(),
            });

            for context in context_lines(head, extra_of(head), file_lines, ContextSide::Before) {
                rows.push(Row::context_line(file_index, &extension, context));
            }

            for member in start..=end {
                let hunk = &file.hunks[member];
                let emphasis = emphasis::hunk_emphasis(&hunk.lines);

                for (line, emphasis) in hunk.lines.iter().zip(emphasis) {
                    rows.push(Row::line(file_index, &extension, line.clone(), emphasis));
                }

                if member < end {
                    for context in
                        gap_context(&file.hunks[member], &file.hunks[member + 1], file_lines)
                    {
                        rows.push(Row::context_line(file_index, &extension, context));
                    }
                }
            }

            for context in context_lines(tail, extra_of(tail), file_lines, ContextSide::After) {
                rows.push(Row::context_line(file_index, &extension, context));
            }

            start = end + 1;
        }
    }

    rows
}

/// Which boundary of a hunk to expand context at.
#[derive(Clone, Copy)]
enum ContextSide {
    Before,
    After,
}

/// The expanded context [`DiffLine`]s on one side of a hunk, in display order.
/// Empty when no file content is available or the expansion runs off the file.
fn context_lines(
    hunk: &Hunk,
    extra: u32,
    file_lines: Option<&Vec<String>>,
    side: ContextSide,
) -> Vec<DiffLine> {
    let Some(file_lines) = file_lines.filter(|_| extra > 0) else {
        return Vec::new();
    };

    let has_old = hunk.old_start > 0;
    let last_new = hunk.new_start + hunk.new_count;
    let last_old = hunk.old_start + hunk.old_count;

    let mut lines: Vec<DiffLine> = (1..=extra)
        .filter_map(|offset| {
            let (new_no, old_no) = match side {
                ContextSide::Before => (
                    hunk.new_start.checked_sub(offset)?,
                    has_old
                        .then(|| hunk.old_start.checked_sub(offset))
                        .flatten(),
                ),
                ContextSide::After => (
                    last_new + offset - 1,
                    has_old.then_some(last_old + offset - 1),
                ),
            };

            let content = file_lines.get(new_no.checked_sub(1)? as usize)?.clone();

            Some(DiffLine {
                kind: DiffLineKind::Context,
                old_no: old_no.and_then(LineNumber::new),
                new_no: LineNumber::new(new_no),
                content,
            })
        })
        .collect();

    // `Before` offsets descend from the hunk; reverse to ascending line order.
    if matches!(side, ContextSide::Before) {
        lines.reverse();
    }

    lines
}

/// Number of unchanged new-side lines between two consecutive hunks.
fn gap_size(prev: &Hunk, next: &Hunk) -> u32 {
    next.new_start
        .saturating_sub(prev.new_start + prev.new_count)
}

/// Whether the expansions on either side reveal the whole gap between two
/// consecutive hunks, so they should render as a single merged block. Requires
/// file contents, since a merged block must show every line it spans.
fn gap_covered(
    prev: &Hunk,
    next: &Hunk,
    extra_prev: u32,
    extra_next: u32,
    file_lines: Option<&Vec<String>>,
) -> bool {
    file_lines.is_some() && extra_prev + extra_next >= gap_size(prev, next)
}

/// Every unchanged line filling the gap between two consecutive hunks, in
/// display order. Used to stitch a merged block together continuously.
fn gap_context(prev: &Hunk, next: &Hunk, file_lines: Option<&Vec<String>>) -> Vec<DiffLine> {
    let Some(file_lines) = file_lines else {
        return Vec::new();
    };

    let first_new = prev.new_start + prev.new_count;
    let first_old = prev.old_start + prev.old_count;
    let has_old = prev.old_start > 0 && next.old_start > 0;

    (first_new..next.new_start)
        .enumerate()
        .filter_map(|(step, new_no)| {
            let content = file_lines.get(new_no.checked_sub(1)? as usize)?.clone();

            Some(DiffLine {
                kind: DiffLineKind::Context,
                old_no: has_old
                    .then(|| first_old + step as u32)
                    .and_then(LineNumber::new),
                new_no: LineNumber::new(new_no),
                content,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_field_inserts_at_the_cursor() {
        let mut field = TextField::seeded("helo".into());
        field.left(); // between 'l' and 'o'
        field.insert('l');
        assert_eq!(field.contents(), "hello");
    }

    #[test]
    fn text_field_backspace_and_delete_act_at_the_cursor() {
        let mut field = TextField::seeded("abcd".into());
        field.left();
        field.left(); // between 'b' and 'c'
        field.backspace(); // removes 'b'
        assert_eq!(field.contents(), "acd");
        field.delete_forward(); // removes 'c'
        assert_eq!(field.contents(), "ad");
    }

    #[test]
    fn text_field_word_motion_and_delete() {
        let mut field = TextField::seeded("one two three".into());
        field.word_left(); // start of "three"
        assert_eq!(field.cursor_row_col(), (0, 8));
        field.delete_word_back(); // removes "two " before the cursor
        assert_eq!(field.contents(), "one three");
    }

    #[test]
    fn text_field_vertical_motion_keeps_the_column() {
        let mut field = TextField::seeded("longline\nx\nanother".into());
        field.line_start(); // start of "another"
        for _ in 0..4 {
            field.right();
        }
        assert_eq!(field.cursor_row_col(), (2, 4));
        field.up(); // "x" is shorter: clamp to its end
        assert_eq!(field.cursor_row_col(), (1, 1));
        field.up(); // back onto the long first line at the same column
        assert_eq!(field.cursor_row_col(), (0, 1));
    }

    #[test]
    fn text_field_respects_char_boundaries() {
        let mut field = TextField::seeded("héllo".into()); // 'é' is two bytes
        field.line_start();
        field.right(); // past 'h'
        field.right(); // past 'é' as one unit, not one byte
        field.insert('X');
        assert_eq!(field.contents(), "héXllo");
    }

    #[test]
    fn template_round_trips_the_body_and_quotes_the_source() {
        let seed = EditorSeed {
            body: "first line\nsecond line".into(),
            location: "lib.rs:12-13".into(),
            source_lines: vec!["fn foo() {".into(), "    bar();".into()],
        };
        let template = editor_template(&seed);

        assert!(template.contains("# Annotating lib.rs:12-13:"));
        assert!(template.contains("#   fn foo() {"));
        assert_eq!(strip_template(&template), seed.body);
    }

    #[test]
    fn strip_template_falls_back_to_whole_content_without_a_marker() {
        assert_eq!(strip_template("just a body\n\n"), "just a body");
    }

    #[test]
    fn keep_in_view_tracks_the_cursor_within_a_window() {
        // Cursor already visible: top stays put.
        assert_eq!(keep_in_view(0, 3, 5), 0);
        // Cursor below the window: top advances so the cursor sits at the bottom.
        assert_eq!(keep_in_view(0, 7, 5), 3);
        // Cursor above the window: top pulls back to the cursor.
        assert_eq!(keep_in_view(4, 1, 5), 1);
        // A zero height is treated as one row.
        assert_eq!(keep_in_view(0, 2, 0), 2);
    }

    #[test]
    fn span_position_brackets_a_multiline_range() {
        assert_eq!(span_position(5, 5, 5), SpanPosition::Single);
        assert_eq!(span_position(2, 2, 6), SpanPosition::Start);
        assert_eq!(span_position(4, 2, 6), SpanPosition::Middle);
        assert_eq!(span_position(6, 2, 6), SpanPosition::End);
    }

    /// A one-line changed hunk at `new_start` (also its old position), so the
    /// gap between two such hunks is `next.new_start - prev.new_start - 1`.
    fn changed_hunk(new_start: u32) -> Hunk {
        Hunk {
            old_start: new_start,
            old_count: 1,
            new_start,
            new_count: 1,
            section: String::new(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Added,
                old_no: None,
                new_no: LineNumber::new(new_start),
                content: format!("changed {new_start}"),
            }],
        }
    }

    fn diff_with(hunks: Vec<Hunk>) -> CommitDiff {
        CommitDiff {
            revision: RevisionId("rev".into()),
            files: vec![crate::vcs::FileDiff {
                old_path: Some(RepoRelPath("f.rs".into())),
                new_path: Some(RepoRelPath("f.rs".into())),
                change: ChangeKind::Modified,
                hunks,
            }],
        }
    }

    fn new_numbers(rows: &[Row]) -> Vec<u32> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Line { line, .. } => line.new_no.map(LineNumber::get),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn overlapping_context_merges_into_one_block() {
        // Hunks at new lines 3 and 7 leave a 3-line gap (4,5,6).
        let diff = diff_with(vec![changed_hunk(3), changed_hunk(7)]);
        let contents = HashMap::from([(0, (1..=10).map(|n| format!("line {n}")).collect())]);

        // 2 + 2 covers the 3-line gap.
        let expansions = HashMap::from([((0, 3), 2), ((0, 7), 2)]);
        let rows = build_rows(&diff, &expansions, &contents);

        // One merged header, no duplicates, continuous lines 1..=9.
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, Row::Hunk { .. }))
                .count(),
            1
        );
        assert_eq!(new_numbers(&rows), (1..=9).collect::<Vec<_>>());
    }

    #[test]
    fn build_rows_carries_intraline_emphasis_on_paired_lines() {
        let hunk = Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            section: String::new(),
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Removed,
                    old_no: LineNumber::new(1),
                    new_no: None,
                    content: "let x = one;".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    old_no: None,
                    new_no: LineNumber::new(1),
                    content: "let x = two;".into(),
                },
            ],
        };
        let rows = build_rows(&diff_with(vec![hunk]), &HashMap::new(), &HashMap::new());

        let emphasized: Vec<String> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Line { line, emphasis, .. } if !emphasis.is_empty() => {
                    Some(line.content[emphasis[0].clone()].to_string())
                }
                _ => None,
            })
            .collect();

        assert_eq!(emphasized, vec!["one", "two"]);
    }

    #[test]
    fn separated_hunks_keep_their_own_headers() {
        let diff = diff_with(vec![changed_hunk(3), changed_hunk(20)]);
        let contents = HashMap::from([(0, (1..=30).map(|n| format!("line {n}")).collect())]);

        // The 16-line gap stays partly hidden, so the hunks do not merge.
        let expansions = HashMap::from([((0, 3), 2), ((0, 20), 2)]);
        let rows = build_rows(&diff, &expansions, &contents);

        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, Row::Hunk { .. }))
                .count(),
            2
        );
        assert_eq!(new_numbers(&rows), vec![1, 2, 3, 4, 5, 18, 19, 20, 21, 22]);
    }
}
