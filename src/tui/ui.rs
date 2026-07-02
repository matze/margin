//! Rendering: a pure function of [`App`] state plus the [`Highlighter`].
//!
//! Panes are borderless (PRD §11 / issues: less chrome): each carries a single
//! header bar, the top band shows one view at a time (commits, files, or
//! annotations) with a horizontal rule separating it from the diff, and the
//! focused pane is marked by a reversed header. Syntax foreground is layered over diff-semantic
//! backgrounds (PRD §11.1); foreground accents use ANSI named colors so they
//! track the terminal theme.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use std::ops::Range;

use crate::export::type_label;
use crate::model::{Anchor, Event, EventKind, Side};
use crate::review::{ResolvedAnnotation, RevisionState};
use crate::vcs::{ChangeKind, DiffLine, DiffLineKind, ListingSource};

use super::app::{
    App, BandView, DiffView, EditorMode, Focus, LineMarker, Marker, Overlay, Row, SpanPosition,
    keep_in_view,
};
use super::emphasis;
use super::highlight::Highlighter;
use super::theme::Palette;

/// Above this many diff rows, skip syntax highlighting to stay responsive.
const HIGHLIGHT_ROW_CAP: usize = 5000;

/// A diff row's focus state, which selects its background tint and whether
/// intraline emphasis is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowFocus {
    /// The diff cursor is on this row.
    Cursor,
    /// The row falls within the visual selection.
    Selected,
    /// Neither; the row shows its natural background.
    Unfocused,
}

impl RowFocus {
    /// The row's focus given whether the pane is `focused`, the cursor sits on
    /// it, and it lies within the selection. The cursor wins over selection.
    fn resolve(focused: bool, is_cursor: bool, in_selection: bool) -> RowFocus {
        match (focused, is_cursor, in_selection) {
            (true, true, _) => RowFocus::Cursor,
            (true, false, true) => RowFocus::Selected,
            _ => RowFocus::Unfocused,
        }
    }

    /// The row background: the cursor/selection tint, else `base`.
    fn background(self, palette: Palette, base: Color) -> Color {
        match self {
            RowFocus::Cursor => palette.cursor_bg,
            RowFocus::Selected => palette.selection_bg,
            RowFocus::Unfocused => base,
        }
    }

    /// A background that tints only under the cursor, else `base`. Used by
    /// headers, which the selection span may cross without highlighting.
    fn cursor_background(self, palette: Palette, base: Color) -> Color {
        match self {
            RowFocus::Cursor => palette.cursor_bg,
            RowFocus::Selected | RowFocus::Unfocused => base,
        }
    }

    /// A bold modifier only under the cursor (see [`cursor_background`]).
    ///
    /// [`cursor_background`]: Self::cursor_background
    fn cursor_modifier(self) -> Modifier {
        match self {
            RowFocus::Cursor => Modifier::BOLD,
            RowFocus::Selected | RowFocus::Unfocused => Modifier::empty(),
        }
    }

    /// Rows under the cursor or selection are bolded to stay legible under the
    /// tint.
    fn modifier(self) -> Modifier {
        match self {
            RowFocus::Unfocused => Modifier::empty(),
            _ => Modifier::BOLD,
        }
    }

    /// Whether a whole-line background is in effect, which overrides per-word
    /// intraline emphasis.
    fn overrides_emphasis(self) -> bool {
        self != RowFocus::Unfocused
    }
}

/// Whether diff content is syntax-highlighted or rendered as plain text (the
/// latter past the row cap, to stay responsive on very large diffs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Highlighting {
    Syntax,
    Plain,
}

/// Columns before a diff line's content: the marker (2) + the two line-number
/// gutters (10) + the +/- sign (1). Inline annotation text is indented to this
/// so it lines up with the code it annotates.
const CONTENT_INDENT: usize = 13;

/// Columns before a split-view cell's content: the marker (2) + one line-number
/// gutter (5) + the +/- sign (1). Each cell shows only its own side's number.
const SPLIT_CONTENT_INDENT: usize = 8;

/// Maximum band height including its header row; see [`band_height`] for how the
/// band shrinks to its content below this.
const BAND_HEIGHT: u16 = 12;

/// Render the whole screen.
pub fn render(frame: &mut Frame, app: &mut App, highlighter: &Highlighter) {
    let area = frame.area();
    let band = band_height(app, area.height);
    let agent_log = agent_log_height(app, area.height);

    let rows = Layout::vertical([
        Constraint::Length(band),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(agent_log),
        Constraint::Length(1),
    ])
    .split(area);

    render_band(frame, app, rows[0]);
    render_band_divider(frame, app, rows[1]);
    render_diff(frame, app, highlighter, rows[2]);
    render_help(frame, app, rows[4]);

    match &app.overlay {
        // The editor renders inline within the diff (see build_attachments).
        Overlay::Editor(_) => {}
        Overlay::Timeline(_) => render_timeline(frame, app, rows[2]),
        Overlay::None => {}
    }

    if app.agent.log_visible {
        render_agent_log(frame, app, rows[3]);
    }
}

/// The height the agent-log row claims when visible: capped at
/// [`AGENT_LOG_HEIGHT`] and never more than half the screen, else zero so the
/// diff keeps the full area.
fn agent_log_height(app: &App, total: u16) -> u16 {
    match app.agent.log_visible {
        true => AGENT_LOG_HEIGHT.min(total / 2).max(3),
        false => 0,
    }
}

