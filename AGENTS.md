This is a plugin for [Herdr](https://github.com/herdrdev/herdr).

## Code Style
- You MUST use meaningful function names.
- Write "How" in the code,"What" in the test code,"Why" in the commit log, and "Why not" in the code comments.

## CI
You MUST execute the following commands when you change any .rs files.

1. cargo fmt --all --check
2. cargo clippy --all-targets --all-features -- -D warnings
3. cargo test --all-features --locked

If you find any errors on above commands, you MUST fix it.

## Agent Authored

- Two backends share one binary, `worktree-sync`: `worktree-sync herdr` (the Herdr plugin, `src/backends/herdr/`) and `worktree-sync rex …` (worktree lifecycle for Rex, `src/backends/rex/` plus `lua/worktree_sync.lua`). User docs: `README.md`, `docs/rex.md`; the binding-file contract other tools (Canopy) read: `docs/rex-bindings.md` — never change its schema without the maintainer's approval.
- Design and plan for the Rex backend: `.local/plans/2026-11-12-rex-backend-design.md` (read it before changing Rex workflows; §2 lists the Rex facts the design relies on).
- Live Rex checks: `.local/scripts/rex-smoke.sh` (after `cargo build`) runs the non-interactive workflows end to end and cleans up. By hand, use only throwaway `wts-probe-*` sessions and a scratch repo, with `XDG_STATE_HOME`/`XDG_CONFIG_HOME` pointed at a temp dir; kill the sessions afterwards. Commands that the binary starts inside Rex terminals run with the Rex server's environment, not yours, so drive those steps directly from your shell (`REX_SESSION=… REX_BLOCK=… worktree-sync rex new … --here`). Try Lua with `rex do ./file.lua --action NAME` instead of reloading the user's config.
