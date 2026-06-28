//! Shared parsing for the git-format diff and the unit-separated log layout.
//!
//! Both backends emit the same shapes — `git diff`/`jj diff --git` produce
//! identical unified diffs, and the git/jj log templates use the same five
//! `FIELD_SEP`-delimited fields — so the parsing lives here rather than in
//! either backend.

use std::path::PathBuf;

use jiff::Timestamp;

use super::{ChangeKind, DiffLine, DiffLineKind, FileDiff, Hunk, Revision, VcsError};
use crate::model::{LineNumber, RepoRelPath, RevisionId};

/// ASCII unit separator, used to delimit log fields unambiguously.
pub(super) const FIELD_SEP: char = '\u{1f}';

/// Parse one `id\x1fdate\x1fauthor\x1fparents\x1fsummary` log line.
pub(super) fn parse_log_line(line: &str) -> Result<Revision, VcsError> {
    let mut fields = line.splitn(5, FIELD_SEP);

    let mut next = |what: &'static str| {
        fields.next().ok_or(VcsError::Parse {
            what,
            detail: format!("missing field in log line: {line:?}"),
        })
    };

    let id = RevisionId(next("commit id")?.to_string());
    let date_raw = next("commit date")?;
    let author = next("author")?.to_string();
    let parents = next("parents")?;
    let summary = next("summary")?.to_string();

    let date = date_raw
        .parse::<Timestamp>()
        .map_err(|error| VcsError::Parse {
            what: "commit date",
            detail: format!("{date_raw:?}: {error}"),
        })?;

    Ok(Revision {
        id,
        summary,
        author,
        date,
        is_merge: parents.split_whitespace().count() > 1,
    })
}

/// Parse git-format diff output into per-file diffs with hunks and line numbers.
pub(super) fn parse_diff(raw: &str) -> Result<Vec<FileDiff>, VcsError> {
    let mut files = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut counters: Option<LineCounters> = None;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file);
            }

            counters = None;
            let (old_path, new_path) = parse_diff_git_paths(rest);
            current = Some(FileDiff {
                old_path,
                new_path,
                change: ChangeKind::Modified,
                hunks: Vec::new(),
            });

            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if let Some(header) = line.strip_prefix("@@ ") {
            let hunk = parse_hunk_header(header)?;
            counters = Some(LineCounters {
                old: hunk.old_start,
                new: hunk.new_start,
            });
            file.hunks.push(hunk);

            continue;
        }

        if apply_file_header(file, line) {
            continue;
        }

        if let (Some(hunk), Some(counters)) = (file.hunks.last_mut(), counters.as_mut())
            && let Some(diff_line) = parse_body_line(line, counters)
        {
            hunk.lines.push(diff_line);
        }
    }

    if let Some(file) = current.take() {
        files.push(file);
    }

    Ok(files)
}

/// Apply an extended/file header line (mode, rename, `---`/`+++`); returns true
/// when the line was consumed as a header.
fn apply_file_header(file: &mut FileDiff, line: &str) -> bool {
    if line.starts_with("new file mode") {
        file.change = ChangeKind::Added;
    } else if line.starts_with("deleted file mode") {
        file.change = ChangeKind::Deleted;
    } else if let Some(path) = line.strip_prefix("rename from ") {
        file.change = ChangeKind::Renamed;
        file.old_path = Some(RepoRelPath(PathBuf::from(path)));
    } else if let Some(path) = line.strip_prefix("rename to ") {
        file.change = ChangeKind::Renamed;
        file.new_path = Some(RepoRelPath(PathBuf::from(path)));
    } else if let Some(path) = line.strip_prefix("--- ") {
        file.old_path = strip_diff_path(path);
    } else if let Some(path) = line.strip_prefix("+++ ") {
        file.new_path = strip_diff_path(path);
    } else if line.starts_with("index ")
        || line.starts_with("old mode")
        || line.starts_with("similarity index")
        || line.starts_with("dissimilarity index")
        || line.starts_with("copy from ")
        || line.starts_with("copy to ")
    {
        // Recognized but carries no data we keep.
    } else {
        return false;
    }

    true
}

