//! The Rex workflows end to end: a fake Rex server, real git repositories,
//! and the real binding file. Prompts are not part of these functions; tests
//! pass the answers a user would give.

mod common;
mod rex_support;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use common::git;
use rex_support::fake_rex::{Call, HOST_ID};
use rex_support::{BIN, Harness, branch_exists, commit, git_out, launched};
use worktree_sync::backends::rex::api::{Rex, Terminal};
use worktree_sync::backends::rex::bindings::{Binding, Store};
use worktree_sync::backends::rex::cli;
use worktree_sync::backends::rex::ui::{self, TOAST_BOUNDS};
use worktree_sync::backends::rex::workflows::{
    self as wf, ActionOutcome, ActionRequest, CreatedWorktree, Focus, NewLaunch, RemoveError,
    SetupOutcome, Switch, Verb,
};
use worktree_sync::git::generated_branch_slug;
use worktree_sync::report;

const CLIENT: &str = "client:app";

fn request(verb: Verb, session: &str) -> ActionRequest {
    ActionRequest {
        verb,
        session: session.into(),
        block: None,
        client: Some(CLIENT.into()),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn layers(calls: &[Call]) -> Vec<(String, Option<String>, Terminal, bool)> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::NewLayer {
                session,
                window,
                terminal,
                focus,
                ..
            } => Some((session.clone(), window.clone(), terminal.clone(), *focus)),
            _ => None,
        })
        .collect()
}

fn destroyed(calls: &[Call]) -> Vec<String> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Destroy { session } => Some(session.clone()),
            _ => None,
        })
        .collect()
}

fn terminal(cwd: &Path, command: Vec<String>) -> Terminal {
    Terminal {
        cwd: cwd.to_path_buf(),
        command: Some(command),
    }
}

// =============================================================================
// Action entry (what a key press runs)
// =============================================================================

/// Bound-only membership: nothing guesses a checkout from a cwd. Every verb
/// but bind tells the user to bind first, and touches nothing.
#[test]
fn actions_on_an_unbound_session_are_refused_except_bind() {
    let h = Harness::new();
    let s = h.rex.add_session("scratch");

    for verb in [Verb::Open, Verb::New, Verb::Remove, Verb::Sync] {
        let err = wf::action(&h.ctx(), &request(verb, &s.session)).unwrap_err();
        assert!(
            format!("{err:#}").to_lowercase().contains("bind"),
            "{verb:?}: {err:#}"
        );
    }
    assert_eq!(h.rex.calls(), []);
    assert!(h.bindings().is_empty());
}

#[test]
fn the_action_outcome_is_json_with_select_only_when_needed() {
    assert_eq!(
        serde_json::to_string(&ActionOutcome { select: None }).unwrap(),
        "{}"
    );
    assert_eq!(
        serde_json::to_string(&ActionOutcome {
            select: Some("session:1".into())
        })
        .unwrap(),
        r#"{"select":"session:1"}"#
    );
}

/// The checkout a session is bound to must still be there before anything is
/// launched for it.
#[test]
fn actions_on_a_session_whose_checkout_vanished_are_refused() {
    let h = Harness::new();
    let repo = h.repo();
    let linked = h.linked(&repo, "feat/gone");
    let s = h.bound_session("gone", &linked);
    std::fs::remove_dir_all(&linked).unwrap();

    for verb in [Verb::Open, Verb::New, Verb::Remove, Verb::Sync] {
        let err = wf::action(&h.ctx(), &request(verb, &s.session)).unwrap_err();
        assert!(
            format!("{err:#}").contains(linked.to_str().unwrap()),
            "{verb:?}: {err:#}"
        );
    }
    assert_eq!(h.rex.calls(), []);
}

/// Every command a workflow launches inside Rex must parse with the real CLI
/// definition; a renamed flag would otherwise only fail on a key press.
#[test]
fn every_command_launched_inside_rex_is_accepted_by_the_cli() {
    let h = Harness::new();
    let repo = h.repo();
    let linked = h.linked(&repo, "feat/a");
    let main = h.bound_session("app", &repo);
    let child = h.bound_session("feat-a", &linked);

    let mut req = request(Verb::Bind, &main.session);
    req.block = Some(main.block.clone());
    wf::action(&h.ctx(), &req).unwrap();
    wf::action(&h.ctx(), &request(Verb::Open, &main.session)).unwrap();
    wf::action(&h.ctx(), &request(Verb::New, &main.session)).unwrap();
    wf::action(&h.ctx(), &request(Verb::Remove, &child.session)).unwrap();
    wf::action(&h.ctx(), &request(Verb::Sync, &child.session)).unwrap();
    for launch in [
        NewLaunch {
            from: main.session.clone(),
            branch: Some("feat/b".into()),
            base: Some("main".into()),
            client: Some(CLIENT.into()),
        },
        NewLaunch {
            from: main.session.clone(),
            branch: None,
            base: None,
            client: None,
        },
    ] {
        wf::launch_new(&h.ctx(), &launch).unwrap();
    }

    let commands: Vec<Vec<String>> = h
        .rex
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::CreateSession { terminal, .. } | Call::NewLayer { terminal, .. } => {
                terminal.command
            }
            _ => None,
        })
        .collect();
    assert_eq!(commands.len(), 7, "{commands:?}");
    for command in commands {
        assert_eq!(command[0], BIN, "{command:?}");
        cli::command()
            .try_get_matches_from(&command[1..])
            .unwrap_or_else(|e| panic!("{command:?} rejected:\n{e}"));
    }
}

// =============================================================================
// bind / unbind
// =============================================================================

#[test]
fn the_bind_action_prompts_in_a_layer_starting_from_the_focused_blocks_cwd() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.rex.add_session("scratch");
    h.rex
        .set_foreground_cwd(&s.session, &s.block, repo.join("src"));

    let outcome = wf::action(
        &h.ctx(),
        &ActionRequest {
            verb: Verb::Bind,
            session: s.session.clone(),
            block: Some(s.block.clone()),
            client: Some(CLIENT.into()),
        },
    )
    .unwrap();

    assert_eq!(outcome.select, None);
    assert_eq!(
        layers(&h.rex.calls()),
        [(
            s.session.clone(),
            None,
            terminal(
                &repo.join("src"),
                launched(&["rex", "bind", "--session", &s.session])
            ),
            true
        )]
    );
    assert!(h.bindings().is_empty(), "the action itself binds nothing");
}

#[test]
fn the_bind_action_starts_from_home_without_a_known_cwd() {
    let h = Harness::new();
    let s = h.rex.add_session("scratch");

    let mut req = request(Verb::Bind, &s.session);
    req.block = Some(s.block.clone());
    wf::action(&h.ctx(), &req).unwrap();

    let (_, _, terminal, _) = layers(&h.rex.calls()).remove(0);
    assert_eq!(terminal.cwd, h.home());
}

