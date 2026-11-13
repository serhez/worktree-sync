# Worktree Sync for Rex

Rex has sessions but no notion of git worktrees. The `rex`
backend adds them: each Rex session can be **bound** to a git checkout, and
five actions create, open, remove, bind, and sync worktrees around those
bindings. New worktrees get the same per-repo setup (copy, clone, symlink,
install, hooks) the Herdr plugin runs, from the same `.worktree-sync.toml`.

Requires Rust 1.89+ to build, and macOS or Linux. Remote Rex hosts are not
supported yet: the actions refuse to run in a session of a remote host.

## Install

1. Build and install the binary:

   ```sh
   cargo install --path . --locked     # installs ~/.cargo/bin/worktree-sync
   ```

   Or `cargo build --release` and copy `target/release/worktree-sync` anywhere.
   Reinstall after every update; the Lua module below comes from the binary.

2. Install the Lua module next to your Rex config:

   ```sh
   worktree-sync rex lua > ~/.config/rex/worktree_sync.lua
   ```

3. Load it from `~/.config/rex/init.lua`:

   ```lua
   require("worktree_sync").setup{
     bin = "/Users/you/.cargo/bin/worktree-sync",
     rex_bin_dir = "/Applications/Rex Beta.app/Contents/Helpers/Rex Server.rex/Contents/MacOS",
     keys = { open = "cmd+w", new = "shift+cmd+w", remove = "ctrl+cmd+w", sync = "ctrl+a>s" },
   }
   ```

   | Option | Meaning |
   | ------ | ------- |
   | `bin` | Absolute path of `worktree-sync`. Required: the Rex server's `PATH` is minimal. |
   | `rex_bin_dir` | Absolute path of the folder holding the `rex` binary, inside the Rex app (adjust the example to your app's name). Defaults to the server's `$REX_BIN_DIR`; setting it explicitly is safer. |
   | `keys` | Optional key per action: `open`, `new`, `remove`, `bind`, `sync`. Every action is in the command palette either way. |

4. Reload the Rex config.

## Actions

All actions are under **Worktrees** in the command palette. Every action except
Bind needs the current session to be bound; an unbound session gets "This
session isn't bound to a checkout. Run Bind Session to Checkout… first."

| Action | What it does |
| ------ | ------------ |
| **Bind Session to Checkout…** | Asks which checkout this session works on, prefilled with the focused terminal's directory. Rebinding replaces the old binding. |
| **New Worktree…** | Opens a new session with a prompt for the branch (prefilled with a generated name like `worktree/calm-river-1a2b`) and the base (`HEAD` of this session's checkout). Creates the worktree at `<worktree_directory>/<repo>/<branch slug>`, binds the session to it, renames it after the folder, and runs the setup in a `setup` tab while a `shell` tab in the worktree is ready to use. An existing local branch is checked out as is (the base is ignored). Esc closes the session without creating anything. |
| **Open Worktree…** | Picks among this repository's worktrees, showing the sessions bound to each. Shows the earliest session bound to the pick, or creates one (named after the folder) and binds it. |
| **Remove Worktree…** | For a linked worktree: lists the branch, the path, and every session bound to it, and asks to confirm. Then `git worktree remove`, drops those bindings, shows the repository's main-checkout session if one is bound, and closes the listed sessions, this one last. The branch is kept. If git refuses because of modified or untracked files, a second confirmation offers to delete them (`--force`). A locked worktree is never forced. |
| **Sync Worktree Files** | Re-applies the copy, clone, and symlink operations to this session's linked worktree, without hooks or installs. |

New and Open work from any bound session, including a linked worktree's. The
repository's main checkout cannot be removed or synced.

### Setup results

- **Success**: a small toast in the `shell` tab for five seconds, per
  `[notify] when` in the repo's config.
- **Failure** with `[failure] action = "keep"` (the default): the `setup` tab
  keeps the full report on screen and is focused; the tab shows Rex's error
  indicator (unless `[notify] when = "never"`). Press Enter to close it.
- **Failure** with `action = "remove"`: the report opens in the session the
  action started from, then the worktree is force-removed, its branch deleted if
  this run created it and it is merged (`git branch -d`), and the new session
  closed.

Reports are also written to
`$XDG_STATE_HOME/worktree-sync/rex/reports/failure-<branch>.log` (default
`~/.local/state/…`).

## Remote Control

Rex shows another session only through the app. Actions run from a key or the
palette switch sessions on their own. Commands run from a terminal (Open and
Remove finish in one) need Rex's **Remote Control** setting (Settings → Rex
Server). When it is off, the work still happens and the command prints where to
go instead, for example `Go to session "app": Rex.app is not accepting actions…`.

## Settings

`${XDG_CONFIG_HOME:-~/.config}/worktree-sync/rex.toml`, optional:

```toml
worktree_directory = "~/.worktrees"        # default; ~/ is expanded, the result must be absolute
repo_config_path   = ".worktree-sync.toml" # default; same rules as the Herdr plugin's setting
```

New worktrees go to `<worktree_directory>/<repo name>/<branch slug>`, the same
path Herdr picks, so set `worktree_directory` to Herdr's
`[worktrees] directory` if you use both. Unknown keys are an error.

## Command line

The actions are thin wrappers around these commands, which also work from any
shell (for scripts and agents). Inside a Rex terminal `--session` and `--from`
default to `$REX_SESSION`. Run `worktree-sync rex <command> --help` for details.

```sh
worktree-sync rex bind [PATH] [--session S]        # bind to the checkout containing PATH (default: cwd)
worktree-sync rex unbind [--session S]
worktree-sync rex new [BRANCH] [--base REF] [--from S]
worktree-sync rex open [PATH] [--from S]            # no PATH: pick (needs a terminal)
worktree-sync rex remove [--session S] [--yes] [--force]
worktree-sync rex sync [--session S]
worktree-sync rex lua                               # print the Lua module
```

Without a terminal, `remove` refuses unless given `--yes`, and refuses a dirty
worktree unless also given `--force` (which deletes its modified and untracked
files).

## Bindings

Bindings live in `${XDG_STATE_HOME:-~/.local/state}/worktree-sync/rex/bindings.json`,
keyed by Rex host and session. Bindings of closed sessions are dropped on the
next change. Other tools may read the file; its format is in
[rex-bindings.md](rex-bindings.md).

**Keep `XDG_STATE_HOME` and `XDG_CONFIG_HOME` the same everywhere.** The
actions, and every command they start in a Rex terminal, run with the Rex
server's environment, while commands you type run with your shell's. If the two
disagree on these variables, they read different bindings and settings.
