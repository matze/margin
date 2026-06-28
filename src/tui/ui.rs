//! Rendering: a pure function of [`App`] state plus the [`Highlighter`].
//!
//! Panes are borderless (PRD §11 / issues: less chrome): each carries a single
//! header bar, a one-column divider separates the sidebar from the diff, and the
//! focused pane is marked by a reversed header. Syntax foreground is layered over
//! diff-semantic backgrounds (PRD §11.1); foreground accents use ANSI named
//! colors so they track the terminal theme.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::export::{status_label, type_label};
use crate::model::{Event, EventKind, Side};
use crate::review::ResolvedAnnotation;
use crate::vcs::{ChangeKind, DiffLine, DiffLineKind, ListingSource};

use super::app::{
    App, EditorMode, Focus, LineMarker, Marker, Overlay, Row, SidebarView, SpanPosition,
    COMMIT_MESSAGE_VIEWPORT,
};
use super::highlight::Highlighter;
use super::theme::Palette;

/// Above this many diff rows, skip syntax highlighting to stay responsive.
const HIGHLIGHT_ROW_CAP: usize = 5000;

/// Columns before a diff line's content: the marker (2) + the two line-number
/// gutters (10) + the +/- sign (1). Inline annotation text is indented to this
/// so it lines up with the code it annotates.
const CONTENT_INDENT: usize = 13;

/// Width of the sidebar column; the vertical divider sits just past it.
const SIDEBAR_WIDTH: u16 = 32;

/// Render the whole screen.
pub fn render(frame: &mut Frame, app: &mut App, highlighter: &Highlighter) {
    let area = frame.area();
    let footer_height = footer_height(app);
    let message_divider = u16::from(footer_height > 0);

    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(message_divider),
        Constraint::Length(footer_height),
        Constraint::Length(1),
    ])
    .split(area);

    let columns = Layout::horizontal([
        Constraint::Length(SIDEBAR_WIDTH),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(rows[0]);

    render_sidebar(frame, app, columns[0]);
    render_divider(frame, columns[1], app.palette);
    render_diff(frame, app, highlighter, columns[2]);

    if message_divider == 1 {
        render_message_divider(frame, app, rows[1]);
    }

    render_footer(frame, app, rows[2]);
    render_help(frame, app, rows[3]);

    match &app.overlay {
        // The editor renders inline within the diff (see build_attachments).
        Overlay::Editor(_) => {}
        Overlay::Timeline(_) => render_timeline(frame, app, area),
        Overlay::None => {}
    }
}

/// Footer height: the commit message when browsing the sidebar; nothing in the
/// diff, where annotations are shown inline beneath their anchor line.
fn footer_height(app: &App) -> u16 {
    match app.focus {
        Focus::Sidebar => app
            .current_message
            .lines()
            .count()
            .clamp(1, COMMIT_MESSAGE_VIEWPORT) as u16,
        Focus::Diff => 0,
    }
}

fn render_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let focused = matches!(app.focus, Focus::Sidebar);
    let [header, body] = pane_split(area);
    let pane_bg = Color::Reset;

    let (title, lines) = match &app.sidebar {
        SidebarView::Commits => (commit_list_title(app), commit_list_lines(app, pane_bg)),
        SidebarView::Annotations { cursor } => (
            format!("annotations · {}", app.overview_annotations().len()),
            annotation_list_lines(app, *cursor, pane_bg),
        ),
    };

    render_header(frame, header, &title, focused, app.palette);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pane_bg)),
        body,
    );
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

