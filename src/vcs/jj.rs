use std::path::{Path, PathBuf};
use std::process::Command;

use super::parse::{parse_diff, parse_log_line, FIELD_SEP};
use super::{Base, CommitDiff, ListingSource, Revision, Revisions, Vcs, VcsError};
use crate::model::{RepoRelPath, RevisionId};

/// The revset naming the working-copy commit, always included in the listing so
/// an undescribed/empty working revision is still reviewable.
const WORKING_COPY: &str = "@";

/// The built-in revset for the repository's main line, used as the default base.
const TRUNK: &str = "trunk()";

/// A `jj` backend that shells out to the `jj` CLI (PRD §6).
#[derive(Debug, Clone)]
pub struct Backend {
    root: PathBuf,
}

impl Backend {
    /// Discover the repository containing `start` via `jj root`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, VcsError> {
        let output = Command::new("jj")
            .current_dir(start.as_ref())
            .args(["root"])
            .output()
            .map_err(|source| VcsError::Spawn { tool: "jj", source })?;

        if !output.status.success() {
            return Err(VcsError::NotARepo { tool: "jj" });
        }

        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();

        Ok(Self { root: PathBuf::from(root) })
    }

    /// Run `jj` with `args`, returning stdout on success.
    fn run(&self, args: &[&str]) -> Result<String, VcsError> {
        let output = Command::new("jj")
            .current_dir(&self.root)
            .args(args)
            .output()
            .map_err(|source| VcsError::Spawn { tool: "jj", source })?;

        if !output.status.success() {
            return Err(VcsError::Command {
                tool: "jj",
                args: args.iter().map(|a| a.to_string()).collect(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Resolve a single-commit revset to its change id.
    fn resolve(&self, revset: &str) -> Result<RevisionId, VcsError> {
        let out = self.run(&["log", "-r", revset, "--no-graph", "-T", "change_id"])?;

        match out.lines().next().map(str::trim) {
            Some(id) if !id.is_empty() => Ok(RevisionId(id.to_string())),
            _ => Err(VcsError::Parse {
                what: "revset",
                detail: format!("{revset} resolved to no commit"),
            }),
        }
    }

    /// List commits for `revset` using the shared five-field log layout.
    fn log(&self, revset: &str) -> Result<Vec<Revision>, VcsError> {
        let template = log_template();

        self.run(&["log", "-r", revset, "--no-graph", "-T", &template])?
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

    fn revisions(&self, base: &Base) -> Result<Revisions, VcsError> {
        match base {
            Base::Branch(name) => {
                let base_id = self.resolve(name)?;
                Ok(Revisions {
                    revisions: self.log(&format!("{name}..{WORKING_COPY}"))?,
                    source: ListingSource::Range { base: base_id },
                })
            }
            // `trunk()` resolves to the root commit when no main line exists;
            // that is no real base, so fall back to recent commits like git.
            Base::Auto { fallback } => match self.resolve(TRUNK) {
                Ok(base_id) if !is_root(&base_id) => Ok(Revisions {
                    revisions: self.log(&format!("{TRUNK}..{WORKING_COPY}"))?,
                    source: ListingSource::Range { base: base_id },
                }),
                _ => Ok(Revisions {
                    revisions: self.log(&format!("ancestors({WORKING_COPY}, {fallback})"))?,
                    source: ListingSource::RecentFallback,
                }),
            },
        }
    }

    fn diff(&self, revision: &RevisionId) -> Result<CommitDiff, VcsError> {
        let raw = self.run(&["diff", "-r", &revision.0, "--git"])?;

        Ok(CommitDiff {
            revision: revision.clone(),
            files: parse_diff(&raw)?,
        })
    }

    fn file_at(&self, revision: &RevisionId, path: &RepoRelPath) -> Result<String, VcsError> {
        self.run(&["file", "show", "-r", &revision.0, "--", &path.0.to_string_lossy()])
    }

    fn file_at_parent(
        &self,
        revision: &RevisionId,
        path: &RepoRelPath,
    ) -> Result<String, VcsError> {
        let parent = format!("{}-", revision.0);
        self.run(&["file", "show", "-r", &parent, "--", &path.0.to_string_lossy()])
    }

    fn head(&self) -> Result<RevisionId, VcsError> {
        self.resolve(WORKING_COPY)
    }

    fn message(&self, revision: &RevisionId) -> Result<String, VcsError> {
        Ok(self
            .run(&["log", "-r", &revision.0, "--no-graph", "-T", "description"])?
            .trim_end()
            .to_string())
    }
}

/// jj's virtual root commit has an all-`z` change id; it is never a real base.
fn is_root(id: &RevisionId) -> bool {
    !id.0.is_empty() && id.0.chars().all(|c| c == 'z')
}

/// The `jj log` template emitting the same five `FIELD_SEP`-delimited fields the
/// shared [`parse_log_line`] consumes: change id, ISO-8601 commit timestamp,
/// author, space-joined parent change ids, and the description's first line.
fn log_template() -> String {
    let sep = FIELD_SEP;

    format!(
        "change_id ++ \"{sep}\" \
         ++ committer.timestamp().format(\"%Y-%m-%dT%H:%M:%S%:z\") ++ \"{sep}\" \
         ++ author.name() ++ \"{sep}\" \
         ++ parents.map(|c| c.change_id()).join(\" \") ++ \"{sep}\" \
         ++ description.first_line() ++ \"\\n\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// Run `jj` in `dir`, asserting success; stderr is ignored (author warnings).
    fn jj(dir: &Path, args: &[&str]) {
        let ok = Command::new("jj")
            .current_dir(dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "jj {args:?} failed");
    }

    /// True when a `jj` CLI is available; tests no-op otherwise (e.g. lean CI).
    fn jj_available() -> bool {
        Command::new("jj").arg("--version").output().is_ok()
    }

    /// A repo whose working copy `@` is undescribed but has real edits — the
    /// case that rendered nothing under the git backend.
    fn fixture() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();

        jj(path, &["git", "init", "."]);
        jj(path, &["config", "set", "--repo", "user.name", "T"]);
        jj(path, &["config", "set", "--repo", "user.email", "t@example.com"]);

        std::fs::write(path.join("file.txt"), "v1\n").unwrap();
        jj(path, &["describe", "-m", "first change"]);
        jj(path, &["new"]);
        std::fs::write(path.join("file.txt"), "v1\nworking edit\n").unwrap();

        repo
    }

    #[test]
    fn lists_and_diffs_undescribed_working_copy() {
        if !jj_available() {
            return;
        }

        let repo = fixture();
        let backend = Backend::discover(repo.path()).unwrap();

        let listing = backend.revisions(&Base::Auto { fallback: 10 }).unwrap();
        let working = listing
            .revisions
            .first()
            .expect("working copy @ should be listed");
        assert!(working.summary.is_empty(), "@ is undescribed");

        let diff = backend.diff(&working.id).unwrap();
        assert!(!diff.files.is_empty(), "undescribed @ still has a diff");
    }
}
