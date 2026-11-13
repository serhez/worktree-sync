//! The whole boundary to Rex: the [`Rex`] trait, and [`RexCli`], which
//! implements it by running the `rex` executable.
//!
//! `rex` always runs with `--autostart=false`: a missing server is an error,
//! never a reason to start one.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// A floating layer's `[x, y, w, h]`, as fractions of the window.
pub type Bounds = [f64; 4];

/// The `rex do` script through which every call that creates blocks runs, so
/// the new blocks pass to the Rex app at once (see the script).
pub const CREATE_SCRIPT: &str = include_str!("create.lua");

/// The terminal block flavor every layout uses.
const TERMINAL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// A terminal block to start.
///
/// No `shell` option is ever set: Rex then starts the user's login shell
/// environment, which lifecycle work (package managers on a Homebrew PATH)
/// needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub cwd: PathBuf,
    /// The program and its arguments; `None` for an interactive shell. A
    /// command's terminal closes when it exits.
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub label: String,
}

/// A session created with one window holding one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub session_id: String,
    pub window_id: String,
    pub block_id: String,
}

/// Everything the workflows ask of Rex.
pub trait Rex {
    /// This server's `host_id`, which keys the binding file.
    fn host_id(&self) -> Result<String>;
    /// Every live session on this host.
    fn sessions(&self) -> Result<Vec<SessionInfo>>;
    fn create_session(
        &self,
        label: &str,
        window_label: &str,
        terminal: &Terminal,
    ) -> Result<NewSession>;
    /// Returns the new window's id.
    fn new_window(
        &self,
        session_id: &str,
        label: &str,
        terminal: &Terminal,
        focus: bool,
    ) -> Result<String>;
    /// A floating layer; `window_id: None` means the session's active window.
    fn new_layer(
        &self,
        session_id: &str,
        window_id: Option<&str>,
        bounds: Bounds,
        terminal: &Terminal,
        focus: bool,
    ) -> Result<()>;
    fn window_of_block(&self, session_id: &str, block_id: &str) -> Result<String>;
    fn set_session_label(&self, session_id: &str, label: &str) -> Result<()>;
    fn set_window_label(&self, session_id: &str, window_id: &str, label: &str) -> Result<()>;
    fn focus_window(&self, session_id: &str, window_id: &str) -> Result<()>;
    /// The cwd of the program in the foreground of a block, when known.
    fn foreground_cwd(&self, session_id: &str, block_id: &str) -> Result<Option<PathBuf>>;
    /// Close a session and everything running in it.
    fn destroy_session(&self, session_id: &str) -> Result<()>;
    /// Ask a Rex app to show a session. Refused while the app's Remote
    /// Control is off; the error carries Rex's own message.
    fn select_session(&self, client_id: Option<&str>, session_id: &str) -> Result<()>;
}

pub struct RexCli {
    bin: PathBuf,
}

impl RexCli {
    pub fn new(rex_bin: impl Into<PathBuf>) -> Self {
        Self {
            bin: rex_bin.into(),
        }
    }

    /// `$REX_BIN_DIR/rex` (set in every Rex terminal, and passed by the Lua
    /// module), else `rex` from `PATH`.
    pub fn from_env() -> Result<Self> {
        Ok(match std::env::var_os("REX_BIN_DIR") {
            Some(dir) if !dir.is_empty() => Self::new(PathBuf::from(dir).join("rex")),
            _ => Self::new("rex"),
        })
    }

    /// Run `rex --autostart=false args…`; stdout on success, Rex's stderr as
    /// the error otherwise.
    fn run(&self, args: &[&str]) -> Result<String> {
        self.run_with_input(args, None)
    }

    /// [`Self::run`], writing `input` to `rex`'s stdin.
    fn run_with_input(&self, args: &[&str], input: Option<&str>) -> Result<String> {
        let mut child = Command::new(&self.bin)
            .arg("--autostart=false")
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {}", self.bin.display()))?;
        // Dropping stdin after the write closes it, so `rex` sees all of it.
        let written = match (input, child.stdin.take()) {
            (Some(input), Some(mut stdin)) => stdin.write_all(input.as_bytes()),
            _ => Ok(()),
        };
        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to run {}", self.bin.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            if message.is_empty() {
                bail!("`rex {}` failed ({})", args.join(" "), output.status);
            }
            bail!("{message}");
        }
        written.context("failed to send rex its input")?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, args: &[&str]) -> Result<T> {
        let out = self.run(args)?;
        serde_json::from_str(&out)
            .with_context(|| format!("unexpected reply from `rex {}`", args.join(" ")))
    }

    /// `rex api call [-s SESSION] METHOD ARGS`.
    fn call(&self, session_id: Option<&str>, method: &str, args: Value) -> Result<Value> {
        let args = args.to_string();
        let mut argv = vec!["api", "call"];
        if let Some(session_id) = session_id {
            argv.extend(["-s", session_id]);
        }
        argv.extend([method, &args]);
        self.json(&argv)
    }

    /// [`Self::call`] for a `method` that creates blocks, run through
    /// [`CREATE_SCRIPT`].
    fn create(&self, session_id: Option<&str>, method: &str, args: Value) -> Result<Value> {
        let mut request = json!({ "method": method, "args": args });
        if let Some(session_id) = session_id {
            request["session_id"] = json!(session_id);
        }
        let out = self.run_with_input(
            &["do", "-", "--args", &request.to_string()],
            Some(CREATE_SCRIPT),
        )?;
        serde_json::from_str(&out).with_context(|| format!("unexpected reply from Rex's {method}"))
    }
}

