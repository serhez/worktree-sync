//! An in-memory Rex server: sessions, windows, and floating layers, with every
//! state-changing request recorded in order.
//!
//! Queries (`host_id`, `sessions`, `window_of_block`, `foreground_cwd`) are not
//! recorded; tests assert on what a workflow *changed*, and in which order.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use worktree_sync::backends::rex::api::{Bounds, NewSession, Rex, SessionInfo, Terminal};

pub const HOST_ID: &str = "host-local";

/// One state-changing request, as the workflow made it.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    CreateSession {
        session: String,
        label: String,
        window_label: String,
        terminal: Terminal,
    },
    NewWindow {
        session: String,
        window: String,
        label: String,
        terminal: Terminal,
        focus: bool,
    },
    NewLayer {
        session: String,
        window: Option<String>,
        bounds: Bounds,
        terminal: Terminal,
        focus: bool,
    },
    SetSessionLabel {
        session: String,
        label: String,
    },
    SetWindowLabel {
        session: String,
        window: String,
        label: String,
    },
    FocusWindow {
        session: String,
        window: String,
    },
    Destroy {
        session: String,
    },
    Select {
        client: Option<String>,
        session: String,
    },
}

/// The ids of a session's first window and block.
#[derive(Clone, Debug)]
pub struct Ids {
    pub session: String,
    pub window: String,
    pub block: String,
}

#[derive(Clone, Debug)]
struct Session {
    label: String,
    /// (window id, block id)
    windows: Vec<(String, String)>,
}

#[derive(Default)]
struct State {
    next: u32,
    /// Ordered by creation, so ids sort in creation order.
    sessions: BTreeMap<String, Session>,
    calls: Vec<Call>,
    refuse_select: Option<String>,
    listing_fails: bool,
    foreground: BTreeMap<(String, String), PathBuf>,
}

impl State {
    fn id(&mut self, kind: &str) -> String {
        self.next += 1;
        format!("{kind}:{:04}", self.next)
    }

    fn session(&mut self, id: &str) -> Result<&mut Session> {
        self.sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("session {id} not found"))
    }
}

type Observer = Box<dyn Fn(&Call)>;

#[derive(Default)]
pub struct FakeRex {
    state: RefCell<State>,
    observer: RefCell<Option<Observer>>,
}

impl FakeRex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A session the user made by hand: one window, one block.
    pub fn add_session(&self, label: &str) -> Ids {
        let mut state = self.state.borrow_mut();
        let session = state.id("session");
        let window = state.id("window");
        let block = state.id("block");
        state.sessions.insert(
            session.clone(),
            Session {
                label: label.to_string(),
                windows: vec![(window.clone(), block.clone())],
            },
        );
        Ids {
            session,
            window,
            block,
        }
    }

    /// The user closed a session outside any workflow.
    pub fn kill_externally(&self, session: &str) {
        self.state.borrow_mut().sessions.remove(session);
    }

    /// Every `select_session` fails with `message` (Remote Control off).
    pub fn refuse_select(&self, message: &str) {
        self.state.borrow_mut().refuse_select = Some(message.to_string());
    }

    /// `sessions()` fails (server unreachable).
    pub fn fail_session_listing(&self) {
        self.state.borrow_mut().listing_fails = true;
    }

    pub fn set_foreground_cwd(&self, session: &str, block: &str, cwd: PathBuf) {
        self.state
            .borrow_mut()
            .foreground
            .insert((session.to_string(), block.to_string()), cwd);
    }

    /// Called with each request before it is applied, so a test can capture
    /// the filesystem and binding state at that moment.
    pub fn observe(&self, observer: impl Fn(&Call) + 'static) {
        *self.observer.borrow_mut() = Some(Box::new(observer));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.borrow().calls.clone()
    }

    pub fn is_live(&self, session: &str) -> bool {
        self.state.borrow().sessions.contains_key(session)
    }

    pub fn label(&self, session: &str) -> Option<String> {
        self.state
            .borrow()
            .sessions
            .get(session)
            .map(|s| s.label.clone())
    }

    pub fn info(&self, session: &str) -> SessionInfo {
        SessionInfo {
            session_id: session.to_string(),
            label: self.label(session).expect("live session"),
        }
    }

    fn record(&self, call: Call) {
        if let Some(observer) = self.observer.borrow().as_ref() {
            observer(&call);
        }
        self.state.borrow_mut().calls.push(call);
    }
}

impl Rex for FakeRex {
    fn host_id(&self) -> Result<String> {
        Ok(HOST_ID.to_string())
    }

    fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let state = self.state.borrow();
        if state.listing_fails {
            bail!("rex is not reachable");
        }
        Ok(state
            .sessions
            .iter()
            .map(|(id, s)| SessionInfo {
                session_id: id.clone(),
                label: s.label.clone(),
            })
            .collect())
    }

    fn create_session(
        &self,
        label: &str,
        window_label: &str,
        terminal: &Terminal,
    ) -> Result<NewSession> {
        let (session, window, block) = {
            let mut state = self.state.borrow_mut();
            (state.id("session"), state.id("window"), state.id("block"))
        };
        self.record(Call::CreateSession {
            session: session.clone(),
            label: label.to_string(),
            window_label: window_label.to_string(),
            terminal: terminal.clone(),
        });
        self.state.borrow_mut().sessions.insert(
            session.clone(),
            Session {
                label: label.to_string(),
                windows: vec![(window.clone(), block.clone())],
            },
        );
        Ok(NewSession {
            session_id: session,
            window_id: window,
            block_id: block,
        })
    }

    fn new_window(
        &self,
        session_id: &str,
        label: &str,
        terminal: &Terminal,
        focus: bool,
    ) -> Result<String> {
        let (window, block) = {
            let mut state = self.state.borrow_mut();
            state.session(session_id)?;
            (state.id("window"), state.id("block"))
        };
        self.record(Call::NewWindow {
            session: session_id.to_string(),
            window: window.clone(),
            label: label.to_string(),
            terminal: terminal.clone(),
            focus,
        });
        self.state
            .borrow_mut()
            .session(session_id)?
            .windows
            .push((window.clone(), block));
        Ok(window)
    }

    fn new_layer(
        &self,
        session_id: &str,
        window_id: Option<&str>,
        bounds: Bounds,
        terminal: &Terminal,
        focus: bool,
    ) -> Result<()> {
        self.state.borrow_mut().session(session_id)?;
        self.record(Call::NewLayer {
            session: session_id.to_string(),
            window: window_id.map(str::to_string),
            bounds,
            terminal: terminal.clone(),
            focus,
        });
        Ok(())
    }

    fn window_of_block(&self, session_id: &str, block_id: &str) -> Result<String> {
        let mut state = self.state.borrow_mut();
        state
            .session(session_id)?
            .windows
            .iter()
            .find(|(_, block)| block == block_id)
            .map(|(window, _)| window.clone())
            .ok_or_else(|| anyhow!("block {block_id} not found"))
    }

    fn set_session_label(&self, session_id: &str, label: &str) -> Result<()> {
        self.state.borrow_mut().session(session_id)?;
        self.record(Call::SetSessionLabel {
            session: session_id.to_string(),
            label: label.to_string(),
        });
        self.state.borrow_mut().session(session_id)?.label = label.to_string();
        Ok(())
    }

    fn set_window_label(&self, session_id: &str, window_id: &str, label: &str) -> Result<()> {
        self.state.borrow_mut().session(session_id)?;
        self.record(Call::SetWindowLabel {
            session: session_id.to_string(),
            window: window_id.to_string(),
            label: label.to_string(),
        });
        Ok(())
    }

    fn focus_window(&self, session_id: &str, window_id: &str) -> Result<()> {
        self.state.borrow_mut().session(session_id)?;
        self.record(Call::FocusWindow {
            session: session_id.to_string(),
            window: window_id.to_string(),
        });
        Ok(())
    }

    fn foreground_cwd(&self, session_id: &str, block_id: &str) -> Result<Option<PathBuf>> {
        let state = self.state.borrow();
        Ok(state
            .foreground
            .get(&(session_id.to_string(), block_id.to_string()))
            .cloned())
    }

    fn destroy_session(&self, session_id: &str) -> Result<()> {
        self.record(Call::Destroy {
            session: session_id.to_string(),
        });
        self.state
            .borrow_mut()
            .sessions
            .remove(session_id)
            .map(|_| ())
            .ok_or_else(|| anyhow!("session {session_id} not found"))
    }

    fn select_session(&self, client_id: Option<&str>, session_id: &str) -> Result<()> {
        self.record(Call::Select {
            client: client_id.map(str::to_string),
            session: session_id.to_string(),
        });
        let state = self.state.borrow();
        if let Some(message) = &state.refuse_select {
            bail!("{message}");
        }
        if !state.sessions.contains_key(session_id) {
            bail!("session {session_id} not found");
        }
        Ok(())
    }
}
