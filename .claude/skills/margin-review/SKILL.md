---
name: margin-review
description: >-
  Address code-review annotations left in the margin TUI. Use when the user asks
  to address/handle/resolve review annotations, margin annotations, or review
  comments, or says "I made an annotation". Reads annotations via
  `margin list --json` and writes back via `margin status`.
---

# Addressing margin review annotations

`margin` is a local review tool. A reviewer leaves annotations on a commit's
diff; you consume them through the `margin` CLI, which is the only contract.
Read `## Gotchas` before the first edit.

## Workflow

1. **Read** all open annotations as JSON, once:

   ```
   margin list --json --open
   ```

   Run from inside the target repository (any subdirectory works; `margin`
   discovers the repo root). Drop `--open` to also see resolved/orphaned items.

   `margin schema` prints the JSON Schema of this output if you want to pin to
   its exact shape.

   If the reviewer is still annotating, add `--watch`: it blocks until they
   hand the review off, then prints. Prefer it over re-running `list` in a
   loop — a listing read mid-review may be missing annotations.

2. **Locate every annotation before editing anything.** The listing is a
   snapshot: the first edit to a file invalidates the `location` ranges of the
   other annotations in it. Resolve them all up front from `file` + `location`
   (a 1-based `[start, end]` line range), or re-run `list` after each edit to
   pick up re-anchored ranges.

   `location: null` means the annotation is **orphaned** — the anchored lines
   moved or vanished; fall back to matching `anchored_text` and confirm with the
   user before guessing.

   Read `history` here too: oldest first, it lists what was already recorded
   against the annotation. A `reopened` entry means the reviewer rejected an
   earlier attempt — its `text` says why, so don't repeat that attempt.

3. **Group** annotations that touch the same file and region, and take
   `question` items first — an answer can moot a `fix`. Decide a group's
   combined change before touching code: a `suggestion` to extract a helper and
   a `nit` on that helper's name are one edit, not two rounds of churn. When two
   annotations genuinely conflict, surface the conflict instead of picking a
   winner silently.

4. **Address and verify per group** rather than batching all checks to the end —
   a failure after a dozen unrelated edits is hard to attribute. Edit, then run
   the project's checks (e.g. `cargo test`, `cargo clippy --all-targets`, or
   whatever the repo uses) before moving to the next group.

   Run the checks even when the edit looks trivially safe. A one-line or
   docs-only change still runs them; "this cannot have broken anything" is not a
   reason to skip. If they fail, fix and re-verify before recording any outcome —
   never record `resolved` against a red check.

   Let the annotation's `type` set the bar:
   - `fix` — a defect to correct.
   - `suggestion` — a proposed improvement; apply if sound.
   - `question` — answer it; a code/comment change may or may not be needed.
   - `nit` — minor; apply unless it conflicts with something.
   - `praise` — no action; do not resolve unless the user asks.

   Honor the repo's own conventions (CLAUDE.md, surrounding code). Where an
   annotation asks for something those conventions forbid, don't quietly split
   the difference: either record `wont-do` naming the convention, or ask the
   reviewer.

   Re-run `margin list --json --open` between groups to see what is still
   outstanding — the store is the progress record, so nothing needs tracking
   alongside it.

5. **Record the outcome** for each annotation, one call per annotation even when
   a single edit covered several:

   ```
   margin status <id-or-prefix> resolved --reply "what changed and why"
   margin status <id-or-prefix> wont-do  --reply "why you declined"
   ```

   `<id-or-prefix>` is the `id` field or any unique prefix of it (e.g. the first
   8 chars). The `--reply` is shown back to the reviewer — make it specific.
   Mark items you addressed `resolved` and items you deliberately skipped
   `wont-do`; do not leave them silently open.

   `resolved` also records the change that addressed the annotation. Pass
   `--addressed-by <revision>` when you know it (e.g. the commit you just made);
   otherwise `margin` infers the current working revision.

