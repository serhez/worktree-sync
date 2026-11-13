//! The Rex backend. Rex has no worktree support, so this backend provides the
//! whole lifecycle (open / new / remove / bind / sync) on top of the shared
//! bootstrap.
//!
//! A Rex session belongs to a checkout only through an explicit binding in
//! [`bindings`]; nothing is inferred from a terminal's cwd. Key-bound Lua
//! actions only launch work (`rex action …` must answer at once); prompts and
//! the lifecycle run in terminal blocks, through [`cli`]. All Rex access goes
//! through the [`api::Rex`] trait so [`workflows`] is testable without Rex.

pub mod api;
pub mod bindings;
pub mod cli;
pub mod settings;
pub mod ui;
pub mod workflows;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// `$<xdg>/<rel>` when the XDG variable is set and absolute (relative values
/// are invalid per the XDG spec and ignored), else `$HOME/<fallback>/<rel>`.
fn xdg_path(
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    fallback: &str,
    rel: &str,
) -> Result<PathBuf> {
    if let Some(dir) = xdg.map(Path::new).filter(|dir| dir.is_absolute()) {
        return Ok(dir.join(rel));
    }
    match home.filter(|home| !home.is_empty()) {
        Some(home) => Ok(Path::new(home).join(fallback).join(rel)),
        None => bail!("HOME is not set"),
    }
}