/// The headless-agent log: a bordered panel in its own row below the diff and
/// above the help bar, showing the most recent streamed lines. The diff stays
/// navigable beside it (non-blocking session).
fn render_agent_log(frame: &mut Frame, app: &App, rect: Rect) {
    frame.render_widget(Clear, rect);

    let title = match app.agent.running {
        true => " agent · running ".to_string(),
        false => " agent · idle ".to_string(),
    };
    let block = modal_block(title, app.palette);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // Show the tail that fits, oldest of the visible window first.
    let visible = inner.height as usize;
    let lines: Vec<Line> = app
        .agent
        .log
        .iter()
        .rev()
        .take(visible)
        .rev()
        .map(|line| Line::from(line.clone()))
        .collect();

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The band height: one header row plus the active view's content, so the band
/// only takes the space it needs and the diff gets the rest. Capped at
/// [`BAND_HEIGHT`] and never more than half the screen.
fn band_height(app: &App, total: u16) -> u16 {
    let rows = match app.band {
        BandView::Commits => app
            .revisions()
            .len()
            .max(app.current_message.lines().count()),
        BandView::Files => app.changed_files().len().max(1),
        BandView::Annotations => app.overview_annotations().len().max(1),
    };

    ((rows + 1) as u16)
        .clamp(2, BAND_HEIGHT)
        .min(total / 2)
        .max(2)
}

/// The commits view's two columns and the divider between them, as one layout so
/// the band body and its bottom rule stay column-aligned. The list column width
/// matches the split-diff cell ([`render_diff`]) so this divider lines up with the
/// split divider below it.
fn commit_columns(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::horizontal([
        Constraint::Length(area.width.saturating_sub(1) / 2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area)
}

/// The top band, showing one view at a time: the commit list beside the selected
/// commit's message, the changed-file list, or the annotation overview. Each
/// scrolls to keep its cursor in view.
fn render_band(frame: &mut Frame, app: &mut App, area: Rect) {
    let body_height = area.height.saturating_sub(1) as usize;
    let focused = matches!(app.focus, Focus::Band);

    match app.band {
        BandView::Commits => render_commit_band(frame, app, area, body_height, focused),
        BandView::Files => {
            app.file_top = keep_in_view(app.file_top, app.file_cursor, body_height);
            render_list_pane(
                frame,
                area,
                &format!("files · {}", app.changed_files().len()),
                file_list_lines(app, Color::Reset, focused),
                focused,
                app.palette,
                app.file_top as u16,
            );
        }
        BandView::Annotations => {
            app.annotation_top =
                keep_in_view(app.annotation_top, app.annotation_cursor, body_height);
            render_list_pane(
                frame,
                area,
                &format!("annotations · {}", app.overview_annotations().len()),
                annotation_list_lines(app, app.annotation_cursor, Color::Reset, focused),
                focused,
                app.palette,
                app.annotation_top as u16,
            );
        }
    }
}

/// The commits view: the commit list and the selected commit's message under one
/// shared heading. The message is the selected commit's detail, not a pane of its
/// own, so it carries no separate label.
fn render_commit_band(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    body_height: usize,
    focused: bool,
) {
    let columns = commit_columns(area);

    let detail = union(columns[0], columns[2]);
    let [heading, _] = pane_split(detail);
    render_header(
        frame,
        heading,
        &commit_list_title(app),
        focused,
        app.palette,
    );

    app.commit_top = keep_in_view(app.commit_top, app.commit_cursor, body_height);
    render_list_body(
        frame,
        body_of(columns[0]),
        commit_list_lines(app, Color::Reset, focused),
        app.commit_top as u16,
    );
    render_message_body(frame, app, body_of(columns[2]));

    // The divider runs only below the shared heading, which spans both columns.
    render_divider(frame, body_of(columns[1]), app.palette);
}

/// The smallest rect covering two horizontally adjacent columns.
fn union(left: Rect, right: Rect) -> Rect {
    Rect {
        x: left.x,
        y: left.y,
        width: right.x + right.width - left.x,
        height: left.height,
    }
}

/// The body of a band column: everything below its one-row header.
fn body_of(area: Rect) -> Rect {
    pane_split(area)[1]
}

/// The selected commit's message, scrolled with ctrl-u/d.
fn render_message_body(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(commit_message_lines(app))
            .scroll((app.message_scroll as u16, 0))
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(Color::Reset)),
        area,
    );
}

/// Render a titled list pane: a one-line header (reversed when focused) above a
/// scrolled body.
fn render_list_pane(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    focused: bool,
    palette: Palette,
    scroll: u16,
) {
    let [header, body] = pane_split(area);

    render_header(frame, header, title, focused, palette);
    render_list_body(frame, body, lines, scroll);
}

/// Render pre-built list lines into `area`, scrolled by `scroll` rows.
fn render_list_body(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, scroll: u16) {
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(Color::Reset))
            .scroll((scroll, 0)),
        area,
    );
}

/// Background and weight for a band list row. The selected row carries the
/// cursor tint only while its pane is focused; unfocused it keeps the bold alone
/// (mirroring the diff cursor, which also tints only when focused) so the
/// selection stays legible without competing with the focused pane.
fn row_style(selected: bool, focused: bool, pane_bg: Color, palette: Palette) -> Style {
    match (selected, focused) {
        (true, true) => Style::default()
            .bg(palette.cursor_bg)
            .add_modifier(Modifier::BOLD),
        (true, false) => Style::default().bg(pane_bg).add_modifier(Modifier::BOLD),
        (false, _) => Style::default().bg(pane_bg),
    }
}

/// The changed-file panel: one row per file in the loaded commit, with its
/// change glyph and repo-relative path.
fn file_list_lines(app: &App, pane_bg: Color, focused: bool) -> Vec<Line<'static>> {
    let files = app.changed_files();

    if files.is_empty() {
        return vec![Line::from(Span::styled(
            " no changes",
            Style::default().fg(app.palette.gutter_fg).bg(pane_bg),
        ))];
    }

    files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let label = file
                .display_path()
                .map(|path| path.0.display().to_string())
                .unwrap_or_else(|| "<unknown>".into());

            let selected = index == app.file_cursor;
            let base_style = row_style(selected, focused, pane_bg, app.palette);

            Line::from(vec![
                Span::styled(
                    format!(" {} ", change_glyph(file.change)),
                    base_style.fg(change_color(file.change, app.palette)),
                ),
                Span::styled(label, base_style.fg(app.palette.default_fg)),
            ])
        })
        .collect()
}

fn commit_list_title(app: &App) -> String {
    match &app.listing_source {
        ListingSource::Range { .. } => format!("commits (base..@) · {}", app.revisions().len()),
        ListingSource::RecentFallback => {
            format!("commits (recent · no base) · {}", app.revisions().len())
        }
    }
}

fn commit_list_lines(app: &App, pane_bg: Color, focused: bool) -> Vec<Line<'static>> {
    app.revisions()
        .iter()
        .enumerate()
        .map(|(index, revision)| {
            let marker = app.commit_marker(&revision.id);
            let glyph = marker.map_or(' ', Marker::glyph);
            let short: String = revision.id.0.chars().take(7).collect();
            let prefix_len = revision
                .unique_prefix_len
                .unwrap_or(0)
                .min(short.chars().count());
            let split = short
                .char_indices()
                .nth(prefix_len)
                .map_or(short.len(), |(byte, _)| byte);
            let (prefix, rest) = short.split_at(split);

            let selected = index == app.commit_cursor;
            let base_style = row_style(selected, focused, pane_bg, app.palette);

            Line::from(vec![
                Span::styled(
                    format!(" {glyph} "),
                    Style::default()
                        .fg(marker_color(marker, app.palette))
                        .bg(base_style.bg.unwrap_or(pane_bg)),
                ),
                Span::styled(
                    prefix.to_string(),
                    base_style.fg(app.palette.revision_prefix),
                ),
                Span::styled(format!("{rest} "), base_style.fg(app.palette.gutter_fg)),
                Span::styled(revision.summary.clone(), base_style),
            ])
        })
        .collect()
}

fn annotation_list_lines(
    app: &App,
    cursor: usize,
    pane_bg: Color,
    focused: bool,
) -> Vec<Line<'static>> {
    let annotations = app.overview_annotations();

    if annotations.is_empty() {
        return vec![Line::from(Span::styled(
            " no annotations yet",
            Style::default().fg(app.palette.gutter_fg).bg(pane_bg),
        ))];
    }

    annotations
        .iter()
        .enumerate()
        .map(|(index, resolved)| {
            let marker = Marker::from_status(resolved.status);
            let selected = index == cursor;
            let base_style = row_style(selected, focused, pane_bg, app.palette);

            let annotation = &resolved.annotation;
            let body = annotation.body.lines().next().unwrap_or("");
            let file = annotation
                .anchor
                .file
                .0
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");

            Line::from(vec![
                Span::styled(
                    format!(" {} ", marker.glyph()),
                    Style::default()
                        .fg(marker_color(Some(marker), app.palette))
                        .bg(base_style.bg.unwrap_or(pane_bg)),
                ),
                Span::styled(
                    format!("{file}:{}  ", annotation.anchor.start_line.get()),
                    base_style.fg(app.palette.gutter_fg),
                ),
                Span::styled(body.to_string(), base_style),
            ])
        })
        .collect()
}

/// One on-screen line of the diff pane. In unified view every entry is `Full`;
/// in split view, paired diff lines become `Split` cells that hold the row
/// indices (into `app.rows`) drawn on the left (old) and right (new) sides.
enum ScreenRow {
    Full(usize),
    Split {
        left: Option<usize>,
        right: Option<usize>,
    },
}

impl ScreenRow {
    /// The `app.rows` indices this screen row draws, for cursor/selection tests
    /// and attachment lookup.
    fn covers(&self, row: usize) -> bool {
        match self {
            ScreenRow::Full(index) => *index == row,
            ScreenRow::Split { left, right } => *left == Some(row) || *right == Some(row),
        }
    }

    /// A representative row index, used to anchor scrolling back to `app.rows`.
    fn anchor_row(&self) -> usize {
        match self {
            ScreenRow::Full(index) => *index,
            ScreenRow::Split { left, right } => left.or(*right).unwrap_or(0),
        }
    }
}

