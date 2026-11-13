//! Shared support for the Rex backend tests: an in-memory Rex
//! ([`fake_rex`]), a scripted `rex` executable ([`rex_script`]), and a
//! workflow harness with real git repositories.
//!
//! Kept apart from `tests/common` so the backend-agnostic tests compile
//! without the Rex backend. Test files using it declare `mod common;` first.
#![allow(dead_code)]

pub mod fake_rex;
pub mod rex_script;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use worktree_sync::backends::rex::bindings::{Binding, Store};
use worktree_sync::backends::rex::settings::Settings;
use worktree_sync::backends::rex::workflows::{self, Context};
use worktree_sync::config::DEFAULT_REPO_CONFIG_PATH;

use crate::common::{Dir, git};
use fake_rex::{FakeRex, HOST_ID, Ids};

/// The binary path workflows put in the commands they launch.
pub const BIN: &str = "/opt/worktree-sync/bin/worktree-sync";

/// `BIN` followed by `args`, as a launched command.
pub fn launched(args: &[&str]) -> Vec<String> {
    std::iter::once(BIN)
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect()
}

/// Run git in `dir` and return its trimmed stdout, panicking on failure.
pub fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Commit everything in `dir`.
pub fn commit(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            message,
        ],
    );
}

/// A repository at `path` with one commit on `main`.
pub fn init_repo_at(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "core.excludesFile", "/dev/null"]);
    std::fs::write(path.join("README"), "app\n").unwrap();
    commit(path, "init");
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

/// A temp root (canonical, so paths compare equal to what git and the
/// workflows report), a fake Rex, settings, and a binding store.
pub struct Harness {
    _root: Dir,
    pub base: PathBuf,
    pub rex: FakeRex,
    pub settings: Settings,
    pub store: Store,
}

impl Harness {
    pub fn new() -> Self {
        let root = Dir::new();
        let base = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        let settings = Settings {
            worktree_directory: base.join("worktrees"),
            repo_config_path: PathBuf::from(DEFAULT_REPO_CONFIG_PATH),
        };
        let store = Store::new(base.join("state/worktree-sync/rex/bindings.json"))
            .with_lock_timeout(Duration::from_secs(2));
        Self {
            _root: root,
            base,
            rex: FakeRex::new(),
            settings,
            store,
        }
    }

    pub fn ctx(&self) -> Context<'_> {
        Context {
            rex: &self.rex,
            settings: &self.settings,
            bindings: &self.store,
            reports_dir: self.reports_dir(),
            bin: PathBuf::from(BIN),
            home: self.home(),
        }
    }

    pub fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    pub fn reports_dir(&self) -> PathBuf {
        self.base.join("state/worktree-sync/rex/reports")
    }

    /// The main checkout of a repo named `app`, one commit on `main`.
    pub fn repo(&self) -> PathBuf {
        let path = self.base.join("dev/app");
        init_repo_at(&path);
        path
    }

    /// A linked worktree of `repo` on a new branch `branch`, made with plain
    /// git outside the configured worktree directory.
    pub fn linked(&self, repo: &Path, branch: &str) -> PathBuf {
        let path = self.base.join("elsewhere").join(branch.replace('/', "-"));
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
        path
    }

    /// A live session labelled `label`, bound to `path` through the workflow.
    pub fn bound_session(&self, label: &str, path: &Path) -> Ids {
        let ids = self.rex.add_session(label);
        workflows::bind(&self.ctx(), &ids.session, path).expect("bind should succeed");
        ids
    }

    /// Bind `session` to `path` with an explicit `bound_at`, bypassing the
    /// workflow, to control which binding is the earliest.
    pub fn seed_binding(&self, session: &str, path: &Path, bound_at: u64) {
        let checkout = worktree_sync::git::checkout_at(path).unwrap();
        let binding = Binding {
            repo_key: checkout.repo_key,
            repo_name: checkout.repo_name,
            checkout_path: checkout.path,
            branch: checkout.branch,
            is_linked_worktree: checkout.is_linked_worktree,
            bound_at,
        };
        self.store
            .update(
                HOST_ID,
                || Err(anyhow::anyhow!("no pruning while seeding")),
                |host| {
                    host.sessions.insert(session.to_string(), binding);
                    Ok(())
                },
            )
            .unwrap();
    }

    /// This host's bindings as currently on disk.
    pub fn bindings(&self) -> BTreeMap<String, Binding> {
        self.store
            .read()
            .unwrap()
            .hosts
            .remove(HOST_ID)
            .map(|host| host.sessions)
            .unwrap_or_default()
    }

    /// Write the per-repo config into `repo`'s working tree.
    pub fn write_repo_config(&self, repo: &Path, toml: &str) {
        std::fs::write(repo.join(&self.settings.repo_config_path), toml).unwrap();
    }

    /// A worktree dir as `create_worktree` names it.
    pub fn worktree_path(&self, slug: &str) -> PathBuf {
        self.settings.worktree_directory.join("app").join(slug)
    }
}
