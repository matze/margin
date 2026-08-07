# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.15.0]

### Changed

- `margin list --json` now returns a versioned envelope
  (`{"format":"margin-review/list","version":1,"annotations":[…]}`, one
  entry per annotation) instead of a bare array. This is a deliberate breaking
  change to the `--json` shape — the envelope carries its own format version so
  a reader can detect and refuse an unknown future shape in-band rather than
  guessing at fields. The `margin-review` skill and README both describe the new
  shape; the version is `1` and is not tied to the margin binary version.

### Added

- `margin schema` prints the JSON Schema of the `list --json` output, and the
  canonical, committed copy lives at `schema/margin-agent-v1.schema.json`. The
  embedded `margin-review` skill is updated to reference it and the `margin
  install-skill` command drops it as `schema.json` next to `SKILL.md`. A
  conformance test guarantees `list --json` output always agrees with the
  schema's envelope and optionality rules.

### Fixed

- Typing past the right edge of the annotation editor now wraps the body to the
  next line instead of clipping the tail, keeping the cursor on the character
  being typed.

## [0.14.0]

### Changed

- Annotations the agent closed (resolved or declined) no longer draw their inline
  block in the diff. The lines they cover keep a gutter icon (`✓`) instead of the
  bracket, so a finished annotation stays findable without spending diff rows on
  a note nobody has to act on. `S` draws every closed annotation's block again
  and collapses them back; hovering one still edits, deletes, reopens or shows
  its timeline, and the annotation overview (`A`) lists it either way.
- The `margin-review` skill now resolves every annotation's location before it
  edits anything, groups annotations that touch the same region so one change
  covers them together, and runs the project's checks per group instead of once
  at the end. Previously it walked the listing item by item, which let the first
  edit invalidate the line ranges of the remaining annotations in that file.
- The `margin-review` skill states that the project's checks run even for an
  edit that looks trivially safe, says what to do when they fail or when an
  annotation conflicts with the repo's conventions, collects the store
  prohibition and the other hazards under `## Gotchas`, carries a worked example,
  and names the `margin` version its JSON field table describes.

## [0.13.0]

### Added

