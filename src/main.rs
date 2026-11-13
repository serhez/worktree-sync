//! Entry point: pick the backend named by the first argument and hand off to
//! it. Everything that decides what happens lives in the library so it can be
//! exercised from `tests/`.

use std::process::ExitCode;

use anyhow::{Result, bail};

use worktree_sync::backends;

const USAGE: &str =
    "usage: worktree-sync herdr | worktree-sync rex <command> (see `worktree-sync rex --help`)";

fn main() -> Result<ExitCode> {
    let mut args = std::env::args_os().skip(1).peekable();
    match args.peek().and_then(|arg| arg.to_str()) {
        Some("herdr") => backends::herdr::main().map(|()| ExitCode::SUCCESS),
        // Rex shows the first stderr line of a failed action as is, so the
        // error is printed plainly rather than as `Error: {:?}`.
        Some("rex") => Ok(match backends::rex::cli::main(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err:#}");
                ExitCode::FAILURE
            }
        }),
        Some(other) => bail!("unknown backend `{other}`\n{USAGE}"),
        None => bail!("{USAGE}"),
    }
}
