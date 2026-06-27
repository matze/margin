//! TUI application state and update logic (PRD §11).
//!
//! State is kept terminal-free so it can be driven by tests with a
//! `ratatui::TestBackend`: [`App`] holds the model, [`App::apply`] folds an
//! [`Action`] into it, and rendering (in [`super::ui`]) is a pure function of
//! the resulting state.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::anchor::{capture, CONTEXT_LINES};
use crate::model::{
    Actor, AnnotationId, AnnotationType, Event, EventKind, LineNumber, RepoRelPath, RevisionId,
    Side, Status,
};
use crate::review::{resolve_all, ResolvedAnnotation};
use crate::store::Store;
use crate::vcs::{
    Backend, Base, ChangeKind, CommitDiff, DiffLine, DiffLineKind, Hunk, ListingSource, Revision,
    Vcs,
};

use super::keymap::Action;
use super::theme::{Palette, ThemeMode};

/// Lines of source context revealed per expand/collapse step.
const CONTEXT_STEP: u32 = 10;

/// Visible rows of the commit-message footer; longer messages scroll.
pub const COMMIT_MESSAGE_VIEWPORT: usize = 8;

/// Which top-level pane has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Diff,
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

/// What the sidebar lists: the commit log, or the cross-commit annotation
/// overview (issues: overview belongs in the sidebar).
pub enum SidebarView {
    Commits,
    Annotations { cursor: usize },
}

