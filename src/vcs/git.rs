use std::path::{Path, PathBuf};
use std::process::Command;

use super::parse::{parse_diff, parse_log_line, FIELD_SEP};
use super::{Base, CommitDiff, ListingSource, Revision, Revisions, Vcs, VcsError};
use crate::model::{RepoRelPath, RevisionId};

/// The well-known SHA of git's empty tree, used to diff a root commit (which has
/// no parent) against "nothing".
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Candidate default-branch names tried, in order, when detecting a base.
const DEFAULT_BRANCH_CANDIDATES: [&str; 3] = ["main", "master", "trunk"];

/// A `git` backend that shells out to the `git` CLI (PRD §6).
#[derive(Debug, Clone)]
pub struct Backend {
    root: PathBuf,
}

impl Backend {
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

impl Vcs for Backend {
    fn root(&self) -> &Path {
        &self.root
    }

    fn message(&self, revision: &RevisionId) -> Result<String, VcsError> {
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

    fn file_at_parent(
        &self,
        revision: &RevisionId,
        path: &RepoRelPath,
    ) -> Result<String, VcsError> {
        let spec = format!("{}^1:{}", revision.0, path.0.display());
        self.run(&["show", &spec])
    }

    fn head(&self) -> Result<RevisionId, VcsError> {
        self.resolve("HEAD")
    }
}
