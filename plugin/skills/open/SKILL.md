---
description: Open the margin review TUI in a new WezTerm tab for a checkout. Use when the user asks to open, start or launch margin, or invokes /margin:open.
argument-hint: "[path] [margin flags, e.g. --base main]"
---

# Open margin in a new tab

Spawn margin in a new WezTerm tab next to this session:

```sh
wezterm cli spawn --cwd <checkout> -- margin [flags]
```

1. Pick the checkout. Use the path in the arguments if there is one. Otherwise
   use the checkout you are working in for this task, such as a lane's
   `.claude/worktrees/<lane>/repos/<repo>`. Fall back to the current
   directory. Resolve it to an absolute path and check that it holds a `.jj/`
   or `.git/`.
2. Pass any remaining arguments to margin unchanged, such as `--base main`.
3. Run the command above. It prints the new pane's ID. Name the tab with
   `wezterm cli set-tab-title --pane-id <id> "margin: <repo>"`.
4. Report the path margin opened. Don't wait for margin or read its pane.

If `$WEZTERM_PANE` is unset, this session isn't running inside WezTerm. Tell
the user to run `margin` in that checkout themselves.
