//! The binding file: which Rex session belongs to which checkout.
//!
//! This file is a published contract (`docs/rex-bindings.md`); other tools
//! read it. Only this tool writes it, always as a locked read-modify-write
//! with an atomic replace, so readers never lock and never see a partial file.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::git;

/// Schema version. Additive optional fields keep it; anything else bumps it.
pub const VERSION: u32 = 1;

const FILE_NAME: &str = "bindings.json";
const LOCK_NAME: &str = "bindings.lock";
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingsFile {
    pub version: u32,
    /// Keyed by Rex `host_id`: session ids only mean something on their host.
    pub hosts: BTreeMap<String, HostBindings>,
}

impl Default for BindingsFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            hosts: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBindings {
    /// Keyed by Rex `session_id`.
    pub sessions: BTreeMap<String, Binding>,
}

/// One session's checkout. Field order is the published order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// Canonical git common dir: equal keys ⇔ same repository.
    pub repo_key: PathBuf,
    pub repo_name: String,
    /// Canonical work-tree root.
    pub checkout_path: PathBuf,
    /// Refreshed on every write; `null` when detached.
    pub branch: Option<String>,
    pub is_linked_worktree: bool,
    /// Unix seconds; the earliest binding of a checkout is its primary session.
    pub bound_at: u64,
}

/// `$XDG_STATE_HOME/worktree-sync/rex/bindings.json` when that is absolute,
/// else under `$HOME/.local/state`, on every platform.
pub fn path_from_env(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    super::xdg_path(
        xdg_state_home,
        home,
        ".local/state",
        "worktree-sync/rex/bindings.json",
    )
}

/// The canonical serialization: pretty-printed with two spaces, newline at
/// the end.
pub fn to_json(file: &BindingsFile) -> String {
    let mut text = serde_json::to_string_pretty(file).expect("bindings always serialize");
    text.push('\n');
    text
}

pub struct Store {
    path: PathBuf,
    lock_timeout: Duration,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }

    /// How long [`Store::update`] waits for another writer before failing.
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current bindings. A missing file has none; a file that doesn't
    /// parse, or has an unknown version, is an error.
    pub fn read(&self) -> Result<BindingsFile> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BindingsFile::default());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", self.path.display()))?;
        match value.get("version").and_then(serde_json::Value::as_u64) {
            Some(v) if v == u64::from(VERSION) => {}
            other => bail!(
                "{} has unsupported version {}; this tool understands version {VERSION}",
                self.path.display(),
                other.map_or("(none)".to_string(), |v| v.to_string())
            ),
        }
        serde_json::from_value(value).with_context(|| format!("parsing {}", self.path.display()))
    }

    /// Change `host_id`'s bindings under the writer lock.
    ///
    /// Under the lock, in order: re-read the file (refusing one it can't
    /// read), drop this host's sessions that `live_sessions` no longer lists
    /// (nothing is pruned when it fails), apply `edit` (nothing is written
    /// when it fails), refresh every branch of this host, then replace the
    /// file atomically.
    pub fn update<T>(
        &self,
        host_id: &str,
        live_sessions: impl FnOnce() -> Result<Vec<String>>,
        edit: impl FnOnce(&mut HostBindings) -> Result<T>,
    ) -> Result<T> {
        let dir = self
            .path
            .parent()
            .context("the bindings path has no directory")?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let _lock = self.lock(dir)?;

        let mut file = self.read()?;
        let host = file.hosts.entry(host_id.to_string()).or_default();
        if let Ok(live) = live_sessions() {
            host.sessions.retain(|id, _| live.contains(id));
        }
        let value = edit(host)?;
        for binding in host.sessions.values_mut() {
            // A checkout that vanished keeps its last known branch.
            if binding.checkout_path.is_dir()
                && let Ok(branch) = git::current_branch(&binding.checkout_path)
            {
                binding.branch = branch;
            }
        }
        if host.sessions.is_empty() {
            file.hosts.remove(host_id);
        }
        self.write(dir, &file)?;
        Ok(value)
    }

    /// Exclusive lock on the sidecar file, polled until the deadline so an
    /// action never waits indefinitely. Released when the handle drops.
    fn lock(&self, dir: &Path) -> Result<File> {
        let path = dir.join(LOCK_NAME);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(lock),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(LOCK_POLL)
                }
                Err(TryLockError::WouldBlock) => bail!(
                    "timed out after {:?} waiting for the lock on {} (another worktree-sync is updating it)",
                    self.lock_timeout,
                    path.display()
                ),
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
    }

    /// Write a temp file beside the target, flush it, rename it over the
    /// target, then flush the directory so the rename itself is durable.
    fn write(&self, dir: &Path, file: &BindingsFile) -> Result<()> {
        let tmp = dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
        let result = (|| {
            let mut out = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)?;
            out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            out.write_all(to_json(file).as_bytes())?;
            out.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            File::open(dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.with_context(|| format!("writing {}", self.path.display()))
    }
}
