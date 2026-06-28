# margin

A local TUI for code-review annotations over git/jj.

Agentic development turns you into a reviewer, but the review loop is stuck in
chat. `margin` lets you step through a change in the terminal, pin comments to
lines or ranges, and hand them to a coding agent through a small CLI — no
throwaway PR or GitHub round-trip.

## Usage

Run inside a repository:

```sh
margin                  # open the TUI; sidebar lists commits in <base>..@
margin --base develop   # set the base ref explicitly
margin --rev <id>       # jump straight into one commit's diff
```

Select a commit, navigate files → hunks → lines, mark a line or range, and type
an annotation. Annotations persist in `.margin/annotations.ndjson`.

## Agent handoff

The CLI is the contract: the agent reads the review and writes back its
resolutions through it, never by parsing the store directly.

```sh
margin list --json                        # the review as machine-readable JSON (read)
margin list [--open]                      # same, one human-readable line per annotation
margin status <id> resolved [--reply ..]  # mark one addressed (write)
margin status <id> wont-do  [--reply ..]  # decline one
margin status <id> open     [--reason ..] # reopen for re-review
```

`margin list --json` folds the event log into current per-annotation state
(status, re-anchored location, snippet), so the agent never touches the raw
NDJSON.

## Config

Optional `.margin/config.toml` at the repo root: `base` and `theme`, both
optional.

## Build

```sh
cargo build --release
cargo test
```

## License

[MIT](./LICENSE)