/// The annotation editor (PRD §11 annotation editor), used both to create a new
/// annotation and to edit an existing one's body/type.
pub struct Editor {
    pub mode: EditorMode,
    pub body: String,
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
    },
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

    revisions: Vec<Revision>,
    pub listing_source: ListingSource,
    pub commit_cursor: usize,

    diff: Option<CommitDiff>,
    pub rows: Vec<Row>,
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

    /// Full message of the selected commit, shown when the sidebar is focused.
    pub current_message: String,
    /// First visible line of the commit message footer.
    pub message_scroll: usize,
    /// Height of the diff viewport, recorded each frame for half-page paging.
    pub diff_viewport_height: usize,

    pub focus: Focus,
    pub sidebar: SidebarView,
    pub overlay: Overlay,
    pub theme_mode: ThemeMode,
    pub palette: Palette,
    pub status_message: Option<String>,
    pub should_quit: bool,
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

        let mut app = Self {
            backend,
            repo_root,
            store,
            revisions: listing.revisions,
            listing_source: listing.source,
            commit_cursor: 0,
            diff: None,
            rows: Vec::new(),
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
            focus: Focus::Sidebar,
            sidebar: SidebarView::Commits,
            overlay: Overlay::None,
            theme_mode,
            palette: Palette::for_mode(theme_mode),
            status_message: None,
            should_quit: false,
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
            Action::ExpandContext => self.expand_context(Direction::Down),
            Action::CollapseContext => self.expand_context(Direction::Up),
            Action::FocusToggle => self.toggle_focus(),
            Action::SelectCommit => self.select_commit(),
            Action::Confirm => self.confirm(),
            Action::Space => self.start_selection(),
            Action::StartSelection => self.start_selection(),
            Action::Annotate => self.begin_annotation(),
            Action::ToggleOverview => self.toggle_overview(),
            Action::Timeline => self.open_timeline(),
            Action::Reopen => self.reopen(),
            Action::Edit => self.begin_edit(),
            Action::Delete => self.delete(),
            Action::Undo => self.undo_delete(),
            Action::Cancel => self.cancel(),
            Action::EditorChar(c) => self.editor_char(c),
            Action::EditorBackspace => self.editor_backspace(),
            Action::EditorNewline => self.editor_newline(),
            Action::EditorCycleType => self.editor_cycle_type(),
            Action::EditorSave => self.editor_save(),
        }
    }

    fn move_up(&mut self) {
        match &mut self.overlay {
            Overlay::Timeline(timeline) => timeline.scroll = timeline.scroll.saturating_sub(1),
            Overlay::Editor(_) => {}
            Overlay::None => match self.focus {
                Focus::Sidebar => self.move_sidebar(Direction::Up),
                Focus::Diff => self.diff_cursor = self.diff_cursor.saturating_sub(1),
            },
        }
    }

    fn move_down(&mut self) {
        match &mut self.overlay {
            Overlay::Timeline(timeline) => timeline.scroll += 1,
            Overlay::Editor(_) => {}
            Overlay::None => match self.focus {
                Focus::Sidebar => self.move_sidebar(Direction::Down),
                Focus::Diff => {
                    let max = self.rows.len().saturating_sub(1);
                    self.diff_cursor = (self.diff_cursor + 1).min(max);
                }
            },
        }
    }

    /// Move the sidebar cursor: through commits, or through the annotation
    /// overview when that view is active.
    fn move_sidebar(&mut self, direction: Direction) {
        match &self.sidebar {
            SidebarView::Commits => {
                let max = self.revisions.len().saturating_sub(1);
                self.commit_cursor = step_index(self.commit_cursor, direction, max);
                self.load_selected_commit();
            }
            SidebarView::Annotations { cursor } => {
                let max = self.overview_annotations().len().saturating_sub(1);
                let next = step_index(*cursor, direction, max);
                self.sidebar = SidebarView::Annotations { cursor: next };
                self.reveal_annotation();
            }
        }
    }

    /// Half-viewport paging: the diff cursor when the diff is focused, or the
    /// commit-message footer when the sidebar is. No-op while an overlay is open.
    fn move_page(&mut self, direction: Direction) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }

        match self.focus {
            Focus::Sidebar => self.scroll_message(direction),
            Focus::Diff => {
                let step = (self.diff_viewport_height / 2).max(1);
                let max = self.rows.len().saturating_sub(1);

                self.diff_cursor = match direction {
                    Direction::Up => self.diff_cursor.saturating_sub(step),
                    Direction::Down => (self.diff_cursor + step).min(max),
                };
            }
        }
    }

    /// Scroll the commit-message footer, clamped to its content.
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

        match (self.focus, &self.sidebar) {
            (Focus::Sidebar, SidebarView::Annotations { .. }) => self.jump_to_annotation(),
            (Focus::Sidebar, SidebarView::Commits) => self.select_commit(),
            (Focus::Diff, _) => self.begin_annotation(),
        }
    }

    /// Reveal the selected overview annotation in the diff pane — load its commit
    /// (only if different) and place the diff cursor on its anchor line — without
    /// changing focus, so the overview can be navigated with the diff following.
    fn reveal_annotation(&mut self) {
        let Some((revision, file, start_line)) = self.focused_annotation().map(|resolved| {
            let anchor = &resolved.annotation.anchor;
            (anchor.revision_id.clone(), anchor.file.clone(), anchor.start_line.get())
        }) else {
            return;
        };

        if let Some(index) = self.revisions.iter().position(|r| r.id == revision) {
            if index != self.commit_cursor {
                self.commit_cursor = index;
                self.load_selected_commit();
            }
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

    fn toggle_focus(&mut self) {
        if matches!(self.overlay, Overlay::None) {
            self.focus = match self.focus {
                Focus::Sidebar => Focus::Diff,
                Focus::Diff => Focus::Sidebar,
            };
        }
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

    /// Toggle the sidebar between the commit log and the annotation overview,
    /// focusing the sidebar so it can be navigated.
    fn toggle_overview(&mut self) {
        self.sidebar = match self.sidebar {
            SidebarView::Commits => SidebarView::Annotations { cursor: 0 },
            SidebarView::Annotations { .. } => SidebarView::Commits,
        };
        self.focus = Focus::Sidebar;

        if matches!(self.sidebar, SidebarView::Annotations { .. }) {
            self.reveal_annotation();
        }
    }

    fn cancel(&mut self) {
        match self.overlay {
            Overlay::None => {
                if self.selection_anchor.take().is_none() {
                    self.focus = Focus::Sidebar;
                }
            }
            _ => self.overlay = Overlay::None,
        }
    }

    /// Begin annotating the current line or selection on the new side. With no
    /// active selection on an already-annotated line, edit that annotation rather
    /// than stacking a duplicate on the same line.
    fn begin_annotation(&mut self) {
        if !self.selecting() {
            if let Some((id, body, annotation_type)) = self.annotation_at_cursor().map(|resolved| {
                (
                    resolved.annotation.id,
                    resolved.annotation.body.clone(),
                    resolved.annotation.annotation_type,
                )
            }) {
                self.overlay = Overlay::Editor(Editor {
                    mode: EditorMode::Edit(id),
                    body,
                    annotation_type,
                });
                return;
            }
        }

        let Some(target) = self.selection_target() else {
            self.status_message = Some("select an added or context line to annotate".into());
            return;
        };

        self.overlay = Overlay::Editor(Editor {
            mode: EditorMode::Create(target),
            body: String::new(),
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
            matches!(self.focus, Focus::Sidebar) && matches!(self.sidebar, SidebarView::Annotations { .. });

        if from_overview {
            self.jump_to_annotation();
        }

        let Some(resolved) = self.focused_annotation() else {
            self.status_message = Some("no annotation here to edit".into());
            return;
        };

        self.overlay = Overlay::Editor(Editor {
            mode: EditorMode::Edit(resolved.annotation.id),
            body: resolved.annotation.body.clone(),
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
        let event = Event::now(id, Actor::Reviewer, EventKind::AnnotationDeleted { reason: None });

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

        let event = Event::now(id, Actor::Reviewer, EventKind::AnnotationRestored { reason: None });

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
        if let Overlay::Timeline(timeline) = &self.overlay {
            if self.annotation(timeline.annotation_id).is_none() {
                self.overlay = Overlay::None;
            }
        }
    }

    /// The annotation the next command acts on: the timeline's annotation when
    /// it is open, the sidebar overview's selection when that view is active,
    /// else the one under the diff cursor.
    fn focused_annotation(&self) -> Option<&ResolvedAnnotation> {
        if let Overlay::Timeline(timeline) = &self.overlay {
            return self.annotations.iter().find(|a| a.id() == timeline.annotation_id);
        }

        match &self.sidebar {
            SidebarView::Annotations { cursor } if matches!(self.focus, Focus::Sidebar) => {
                self.overview_annotations().into_iter().nth(*cursor)
            }
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

    fn editor_char(&mut self, c: char) {
        if let Overlay::Editor(editor) = &mut self.overlay {
            editor.body.push(c);
        }
    }

    fn editor_backspace(&mut self) {
        if let Overlay::Editor(editor) = &mut self.overlay {
            editor.body.pop();
        }
    }

    fn editor_newline(&mut self) {
        if let Overlay::Editor(editor) = &mut self.overlay {
            editor.body.push('\n');
        }
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

        if editor.body.trim().is_empty() {
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

        let anchor = capture(
            target.path.clone(),
            target.revision.clone(),
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
                body: editor.body.trim().to_string(),
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
                body: Some(editor.body.trim().to_string()),
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

            if let Some(path) = file.new_path.clone() {
                if let Ok(text) = self.backend.file_at(&diff.revision, &path) {
                    contents.insert(file_index, text.lines().map(str::to_string).collect());
                }
            }
        }

        self.rows = build_rows(diff, &self.expansions, &contents);
    }

    /// Expanded-context count for a hunk.
    fn expansion(&self, file_index: usize, new_start: u32) -> u32 {
        self.expansions.get(&(file_index, new_start)).copied().unwrap_or(0)
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

        self.rebuild_rows();
        self.diff_cursor = self.diff_cursor.min(self.rows.len().saturating_sub(1));
    }

    /// The `(file_index, new_start)` key of the hunk containing the cursor.
    fn focused_hunk_key(&self) -> Option<(usize, u32)> {
        (0..=self.diff_cursor).rev().find_map(|index| match self.rows.get(index)? {
            Row::Hunk { file_index, new_start, .. } => Some((*file_index, *new_start)),
            _ => None,
        })
    }

    fn refresh_annotations(&mut self) {
        self.annotations =
            resolve_all(&self.store, &self.repo_root, &self.backend).unwrap_or_default();
        self.recompute_commit_markers();
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

/// Step a cursor index up or down, clamped to `[0, max]`.
fn step_index(index: usize, direction: Direction, max: usize) -> usize {
    match direction {
        Direction::Up => index.saturating_sub(1),
        Direction::Down => (index + 1).min(max),
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

        for hunk in &file.hunks {
            rows.push(Row::Hunk {
                file_index,
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                section: hunk.section.clone(),
            });

            let extra = expansions.get(&(file_index, hunk.new_start)).copied().unwrap_or(0);
            let file_lines = contents.get(&file_index);

            for context in context_lines(hunk, extra, file_lines, ContextSide::Before) {
                rows.push(Row::Line { file_index, extension: extension.clone(), line: context });
            }

            for line in &hunk.lines {
                rows.push(Row::Line {
                    file_index,
                    extension: extension.clone(),
                    line: line.clone(),
                });
            }

            for context in context_lines(hunk, extra, file_lines, ContextSide::After) {
                rows.push(Row::Line { file_index, extension: extension.clone(), line: context });
            }
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
                    has_old.then(|| hunk.old_start.checked_sub(offset)).flatten(),
                ),
                ContextSide::After => (last_new + offset - 1, has_old.then_some(last_old + offset - 1)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_position_brackets_a_multiline_range() {
        assert_eq!(span_position(5, 5, 5), SpanPosition::Single);
        assert_eq!(span_position(2, 2, 6), SpanPosition::Start);
        assert_eq!(span_position(4, 2, 6), SpanPosition::Middle);
        assert_eq!(span_position(6, 2, 6), SpanPosition::End);
    }
}