/// Lay out `app.rows` into on-screen rows for the active view. Unified maps each
/// row to its own line (identical to the row order); split pairs each run of
/// removed lines with the following run of added lines, drawing context lines in
/// both cells.
fn screen_rows(app: &App) -> Vec<ScreenRow> {
    if matches!(app.view, DiffView::Unified) {
        return (0..app.rows.len()).map(ScreenRow::Full).collect();
    }

    let mut screen = Vec::new();
    let mut index = 0;

    while index < app.rows.len() {
        match &app.rows[index] {
            Row::Line { line, .. } if matches!(line.kind, DiffLineKind::Removed) => {
                let removed: Vec<usize> = run(app, index, DiffLineKind::Removed);
                let added: Vec<usize> = run(app, index + removed.len(), DiffLineKind::Added);

                for pair in 0..removed.len().max(added.len()) {
                    screen.push(ScreenRow::Split {
                        left: removed.get(pair).copied(),
                        right: added.get(pair).copied(),
                    });
                }

                index += removed.len() + added.len();
            }
            Row::Line { line, .. } if matches!(line.kind, DiffLineKind::Added) => {
                let added = run(app, index, DiffLineKind::Added);

                for &row in &added {
                    screen.push(ScreenRow::Split {
                        left: None,
                        right: Some(row),
                    });
                }

                index += added.len();
            }
            Row::Line { .. } => {
                // Context: the same line shows on both sides.
                screen.push(ScreenRow::Split {
                    left: Some(index),
                    right: Some(index),
                });
                index += 1;
            }
            _ => {
                screen.push(ScreenRow::Full(index));
                index += 1;
            }
        }
    }

    screen
}

/// The maximal run of `app.rows` lines of `kind` starting at `start`.
fn run(app: &App, start: usize, kind: DiffLineKind) -> Vec<usize> {
    (start..app.rows.len())
        .take_while(
            |&index| matches!(&app.rows[index], Row::Line { line, .. } if line.kind == kind),
        )
        .collect()
}

/// The screen-row index that draws `row`, or 0 if none does.
fn screen_index_of(screen: &[ScreenRow], row: usize) -> usize {
    screen.iter().position(|sr| sr.covers(row)).unwrap_or(0)
}

fn render_diff(frame: &mut Frame, app: &mut App, highlighter: &Highlighter, area: Rect) {
    let focused = matches!(app.focus, Focus::Diff);
    // No header: the selected commit is identified in the band, so the diff uses
    // its whole area as the scrollable body.
    let body = area;

    let height = body.height as usize;
    app.diff_viewport_height = height;

    let width = body.width as usize;
    let attachments = build_attachments(app, width);
    let screen = screen_rows(app);

    // Keep the editor (while open) or the cursor within the viewport, counting
    // the inline attachment lines that sit between the scroll top and it.
    let anchor_row = editor_anchor_row(app).unwrap_or(app.diff_cursor);
    adjust_diff_top(app, height, &screen, &attachments, anchor_row);

    let highlighting = if app.rows.len() <= HIGHLIGHT_ROW_CAP {
        Highlighting::Syntax
    } else {
        Highlighting::Plain
    };
    let (lo, hi) = app.selection();
    let pane_bg = Color::Reset;

    let mut lines: Vec<Line> = Vec::with_capacity(height);

    if app.rows.is_empty() {
        let note = Line::from(Span::styled(
            "no changes in this revision",
            Style::default().fg(app.palette.gutter_fg).bg(pane_bg),
        ));
        frame.render_widget(
            Paragraph::new(note).style(Style::default().bg(pane_bg)),
            body,
        );
        return;
    }

    let top = screen_index_of(&screen, app.diff_top);

    for screen_row in &screen[top..] {
        if lines.len() >= height {
            break;
        }

        lines.push(render_screen_row(
            app,
            highlighter,
            screen_row,
            width,
            pane_bg,
            focused,
            lo,
            hi,
            highlighting,
        ));

        for row in covered_rows(screen_row) {
            if let Some(block) = attachments.get(&row) {
                for line in block {
                    if lines.len() >= height {
                        break;
                    }
                    lines.push(line.clone());
                }
            }
        }
    }

    // Carry the cell divider down through the empty area below the last line so
    // the split column reads as one unbroken rule, like the sidebar divider.
    if matches!(app.view, DiffView::Split) {
        let col = width.saturating_sub(1) / 2;

        while lines.len() < height {
            lines.push(divider_only(col, app.palette, pane_bg));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pane_bg)),
        body,
    );
}

/// A blank diff line carrying just the split cell divider at column `col`.
fn divider_only(col: usize, palette: Palette, bg: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ".repeat(col), Style::default().bg(bg)),
        Span::styled("│", Style::default().fg(palette.gutter_fg).bg(bg)),
    ])
}

/// Replace the character at column `col` of `line` with the split cell divider,
/// keeping the rest (and the line's width) intact, so full-width rows — file and
/// hunk headers — don't break the vertical divider.
fn overlay_divider(line: Line<'static>, col: usize, palette: Palette) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    let mut pos = 0;

    for span in line.spans {
        let len = span.content.chars().count();

        if pos + len <= col || pos > col {
            spans.push(span);
        } else {
            let rel = col - pos;
            let chars: Vec<char> = span.content.chars().collect();
            let before: String = chars[..rel].iter().collect();
            let after: String = chars[rel + 1..].iter().collect();

            if !before.is_empty() {
                spans.push(Span::styled(before, span.style));
            }

            spans.push(Span::styled("│", span.style.fg(palette.gutter_fg)));

            if !after.is_empty() {
                spans.push(Span::styled(after, span.style));
            }
        }

        pos += len;
    }

    Line::from(spans)
}

/// The distinct `app.rows` indices a screen row draws, for attachment lookup
/// (deduplicated so a context line drawn in both cells isn't visited twice).
fn covered_rows(screen_row: &ScreenRow) -> Vec<usize> {
    match screen_row {
        ScreenRow::Full(index) => vec![*index],
        ScreenRow::Split { left, right } if left == right => left.iter().copied().collect(),
        ScreenRow::Split { left, right } => left.iter().chain(right.iter()).copied().collect(),
    }
}

/// Render one on-screen row: a full-width row in unified view (or a file/hunk
/// header in split), or a paired split line.
#[allow(clippy::too_many_arguments)]
fn render_screen_row(
    app: &App,
    highlighter: &Highlighter,
    screen_row: &ScreenRow,
    width: usize,
    pane_bg: Color,
    focused: bool,
    lo: usize,
    hi: usize,
    highlighting: Highlighting,
) -> Line<'static> {
    match screen_row {
        ScreenRow::Full(index) => {
            let focus = RowFocus::resolve(
                focused,
                *index == app.diff_cursor,
                app.selecting() && (lo..=hi).contains(index),
            );
            let line = render_row(
                app,
                highlighter,
                &app.rows[*index],
                width,
                pane_bg,
                focus,
                highlighting,
            );

            match app.view {
                DiffView::Split => overlay_divider(line, width.saturating_sub(1) / 2, app.palette),
                DiffView::Unified => line,
            }
        }
        ScreenRow::Split { left, right } => {
            let cell_width = width.saturating_sub(1) / 2;
            let mut spans = render_cell(
                app,
                highlighter,
                *left,
                Side::Old,
                cell_width,
                pane_bg,
                focused,
                lo,
                hi,
                highlighting,
            );
            spans.push(Span::styled(
                "│",
                Style::default().fg(app.palette.gutter_fg).bg(pane_bg),
            ));
            spans.extend(render_cell(
                app,
                highlighter,
                *right,
                Side::New,
                width.saturating_sub(cell_width + 1),
                pane_bg,
                focused,
                lo,
                hi,
                highlighting,
            ));
            Line::from(spans)
        }
    }
}

