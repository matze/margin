//! End-to-end tests driving the built `margin` binary against a fixture repo:
//! seed an annotation via the library, then exercise list → list --json →
//! resolve → list and confirm the event-fold round-trip (PRD §10, §12).

use std::path::Path;
use std::process::Command;

use margin::anchor::{capture, CONTEXT_LINES};
use margin::model::{
    Actor, AnnotationId, AnnotationType, Event, EventKind, LineNumber, RepoRelPath, RevisionId,
    Side,
};
use margin::store::Store;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Run the built binary in `dir`; returns (success, stdout).
fn margin(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_margin"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn margin");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn list_json_resolve_round_trip() {
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path();

    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "T"]);

    let source = "pub struct Limiter {\n    max: u32,\n    window: u32,\n}\n";
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(path.join("src/limiter.rs"), source).unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "add limiter"]);
    let rev_sha = git(path, &["rev-parse", "HEAD"]);

    // Seed an annotation on `window: u32,` (line 3) via the library.
    let store = Store::open(path);
    let id = AnnotationId::new();
    let anchor = capture(
        RepoRelPath("src/limiter.rs".into()),
        RevisionId(rev_sha.clone()),
        Side::New,
        source,
        LineNumber::new(3).unwrap(),
        LineNumber::new(3).unwrap(),
        CONTEXT_LINES,
    )
    .unwrap();
    store
        .append(&Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationCreated {
                anchor,
                body: "window should be a Duration".into(),
                annotation_type: Some(AnnotationType::Fix),
            },
        ))
        .unwrap();

    let short = &id.0.simple().to_string()[..8];

    // list --open shows the open annotation at its current line.
    let (ok, out) = margin(path, &["list", "--open"]);
    assert!(ok);
    assert!(out.contains(short), "list output: {out}");
    assert!(out.contains("src/limiter.rs:3"), "list output: {out}");
    assert!(out.contains("[open]"), "list output: {out}");

    // list --json emits the folded projection with body, location, and snippet.
    let (ok, json) = margin(path, &["list", "--json"]);
    assert!(ok);
    assert!(json.contains("window should be a Duration"), "json: {json}");
    assert!(json.contains("window: u32,"), "json: {json}");
    assert!(json.contains("\"location\""), "json: {json}");

    // `status resolved` flips the derived status.
    let (ok, _) = margin(path, &["status", short, "resolved"]);
    assert!(ok);

    let (_, open_after) = margin(path, &["list", "--open"]);
    assert!(!open_after.contains(short), "still open: {open_after}");

    let (_, all_after) = margin(path, &["list"]);
    assert!(all_after.contains("[resolved]"), "list: {all_after}");

    // Resolving inferred the addressing revision (HEAD) and recorded it.
    let (_, json_after) = margin(path, &["list", "--json"]);
    assert!(json_after.contains("\"addressed_by\""), "json: {json_after}");
    assert!(json_after.contains(&rev_sha), "json: {json_after}");

    // The store kept created + resolved + inferred-addressed_by (append-only).
    assert_eq!(store.load().unwrap().len(), 3);
}

#[test]
fn status_wont_do_and_reopen() {
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path();

    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "T"]);
    std::fs::write(path.join("f.rs"), "fn keep() {}\n").unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "init"]);
    let rev = RevisionId(git(path, &["rev-parse", "HEAD"]));

    let store = Store::open(path);
    let id = AnnotationId::new();
    let anchor = capture(
        RepoRelPath("f.rs".into()),
        rev,
        Side::New,
        "fn keep() {}\n",
        LineNumber::new(1).unwrap(),
        LineNumber::new(1).unwrap(),
        CONTEXT_LINES,
    )
    .unwrap();
    store
        .append(&Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationCreated {
                anchor,
                body: "rename keep".into(),
                annotation_type: None,
            },
        ))
        .unwrap();

    let short = &id.0.simple().to_string()[..8];

    // wont-do declines the annotation.
    let (ok, _) = margin(path, &["status", short, "wont-do", "--reply", "out of scope"]);
    assert!(ok);
    let (_, after_decline) = margin(path, &["list"]);
    assert!(after_decline.contains("[wont_do]"), "list: {after_decline}");

    // open reopens it for re-review.
    let (ok, _) = margin(path, &["status", short, "open"]);
    assert!(ok);
    let (_, after_reopen) = margin(path, &["list", "--open"]);
    assert!(after_reopen.contains(short), "reopened: {after_reopen}");
}

#[test]
fn orphaned_annotation_is_flagged() {
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path();

    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "T"]);
    std::fs::write(path.join("f.rs"), "fn keep() {}\n").unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "init"]);

    // Anchor to text that does not exist in the working tree.
    let store = Store::open(path);
    let id = AnnotationId::new();
    let anchor = capture(
        RepoRelPath("f.rs".into()),
        RevisionId("deadbeef".into()),
        Side::New,
        "fn vanished() {}\n",
        LineNumber::new(1).unwrap(),
        LineNumber::new(1).unwrap(),
        CONTEXT_LINES,
    )
    .unwrap();
    store
        .append(&Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationCreated {
                anchor,
                body: "gone".into(),
                annotation_type: None,
            },
        ))
        .unwrap();

    let (ok, out) = margin(path, &["list"]);
    assert!(ok);
    assert!(out.contains("[orphaned]"), "list: {out}");
}

#[test]
fn resolved_then_vanished_is_flagged_orphaned() {
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path();

    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "T"]);
    let source = "fn target() {}\n";
    std::fs::write(path.join("f.rs"), source).unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "init"]);
    let rev = RevisionId(git(path, &["rev-parse", "HEAD"]));

    let store = Store::open(path);
    let id = AnnotationId::new();
    let anchor = capture(
        RepoRelPath("f.rs".into()),
        rev,
        Side::New,
        source,
        LineNumber::new(1).unwrap(),
        LineNumber::new(1).unwrap(),
        CONTEXT_LINES,
    )
    .unwrap();
    store
        .append(&Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationCreated {
                anchor,
                body: "rename".into(),
                annotation_type: None,
            },
        ))
        .unwrap();

    let short = &id.0.simple().to_string()[..8];
    let (ok, _) = margin(path, &["status", short, "resolved"]);
    assert!(ok);

    // The resolved line vanishes from the working tree.
    std::fs::write(path.join("f.rs"), "fn other() {}\n").unwrap();

    // Status stays resolved, but the gone anchor is surfaced, not silently null.
    let (_, json) = margin(path, &["list", "--json"]);
    assert!(json.contains("\"status\": \"resolved\""), "json: {json}");
    assert!(json.contains("\"orphaned\": true"), "json: {json}");
    assert!(json.contains("\"location\": null"), "json: {json}");

    let (_, line) = margin(path, &["list"]);
    assert!(line.contains("(orphaned)"), "list: {line}");
    assert!(line.contains("[resolved]"), "list: {line}");
}
