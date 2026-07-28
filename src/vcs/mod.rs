//! VCS abstraction (PRD §6).
//!
//! A [`Vcs`] backend exposes just enough of a version-control system for review:
//! the list of commits/revisions under review, a single revision's own diff
//! against its parent, and file content at a revision (for anchoring). The
//! `git` and `jj` backends implement this trait; [`Backend`] dispatches between
//! them, preferring `jj` when a jj repo is present.
//!
//! Per PRD §6 the tool shells out to the `git`/`jj` CLIs rather than linking a
//! library — simpler, fewer build deps, revisited only if performance hurts.

mod git;
mod jj;
mod parse;

use std::path::{Path, PathBuf};
use std::process::Command;

use jiff::Timestamp;

use crate::model::{CommitId, LineNumber, RepoRelPath, ReviewTarget, RevisionId, Side};

/// Errors a VCS backend can surface.
#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("failed to run {tool}: {source}")]
    Spawn {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{tool} {args:?} failed ({status}): {stderr}")]
    Command {
        tool: &'static str,
        args: Vec<String>,
        status: String,
        stderr: String,
    },
    #[error("not inside a {tool} repository")]
    NotARepo { tool: &'static str },
    #[error("failed to parse {what}: {detail}")]
    Parse { what: &'static str, detail: String },
}

/// Run `tool` with `args` in `dir`, returning stdout on success. A spawn failure
/// maps to [`VcsError::Spawn`], a non-zero exit to [`VcsError::Command`].
pub(super) fn run_tool(tool: &'static str, dir: &Path, args: &[&str]) -> Result<String, VcsError> {
    let output = Command::new(tool)
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|source| VcsError::Spawn { tool, source })?;

    if !output.status.success() {
        return Err(VcsError::Command {
            tool,
            args: args.iter().map(|a| a.to_string()).collect(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `tool`'s root-printing command in `start`, returning the trimmed root
/// path. A non-zero exit maps to [`VcsError::NotARepo`].
pub(super) fn discover_root(
    tool: &'static str,
    start: &Path,
    args: &[&str],
) -> Result<PathBuf, VcsError> {
    let output = Command::new(tool)
        .current_dir(start)
        .args(args)
        .output()
        .map_err(|source| VcsError::Spawn { tool, source })?;

    if !output.status.success() {
        return Err(VcsError::NotARepo { tool });
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();

    Ok(PathBuf::from(root))
}

/// Which commits populate the sidebar (PRD §6).
#[derive(Debug, Clone)]
pub enum Base {
    /// An explicit base ref (`--base`): list commits unique to `base..@`.
    Branch(String),
    /// Detect the default branch; if none resolves, fall back to recent commits.
    Auto {
        /// How many recent commits to show when no base can be resolved.
        fallback: usize,
    },
}

/// The sidebar listing plus how it was derived, so the TUI can show a notice
/// when it fell back (PRD §6 "no-base fallback").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revisions {
    pub revisions: Vec<Revision>,
    pub source: ListingSource,
}

/// How a [`Revisions`] listing was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingSource {
    /// Commits unique to `base..@`.
    Range { base: RevisionId },
    /// No base resolved; these are the most recent commits.
    RecentFallback,
}

/// One entry in the sidebar: a commit, or the working copy when the backend
/// surfaces uncommitted changes as their own reviewable target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub target: ReviewTarget,
    pub summary: String,
    pub author: String,
    pub date: Timestamp,
    /// Merge commits are listed, flagged, and diffed against their first parent.
    pub is_merge: bool,
    /// Length of the id's shortest unique prefix, when the backend resolves one
    /// (jj). Highlighted in the listing the way jj highlights it; git leaves it
    /// `None` and the id renders without an accented prefix.
    pub unique_prefix_len: Option<usize>,
}

/// A single target's own diff against its parent (PRD §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDiff {
    pub target: ReviewTarget,
    pub files: Vec<FileDiff>,
}

/// How a file changed in a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// The diff for one file within a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path on the old side (absent for added files).
    pub old_path: Option<RepoRelPath>,
    /// Path on the new side (absent for deleted files).
    pub new_path: Option<RepoRelPath>,
    pub change: ChangeKind,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path to display: new side when present, else old side.
    pub fn display_path(&self) -> Option<&RepoRelPath> {
        self.new_path.as_ref().or(self.old_path.as_ref())
    }
}

/// A contiguous run of changed/context lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// The text after the `@@ ... @@` marker (often the enclosing scope).
    pub section: String,
    pub lines: Vec<DiffLine>,
}

/// One line within a hunk, carrying both old and new line numbers where they
/// apply so an anchor can reference either [`Side`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<LineNumber>,
    pub new_no: Option<LineNumber>,
    pub content: String,
}

