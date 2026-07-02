# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Expanding or collapsing context (`+`/`-`) now keeps the cursor on the same
  source line instead of letting the spliced-in context shift it to a different
  line.

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
