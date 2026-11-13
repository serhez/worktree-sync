//! User-level Rex settings: `${XDG_CONFIG_HOME:-~/.config}/worktree-sync/rex.toml`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{self, DEFAULT_REPO_CONFIG_PATH, PluginConfig};

pub const FILE_NAME: &str = "rex.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Root of new worktrees: `<root>/<repo_name>/<branch slug>`. Absolute.
    pub worktree_directory: PathBuf,
    /// Where each repository keeps its sync config, relative to its root.
    pub repo_config_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    worktree_directory: Option<String>,
    repo_config_path: Option<PathBuf>,
}

impl Settings {
    pub fn defaults(home: &Path) -> Self {
        Self {
            worktree_directory: home.join(".worktrees"),
            repo_config_path: PathBuf::from(DEFAULT_REPO_CONFIG_PATH),
        }
    }

    /// Read `rex.toml` from `config_dir`; a missing file means defaults.
    pub fn load(config_dir: &Path, home: &Path) -> Result<Self> {
        let path = config_dir.join(FILE_NAME);
        let mut settings = Self::defaults(home);
        if !path.is_file() {
            return Ok(settings);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: File =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        if let Some(dir) = file.worktree_directory {
            settings.worktree_directory = expand_home(&dir, home);
            if !settings.worktree_directory.is_absolute() {
                bail!(
                    "{}: worktree_directory must be an absolute path or start with ~/: {dir}",
                    path.display()
                );
            }
        }
        if let Some(repo_config_path) = file.repo_config_path {
            config::validate_repo_config_path(&repo_config_path)
                .with_context(|| format!("in {}", path.display()))?;
            settings.repo_config_path = repo_config_path;
        }
        Ok(settings)
    }

    /// The per-repo config location in the shape `config::load` takes.
    pub fn plugin_config(&self) -> PluginConfig {
        PluginConfig {
            repo_config_path: self.repo_config_path.clone(),
        }
    }
}

/// Herdr's expansion: a leading `~/` (or a lone `~`) is the home directory.
fn expand_home(dir: &str, home: &Path) -> PathBuf {
    match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if dir == "~" => home.to_path_buf(),
        None => PathBuf::from(dir),
    }
}

/// The directory holding `rex.toml`.
pub fn config_dir_from_env(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    super::xdg_path(xdg_config_home, home, ".config", "worktree-sync")
}
