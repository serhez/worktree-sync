//! Git plumbing for hosts that have no worktree support of their own: repo
//! identity, Herdr-compatible naming, and worktree add/remove.
//!
//! Every command runs with `LC_ALL=C` so error classification can match git's
//! English messages. Paths handed back are canonical, so they compare equal
//! however a repo was reached (macOS `/var` vs `/private/var`).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

/// A checkout folder name for `branch`, as Herdr derives it: lowercase ASCII
/// alphanumerics, every other run of characters collapsed to one `-`.
pub fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::with_capacity(branch.len());
    for c in branch.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "worktree".to_string()
    } else {
        slug.to_string()
    }
}

/// Where a new worktree for `branch` goes: `<root>/<repo_name>/<slug>`, the
/// same path Herdr picks, so both hosts agree on where a branch lives.
pub fn default_checkout_path(root: &Path, repo_name: &str, branch: &str) -> PathBuf {
    root.join(repo_name).join(branch_to_path_slug(branch))
}

const ADJECTIVES: [&str; 8] = [
    "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
];
const NOUNS: [&str; 8] = [
    "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
];

/// Herdr's generated branch name, `worktree/<adjective>-<noun>-<hex4>`.
pub fn generated_branch_slug(seed: u64) -> String {
    let adjective = ADJECTIVES[(seed % 8) as usize];
    let noun = NOUNS[((seed / 8) % 8) as usize];
    format!("worktree/{adjective}-{noun}-{:04x}", seed & 0xffff)
}

/// The work tree containing some path, and the repository it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// Canonical work-tree root.
    pub path: PathBuf,
    /// Canonical git common dir: equal keys ⇔ same repository.
    pub repo_key: PathBuf,
    pub repo_name: String,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    pub is_linked_worktree: bool,
}

/// Identify the work tree containing `path`. Fails outside any work tree,
/// including inside a bare repository.
pub fn checkout_at(path: &Path) -> Result<Checkout> {
    let out = run(git(path).args([
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-common-dir",
        "--git-dir",
    ]))
    .with_context(|| format!("{} is not inside a git work tree", path.display()))?;
    let mut lines = out.lines();
    let (Some(top), Some(common), Some(git_dir)) = (lines.next(), lines.next(), lines.next())
    else {
        bail!("unexpected `git rev-parse` output in {}", path.display());
    };
    let path = canonical(Path::new(top))?;
    let repo_key = canonical(Path::new(common))?;
    let is_linked_worktree = canonical(Path::new(git_dir))? != repo_key;
    Ok(Checkout {
        branch: current_branch(&path)?,
        repo_name: repo_name(&repo_key),
        path,
        repo_key,
        is_linked_worktree,
    })
}

/// The branch checked out in `checkout`: `Ok(None)` when detached, an error
/// when git can't tell (not a repo any more).
pub fn current_branch(checkout: &Path) -> Result<Option<String>> {
    let output = output(git(checkout).args(["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
    match output.status.code() {
        Some(0) => Ok(Some(stdout(&output))),
        Some(1) => Ok(None),
        _ => bail!("{}", stderr(&output)),
    }
}

/// Herdr's repo name: the folder holding `.git` or an embedded `.bare`,
/// otherwise the common dir's own name.
fn repo_name(common_dir: &Path) -> String {
    let own = common_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    match own.as_deref() {
        Some(".git" | ".bare") => common_dir
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        _ => own.unwrap_or_default(),
    }
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// Without `refs/heads/`; `None` when detached or bare.
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    /// Git has lost the work tree (its folder is gone).
    pub is_prunable: bool,
}

pub fn parse_worktree_list_porcelain(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in output.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                entries.extend(current.take());
                current = Some(WorktreeEntry {
                    path: PathBuf::from(value),
                    branch: None,
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                });
            }
            _ => {
                let Some(entry) = current.as_mut() else {
                    continue;
                };
                match key {
                    "branch" => {
                        entry.branch =
                            Some(value.strip_prefix("refs/heads/").unwrap_or(value).into());
                    }
                    "bare" => entry.is_bare = true,
                    "detached" => entry.is_detached = true,
                    "prunable" => entry.is_prunable = true,
                    _ => {}
                }
            }
        }
    }
    entries.extend(current);
    entries
}