- The context header's counters now carry the key that steps them (`J/K commit
  2/7`, `N/P 3 of 5 open`), so moving between commits and between annotations is
  visible where the position is read rather than only in the key reference.
- `?` opens a key reference listing every binding, grouped by what it does. The
  help bar under the diff now carries only the keys a review reaches for
  constantly plus whatever the cursor makes available, instead of trying to
  spell out the whole keymap on one line.

### Changed

- The top band is gone and with it the focus/view split that `Tab` and
  `Shift-Tab` drove. The diff now owns the screen and always has the keyboard,
  under a one-line context header naming the loaded commit, the current file,
  the commit's place in the review and the open-annotation count. The commit,
  file and annotation lists open as pickers over the diff — `c`, `f` and `A`,
  each reaching its list directly and switching between them without closing
  first. Moving a picker previews the target in the diff; `Enter` keeps the
  preview, `Esc` discards it and restores the position the picker opened from
  (`Shift-Tab` used to move the diff cursor, and could switch commits, as a
  side effect of cycling views). The diff no longer resizes as lists grow or
  views change, and it gains the rows the band used to hold.
- The agent handoff moved from `c` / `C` to `x` / `X`, freeing `c` for the
  commit list.
- The key reference is dismissed by any key, not just `?` or `Esc`, so a glance
  at it costs one keystroke to leave and the dismissing key does not also act on
  the diff.
- A picker's own key now closes it: `c`, `f` and `A` toggle their list, leaving
  the diff on the preview the picker produced like `Enter` does. `t` likewise
  closes the timeline it opened. `Esc` still dismisses, restoring the position
  the picker opened from.
- A commit's annotation marker now follows its id instead of leading it, in both
  the context header and the commit picker, and the column is only held open
  once a commit fills it. Ahead of the id, the cell an unannotated commit left
  empty indented the header past the file and hunk headers below it and pushed
  unmarked picker rows three columns off the panel's title. Rows now start where
  the title does whether or not they carry a marker, with the ids, markers and
  summaries each in their own column.
- The commit message beside the commit picker is inset by a column, so it clears
  the divider the way the list clears the border instead of running flush
  against it. The agent log's lines are inset the same way.
- The panel holding the keyboard now says so: a picker, the timeline and the key
  reference draw their border in cyan and sink the diff they cover toward a new
  backdrop background, while the agent log — which the diff stays navigable
  beside — keeps its gutter-colored border. Nothing marked the active surface
  before, so an open picker read as one more pane. A tinted background is
  darkened by scaling its channels rather than blended toward the backdrop, so
  added, removed and annotated rows keep their hue and stay recognizable while
  they are out of play.
- `q` leaves margin only from the bare diff. With a picker, the timeline, the
  key reference or the agent log open it closes that instead, so reaching for it
  to dismiss an overlay can no longer end the review by accident. `Ctrl-c`
  still quits from anywhere.

### Removed

- `Tab`, `Shift-Tab`, and the `h` / `l` focus keys, which no longer have a
  second pane to move between.

## [0.12.0]

### Added

- A landing page under `docs/`, deployed to GitHub Pages by a new `pages.yml`
  workflow: the loop as a diagram, the four stages of a review, the agent
  contract, install, and the comparison table.
- The README compares margin with tuicr, hunk and lumen on margin's own axes,
  and states the non-goals (forge submission, foreign pull requests, mercurial,
  a full vim modality, a theme gallery, a library API) so the tools that do
  cover them are easy to find.
- Track the annotated commit across amend and rebase on git, not only on jj: a
  commit that history no longer contains is matched against recent commits on
  all refs by author, author date and subject, so `revision_state` reports
  `amended`, `divergent` or `abandoned` there too. The heuristic is weaker than
  jj's change ids — a reworded commit reads as `abandoned` — and `revision_state`
  is now omitted only for annotations on git's working copy.
- `margin list --json` now reports each annotation's `history`: the outcomes
  recorded against it, oldest first, with the reply or reopen reason said about
  each. Replies were written to the log but never read back out, so an agent
  re-run could not tell that its previous attempt had been rejected, or why.
- Review uncommitted changes on git: the working copy is listed as its own
  target (`(uncommitted changes)`) and diffed against `HEAD`, so an agent's
  edits can be reviewed before they are committed. jj already surfaced them as
  the `@` revision. In `list --json`, such an annotation reports
  `"revision_id": "(working copy)"`.
- Hand a finished review off to a waiting agent: `H` in the TUI records that
  every open annotation is now the agent's to act on, and `margin list --watch`
  blocks until that happens before printing. An agent can be pointed at a review
  once instead of polling and guessing when the reviewer is done.
- `margin install-skill` learned `--print`, writing the skill to stdout instead
  of installing it, and `--dir` for a skills root other than `~/.claude/skills`.
  The skill documents nothing but the CLI, so agents that read an `AGENTS.md` or
  keep instructions elsewhere can use it too.

## [0.11.0]

### Changed

- **Breaking:** `--theme` enforcement has been removed and relies entirely on
  terminal detection or dark fallbacks.

## [0.10.0]

### Changed

- Soft-wrap long diff lines onto continuation lines instead of truncating them,
  in both unified and split views.

## [0.9.0]

### Changed

- Inline annotations now show their type as a single-width glyph in the gutter
  (fix `✗`, question `?`, suggestion `✎`, nit `·`, praise `★`, untyped note `◦`)
  instead of the spelled-out word, which read as noise next to the body. The
  editor's `type:` field pairs the glyph with the word so the mapping stays
  discoverable; `list --json` keeps the full word as the agent contract.
- `n`/`p` (next/previous change) now stop separately on the removed and added
  side of a modification instead of only landing on the removal, so both sides
  can be annotated directly without an extra `j` — most noticeable in split view.

## [0.8.1]

### Fixed

- Expanding or collapsing context (`+`/`-`) now keeps the cursor on the same
  source line instead of letting the spliced-in context shift it to a different
  line.
- Inline annotation bodies now word-wrap across multiple lines instead of
  rendering on a single line whose tail ran off the right edge.

## [0.8.0]

### Added

- Word-level ("intraline") diff highlighting: within a modified line paired with
  its replacement, only the changed words get a brighter background tint on top
  of the base add/remove tint, so a one-word change no longer reads as a
  whole-line change (like `delta`'s `minus-emph`/`plus-emph`). Applies to both
  the unified and split diff views.

### Changed

- Diff line-number gutters now tint green/red on added/removed lines (matching
  the sign color) instead of a flat gray, closer to how pagers like `delta`
  color their line-number columns.

## [0.7.0]

### Changed

- Syntax highlighting now runs on a background thread and prewarms the whole
  diff: the visible lines color first and the rest fill in behind them, so the
  redraw never stalls (notably on Markdown, which is expensive to highlight) and
  scrolling does not flash plain text onto already-loaded lines.
- The TUI now always starts with the diff focused (where annotating happens),
  rather than the top band — previously only single-commit reviews did so.
- jj revisions in the commit list now highlight their shortest unique change-id
  prefix in magenta, matching how jj itself renders change ids.

## [0.6.0]

### Added

- README screenshots (dark and light), rendered deterministically from the
  headless TUI via the ignored `dump_screenshot` test (`cargo test
  dump_screenshot -- --ignored`).
- Compose an annotation in `$EDITOR`: `Ctrl-e` in the annotation editor suspends
  the TUI and opens the body in `$VISUAL`/`$EDITOR` (falling back to `vi`), seeded
  below a marker line; saving feeds the text back. The ignored block above the
  marker quotes the annotated source lines for reference. Everything above the
  marker is ignored.
- Trigger a headless coding agent from the TUI: `c` hands the focused annotation
  to a `claude` session, `C` hands it every open annotation, and `L` toggles a
  log panel below the diff that streams the session's assistant messages and tool calls. The
  status line tracks progress and markers flip live as the agent records
  outcomes; the session is non-blocking. The agent inherits the environment (so
  `CLAUDE_CONFIG_DIR`/`PATH` reach it), and `MARGIN_AGENT_CMD` overrides the
  command.

### Changed

- The annotation editor now supports in-buffer cursor movement and editing:
  character/line motion (arrows), word motion (`Ctrl-←`/`Ctrl-→`), line ends
  (`Home`/`End`), and `Del` / `Ctrl-w` deletion. Typing and Backspace act at the
  cursor instead of only at the end.

### Fixed

- The commit list/message divider in the top band now lines up with the
  split-diff divider below it. The two were off by one column.

## [0.5.0]

### Added

- Jump between annotations: `N` / `P` (from either pane) move the cursor to the
  first line of the next / previous annotated span, crossing into the nearest
  adjacent commit with an anchored annotation once the current diff is
  exhausted.
- Reload the review state without restarting: `R` re-reads revisions, the diff,
  and the annotation log from disk, reflecting work an agent did while margin
  stayed open. The same reload also runs automatically as soon as the annotation
  log changes on disk.

### Fixed

- The selected commit/file/annotation in the top band no longer keeps the
  cursor background tint once focus moves to the diff; an unfocused band now
  marks its selection with bold alone, matching the diff cursor.

### Changed

- The timeline popup (`t`) now aligns under the annotation's text and opens
  directly above or below the annotated line(s) instead of covering them. Events
  read newest-first as one connected thread (subdued bullets joined by a
  continuing bar), long replies word-wrap with the bar carried down every line,
  and the border is muted.
- The TUI input loop is now async and fully event-driven (crossterm
  `event-stream` + `notify` on a `futures-lite` executor): it reacts to key
  input and filesystem changes via wakers instead of polling on a timer, so it
  no longer wakes periodically while idle.

## [0.4.0]

### Changed

- Reworked the interface: the left sidebar is replaced by a top band that shows
  one view at a time: commits (list beside the selected commit's message),
  files, or annotations. `Shift-Tab` cycles the band view; `Tab` toggles focus
  between the band and the diff.
- Annotation editor key hints are styled consistently with the diff help line
  (bold, accented keys). The redundant `(ctrl-t)` is dropped from the box title.

### Fixed

- Annotations on deleted lines now render inline in the diff. They were filtered
  out after saving, so the block showed only while the editor was open. In split
  view an old-side block hangs under the left cell with the column divider
  intact.

## [0.3.0]

### Added

- jj change tracking: each annotation records the commit its change pointed at
  when captured, and re-anchoring classifies the change as `unchanged`,
  `amended`, `divergent`, or `abandoned`. Surfaced in `margin list --json`
  (`revision_state`, plus `current_commit` when amended) and flagged in the
  timeline view (`~`/`!`/`×`). git has no stable change identity across amend, so
  the field is reported as unsupported/omitted there.

### Changed

- **Breaking (store format):** annotation anchors now require a captured commit
  hash, so `.margin/annotations.ndjson` logs written before this change will no
  longer parse.

## [0.2.0]

### Added

- Split view, reachable with `s`.
- `shift-j`/`shift-k` to move between commits.

### Fixed

- Gate Unix-only terminal theme detection behind `cfg(unix)`.

## [0.1.0]

### Added

- Initial release: a local TUI for code-review annotations over git/jj.
- Annotate a commit's diff and consume annotations through the `margin` CLI
  (`list --json` to read, `status` to write back).
