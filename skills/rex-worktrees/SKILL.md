---
name: rex-worktrees
description: "Create, open, sync and remove git worktrees in Rex with worktree-sync, one Rex session each, set up by the repository's config (copied files, installs, hooks). Use when the user asks for a worktree in Rex, or when you need one for yourself or another agent. Requires Rex (REX_SESSION set)."
---

# Rex worktrees

`worktree-sync rex` gives each worktree its own Rex session, bound to its
checkout, and sets new worktrees up from the repository's
`.worktree-sync.toml`. If `$REX_SESSION` is empty, say you are not in Rex and
stop.

Commands act on your session's repository (`--from` and `--session` default to
`$REX_SESSION`). If your session is not bound to a checkout,
`worktree-sync rex bind` binds it to your cwd's. Pass `--no-switch` unless the
user wants to see the session: without it, the Rex app moves there.

- `new BRANCH --wait --no-switch [--base REF]` checks out BRANCH, or creates it
  at REF (default `HEAD`), in a new worktree and session, runs the setup, and
  prints JSON: `session_id`, `block_id` (a shell there), `checkout_path`,
  `branch`. Installs can take minutes: set your shell tool's time limit to
  match. A failed setup exits 1 and names its report.
- `open PATH --no-switch` prints the session of PATH's checkout, making and
  binding one if needed.
- `remove --session S --yes --no-switch` removes S's worktree (the branch
  stays) and closes its sessions, ending what runs there. Remove only worktrees
  you made or the user named. Modified or untracked files make it fail: ask the
  user before adding `--force`, which deletes them.
- `sync --session S` copies, clones and links the configured files again.
- `bind [PATH] --session S`, `unbind --session S`.

Results go to stdout, progress to stderr; `--help` has details. To start agents
in a worktree, see skill `rex-agents`.
