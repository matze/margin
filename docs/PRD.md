# PRD: `margin` — a local TUI for code annotations

> Historical v1 spec and decision record. The product has shipped; this is kept
> for the *rationale* (problem, non-goals, design decisions) that isn't
> recoverable from the code, not as a description of current behaviour. For how
> the code is laid out, see `CLAUDE.md`; for usage, see `README.md`.

## 1. Problem

Agentic development pushes humans into a reviewer role, but the review loop is
stuck in chat. Two bad options today:

1. Approve edits inline as they happen — precise, but the big picture is lost.
2. Review the accumulated diff afterwards — keeps the big picture, but pointing
   at specific lines while scrolling a `git`/`jj` diff is clunky and imprecise.

The workaround is to push throwaway PRs to GitHub purely to use its line-comment
UI, then have the agent fetch comments and act on them. That adds round-trip
latency, context-switching, external dependency, and token cost.

## 2. Goal

A local, "in-the-box" TUI that:

- Understands `git` and `jj` enough to have a notion of commits/revisions and
  their diffs.
- Lets a reviewer step through changes and attach annotations to a single line,
  a line range, or multiple disjoint locations.
- Emits those annotations in a form an agent can consume to do the follow-up
  work — without a GitHub round-trip.

### Non-goals (v1)

- Not a general code editor.
- Not a replacement for `git`/`jj` themselves (no committing, rebasing, etc.).
- Not a chat UI with the agent. `margin` produces review artifacts; the agent
  acts on them elsewhere.
- Not a multi-user / collaborative review server.

### Deferred to v2

- File-tree navigation in the diff view.
- Nerd Fonts glyphs (plain Unicode for now).
- `jj` backend, working-tree / uncommitted-change review.

## 3. Target user

Solo developer (or small team) using a coding agent (Claude Code, etc.) on a
local checkout, who wants to review and direct the agent without leaving the
terminal.

## 4. Core concepts

- **Commit/revision list** — the primary navigation unit. On launch `margin`
  shows a sidebar of commits/revisions; the reviewer selects one to review.
- **Per-commit diff** — when a commit/revision is selected, `margin` shows *that
  commit's own* diff (commit vs. its parent for git; revision vs. its parent for
  jj). No combined/squashed diff across commits, and no implicit working-tree
  diff is assumed.
- **Annotation** — a comment anchored to a location (file + line range +
  revision), with body text and an optional `type`.
- **Anchor** — the durable reference to where an annotation points, robust to
  later edits (see §6).
- **Export** — the structured output handed to the agent.

## 5. VCS model

Selecting a commit shows *that commit's own* diff (vs. its parent) — no combined
diff across commits, no implicit working-tree-vs-HEAD diff. The sidebar lists
commits unique vs. a base branch (`base..@`) — "the work under review."

- **Base resolution.** Detected default branch (main/master/trunk), overridable
  via `--base` and config.
- **No-base fallback.** When no base resolves (detached, fresh repo), fall back
  to the recent N commits with a visible notice.
- **Merge commits** are listed, flagged, and diffed against their first parent.
- **Working-tree changes** are out of scope in v1.

**[ASSUMPTION]** We shell out to the `git`/`jj` CLIs rather than linking
libraries (libgit2, etc.) for v1 — simpler, fewer build deps. Revisit if perf
hurts.

## 6. Annotation anchoring (the hard part)

Annotations must survive the agent editing the file underneath them. Approaches,
roughly increasing robustness and cost: line number + revision (breaks on any
shift); content hash of the anchored line(s) + nearby context (re-locate by
searching the context window); diff-based re-anchoring (map old→new line via the
diff).

**[ASSUMPTION]** v1 stores `{file, revision_id, old_start, old_end, captured text
of the range, N lines of leading/trailing context}`. On read, resolve by (1)
trying the recorded lines, (2) falling back to context search. Degrades
gracefully to "stale — needs manual relocation."

When an anchor can no longer be resolved, the annotation is kept, flagged
`orphaned`, and surfaced for manual re-anchor. No silent data loss.

