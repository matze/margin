//! Fixture-repo tests for the git backend: build a real repository in a tempdir
//! and exercise revision listing, per-commit diffing, and reading file content
//! at a revision (PRD §6).

use std::path::Path;
use std::process::Command;

use margin::model::{CommitId, RepoRelPath, ReviewTarget, RevisionId};
use margin::vcs::{
    Base, ChangeCommits, ChangeKind, DiffLineKind, Kind, ListingSource, Vcs, discover,
};

/// Discover a forced-git backend for `path`.
fn git_backend(path: &Path) -> Box<dyn Vcs> {
    discover(path, Some(Kind::Git)).unwrap()
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

/// Commit `files` (path, contents) with `message`, returning the commit SHA as
/// a review target.
fn commit(dir: &Path, message: &str, files: &[(&str, &str)]) -> ReviewTarget {
    write_files(dir, files);

    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    ReviewTarget::Revision(RevisionId(git(dir, &["rev-parse", "HEAD"])))
}

/// Write `files` (path, contents) into the working tree without committing.
fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
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
    let ids: Vec<_> = revisions
        .revisions
        .iter()
        .map(|r| r.target.clone())
        .collect();
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

    let merge = ReviewTarget::Revision(RevisionId(git(path, &["rev-parse", "HEAD"])));
    let backend = git_backend(path);

    // The work under review is unique to main..feature, including the merge.
    let listed = backend.revisions(&Base::Branch("main".into())).unwrap();
    let merge_rev = listed.revisions.iter().find(|r| r.target == merge).unwrap();
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

#[test]
fn commit_of_is_the_sha_and_an_untouched_commit_is_unchanged() {
    let repo = init_repo();
    let path = repo.path();
    let rev = commit(path, "only", &[("f.txt", "x\n")]);

    let backend = git_backend(path);

    // git has no change identity distinct from the commit, so commit_of echoes
    // the SHA and an unrewritten commit resolves to itself.
    assert_eq!(
        backend.commit_of(&rev).unwrap(),
        CommitId(rev.as_str().to_string())
    );
    assert_eq!(
        backend.change_commits(&rev).unwrap(),
        ChangeCommits::One(CommitId(rev.as_str().to_string()))
    );
}

#[test]
fn the_working_copy_has_no_change_to_track() {
    let repo = init_repo();
    let path = repo.path();
    commit(path, "base", &[("f.txt", "x\n")]);
    write_files(path, &[("f.txt", "edited\n")]);

    let backend = git_backend(path);

    assert_eq!(
        backend.change_commits(&ReviewTarget::WorkingCopy).unwrap(),
        ChangeCommits::Unsupported
    );
}

#[test]
fn an_amended_commit_is_followed_to_its_rewrite() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("README.md", "hello\n")]);
    let reviewed = commit(path, "Add limiter", &[("src/limiter.rs", "fn a() {}\n")]);

    write_files(path, &[("src/limiter.rs", "fn a() { todo!() }\n")]);
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "--amend", "--no-edit"]);
    let amended = CommitId(git(path, &["rev-parse", "HEAD"]));

    let backend = git_backend(path);

    assert_eq!(
        backend.change_commits(&reviewed).unwrap(),
        ChangeCommits::One(amended)
    );
}

#[test]
fn a_rebased_commit_is_followed_to_its_rewrite() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("README.md", "hello\n")]);
    git(path, &["checkout", "-q", "-b", "feature"]);
    let reviewed = commit(path, "Add limiter", &[("src/limiter.rs", "fn a() {}\n")]);

    // Move the branch point: the same work, a new SHA on a new parent.
    git(path, &["checkout", "-q", "main"]);
    commit(path, "Unrelated", &[("docs/notes.md", "notes\n")]);
    git(path, &["checkout", "-q", "feature"]);
    git(path, &["rebase", "-q", "main"]);
    let rebased = CommitId(git(path, &["rev-parse", "HEAD"]));

    let backend = git_backend(path);

    assert_ne!(rebased.0, reviewed.as_str());
    assert_eq!(
        backend.change_commits(&reviewed).unwrap(),
        ChangeCommits::One(rebased)
    );
}