#[test]
fn bind_records_the_work_tree_containing_the_path() {
    let h = Harness::new();
    let repo = h.repo();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let s = h.rex.add_session("my label");
    let before = now();

    let binding = wf::bind(&h.ctx(), &s.session, &repo.join("src")).unwrap();

    assert_eq!(binding.checkout_path, repo);
    assert_eq!(binding.repo_key, repo.join(".git"));
    assert_eq!(binding.repo_name, "app");
    assert_eq!(binding.branch.as_deref(), Some("main"));
    assert!(!binding.is_linked_worktree);
    assert!((before..=now()).contains(&binding.bound_at));
    assert_eq!(h.bindings()[&s.session], binding);
    assert_eq!(
        h.rex.calls(),
        [],
        "a user-made session keeps its label and layout"
    );
}

#[test]
fn bind_marks_a_linked_worktree_as_a_child() {
    let h = Harness::new();
    let repo = h.repo();
    let linked = h.linked(&repo, "feat/x");
    let s = h.rex.add_session("x");

    let binding = wf::bind(&h.ctx(), &s.session, &linked).unwrap();

    assert!(binding.is_linked_worktree);
    assert_eq!(binding.repo_key, repo.join(".git"));
    assert_eq!(binding.branch.as_deref(), Some("feat/x"));
}

#[test]
fn bind_refuses_a_path_outside_any_work_tree() {
    let h = Harness::new();
    let s = h.rex.add_session("x");
    let plain = h.base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    assert!(wf::bind(&h.ctx(), &s.session, &plain).is_err());
    assert!(wf::bind(&h.ctx(), &s.session, &h.base.join("missing")).is_err());
    assert!(h.bindings().is_empty());
}

#[test]
fn rebinding_replaces_the_previous_binding() {
    let h = Harness::new();
    let repo = h.repo();
    let linked = h.linked(&repo, "feat/x");
    let s = h.bound_session("x", &repo);

    wf::bind(&h.ctx(), &s.session, &linked).unwrap();

    let bindings = h.bindings();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[&s.session].checkout_path, linked);
}

#[test]
fn unbind_drops_only_that_session() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.bound_session("a", &repo);
    let b = h.bound_session("b", &repo);

    let removed = wf::unbind(&h.ctx(), &a.session).unwrap();

    assert_eq!(removed.map(|b| b.checkout_path), Some(repo.clone()));
    assert_eq!(h.bindings().keys().collect::<Vec<_>>(), [&b.session]);
    assert_eq!(wf::unbind(&h.ctx(), &a.session).unwrap(), None);
}

#[test]
fn binding_prunes_sessions_that_no_longer_exist() {
    let h = Harness::new();
    let repo = h.repo();
    let gone = h.bound_session("gone", &repo);
    h.rex.kill_externally(&gone.session);

    let kept = h.bound_session("kept", &repo);

    assert_eq!(h.bindings().keys().collect::<Vec<_>>(), [&kept.session]);
}

#[test]
fn binding_without_a_session_list_prunes_nothing() {
    let h = Harness::new();
    let repo = h.repo();
    let maybe_gone = h.bound_session("maybe", &repo);
    h.rex.kill_externally(&maybe_gone.session);
    h.rex.fail_session_listing();
    let s = h.rex.add_session("new");

    wf::bind(&h.ctx(), &s.session, &repo).unwrap();

    assert!(h.bindings().contains_key(&maybe_gone.session));
    assert!(h.bindings().contains_key(&s.session));
}

#[test]
fn other_hosts_bindings_are_left_alone() {
    let h = Harness::new();
    let repo = h.repo();
    let foreign = Binding {
        repo_key: "/remote/app/.git".into(),
        repo_name: "app".into(),
        checkout_path: "/remote/app".into(),
        branch: Some("main".into()),
        is_linked_worktree: false,
        bound_at: 1,
    };
    h.store
        .update(
            "host-remote",
            || Ok(vec![]),
            |host| {
                host.sessions
                    .insert("session:remote".into(), foreign.clone());
                Ok(())
            },
        )
        .unwrap();

    h.bound_session("local", &repo);

    let file = h.store.read().unwrap();
    assert_eq!(
        file.hosts["host-remote"].sessions["session:remote"],
        foreign
    );
    assert_eq!(file.hosts[HOST_ID].sessions.len(), 1);
}

// =============================================================================
// open
// =============================================================================

#[test]
fn the_open_action_shows_a_picker_in_the_bound_session() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    let outcome = wf::action(&h.ctx(), &request(Verb::Open, &s.session)).unwrap();

    assert_eq!(outcome.select, None);
    assert_eq!(
        layers(&h.rex.calls()),
        [(
            s.session.clone(),
            None,
            terminal(
                &repo,
                launched(&["rex", "open", "--from", &s.session, "--client", CLIENT])
            ),
            true
        )]
    );
}

#[test]
fn the_open_action_without_a_client_passes_none() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);
    let mut req = request(Verb::Open, &s.session);
    req.client = None;

    wf::action(&h.ctx(), &req).unwrap();

    let (_, _, terminal, _) = layers(&h.rex.calls()).remove(0);
    assert_eq!(
        terminal.command.unwrap(),
        launched(&["rex", "open", "--from", &s.session])
    );
}

/// (path, label, branch, is_linked_worktree, bound session ids)
type CandidateRow = (PathBuf, String, Option<String>, bool, Vec<String>);

