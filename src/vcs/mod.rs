//! VCS abstraction (PRD §6).
//!
//! A [`Vcs`] backend exposes just enough of a version-control system for review:
//! the list of commits/revisions under review, a single revision's own diff
//! against its parent, and file content at a revision (for anchoring). The
//! `git` backend lands first; `jj` implements the same trait later.
//!
//! Per PRD §6 the tool shells out to the `git`/`jj` CLIs rather than linking a
//! library — simpler, fewer build deps, revisited only if performance hurts.

mod git;

pub use git::GitBackend;

use jiff::Timestamp;

use crate::model::{LineNumber, RepoRelPath, RevisionId, Side};

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

/// One commit/revision in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub id: RevisionId,
    pub summary: String,
    pub author: String,
    pub date: Timestamp,
    /// Merge commits are listed, flagged, and diffed against their first parent.
    pub is_merge: bool,
}

/// A single revision's own diff against its parent (PRD §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDiff {
    pub revision: RevisionId,
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
    /// Commits under review for the sidebar, plus how the listing was derived.
    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError>;

    /// A revision's own diff against its parent (first parent for merges).
    fn diff(&self, revision: &RevisionId) -> Result<CommitDiff, VcsError>;

    /// File content at a revision, for anchoring and context capture.
    fn file_at(&self, revision: &RevisionId, path: &RepoRelPath) -> Result<String, VcsError>;
}
