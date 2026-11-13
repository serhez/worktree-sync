---
name: rex-worktrees
description: "Create, open, sync and remove git worktrees in Rex with worktree-sync: one Rex session per worktree, set up by the repository's own config (copied files, installs, hooks). Use when the user asks for a worktree or a separate checkout of a branch in Rex, or when you need one for yourself or another agent. Requires running inside Rex (REX_SESSION set) and worktree-sync."
---

# Rex worktrees

`worktree-sync rex` gives each worktree a Rex session of its own, remembers
which checkout each session works on (its binding), and sets every new
worktree up from the repository's `.worktree-sync.toml`.

First check that you run inside Rex:

```bash
test -n "${REX_SESSION:-}"
```

If the check fails, say that you are not running inside Rex and stop.

The commands act on your session's repository (`--from` and `--session`
default to `$REX_SESSION`). If your session is not bound to a checkout they
say so: `worktree-sync rex bind` binds it to the checkout of your cwd.

Pass `--no-switch` unless the user wants to see the session: without it, the
Rex app moves there.

```bash
worktree-sync rex new feat/login --wait --no-switch
```

- `new BRANCH --wait [--base REF]` checks out BRANCH if it exists, else starts
  it at REF (default: your checkout's `HEAD`), in a new worktree with a session
  of its own, and runs the setup. Then it prints JSON: `session_id`,
  `block_id` (the session's shell), `checkout_path`, `branch`. Installs can
  take minutes; give your shell tool a time limit to match. A failed setup
  exits 1 and names its report; the repository's config decides whether the
  worktree stays.
- `open PATH --no-switch` prints the session of the checkout containing PATH,
  making and binding one if there is none.
- `remove --session S --yes` removes S's worktree and closes every session
  bound to it, ending what runs there. The branch is kept. Remove only
  worktrees you made or the user named. Modified or untracked files make it
  fail: ask the user before adding `--force`, which deletes them.
- `sync --session S` copies, clones and links the configured files into S's
  worktree again.
- `bind [PATH] --session S` and `unbind --session S` set or drop a binding.

Results go to stdout, progress to stderr. `worktree-sync rex COMMAND --help`
has the details. To start agents in a worktree's session, see the `rex-agents`
skill.