/// Herdr parity: candidates come in `git worktree list` order (main checkout
/// first, then linked worktrees as git lists them, by path), and bare and
/// prunable entries are not offered. Bound live sessions are shown next to
/// their checkout; dead ones are not.
#[test]
fn open_candidates_are_the_repos_live_worktrees_with_their_sessions() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    let detached = h.base.join("elsewhere/detached");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ],
    );
    let stale = h.linked(&repo, "feat/stale");
    std::fs::remove_dir_all(&stale).unwrap();
    let from = h.bound_session("app", &repo);
    let on_a = h.bound_session("working on a", &a);
    let dead = h.bound_session("dead", &a);
    h.rex.kill_externally(&dead.session);

    let candidates = wf::open_candidates(&h.ctx(), &from.session).unwrap();

    let summary: Vec<_> = candidates
        .iter()
        .map(|c| {
            (
                c.checkout_path.clone(),
                c.label.clone(),
                c.branch.clone(),
                c.is_linked_worktree,
                c.sessions
                    .iter()
                    .map(|s| s.session_id.clone())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let expected: Vec<CandidateRow> = vec![
        (
            repo.clone(),
            "main".into(),
            Some("main".into()),
            false,
            vec![from.session.clone()],
        ),
        (
            detached.canonicalize().unwrap(),
            "detached".into(),
            None,
            true,
            vec![],
        ),
        (
            a.clone(),
            "feat/a".into(),
            Some("feat/a".into()),
            true,
            vec![on_a.session.clone()],
        ),
    ];
    assert_eq!(summary, expected);
    assert_eq!(candidates[2].sessions[0].label, "working on a");
}

#[test]
fn open_candidates_exclude_the_bare_repository() {
    let h = Harness::new();
    let src = h.repo();
    let bare = h.base.join("proj/.bare");
    assert!(
        std::process::Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(&src)
            .arg(&bare)
            .status()
            .unwrap()
            .success()
    );
    let main = h.base.join("proj/main");
    git(
        &bare,
        &["worktree", "add", "-q", main.to_str().unwrap(), "main"],
    );
    let from = h.bound_session("proj", &main);

    let candidates = wf::open_candidates(&h.ctx(), &from.session).unwrap();

    assert_eq!(
        candidates
            .iter()
            .map(|c| c.checkout_path.clone())
            .collect::<Vec<_>>(),
        [main]
    );
}

#[test]
fn opening_an_unbound_checkout_creates_binds_and_selects_a_session() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    let from = h.bound_session("app", &repo);

    let opened =
        wf::open_checkout(&h.ctx(), &a, Some(&from.session), Switch::App(Some(CLIENT))).unwrap();

    assert!(opened.created);
    assert_eq!(opened.focus, Focus::Switched);
    let calls = h.rex.calls();
    let Call::CreateSession {
        session,
        label,
        terminal,
        ..
    } = &calls[0]
    else {
        panic!("{calls:?}")
    };
    assert_eq!(session, &opened.session_id);
    assert_eq!(
        label, "feat-a",
        "Herdr's automatic label: the checkout's basename"
    );
    assert_eq!(
        terminal,
        &Terminal {
            cwd: a.clone(),
            command: None
        },
        "an interactive login shell in the checkout"
    );
    assert_eq!(
        calls[1..],
        [Call::Select {
            client: Some(CLIENT.into()),
            session: opened.session_id.clone()
        }]
    );
    let binding = &h.bindings()[&opened.session_id];
    assert_eq!(binding.checkout_path, a);
    assert!(binding.is_linked_worktree);
}

/// `rex open PATH` works for any work tree, including the main checkout.
#[test]
fn opening_a_main_checkout_binds_it_as_the_parent() {
    let h = Harness::new();
    let repo = h.repo();

    let opened = wf::open_checkout(&h.ctx(), &repo.join("."), None, Switch::App(None)).unwrap();

    assert!(opened.created);
    let binding = &h.bindings()[&opened.session_id];
    assert_eq!(binding.checkout_path, repo);
    assert!(!binding.is_linked_worktree);
}

#[test]
fn opening_a_bound_checkout_selects_its_earliest_session() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    let late = h.rex.add_session("late");
    let early = h.rex.add_session("early");
    h.seed_binding(&late.session, &a, 200);
    h.seed_binding(&early.session, &a, 100);

    let opened = wf::open_checkout(&h.ctx(), &a, None, Switch::App(Some(CLIENT))).unwrap();

    assert!(!opened.created);
    assert_eq!(opened.session_id, early.session);
    assert_eq!(
        h.rex.calls(),
        [Call::Select {
            client: Some(CLIENT.into()),
            session: early.session.clone()
        }]
    );
}

/// Same-second bindings are ordered by session id, so every reader agrees.
#[test]
fn equal_bind_times_are_ordered_by_session_id() {
    let h = Harness::new();
    let repo = h.repo();
    let first = h.rex.add_session("first");
    let second = h.rex.add_session("second");
    h.seed_binding(&second.session, &repo, 100);
    h.seed_binding(&first.session, &repo, 100);
    assert!(first.session < second.session);

    let opened = wf::open_checkout(&h.ctx(), &repo, None, Switch::App(None)).unwrap();

    assert_eq!(opened.session_id, first.session);
}

#[test]
fn opening_the_current_sessions_checkout_switches_nothing() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    let opened =
        wf::open_checkout(&h.ctx(), &repo, Some(&s.session), Switch::App(Some(CLIENT))).unwrap();

    assert_eq!(opened.session_id, s.session);
    assert_eq!(opened.focus, Focus::NotNeeded);
    assert_eq!(h.rex.calls(), []);
}

/// Focus is best effort (Remote Control may be off): the session is still
/// created and bound, and the user is told where to go.
#[test]
fn a_refused_switch_does_not_fail_open() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    h.rex.refuse_select("Turn on Remote Control");

    let opened = wf::open_checkout(&h.ctx(), &a, None, Switch::App(Some(CLIENT))).unwrap();

    match &opened.focus {
        Focus::Refused(message) => assert!(message.contains("Turn on Remote Control"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(h.rex.is_live(&opened.session_id));
    assert!(h.bindings().contains_key(&opened.session_id));
}

#[test]
fn opening_a_path_outside_any_work_tree_creates_nothing() {
    let h = Harness::new();
    let plain = h.base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    assert!(wf::open_checkout(&h.ctx(), &plain, None, Switch::App(None)).is_err());
    assert_eq!(h.rex.calls(), []);
    assert!(h.bindings().is_empty());
}

/// For agents and scripts: the session is made and bound, and every app stays
/// where it is.
#[test]
fn opening_without_switching_leaves_the_apps_alone() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");

    let opened = wf::open_checkout(&h.ctx(), &a, None, Switch::Off).unwrap();

    assert!(opened.created);
    assert_eq!(opened.focus, Focus::NotNeeded);
    let calls = h.rex.calls();
    assert!(
        matches!(calls.as_slice(), [Call::CreateSession { .. }]),
        "{calls:?}"
    );
    assert_eq!(h.bindings()[&opened.session_id].checkout_path, a);
}

// =============================================================================
// new
// =============================================================================

/// Session-first: the prompt runs in the session that will own the worktree,
/// and the action asks Lua to switch to it.
#[test]
fn the_new_action_launches_the_prompt_in_a_new_session_and_selects_it() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    let outcome = wf::action(&h.ctx(), &request(Verb::New, &s.session)).unwrap();

    let calls = h.rex.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let Call::CreateSession {
        session,
        label,
        window_label,
        terminal: t,
    } = &calls[0]
    else {
        panic!("{calls:?}")
    };
    assert_eq!(label, "new worktree");
    assert_eq!(window_label, "new worktree");
    assert_eq!(
        t,
        &terminal(
            &repo,
            launched(&[
                "rex", "new", "--from", &s.session, "--here", "--client", CLIENT
            ])
        )
    );
    assert_eq!(outcome.select.as_ref(), Some(session));
    assert!(
        !h.bindings().contains_key(session),
        "bound only once a worktree exists"
    );
}