/// The diff row the open editor is attached to, if any (used as the scroll
/// anchor so the editor stays visible while typing).
fn editor_anchor_row(app: &App) -> Option<usize> {
    let Overlay::Editor(editor) = &app.overlay else {
        return None;
    };

    let (file, side, end_line) = match &editor.mode {
        EditorMode::Create(target) => (&target.path, target.side, target.end.get()),
        EditorMode::Edit(id) => {
            let anchor = &app.annotation(*id)?.annotation.anchor;
            (&anchor.file, anchor.side, anchor.end_line.get())
        }
    };

    row_of_line(app, app.file_index_of(file)?, side, end_line)
}

/// The diff row whose `side` line number is `line_no` within `file_index`.
fn row_of_line(app: &App, file_index: usize, side: Side, line_no: u32) -> Option<usize> {
    app.rows.iter().position(|row| {
        matches!(
            row,
            Row::Line { file_index: fi, line, .. }
                if *fi == file_index
                    && match side {
                        Side::New => line.new_no,
                        Side::Old => line.old_no,
                    }.map(|n| n.get()) == Some(line_no)
        )
    })
}

/// Advance the scroll top so the screen row holding `anchor_row` and everything
/// above it down from the top fits within `height`, counting inline attachment
/// lines. Works in screen-row space so split-view pairing stays aligned, then
/// stores the result back as the `app.rows` index `app.diff_top` points at.
fn adjust_diff_top(
    app: &mut App,
    height: usize,
    screen: &[ScreenRow],
    attachments: &Attachments,
    anchor_row: usize,
) {
    if height == 0 || screen.is_empty() {
        return;
    }

    let anchor = screen_index_of(screen, anchor_row);
    let mut top = screen_index_of(screen, app.diff_top).min(anchor);

    while top < anchor {
        let used: usize = screen[top..=anchor]
            .iter()
            .map(|screen_row| screen_height(screen_row, attachments))
            .sum();

        if used <= height {
            break;
        }

        top += 1;
    }

    app.diff_top = screen[top].anchor_row();
}

/// The on-screen height of a screen row: its single line plus any inline
/// attachment lines hanging beneath the rows it draws.
fn screen_height(screen_row: &ScreenRow, attachments: &Attachments) -> usize {
    let extra: usize = covered_rows(screen_row)
        .iter()
        .map(|row| attachments.get(row).map_or(0, Vec::len))
        .sum();

    1 + extra
}

/// Inline lines to render after each diff row, keyed by row index: annotation
/// blocks beneath their anchor line and the editor block while it is open.
type Attachments = std::collections::HashMap<usize, Vec<Line<'static>>>;

fn build_attachments(app: &App, width: usize) -> Attachments {
    let mut attachments: Attachments = std::collections::HashMap::new();

    let Some(revision) = app.current_revision().map(|r| r.id.clone()) else {
        return attachments;
    };

    let editing = match &app.overlay {
        Overlay::Editor(editor) => match &editor.mode {
            EditorMode::Edit(id) => Some(*id),
            EditorMode::Create(_) => None,
        },
        _ => None,
    };

    for resolved in app.annotations() {
        let anchor = &resolved.annotation.anchor;

        if anchor.revision_id != revision {
            continue;
        }

        // The editor replaces the block for the annotation being edited.
        if Some(resolved.annotation.id) == editing {
            continue;
        }

        let Some(file_index) = app.file_index_of(&anchor.file) else {
            continue;
        };

        if let Some(row) = row_of_line(app, file_index, anchor.side, anchor.end_line.get()) {
            let layout = block_layout(app, width, anchor.side);
            attachments.entry(row).or_default().extend(annotation_block(
                resolved,
                app.palette,
                width,
                &layout,
            ));
        }
    }

    if let (Overlay::Editor(editor), Some(row)) = (&app.overlay, editor_anchor_row(app)) {
        let layout = block_layout(app, width, editor_side(app, editor));
        attachments.entry(row).or_default().extend(editor_block(
            editor,
            app.palette,
            width,
            &layout,
        ));
    }

    attachments
}

/// How an inline block is positioned within a diff row: spans to its left, the
/// content indent, and spans to its right.
struct BlockLayout {
    lead: Vec<Span<'static>>,
    indent: usize,
    trailer: Vec<Span<'static>>,
}

impl BlockLayout {
    /// Columns available for body text after the lead, gutter indent, and
    /// trailer are accounted for.
    fn text_width(&self, width: usize) -> usize {
        let lead: usize = self.lead.iter().map(|s| s.content.chars().count()).sum();
        let trailer: usize = self.trailer.iter().map(|s| s.content.chars().count()).sum();

        width.saturating_sub(lead + self.indent + trailer)
    }

    /// Wrap a block's `content` spans in its lead and trailer, padding the gap to
    /// the next cell boundary with `bg`.
    fn finish(&self, content: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
        let trailer_width: usize = self.trailer.iter().map(|s| s.content.chars().count()).sum();

        let mut row = self.lead.clone();
        row.extend(content);

        let mut spans = padded_row(row, width.saturating_sub(trailer_width), bg).spans;
        spans.extend(self.trailer.iter().cloned());

        Line::from(spans)
    }
}

/// Where an inline block hangs for the active view and anchored `side`. Unified
/// blocks start at column 0; split blocks hang under the cell of their side (new
/// on the right, old on the left), keeping the column divider unbroken.
fn block_layout(app: &App, width: usize, side: Side) -> BlockLayout {
    let divider = || Span::styled("│", Style::default().fg(app.palette.gutter_fg));
    let blank = |cells: usize| Span::styled(" ".repeat(cells), Style::default().bg(Color::Reset));

    match (app.view, side) {
        (DiffView::Unified, _) => BlockLayout {
            lead: Vec::new(),
            indent: CONTENT_INDENT,
            trailer: Vec::new(),
        },
        (DiffView::Split, Side::New) => {
            let cell_width = width.saturating_sub(1) / 2;
            BlockLayout {
                lead: vec![blank(cell_width), divider()],
                indent: SPLIT_CONTENT_INDENT,
                trailer: Vec::new(),
            }
        }
        (DiffView::Split, Side::Old) => {
            let cell_width = width.saturating_sub(1) / 2;
            BlockLayout {
                lead: Vec::new(),
                indent: SPLIT_CONTENT_INDENT,
                trailer: vec![divider(), blank(width.saturating_sub(cell_width + 1))],
            }
        }
    }
}

/// The side the open editor anchors to: its create target's, or the side of the
/// annotation being edited.
fn editor_side(app: &App, editor: &super::app::Editor) -> Side {
    match &editor.mode {
        EditorMode::Create(target) => target.side,
        EditorMode::Edit(id) => app
            .annotation(*id)
            .map(|resolved| resolved.annotation.anchor.side)
            .unwrap_or(Side::New),
    }
}

/// A compact, read-only inline block for an annotation. Its left bar sits in the
/// gutter column and closes (`└`) the bracket opened by the annotated lines
/// above; the type label occupies the gutter region so the body text lines up
/// with the code it annotates, on its own background tint.
fn annotation_block(
    resolved: &ResolvedAnnotation,
    palette: Palette,
    width: usize,
    layout: &BlockLayout,
) -> Vec<Line<'static>> {
    let annotation = &resolved.annotation;
    let bar_color = palette.marker_open;
    let bg = palette.annotation_bg;
    let kind = annotation.annotation_type.map(type_label).unwrap_or("note");

    let body_lines = wrap_text(&annotation.body, layout.text_width(width));
    let last = body_lines.len() - 1;

    body_lines
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let bracket = if index == last { '└' } else { '│' };
            // The bracket takes 2 columns; the type tag fills the rest of the
            // gutter region so body text starts at the content column.
            let tag = if index == 0 { kind } else { "" };
            let gutter = format!("{tag:<width$}", width = layout.indent - 2);

            layout.finish(
                vec![
                    bracket_span(bracket, bar_color, bg),
                    Span::styled(gutter, Style::default().fg(palette.gutter_fg).bg(bg)),
                    Span::styled(
                        text.to_string(),
                        Style::default().fg(palette.default_fg).bg(bg),
                    ),
                ],
                width,
                bg,
            )
        })
        .collect()
}

