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

use std::path::Path;

use jiff::Timestamp;

use crate::model::{CommitId, LineNumber, RepoRelPath, RevisionId, Side};

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
    /// The discovered repository root.
    fn root(&self) -> &Path;

    /// Commits under review for the sidebar, plus how the listing was derived.
    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError>;

    /// A revision's own diff against its parent (first parent for merges).
    fn diff(&self, revision: &RevisionId) -> Result<CommitDiff, VcsError>;

    /// File content at a revision, for anchoring and context capture.
    fn file_at(&self, revision: &RevisionId, path: &RepoRelPath) -> Result<String, VcsError>;

    /// File content at a revision's first parent, for resolving old-side anchors
    /// against the version the line was deleted from.
    fn file_at_parent(&self, revision: &RevisionId, path: &RepoRelPath)
    -> Result<String, VcsError>;

    /// The current working revision (`HEAD`/`@`), used to infer the change that
    /// addressed an annotation.
    fn head(&self) -> Result<RevisionId, VcsError>;

    /// The full commit message (subject and body) for a revision.
    fn message(&self, revision: &RevisionId) -> Result<String, VcsError>;

    /// The concrete commit `revision` points at right now, captured with an
    /// anchor so later re-anchoring can detect amend/rebase.
    fn commit_of(&self, revision: &RevisionId) -> Result<CommitId, VcsError>;

    /// The commits `revision`'s change identity currently resolves to, for
    /// classifying an annotation's revision as unchanged/amended/divergent/
    /// abandoned at review time.
    fn change_commits(&self, revision: &RevisionId) -> Result<ChangeCommits, VcsError>;
}

/// The commits a change identity currently resolves to (PRD §6 change tracking).
///
/// Backends with stable change identity (jj) report whether a change still
/// exists and at which commit; git has no such identity across history edits and
/// reports [`ChangeCommits::Unsupported`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeCommits {
    /// The change identity no longer resolves: it was abandoned.
    None,
    /// The change resolves to a single commit.
    One(CommitId),
    /// The change resolves to several commits: it is divergent.
    Many(Vec<CommitId>),
    /// The backend cannot track change identity across history edits (git).
    Unsupported,
}

/// Which backend a repository uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Git,
    Jj,
}

impl Kind {
    /// Parse a `--vcs` / config `vcs` value; unknown values yield `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "git" => Some(Kind::Git),
            "jj" => Some(Kind::Jj),
            _ => None,
        }
    }
}

/// A concrete VCS backend, dispatched statically.
#[derive(Debug, Clone)]
pub enum Backend {
    Git(git::Backend),
    Jj(jj::Backend),
}

impl Backend {
    /// Select a backend for `start`. `forced` honors `--vcs`/config; otherwise
    /// jj is preferred when a jj repo resolves, falling back to git.
    pub fn discover(start: impl AsRef<Path>, forced: Option<Kind>) -> Result<Self, VcsError> {
        let start = start.as_ref();

        match forced {
            Some(Kind::Git) => Ok(Backend::Git(git::Backend::discover(start)?)),
            Some(Kind::Jj) => Ok(Backend::Jj(jj::Backend::discover(start)?)),
            None => match jj::Backend::discover(start) {
                Ok(backend) => Ok(Backend::Jj(backend)),
                Err(_) => Ok(Backend::Git(git::Backend::discover(start)?)),
            },
        }
    }
}

impl Vcs for Backend {
    fn root(&self) -> &Path {
        match self {
            Backend::Git(backend) => backend.root(),
            Backend::Jj(backend) => backend.root(),
        }
    }

    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError> {
        match self {
            Backend::Git(backend) => backend.revisions(base),
            Backend::Jj(backend) => backend.revisions(base),
        }
    }

    fn diff(&self, revision: &RevisionId) -> Result<CommitDiff, VcsError> {
        match self {
            Backend::Git(backend) => backend.diff(revision),
            Backend::Jj(backend) => backend.diff(revision),
        }
    }

    fn file_at(&self, revision: &RevisionId, path: &RepoRelPath) -> Result<String, VcsError> {
        match self {
            Backend::Git(backend) => backend.file_at(revision, path),
            Backend::Jj(backend) => backend.file_at(revision, path),
        }
    }

    fn file_at_parent(
        &self,
        revision: &RevisionId,
        path: &RepoRelPath,
    ) -> Result<String, VcsError> {
        match self {
            Backend::Git(backend) => backend.file_at_parent(revision, path),
            Backend::Jj(backend) => backend.file_at_parent(revision, path),
        }
    }

    fn head(&self) -> Result<RevisionId, VcsError> {
        match self {
            Backend::Git(backend) => backend.head(),
            Backend::Jj(backend) => backend.head(),
        }
    }

    fn message(&self, revision: &RevisionId) -> Result<String, VcsError> {
        match self {
            Backend::Git(backend) => backend.message(revision),
            Backend::Jj(backend) => backend.message(revision),
        }
    }

    fn commit_of(&self, revision: &RevisionId) -> Result<CommitId, VcsError> {
        match self {
            Backend::Git(backend) => backend.commit_of(revision),
            Backend::Jj(backend) => backend.commit_of(revision),
        }
    }

    fn change_commits(&self, revision: &RevisionId) -> Result<ChangeCommits, VcsError> {
        match self {
            Backend::Git(backend) => backend.change_commits(revision),
            Backend::Jj(backend) => backend.change_commits(revision),
        }
    }
}
