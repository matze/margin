use std::path::{Path, PathBuf};
use std::process::Command;

use jiff::Timestamp;

use super::{
    Base, ChangeKind, CommitDiff, DiffLine, DiffLineKind, FileDiff, Hunk, ListingSource, Revision,
    Revisions, Vcs, VcsError,
};
use crate::model::{LineNumber, RepoRelPath, RevisionId};

/// The well-known SHA of git's empty tree, used to diff a root commit (which has
/// no parent) against "nothing".
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Candidate default-branch names tried, in order, when detecting a base.
const DEFAULT_BRANCH_CANDIDATES: [&str; 3] = ["main", "master", "trunk"];

/// ASCII unit separator, used to delimit `git log` fields unambiguously.
const FIELD_SEP: char = '\u{1f}';

/// A `git` backend that shells out to the `git` CLI (PRD §6).
#[derive(Debug, Clone)]
pub struct GitBackend {
    root: PathBuf,
}

impl GitBackend {
    /// Discover the repository containing `start` via `git rev-parse`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, VcsError> {
        let output = Command::new("git")
            .current_dir(start.as_ref())
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|source| VcsError::Spawn { tool: "git", source })?;

        if !output.status.success() {
            return Err(VcsError::NotARepo { tool: "git" });
        }

        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();

        Ok(Self { root: PathBuf::from(root) })
    }

    /// The discovered repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The full commit message (subject and body) for `revision`.
    pub fn message(&self, revision: &RevisionId) -> Result<String, VcsError> {
        Ok(self
            .run(&["log", "-1", "--format=%B", &revision.0])?
            .trim_end()
            .to_string())
    }

    /// Run `git` with `args`, returning stdout on success.
    fn run(&self, args: &[&str]) -> Result<String, VcsError> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .map_err(|source| VcsError::Spawn { tool: "git", source })?;

        if !output.status.success() {
            return Err(VcsError::Command {
                tool: "git",
                args: args.iter().map(|a| a.to_string()).collect(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// True when `rev` resolves to a commit.
    fn verify(&self, rev: &str) -> bool {
        self.run(&["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
            .is_ok()
    }

    /// Resolve a ref to its commit SHA.
    fn resolve(&self, rev: &str) -> Result<RevisionId, VcsError> {
        Ok(RevisionId(self.run(&["rev-parse", "--verify", rev])?.trim().to_string()))
    }

    /// Detect the repository's default branch (PRD §6 base resolution).
    fn detect_default_branch(&self) -> Option<String> {
        if let Ok(out) = self.run(&["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
            if let Some(branch) = out.trim().strip_prefix("origin/") {
                return Some(branch.to_string());
            }
        }

        DEFAULT_BRANCH_CANDIDATES
            .into_iter()
            .find(|name| self.verify(name))
            .map(str::to_string)
    }

    /// List commits for `range` (e.g. `base..HEAD`, or `HEAD` for fallback).
    fn log(&self, range: &str, extra: &[&str]) -> Result<Vec<Revision>, VcsError> {
        let format = format!("--pretty=format:%H{sep}%cI{sep}%an{sep}%P{sep}%s", sep = FIELD_SEP);
        let mut args = vec!["log", &format, range];
        args.extend_from_slice(extra);

        self.run(&args)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse_log_line)
            .collect()
    }
}

impl Vcs for GitBackend {
    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError> {
        let resolved_base = match base {
            Base::Branch(name) => Some(self.resolve(name)?),
            Base::Auto { .. } => self
                .detect_default_branch()
                .map(|name| self.resolve(&name))
                .transpose()?,
        };

        match (resolved_base, base) {
            (Some(base_id), _) => {
                let range = format!("{}..HEAD", base_id.0);
                Ok(Revisions {
                    revisions: self.log(&range, &[])?,
                    source: ListingSource::Range { base: base_id },
                })
            }
            (None, Base::Auto { fallback }) => Ok(Revisions {
                revisions: self.log("HEAD", &["-n", &fallback.to_string()])?,
                source: ListingSource::RecentFallback,
            }),
            (None, Base::Branch(name)) => Err(VcsError::Parse {
                what: "base",
                detail: format!("could not resolve base ref {name}"),
            }),
        }
    }

    fn diff(&self, revision: &RevisionId) -> Result<CommitDiff, VcsError> {
        let parent = format!("{}^1", revision.0);
        let parent: &str = if self.verify(&parent) { &parent } else { EMPTY_TREE };

        let raw = self.run(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            parent,
            &revision.0,
        ])?;

        Ok(CommitDiff {
            revision: revision.clone(),
            files: parse_diff(&raw)?,
        })
    }

    fn file_at(&self, revision: &RevisionId, path: &RepoRelPath) -> Result<String, VcsError> {
        let spec = format!("{}:{}", revision.0, path.0.display());
        self.run(&["show", &spec])
    }
}

/// Parse one `%H\x1f%cI\x1f%an\x1f%P\x1f%s` log line.
fn parse_log_line(line: &str) -> Result<Revision, VcsError> {
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

    let date = date_raw.parse::<Timestamp>().map_err(|error| VcsError::Parse {
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

/// Parse `git diff` output into per-file diffs with hunks and line numbers.
fn parse_diff(raw: &str) -> Result<Vec<FileDiff>, VcsError> {
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

        if let (Some(hunk), Some(counters)) = (file.hunks.last_mut(), counters.as_mut()) {
            if let Some(diff_line) = parse_body_line(line, counters) {
                hunk.lines.push(diff_line);
            }
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
    let start = parts.next().ok_or_else(invalid)?.parse().map_err(|_| invalid())?;
    let count = parts.next().map_or(Ok(1), str::parse).map_err(|_| invalid())?;

    Ok((start, count))
}
