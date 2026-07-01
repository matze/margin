//! Word-level ("intraline") diff emphasis.
//!
//! A modified line renders with a flat add/remove background, which reads as
//! "the whole line changed" even when a single word did. To narrow that, each
//! modified line paired with its replacement is word-diffed and the changed
//! substrings get a brighter background tint (like `delta`'s
//! `minus-emph-style`/`plus-emph-style`).
//!
//! [`hunk_emphasis`] computes the changed byte ranges per line, once when rows
//! are built. [`styled_content`] overlays those ranges onto a line's syntax
//! spans at render time. Pairing uses `similar`'s inline machinery, which aligns
//! whole removed/added blocks and falls back to no emphasis when two sides are
//! too dissimilar to align.

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use similar::{ChangeTag, TextDiff};

use super::highlight;
use crate::vcs::{DiffLine, DiffLineKind};

/// Changed byte ranges for each line in a hunk, indexed 1:1 with `lines`.
/// Context lines, and added/removed lines with no (or a too-dissimilar) partner,
/// carry no ranges.
pub fn hunk_emphasis(lines: &[DiffLine]) -> Vec<Vec<Range<usize>>> {
    let mut out = vec![Vec::new(); lines.len()];
    let mut index = 0;

    while index < lines.len() {
        if lines[index].kind != DiffLineKind::Removed {
            index += 1;
            continue;
        }

        let removed_start = index;

        while index < lines.len() && lines[index].kind == DiffLineKind::Removed {
            index += 1;
        }

        let added_start = index;

        while index < lines.len() && lines[index].kind == DiffLineKind::Added {
            index += 1;
        }

        if added_start < index {
            emphasize_block(
                lines,
                removed_start..added_start,
                added_start..index,
                &mut out,
            );
        }
    }

    out
}

/// Word-diff a contiguous removed block against the added block that replaces
/// it, writing each side's changed ranges into `out` at the line's index.
fn emphasize_block(
    lines: &[DiffLine],
    removed: Range<usize>,
    added: Range<usize>,
    out: &mut [Vec<Range<usize>>],
) {
    let join = |range: Range<usize>| -> String {
        range
            .map(|idx| format!("{}\n", lines[idx].content))
            .collect()
    };

    let old_text = join(removed.clone());
    let new_text = join(added.clone());
    let diff = TextDiff::from_lines(&old_text, &new_text);

    for op in diff.ops() {
        for change in diff.iter_inline_changes(op) {
            let line_index = match change.tag() {
                ChangeTag::Delete => change.old_index().map(|local| removed.start + local),
                ChangeTag::Insert => change.new_index().map(|local| added.start + local),
                ChangeTag::Equal => None,
            };

            let Some(line_index) = line_index else {
                continue;
            };

            out[line_index] = emphasized_ranges(change.values(), lines[line_index].content.len());
        }
    }
}

/// Collapse an inline change's `(emphasized, text)` segments into the byte
/// ranges of the emphasized ones, clamped to `content_len` (dropping the
/// synthetic trailing newline) and coalescing adjacent ranges.
fn emphasized_ranges(values: &[(bool, &str)], content_len: usize) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut offset = 0;

    for &(emphasized, text) in values {
        let end = offset + text.len();

        if emphasized {
            let start = offset.min(content_len);
            let stop = end.min(content_len);

            if start < stop {
                match ranges.last_mut() {
                    Some(last) if last.end == start => last.end = stop,
                    _ => ranges.push(start..stop),
                }
            }
        }

        offset = end;
    }

    ranges
}

/// Render a line's content into styled spans, splitting each `(text, fg)`
/// segment at `emphasis` boundaries so the changed byte ranges paint on
/// `emph_bg` and the rest on `base_bg`. `emphasis` must be sorted,
/// non-overlapping byte ranges into the concatenated segment text; an empty
/// slice yields one span per segment (no split).
pub fn styled_content(
    segments: impl IntoIterator<Item = (String, Color)>,
    emphasis: &[Range<usize>],
    base_bg: Color,
    emph_bg: Color,
    modifier: Modifier,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut offset = 0;

    for (text, fg) in segments {
        let start = offset;
        let end = offset + text.len();
        offset = end;

        let mut pos = start;

        while pos < end {
            let (piece_end, bg) = match emphasis.iter().find(|range| range.contains(&pos)) {
                Some(range) => (range.end.min(end), emph_bg),
                None => {
                    let next = emphasis
                        .iter()
                        .map(|range| range.start)
                        .find(|&start| start > pos)
                        .unwrap_or(end)
                        .min(end);

                    (next, base_bg)
                }
            };

            spans.push(Span::styled(
                text[pos - start..piece_end - start].to_string(),
                Style::default().fg(fg).bg(bg).add_modifier(modifier),
            ));
            pos = piece_end;
        }
    }

    spans
}

