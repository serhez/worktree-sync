//! The Lua that runs in Rex, under Lua 5.1 (Rex's version) against a stubbed
//! `rex` table: the module (`lua/worktree_sync.lua`), which must stay a thin
//! launcher (validate, run `worktree-sync rex action …`, queue a switch), and
//! the create script that `RexCli` runs for every call that creates blocks.

use std::collections::BTreeMap;

use mlua::{Function, Lua, Table, Value};
use serde_json::json;
use worktree_sync::backends::rex::{api, cli};

const MODULE: &str = include_str!("../lua/worktree_sync.lua");
const BIN: &str = "/opt/wts/worktree-sync";
const REX_BIN_DIR: &str = "/opt/rex/bin";

/// The stub records what the module asks of Rex in the global `T`.
const STUB: &str = r#"
T = { actions = {}, binds = {}, execs = {}, queued = {},
      reply = { ok = true, code = 0, stdout = "{}", stderr = "", timed_out = false } }
rex = {
  action = function(spec) T.actions[spec.name] = spec end,
  bind = function(key, action, args) table.insert(T.binds, { key = key, action = action }) end,
  exec = function(argv, opts)
    table.insert(T.execs, { argv = argv, opts = opts or {} })
    return T.reply
  end,
  client = { queue = function(name, args)
    if T.queue_error then error(T.queue_error, 0) end
    table.insert(T.queued, { name = name, args = args })
  end },
  json = { decode = __json_decode },
  log = function() end,
}
function __run(name, ctx) return pcall(T.actions[name].run, ctx, {}) end
"#;

const VERBS: [(&str, &str, &str); 5] = [
    ("worktree_open", "open", "Open Worktree…"),
    ("worktree_new", "new", "New Worktree…"),
    ("worktree_remove", "remove", "Remove Worktree…"),
    ("worktree_bind", "bind", "Bind Session to Checkout…"),
    ("worktree_sync", "sync", "Sync Worktree Files"),
];

fn to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => Value::Number(n.as_f64().unwrap()),
        J::String(s) => Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.set(i + 1, to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.set(k.as_str(), to_lua(lua, v)?)?;
            }
            Value::Table(t)
        }
    })
}

/// A Lua state with the stub installed and the module loaded (not set up).
fn load() -> (Lua, Table) {
    let lua = Lua::new();
    let decode = lua
        .create_function(|lua, text: String| {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => Ok((to_lua(lua, &v)?, Value::Nil)),
                Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?))),
            }
        })
        .unwrap();
    lua.globals().set("__json_decode", decode).unwrap();
    lua.load(STUB).exec().unwrap();
    let module: Table = lua
        .load(MODULE)
        .set_name("@worktree_sync.lua")
        .eval()
        .expect("the module returns its table");
    (lua, module)
}

/// Loaded and set up with `opts` (Lua table source).
fn setup(opts: &str) -> Lua {
    let (lua, module) = load();
    let opts: Table = lua.load(format!("return {opts}")).eval().unwrap();
    module
        .get::<Function>("setup")
        .unwrap()
        .call::<()>(opts)
        .unwrap();
    lua
}

fn standard() -> Lua {
    setup(&format!(
        "{{ bin = {BIN:?}, rex_bin_dir = {REX_BIN_DIR:?} }}"
    ))
}

/// Run action `name` with `ctx` (Lua table source): (ok, value or error).
fn run(lua: &Lua, name: &str, ctx: &str) -> (bool, Value) {
    let ctx: Table = lua.load(format!("return {ctx}")).eval().unwrap();
    lua.globals()
        .get::<Function>("__run")
        .unwrap()
        .call::<(bool, Value)>((name, ctx))
        .unwrap()
}

fn set_reply(lua: &Lua, reply: &str) {
    lua.load(format!("T.reply = {reply}")).exec().unwrap();
}

struct Exec {
    argv: Vec<String>,
    timeout: Option<f64>,
    rex_bin_dir: Option<String>,
}

fn execs(lua: &Lua) -> Vec<Exec> {
    let execs: Vec<Table> = lua.load("return T.execs").eval().unwrap();
    execs
        .into_iter()
        .map(|e| {
            let opts: Table = e.get("opts").unwrap();
            let env: Option<Table> = opts.get("env").unwrap();
            Exec {
                argv: e.get("argv").unwrap(),
                timeout: opts.get("timeout").unwrap(),
                rex_bin_dir: env.map(|env| env.get("REX_BIN_DIR").unwrap()),
            }
        })
        .collect()
}

