use std::path::{Path, PathBuf};

use super::parse::{FIELD_SEP, parse_diff, parse_log_line};
use super::{Base, ChangeCommits, CommitDiff, ListingSource, Revision, Revisions, Vcs, VcsError};
use crate::model::{CommitId, RepoRelPath, RevisionId};

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
        let root = super::discover_root("jj", start.as_ref(), &["root"])?;

        Ok(Self { root })
    }

    /// Run `jj` with `args`, returning stdout on success.
    fn run(&self, args: &[&str]) -> Result<String, VcsError> {
        super::run_tool("jj", &self.root, args)
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
        self.run(&[
            "file",
            "show",
            "-r",
            &revision.0,
            "--",
            &path.0.to_string_lossy(),
        ])
    }

    fn file_at_parent(
        &self,
        revision: &RevisionId,
        path: &RepoRelPath,
    ) -> Result<String, VcsError> {
        let parent = format!("{}-", revision.0);
        self.run(&[
            "file",
            "show",
            "-r",
            &parent,
            "--",
            &path.0.to_string_lossy(),
        ])
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

    fn commit_of(&self, revision: &RevisionId) -> Result<CommitId, VcsError> {
        match self.change_commits(revision)? {
            ChangeCommits::One(commit) => Ok(commit),
            // At capture time the change is the one under the cursor; absence or
            // divergence here means the revset stopped resolving to it.
            _ => Err(VcsError::Parse {
                what: "change_id",
                detail: format!("{} resolved to no single commit", revision.0),
            }),
        }
    }

    fn change_commits(&self, revision: &RevisionId) -> Result<ChangeCommits, VcsError> {
        // `change_id(..)` matches every commit carrying the change id: an empty
        // set for an abandoned change, several for a divergent one. A bare
        // change-id symbol instead errors on divergence, so it can't report it.
        let revset = format!("change_id(\"{}\")", revision.0);
        let commits: Vec<CommitId> = self
            .run(&[
                "log",
                "-r",
                &revset,
                "--no-graph",
                "-T",
                "commit_id ++ \"\\n\"",
            ])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(|id| CommitId(id.to_string()))
            .collect();

        Ok(match commits.len() {
            0 => ChangeCommits::None,
            1 => ChangeCommits::One(commits.into_iter().next().expect("len checked")),
            _ => ChangeCommits::Many(commits),
        })
    }
}

/// jj's virtual root commit has an all-`z` change id; it is never a real base.
fn is_root(id: &RevisionId) -> bool {
    !id.0.is_empty() && id.0.chars().all(|c| c == 'z')
}

/// The `jj log` template emitting the `FIELD_SEP`-delimited fields the shared
/// [`parse_log_line`] consumes: change id, ISO-8601 commit timestamp, author,
/// space-joined parent change ids, the description's first line, and the change
/// id's shortest unique prefix.
fn log_template() -> String {
    let sep = FIELD_SEP;

    format!(
        "change_id ++ \"{sep}\" \
         ++ committer.timestamp().format(\"%Y-%m-%dT%H:%M:%S%:z\") ++ \"{sep}\" \
         ++ author.name() ++ \"{sep}\" \
         ++ parents.map(|c| c.change_id()).join(\" \") ++ \"{sep}\" \
         ++ description.first_line() ++ \"{sep}\" \
         ++ change_id.shortest().prefix() ++ \"\\n\""
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
        jj(
            path,
            &["config", "set", "--repo", "user.email", "t@example.com"],
        );

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
        assert!(
            working.unique_prefix_len.is_some_and(|len| len > 0),
            "jj revisions carry a shortest unique prefix"
        );

        let diff = backend.diff(&working.id).unwrap();
        assert!(!diff.files.is_empty(), "undescribed @ still has a diff");
    }

    #[test]
    fn change_commits_resolves_to_the_single_current_commit() {
        if !jj_available() {
            return;
        }

        let repo = fixture();
        let backend = Backend::discover(repo.path()).unwrap();
        let head = backend.head().unwrap();

        match backend.change_commits(&head).unwrap() {
            ChangeCommits::One(commit) => {
                assert_eq!(backend.commit_of(&head).unwrap(), commit)
            }
            other => panic!("expected one commit, got {other:?}"),
        }
    }

    #[test]
    fn amending_a_change_keeps_its_id_but_moves_its_commit() {
        if !jj_available() {
            return;
        }

        let repo = fixture();
        let path = repo.path();
        let backend = Backend::discover(path).unwrap();

        // `head` is the change id (stable); capture the commit it points at now.
        let head = backend.head().unwrap();
        let before = backend.commit_of(&head).unwrap();

        // Editing the working copy amends @ in place: same change id, new commit
        // (the next jj command snapshots the change).
        std::fs::write(path.join("file.txt"), "v1\nworking edit\nmore\n").unwrap();

        match backend.change_commits(&head).unwrap() {
            ChangeCommits::One(after) => assert_ne!(
                after, before,
                "amend should move the commit under a stable change id"
            ),
            other => panic!("expected one commit, got {other:?}"),
        }
    }

    #[test]
    fn abandoned_change_resolves_to_no_commit() {
        if !jj_available() {
            return;
        }

        let repo = fixture();
        let path = repo.path();
        let backend = Backend::discover(path).unwrap();

        let doomed = backend.head().unwrap();
        jj(path, &["abandon", &doomed.0]);

        assert_eq!(
            backend.change_commits(&doomed).unwrap(),
            ChangeCommits::None
        );
    }

    /// Divergence (one change id, several commits) is constructed via concurrent
    /// operations, exercising the [`ChangeCommits::Many`] path.
    #[test]
    fn divergent_change_resolves_to_many_commits() {
        if !jj_available() {
            return;
        }

        let repo = fixture();
        let path = repo.path();
        let backend = Backend::discover(path).unwrap();

        // Detach @ so the target change is a stable, non-working commit.
        jj(path, &["describe", "-m", "target"]);
        let target = backend.head().unwrap();
        jj(path, &["new"]);

        // Two concurrent rewrites of the same change diverge it: the second runs
        // against the operation before the first, so neither supersedes the other.
        jj(path, &["describe", &target.0, "-m", "a"]);
        jj(
            path,
            &["describe", &target.0, "-m", "b", "--at-operation", "@-"],
        );

        assert!(
            matches!(
                backend.change_commits(&target).unwrap(),
                ChangeCommits::Many(_)
            ),
            "concurrent rewrites should diverge the change"
        );
    }
}
