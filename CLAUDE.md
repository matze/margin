# margin

A local TUI for code-review annotations over git/jj. Reviewers annotate a
commit's diff; a coding agent consumes the annotations through the `margin` CLI
(`list --json` to read, `status` to write back).

## Layout

- `src/lib.rs` — terminal-free reusable core, exercised entirely by unit/fixture
  tests. `src/main.rs` is a thin CLI/TUI wrapper on top.
- `model/` — events, fold logic, anchors. The store is an append-only NDJSON
  event log (`.margin/annotations.ndjson`); current state is derived by folding
  events (`model/fold.rs`), never mutated in place. Deletion/undo are
  compensating tombstone events, not removals.
- `vcs/` — `git`/`jj` backend abstraction (shells out to the CLIs).
- `tui/` — `app.rs` state + dispatch, `ui.rs` rendering, `keymap.rs` key→action,
  `theme.rs` palette. Built on ratatui; rendered headlessly in tests via
  `TestBackend`.
- `anchor/`, `review/`, `export/` — re-anchoring, resolution, the
  `list --json` projection (`export/render_json`).

## Conventions

- Append-only: never edit or drop a logged event. Express changes as new events.
- The CLI is the agent contract. The agent reads/writes via `margin` (folded,
  versioned); the NDJSON store is internal — don't expose it as an interface.
- Keep the core terminal-free so it stays testable without a TTY.
- Palette is ANSI-first (default fg/bg inherit the terminal); per-mode RGB is
  reserved for diff/selection/cursor/annotation backgrounds only.
- `README.md` documents the CLI flags/subcommands (`cli.rs`) and TUI keys
  (`keymap.rs`). It drifts easily — when you change either, update the README's
  Usage/Navigation/Agent-handoff sections in the same change.
- Every user-facing change (feature, fix, behavior/UI change) gets a
  `CHANGELOG.md` entry under `## [Unreleased]` in the same commit, using the
  Keep a Changelog sections (`Added`/`Changed`/`Fixed`/...).

## Verify

`cargo fmt`, `cargo test` and `cargo clippy --all-targets` should be clean
before finishing. The `#[ignore]`d `dump_preview` test renders the TUI for
eyeballing.

`docs/PRD.md` holds the original v1 spec and decision log.
