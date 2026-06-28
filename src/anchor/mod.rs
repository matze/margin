//! Annotation anchoring (PRD §7).
//!
//! An annotation must survive the agent editing the file underneath it. On
//! creation we [`capture`] the anchored text plus a window of leading/trailing
//! context at the reviewed revision. To [`resolve`] against current code we try
//! the recorded line numbers first, then fall back to searching for the
//! captured window. When neither locates the range the annotation is reported
//! [`Resolution::Orphaned`] — kept and surfaced, never silently dropped.

use crate::model::{Anchor, LineNumber, RepoRelPath, RevisionId, Side};

/// Default number of leading/trailing context lines captured per anchor.
pub const CONTEXT_LINES: usize = 3;

/// The outcome of re-anchoring against current file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The range was located at this (possibly shifted) position.
    Located { start: LineNumber, end: LineNumber },
    /// The range could not be located; the annotation is orphaned.
    Orphaned,
}

/// Capture an anchor for the inclusive 1-based range `[start, end]` of `source`,
/// recording up to `context_lines` lines of surrounding context.
///
/// Returns `None` when the range is empty or falls outside `source`.
pub fn capture(
    file: RepoRelPath,
    revision_id: RevisionId,
    side: Side,
    source: &str,
    start: LineNumber,
    end: LineNumber,
    context_lines: usize,
) -> Option<Anchor> {
    let lines: Vec<&str> = source.lines().collect();
    let (from, to) = (start.get() as usize, end.get() as usize);

    if from > to || to > lines.len() {
        return None;
    }

    let owned = |slice: &[&str]| slice.iter().map(|s| s.to_string()).collect();

    Some(Anchor {
        file,
        revision_id,
        start_line: start,
        end_line: end,
        side,
        context_before: owned(
            &lines[from.saturating_sub(1).saturating_sub(context_lines)..from - 1],
        ),
        anchored_text: owned(&lines[from - 1..to]),
        context_after: owned(&lines[to..(to + context_lines).min(lines.len())]),
    })
}

/// Resolve `anchor` against the `current` working-tree content of its file.
///
/// 1. If the recorded line range still holds the anchored text, keep it.
/// 2. Otherwise search for the anchored text; among matches, prefer the one
///    whose surrounding context agrees best, breaking ties by proximity to the
///    recorded position.
/// 3. If the anchored text appears nowhere, report [`Resolution::Orphaned`].
pub fn resolve(anchor: &Anchor, current: &str) -> Resolution {
    if anchor.anchored_text.is_empty() {
        return Resolution::Orphaned;
    }

    let lines: Vec<&str> = current.lines().collect();
    let span = anchor.anchored_text.len();
    let recorded = anchor.start_line.get() as usize - 1;

    if matches_at(&lines, recorded, &anchor.anchored_text) {
        return located(recorded, span);
    }

    let best = (0..=lines.len().saturating_sub(span))
        .filter(|&start| matches_at(&lines, start, &anchor.anchored_text))
        .max_by_key(|&start| {
            (
                context_score(&lines, start, span, anchor),
                // Closer to the recorded line wins ties (negated for max).
                std::cmp::Reverse(start.abs_diff(recorded)),
            )
        });

    match best {
        Some(start) => located(start, span),
        None => Resolution::Orphaned,
    }
}

/// Build a `Located` resolution for a 0-based `start` spanning `span` lines.
fn located(start: usize, span: usize) -> Resolution {
    Resolution::Located {
        start: LineNumber::new(start as u32 + 1).expect("start + 1 is non-zero"),
        end: LineNumber::new((start + span) as u32).expect("span is non-zero"),
    }
}

/// True when `needle` matches `lines` starting at 0-based `start`.
fn matches_at(lines: &[&str], start: usize, needle: &[String]) -> bool {
    start + needle.len() <= lines.len()
        && lines[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(line, want)| *line == want)
}

/// Count how many recorded context lines still sit immediately around a match.
fn context_score(lines: &[&str], start: usize, span: usize, anchor: &Anchor) -> usize {
    let before = anchor
        .context_before
        .iter()
        .rev()
        .zip((0..start).rev().map(|i| lines[i]))
        .take_while(|(want, line)| want.as_str() == *line)
        .count();

    let after_start = start + span;
    let after = anchor
        .context_after
        .iter()
        .zip(lines.iter().skip(after_start))
        .take_while(|(want, line)| want.as_str() == **line)
        .count();

    before + after
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn anchor_for(source: &str, start: u32, end: u32) -> Anchor {
        capture(
            RepoRelPath(PathBuf::from("src/lib.rs")),
            RevisionId("rev0".into()),
            Side::New,
            source,
            LineNumber::new(start).unwrap(),
            LineNumber::new(end).unwrap(),
            CONTEXT_LINES,
        )
        .unwrap()
    }

    const SOURCE: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    println!(\"{a}{b}{c}\");\n}\n";

    #[test]
    fn capture_records_text_and_context() {
        let anchor = anchor_for(SOURCE, 3, 3);
        assert_eq!(anchor.anchored_text, vec!["    let b = 2;"]);
        assert_eq!(anchor.context_before, vec!["fn main() {", "    let a = 1;"]);
        assert_eq!(
            anchor.context_after,
            vec!["    let c = 3;", "    println!(\"{a}{b}{c}\");", "}"]
        );
    }

    #[test]
    fn capture_rejects_out_of_range() {
        assert!(capture(
            RepoRelPath(PathBuf::from("f")),
            RevisionId("r".into()),
            Side::New,
            SOURCE,
            LineNumber::new(99).unwrap(),
            LineNumber::new(99).unwrap(),
            CONTEXT_LINES,
        )
        .is_none());
    }

    #[test]
    fn resolve_unshifted_keeps_position() {
        let anchor = anchor_for(SOURCE, 3, 3);
        assert_eq!(
            resolve(&anchor, SOURCE),
            Resolution::Located {
                start: LineNumber::new(3).unwrap(),
                end: LineNumber::new(3).unwrap(),
            }
        );
    }

    #[test]
    fn resolve_finds_shifted_range_via_context() {
        let anchor = anchor_for(SOURCE, 3, 3);
        // Two lines inserted at the top push the anchored line from 3 to 5.
        let shifted = format!("// header\n// header2\n{SOURCE}");

        assert_eq!(
            resolve(&anchor, &shifted),
            Resolution::Located {
                start: LineNumber::new(5).unwrap(),
                end: LineNumber::new(5).unwrap(),
            }
        );
    }

    #[test]
    fn resolve_disambiguates_duplicates_by_context() {
        // The anchored line "    value" occurs twice; context picks the second.
        let source = "fn a() {\n    value\n}\nfn b() {\n    value\n}\n";
        let anchor = anchor_for(source, 5, 5);
        // Prepend a line so recorded position no longer matches exactly.
        let shifted = format!("// top\n{source}");

        assert_eq!(
            resolve(&anchor, &shifted),
            Resolution::Located {
                start: LineNumber::new(6).unwrap(),
                end: LineNumber::new(6).unwrap(),
            }
        );
    }

    #[test]
    fn resolve_orphans_when_text_is_gone() {
        let anchor = anchor_for(SOURCE, 3, 3);
        let rewritten = "totally different\ncontent here\n";
        assert_eq!(resolve(&anchor, rewritten), Resolution::Orphaned);
    }
}
