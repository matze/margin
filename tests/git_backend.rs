//! Fixture-repo tests for the git backend: build a real repository in a tempdir
//! and exercise revision listing, per-commit diffing, and reading file content
//! at a revision (PRD §6).

use std::path::Path;
use std::process::Command;

use margin::model::{RepoRelPath, RevisionId};
use margin::vcs::{Backend, Base, ChangeKind, DiffLineKind, Kind, ListingSource, Vcs};

/// Discover a forced-git backend for `path`.
fn git_backend(path: &Path) -> Backend {
    Backend::discover(path, Some(Kind::Git)).unwrap()
}

/// Run a git command in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");

    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Commit `files` (path, contents) with `message`, returning the commit SHA.
fn commit(dir: &Path, message: &str, files: &[(&str, &str)]) -> RevisionId {
    for (path, contents) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    RevisionId(git(dir, &["rev-parse", "HEAD"]))
}

/// A fresh repo with a `main` base commit and deterministic identity/config.
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);

    dir
}

#[test]
fn revisions_lists_commits_unique_to_base() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("README.md", "hello\n")]);
    git(path, &["checkout", "-q", "-b", "feature"]);
    let first = commit(path, "Add limiter", &[("src/limiter.rs", "fn a() {}\n")]);
    let second = commit(path, "Wire config", &[("src/config.rs", "fn b() {}\n")]);

    let backend = git_backend(path);
    let revisions = backend.revisions(&Base::Branch("main".into())).unwrap();

    assert!(matches!(revisions.source, ListingSource::Range { .. }));
    let ids: Vec<_> = revisions.revisions.iter().map(|r| r.id.clone()).collect();
    // git log is newest-first.
    assert_eq!(ids, vec![second, first]);
    assert_eq!(revisions.revisions[0].summary, "Wire config");
    assert!(!revisions.revisions[0].is_merge);
}

#[test]
fn auto_base_falls_back_to_recent_when_unresolvable() {
    let repo = init_repo();
    let path = repo.path();

    // Rename the only branch away from any default-branch candidate.
    git(path, &["branch", "-m", "main", "wip-branch"]);
    commit(path, "only", &[("a.txt", "1\n")]);

    let backend = git_backend(path);
    let revisions = backend.revisions(&Base::Auto { fallback: 10 }).unwrap();

    assert_eq!(revisions.source, ListingSource::RecentFallback);
    assert_eq!(revisions.revisions.len(), 1);
}

#[test]
fn diff_reports_added_modified_and_line_numbers() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("src/lib.rs", "one\ntwo\nthree\n")]);
    let rev = commit(
        path,
        "edit",
        &[("src/lib.rs", "one\nTWO\nthree\nfour\n"), ("new.rs", "x\n")],
    );

    let backend = git_backend(path);
    let diff = backend.diff(&rev).unwrap();

    assert_eq!(diff.files.len(), 2);

    let lib = diff
        .files
        .iter()
        .find(|f| f.display_path().unwrap().0.ends_with("lib.rs"))
        .unwrap();
    assert_eq!(lib.change, ChangeKind::Modified);

    let added: Vec<_> = lib
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == DiffLineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(added, vec!["TWO", "four"]);

    let two = lib
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .find(|l| l.content == "TWO")
        .unwrap();
    assert_eq!(two.new_no.unwrap().get(), 2);
    assert!(two.old_no.is_none());

    let new_file = diff
        .files
        .iter()
        .find(|f| f.display_path().unwrap().0.ends_with("new.rs"))
        .unwrap();
    assert_eq!(new_file.change, ChangeKind::Added);
}

#[test]
fn diff_of_root_commit_uses_empty_tree() {
    let repo = init_repo();
    let path = repo.path();
    let rev = commit(path, "root", &[("first.rs", "a\nb\n")]);

    let backend = git_backend(path);
    let diff = backend.diff(&rev).unwrap();

    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].change, ChangeKind::Added);
}

#[test]
fn merge_commit_is_flagged_and_diffable() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("a.txt", "a\n")]);
    git(path, &["checkout", "-q", "-b", "feature"]);
    commit(path, "feature change", &[("c.txt", "c\n")]);
    git(path, &["checkout", "-q", "-b", "side"]);
    commit(path, "side change", &[("b.txt", "b\n")]);
    git(path, &["checkout", "-q", "feature"]);
    git(
        path,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );

    let merge = RevisionId(git(path, &["rev-parse", "HEAD"]));
    let backend = git_backend(path);

    // The work under review is unique to main..feature, including the merge.
    let listed = backend.revisions(&Base::Branch("main".into())).unwrap();
    let merge_rev = listed.revisions.iter().find(|r| r.id == merge).unwrap();
    assert!(merge_rev.is_merge);

    // Diffs against the first parent without error.
    backend.diff(&merge).unwrap();
}

#[test]
fn file_at_reads_content_at_revision() {
    let repo = init_repo();
    let path = repo.path();

    let first = commit(path, "v1", &[("f.txt", "version one\n")]);
    commit(path, "v2", &[("f.txt", "version two\n")]);

    let backend = git_backend(path);
    let content = backend
        .file_at(&first, &RepoRelPath("f.txt".into()))
        .unwrap();

    assert_eq!(content, "version one\n");
}
