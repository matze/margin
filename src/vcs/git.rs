use std::path::{Path, PathBuf};

use jiff::Timestamp;

use super::parse::{FIELD_SEP, parse_diff, parse_log_line};
use super::{Base, ChangeCommits, CommitDiff, ListingSource, Revision, Revisions, Vcs, VcsError};
use crate::model::{CommitId, RepoRelPath, ReviewTarget, RevisionId};

/// The well-known SHA of git's empty tree, used to diff a root commit (which has
/// no parent) against "nothing".
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Candidate default-branch names tried, in order, when detecting a base.
const DEFAULT_BRANCH_CANDIDATES: [&str; 3] = ["main", "master", "trunk"];

/// Diff of the working copy against `HEAD`. Tracked files only: an untracked
/// file has no diff to review, and `HEAD` is the parent the review is against.
const DIFF_ARGS_WORKING_COPY: [&str; 5] = [
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--find-renames",
    "HEAD",
];

/// Stands in for the commit message of uncommitted changes, which have none.
const UNCOMMITTED_MESSAGE: &str = "(uncommitted changes)";

/// A `git` backend that shells out to the `git` CLI (PRD §6).
#[derive(Debug, Clone)]
pub struct Backend {
    root: PathBuf,
}

impl Backend {
    /// Discover the repository containing `start` via `git rev-parse`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, VcsError> {
        let root = super::discover_root("git", start.as_ref(), &["rev-parse", "--show-toplevel"])?;

        Ok(Self { root })
    }

    /// Run `git` with `args`, returning stdout on success.
    fn run(&self, args: &[&str]) -> Result<String, VcsError> {
        super::run_tool("git", &self.root, args)
    }

    /// True when `rev` resolves to a commit.
    fn verify(&self, rev: &str) -> bool {
        self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .is_ok()
    }

    /// Resolve a ref to its commit SHA.
    fn resolve(&self, rev: &str) -> Result<RevisionId, VcsError> {
        Ok(RevisionId(
            self.run(&["rev-parse", "--verify", rev])?
                .trim()
                .to_string(),
        ))
    }

    /// Detect the repository's default branch (PRD §6 base resolution).
    fn detect_default_branch(&self) -> Option<String> {
        if let Ok(out) = self.run(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]) && let Some(branch) = out.trim().strip_prefix("origin/")
        {
            return Some(branch.to_string());
        }

        DEFAULT_BRANCH_CANDIDATES
            .into_iter()
            .find(|name| self.verify(name))
            .map(str::to_string)
    }

    /// The working copy as a sidebar entry, when it has changes worth reviewing.
    /// git has no commit for it, so unlike jj's `@` it must be synthesized.
    fn working_copy(&self) -> Option<Revision> {
        let changed = self.verify("HEAD")
            && self
                .run(&["diff", "--name-only", "HEAD"])
                .is_ok_and(|out| !out.trim().is_empty());

        changed.then(|| Revision {
            target: ReviewTarget::WorkingCopy,
            summary: UNCOMMITTED_MESSAGE.to_string(),
            author: String::new(),
            date: Timestamp::now(),
            is_merge: false,
            unique_prefix_len: None,
        })
    }

    /// List commits for `range` (e.g. `base..HEAD`, or `HEAD` for fallback).
    fn log(&self, range: &str, extra: &[&str]) -> Result<Vec<Revision>, VcsError> {
        let format = format!(
            "--pretty=format:%H{sep}%cI{sep}%an{sep}%P{sep}%s",
            sep = FIELD_SEP
        );
        let mut args = vec!["log", &format, range];
        args.extend_from_slice(extra);

        self.run(&args)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse_log_line)
            .collect()
    }
}

impl Vcs for Backend {
    fn root(&self) -> &Path {
        &self.root
    }

    fn message(&self, target: &ReviewTarget) -> Result<String, VcsError> {
        let Some(revision) = target.revision() else {
            return Ok(UNCOMMITTED_MESSAGE.to_string());
        };

        Ok(self
            .run(&["log", "-1", "--format=%B", &revision.0])?
            .trim_end()
            .to_string())
    }

    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError> {
        let resolved_base = match base {
            Base::Branch(name) => Some(self.resolve(name)?),
            Base::Auto { .. } => self
                .detect_default_branch()
                .map(|name| self.resolve(&name))
                .transpose()?,
        };

        // The working copy leads the listing: it is the newest state, and the
        // one a reviewer handed an agent's uncommitted edits looks at first.
        let lead = self.working_copy();
        let with_lead = |commits: Vec<Revision>| lead.into_iter().chain(commits).collect();

        match (resolved_base, base) {
            (Some(base_id), _) => {
                let range = format!("{}..HEAD", base_id.0);
                Ok(Revisions {
                    revisions: with_lead(self.log(&range, &[])?),
                    source: ListingSource::Range { base: base_id },
                })
            }
            (None, Base::Auto { fallback }) => Ok(Revisions {
                revisions: with_lead(self.log("HEAD", &["-n", &fallback.to_string()])?),
                source: ListingSource::RecentFallback,
            }),
            (None, Base::Branch(name)) => Err(VcsError::Parse {
                what: "base",
                detail: format!("could not resolve base ref {name}"),
            }),
        }
    }

    fn diff(&self, target: &ReviewTarget) -> Result<CommitDiff, VcsError> {
        let Some(revision) = target.revision() else {
            let raw = self.run(&DIFF_ARGS_WORKING_COPY)?;

            return Ok(CommitDiff {
                target: target.clone(),
                files: parse_diff(&raw)?,
            });
        };

        let parent = format!("{}^1", revision.0);
        let parent: &str = if self.verify(&parent) {
            &parent
        } else {
            EMPTY_TREE
        };

        let raw = self.run(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            parent,
            &revision.0,
        ])?;

        Ok(CommitDiff {
            target: target.clone(),
            files: parse_diff(&raw)?,
        })
    }

    fn file_at(&self, target: &ReviewTarget, path: &RepoRelPath) -> Result<String, VcsError> {
        // The working copy's own version is the file on disk.
        let Some(revision) = target.revision() else {
            return std::fs::read_to_string(self.root.join(&path.0)).map_err(|source| {
                VcsError::Spawn {
                    tool: "git",
                    source,
                }
            });
        };

        let spec = format!("{}:{}", revision.0, path.0.display());
        self.run(&["show", &spec])
    }

    fn file_at_parent(
        &self,
        target: &ReviewTarget,
        path: &RepoRelPath,
    ) -> Result<String, VcsError> {
        // The working copy is diffed against `HEAD`, so that is its parent.
        let spec = match target.revision() {
            Some(revision) => format!("{}^1:{}", revision.0, path.0.display()),
            None => format!("HEAD:{}", path.0.display()),
        };

        self.run(&["show", &spec])
    }

    fn head(&self) -> Result<RevisionId, VcsError> {
        self.resolve("HEAD")
    }

    fn commit_of(&self, target: &ReviewTarget) -> Result<CommitId, VcsError> {
        match target.revision() {
            // A git `RevisionId` is already the commit SHA.
            Some(revision) => Ok(CommitId(revision.0.clone())),
            // The working copy has no commit of its own; record the one it sits
            // on, so an anchor still says what the change was based on.
            None => Ok(CommitId(self.head()?.0)),
        }
    }

    fn change_commits(&self, _target: &ReviewTarget) -> Result<ChangeCommits, VcsError> {
        // git commit SHAs are not preserved across amend/rebase, so there is no
        // stable change identity to follow.
        Ok(ChangeCommits::Unsupported)
    }
}