/// Whether a diff line is unchanged, added, or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

impl DiffLineKind {
    /// The diff side a line belongs to: added lines are new-only, removed lines
    /// old-only, context lines exist on both (reported as [`Side::New`]).
    pub fn side(self) -> Side {
        match self {
            DiffLineKind::Removed => Side::Old,
            DiffLineKind::Context | DiffLineKind::Added => Side::New,
        }
    }
}

/// Minimum capabilities a backend must provide (PRD §6).
pub trait Vcs {
    /// The discovered repository root.
    fn root(&self) -> &Path;

    /// Commits under review for the sidebar, plus how the listing was derived.
    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError>;

    /// A target's own diff against its parent (first parent for merges; `HEAD`
    /// for the working copy).
    fn diff(&self, target: &ReviewTarget) -> Result<CommitDiff, VcsError>;

    /// File content at a target, for anchoring and context capture.
    fn file_at(&self, target: &ReviewTarget, path: &RepoRelPath) -> Result<String, VcsError>;

    /// File content at a target's first parent, for resolving old-side anchors
    /// against the version the line was deleted from.
    fn file_at_parent(&self, target: &ReviewTarget, path: &RepoRelPath)
    -> Result<String, VcsError>;

    /// The current working revision (`HEAD`/`@`), used to infer the change that
    /// addressed an annotation.
    fn head(&self) -> Result<RevisionId, VcsError>;

    /// The full commit message (subject and body) for a target.
    fn message(&self, target: &ReviewTarget) -> Result<String, VcsError>;

    /// The concrete commit `target` points at right now, captured with an
    /// anchor so later re-anchoring can detect amend/rebase.
    fn commit_of(&self, target: &ReviewTarget) -> Result<CommitId, VcsError>;

    /// The commits `target`'s change identity currently resolves to, for
    /// classifying an annotation's revision as unchanged/amended/divergent/
    /// abandoned at review time. Exact under jj, heuristic under git.
    fn change_commits(&self, target: &ReviewTarget) -> Result<ChangeCommits, VcsError>;
}

/// The commits a change identity currently resolves to (PRD §6 change tracking).
///
/// jj resolves this exactly, from the change id every rewrite preserves. git has
/// no such identity and reconstructs it heuristically from what a rewrite leaves
/// alone; where even that has nothing to go on it reports
/// [`ChangeCommits::Unsupported`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeCommits {
    /// The change identity no longer resolves: it was abandoned.
    None,
    /// The change resolves to a single commit.
    One(CommitId),
    /// The change resolves to several commits: it is divergent.
    Many(Vec<CommitId>),
    /// The backend cannot say which commits the change resolves to.
    Unsupported,
}

impl ChangeCommits {
    /// Classify the commits a change resolved to by how many there are.
    pub(super) fn from_commits(mut commits: Vec<CommitId>) -> Self {
        match commits.len() {
            0 => ChangeCommits::None,
            1 => ChangeCommits::One(commits.pop().expect("len checked")),
            _ => ChangeCommits::Many(commits),
        }
    }
}

/// Which backend a repository uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Git,
    Jj,
}

/// Select a backend for `start`. `forced` honors `--vcs`/config; otherwise jj is
/// preferred when a jj repo resolves, falling back to git.
pub fn discover(start: impl AsRef<Path>, forced: Option<Kind>) -> Result<Box<dyn Vcs>, VcsError> {
    let start = start.as_ref();

    match forced {
        Some(Kind::Git) => Ok(Box::new(git::Backend::discover(start)?)),
        Some(Kind::Jj) => Ok(Box::new(jj::Backend::discover(start)?)),
        None => match jj::Backend::discover(start) {
            Ok(backend) => Ok(Box::new(backend)),
            Err(_) => Ok(Box::new(git::Backend::discover(start)?)),
        },
    }
}