/// The inline editor block: a title line, the body with a text cursor, and a key
/// hint, wrapped in a self-contained bracket on the annotation background.
fn editor_block(
    editor: &super::app::Editor,
    palette: Palette,
    width: usize,
    layout: &BlockLayout,
) -> Vec<Line<'static>> {
    let bg = palette.annotation_bg;
    let bar_color = palette.marker_open;
    let kind = editor.annotation_type.map(type_label).unwrap_or("none");

    let title = match &editor.mode {
        EditorMode::Create(target) => format!(
            "annotate {}:{}-{}",
            target.path.0.display(),
            target.start.get(),
            target.end.get()
        ),
        EditorMode::Edit(_) => "edit annotation".to_string(),
    };

    let mut contents: Vec<Vec<Span<'static>>> = vec![vec![Span::styled(
        format!("{title}   type: {kind}"),
        Style::default()
            .fg(palette.gutter_fg)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )]];

    let body = editor.text.contents();
    let body_lines: Vec<&str> = if body.is_empty() {
        vec![""]
    } else {
        body.split('\n').collect()
    };

    let (cursor_row, cursor_col) = editor.text.cursor_row_col();
    let cursor_style = Style::default()
        .fg(palette.text_cursor_fg)
        .bg(palette.text_cursor_bg);
    let text_style = Style::default().fg(palette.default_fg).bg(bg);

    for (index, text) in body_lines.iter().enumerate() {
        if index == cursor_row {
            contents.push(cursor_line(text, cursor_col, text_style, cursor_style));
        } else {
            contents.push(vec![Span::styled(text.to_string(), text_style)]);
        }
    }

    let dim = Style::default().fg(palette.gutter_fg).bg(bg);
    let key_style = Style::default()
        .fg(palette.help_key)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let mut hint = Vec::new();

    for (index, (key, label)) in [
        ("ctrl-s", "save"),
        ("ctrl-t", "type"),
        ("ctrl-e", "editor"),
        ("esc", "cancel"),
    ]
    .iter()
    .enumerate()
    {
        if index > 0 {
            hint.push(Span::styled(" · ", dim));
        }

        hint.push(Span::styled(*key, key_style));
        hint.push(Span::styled(format!(" {label}"), dim));
    }

    contents.push(hint);

    // When editing, the annotated lines above already opened the bracket, so the
    // block continues it; when creating, the block opens its own.
    let opening = match editor.mode {
        EditorMode::Create(_) => '┌',
        EditorMode::Edit(_) => '│',
    };
    let last = contents.len() - 1;

    contents
        .into_iter()
        .enumerate()
        .map(|(index, mut spans)| {
            let bracket = match index {
                0 => opening,
                i if i == last => '└',
                _ => '│',
            };
            let mut content = vec![bracket_span(bracket, bar_color, bg)];
            content.append(&mut spans);
            layout.finish(content, width, bg)
        })
        .collect()
}

/// Render one editor line with the cursor highlighted at `column` (counted in
/// `char`s). At or past the line's end the cursor sits on a trailing space.
fn cursor_line(
    line: &str,
    column: usize,
    text_style: Style,
    cursor_style: Style,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let before: String = chars[..column.min(chars.len())].iter().collect();
    let at = chars.get(column).copied().unwrap_or(' ');
    let after: String = chars.get(column + 1..).unwrap_or_default().iter().collect();

    let mut spans = vec![
        Span::styled(before, text_style),
        Span::styled(at.to_string(), cursor_style),
    ];

    if !after.is_empty() {
        spans.push(Span::styled(after, text_style));
    }

    spans
}

/// A gutter-aligned bracket segment (the column-0 glyph plus its trailing space).
fn bracket_span(bracket: char, color: Color, bg: Color) -> Span<'static> {
    Span::styled(format!("{bracket} "), Style::default().fg(color).bg(bg))
}

fn render_row(
    app: &App,
    highlighter: &Highlighter,
    row: &Row,
    width: usize,
    pane_bg: Color,
    focus: RowFocus,
    highlighting: Highlighting,
) -> Line<'static> {
    let palette = app.palette;

    match row {
        Row::File { label, change } => {
            // Headers track only the cursor, not the selection span that may
            // cross them.
            let bg = focus.cursor_background(palette, pane_bg);
            padded_row(
                vec![Span::styled(
                    format!(" ▸ {} {label}", change_glyph(*change)),
                    Style::default()
                        .fg(palette.default_fg)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )],
                width,
                bg,
            )
        }

        Row::Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            section,
            ..
        } => {
            let bg = focus.cursor_background(palette, pane_bg);
            let text = format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@ {section}  (± context)"
            );
            padded_row(
                vec![Span::styled(
                    text,
                    Style::default()
                        .fg(palette.hunk_fg)
                        .bg(bg)
                        .add_modifier(focus.cursor_modifier()),
                )],
                width,
                bg,
            )
        }

        Row::Line {
            file_index,
            extension,
            line,
            emphasis,
        } => render_diff_line(
            app,
            highlighter,
            *file_index,
            extension,
            line,
            emphasis,
            width,
            pane_bg,
            focus,
            highlighting,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_diff_line(
    app: &App,
    highlighter: &Highlighter,
    file_index: usize,
    extension: &str,
    line: &DiffLine,
    emphasis: &[Range<usize>],
    width: usize,
    pane_bg: Color,
    focus: RowFocus,
    highlighting: Highlighting,
) -> Line<'static> {
    let palette = app.palette;

    let line_marker = match line.kind {
        DiffLineKind::Removed => line
            .old_no
            .and_then(|no| app.line_marker(file_index, Side::Old, no.get())),
        _ => line
            .new_no
            .and_then(|no| app.line_marker(file_index, Side::New, no.get())),
    };

    let base_bg = if line_marker.is_some() {
        palette.annotated_line_bg
    } else {
        match line.kind {
            DiffLineKind::Added => palette.add_bg,
            DiffLineKind::Removed => palette.remove_bg,
            DiffLineKind::Context => pane_bg,
        }
    };
    let bg = focus.background(palette, base_bg);

    // Bold the highlighted row so the cursor and selection stay legible under a
    // subtle background tint.
    let modifier = focus.modifier();

    let marker_span = Span::styled(
        format!("{} ", line_marker.map_or(' ', marker_glyph)),
        Style::default()
            .fg(palette.marker_open)
            .bg(bg)
            .add_modifier(modifier),
    );

    let gutter = format!(
        "{:>4} {:>4} ",
        line.old_no.map(|n| n.get().to_string()).unwrap_or_default(),
        line.new_no.map(|n| n.get().to_string()).unwrap_or_default(),
    );

    let (sign, sign_fg) = match line.kind {
        DiffLineKind::Added => ('+', palette.sign_add),
        DiffLineKind::Removed => ('-', palette.sign_remove),
        DiffLineKind::Context => (' ', palette.gutter_fg),
    };

    let mut spans = vec![
        marker_span,
        Span::styled(
            gutter,
            Style::default().fg(sign_fg).bg(bg).add_modifier(modifier),
        ),
        Span::styled(
            sign.to_string(),
            Style::default().fg(sign_fg).bg(bg).add_modifier(modifier),
        ),
    ];

    spans.extend(content_spans(
        highlighter,
        extension,
        line,
        emphasis,
        bg,
        palette,
        focus,
        line_marker,
        modifier,
        highlighting,
    ));

    padded_row(spans, width, bg)
}