/// Decision 5: new is allowed from a linked-worktree session.
#[test]
fn the_new_action_works_from_a_linked_worktree_session() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    let s = h.bound_session("feat-a", &a);

    let outcome = wf::action(&h.ctx(), &request(Verb::New, &s.session)).unwrap();

    assert!(outcome.select.is_some());
}

#[test]
fn launch_new_passes_the_given_answers_through() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    let new = wf::launch_new(
        &h.ctx(),
        &NewLaunch {
            from: s.session.clone(),
            branch: Some("feat/x".into()),
            base: Some("dev".into()),
            client: None,
        },
    )
    .unwrap();

    assert_eq!(
        h.rex.calls(),
        [Call::CreateSession {
            session: new.session_id.clone(),
            label: "new worktree".into(),
            window_label: "new worktree".into(),
            terminal: terminal(
                &repo,
                launched(&[
                    "rex", "new", "feat/x", "--base", "dev", "--from", &s.session, "--here"
                ])
            ),
        }]
    );
}

#[test]
fn new_defaults_are_a_generated_branch_from_head() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    let defaults = wf::new_defaults(&h.ctx(), &s.session, 9).unwrap();

    assert_eq!(defaults.branch, generated_branch_slug(9));
    assert_eq!(defaults.base, "HEAD");
    assert_eq!(defaults.repo_name, "app");
}

#[test]
fn create_worktree_makes_a_new_branch_at_the_configured_path() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    let created = wf::create_worktree(&h.ctx(), &s.session, "feat/login", "HEAD").unwrap();

    assert_eq!(
        created,
        CreatedWorktree {
            path: h.worktree_path("feat-login"),
            branch: "feat/login".into(),
            new_branch: true,
        }
    );
    assert_eq!(
        git_out(&created.path, &["rev-parse", "HEAD"]),
        git_out(&repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git_out(&created.path, &["branch", "--show-current"]),
        "feat/login"
    );
    assert_eq!(h.bindings().len(), 1, "binding is adopt_worktree's job");
}

#[test]
fn create_worktree_checks_out_an_existing_branch() {
    let h = Harness::new();
    let repo = h.repo();
    git(&repo, &["branch", "existing"]);
    let tip = git_out(&repo, &["rev-parse", "existing"]);
    commit(&repo, "main moves on");
    let s = h.bound_session("app", &repo);

    let created = wf::create_worktree(&h.ctx(), &s.session, "existing", "HEAD").unwrap();

    assert!(!created.new_branch);
    assert_eq!(git_out(&created.path, &["rev-parse", "HEAD"]), tip);
}

/// `HEAD` is the origin session's checkout's HEAD, and the path still uses
/// the repository's name.
#[test]
fn create_worktree_from_a_linked_session_starts_from_its_head() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    commit(&a, "work on a");
    let s = h.bound_session("feat-a", &a);

    let created = wf::create_worktree(&h.ctx(), &s.session, "feat/b", "HEAD").unwrap();

    assert_eq!(created.path, h.worktree_path("feat-b"));
    assert_eq!(
        git_out(&created.path, &["rev-parse", "HEAD"]),
        git_out(&a, &["rev-parse", "HEAD"])
    );
}

#[test]
fn create_worktree_refuses_an_existing_path() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);
    let taken = h.worktree_path("feat-x");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("mine"), "keep").unwrap();

    assert!(wf::create_worktree(&h.ctx(), &s.session, "feat/x", "HEAD").is_err());

    assert!(!branch_exists(&repo, "feat/x"));
    assert_eq!(std::fs::read_to_string(taken.join("mine")).unwrap(), "keep");
}

#[test]
fn create_worktree_refuses_an_invalid_branch_name() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    assert!(wf::create_worktree(&h.ctx(), &s.session, "bad..name", "HEAD").is_err());
    assert!(!h.settings.worktree_directory.exists());
}

/// Fixture for the steps after the prompt: origin session `from` on the main
/// checkout, session `n` (where `rex new --here` runs) with its first block,
/// and a freshly created worktree.
struct NewFixture {
    h: Harness,
    repo: PathBuf,
    from: String,
    n: rex_support::fake_rex::Ids,
    created: CreatedWorktree,
}

fn new_fixture(config: Option<&str>, branch: &str) -> NewFixture {
    let h = Harness::new();
    let repo = h.repo();
    if let Some(config) = config {
        h.write_repo_config(&repo, config);
    }
    let from = h.bound_session("app", &repo).session;
    let n = h.rex.add_session("new worktree");
    let created = wf::create_worktree(&h.ctx(), &from, branch, "HEAD").unwrap();
    NewFixture {
        h,
        repo,
        from,
        n,
        created,
    }
}

#[test]
fn adopting_a_worktree_binds_relabels_and_opens_a_usable_shell() {
    let f = new_fixture(None, "feat/login");

    let adopted = wf::adopt_worktree(&f.h.ctx(), &f.n.session, &f.n.block, &f.created).unwrap();

    let binding = &f.h.bindings()[&f.n.session];
    assert_eq!(binding.checkout_path, f.created.path);
    assert!(binding.is_linked_worktree);
    assert_eq!(binding.branch.as_deref(), Some("feat/login"));

    assert_eq!(adopted.setup_window, f.n.window);
    let calls = f.h.rex.calls();
    assert!(calls.contains(&Call::SetSessionLabel {
        session: f.n.session.clone(),
        label: "feat-login".into()
    }));
    assert!(calls.contains(&Call::SetWindowLabel {
        session: f.n.session.clone(),
        window: f.n.window.clone(),
        label: "setup".into()
    }));
    let shell: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            Call::NewWindow {
                session,
                window,
                terminal,
                focus,
                ..
            } => Some((session.clone(), window.clone(), terminal.clone(), *focus)),
            _ => None,
        })
        .collect();
    assert_eq!(
        shell,
        [(
            f.n.session.clone(),
            adopted.shell_window.clone(),
            Terminal {
                cwd: f.created.path.clone(),
                command: None
            },
            true
        )],
        "one focused interactive shell in the new worktree"
    );
    assert!(!destroyed(&calls).contains(&f.n.session));
}

/// If the binding can't be written, the worktree is kept (the user binds it
/// by hand) rather than lost.
#[test]
fn a_failed_adoption_keeps_the_worktree() {
    let f = new_fixture(None, "feat/login");
    std::fs::write(f.h.store.path(), "{ corrupt").unwrap();

    assert!(wf::adopt_worktree(&f.h.ctx(), &f.n.session, &f.n.block, &f.created).is_err());

    assert!(f.created.path.exists());
    assert!(branch_exists(&f.repo, "feat/login"));
    assert!(destroyed(&f.h.rex.calls()).is_empty());
}

