//! Put the full failure text somewhere it can actually be read.
//!
//! A toast is a few lines wide and a host's own dialog truncates what it cannot
//! fit, so the line that says *why* a bootstrap failed is routinely the line
//! that gets cut. This module composes the whole report and writes it to a
//! file; each backend decides where to show it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The report file's contents: what failed, where, and the full error chain.
///
/// Kept free of I/O so the wording is testable without a host or a writable
/// state directory in the loop.
pub fn compose(branch: &str, worktree: &Path, err: &anyhow::Error) -> String {
    let mut body = format!(
        "Worktree sync failed\n\nbranch:   {branch}\nworktree: {}\n\n",
        worktree.display()
    );
    // One cause per line rather than anyhow's `{:#}`, which joins them with
    // ": " — the innermost cause here is a command's captured output, and
    // folding its newlines into one line is exactly the truncation this file
    // exists to avoid.
    for cause in err.chain() {
        body.push_str(&cause.to_string());
        body.push('\n');
    }
    body
}

/// Write `body` to `dir`, returning where it landed.
///
/// One file per branch, overwritten on each run: reports are only interesting
/// until the worktree is fixed or removed, and a timestamped name would grow a
/// directory nobody ever prunes.
pub fn write(dir: &Path, branch: &str, body: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = dir.join(report_file_name(branch));
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// A file name for `branch`. Branch names contain `/` (and herdr's own default
/// is `worktree/<name>`), which would otherwise be read as a subdirectory that
/// does not exist.
pub fn report_file_name(branch: &str) -> String {
    let safe: String = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("failure-{safe}.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> anyhow::Error {
        anyhow::anyhow!("`pnpm install` exited with exit status: 1\nERR_PNPM_ENOENT\nreflink")
            .context("install phase")
    }

    /// The report exists to carry the multi-line command output that a toast
    /// cannot: every line of it has to survive, on its own line.
    #[test]
    fn the_report_keeps_the_command_output_line_by_line() {
        let body = compose("worktree/x", Path::new("/tmp/wt"), &failure());

        assert!(body.contains("\nbranch:   worktree/x\n"), "got: {body}");
        assert!(body.contains("\nworktree: /tmp/wt\n"), "got: {body}");
        assert!(body.contains("\ninstall phase\n"), "got: {body}");
        assert!(body.contains("\nERR_PNPM_ENOENT\nreflink\n"), "got: {body}");
    }

    #[test]
    fn a_slashed_branch_name_does_not_become_a_subdirectory() {
        assert_eq!(
            report_file_name("worktree/silver-stone"),
            "failure-worktree-silver-stone.log"
        );
        assert!(!report_file_name("feature/a/b").contains('/'));
    }
}