/// Every work tree of the repository containing `repo`, main checkout (or the
/// bare repo) first. Paths are canonical where they still exist.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = run(git(repo).args(["worktree", "list", "--porcelain"]))?;
    let mut entries = parse_worktree_list_porcelain(&out);
    for entry in &mut entries {
        if let Ok(path) = entry.path.canonicalize() {
            entry.path = path;
        }
    }
    Ok(entries)
}

/// The repository's main checkout: the first `worktree list` entry. It is the
/// source of synced files and of the per-repo config, as in Herdr.
pub fn main_checkout(repo: &Path) -> Result<PathBuf> {
    list_worktrees(repo)?
        .into_iter()
        .next()
        .map(|entry| entry.path)
        .context("`git worktree list` returned nothing")
}

/// Reject what git would refuse as a branch name, before anything is created.
pub fn validate_branch_name(repo: &Path, branch: &str) -> Result<()> {
    if branch.is_empty() || branch.starts_with('-') {
        bail!("invalid branch name {branch:?}");
    }
    let output = output(git(repo).args(["check-ref-format", "--branch", branch]))?;
    if !output.status.success() {
        bail!("invalid branch name {branch:?}");
    }
    Ok(())
}

/// Whether [`add_worktree`] created the branch or checked out an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    NewBranch,
    ExistingBranch,
}

/// Add a work tree at `path` (which must not exist). An existing local branch
/// is checked out as is (`base` ignored, as in Herdr); otherwise the branch is
/// created at `base`, resolved in `repo`'s checkout.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<Added> {
    validate_branch_name(repo, branch)?;
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let mut cmd = git(repo);
    let added = if branch_exists(repo, branch)? {
        cmd.args(["worktree", "add", "--"]).arg(path).arg(branch);
        Added::ExistingBranch
    } else {
        cmd.args(["worktree", "add", "-b", branch, "--"])
            .arg(path)
            .arg(base);
        Added::NewBranch
    };
    run(&mut cmd)?;
    Ok(added)
}

fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    let output = output(
        git(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}")),
    )?;
    Ok(output.status.success())
}

/// Why [`remove_worktree`] did not remove anything.
#[derive(Debug)]
pub enum RemoveError {
    /// Modified or untracked files (or submodules): removable with `force`.
    NeedsForce(String),
    /// Anything else git refused, such as a locked work tree.
    Failed(String),
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsForce(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RemoveError {}

/// `git worktree remove [--force]`. One `--force` at most: a locked work tree
/// (which needs two) is reported, never overridden. Deleting files is left to
/// git; this never deletes anything itself.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<(), RemoveError> {
    let mut cmd = git(repo);
    cmd.args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg("--").arg(path);
    let output = output(&mut cmd).map_err(|e| RemoveError::Failed(format!("{e:#}")))?;
    if output.status.success() {
        return Ok(());
    }
    let message = stderr(&output);
    if !force && is_dirty_worktree_remove_error(&message) {
        Err(RemoveError::NeedsForce(message))
    } else {
        Err(RemoveError::Failed(message))
    }
}

/// Herdr's classifier for git refusals that a forced retry resolves.
pub fn is_dirty_worktree_remove_error(message: &str) -> bool {
    let message = message.to_lowercase();
    (message.contains("contains modified or untracked files")
        && message.contains("use --force to delete it"))
        || message.contains("working trees containing submodules cannot be moved or removed")
}

/// `git branch -d`: deletes `branch` only if it is merged, so unmerged work
/// survives. Never `-D`.
pub fn delete_merged_branch(repo: &Path, branch: &str) -> Result<()> {
    run(git(repo).args(["branch", "-d", branch])).map(drop)
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("LC_ALL", "C");
    cmd
}

fn output(cmd: &mut Command) -> Result<Output> {
    cmd.output().context("failed to run git")
}

/// Run `cmd`, returning its trimmed stdout or failing with its stderr.
fn run(cmd: &mut Command) -> Result<String> {
    let output = output(cmd)?;
    if !output.status.success() {
        bail!("{}", stderr(&output));
    }
    Ok(stdout(&output))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("resolving {}", path.display()))
}