fn render_diff(frame: &mut Frame, app: &mut App, highlighter: &Highlighter, area: Rect) {
    let focused = matches!(app.focus, Focus::Diff);
    let [header, body] = pane_split(area);

    let title = match app.current_revision() {
        Some(revision) => {
            let short: String = revision.id.0.chars().take(7).collect();
            let merge = if revision.is_merge { " (merge)" } else { "" };
            format!("{short}{merge}  {}", revision.summary)
        }
        None => "no commit selected".to_string(),
    };
    render_header(frame, header, &title, focused, app.palette);

    let height = body.height as usize;
    app.diff_viewport_height = height;

    let width = body.width as usize;
    let attachments = build_attachments(app, width);

    // Keep the editor (while open) or the cursor within the viewport, counting
    // the inline attachment lines that sit between the scroll top and it.
    let anchor_row = editor_anchor_row(app).unwrap_or(app.diff_cursor);
    adjust_diff_top(app, height, &attachments, anchor_row);

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

    for index in app.diff_top..app.rows.len() {
        if lines.len() >= height {
            break;
        }

        let is_cursor = index == app.diff_cursor && focused;
        let in_selection = focused && app.selecting() && (lo..=hi).contains(&index);
        lines.push(render_row(
            app,
            highlighter,
            &app.rows[index],
            width,
            pane_bg,
            is_cursor,
            in_selection,
            highlight,
        ));

        if let Some(block) = attachments.get(&index) {
            for line in block {
                if lines.len() >= height {
                    break;
                }
                lines.push(line.clone());
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pane_bg)),
        body,
    );
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

/// Advance the scroll top so `anchor_row` and everything above it down from the
/// top fits within `height`, counting inline attachment lines.
fn adjust_diff_top(app: &mut App, height: usize, attachments: &Attachments, anchor_row: usize) {
    if height == 0 {
        return;
    }

    if anchor_row < app.diff_top {
        app.diff_top = anchor_row;
    }

    while app.diff_top < anchor_row {
        let used: usize = (app.diff_top..=anchor_row)
            .map(|index| 1 + attachments.get(&index).map_or(0, Vec::len))
            .sum();

        if used <= height {
            break;
        }

        app.diff_top += 1;
    }
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
            ));
        }
    }

    if let (Overlay::Editor(editor), Some(row)) = (&app.overlay, editor_anchor_row(app)) {
        attachments
            .entry(row)
            .or_default()
            .extend(editor_block(editor, app.palette, width));
    }

    attachments
}

/// A compact, read-only inline block for an annotation. Its left bar sits in the
/// gutter column and closes (`└`) the bracket opened by the annotated lines
/// above; the type label occupies the gutter region so the body text lines up
/// with the code it annotates, on its own background tint.
fn annotation_block(
    resolved: &ResolvedAnnotation,
    palette: Palette,
    width: usize,
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
            let gutter = format!("{tag:<width$}", width = CONTENT_INDENT - 2);

            let spans = vec![
                bracket_span(bracket, bar_color, bg),
                Span::styled(gutter, Style::default().fg(palette.gutter_fg).bg(bg)),
                Span::styled(
                    text.to_string(),
                    Style::default().fg(palette.default_fg).bg(bg),
                ),
            ];
            padded_row(spans, width, bg)
        })
        .collect()
}

/// The inline editor block: a title line, the body with a text cursor, and a key
/// hint, wrapped in a self-contained bracket on the annotation background.
fn editor_block(editor: &super::app::Editor, palette: Palette, width: usize) -> Vec<Line<'static>> {
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
            let mut row = vec![bracket_span(bracket, bar_color, bg)];
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

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Only the sidebar uses the footer, to show the selected commit's message
    // (scrollable with ctrl-u/d); the diff shows annotations inline instead.
    frame.render_widget(
        Paragraph::new(commit_message_lines(app))
            .scroll((app.message_scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// A horizontal rule separating the diff/sidebar from the commit message, with a
/// `┴` where it meets the bottom of the vertical sidebar divider.
fn render_message_divider(frame: &mut Frame, app: &App, area: Rect) {
    let mut rule: Vec<char> = "─".repeat(area.width as usize).chars().collect();

    if let Some(cell) = rule.get_mut(SIDEBAR_WIDTH as usize) {
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
    let hints: &[(&str, &str)] = match (&app.overlay, app.focus, &app.sidebar) {
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
        (Overlay::None, Focus::Sidebar, SidebarView::Annotations { .. }) => &[
            ("j/k ↑↓", "move"),
            ("enter", "jump"),
            ("t", "timeline"),
            ("e", "edit"),
            ("d", "delete"),
            ("g", "commits"),
        ],
        (Overlay::None, Focus::Sidebar, SidebarView::Commits) => &[
            ("j/k ↑↓", "commits"),
            ("enter", "open"),
            ("ctrl-u/d", "scroll msg"),
            ("tab", "diff"),
            ("g", "overview"),
            ("q", "quit"),
        ],
        (Overlay::None, Focus::Diff, _) => return diff_help_line(app),
    };

    Line::from(hint_spans(app, hints, None))
}

/// The diff-pane hints, with the select key emphasized while a selection is live.
fn diff_help_line(app: &App) -> Line<'static> {
    let hints: &[(&str, &str)] = &[
        ("j/k ↑↓", "move"),
        ("n/p", "change"),
        ("+/-", "context"),
        ("v", "select"),
        ("a", "annotate"),
        ("d", "delete"),
        ("u", "undo"),
        ("t", "timeline"),
        ("tab", "focus"),
        ("g", "overview"),
        ("q", "quit"),
    ];

    Line::from(hint_spans(app, hints, app.selecting().then_some("v")))
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

    let view: Vec<Line> = lines.into_iter().skip(timeline.scroll).collect();
    frame.render_widget(Paragraph::new(view).wrap(Wrap { trim: false }), inner);
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