## JSON fields (`margin list --json`)

The `list --json` output is a versioned envelope: a top-level
`{"format":"margin-review/list","version":1,"annotations":[…]}`. `version` is
the shape's format version — not the margin binary version — and a
machine-validated copy of this shape lives at
`schema/margin-agent-v1.schema.json`, retrievable via `margin schema`. Read the
wrapper and refuse to act on an unknown `version`. Field names below move with
the CLI, so trust `margin --help` and `margin list --help` if the two disagree.

| field           | meaning |
|-----------------|---------|
| `id`            | UUID; pass to `status` (prefix accepted). |
| `file`          | Path, relative to repo root. |
| `status`        | `open` \| `resolved` \| `wont_do` \| `orphaned`. |
| `type`          | `fix` \| `question` \| `suggestion` \| `nit` \| `praise` (omitted = plain note). |
| `body`          | The reviewer's text — the actual request. |
| `revision_id`   | Commit the annotation was anchored to, or `(working copy)` when it marks an uncommitted change. |
| `side`          | `new` (added/changed line) or `old` (a line the commit **deleted**; `location` then refers to the commit's parent). |
| `location`      | Current `[start, end]` 1-based lines, or `null` when the anchor is gone. |
| `orphaned`      | `true` when the anchor no longer resolves, whatever the `status`. |
| `anchored_text` | The lines the annotation was attached to (use to relocate if orphaned). |
| `addressed_by`  | Revisions already recorded as addressing it. |
| `history`       | Outcomes recorded so far, oldest first: `at`, `actor`, `action` (`handed_off` \| `resolved` \| `wont_do` \| `reopened` \| `addressed_by`), and the `text` said about it. |

## Example

One open annotation in the listing (the output is wrapped in a versioned
envelope — see the `## JSON fields` section for how to read `format`/`version`):

```json
{
  "format": "margin-review/list",
  "version": 1,
  "annotations": [
    {
      "id": "9f2c1a4e-7b30-4d5c-8e11-2a6f0c9d3b57",
      "file": "src/tui/app.rs",
      "status": "open",
      "type": "fix",
      "body": "Unwraps on an empty diff — this panics for a commit that touches no files.",
      "revision_id": "26a750dc",
      "side": "new",
      "location": [412, 414],
      "orphaned": false,
      "anchored_text": "let first = files.first().unwrap();",
      "addressed_by": [],
      "history": [{ "action": "handed_off", "actor": "reviewer", "text": null }]
    }
  ]
}
```

`location` puts it at `src/tui/app.rs:412-414`. Replace the `unwrap` with a
guard, run `cargo test` and `cargo clippy --all-targets`, then record it:

```
margin status 9f2c1a4e resolved --reply "Empty diff now short-circuits to an empty file view instead of unwrapping first()."
```

## Gotchas

- **Never read or edit the store.** `.margin/annotations.ndjson` is append-only
  and internal; current state is a fold over it. Go through the CLI — a
  hand-edited store desyncs from what every other reader derives.
- **The listing goes stale as you edit.** Its line ranges are a snapshot from
  step 1, not live — see step 2.
- **A listing taken mid-review can be short annotations.** Use `--watch` instead
  of polling `list`.
- **`praise` is not a task.** Leave it open; resolving it discards the reviewer's
  record of what they liked.
- **An id prefix must be unique.** `status` refuses an ambiguous one — lengthen
  the prefix rather than picking a candidate yourself.
- **`margin list --json` is version-carried.** Parse the wrapper
  (`format`/`version`/`annotations`) and refuse to proceed on an unknown
  `version` rather than guessing at fields you've never seen.

## Rules

- Mark `resolved` only what you actually addressed; mark `wont-do` what you
  deliberately declined, with a `--reply` saying why. Don't resolve an item you
  didn't address.
- One `status` call per annotation, each with its own `--reply`.
- Reopen a resolved item for re-review with `margin status <id> open`.