#[test]
fn a_dropped_commit_reads_as_abandoned() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("README.md", "hello\n")]);
    let reviewed = commit(path, "Add limiter", &[("src/limiter.rs", "fn a() {}\n")]);
    git(path, &["reset", "-q", "--hard", "HEAD~1"]);

    let backend = git_backend(path);

    assert_eq!(
        backend.change_commits(&reviewed).unwrap(),
        ChangeCommits::None
    );
}

#[test]
fn two_copies_of_a_rewritten_commit_read_as_divergent() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("README.md", "hello\n")]);
    commit(path, "Unrelated", &[("docs/notes.md", "notes\n")]);
    git(path, &["checkout", "-q", "-b", "original"]);
    let reviewed = commit(path, "Add limiter", &[("src/limiter.rs", "fn a() {}\n")]);

    // Land the same work twice, onto parents other than the one it was reviewed
    // on so both copies are new commits, then drop the branch it came from.
    git(path, &["checkout", "-q", "-b", "earlier", "main~1"]);
    git(path, &["cherry-pick", reviewed.as_str()]);

    git(path, &["checkout", "-q", "main"]);
    commit(path, "Later", &[("docs/later.md", "later\n")]);
    git(path, &["cherry-pick", reviewed.as_str()]);

    git(path, &["branch", "-D", "original"]);

    let backend = git_backend(path);

    assert!(
        matches!(
            backend.change_commits(&reviewed).unwrap(),
            ChangeCommits::Many(commits) if commits.len() == 2
        ),
        "the reviewed commit lives on in two rewrites"
    );
}

#[test]
fn uncommitted_changes_are_listed_and_diffed_as_the_working_copy() {
    let repo = init_repo();
    let path = repo.path();

    commit(path, "base", &[("src/lib.rs", "one\ntwo\n")]);

    let backend = git_backend(path);

    // A clean tree has nothing uncommitted to review.
    let clean = backend.revisions(&Base::Auto { fallback: 10 }).unwrap();
    assert!(
        !clean
            .revisions
            .iter()
            .any(|r| r.target == ReviewTarget::WorkingCopy)
    );

    write_files(path, &[("src/lib.rs", "one\nTWO\n")]);

    // Uncommitted work leads the listing, ahead of the commits behind it.
    let dirty = backend.revisions(&Base::Auto { fallback: 10 }).unwrap();
    assert_eq!(dirty.revisions[0].target, ReviewTarget::WorkingCopy);

    let diff = backend.diff(&ReviewTarget::WorkingCopy).unwrap();
    let lib = diff
        .files
        .iter()
        .find(|f| f.display_path().unwrap().0.ends_with("lib.rs"))
        .unwrap();
    assert_eq!(lib.change, ChangeKind::Modified);
    assert!(
        lib.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == DiffLineKind::Added && l.content.contains("TWO"))
    );
}

#[test]
fn the_working_copy_anchors_against_the_tree_and_head() {
    let repo = init_repo();
    let path = repo.path();

    let base = commit(path, "base", &[("f.txt", "old\n")]);
    write_files(path, &[("f.txt", "new\n")]);

    let backend = git_backend(path);
    let file = RepoRelPath("f.txt".into());

    // The working copy's own version is the file on disk; its parent is HEAD,
    // which is what an old-side (deleted-line) anchor resolves against.
    assert_eq!(
        backend.file_at(&ReviewTarget::WorkingCopy, &file).unwrap(),
        "new\n"
    );
    assert_eq!(
        backend
            .file_at_parent(&ReviewTarget::WorkingCopy, &file)
            .unwrap(),
        "old\n"
    );

    // It has no commit of its own, so an anchor records the one it sits on.
    assert_eq!(
        backend.commit_of(&ReviewTarget::WorkingCopy).unwrap(),
        CommitId(base.as_str().to_string())
    );
}
