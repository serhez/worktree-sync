//! `rex.toml`: where new worktrees go and where each repo keeps its sync file.

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use common::Dir;
use worktree_sync::backends::rex::settings::{FILE_NAME, Settings, config_dir_from_env};
use worktree_sync::config::DEFAULT_REPO_CONFIG_PATH;

const HOME: &str = "/home/me";

fn load(toml: &str) -> anyhow::Result<Settings> {
    let dir = Dir::new();
    dir.write(FILE_NAME, toml);
    Settings::load(dir.path(), Path::new(HOME))
}

#[test]
fn the_file_is_named_rex_toml() {
    assert_eq!(FILE_NAME, "rex.toml");
}

#[test]
fn a_missing_file_uses_the_defaults() {
    let dir = Dir::new();
    let settings = Settings::load(dir.path(), Path::new(HOME)).unwrap();

    assert_eq!(settings, Settings::defaults(Path::new(HOME)));
    assert_eq!(
        settings.worktree_directory,
        PathBuf::from("/home/me/.worktrees")
    );
    assert_eq!(
        settings.repo_config_path,
        PathBuf::from(DEFAULT_REPO_CONFIG_PATH)
    );
}

#[test]
fn an_empty_file_uses_the_defaults() {
    assert_eq!(load("").unwrap(), Settings::defaults(Path::new(HOME)));
}

/// Same expansion rule as Herdr's `worktree_directory`.
#[test]
fn a_leading_tilde_expands_to_home() {
    let settings = load(r#"worktree_directory = "~/dev/.worktrees""#).unwrap();
    assert_eq!(
        settings.worktree_directory,
        PathBuf::from("/home/me/dev/.worktrees")
    );
}

#[test]
fn an_absolute_directory_is_kept() {
    let settings = load(r#"worktree_directory = "/srv/wt""#).unwrap();
    assert_eq!(settings.worktree_directory, PathBuf::from("/srv/wt"));
}

/// A relative directory would resolve against whatever cwd the binary runs
/// in (the Rex server's, a layer's), landing worktrees somewhere different
/// each time.
#[test]
fn a_relative_directory_is_rejected() {
    let err = load(r#"worktree_directory = "dev/wt""#).unwrap_err();
    assert!(format!("{err:#}").contains("absolute"), "{err:#}");
}

#[test]
fn a_custom_repo_config_path_is_used() {
    let settings = load(r#"repo_config_path = "config/worktree.yaml""#).unwrap();
    assert_eq!(
        settings.repo_config_path,
        PathBuf::from("config/worktree.yaml")
    );
    assert_eq!(
        settings.plugin_config().repo_config_path,
        PathBuf::from("config/worktree.yaml")
    );
}

/// Same validation as the Herdr plugin setting: inside the repo, a known
/// format.
#[test]
fn an_invalid_repo_config_path_is_rejected() {
    for bad in [
        r#"repo_config_path = "../outside.toml""#,
        r#"repo_config_path = "/abs.toml""#,
        r#"repo_config_path = "config.json""#,
        r#"repo_config_path = """#,
    ] {
        assert!(load(bad).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn unknown_fields_are_rejected() {
    let err = load(r#"worktree_dir = "~/wt""#).unwrap_err();
    assert!(format!("{err:#}").contains("worktree_dir"), "{err:#}");
}

#[test]
fn a_parse_error_names_the_file() {
    let err = load("worktree_directory = ").unwrap_err();
    assert!(format!("{err:#}").contains(FILE_NAME), "{err:#}");
}

#[test]
fn the_config_dir_follows_xdg_config_home_when_absolute() {
    let home = Some(OsStr::new(HOME));
    assert_eq!(
        config_dir_from_env(Some(OsStr::new("/xdg")), home).unwrap(),
        PathBuf::from("/xdg/worktree-sync")
    );
    // The XDG spec says relative values are invalid and must be ignored.
    for ignored in [Some(OsStr::new("rel/xdg")), Some(OsStr::new("")), None] {
        assert_eq!(
            config_dir_from_env(ignored, home).unwrap(),
            PathBuf::from("/home/me/.config/worktree-sync"),
            "{ignored:?}"
        );
    }
    assert!(config_dir_from_env(None, None).is_err());
}
