# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Reworked the interface: the left sidebar is replaced by a top band that shows
  one view at a time — commits (list beside the selected commit's message),
  files, or annotations. `Shift-Tab` cycles the band view; `Tab` toggles focus
  between the band and the diff.
- Annotation editor key hints are styled consistently with the diff help line
  (bold, accented keys); the redundant `(ctrl-t)` is dropped from the box title.

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