/// The styled content spans for a diff line: the syntax-highlighted (or plain)
/// text, with intraline-changed byte ranges tinted on top. Emphasis is
/// suppressed when the row already carries a whole-line background (cursor,
/// selection, or annotation) so those stay uniform.
#[allow(clippy::too_many_arguments)]
fn content_spans(
    highlighter: &Highlighter,
    extension: &str,
    line: &DiffLine,
    emphasis: &[Range<usize>],
    bg: Color,
    palette: Palette,
    focus: RowFocus,
    line_marker: Option<LineMarker>,
    modifier: Modifier,
    highlighting: Highlighting,
) -> Vec<Span<'static>> {
    let (emphasis, emph_bg) = if focus.overrides_emphasis() || line_marker.is_some() {
        (&[][..], bg)
    } else {
        match line.kind {
            DiffLineKind::Added => (emphasis, palette.add_emph_bg),
            DiffLineKind::Removed => (emphasis, palette.remove_emph_bg),
            DiffLineKind::Context => (&[][..], bg),
        }
    };

    let segments: Vec<(String, Color)> = match highlighting {
        Highlighting::Syntax => {
            emphasis::segments_from(highlighter.spans(extension, &line.content)).collect()
        }
        Highlighting::Plain => vec![(line.content.clone(), palette.default_fg)],
    };

    emphasis::styled_content(segments, emphasis, bg, emph_bg, modifier)
}

/// Render one side of a split diff line into exactly `width` columns: the
/// marker, this side's line number, the +/- sign, and truncated content. An
/// absent `row` (no paired line on this side) renders a blank cell.
#[allow(clippy::too_many_arguments)]
fn render_cell(
    app: &App,
    highlighter: &Highlighter,
    row: Option<usize>,
    side: Side,
    width: usize,
    pane_bg: Color,
    focused: bool,
    lo: usize,
    hi: usize,
    highlighting: Highlighting,
) -> Vec<Span<'static>> {
    let palette = app.palette;

    let Some(row_index) = row else {
        return vec![Span::styled(
            " ".repeat(width),
            Style::default().bg(pane_bg),
        )];
    };

    let Some(Row::Line {
        file_index,
        extension,
        line,
        emphasis,
    }) = app.rows.get(row_index)
    else {
        return vec![Span::styled(
            " ".repeat(width),
            Style::default().bg(pane_bg),
        )];
    };

    let number = match side {
        Side::Old => line.old_no,
        Side::New => line.new_no,
    };

    let line_marker = number.and_then(|no| app.line_marker(*file_index, side, no.get()));

    let base_bg = if line_marker.is_some() {
        palette.annotated_line_bg
    } else {
        match line.kind {
            DiffLineKind::Added => palette.add_bg,
            DiffLineKind::Removed => palette.remove_bg,
            DiffLineKind::Context => pane_bg,
        }
    };

    let focus = RowFocus::resolve(
        focused,
        row_index == app.diff_cursor,
        app.selecting() && (lo..=hi).contains(&row_index),
    );

    let bg = focus.background(palette, base_bg);
    let modifier = focus.modifier();

    let (sign, sign_fg) = match line.kind {
        DiffLineKind::Added => ('+', palette.sign_add),
        DiffLineKind::Removed => ('-', palette.sign_remove),
        DiffLineKind::Context => (' ', palette.gutter_fg),
    };

    let mut spans = vec![
        Span::styled(
            format!("{} ", line_marker.map_or(' ', marker_glyph)),
            Style::default()
                .fg(palette.marker_open)
                .bg(bg)
                .add_modifier(modifier),
        ),
        Span::styled(
            format!(
                "{:>4} ",
                number.map(|n| n.get().to_string()).unwrap_or_default()
            ),
            Style::default().fg(sign_fg).bg(bg).add_modifier(modifier),
        ),
        Span::styled(
            sign.to_string(),
            Style::default().fg(sign_fg).bg(bg).add_modifier(modifier),
        ),
    ];

    spans.extend(content_spans(
        highlighter,
        extension,
        line,
        emphasis,
        bg,
        palette,
        focus,
        line_marker,
        modifier,
        highlighting,
    ));

    fit_spans(spans, width, bg)
}

/// Truncate `spans` to at most `width` columns (cutting mid-span if needed), then
/// pad the remainder with `bg` so the cell fills exactly `width`.
fn fit_spans(spans: Vec<Span<'static>>, width: usize, bg: Color) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0;

    for span in spans {
        if used >= width {
            break;
        }

        let len = span.content.chars().count();

        if used + len <= width {
            used += len;
            out.push(span);
        } else {
            let text: String = span.content.chars().take(width - used).collect();
            out.push(Span::styled(text, span.style));
            used = width;
        }
    }

    if used < width {
        out.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }

    out
}

/// Pad a row's spans to `width` so its background fills the line.
fn padded_row(spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let mut spans = spans;
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();

    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }

    Line::from(spans)
}

/// The horizontal rule separating the band from the diff, with a `┴` where the
/// commits view's column divider meets it from above (the files and annotations
/// views are single columns, so their rule is unbroken).
fn render_band_divider(frame: &mut Frame, app: &App, area: Rect) {
    let mut rule: Vec<char> = "─".repeat(area.width as usize).chars().collect();

    if matches!(app.band, BandView::Commits)
        && let Some(cell) = rule.get_mut((commit_columns(area)[1].x - area.x) as usize)
    {
        *cell = '┴';
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rule.into_iter().collect::<String>(),
            Style::default().fg(app.palette.gutter_fg),
        ))),
        area,
    );
}

/// The selected commit's full message, dimming everything after the subject.
fn commit_message_lines(app: &App) -> Vec<Line<'static>> {
    if app.current_message.is_empty() {
        return vec![Line::from(Span::styled(
            "no message",
            Style::default().fg(app.palette.gutter_fg),
        ))];
    }

    app.current_message
        .lines()
        .enumerate()
        .map(|(index, text)| {
            let style = if index == 0 {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(app.palette.gutter_fg)
            };
            Line::from(Span::styled(text.to_string(), style))
        })
        .collect()
}

fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.status_message {
        Some(message) => Line::from(Span::styled(
            format!(" {message}"),
            Style::default()
                .fg(app.palette.marker_open)
                .add_modifier(Modifier::BOLD),
        )),
        None => help_line(app),
    };

    frame.render_widget(Paragraph::new(line), area);
}

/// The key-hint line for the current context. Only keys that act in that context
/// are shown (diff navigation does not appear while browsing commits).
fn help_line(app: &App) -> Line<'static> {
    let hints: &[(&str, &str)] = match (&app.overlay, app.focus, app.band) {
        (Overlay::Editor(_), ..) => &[
            ("type", "body"),
            ("ctrl-t", "type"),
            ("ctrl-s", "save"),
            ("esc", "cancel"),
        ],
        (Overlay::Timeline(_), ..) => &[
            ("j/k ↑/↓", "scroll"),
            ("e", "edit"),
            ("r", "reopen"),
            ("d", "delete"),
            ("esc", "back"),
        ],
        (Overlay::None, Focus::Diff, _) => return diff_help_line(app),
        (Overlay::None, Focus::Band, BandView::Commits) => &[
            ("j/k ↑/↓", "commits"),
            ("enter", "open"),
            ("ctrl-u/d", "scroll msg"),
            ("tab", "diff"),
            ("⇧tab", "view"),
            ("q", "quit"),
        ],
        (Overlay::None, Focus::Band, BandView::Files) => &[
            ("j/k ↑/↓", "files"),
            ("enter", "open"),
            ("tab", "diff"),
            ("⇧tab", "view"),
            ("q", "quit"),
        ],
        (Overlay::None, Focus::Band, BandView::Annotations) => &[
            ("j/k ↑/↓", "move"),
            ("enter", "jump"),
            ("t", "timeline"),
            ("e", "edit"),
            ("d", "delete"),
            ("c", "agent"),
            ("tab", "diff"),
            ("⇧tab", "view"),
        ],
    };

    Line::from(hint_spans(app, hints, None))
}