fn layout(terminal: &Terminal) -> Value {
    let mut options = json!({ "cwd": terminal.cwd });
    if let Some(command) = &terminal.command {
        options["command"] = json!(command);
    }
    json!({ "block": { "flavor": TERMINAL_FLAVOR, "options": options } })
}

fn string_at(value: &Value, pointer: &str) -> Result<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("Rex's reply has no {pointer}: {value}"))
}

impl Rex for RexCli {
    fn host_id(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct WhoAmI {
            host_id: String,
        }
        Ok(self.json::<WhoAmI>(&["whoami", "--json"])?.host_id)
    }

    fn sessions(&self) -> Result<Vec<SessionInfo>> {
        #[derive(Deserialize)]
        struct Ls {
            sessions: Vec<SessionInfo>,
        }
        Ok(self.json::<Ls>(&["ls", "--json"])?.sessions)
    }

    fn create_session(
        &self,
        label: &str,
        window_label: &str,
        terminal: &Terminal,
    ) -> Result<NewSession> {
        let reply = self.create(
            None,
            "session.create",
            json!({
                "label": label,
                "initial_windows": [{ "window_label": window_label, "layout": layout(terminal) }],
            }),
        )?;
        Ok(NewSession {
            session_id: string_at(&reply, "/session_id")?,
            window_id: string_at(&reply, "/initial_windows/0/window_id")?,
            block_id: string_at(&reply, "/initial_windows/0/block_ids/0")?,
        })
    }

    fn new_window(
        &self,
        session_id: &str,
        label: &str,
        terminal: &Terminal,
        focus: bool,
    ) -> Result<String> {
        let reply = self.create(
            Some(session_id),
            "session.new_window",
            json!({ "window_label": label, "layout": layout(terminal), "focus": focus }),
        )?;
        string_at(&reply, "/window_id")
    }

    fn new_layer(
        &self,
        session_id: &str,
        window_id: Option<&str>,
        bounds: Bounds,
        terminal: &Terminal,
        focus: bool,
    ) -> Result<()> {
        let mut args = json!({ "bounds": bounds, "layout": layout(terminal), "focus": focus });
        if let Some(window_id) = window_id {
            args["window_id"] = json!(window_id);
        }
        self.create(Some(session_id), "session.new_layer", args)
            .map(drop)
    }

    fn window_of_block(&self, session_id: &str, block_id: &str) -> Result<String> {
        let reply = self.call(
            Some(session_id),
            "session.describe_block",
            json!({ "block_id": block_id }),
        )?;
        string_at(&reply, "/block/window_id")
    }

    fn set_session_label(&self, session_id: &str, label: &str) -> Result<()> {
        self.call(
            Some(session_id),
            "session.set_label",
            json!({ "label": label }),
        )
        .map(drop)
    }

    fn set_window_label(&self, session_id: &str, window_id: &str, label: &str) -> Result<()> {
        self.call(
            Some(session_id),
            "session.set_window_label",
            json!({ "window_id": window_id, "label": label }),
        )
        .map(drop)
    }

    fn focus_window(&self, session_id: &str, window_id: &str) -> Result<()> {
        self.call(
            Some(session_id),
            "session.focus_window",
            json!({ "window_id": window_id }),
        )
        .map(drop)
    }

    fn foreground_cwd(&self, session_id: &str, block_id: &str) -> Result<Option<PathBuf>> {
        let reply: Value =
            self.json(&["block", "call", "-s", session_id, "-b", block_id, "process"])?;
        Ok(reply
            .pointer("/foreground/cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from))
    }

    fn destroy_session(&self, session_id: &str) -> Result<()> {
        self.run(&["kill", session_id]).map(drop)
    }

    fn select_session(&self, client_id: Option<&str>, session_id: &str) -> Result<()> {
        let target = format!("session_id={session_id}");
        let mut argv = Vec::new();
        if let Some(client_id) = client_id {
            argv.extend(["-C", client_id]);
        }
        argv.extend(["do", "session.select", &target]);
        self.run(&argv).map(drop)
    }
}