const HOOK_OK: &str = r#"
[[hooks.post]]
command = ["sh", "-c", "echo ran > hook.log"]
"#;

fn failing(action: &str, notify: &str) -> String {
    format!(
        r#"
[[hooks.post]]
command = ["sh", "-c", "echo boom >&2; exit 3"]

[failure]
action = "{action}"

[notify]
when = "{notify}"
"#
    )
}

fn adopt(f: &NewFixture) -> wf::Adopted {
    wf::adopt_worktree(&f.h.ctx(), &f.n.session, &f.n.block, &f.created).unwrap()
}

#[test]
fn a_successful_setup_runs_the_lifecycle_and_toasts_in_the_shell_tab() {
    let f = new_fixture(Some(HOOK_OK), "feat/login");
    let adopted = adopt(&f);
    let before = f.h.rex.calls().len();

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    let SetupOutcome::Succeeded(summary) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(summary.hooks_run, 1);
    assert_eq!(
        std::fs::read_to_string(f.created.path.join("hook.log")).unwrap(),
        "ran\n",
        "hooks run in the new worktree, config read from the main checkout"
    );
    let calls = f.h.rex.calls()[before..].to_vec();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let Call::NewLayer {
        session,
        window,
        bounds,
        terminal,
        focus,
    } = &calls[0]
    else {
        panic!("{calls:?}")
    };
    assert_eq!(session, &f.n.session);
    assert_eq!(window.as_ref(), Some(&adopted.shell_window));
    assert_eq!(*bounds, TOAST_BOUNDS);
    assert!(!focus, "a toast never takes focus from the shell");
    let command = terminal.command.as_ref().unwrap();
    assert!(
        command
            .iter()
            .any(|a| a == "Worktree sync done · feat/login"),
        "{command:?}"
    );
}

#[test]
fn a_repo_without_config_sets_up_silently() {
    let f = new_fixture(None, "feat/login");
    let adopted = adopt(&f);
    let before = f.h.rex.calls().len();

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    assert!(
        matches!(outcome, SetupOutcome::Succeeded(ref s) if s.is_empty()),
        "{outcome:?}"
    );
    assert_eq!(f.h.rex.calls().len(), before, "no toast for a no-op run");
}

#[test]
fn notify_failure_suppresses_the_success_toast() {
    let f = new_fixture(
        Some(&format!("{HOOK_OK}\n[notify]\nwhen = \"failure\"\n")),
        "feat/login",
    );
    let adopted = adopt(&f);
    let before = f.h.rex.calls().len();

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    assert!(matches!(outcome, SetupOutcome::Succeeded(_)), "{outcome:?}");
    assert_eq!(f.h.rex.calls().len(), before);
}

/// `failure.action = keep`: everything stays, the report is written, and the
/// setup tab (which shows it) is brought forward.
#[test]
fn a_failed_setup_with_keep_leaves_everything_and_focuses_the_report() {
    let f = new_fixture(Some(&failing("keep", "always")), "feat/login");
    let adopted = adopt(&f);
    let before = f.h.rex.calls().len();

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    let SetupOutcome::Failed {
        error,
        report,
        announce,
        rolled_back,
    } = outcome
    else {
        panic!("{outcome:?}")
    };
    assert!(announce);
    assert!(!rolled_back);
    assert!(error.contains("boom") || error.contains('3'), "{error}");
    let report = report.expect("a report file");
    assert_eq!(
        report,
        f.h.reports_dir()
            .join(report::report_file_name("feat/login"))
    );
    assert!(
        std::fs::read_to_string(&report)
            .unwrap()
            .contains("feat/login")
    );

    assert!(f.created.path.exists());
    assert!(branch_exists(&f.repo, "feat/login"));
    assert!(f.h.bindings().contains_key(&f.n.session));
    assert_eq!(
        f.h.rex.calls()[before..],
        [Call::FocusWindow {
            session: f.n.session.clone(),
            window: adopted.setup_window.clone()
        }]
    );
}

#[test]
fn notify_never_keeps_a_failure_quiet() {
    let f = new_fixture(Some(&failing("keep", "never")), "feat/login");
    let adopted = adopt(&f);

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    assert!(
        matches!(
            outcome,
            SetupOutcome::Failed {
                announce: false,
                ..
            }
        ),
        "{outcome:?}"
    );
}

/// Snapshot of what exists when a Rex request is made.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    call: Call,
    worktree_exists: bool,
    branch_exists: bool,
    bound: bool,
}

fn watch(f: &NewFixture) -> Rc<RefCell<Vec<Seen>>> {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (log, path, repo, store, session) = (
        seen.clone(),
        f.created.path.clone(),
        f.repo.clone(),
        f.h.store.path().to_path_buf(),
        f.n.session.clone(),
    );
    f.h.rex.observe(move |call| {
        let bound = Store::new(&store)
            .read()
            .unwrap()
            .hosts
            .get(HOST_ID)
            .is_some_and(|h| h.sessions.contains_key(&session));
        log.borrow_mut().push(Seen {
            call: call.clone(),
            worktree_exists: path.exists(),
            branch_exists: branch_exists(&repo, "feat/login"),
            bound,
        });
    });
    seen
}

/// `failure.action = remove`, Herdr's order: report first (in the origin
/// session, which outlives this one), then remove the worktree, delete the
/// branch this run created, unbind, and destroy the own session last.
#[test]
fn a_failed_setup_with_remove_reports_then_rolls_back_then_closes_itself() {
    let f = new_fixture(Some(&failing("remove", "always")), "feat/login");
    let adopted = adopt(&f);
    let seen = watch(&f);

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    let SetupOutcome::Failed {
        report,
        rolled_back,
        ..
    } = outcome
    else {
        panic!("{outcome:?}")
    };
    assert!(rolled_back);
    let report = report.expect("a report file");
    assert!(report.exists(), "the report outlives the worktree");

    let seen = seen.borrow().clone();
    assert_eq!(seen.len(), 2, "{seen:#?}");
    let Call::NewLayer {
        session,
        terminal,
        focus,
        ..
    } = &seen[0].call
    else {
        panic!("the report comes first: {seen:#?}")
    };
    assert_eq!(
        session, &f.from,
        "shown in the origin session, which survives"
    );
    assert_eq!(
        terminal.command.as_ref(),
        Some(&ui::report_command(&report))
    );
    assert!(focus);
    assert!(
        seen[0].worktree_exists && seen[0].branch_exists && seen[0].bound,
        "the report is shown before anything is torn down: {seen:#?}"
    );
    assert_eq!(
        seen[1],
        Seen {
            call: Call::Destroy {
                session: f.n.session.clone()
            },
            worktree_exists: false,
            branch_exists: false,
            bound: false,
        },
        "the own session goes last, after git and the binding are cleaned up"
    );
}