/// The diff-pane hints, with the select key emphasized while a selection is live.
fn diff_help_line(app: &App) -> Line<'static> {
    let view = match app.view {
        DiffView::Unified => ("s", "split"),
        DiffView::Split => ("s", "unified"),
    };
    let select = match app.selecting() {
        true => ("v", "unselect"),
        false => ("v", "select"),
    };
    let mut hints: Vec<(&str, &str)> = vec![
        ("j/k ↑/↓", "move"),
        ("n/p", "change"),
        ("J/K", "commit"),
        ("+/-", "context"),
        view,
        select,
        ("a", "annotate"),
    ];

    if app.annotation_at_cursor().is_some() {
        hints.push(("d", "delete"));
    }

    if app.annotation_at_cursor().is_some() {
        hints.push(("c", "agent"));
    }

    hints.extend([("u", "undo"), ("t", "timeline")]);

    if app.has_open_annotations() {
        hints.push(("C", "agent all"));
    }

    hints.extend([
        ("L", "log"),
        ("tab", "band"),
        ("⇧tab", "view"),
        ("q", "quit"),
    ]);

    Line::from(hint_spans(app, &hints, app.selecting().then_some("v")))
}

/// Render `(key, label)` hints into a help line: each key bold in the accent
/// color, each label dim, joined by `·`. The key matching `emphasize` is recast
/// in the attention color (used to flag a live selection).
fn hint_spans(app: &App, hints: &[(&str, &str)], emphasize: Option<&str>) -> Vec<Span<'static>> {
    let dim = Style::default().fg(app.palette.gutter_fg);
    let key_style = Style::default()
        .fg(app.palette.help_key)
        .add_modifier(Modifier::BOLD);
    let emphasis_style = Style::default()
        .fg(app.palette.marker_open)
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(" ", dim)];

    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", dim));
        }

        let style = match emphasize {
            Some(emphasized) if emphasized == *key => emphasis_style,
            _ => key_style,
        };
        spans.push(Span::styled(key.to_string(), style));
        spans.push(Span::styled(format!(" {label}"), dim));
    }

    spans
}

fn render_timeline(frame: &mut Frame, app: &App, diff_area: Rect) {
    let Overlay::Timeline(timeline) = &app.overlay else {
        return;
    };
    let Some(resolved) = app.annotation(timeline.annotation_id) else {
        return;
    };
    let annotation = &resolved.annotation;

    let kind = annotation.annotation_type.map(type_label).unwrap_or("note");
    let title = format!(
        " timeline · {}:{} · {kind} ",
        annotation.anchor.file.0.display(),
        annotation.anchor.start_line.get()
    );

    let width = timeline_width(diff_area);
    let text_width = (width as usize).saturating_sub(2 + DETAIL_PREFIX.chars().count());

    let mut lines = vec![
        Line::from(Span::styled(
            format!("\"{}\"", annotation.body.lines().next().unwrap_or("")),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    // Newest first, with a connecting thread between events; the oldest event
    // (the last shown) closes the thread.
    let events = &annotation.timeline;

    for (index, event) in events.iter().rev().enumerate() {
        let is_oldest = index + 1 == events.len();
        lines.extend(event_lines(event, app.palette, text_width, is_oldest));
    }

    if let Some(line) = revision_state_line(&resolved.revision_state, app.palette) {
        lines.push(Line::from(""));
        lines.push(line);
    }

    let rect = timeline_rect(app, diff_area, width, lines.len());
    frame.render_widget(Clear, rect);

    let block = modal_block(title, app.palette);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let view: Vec<Line> = lines.into_iter().skip(timeline.scroll).collect();
    frame.render_widget(Paragraph::new(view).wrap(Wrap { trim: false }), inner);
}

/// The timeline popup width: a comfortable reading column for replies, capped to
/// the diff area so it stays narrower than the full pane.
fn timeline_width(diff_area: Rect) -> u16 {
    diff_area.width.clamp(1, TIMELINE_WIDTH)
}

/// The column within the diff body where an annotation's text begins, so the
/// popup can align under the annotation's content rather than the line gutter.
fn annotation_text_column(app: &App, diff_area: Rect, side: Side) -> u16 {
    match (app.view, side) {
        (DiffView::Unified, _) => CONTENT_INDENT as u16,
        (DiffView::Split, Side::Old) => SPLIT_CONTENT_INDENT as u16,
        (DiffView::Split, Side::New) => {
            diff_area.width.saturating_sub(1) / 2 + 1 + SPLIT_CONTENT_INDENT as u16
        }
    }
}

/// The rectangle for the timeline popup: anchored to the annotation's left edge
/// and placed directly above or below it — whichever side of the diff has more
/// room — so it never covers the annotation. Falls back to centering within the
/// diff area when the annotation isn't on screen or neither side has room.
fn timeline_rect(app: &App, diff_area: Rect, width: u16, content_lines: usize) -> Rect {
    let desired = (content_lines as u16).saturating_add(2);

    let centered = || {
        let height = desired.min(diff_area.height).max(3);
        Rect {
            x: diff_area.x + diff_area.width.saturating_sub(width) / 2,
            y: diff_area.y + diff_area.height.saturating_sub(height) / 2,
            width,
            height,
        }
    };

    let Overlay::Timeline(timeline) = &app.overlay else {
        return centered();
    };
    let Some(anchor) = app
        .annotation(timeline.annotation_id)
        .map(|resolved| resolved.annotation.anchor.clone())
    else {
        return centered();
    };
    let Some((start_y, end_y)) = annotation_screen_span(app, &anchor, diff_area.width as usize)
    else {
        return centered();
    };

    let start_y = start_y.min(diff_area.height);
    let end_y = end_y.min(diff_area.height);
    let space_above = start_y;
    let space_below = diff_area.height.saturating_sub(end_y);

    let (y, height) = if space_below >= space_above {
        (diff_area.y + end_y, desired.min(space_below))
    } else {
        let height = desired.min(space_above);
        (diff_area.y + start_y - height, height)
    };

    if height < 3 {
        return centered();
    }

    // Align under the annotation's text, clamped so the popup stays in the diff.
    let indent = annotation_text_column(app, diff_area, anchor.side);
    let max_x = diff_area.right().saturating_sub(width);
    let x = (diff_area.x + indent).min(max_x).max(diff_area.x);

    Rect {
        x,
        y,
        width,
        height,
    }
}

/// The vertical span the annotation occupies within the diff body, in screen
/// lines measured from its top: `(start_y, end_y)`, where `end_y` includes the
/// inline annotation block hanging beneath the range. `None` when the
/// annotation's lines aren't visible below the current scroll top.
fn annotation_screen_span(app: &App, anchor: &Anchor, width: usize) -> Option<(u16, u16)> {
    let file_index = app.file_index_of(&anchor.file)?;
    let start_row = row_of_line(app, file_index, anchor.side, anchor.start_line.get())?;
    let end_row = row_of_line(app, file_index, anchor.side, anchor.end_line.get())?;

    let screen = screen_rows(app);
    let attachments = build_attachments(app, width);
    let top = screen_index_of(&screen, app.diff_top);

    let mut offset = 0usize;
    let mut start = None;
    let mut end = None;

    for screen_row in &screen[top..] {
        let height = screen_height(screen_row, &attachments);

        if start.is_none() && screen_row.covers(start_row) {
            start = Some(offset);
        }

        if screen_row.covers(end_row) {
            end = Some(offset + height);
        }

        offset += height;
    }

    let cap = |value: usize| value.min(u16::MAX as usize) as u16;
    Some((cap(start?), cap(end?)))
}

/// A line flagging that the anchored change moved in history (amended/rebased),
/// diverged, or was abandoned. `None` when the change is unchanged or the
/// backend can't track change identity (git), so the modal stays quiet.
fn revision_state_line(state: &RevisionState, palette: Palette) -> Option<Line<'static>> {
    let (marker, text) = match state {
        RevisionState::Unchanged | RevisionState::Unsupported => return None,
        RevisionState::Amended { current } => {
            let short: String = current.0.chars().take(7).collect();
            ('~', format!("change amended/rebased since (now {short})"))
        }
        RevisionState::Divergent { commits } => (
            '!',
            format!("change is divergent ({} commits)", commits.len()),
        ),
        RevisionState::Abandoned => ('×', "change was abandoned".to_string()),
    };

    Some(Line::from(Span::styled(
        format!("revision: {marker} {text}"),
        Style::default()
            .fg(palette.marker_attention)
            .add_modifier(Modifier::BOLD),
    )))
}

/// The timeline popup's reading width before it is capped to the diff area.
const TIMELINE_WIDTH: u16 = 96;

/// The agent log row's maximum height before it is capped to half the screen.
const AGENT_LOG_HEIGHT: u16 = 16;

/// The connector prefix carried by every line of an event's detail, drawn so the
/// bar lines up under the event's bullet and the timeline reads as one thread.
const DETAIL_PREFIX: &str = "│ ";

/// Render one event as a bullet header line plus its detail, word-wrapped to
/// `text_width`. The bullet and the detail's connector bar share the leftmost
/// column so consecutive events read as one connected thread; unless this is the
/// oldest event, the thread is carried on to the next with a trailing bar.
fn event_lines(
    event: &Event,
    palette: Palette,
    text_width: usize,
    is_oldest: bool,
) -> Vec<Line<'static>> {
    let actor = match event.actor {
        crate::model::Actor::Reviewer => "reviewer",
        crate::model::Actor::Agent => "agent",
    };
    let (label, detail) = describe_event(&event.kind);
    let thread = Style::default().fg(palette.gutter_fg);

    let mut lines = vec![Line::from(vec![
        Span::styled("● ", thread),
        Span::styled(format!("{}  ", format_timestamp(event)), thread),
        Span::styled(format!("{actor:<8}  "), thread),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
    ])];

    if let Some(detail) = detail {
        for physical in wrap_text(&detail, text_width) {
            lines.push(Line::from(vec![
                Span::styled(DETAIL_PREFIX, thread),
                Span::styled(physical, Style::default().fg(palette.default_fg)),
            ]));
        }
    }

    if !is_oldest {
        lines.push(Line::from(Span::styled("│", thread)));
    }

    lines
}

/// Greedy word-wrap `text` to `width` columns, breaking words longer than the
/// width and preserving blank lines between paragraphs. Always returns at least
/// one line.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        let mut current = String::new();

        for word in paragraph.split_whitespace() {
            if word.chars().count() > width {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }

                let mut chunk = String::new();

                for ch in word.chars() {
                    if chunk.chars().count() == width {
                        lines.push(std::mem::take(&mut chunk));
                    }

                    chunk.push(ch);
                }

                current = chunk;
                continue;
            }

            let separator = usize::from(!current.is_empty());

            if current.chars().count() + separator + word.chars().count() > width {
                lines.push(std::mem::take(&mut current));
                current = word.to_string();
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }

                current.push_str(word);
            }
        }

        lines.push(current);
    }

    lines
}