## 7. Storage & event log

The store is an **append-only event log**, not mutable records: each line of
`.margin/annotations.ndjson` (repo root, gitignored) is an event, so the full
train of modifications is preserved and auditable. Current per-annotation state
(`status` ∈ `open | resolved | wont_do | orphaned`) is *derived* by folding
events per `annotation_id`, never stored directly. Annotation `type` ∈
`fix | question | suggestion | nit | praise` (no `severity`).

Event kinds: `annotation_created` (anchor + body + type), `annotation_edited`,
`annotation_deleted` / `annotation_restored` (tombstones — deletion is a
compensating event, not a removal), `agent_resolved` / `agent_wont_do`,
`agent_addressed_by` (links a note to the change that addressed it, making the
round-trip legible), `reviewer_reopened`. Every event carries
`{event_id, annotation_id, timestamp, actor: reviewer|agent}`.

Benefits: natural audit trail, safe concurrent appends from `margin` and the
agent (no read-modify-write races), and a per-annotation timeline view. No
compaction in v1 (logs are small).

Rejected alternatives: an outside-repo XDG dir (loses locality); git notes / jj
metadata (couples tightly to each VCS); storing `status` directly (loses the
audit trail and invites write races).

## 8. Agent integration

The NDJSON sidecar is the source of truth; the `margin` CLI is the agent's only
sanctioned interface to it.

- **Source of truth: `.margin/annotations.ndjson`.** Keeps the commit-centric
  model intact (annotate a historical revision's diff, the old side, deleted
  lines), avoids polluting the working tree, and preserves structure.
- **Read: `margin list --json`.** Emits the folded, re-anchored projection on
  stdout — per annotation: id, file, status, type, body, current resolved
  location, snippet. The agent never parses the raw event log or re-derives the
  fold semantics.
- **Write: `margin status <id> <state>` (resolve, wont-do, reopen).** Transitions
  are written back as events, folded on next read.

The CLI — not a generated file — is the contract: it is deterministic, versioned,
and folds the log behind a single boundary, so the agent stays decoupled from the
storage format. No MCP server in v1.

> Superseded after v1: the original design also shipped a Markdown artifact
> (`MARGIN_REVIEW.md`), a JSON file export, an inline `// MARGIN[<id>]:` marker
> mode, and an on-finish shell hook. All were dropped in favor of the CLI-only
> contract above — the agent reads live state via `list --json` rather than a
> file the reviewer has to regenerate.

## 9. Decisions log

| # | Topic | Decision |
|---|-------|----------|
| 1 | Default view | Commit/revision sidebar; per-commit diff; no assumption about what is diffed |
| 2 | Sidebar contents | Commits unique vs. a base branch |
| 3 | Base resolution | Detected default branch (main/master/trunk), `--base` override |
| 4 | No-base fallback | Recent N commits + visible notice |
| 5 | Merge commits | Listed, flagged, diffed vs. first parent |
| 6 | Working-tree changes | Out of scope in v1 |
| 7 | Orphaned anchors | Keep + flag `orphaned` + manual re-anchor |
| 8 | Taxonomy | `type` enum only; no `severity` |
| 9 | Storage | Single `.margin/annotations.ndjson`, append-only event log, source of truth |
| 10 | Agent interface | Sidecar source of truth; CLI-only contract (`list --json` read, `resolve` write); no file artifacts (markdown/inline/JSON-file dropped post-v1) |
| 11 | Change tracking | Append-only event log; `status` derived; deletion via tombstone; no compaction in v1 |
| 12 | `agent_addressed_by` | Agent emits when able; `margin` infers as fallback |
| 13 | Agent invocation | CLI `list --json` / `resolve`; no file artifact, no on-finish hook (dropped post-v1), no MCP |
| 14 | Write-back | Yes, as events |
| 15 | Stack | Rust + `ratatui` / `crossterm`; vim-style keys + arrow fallback |
| 16 | Highlighting | `syntect`, light/dark aware (both shipped), default dark when undetectable, lazy/viewport + size cap |