/// Map a highlighter's foreground spans into `(text, fg)` segments for
/// [`styled_content`].
pub fn segments_from(spans: Vec<highlight::Span>) -> impl Iterator<Item = (String, Color)> {
    spans.into_iter().map(|span| (span.text, span.color))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LineNumber;

    fn line(kind: DiffLineKind, content: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no: LineNumber::new(1),
            new_no: LineNumber::new(1),
            content: content.to_string(),
        }
    }

    #[test]
    fn single_word_change_tints_only_that_word() {
        let lines = vec![
            line(DiffLineKind::Removed, "let x = one;"),
            line(DiffLineKind::Added, "let x = two;"),
        ];

        let emphasis = hunk_emphasis(&lines);

        assert_eq!(&lines[0].content[emphasis[0][0].clone()], "one");
        assert_eq!(&lines[1].content[emphasis[1][0].clone()], "two");
    }

    #[test]
    fn pure_insertion_and_deletion_have_no_emphasis() {
        let deletion = hunk_emphasis(&[line(DiffLineKind::Removed, "gone")]);
        assert!(deletion[0].is_empty());

        let insertion = hunk_emphasis(&[line(DiffLineKind::Added, "new")]);
        assert!(insertion[0].is_empty());
    }

    #[test]
    fn context_lines_have_no_emphasis() {
        let lines = vec![
            line(DiffLineKind::Context, "unchanged"),
            line(DiffLineKind::Removed, "alpha beta"),
            line(DiffLineKind::Added, "alpha gamma"),
            line(DiffLineKind::Context, "unchanged"),
        ];

        let emphasis = hunk_emphasis(&lines);

        assert!(emphasis[0].is_empty());
        assert!(emphasis[3].is_empty());
        assert_eq!(&lines[1].content[emphasis[1][0].clone()], "beta");
        assert_eq!(&lines[2].content[emphasis[2][0].clone()], "gamma");
    }

    #[test]
    fn wholly_dissimilar_lines_fall_back_to_no_emphasis() {
        let lines = vec![
            line(DiffLineKind::Removed, "foo"),
            line(DiffLineKind::Added, "completely unrelated sentence here"),
        ];

        let emphasis = hunk_emphasis(&lines);

        assert!(emphasis[0].is_empty());
        assert!(emphasis[1].is_empty());
    }

    #[test]
    fn two_replace_groups_are_diffed_independently() {
        let lines = vec![
            line(DiffLineKind::Removed, "alpha one"),
            line(DiffLineKind::Added, "alpha two"),
            line(DiffLineKind::Context, "gap"),
            line(DiffLineKind::Removed, "beta three"),
            line(DiffLineKind::Added, "beta four"),
        ];

        let emphasis = hunk_emphasis(&lines);

        assert_eq!(&lines[0].content[emphasis[0][0].clone()], "one");
        assert_eq!(&lines[1].content[emphasis[1][0].clone()], "two");
        assert!(emphasis[2].is_empty());
        assert_eq!(&lines[3].content[emphasis[3][0].clone()], "three");
        assert_eq!(&lines[4].content[emphasis[4][0].clone()], "four");
    }

    #[test]
    fn styled_content_splits_a_segment_at_emphasis_boundaries() {
        // One syntax span covering "abcdef"; emphasize the middle "cd".
        let spans = styled_content(
            [("abcdef".to_string(), Color::White)],
            &[2..4],
            Color::Reset,
            Color::Red,
            Modifier::empty(),
        );

        let texts: Vec<&str> = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(texts, ["ab", "cd", "ef"]);
        assert_eq!(spans[0].style.bg, Some(Color::Reset));
        assert_eq!(spans[1].style.bg, Some(Color::Red));
        assert_eq!(spans[2].style.bg, Some(Color::Reset));
    }

    #[test]
    fn styled_content_without_emphasis_keeps_one_span_per_segment() {
        let spans = styled_content(
            [
                ("a".to_string(), Color::White),
                ("bc".to_string(), Color::Blue),
            ],
            &[],
            Color::Reset,
            Color::Red,
            Modifier::empty(),
        );

        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|span| span.style.bg == Some(Color::Reset)));
    }
}
