//! A stand-in `rex` executable for tests that go through the real CLI
//! boundary (`RexCli`, the `worktree-sync` binary).
//!
//! It logs its argv, then answers from files in its directory, keyed by the
//! first subcommand word it sees (`whoami`, `ls`, `kill`, `do`, `api`,
//! `block`): `reply.<word>` if present, else `reply`. If `fail.<word>` or
//! `fail` exists, its contents go to stderr and it exits 1 instead. A script
//! piped to `do -` is kept in `stdin`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
for a in "$@"; do printf '%s\037' "$a" >> "$dir/log"; done
printf '\n' >> "$dir/log"
for a in "$@"; do
  if [ "$a" = "-" ]; then cat > "$dir/stdin"; break; fi
done
cmd=
for a in "$@"; do
  case "$a" in whoami|ls|kill|do|api|block) cmd=$a; break ;; esac
done
for f in "$dir/fail.$cmd" "$dir/fail"; do
  if [ -f "$f" ]; then cat "$f" >&2; exit 1; fi
done
for f in "$dir/reply.$cmd" "$dir/reply"; do
  if [ -f "$f" ]; then cat "$f"; exit 0; fi
done
"#;

pub struct RexScript {
    dir: PathBuf,
}

impl RexScript {
    /// Install the script as `<dir>/rex` (so `dir` works as `REX_BIN_DIR`).
    pub fn install(dir: &Path) -> Self {
        std::fs::create_dir_all(dir).unwrap();
        let bin = dir.join("rex");
        std::fs::write(&bin, SCRIPT).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn bin(&self) -> PathBuf {
        self.dir.join("rex")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Answer every request with `body`.
    pub fn reply(&self, body: &str) {
        std::fs::write(self.dir.join("reply"), body).unwrap();
    }

    /// Answer requests whose subcommand is `word` with `body`.
    pub fn reply_to(&self, word: &str, body: &str) {
        std::fs::write(self.dir.join(format!("reply.{word}")), body).unwrap();
    }

    /// Fail every request, printing `stderr`.
    pub fn fail(&self, stderr: &str) {
        std::fs::write(self.dir.join("fail"), stderr).unwrap();
    }

    /// Fail requests whose subcommand is `word`, printing `stderr`.
    pub fn fail_on(&self, word: &str, stderr: &str) {
        std::fs::write(self.dir.join(format!("fail.{word}")), stderr).unwrap();
    }

    /// What the last invocation with a `-` argument read from stdin.
    pub fn stdin(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("stdin")).ok()
    }

    /// The argv of every invocation so far, in order.
    pub fn invocations(&self) -> Vec<Vec<String>> {
        let Ok(log) = std::fs::read_to_string(self.dir.join("log")) else {
            return Vec::new();
        };
        log.lines()
            .map(|line| line.split('\u{1f}').map(str::to_string).collect::<Vec<_>>())
            .map(|mut args| {
                // Each argument is followed by the separator, so the split
                // leaves one empty trailing element.
                args.pop();
                args
            })
            .collect()
    }
}