/// Rollback only deletes a branch it created.
#[test]
fn rollback_keeps_a_branch_that_existed_before() {
    let h = Harness::new();
    let repo = h.repo();
    h.write_repo_config(&repo, &failing("remove", "always"));
    git(&repo, &["branch", "feat/login"]);
    let from = h.bound_session("app", &repo).session;
    let n = h.rex.add_session("new worktree");
    let created = wf::create_worktree(&h.ctx(), &from, "feat/login", "HEAD").unwrap();
    assert!(!created.new_branch);
    let adopted = wf::adopt_worktree(&h.ctx(), &n.session, &n.block, &created).unwrap();

    let outcome = wf::run_setup(&h.ctx(), &n.session, &from, &created, Some(&adopted));

    assert!(
        matches!(
            outcome,
            SetupOutcome::Failed {
                rolled_back: true,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(!created.path.exists());
    assert!(branch_exists(&repo, "feat/login"));
}

/// `git branch -d`, never `-D`: commits made during setup survive, even though
/// the (dirty) worktree itself is force-removed as configured.
#[test]
fn rollback_keeps_a_branch_with_unmerged_commits() {
    let config = r#"
[[hooks.post]]
command = ["sh", "-c", "echo x > made.txt && git add made.txt && git -c user.email=t@e -c user.name=t -c commit.gpgsign=false commit -qm wip && echo junk > untracked.txt && exit 1"]

[failure]
action = "remove"
"#;
    let f = new_fixture(Some(config), "feat/login");
    let adopted = adopt(&f);

    let outcome = wf::run_setup(
        &f.h.ctx(),
        &f.n.session,
        &f.from,
        &f.created,
        Some(&adopted),
    );

    assert!(
        matches!(
            outcome,
            SetupOutcome::Failed {
                rolled_back: true,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(!f.created.path.exists());
    assert!(branch_exists(&f.repo, "feat/login"));
    assert!(!f.h.bindings().contains_key(&f.n.session));
    assert!(!f.h.rex.is_live(&f.n.session));
}

/// Esc at the prompt: nothing was created, so the session just closes.
#[test]
fn cancelling_new_closes_its_session_and_creates_nothing() {
    let h = Harness::new();
    let repo = h.repo();
    h.bound_session("app", &repo);
    let n = h.rex.add_session("new worktree");
    let bindings_before = h.bindings();

    wf::cancel_new(&h.ctx(), &n.session).unwrap();

    assert_eq!(
        h.rex.calls(),
        [Call::Destroy {
            session: n.session.clone()
        }]
    );
    assert!(!h.settings.worktree_directory.exists());
    assert_eq!(h.bindings(), bindings_before);
}

// --- new --wait (agents and scripts) -------------------------------------------

/// The worktree, a session in it with one shell, bound to it, and the setup run
/// before returning. Rex shows nothing more: no setup tab, toast, or switch.
#[test]
fn new_worktree_sets_up_a_bound_session_and_shows_nothing_else() {
    let h = Harness::new();
    let repo = h.repo();
    h.write_repo_config(&repo, HOOK_OK);
    let from = h.bound_session("app", &repo).session;

    let made = wf::new_worktree(&h.ctx(), &from, "feat/login", None).unwrap();

    let path = h.worktree_path("feat-login");
    assert_eq!(made.checkout_path, path);
    assert_eq!(made.branch, "feat/login");
    assert_eq!(
        std::fs::read_to_string(path.join("hook.log")).unwrap(),
        "ran\n",
        "the setup ran before returning"
    );
    assert_eq!(
        h.rex.calls(),
        [Call::CreateSession {
            session: made.session_id.clone(),
            label: "feat-login".into(),
            window_label: "shell".into(),
            terminal: Terminal {
                cwd: path.clone(),
                command: None
            },
        }]
    );
    assert!(
        h.rex
            .window_of_block(&made.session_id, &made.block_id)
            .is_ok(),
        "the block is the session's shell"
    );
    let binding = &h.bindings()[&made.session_id];
    assert_eq!(binding.checkout_path, path);
    assert!(binding.is_linked_worktree);
}

#[test]
fn new_worktree_starts_a_new_branch_at_the_base_or_the_origin_head() {
    let h = Harness::new();
    let repo = h.repo();
    let first = git_out(&repo, &["rev-parse", "HEAD"]);
    commit(&repo, "second");
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    let from = h.bound_session("app", &repo).session;

    let at_base = wf::new_worktree(&h.ctx(), &from, "feat/old", Some(&first)).unwrap();
    let at_head = wf::new_worktree(&h.ctx(), &from, "feat/new", None).unwrap();

    assert_eq!(
        git_out(&at_base.checkout_path, &["rev-parse", "HEAD"]),
        first
    );
    assert_eq!(
        git_out(&at_head.checkout_path, &["rev-parse", "HEAD"]),
        head
    );
}

#[test]
fn new_worktree_with_a_bad_branch_creates_nothing() {
    let h = Harness::new();
    let repo = h.repo();
    let from = h.bound_session("app", &repo).session;

    assert!(wf::new_worktree(&h.ctx(), &from, "bad..name", None).is_err());

    assert_eq!(h.rex.calls(), []);
    assert!(!h.settings.worktree_directory.exists());
}

/// A failed setup follows `[failure] action` as in the setup tab, but tells the
/// caller instead of showing anything. With `keep`, the worktree and its
/// session stay, and the error says where they and the report are.
#[test]
fn a_failed_new_worktree_with_keep_stays_and_the_error_says_where() {
    let h = Harness::new();
    let repo = h.repo();
    h.write_repo_config(&repo, &failing("keep", "always"));
    let from = h.bound_session("app", &repo).session;

    let err = wf::new_worktree(&h.ctx(), &from, "feat/login", None).unwrap_err();

    let calls = h.rex.calls();
    let [Call::CreateSession { session, .. }] = calls.as_slice() else {
        panic!("only the session is made: {calls:?}")
    };
    let path = h.worktree_path("feat-login");
    let report = h.reports_dir().join(report::report_file_name("feat/login"));
    assert!(report.exists());
    let message = format!("{err:#}");
    for part in [
        session.clone(),
        path.display().to_string(),
        report.display().to_string(),
    ] {
        assert!(message.contains(&part), "{part} missing from: {message}");
    }
    assert!(h.rex.is_live(session));
    assert_eq!(h.bindings()[session].checkout_path, path);
}

/// With `remove`, the worktree, the branch this run created, the binding and
/// the session go; the report stays, and the error names it.
#[test]
fn a_failed_new_worktree_with_remove_is_rolled_back_and_names_the_report() {
    let h = Harness::new();
    let repo = h.repo();
    h.write_repo_config(&repo, &failing("remove", "always"));
    let from = h.bound_session("app", &repo).session;

    let err = wf::new_worktree(&h.ctx(), &from, "feat/login", None).unwrap_err();

    let calls = h.rex.calls();
    let [
        Call::CreateSession { session, .. },
        Call::Destroy { session: destroyed },
    ] = calls.as_slice()
    else {
        panic!("the session is made, then closed; nothing is shown: {calls:?}")
    };
    assert_eq!(session, destroyed);
    assert!(!h.worktree_path("feat-login").exists());
    assert!(!branch_exists(&repo, "feat/login"));
    assert!(!h.bindings().contains_key(session));
    let report = h.reports_dir().join(report::report_file_name("feat/login"));
    assert!(report.exists());
    assert!(
        format!("{err:#}").contains(&report.display().to_string()),
        "{err:#}"
    );
}

// =============================================================================
// remove
// =============================================================================

#[test]
fn the_remove_action_confirms_in_a_layer() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    let s = h.bound_session("feat-a", &a);

    let outcome = wf::action(&h.ctx(), &request(Verb::Remove, &s.session)).unwrap();

    assert_eq!(outcome.select, None);
    assert_eq!(
        layers(&h.rex.calls()),
        [(
            s.session.clone(),
            None,
            terminal(
                &a,
                launched(&["rex", "remove", "--session", &s.session, "--client", CLIENT])
            ),
            true
        )]
    );
}

/// Herdr parity: the main checkout is never a remove (or sync) target.
#[test]
fn remove_and_sync_refuse_the_main_checkout() {
    let h = Harness::new();
    let repo = h.repo();
    let s = h.bound_session("app", &repo);

    assert!(wf::action(&h.ctx(), &request(Verb::Remove, &s.session)).is_err());
    assert!(wf::action(&h.ctx(), &request(Verb::Sync, &s.session)).is_err());
    assert!(wf::plan_remove(&h.ctx(), &s.session).is_err());
    assert!(wf::sync(&h.ctx(), &s.session).is_err());
    assert_eq!(h.rex.calls(), []);
    assert!(repo.exists());
}

struct RemoveFixture {
    h: Harness,
    repo: PathBuf,
    linked: PathBuf,
    current: String,
    sibling: String,
    parent: String,
    later_parent: String,
    unrelated: String,
    unbound: String,
}

/// `current` and `sibling` bound to the linked worktree; two sessions on the
/// main checkout (`parent` bound earlier); one session on another worktree;
/// one unbound session.
fn remove_fixture() -> RemoveFixture {
    let h = Harness::new();
    let repo = h.repo();
    let linked = h.linked(&repo, "feat/a");
    let other = h.linked(&repo, "feat/other");
    let current = h.rex.add_session("feat-a").session;
    let sibling = h.rex.add_session("feat-a 2").session;
    let later_parent = h.rex.add_session("app 2").session;
    let parent = h.rex.add_session("app").session;
    let unrelated = h.rex.add_session("other").session;
    let unbound = h.rex.add_session("scratch").session;
    h.seed_binding(&current, &linked, 300);
    h.seed_binding(&sibling, &linked, 400);
    h.seed_binding(&later_parent, &repo, 200);
    h.seed_binding(&parent, &repo, 100);
    h.seed_binding(&unrelated, &other, 100);
    RemoveFixture {
        h,
        repo,
        linked,
        current,
        sibling,
        parent,
        later_parent,
        unrelated,
        unbound,
    }
}

#[test]
fn the_remove_plan_names_every_session_that_will_close() {
    let f = remove_fixture();

    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();

    assert_eq!(plan.checkout_path, f.linked);
    assert_eq!(plan.branch.as_deref(), Some("feat/a"));
    assert_eq!(plan.main_checkout, f.repo);
    assert_eq!(
        plan.sessions,
        [f.h.rex.info(&f.current), f.h.rex.info(&f.sibling)],
        "bound live sessions of that checkout, earliest first"
    );
    assert_eq!(
        plan.parent,
        Some(f.h.rex.info(&f.parent)),
        "the earliest-bound parent"
    );
    assert_eq!(f.h.rex.calls(), [], "planning changes nothing");
}

#[test]
fn removing_a_clean_worktree_keeps_the_branch_and_closes_its_sessions_last_one_last() {
    let f = remove_fixture();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();

    let removed = wf::remove(
        &f.h.ctx(),
        &plan,
        false,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    assert!(!f.linked.exists());
    assert!(
        branch_exists(&f.repo, "feat/a"),
        "remove never deletes the branch"
    );
    assert_eq!(removed.focus, Focus::Switched);
    assert_eq!(removed.closed, [f.sibling.clone(), f.current.clone()]);
    assert_eq!(
        f.h.rex.calls(),
        [
            Call::Select {
                client: Some(CLIENT.into()),
                session: f.parent.clone()
            },
            Call::Destroy {
                session: f.sibling.clone()
            },
            Call::Destroy {
                session: f.current.clone()
            },
        ]
    );
    let bindings = f.h.bindings();
    assert!(!bindings.contains_key(&f.current) && !bindings.contains_key(&f.sibling));
    for kept in [&f.parent, &f.later_parent, &f.unrelated] {
        assert!(bindings.contains_key(kept), "{kept} must stay bound");
        assert!(f.h.rex.is_live(kept), "{kept} must stay open");
    }
    assert!(f.h.rex.is_live(&f.unbound));
}

/// Run from a shell outside the removed sessions (CLI): all of them close.
#[test]
fn removing_from_outside_closes_every_bound_session() {
    let f = remove_fixture();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();

    let removed = wf::remove(&f.h.ctx(), &plan, false, None, Switch::App(None))
        .unwrap_or_else(|e| panic!("{e:?}"));

    let mut closed = removed.closed.clone();
    closed.sort();
    let mut expected = vec![f.current.clone(), f.sibling.clone()];
    expected.sort();
    assert_eq!(closed, expected);
    assert!(!f.h.rex.is_live(&f.current) && !f.h.rex.is_live(&f.sibling));
}

/// Without consent to force, a dirty worktree stops the whole workflow before
/// anything changes.
#[test]
fn a_dirty_worktree_is_not_removed_without_force() {
    let f = remove_fixture();
    std::fs::write(f.linked.join("notes.txt"), "precious").unwrap();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();
    let bindings_before = f.h.bindings();

    let err = wf::remove(
        &f.h.ctx(),
        &plan,
        false,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_err();

    assert!(matches!(err, RemoveError::NeedsForce(_)), "{err:?}");
    assert_eq!(
        std::fs::read_to_string(f.linked.join("notes.txt")).unwrap(),
        "precious"
    );
    assert_eq!(f.h.bindings(), bindings_before);
    assert_eq!(f.h.rex.calls(), []);
}

#[test]
fn a_dirty_worktree_is_removed_with_force() {
    let f = remove_fixture();
    std::fs::write(f.linked.join("notes.txt"), "scratch").unwrap();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();

    wf::remove(
        &f.h.ctx(),
        &plan,
        true,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    assert!(!f.linked.exists());
    assert!(branch_exists(&f.repo, "feat/a"));
    assert!(!f.h.rex.is_live(&f.current));
}

/// Any other git refusal (here: a locked worktree) changes nothing, even when
/// forcing was allowed.
#[test]
fn a_git_failure_leaves_everything_in_place() {
    let f = remove_fixture();
    git(&f.repo, &["worktree", "lock", f.linked.to_str().unwrap()]);
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();
    let bindings_before = f.h.bindings();

    let err = wf::remove(
        &f.h.ctx(),
        &plan,
        true,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_err();

    assert!(matches!(err, RemoveError::Failed(_)), "{err:?}");
    assert!(f.linked.exists());
    assert_eq!(f.h.bindings(), bindings_before);
    assert_eq!(f.h.rex.calls(), []);
}

#[test]
fn a_refused_switch_does_not_stop_remove() {
    let f = remove_fixture();
    f.h.rex.refuse_select("Turn on Remote Control");
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();

    let removed = wf::remove(
        &f.h.ctx(),
        &plan,
        false,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    assert!(matches!(removed.focus, Focus::Refused(ref m) if m.contains("Remote Control")));
    assert_eq!(
        destroyed(&f.h.rex.calls()),
        [f.sibling.clone(), f.current.clone()]
    );
}

#[test]
fn without_a_bound_parent_remove_switches_nothing() {
    let h = Harness::new();
    let repo = h.repo();
    let linked = h.linked(&repo, "feat/a");
    let s = h.bound_session("feat-a", &linked);
    let plan = wf::plan_remove(&h.ctx(), &s.session).unwrap();
    assert_eq!(plan.parent, None);

    let removed = wf::remove(
        &h.ctx(),
        &plan,
        false,
        Some(&s.session),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    assert_eq!(removed.focus, Focus::NotNeeded);
    assert_eq!(
        h.rex.calls(),
        [Call::Destroy {
            session: s.session.clone()
        }]
    );
}

/// For agents and scripts: everything is removed and closed, and every app
/// stays where it is, even with a parent to show.
#[test]
fn removing_without_switching_leaves_the_apps_alone() {
    let f = remove_fixture();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();
    assert!(plan.parent.is_some());

    let removed =
        wf::remove(&f.h.ctx(), &plan, false, None, Switch::Off).unwrap_or_else(|e| panic!("{e:?}"));

    assert_eq!(removed.focus, Focus::NotNeeded);
    assert!(!f.linked.exists());
    let calls = f.h.rex.calls();
    assert!(
        calls.iter().all(|c| matches!(c, Call::Destroy { .. })),
        "{calls:?}"
    );
    assert!(!f.h.rex.is_live(&f.current) && !f.h.rex.is_live(&f.sibling));
}

/// Bindings are re-read at removal time: a session bound to the checkout after
/// the confirmation loses its (now dangling) binding, but only the sessions the
/// user confirmed are closed.
#[test]
fn remove_drops_late_bindings_but_closes_only_confirmed_sessions() {
    let f = remove_fixture();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();
    let late = f.h.rex.add_session("late");
    wf::bind(&f.h.ctx(), &late.session, &f.linked).unwrap();

    wf::remove(
        &f.h.ctx(),
        &plan,
        false,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    assert!(!f.h.bindings().contains_key(&late.session));
    assert!(f.h.rex.is_live(&late.session));
}

/// A session that closed in the meantime doesn't stop the others from closing,
/// and the current one still goes last.
#[test]
fn remove_tolerates_a_session_that_already_closed() {
    let f = remove_fixture();
    let plan = wf::plan_remove(&f.h.ctx(), &f.current).unwrap();
    f.h.rex.kill_externally(&f.sibling);

    let removed = wf::remove(
        &f.h.ctx(),
        &plan,
        false,
        Some(&f.current),
        Switch::App(Some(CLIENT)),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    assert_eq!(removed.closed, std::slice::from_ref(&f.current));
    assert_eq!(destroyed(&f.h.rex.calls()).last(), Some(&f.current));
    assert!(!f.h.rex.is_live(&f.current));
}

// =============================================================================
// sync
// =============================================================================

#[test]
fn the_sync_action_runs_in_a_small_focused_layer() {
    let h = Harness::new();
    let repo = h.repo();
    let a = h.linked(&repo, "feat/a");
    let s = h.bound_session("feat-a", &a);

    let outcome = wf::action(&h.ctx(), &request(Verb::Sync, &s.session)).unwrap();

    assert_eq!(outcome.select, None);
    assert_eq!(
        layers(&h.rex.calls()),
        [(
            s.session.clone(),
            None,
            terminal(&a, launched(&["rex", "sync", "--session", &s.session])),
            true
        )]
    );
}

/// File operations only, from the main checkout, using the configured repo
/// config path; hooks and installs are not re-run.
#[test]
fn sync_reapplies_file_operations_from_the_main_checkout() {
    let mut h = Harness::new();
    h.settings.repo_config_path = PathBuf::from("conf/worktree.yaml");
    let repo = h.repo();
    std::fs::create_dir_all(repo.join("conf")).unwrap();
    std::fs::write(
        repo.join("conf/worktree.yaml"),
        "copy:\n  enabled: true\n  files: [.env]\nhooks:\n  post:\n    - command: [sh, -c, echo ran > hook.log]\n",
    )
    .unwrap();
    let a = h.linked(&repo, "feat/a");
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    let s = h.bound_session("feat-a", &a);

    let summary = wf::sync(&h.ctx(), &s.session).unwrap();

    assert_eq!(summary.copied, Some(1));
    assert_eq!(
        std::fs::read_to_string(a.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert!(!a.join("hook.log").exists(), "sync never runs hooks");
    assert_eq!(summary.hooks_run, 0);
}

#[test]
fn sync_refuses_an_unbound_session() {
    let h = Harness::new();
    let s = h.rex.add_session("scratch");
    assert!(wf::sync(&h.ctx(), &s.session).is_err());
}
