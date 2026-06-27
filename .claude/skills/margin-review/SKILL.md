---
name: margin-review
description: >-
  Address code-review annotations left in the margin TUI. Use when the user asks
  to address/handle/resolve review annotations, margin annotations, or review
  comments, or says "I made an annotation". Reads annotations via
  `margin list --json` and writes back via `margin resolve`.
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

5. **Resolve** each addressed annotation, recording what you did:

   ```
   margin resolve <id-or-prefix> --reply "what changed and why"
   ```

   `<id-or-prefix>` is the `id` field or any unique prefix of it (e.g. the first
   8 chars). The `--reply` is shown back to the reviewer — make it specific.

## JSON fields (`margin list --json`)

| field           | meaning |
|-----------------|---------|
| `id`            | UUID; pass to `resolve` (prefix accepted). |
| `file`          | Path, relative to repo root. |
| `status`        | `open` \| `resolved` \| `wont_do` \| `orphaned`. |
| `type`          | `fix` \| `question` \| `suggestion` \| `nit` \| `praise` (omitted = plain note). |
| `body`          | The reviewer's text — the actual request. |
| `revision_id`   | Commit the annotation was anchored to. |
| `location`      | Current `[start, end]` 1-based lines, or `null` when orphaned. |
| `anchored_text` | The lines the annotation was attached to (use to relocate if orphaned). |
| `addressed_by`  | Revisions already recorded as addressing it. |

## Rules

- Resolve only what you actually addressed. If you decline an item, leave it open
  and tell the user why rather than resolving it silently.
- One `resolve` call per annotation, each with its own `--reply`.