fn queued(lua: &Lua) -> Vec<(String, Option<String>)> {
    let queued: Vec<Table> = lua.load("return T.queued").eval().unwrap();
    queued
        .into_iter()
        .map(|q| {
            let args: Option<Table> = q.get("args").unwrap();
            (
                q.get("name").unwrap(),
                args.and_then(|a| a.get("session_id").unwrap()),
            )
        })
        .collect()
}

fn error_text(value: Value) -> String {
    match value {
        Value::String(s) => s.to_str().unwrap().to_string(),
        other => panic!("expected an error message, got {other:?}"),
    }
}

#[test]
fn setup_defines_the_five_actions() {
    let lua = standard();
    let actions: BTreeMap<String, Table> = lua.load("return T.actions").eval().unwrap();

    assert_eq!(actions.len(), 5);
    for (name, _, title) in VERBS {
        let action = actions
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(action.get::<String>("title").unwrap(), title, "{name}");
        assert!(action.get::<Function>("run").is_ok(), "{name}");
    }
}

#[test]
fn only_configured_keys_are_bound() {
    let lua = standard();
    let binds: Vec<Table> = lua.load("return T.binds").eval().unwrap();
    assert!(binds.is_empty(), "no keys unless asked");

    let lua = setup(&format!(
        r#"{{ bin = {BIN:?}, rex_bin_dir = {REX_BIN_DIR:?}, keys = {{ open = "cmd+w", sync = "ctrl+a>s" }} }}"#
    ));
    let binds: Vec<Table> = lua.load("return T.binds").eval().unwrap();
    let mut binds: Vec<(String, String)> = binds
        .into_iter()
        .map(|b| (b.get("key").unwrap(), b.get("action").unwrap()))
        .collect();
    binds.sort();
    assert_eq!(
        binds,
        [
            ("cmd+w".to_string(), "worktree_open".to_string()),
            ("ctrl+a>s".to_string(), "worktree_sync".to_string()),
        ]
    );
}

