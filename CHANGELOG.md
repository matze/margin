# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
