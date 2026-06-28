//! Rendering: a pure function of [`App`] state plus the [`Highlighter`].
//!
//! Panes are borderless (PRD §11 / issues: less chrome): each carries a single
//! header bar, the top band shows one view at a time (commits, files, or
//! annotations) with a horizontal rule separating it from the diff, and the
//! focused pane is marked by a reversed header. Syntax foreground is layered over diff-semantic
//! backgrounds (PRD §11.1); foreground accents use ANSI named colors so they
//! track the terminal theme.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::export::{status_label, type_label};
use crate::model::{Event, EventKind, Side};
use crate::review::{ResolvedAnnotation, RevisionState};
use crate::vcs::{ChangeKind, DiffLine, DiffLineKind, ListingSource};

use super::app::{
    App, BandView, DiffView, EditorMode, Focus, LineMarker, Marker, Overlay, Row, SpanPosition,
    keep_in_view,
};
use super::highlight::Highlighter;
use super::theme::Palette;

/// Above this many diff rows, skip syntax highlighting to stay responsive.
const HIGHLIGHT_ROW_CAP: usize = 5000;

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

    let rows = Layout::vertical([
        Constraint::Length(band),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    render_band(frame, app, rows[0]);
    render_band_divider(frame, app, rows[1]);
    render_diff(frame, app, highlighter, rows[2]);
    render_help(frame, app, rows[3]);

    match &app.overlay {
        // The editor renders inline within the diff (see build_attachments).
        Overlay::Editor(_) => {}
        Overlay::Timeline(_) => render_timeline(frame, app, area),
        Overlay::None => {}
    }
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
/// the band body and its bottom rule stay column-aligned. The commit list and the
/// message column split the band evenly.
fn commit_columns(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::horizontal([
        Constraint::Percentage(50),
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
                file_list_lines(app, Color::Reset),
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
                annotation_list_lines(app, app.annotation_cursor, Color::Reset),
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
        commit_list_lines(app, Color::Reset),
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

/// The changed-file panel: one row per file in the loaded commit, with its
/// change glyph and repo-relative path.
fn file_list_lines(app: &App, pane_bg: Color) -> Vec<Line<'static>> {
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
            let base_style = if selected {
                Style::default()
                    .bg(app.palette.cursor_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(pane_bg)
            };

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

fn commit_list_lines(app: &App, pane_bg: Color) -> Vec<Line<'static>> {
    app.revisions()
        .iter()
        .enumerate()
        .map(|(index, revision)| {
            let marker = app.commit_marker(&revision.id);
            let glyph = marker.map_or(' ', Marker::glyph);
            let short: String = revision.id.0.chars().take(7).collect();

            let selected = index == app.commit_cursor;
            let base_style = if selected {
                Style::default()
                    .bg(app.palette.cursor_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(pane_bg)
            };

            Line::from(vec![
                Span::styled(
                    format!(" {glyph} "),
                    Style::default()
                        .fg(marker_color(marker, app.palette))
                        .bg(base_style.bg.unwrap_or(pane_bg)),
                ),
                Span::styled(format!("{short} "), base_style.fg(app.palette.gutter_fg)),
                Span::styled(revision.summary.clone(), base_style),
            ])
        })
        .collect()
}

fn annotation_list_lines(app: &App, cursor: usize, pane_bg: Color) -> Vec<Line<'static>> {
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
            let base_style = if selected {
                Style::default()
                    .bg(app.palette.cursor_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(pane_bg)
            };

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

    let highlight = app.rows.len() <= HIGHLIGHT_ROW_CAP;
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
            highlight,
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
    highlight: bool,
) -> Line<'static> {
    match screen_row {
        ScreenRow::Full(index) => {
            let is_cursor = *index == app.diff_cursor && focused;
            let in_selection = focused && app.selecting() && (lo..=hi).contains(index);
            let line = render_row(
                app,
                highlighter,
                &app.rows[*index],
                width,
                pane_bg,
                is_cursor,
                in_selection,
                highlight,
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
                highlight,
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
                highlight,
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

    let (lead, indent) = block_layout(app, width);

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

        if anchor.revision_id != revision || anchor.side != Side::New {
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
            attachments.entry(row).or_default().extend(annotation_block(
                resolved,
                app.palette,
                width,
                &lead,
                indent,
            ));
        }
    }

    if let (Overlay::Editor(editor), Some(row)) = (&app.overlay, editor_anchor_row(app)) {
        attachments
            .entry(row)
            .or_default()
            .extend(editor_block(editor, app.palette, width, &lead));
    }

    attachments
}

/// The left lead spans and content indent for inline blocks. Unified blocks
/// start at column 0; split blocks hang under the right (new) cell, past the
/// left cell and divider, since annotations anchor the new side.
fn block_layout(app: &App, width: usize) -> (Vec<Span<'static>>, usize) {
    match app.view {
        DiffView::Unified => (Vec::new(), CONTENT_INDENT),
        DiffView::Split => {
            let cell_width = width.saturating_sub(1) / 2;
            let lead = vec![
                Span::styled(" ".repeat(cell_width), Style::default().bg(Color::Reset)),
                Span::styled("│", Style::default().fg(app.palette.gutter_fg)),
            ];
            (lead, SPLIT_CONTENT_INDENT)
        }
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
    lead: &[Span<'static>],
    indent: usize,
) -> Vec<Line<'static>> {
    let annotation = &resolved.annotation;
    let bar_color = palette.marker_open;
    let bg = palette.annotation_bg;
    let kind = annotation.annotation_type.map(type_label).unwrap_or("note");

    let body_lines: Vec<&str> = match annotation.body.lines().collect::<Vec<_>>() {
        empty if empty.is_empty() => vec![""],
        lines => lines,
    };
    let last = body_lines.len() - 1;

    body_lines
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let bracket = if index == last { '└' } else { '│' };
            // The bracket takes 2 columns; the type tag fills the rest of the
            // gutter region so body text starts at the content column.
            let tag = if index == 0 { kind } else { "" };
            let gutter = format!("{tag:<width$}", width = indent - 2);

            let mut spans = lead.to_vec();
            spans.extend([
                bracket_span(bracket, bar_color, bg),
                Span::styled(gutter, Style::default().fg(palette.gutter_fg).bg(bg)),
                Span::styled(
                    text.to_string(),
                    Style::default().fg(palette.default_fg).bg(bg),
                ),
            ]);
            padded_row(spans, width, bg)
        })
        .collect()
}

/// The inline editor block: a title line, the body with a text cursor, and a key
/// hint, wrapped in a self-contained bracket on the annotation background.
fn editor_block(
    editor: &super::app::Editor,
    palette: Palette,
    width: usize,
    lead: &[Span<'static>],
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
        format!("{title}   type: {kind} (ctrl-t)"),
        Style::default()
            .fg(palette.gutter_fg)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )]];

    let body_lines: Vec<&str> = if editor.body.is_empty() {
        vec![""]
    } else {
        editor.body.split('\n').collect()
    };

    for (index, text) in body_lines.iter().enumerate() {
        let mut shown = text.to_string();

        if index + 1 == body_lines.len() {
            shown.push('▏'); // text cursor at the end of the buffer
        }

        contents.push(vec![Span::styled(
            shown,
            Style::default().fg(palette.default_fg).bg(bg),
        )]);
    }

    contents.push(vec![Span::styled(
        "ctrl-s save · ctrl-t type · esc cancel",
        Style::default().fg(palette.gutter_fg).bg(bg),
    )]);

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
            let mut row = lead.to_vec();
            row.push(bracket_span(bracket, bar_color, bg));
            row.append(&mut spans);
            padded_row(row, width, bg)
        })
        .collect()
}

/// A gutter-aligned bracket segment (the column-0 glyph plus its trailing space).
fn bracket_span(bracket: char, color: Color, bg: Color) -> Span<'static> {
    Span::styled(format!("{bracket} "), Style::default().fg(color).bg(bg))
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    app: &App,
    highlighter: &Highlighter,
    row: &Row,
    width: usize,
    pane_bg: Color,
    is_cursor: bool,
    in_selection: bool,
    highlight: bool,
) -> Line<'static> {
    let palette = app.palette;

    match row {
        Row::File { label, change } => {
            let bg = if is_cursor {
                palette.cursor_bg
            } else {
                pane_bg
            };
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
            let bg = if is_cursor {
                palette.cursor_bg
            } else {
                pane_bg
            };
            let emphasis = if is_cursor {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
            let text = format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@ {section}  (± context)"
            );
            padded_row(
                vec![Span::styled(
                    text,
                    Style::default()
                        .fg(palette.hunk_fg)
                        .bg(bg)
                        .add_modifier(emphasis),
                )],
                width,
                bg,
            )
        }

        Row::Line {
            file_index,
            extension,
            line,
        } => render_diff_line(
            app,
            highlighter,
            *file_index,
            extension,
            line,
            width,
            pane_bg,
            is_cursor,
            in_selection,
            highlight,
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
    width: usize,
    pane_bg: Color,
    is_cursor: bool,
    in_selection: bool,
    highlight: bool,
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
    let bg = if is_cursor {
        palette.cursor_bg
    } else if in_selection {
        palette.selection_bg
    } else {
        base_bg
    };

    // Bold the highlighted row so the cursor and selection stay legible under a
    // subtle background tint.
    let emphasis = if is_cursor || in_selection {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };

    let marker_span = Span::styled(
        format!("{} ", line_marker.map_or(' ', marker_glyph)),
        Style::default()
            .fg(palette.marker_open)
            .bg(bg)
            .add_modifier(emphasis),
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
            Style::default()
                .fg(palette.gutter_fg)
                .bg(bg)
                .add_modifier(emphasis),
        ),
        Span::styled(
            sign.to_string(),
            Style::default().fg(sign_fg).bg(bg).add_modifier(emphasis),
        ),
    ];

    let content_spans = if highlight {
        highlighter
            .spans(extension, &line.content)
            .into_iter()
            .map(|span| {
                Span::styled(
                    span.text,
                    Style::default()
                        .fg(span.color)
                        .bg(bg)
                        .add_modifier(emphasis),
                )
            })
            .collect()
    } else {
        vec![Span::styled(
            line.content.clone(),
            Style::default()
                .fg(palette.default_fg)
                .bg(bg)
                .add_modifier(emphasis),
        )]
    };
    spans.extend(content_spans);

    padded_row(spans, width, bg)
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
    highlight: bool,
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

    let is_cursor = row_index == app.diff_cursor && focused;
    let in_selection = focused && app.selecting() && (lo..=hi).contains(&row_index);

    let bg = if is_cursor {
        palette.cursor_bg
    } else if in_selection {
        palette.selection_bg
    } else {
        base_bg
    };

    let emphasis = if is_cursor || in_selection {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };

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
                .add_modifier(emphasis),
        ),
        Span::styled(
            format!(
                "{:>4} ",
                number.map(|n| n.get().to_string()).unwrap_or_default()
            ),
            Style::default()
                .fg(palette.gutter_fg)
                .bg(bg)
                .add_modifier(emphasis),
        ),
        Span::styled(
            sign.to_string(),
            Style::default().fg(sign_fg).bg(bg).add_modifier(emphasis),
        ),
    ];

    if highlight {
        spans.extend(
            highlighter
                .spans(extension, &line.content)
                .into_iter()
                .map(|span| {
                    Span::styled(
                        span.text,
                        Style::default()
                            .fg(span.color)
                            .bg(bg)
                            .add_modifier(emphasis),
                    )
                }),
        );
    } else {
        spans.push(Span::styled(
            line.content.clone(),
            Style::default()
                .fg(palette.default_fg)
                .bg(bg)
                .add_modifier(emphasis),
        ));
    }

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
            ("j/k ↑↓", "scroll"),
            ("e", "edit"),
            ("r", "reopen"),
            ("d", "delete"),
            ("esc", "back"),
        ],
        (Overlay::None, Focus::Diff, _) => return diff_help_line(app),
        (Overlay::None, Focus::Band, BandView::Commits) => &[
            ("j/k ↑↓", "commits"),
            ("enter", "open"),
            ("ctrl-u/d", "scroll msg"),
            ("tab", "diff"),
            ("⇧tab", "view"),
            ("q", "quit"),
        ],
        (Overlay::None, Focus::Band, BandView::Files) => &[
            ("j/k ↑↓", "files"),
            ("enter", "open"),
            ("tab", "diff"),
            ("⇧tab", "view"),
            ("q", "quit"),
        ],
        (Overlay::None, Focus::Band, BandView::Annotations) => &[
            ("j/k ↑↓", "move"),
            ("enter", "jump"),
            ("t", "timeline"),
            ("e", "edit"),
            ("d", "delete"),
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
        ("j/k ↑↓", "move"),
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

    hints.extend([
        ("u", "undo"),
        ("t", "timeline"),
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

fn render_timeline(frame: &mut Frame, app: &App, area: Rect) {
    let Overlay::Timeline(timeline) = &app.overlay else {
        return;
    };
    let Some(resolved) = app.annotation(timeline.annotation_id) else {
        return;
    };
    let annotation = &resolved.annotation;

    let rect = centered(area, 80, 70);
    frame.render_widget(Clear, rect);

    let short_rev: String = annotation.anchor.revision_id.0.chars().take(7).collect();
    let side = match annotation.anchor.side {
        Side::New => "new",
        Side::Old => "old",
    };
    let title = format!(
        " timeline · {}:{} (@ {short_rev}, {side}) ",
        annotation.anchor.file.0.display(),
        annotation.anchor.start_line.get()
    );
    let block = modal_block(title, app.palette);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let kind = annotation.annotation_type.map(type_label).unwrap_or("note");
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("\"{}\"", annotation.body.lines().next().unwrap_or("")),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   type: {kind}"),
                Style::default().fg(app.palette.gutter_fg),
            ),
        ]),
        Line::from(""),
    ];

    for event in &annotation.timeline {
        lines.extend(event_lines(event, app.palette));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("status (derived): {}", status_label(resolved.status)),
        Style::default().add_modifier(Modifier::BOLD),
    )));

    if let Some(line) = revision_state_line(&resolved.revision_state, app.palette) {
        lines.push(line);
    }

    let view: Vec<Line> = lines.into_iter().skip(timeline.scroll).collect();
    frame.render_widget(Paragraph::new(view).wrap(Wrap { trim: false }), inner);
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

/// Render one event as a header line plus an optional indented detail line.
fn event_lines(event: &Event, palette: Palette) -> Vec<Line<'static>> {
    let actor = match event.actor {
        crate::model::Actor::Reviewer => "reviewer",
        crate::model::Actor::Agent => "agent",
    };
    let (label, detail) = describe_event(&event.kind);

    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::default().fg(palette.marker_open)),
        Span::styled(
            format!("{}  ", format_timestamp(event)),
            Style::default().fg(palette.gutter_fg),
        ),
        Span::styled(
            format!("{actor:<9}"),
            Style::default().fg(palette.default_fg),
        ),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
    ])];

    if let Some(detail) = detail {
        lines.push(Line::from(Span::styled(
            format!("│   {detail}"),
            Style::default().fg(palette.gutter_fg),
        )));
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
fn modal_block(title: String, palette: Palette) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(palette.marker_open))
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

/// A rectangle centered in `area` at the given percentage of its size.
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [horizontal] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    let [rect] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(horizontal);
    rect
}
