# Positioning: margin next to tuicr

> Working document, written 2026-07-28 after
> [tuicr](https://github.com/agavra/tuicr) surfaced as a close neighbour. It
> records the thesis to defend, the gaps that currently falsify it, and the
> artifacts needed to make it legible. Not a spec — items graduate into issues
> and `CHANGELOG.md` entries as they ship.
>
> **Status (2026-07-28).** Tier 1 is done except §3.3 (git change tracking);
> Tier 2 and Tier 3 are untouched. Each shipped item is marked **Done** with the
> commit that closed it.

## 1. The neighbour

tuicr ("tweaker") is a Rust code-review TUI: continuous GitHub-style diff, full
vim model (visual mode, count prefixes, `:` commands, `/` search), review
tracking at file/hunk granularity, 23 themes, mouse, git/jj/hg, reviewing
uncommitted changes, commit ranges, GitHub PRs and GitLab MRs. It ends in an
*export*: `:submit` pushes a real inline review to GitHub or GitLab, `y` copies
structured markdown, `--stdout` pipes.

Its agent story is one-way. `tuicr review comments` reads the human's comments
as JSON, `tuicr review add` lets an agent write new comments, and the bundled
skill instructs the agent to poll every 30 seconds and to open tuicr in a
tmux/zellij split. `lifecycle_state` tracks submission to a forge, not whether
anything was addressed. There is no state anywhere in tuicr for *the agent
handled this*.

By the numbers: ~1.2k stars, 124 forks, 84 open issues, started January 2026,
sponsored, actively developed. margin is at 0.11.0 and solo. Breadth is not the
axis to compete on.

## 2. Thesis

> tuicr is a PR review client that exports. margin is a closed review loop over
> commits your agent will rewrite.

Every item below either makes that sentence true or makes it legible. Anything
that does neither is out of scope, however reasonable it looks next to tuicr's
feature list.

Three properties carry it, and none are things tuicr can add cheaply — they
follow from decisions already made here (append-only event log, in-repo store,
change-id anchoring):

1. **Closed loop.** The agent writes outcomes back through `margin status`,
   markers flip live in the open TUI, and the reviewer re-reviews the reply.
   tuicr's flow ends at "paste this markdown"; the human then reconciles what
   got fixed from memory.
2. **Annotations survive history rewriting.** The fix *amends or rebases the
   commit that was annotated*. margin follows it and reports `amended`,
   `divergent`, `abandoned`. tuicr pins comments to a session snapshot; the
   review target moving is not a modelled case.
3. **Auditable state.** Append-only events with compensating tombstones give
   undo, a per-annotation timeline, and safe concurrent agent writes while the
   TUI is open. Session JSON mutated in place gives none of that.

## 3. Tier 1 — gaps that must close

Without these the thesis is false, or true only with an asterisk.

### 3.1 Export replies and history in `list --json` — **Done**

`Resolved { reply }`, `Declined { reply }` and `Reopened { reason }` are logged
(`src/model/event.rs`) but `AnnotationView` (`src/export/mod.rs`) exposes
neither. The loop is therefore write-only: an agent can record "declined,
because X" and nothing can read it back. A second agent run starts amnesiac, and
external tooling sees a bare status enum.

Add `replies` (or a compact `history`: actor, kind, text, revision) to the JSON
projection. This is the differentiator itself being half-implemented; it ships
first.

*Shipped as a `history` array: one entry per recorded outcome, oldest first,
with actor, action, timestamp and the text said about it. Events that record no
outcome stay out, as does a bare `addressed_by` link the top-level field already
carries.*

### 3.2 Working-tree / uncommitted review — **Done**

Deferred in `PRD.md` §5, and now the wrong call. Agents leave uncommitted
changes by default, so requiring a commit first puts friction exactly where the
loop should start. tuicr has `-w`. Today "review your agent's work" means "first
commit your agent's work".

*Narrower than written: jj snapshots the working copy into `@`, which the jj
backend already listed, so this was a git-only gap. Shipped as a `ReviewTarget`
enum (`Revision` | `WorkingCopy`) threaded through the `Vcs` trait, anchors, and
the TUI, rather than a sentinel revision id. On git the working copy leads the
listing and is diffed against `HEAD`; it has no change identity, so it records
the commit it sits on. Anchors serialize unchanged, so existing logs still
load.*

### 3.3 Change tracking on git, not only jj — *open, next up*

`revision_state` is jj-only, so the headline claim holds for a minority of
users; on git, annotations silently orphan when the agent amends. Stable change
identity is not required — patch-id or content rematch across a rewrite covers
the common amend case. Ship whatever degrades gracefully, and document the
degradation.

### 3.4 A blocking/streaming read for the agent side — **Done**

tuicr's skill tells agents to poll every 30 seconds. margin should obsolete that
rather than match it: `margin list --watch` streaming new and changed
annotations turns "go review, I'll wait" into one command. The log is already
watched for the TUI's auto-reload, so the mechanism exists.

*As first written this was underdesigned: with no "review finished" signal, a
stream never ends and the agent still cannot tell when to start. Shipped instead
as an explicit hand-off — `H` in the TUI records a `reviewer_handed_off` event
per open annotation, and `margin list --watch` blocks until one lands, then
prints and exits.*

### 3.5 De-Claude the agent story — **Done**

`install-skill` writes only to `~/.claude/skills/`, which makes "agent-first"
read as "Claude-only". Add other targets (`AGENTS.md` emission, `--agent`), and
lead the docs with `MARGIN_AGENT_CMD` for the in-TUI launch.

*Shipped as `--print` (write the document to stdout, to redirect wherever an
agent reads) and `--dir` (any skills root). margin stays out of guessing each
agent's convention.*

## 4. Tier 2 — strengthen what exists into arguments

- **Turn the timeline into a thread.** Once §3.1 lands, `t` should read as
  annotation → agent reply → reopen reason, not as an event dump. tuicr has no
  equivalent artifact; it should look like one.
- **Re-review mode instead of reviewed-marks.** tuicr has manual `r`/`R`
  file/hunk checkmarks. Copying that is the wrong move — the agent-first answer
  to the same need is *what changed since I last reviewed*: jump to what the
  agent touched after the annotation was written. Only possible because margin
  knows about the fix.
- **File-level and review-level annotations.** Line and range only today.
  "Restructure this module" and "the whole approach is wrong" have no anchor,
  and those are exactly the notes agent-written code attracts.
- **Batch `margin status`.** One process per annotation is chatty for a `C` run
  over ten annotations. Accept several ids, or a JSON batch on stdin.
- **Surface agent failure in the log panel.** Non-zero exit and tool errors must
  be unmissable: the session runs `bypassPermissions` against the working tree,
  and trust in that path is a prerequisite for the whole pitch.
- **`/` search in the diff.** Table stakes, cheap, currently absent.
- **A config file** for base, theme, agent command — flags-only starts to hurt
  once §3.5 lands.

## 5. Tier 3 — positioning artifacts

### 5.1 Landing page (does not exist)

tuicr has tuicr.dev. margin has a README and no site, no Pages workflow, no
gh-pages branch. Add `docs/` plus a workflow beside `ci.yml`/`release.yml`.
Content in order:

1. The thesis in one line, then the loop as a diagram: annotate → agent fixes →
   status flips live → re-review the reply. Lead with the loop, not the TUI.
2. A demo *of the loop*, not a diff screenshot. The moment a marker flips while
   you keep navigating is the product, so this needs motion (asciinema/GIF); the
   `dump_screenshot` SVG renderer is the wrong medium for it.
3. The agent contract in about six lines of shell (`list --json`,
   `status … --reply`).
4. Install, then the comparison table.

### 5.2 Comparison table, in `README.md` and on the landing page

Use margin's axes. tuicr's own table is built around forge push and vim depth,
where margin loses by design.

| | margin | tuicr |
|---|:--:|:--:|
| Agent reports outcomes back into the review | ✅ | ❌ |
| Annotations survive amend/rebase of the reviewed commit | ✅ | ❌ |
| Agent launched from inside the review | ✅ | via tmux wrapper |
| Append-only history, undo, per-annotation timeline | ✅ | ❌ |
| Annotations stored in the repo | ✅ | app-data dir |
| Push an inline review to GitHub / GitLab | ❌ | ✅ |
| Review someone else's PR / MR | ❌ | ✅ |
| Vim model, mouse, theme gallery, hg | minimal | extensive |

The last three rows are load-bearing. Conceding them is what makes the top rows
believable, and it routes people who want a PR client to tuicr instead of having
them bounce off margin.

### 5.3 State the non-goals publicly

Forge submission, PR/MR review, mercurial, full vim modality, library API. They
live in `PRD.md` §2 today, where no visitor reads them.

## 6. Sequencing

§3.1 → §3.2 → §3.4 (contract complete, loop demoable) → §3.3 (widens the
audience) → §5.1/§5.2 (page and table then describe shipped behaviour rather
than intent) → Tier 2 continuously.

**Remaining:** §3.3, then Tier 2 and Tier 3. The loop is now complete enough to
demo end to end — annotate, hand off, agent replies, reply reads back — which is
what §5.1's demo needs to show.

Deliberately not doing: forge submission, mercurial, vim visual/count model,
theme gallery, clipboard export. Each costs real work and blurs §2.