/// One body line of a hunk; `None` for the "\ No newline at end of file" marker.
fn parse_body_line(line: &str, counters: &mut LineCounters) -> Option<DiffLine> {
    // Diff prefixes are ASCII, so the first byte is the marker and the rest is
    // the line content. A truly empty line is treated as empty context.
    let (kind, content) = match line.as_bytes().first() {
        Some(b'+') => (DiffLineKind::Added, &line[1..]),
        Some(b'-') => (DiffLineKind::Removed, &line[1..]),
        Some(b' ') => (DiffLineKind::Context, &line[1..]),
        Some(b'\\') => return None,
        None => (DiffLineKind::Context, ""),
        _ => return None,
    };

    let old_no = matches!(kind, DiffLineKind::Removed | DiffLineKind::Context)
        .then(|| LineNumber::new(counters.old))
        .flatten();
    let new_no = matches!(kind, DiffLineKind::Added | DiffLineKind::Context)
        .then(|| LineNumber::new(counters.new))
        .flatten();

    match kind {
        DiffLineKind::Added => counters.new += 1,
        DiffLineKind::Removed => counters.old += 1,
        DiffLineKind::Context => {
            counters.old += 1;
            counters.new += 1;
        }
    }

    Some(DiffLine {
        kind,
        old_no,
        new_no,
        content: content.to_string(),
    })
}

struct LineCounters {
    old: u32,
    new: u32,
}

/// Parse `a/old b/new` from the `diff --git` line (best-effort; unquoted paths).
fn parse_diff_git_paths(rest: &str) -> (Option<RepoRelPath>, Option<RepoRelPath>) {
    let mut parts = rest.splitn(2, ' ');
    let old = parts.next().and_then(strip_ab_prefix);
    let new = parts.next().and_then(strip_ab_prefix);
    (old, new)
}

/// Turn a `--- `/`+++ ` operand into a path, treating `/dev/null` as absent.
fn strip_diff_path(value: &str) -> Option<RepoRelPath> {
    let value = value.split('\t').next().unwrap_or(value);

    if value == "/dev/null" {
        return None;
    }

    strip_ab_prefix(value)
}

/// Strip the leading `a/` or `b/` git adds to diff paths.
fn strip_ab_prefix(value: &str) -> Option<RepoRelPath> {
    if value == "/dev/null" {
        return None;
    }

    let path = value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
        .unwrap_or(value);

    Some(RepoRelPath(PathBuf::from(path)))
}

/// Parse `-l[,s] +l[,s] @@ section` (the part after the leading `@@ `).
fn parse_hunk_header(header: &str) -> Result<Hunk, VcsError> {
    let invalid = || VcsError::Parse {
        what: "hunk header",
        detail: format!("@@ {header}"),
    };

    let (ranges, section) = header.split_once(" @@").ok_or_else(invalid)?;
    let mut sides = ranges.split_whitespace();

    let (old_start, old_count) = parse_range(sides.next().ok_or_else(invalid)?, '-')?;
    let (new_start, new_count) = parse_range(sides.next().ok_or_else(invalid)?, '+')?;

    Ok(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section: section.trim_start().to_string(),
        lines: Vec::new(),
    })
}

/// Parse one side of a hunk range, e.g. `-12,7` or `+12`.
fn parse_range(value: &str, sign: char) -> Result<(u32, u32), VcsError> {
    let invalid = || VcsError::Parse {
        what: "hunk range",
        detail: value.to_string(),
    };

    let digits = value.strip_prefix(sign).ok_or_else(invalid)?;
    let mut parts = digits.splitn(2, ',');
    let start = parts
        .next()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let count = parts
        .next()
        .map_or(Ok(1), str::parse)
        .map_err(|_| invalid())?;

    Ok((start, count))
}