/// A short label and optional detail for an event kind.
fn describe_event(kind: &EventKind) -> (String, Option<String>) {
    match kind {
        EventKind::AnnotationCreated { body, .. } => ("created".into(), Some(body.clone())),
        EventKind::AnnotationEdited { body, .. } => ("edited".into(), body.clone()),
        EventKind::AgentResolved { reply } => ("resolved".into(), reply.clone()),
        EventKind::AgentWontDo { reply } => ("won't do".into(), reply.clone()),
        EventKind::AgentAddressedBy { revision_id, reply } => {
            let short: String = revision_id.0.chars().take(7).collect();
            (format!("addressed_by → {short}"), reply.clone())
        }
        EventKind::ReviewerReopened { reason } => ("reopened".into(), reason.clone()),
        EventKind::AnnotationDeleted { reason } => ("deleted".into(), reason.clone()),
        EventKind::AnnotationRestored { reason } => ("restored".into(), reason.clone()),
    }
}

/// Format an event timestamp as `YYYY-MM-DD HH:MM`.
fn format_timestamp(event: &Event) -> String {
    event
        .timestamp
        .to_string()
        .chars()
        .take(16)
        .map(|c| if c == 'T' { ' ' } else { c })
        .collect()
}

/// Split a pane into a one-line header and its body.
fn pane_split(area: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area)
}

/// Render a borderless pane header; the focused pane gets a reversed bar.
fn render_header(frame: &mut Frame, area: Rect, title: &str, focused: bool, palette: Palette) {
    let style = if focused {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default().fg(palette.gutter_fg)
    };

    let text = format!(
        " {title:<width$}",
        width = (area.width as usize).saturating_sub(1)
    );
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

/// Render the single-column vertical divider between the sidebar and the diff.
fn render_divider(frame: &mut Frame, area: Rect, palette: Palette) {
    let lines: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(palette.gutter_fg))))
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

/// A bordered block for a modal overlay (popups keep a border; main panes do not).
/// The border uses the muted gutter color so it reads as quiet chrome around the
/// content rather than a loud frame.
fn modal_block(title: String, palette: Palette) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(palette.gutter_fg))
}

/// The gutter glyph for an annotated line: a top hook on the first line, then a
/// vertical bar. The bracket is closed (`└`) by the inline annotation block that
/// hangs below the range, so every annotation reads as one connected shape.
fn marker_glyph(line_marker: LineMarker) -> char {
    match line_marker.position {
        SpanPosition::Single | SpanPosition::Start => '┌',
        SpanPosition::Middle | SpanPosition::End => '│',
    }
}

fn marker_color(marker: Option<Marker>, palette: Palette) -> Color {
    match marker {
        Some(Marker::Open) => palette.marker_open,
        Some(Marker::Resolved) => palette.marker_resolved,
        Some(Marker::Attention) => palette.marker_attention,
        None => palette.gutter_fg,
    }
}

fn change_glyph(change: ChangeKind) -> char {
    match change {
        ChangeKind::Added => 'A',
        ChangeKind::Modified => 'M',
        ChangeKind::Deleted => 'D',
        ChangeKind::Renamed => 'R',
    }
}

fn change_color(change: ChangeKind, palette: Palette) -> Color {
    match change {
        ChangeKind::Added => palette.sign_add,
        ChangeKind::Deleted => palette.sign_remove,
        ChangeKind::Modified | ChangeKind::Renamed => palette.default_fg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_breaks_long_replies_into_multiple_lines() {
        let wrapped = wrap_text("the quick brown fox jumps", 10);

        assert!(wrapped.len() > 1, "a long reply wraps: {wrapped:?}");
        assert!(
            wrapped.iter().all(|line| line.chars().count() <= 10),
            "every wrapped line fits the width: {wrapped:?}"
        );
    }

    #[test]
    fn wrap_text_preserves_explicit_line_breaks() {
        assert_eq!(wrap_text("first\nsecond", 40), vec!["first", "second"]);
    }

    #[test]
    fn wrap_text_hard_breaks_a_word_longer_than_the_width() {
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_text_always_returns_a_line() {
        assert_eq!(wrap_text("", 10), vec![""]);
    }
}
