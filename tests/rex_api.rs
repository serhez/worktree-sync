//! `RexCli`: the one place that talks to Rex, through the `rex` executable.
//! Driven against a scripted `rex` that logs its argv and replies on cue.

mod common;
mod rex_support;

use std::path::PathBuf;

use common::Dir;
use rex_support::rex_script::RexScript;
use serde_json::{Value, json};
use worktree_sync::backends::rex::api::{
    CREATE_SCRIPT, NewSession, Rex, RexCli, SessionInfo, Terminal,
};

fn setup() -> (Dir, RexScript, RexCli) {
    let dir = Dir::new();
    let script = RexScript::install(&dir.path().join("bin"));
    let rex = RexCli::new(script.bin());
    (dir, script, rex)
}

fn only_invocation(script: &RexScript) -> Vec<String> {
    let mut all = script.invocations();
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

/// `[--autostart=false, api, call, -s, S, method, json]` → (S, method, json).
fn api_call(argv: &[String]) -> (Option<String>, String, Value) {
    assert_eq!(argv[..3], ["--autostart=false", "api", "call"], "{argv:?}");
    let (session, rest) = if argv[3] == "-s" {
        (Some(argv[4].clone()), &argv[5..])
    } else {
        (None, &argv[3..])
    };
    assert_eq!(rest.len(), 2, "{argv:?}");
    (
        session,
        rest[0].clone(),
        serde_json::from_str(&rest[1]).unwrap(),
    )
}

/// A creating call goes through the create script, which hands the new blocks
/// to the app: `[--autostart=false, do, -, --args, json]` with the script on
/// stdin → (session_id, method, args) from the json.
fn create_call(script: &RexScript, argv: &[String]) -> (Option<String>, String, Value) {
    assert_eq!(
        argv[..4],
        ["--autostart=false", "do", "-", "--args"],
        "{argv:?}"
    );
    assert_eq!(argv.len(), 5, "{argv:?}");
    assert_eq!(script.stdin().as_deref(), Some(CREATE_SCRIPT));
    let request: Value = serde_json::from_str(&argv[4]).unwrap();
    (
        request["session_id"].as_str().map(str::to_string),
        request["method"].as_str().unwrap().to_string(),
        request["args"].clone(),
    )
}

/// Every object in `value` that has a `cwd` key: the terminal options.
fn terminal_options(value: &Value) -> Vec<&Value> {
    let mut found = Vec::new();
    match value {
        Value::Object(map) => {
            if map.contains_key("cwd") {
                found.push(value);
            }
            map.values().for_each(|v| found.extend(terminal_options(v)));
        }
        Value::Array(items) => items.iter().for_each(|v| found.extend(terminal_options(v))),
        _ => {}
    }
    found
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
        Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

fn command_terminal() -> Terminal {
    Terminal {
        cwd: PathBuf::from("/w/app"),
        command: Some(vec!["/bin/wts".into(), "rex".into(), "open".into()]),
    }
}

/// Lifecycle work needs the user's login environment (Homebrew on PATH), so
/// no layout may ever pick a shell; and the terminal must be a terminal.
fn assert_login_terminal(args: &Value, terminal: &Terminal) {
    assert!(!contains_key(args, "shell"), "no `shell` option: {args}");
    let options = terminal_options(args);
    assert_eq!(options.len(), 1, "{args}");
    assert_eq!(options[0]["cwd"], json!(terminal.cwd));
    match &terminal.command {
        Some(command) => assert_eq!(options[0]["command"], json!(command)),
        None => assert!(options[0].get("command").is_none(), "{args}"),
    }
    assert!(
        args.to_string().contains("com.superlogical.terminal.shell"),
        "{args}"
    );
}

#[test]
fn host_id_comes_from_whoami() {
    let (_dir, script, rex) = setup();
    script.reply(r#"{"host_id":"h-1","label":"mac","extra":[1]}"#);

    assert_eq!(rex.host_id().unwrap(), "h-1");
    assert_eq!(
        only_invocation(&script),
        ["--autostart=false", "whoami", "--json"]
    );
}

#[test]
fn sessions_come_from_ls() {
    let (_dir, script, rex) = setup();
    script.reply(
        r#"{"sessions":[{"session_id":"s1","label":"app","windows":2},{"session_id":"s2","label":""}]}"#,
    );

    assert_eq!(
        rex.sessions().unwrap(),
        [
            SessionInfo {
                session_id: "s1".into(),
                label: "app".into()
            },
            SessionInfo {
                session_id: "s2".into(),
                label: "".into()
            },
        ]
    );
    assert_eq!(
        only_invocation(&script),
        ["--autostart=false", "ls", "--json"]
    );
}

#[test]
fn create_session_creates_it_with_one_window() {
    let (_dir, script, rex) = setup();
    script.reply(
        r#"{"session_id":"S","initial_windows":[{"window_id":"W","block_ids":["B"],"revision":1}]}"#,
    );
    let terminal = command_terminal();

    let created = rex
        .create_session("new worktree", "setup", &terminal)
        .unwrap();

    assert_eq!(
        created,
        NewSession {
            session_id: "S".into(),
            window_id: "W".into(),
            block_id: "B".into()
        }
    );
    let (session, method, args) = create_call(&script, &only_invocation(&script));
    assert_eq!(session, None, "a new session has no session to scope to");
    assert_eq!(method, "session.create");
    assert_eq!(args["label"], "new worktree");
    let windows = args["initial_windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["window_label"], "setup");
    assert_login_terminal(&args, &terminal);
}

#[test]
fn an_interactive_terminal_has_no_command() {
    let (_dir, script, rex) = setup();
    script.reply(
        r#"{"session_id":"S","initial_windows":[{"window_id":"W","block_ids":["B"],"revision":1}]}"#,
    );
    let terminal = Terminal {
        cwd: PathBuf::from("/w/app"),
        command: None,
    };

    rex.create_session("app", "app", &terminal).unwrap();

    let (_, _, args) = create_call(&script, &only_invocation(&script));
    assert_login_terminal(&args, &terminal);
}

#[test]
fn new_window_is_scoped_to_the_session() {
    let (_dir, script, rex) = setup();
    script.reply(r#"{"window_id":"W2","block_ids":["B2"],"revision":4}"#);
    let terminal = command_terminal();

    assert_eq!(rex.new_window("S", "shell", &terminal, true).unwrap(), "W2");

    let (session, method, args) = create_call(&script, &only_invocation(&script));
    assert_eq!(session.as_deref(), Some("S"));
    assert_eq!(method, "session.new_window");
    assert_eq!(args["window_label"], "shell");
    assert_eq!(args["focus"], true);
    assert_login_terminal(&args, &terminal);
}

#[test]
fn new_layer_sends_bounds_focus_and_window() {
    let (_dir, script, rex) = setup();
    script.reply(r#"{"layer_id":"L","block_ids":["B"],"revision":2}"#);
    let terminal = command_terminal();

    rex.new_layer("S", Some("W"), [0.55, 0.03, 0.42, 0.3], &terminal, false)
        .unwrap();
    rex.new_layer("S", None, [0.15, 0.2, 0.7, 0.6], &terminal, true)
        .unwrap();

    let calls = script.invocations();
    let (session, method, args) = create_call(&script, &calls[0]);
    assert_eq!(session.as_deref(), Some("S"));
    assert_eq!(method, "session.new_layer");
    assert_eq!(args["bounds"], json!([0.55, 0.03, 0.42, 0.3]));
    assert_eq!(args["focus"], false);
    assert_eq!(args["window_id"], "W");
    assert_login_terminal(&args, &terminal);

    let (_, _, args) = create_call(&script, &calls[1]);
    assert_eq!(args["focus"], true);
    assert!(args.get("window_id").is_none(), "{args}");
}

#[test]
fn window_of_block_reads_describe_block() {
    let (_dir, script, rex) = setup();
    script.reply(r#"{"block":{"block_id":"B","window_id":"W","title":"zsh"}}"#);

    assert_eq!(rex.window_of_block("S", "B").unwrap(), "W");

    let (session, method, args) = api_call(&only_invocation(&script));
    assert_eq!(session.as_deref(), Some("S"));
    assert_eq!(method, "session.describe_block");
    assert_eq!(args["block_id"], "B");
}

#[test]
fn labels_and_focus_are_session_methods() {
    let (_dir, script, rex) = setup();
    script.reply("{}");

    rex.set_session_label("S", "feat-login").unwrap();
    rex.set_window_label("S", "W", "setup").unwrap();
    rex.focus_window("S", "W").unwrap();

    let calls = script.invocations();
    let (s, method, args) = api_call(&calls[0]);
    assert_eq!(
        (s.as_deref(), method.as_str()),
        (Some("S"), "session.set_label")
    );
    assert_eq!(args["label"], "feat-login");
    let (s, method, args) = api_call(&calls[1]);
    assert_eq!(
        (s.as_deref(), method.as_str()),
        (Some("S"), "session.set_window_label")
    );
    assert_eq!(
        (&args["window_id"], &args["label"]),
        (&json!("W"), &json!("setup"))
    );
    let (s, method, args) = api_call(&calls[2]);
    assert_eq!(
        (s.as_deref(), method.as_str()),
        (Some("S"), "session.focus_window")
    );
    assert_eq!(args["window_id"], "W");
}

#[test]
fn foreground_cwd_comes_from_the_blocks_process() {
    let (_dir, script, rex) = setup();
    script.reply(r#"{"foreground":{"pid":42,"cwd":"/w/app/src"}}"#);

    assert_eq!(
        rex.foreground_cwd("S", "B").unwrap(),
        Some(PathBuf::from("/w/app/src"))
    );
    assert_eq!(
        only_invocation(&script),
        [
            "--autostart=false",
            "block",
            "call",
            "-s",
            "S",
            "-b",
            "B",
            "process"
        ]
    );
}

#[test]
fn an_unknown_foreground_cwd_is_none() {
    let (_dir, script, rex) = setup();
    script.reply(r#"{"foreground":null}"#);
    assert_eq!(rex.foreground_cwd("S", "B").unwrap(), None);
}

#[test]
fn destroy_session_kills_it() {
    let (_dir, script, rex) = setup();
    rex.destroy_session("S").unwrap();
    assert_eq!(only_invocation(&script), ["--autostart=false", "kill", "S"]);
}

#[test]
fn select_session_targets_the_given_client() {
    let (_dir, script, rex) = setup();

    rex.select_session(Some("client:1"), "S").unwrap();
    rex.select_session(None, "S").unwrap();

    assert_eq!(
        script.invocations(),
        [
            vec![
                "--autostart=false",
                "-C",
                "client:1",
                "do",
                "session.select",
                "session_id=S"
            ],
            vec!["--autostart=false", "do", "session.select", "session_id=S"],
        ]
    );
}

/// Rex's own message is what the user sees ("Go to session …: <message>").
#[test]
fn a_failure_carries_rexs_message() {
    let (_dir, script, rex) = setup();
    script.fail("Rex.app is not accepting actions. Turn on Remote Control in its Settings under Rex Server.\n");

    let err = rex.select_session(None, "S").unwrap_err();

    assert!(
        format!("{err:#}").contains("Turn on Remote Control"),
        "{err:#}"
    );
}

#[test]
fn a_failed_create_carries_rexs_message() {
    let (_dir, script, rex) = setup();
    script.fail_on("do", "session not found\n");

    let err = rex
        .new_window("S", "shell", &command_terminal(), true)
        .unwrap_err();

    assert!(format!("{err:#}").contains("session not found"), "{err:#}");
}

#[test]
fn an_unparseable_reply_is_an_error() {
    let (_dir, script, rex) = setup();
    script.reply("not json");
    assert!(rex.host_id().is_err());
    assert!(rex.sessions().is_err());
    assert!(
        rex.create_session("app", "app", &command_terminal())
            .is_err()
    );
}
