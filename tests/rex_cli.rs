//! The `worktree-sync rex …` binary as Rex and users run it: a scripted `rex`
//! on `REX_BIN_DIR`, an isolated state/config dir, and no terminal on stdin.

mod common;
mod rex_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::Dir;
use rex_support::fake_rex::HOST_ID;
use rex_support::rex_script::RexScript;
use rex_support::{branch_exists, init_repo_at};
use worktree_sync::backends::rex::bindings::{Binding, Store};

const EXE: &str = env!("CARGO_BIN_EXE_worktree-sync");
const SESSION: &str = "session:0001";
/// Rex's reply to the `session.create` of a new session.
const CREATED: &str = r#"{"session_id":"session:0002","initial_windows":[{"window_id":"w","block_ids":["b"],"revision":1}]}"#;

struct Env {
    _root: Dir,
    base: PathBuf,
    rex: RexScript,
}

impl Env {
    fn new() -> Self {
        let root = Dir::new();
        let base = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        let rex = RexScript::install(&base.join("rex-bin"));
        rex.reply_to("whoami", &format!(r#"{{"host_id":"{HOST_ID}"}}"#));
        rex.reply_to(
            "ls",
            &format!(r#"{{"sessions":[{{"session_id":"{SESSION}","label":"feat-a"}}]}}"#),
        );
        Self {
            _root: root,
            base,
            rex,
        }
    }

    fn store(&self) -> Store {
        Store::new(self.base.join("state/worktree-sync/rex/bindings.json"))
    }

    /// Run `worktree-sync args…` with stdin an open pipe that never delivers
    /// input: a command that waits for a prompt answer hangs and fails the
    /// test instead of reading EOF.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    /// [`Env::run`] with `vars` added to the environment.
    fn run_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut child = Command::new(EXE)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.base.join("home"))
            .env("XDG_STATE_HOME", self.base.join("state"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("REX_BIN_DIR", self.rex.dir())
            .envs(vars.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _stdin = child.stdin.take();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(20) {
                child.kill().unwrap();
                panic!(
                    "`worktree-sync {}` hung (waiting for input?)",
                    args.join(" ")
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }

    /// A repo with a linked worktree `feat/a`, bound to `SESSION`.
    fn bound_linked_worktree(&self) -> (PathBuf, PathBuf) {
        let repo = self.base.join("dev/app");
        init_repo_at(&repo);
        let linked = self.base.join("wt/feat-a");
        common::git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/a",
                linked.to_str().unwrap(),
            ],
        );
        let binding = Binding {
            repo_key: repo.join(".git"),
            repo_name: "app".into(),
            checkout_path: linked.clone(),
            branch: Some("feat/a".into()),
            is_linked_worktree: true,
            bound_at: 1,
        };
        self.store()
            .update(
                HOST_ID,
                || Ok(vec![SESSION.into()]),
                |h| {
                    h.sessions.insert(SESSION.into(), binding);
                    Ok(())
                },
            )
            .unwrap();
        (repo, linked)
    }

    /// Whether any Rex app was asked to switch sessions.
    fn switched(&self) -> bool {
        self.rex
            .invocations()
            .iter()
            .any(|argv| argv.iter().any(|a| a == "session.select"))
    }

    fn killed(&self) -> bool {
        self.rex
            .invocations()
            .iter()
            .any(|argv| argv.iter().any(|a| a == "kill"))
    }

    fn bound(&self) -> bool {
        self.store()
            .read()
            .unwrap()
            .hosts
            .get(HOST_ID)
            .is_some_and(|h| h.sessions.contains_key(SESSION))
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn usage_names_both_backends() {
    let env = Env::new();
    let output = env.run(&[]);
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("herdr") && text.contains("rex"), "{text}");
}

/// The Lua module ships inside the binary, so the two can't drift apart.
#[test]
fn rex_lua_prints_the_embedded_module() {
    let env = Env::new();
    let output = env.run(&["rex", "lua"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("../lua/worktree_sync.lua")
    );
}

/// What a key press runs: it must answer at once and never wait on stdin
/// (Rex holds the user's keystrokes until the action returns). Errors are
/// one line first, for Rex to show.
#[test]
fn the_action_entry_never_prompts() {
    let env = Env::new();

    let output = env.run(&["rex", "action", "open", "--session", SESSION]);

    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.lines()
            .next()
            .unwrap_or("")
            .to_lowercase()
            .contains("bind"),
        "{text}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn the_action_entry_prints_its_outcome_as_json() {
    let env = Env::new();
    env.bound_linked_worktree();
    env.rex.reply_to("do", CREATED);

    let output = env.run(&["rex", "action", "new", "--session", SESSION]);

    assert!(output.status.success(), "{}", stderr(&output));
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome, serde_json::json!({"select": "session:0002"}));
}

/// Without a terminal nobody can confirm, so remove refuses unless `--yes`
/// says the caller already did.
#[test]
fn remove_without_a_terminal_needs_yes() {
    let env = Env::new();
    let (repo, linked) = env.bound_linked_worktree();

    let output = env.run(&["rex", "remove", "--session", SESSION]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--yes"), "{}", stderr(&output));
    assert!(linked.exists());
    assert!(branch_exists(&repo, "feat/a"));
    assert!(env.bound());
    assert!(!env.killed());
}

/// `--yes` confirms the removal, not deleting uncommitted work: that needs
/// `--force` too.
#[test]
fn remove_yes_still_needs_force_for_a_dirty_worktree() {
    let env = Env::new();
    let (_repo, linked) = env.bound_linked_worktree();
    std::fs::write(linked.join("notes.txt"), "precious").unwrap();

    let output = env.run(&["rex", "remove", "--session", SESSION, "--yes"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--force"), "{}", stderr(&output));
    assert_eq!(
        std::fs::read_to_string(linked.join("notes.txt")).unwrap(),
        "precious"
    );
    assert!(env.bound());
    assert!(!env.killed());
}

#[test]
fn remove_yes_removes_unbinds_and_closes_the_session() {
    let env = Env::new();
    let (repo, linked) = env.bound_linked_worktree();

    let output = env.run(&["rex", "remove", "--session", SESSION, "--yes"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!linked.exists());
    assert!(branch_exists(&repo, "feat/a"));
    assert!(!env.bound());
    assert!(
        env.rex.invocations().contains(&vec![
            "--autostart=false".into(),
            "kill".into(),
            SESSION.into()
        ]),
        "{:?}",
        env.rex.invocations()
    );
}

#[test]
fn bind_and_unbind_from_the_cli() {
    let env = Env::new();
    let repo = env.base.join("dev/app");
    init_repo_at(&repo);
    std::fs::create_dir_all(repo.join("src")).unwrap();

    let output = env.run(&[
        "rex",
        "bind",
        repo.join("src").to_str().unwrap(),
        "--session",
        SESSION,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let file = env.store().read().unwrap();
    assert_eq!(
        file.hosts[HOST_ID].sessions[SESSION].checkout_path,
        Path::new(&repo)
    );

    let output = env.run(&["rex", "unbind", "--session", SESSION]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!env.bound());
}

/// `new --wait` is for agents and scripts: it returns once the worktree is set
/// up and prints only the new session as JSON on stdout; progress goes to
/// stderr. With `--no-switch`, every app stays where it is.
#[test]
fn new_wait_prints_only_the_set_up_session_as_json() {
    let env = Env::new();
    let (repo, _) = env.bound_linked_worktree();
    std::fs::write(
        repo.join(".worktree-sync.toml"),
        "[copy]\nenabled = true\nfiles = [\".env\"]\n",
    )
    .unwrap();
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    env.rex.reply_to("do", CREATED);

    let output = env.run(&[
        "rex",
        "new",
        "feat/b",
        "--from",
        SESSION,
        "--wait",
        "--no-switch",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    let path = env.base.join("home/.worktrees/app/feat-b");
    let made: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stdout)));
    assert_eq!(
        made,
        serde_json::json!({
            "session_id": "session:0002",
            "block_id": "b",
            "checkout_path": path,
            "branch": "feat/b",
        })
    );
    assert!(
        stderr(&output).contains("[copy] .env"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert!(!env.switched(), "{:?}", env.rex.invocations());
}

#[test]
fn new_wait_switches_to_the_set_up_session_by_default() {
    let env = Env::new();
    env.bound_linked_worktree();
    env.rex.reply_to("do", CREATED);

    let output = env.run(&["rex", "new", "feat/b", "--from", SESSION, "--wait"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let made: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(made["session_id"], "session:0002");
    let selects: Vec<_> = env
        .rex
        .invocations()
        .into_iter()
        .filter(|argv| argv.iter().any(|a| a == "session.select"))
        .collect();
    assert_eq!(selects.len(), 1, "{selects:?}");
    assert!(selects[0].contains(&"session_id=session:0002".to_string()));
}

/// Nobody can be asked for a branch, so `--wait` needs one up front.
#[test]
fn new_wait_needs_a_branch() {
    let env = Env::new();
    env.bound_linked_worktree();

    let output = env.run(&["rex", "new", "--from", SESSION, "--wait"]);

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(env.rex.invocations().is_empty());
    assert!(!env.base.join("home/.worktrees").exists());
}

/// Inside Rex, `open` normally just switches the app there; with `--no-switch`
/// it prints the session instead, for the agent or script that asked.
#[test]
fn open_no_switch_prints_the_session_and_leaves_the_apps_alone() {
    let env = Env::new();
    let (_, linked) = env.bound_linked_worktree();

    let output = env.run_with(
        &["rex", "open", linked.to_str().unwrap(), "--no-switch"],
        &[("REX_SESSION", "session:0009")],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{SESSION}\n")
    );
    assert!(!env.switched(), "{:?}", env.rex.invocations());
}

/// With a main-checkout session to go back to, `remove` normally switches the
/// app there; `--no-switch` leaves every app where it is.
#[test]
fn remove_no_switch_leaves_the_apps_alone() {
    let env = Env::new();
    let (repo, linked) = env.bound_linked_worktree();
    let parent = "session:0003";
    env.rex.reply_to(
        "ls",
        &format!(
            r#"{{"sessions":[{{"session_id":"{SESSION}","label":"feat-a"}},{{"session_id":"{parent}","label":"app"}}]}}"#
        ),
    );
    let output = env.run(&["rex", "bind", repo.to_str().unwrap(), "--session", parent]);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = env.run(&[
        "rex",
        "remove",
        "--session",
        SESSION,
        "--yes",
        "--no-switch",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!linked.exists());
    assert!(!env.switched(), "{:?}", env.rex.invocations());
}