/// The Rex server's PATH is minimal, so a bare command name would not be
/// found; setup fails loudly instead of every key press failing later.
#[test]
fn setup_requires_absolute_paths() {
    for opts in [
        "{}".to_string(),
        r#"{ bin = "worktree-sync" }"#.to_string(),
        format!(r#"{{ bin = {BIN:?}, rex_bin_dir = "rex/bin" }}"#),
    ] {
        let (lua, module) = load();
        let opts_table: Table = lua.load(format!("return {opts}")).eval().unwrap();
        let result = module
            .get::<Function>("setup")
            .unwrap()
            .call::<()>(opts_table);
        assert!(result.is_err(), "{opts} should be rejected");
    }
}

#[test]
fn each_action_runs_the_binary_with_its_verb_and_context() {
    for (name, verb, _) in VERBS {
        let lua = standard();

        let (ok, _) = run(
            &lua,
            name,
            r#"{ session_id = "S", block_id = "B", client_id = "C", origin = "key" }"#,
        );

        assert!(ok, "{name}");
        let execs = execs(&lua);
        assert_eq!(execs.len(), 1, "{name}");
        assert_eq!(
            execs[0].argv,
            [
                BIN,
                "rex",
                "action",
                verb,
                "--session",
                "S",
                "--block",
                "B",
                "--client",
                "C"
            ],
            "{name}"
        );
        assert_eq!(
            execs[0].timeout,
            Some(10.0),
            "an action must never hang Rex's input"
        );
        assert_eq!(execs[0].rex_bin_dir.as_deref(), Some(REX_BIN_DIR));
    }
}

#[test]
fn unknown_block_and_client_are_left_out() {
    let lua = standard();

    run(&lua, "worktree_open", r#"{ session_id = "S" }"#);

    assert_eq!(
        execs(&lua)[0].argv,
        [BIN, "rex", "action", "open", "--session", "S"]
    );
}

/// The drift guard between the two halves: whatever the Lua runs, the CLI
/// accepts.
#[test]
fn every_command_the_module_runs_is_accepted_by_the_cli() {
    for (name, _, _) in VERBS {
        for ctx in [
            r#"{ session_id = "S", block_id = "B", client_id = "C" }"#,
            r#"{ session_id = "S" }"#,
        ] {
            let lua = standard();
            run(&lua, name, ctx);
            let argv = execs(&lua).remove(0).argv;
            cli::command()
                .try_get_matches_from(&argv[1..])
                .unwrap_or_else(|e| panic!("{argv:?} rejected:\n{e}"));
        }
    }
}

#[test]
fn a_select_reply_queues_the_switch() {
    let lua = standard();
    set_reply(
        &lua,
        r#"{ ok = true, code = 0, stdout = '{"select":"session:9"}\n', stderr = "" }"#,
    );

    let (ok, _) = run(&lua, "worktree_new", r#"{ session_id = "S" }"#);

    assert!(ok);
    assert_eq!(
        queued(&lua),
        [("session.select".to_string(), Some("session:9".to_string()))]
    );
}

#[test]
fn an_empty_reply_queues_nothing() {
    let lua = standard();
    let (ok, _) = run(&lua, "worktree_open", r#"{ session_id = "S" }"#);
    assert!(ok);
    assert!(queued(&lua).is_empty());
}

/// The work already happened when the switch fails (e.g. no client, as with
/// `rex do`): report it, don't raise.
#[test]
fn a_failed_switch_is_a_warning_not_an_error() {
    let lua = standard();
    set_reply(
        &lua,
        r#"{ ok = true, code = 0, stdout = '{"select":"session:9"}', stderr = "" }"#,
    );
    lua.load(r#"T.queue_error = "no client""#).exec().unwrap();

    let (ok, value) = run(&lua, "worktree_new", r#"{ session_id = "S" }"#);

    assert!(ok, "must not raise: {value:?}");
    let Value::Table(result) = value else {
        panic!("{value:?}")
    };
    let warning: String = result.get("warning").unwrap();
    assert!(warning.contains("no client"), "{warning}");
}

/// The binary's one-line error is what Rex shows the user.
#[test]
fn a_failed_command_raises_its_trimmed_stderr() {
    let lua = standard();
    set_reply(
        &lua,
        r#"{ ok = false, code = 1, stdout = "", stderr = "  This session isn't bound to a checkout.\n" }"#,
    );

    let (ok, value) = run(&lua, "worktree_open", r#"{ session_id = "S" }"#);

    assert!(!ok);
    assert_eq!(error_text(value), "This session isn't bound to a checkout.");
    assert!(queued(&lua).is_empty());
}

#[test]
fn a_timed_out_command_raises() {
    let lua = standard();
    set_reply(
        &lua,
        r#"{ ok = false, code = -1, signal = "SIGTERM", stdout = "", stderr = "", timed_out = true }"#,
    );

    let (ok, value) = run(&lua, "worktree_open", r#"{ session_id = "S" }"#);

    assert!(!ok);
    assert!(!error_text(value).is_empty());
}

#[test]
fn sessionless_and_remote_contexts_are_refused_before_running_anything() {
    for ctx in ["{}", r#"{ session_id = "S", server = "studio" }"#] {
        let lua = standard();
        let (ok, value) = run(&lua, "worktree_open", ctx);
        assert!(!ok, "{ctx}");
        assert!(!error_text(value).is_empty());
        assert!(execs(&lua).is_empty(), "{ctx}");
    }
}

// The create script (`api::CREATE_SCRIPT`). A block follows its owner's size,
// and the client that creates it owns it, so blocks left with the short-lived
// `rex do` client would be drawn only once the app takes them over, seconds
// later. The script must give them up before it exits.

const CREATE_STUB: &str = r#"
C = { calls = {}, answers = {}, printed = {} }
rex = {
  call = function(method, args)
    table.insert(C.calls, { method = method, args = args or {} })
    local a = C.answers[method]
    if a == nil then return {} end
    return a.result, a.err
  end,
  json = { encode = __json_encode },
}
print = function(s) table.insert(C.printed, s) end
"#;

fn from_lua(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => J::from(*i),
        Value::Number(n) => serde_json::Number::from_f64(*n).map_or(J::Null, J::Number),
        Value::String(s) => J::String(s.to_str().unwrap().to_string()),
        Value::Table(t) if t.raw_len() > 0 => J::Array(
            t.clone()
                .sequence_values::<Value>()
                .map(|v| from_lua(&v.unwrap()))
                .collect(),
        ),
        Value::Table(t) => J::Object(
            t.clone()
                .pairs::<String, Value>()
                .map(|kv| {
                    let (k, v) = kv.unwrap();
                    (k, from_lua(&v))
                })
                .collect(),
        ),
        other => panic!("unexpected Lua value {other:?}"),
    }
}

struct Created {
    /// The script's error, if it raised one.
    error: Option<String>,
    /// Every `rex.call`, as (method, args).
    calls: Vec<(String, serde_json::Value)>,
    /// What it printed, decoded.
    printed: Vec<serde_json::Value>,
}

/// Run the create script with `request` as `rex.args`; `answers` maps a
/// method to `{result = …}` or `{err = …}` (others answer `{}`).
fn create(request: serde_json::Value, answers: serde_json::Value) -> Created {
    let lua = Lua::new();
    let encode = lua
        .create_function(|_, value: Value| Ok(from_lua(&value).to_string()))
        .unwrap();
    lua.globals().set("__json_encode", encode).unwrap();
    lua.load(CREATE_STUB).exec().unwrap();
    let rex: Table = lua.globals().get("rex").unwrap();
    rex.set("args", to_lua(&lua, &request).unwrap()).unwrap();
    let c: Table = lua.globals().get("C").unwrap();
    c.set("answers", to_lua(&lua, &answers).unwrap()).unwrap();

    let error = lua
        .load(api::CREATE_SCRIPT)
        .set_name("@create.lua")
        .exec()
        .err()
        .map(|e| e.to_string());

    let calls = from_lua(&c.get::<Value>("calls").unwrap());
    let printed: Vec<String> = c
        .get::<Table>("printed")
        .unwrap()
        .sequence_values::<String>()
        .map(Result::unwrap)
        .collect();
    Created {
        error,
        calls: calls
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|c| (c["method"].as_str().unwrap().to_string(), c["args"].clone()))
                    .collect()
            })
            .unwrap_or_default(),
        printed: printed
            .iter()
            .map(|p| serde_json::from_str(p).unwrap())
            .collect(),
    }
}

fn methods(created: &Created) -> Vec<&str> {
    created.calls.iter().map(|(m, _)| m.as_str()).collect()
}

fn args_of<'a>(created: &'a Created, method: &str) -> &'a serde_json::Value {
    &created.calls.iter().find(|(m, _)| m == method).unwrap().1
}

#[test]
fn a_new_layer_is_made_in_its_session_then_its_block_given_up() {
    let reply = json!({ "layer_id": "L", "block_ids": ["B"], "revision": 3 });
    let created = create(
        json!({ "session_id": "S", "method": "session.new_layer",
                "args": { "bounds": [0.1, 0.1, 0.8, 0.8], "focus": true } }),
        json!({ "session.new_layer": { "result": reply } }),
    );

    assert_eq!(created.error, None);
    assert_eq!(
        methods(&created),
        [
            "session.attach_all",
            "session.attach",
            "session.new_layer",
            "session.disown_block"
        ]
    );
    assert_eq!(args_of(&created, "session.attach")["session_id"], "S");
    let layer = args_of(&created, "session.new_layer");
    assert_eq!(layer["session_id"], "S");
    assert_eq!(layer["bounds"], json!([0.1, 0.1, 0.8, 0.8]));
    assert_eq!(layer["focus"], true);
    assert_eq!(
        args_of(&created, "session.disown_block"),
        &json!({ "session_id": "S", "block_ids": ["B"] })
    );
    assert_eq!(created.printed, [reply]);
}

#[test]
fn a_new_session_gives_up_the_blocks_of_every_initial_window() {
    let reply = json!({ "session_id": "S", "initial_windows": [
        { "window_id": "W1", "block_ids": ["B1"] },
        { "window_id": "W2", "block_ids": ["B2", "B3"] },
    ] });
    let created = create(
        json!({ "method": "session.create", "args": { "label": "app" } }),
        json!({ "session.create": { "result": reply } }),
    );

    assert_eq!(created.error, None);
    assert_eq!(
        methods(&created),
        [
            "session.attach_all",
            "session.create",
            "session.attach",
            "session.disown_block"
        ]
    );
    assert_eq!(
        args_of(&created, "session.create"),
        &json!({ "label": "app" })
    );
    assert_eq!(args_of(&created, "session.attach")["session_id"], "S");
    assert_eq!(
        args_of(&created, "session.disown_block"),
        &json!({ "session_id": "S", "block_ids": ["B1", "B2", "B3"] })
    );
    assert_eq!(created.printed, [reply]);
}

#[test]
fn a_failed_create_raises_rexs_message_and_prints_nothing() {
    let created = create(
        json!({ "session_id": "S", "method": "session.new_window", "args": {} }),
        json!({ "session.new_window": { "err": "window limit reached" } }),
    );

    assert!(
        created
            .error
            .as_deref()
            .unwrap_or("")
            .contains("window limit reached"),
        "{:?}",
        created.error
    );
    assert!(!methods(&created).contains(&"session.disown_block"));
    assert!(created.printed.is_empty());
}

#[test]
fn a_block_that_cannot_be_given_up_still_counts_as_created() {
    let reply = json!({ "window_id": "W", "block_ids": ["B"] });
    let created = create(
        json!({ "session_id": "S", "method": "session.new_window", "args": {} }),
        json!({ "session.new_window": { "result": reply },
                "session.disown_block": { "err": "not attached" } }),
    );

    assert_eq!(created.error, None);
    assert_eq!(created.printed, [reply]);
}
