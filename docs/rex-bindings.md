# Rex session bindings

Rex sessions have no metadata, so worktree-sync records which git checkout each
Rex session works on in a file of its own. This page is the contract for
programs that read that file. worktree-sync is its only writer.

## Location

```
$XDG_STATE_HOME/worktree-sync/rex/bindings.json    when XDG_STATE_HOME is set and absolute
$HOME/.local/state/worktree-sync/rex/bindings.json otherwise (on every platform, macOS included)
```

A relative `XDG_STATE_HOME` is ignored, as the XDG spec requires. Writers and
readers must resolve the same path, so they must see the same `XDG_STATE_HOME`;
the Rex server's environment counts, because worktree-sync runs under it.

## Format (version 1)

```json
{
  "version": 1,
  "hosts": {
    "fcf58739-fba5-4b01-b6cf-022cd6ea6b06": {
      "sessions": {
        "session:01a1d1dc-1bdf-7111-bc71-452288dfffa8": {
          "repo_key": "/Users/ser/dev/app/.git",
          "repo_name": "app",
          "checkout_path": "/Users/ser/dev/.worktrees/app/feat-login",
          "branch": "feat/login",
          "is_linked_worktree": true,
          "bound_at": 1794522240
        },
        "session:01a1d1dc-2c4e-7a10-9d3b-1f0e5a6c7b8d": {
          "repo_key": "/Users/ser/dev/app/.git",
          "repo_name": "app",
          "checkout_path": "/Users/ser/dev/app",
          "branch": null,
          "is_linked_worktree": false,
          "bound_at": 1794520000
        }
      }
    }
  }
}
```

| Field | Meaning |
| ----- | ------- |
| `version` | Integer. A reader must treat a version it does not know as unreadable. |
| `hosts` key | The Rex server's `host_id`, from `rex whoami --json`. Session ids mean something only on their host. |
| `sessions` key | A Rex `session_id`, as `rex ls --json` lists it. |
| `repo_key` | Canonical absolute path of the git common dir. Equal `repo_key` means the same repository. |
| `repo_name` | Display name of the repository: the folder holding `.git` (or an embedded `.bare`), otherwise the common dir's own name. Herdr uses the same rule. |
| `checkout_path` | Canonical absolute root of the work tree the session is bound to. |
| `branch` | The branch checked out there, or `null` when HEAD is detached. Refreshed on every write, so it can lag behind a `git switch` until the next one. Kept as last seen if the checkout no longer exists. |
| `is_linked_worktree` | `false` for the repository's main checkout (the parent), `true` for a linked worktree (a child). |
| `bound_at` | Unix seconds (UTC) when the binding was made. |

## Reading

- **Membership is exactly the entries.** A session that is absent is unbound.
  Never infer membership from a terminal's working directory.
- **Parent of a linked session**: the sessions on the same host with the same
  `repo_key` and `is_linked_worktree = false`. When there are several, the
  parent is the one with the smallest `bound_at`; a tie goes to the smaller
  `session_id` (plain string comparison). A linked session with no bound parent
  still belongs to its repository by `repo_key` and `repo_name`.
- **Several sessions on one checkout** are ordered the same way: `bound_at`,
  then `session_id`.
- **Stale entries**: ignore any entry whose session is not in that host's
  `rex ls`. The writer prunes them, but only on its next write.
- **No file, or no key for this host**, means no bindings.
- **No locking needed.** The writer replaces the file atomically, so a read
  always sees a complete document.
- **Ignore unknown fields.** New optional fields do not change `version`.
- **Do not write the file.** Ask worktree-sync to change bindings instead
  (`worktree-sync rex bind`, `unbind`).

## Writing (worktree-sync only)

- Every change is a read-modify-write under an exclusive lock on the sidecar
  `bindings.lock` in the same directory. Waiting for the lock gives up after
  5 seconds with an error instead of hanging.
- The new content goes to `bindings.json.<pid>.tmp` in the same directory, is
  flushed to disk, renamed over `bindings.json`, and the directory is flushed.
  The directory is created with mode `0700`, the files with `0600`.
- A file that does not parse, or has an unknown `version`, is never
  overwritten: the write fails and says why.
- While holding the lock, the writer drops this host's entries whose session is
  no longer live, using a fresh `rex ls`. When Rex can't be asked, nothing is
  pruned. Other hosts' entries are never touched. A host left with no sessions
  is removed.

## Versioning

Adding an optional field keeps the version. Renaming or removing a field, or
changing what one means, bumps it.
