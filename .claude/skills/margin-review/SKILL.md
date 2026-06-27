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
diff; you consume them through the `margin` CLI. The CLI is the only contract —
never read or edit the NDJSON store under `.margin/` directly.

## Workflow

1. **Read** the open annotations as JSON:

   ```
   margin list --json --open
   ```

   Run from inside the target repository (any subdirectory works; `margin`
   discovers the repo root). Drop `--open` to also see resolved/orphaned items.

2. **Locate** each annotation in the code. Use `file` + `location` (a 1-based
   `[start, end]` line range). If `location` is `null` the annotation is
   **orphaned** — the anchored lines moved or vanished; fall back to matching
   `anchored_text` and confirm with the user before guessing.

3. **Address** it by editing the code. Let the annotation's `type` set the bar:
   - `fix` — a defect to correct.
   - `suggestion` — a proposed improvement; apply if sound.
   - `question` — answer it; a code/comment change may or may not be needed.
   - `nit` — minor; apply unless it conflicts with something.
   - `praise` — no action; do not resolve unless the user asks.

   Honor the repo's own conventions (CLAUDE.md, surrounding code).

4. **Verify** before resolving — run the project's checks (e.g. `cargo test`,
   `cargo clippy --all-targets`, or whatever the repo uses).

5. **Record the outcome** for each annotation:

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

| field           | meaning |
|-----------------|---------|
| `id`            | UUID; pass to `status` (prefix accepted). |
| `file`          | Path, relative to repo root. |
| `status`        | `open` \| `resolved` \| `wont_do` \| `orphaned`. |
| `type`          | `fix` \| `question` \| `suggestion` \| `nit` \| `praise` (omitted = plain note). |
| `body`          | The reviewer's text — the actual request. |
| `revision_id`   | Commit the annotation was anchored to. |
| `location`      | Current `[start, end]` 1-based lines, or `null` when orphaned. |
| `anchored_text` | The lines the annotation was attached to (use to relocate if orphaned). |
| `addressed_by`  | Revisions already recorded as addressing it. |

## Rules

- Mark `resolved` only what you actually addressed; mark `wont-do` what you
  deliberately declined, with a `--reply` saying why. Don't resolve an item you
  didn't address.
- One `status` call per annotation, each with its own `--reply`.
- Reopen a resolved item for re-review with `margin status <id> open`.
