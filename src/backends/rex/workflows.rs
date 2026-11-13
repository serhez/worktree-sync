//! The Rex workflows: open, new, remove, bind, unbind, sync, and the action
//! entry the Lua module calls.
//!
//! Functions here never prompt; the CLI asks and passes the answers in. Every
//! binding change goes through [`Store::update`], which re-reads the file
//! under its lock, so decisions are made against the current bindings.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Serialize;

use super::api::{NewSession, Rex, SessionInfo, Terminal};
use super::bindings::{Binding, Store};
use super::settings::Settings;
use super::ui::{self, DIALOG_BOUNDS, SYNC_BOUNDS, TOAST_BOUNDS};
use crate::config::{self, NotifyWhen, OnFailure};
use crate::notify::{self, Outcome};
use crate::{Summary, git, report};

/// Label of a session `new` creates before its worktree exists.
const NEW_SESSION_LABEL: &str = "new worktree";
/// Label of the tab that ran the setup of a new worktree.
const SETUP_WINDOW_LABEL: &str = "setup";
/// Label of the shell tab opened beside it.
const SHELL_WINDOW_LABEL: &str = "shell";
/// How long a success toast stays up.
const TOAST_SECONDS: u32 = 5;

const UNBOUND: &str =
    "This session isn't bound to a checkout. Run Bind Session to Checkout… first.";

/// What the workflows run against.
pub struct Context<'a> {
    pub rex: &'a dyn Rex,
    pub settings: &'a Settings,
    pub bindings: &'a Store,
    /// Where failure reports are written.
    pub reports_dir: PathBuf,
    /// This binary, as the commands launched inside Rex run it.
    pub bin: PathBuf,
    pub home: PathBuf,
}

impl Context<'_> {
    /// `bin rex args…`.
    fn command(&self, args: &[&str]) -> Vec<String> {
        std::iter::once(self.bin.to_string_lossy().into_owned())
            .chain(std::iter::once("rex".to_string()))
            .chain(args.iter().map(|a| a.to_string()))
            .collect()
    }

    /// The live session ids, for pruning under the binding lock.
    fn live_ids(&self) -> impl FnOnce() -> Result<Vec<String>> + '_ {
        || {
            Ok(self
                .rex
                .sessions()?
                .into_iter()
                .map(|s| s.session_id)
                .collect())
        }
    }

    /// This host's bindings, as on disk.
    fn host_bindings(&self, host: &str) -> Result<Vec<(String, Binding)>> {
        Ok(self
            .bindings
            .read()?
            .hosts
            .remove(host)
            .map(|h| h.sessions.into_iter().collect())
            .unwrap_or_default())
    }
}

/// Sessions that are both live and matched by `keep`, earliest binding first
/// (ties by session id, so every reader agrees).
fn live_bound(
    bindings: Vec<(String, Binding)>,
    live: &[SessionInfo],
    keep: impl Fn(&Binding) -> bool,
) -> Vec<(SessionInfo, Binding)> {
    let mut found: Vec<_> = bindings
        .into_iter()
        .filter(|(_, b)| keep(b))
        .filter_map(|(id, b)| {
            live.iter()
                .find(|s| s.session_id == id)
                .map(|s| (s.clone(), b))
        })
        .collect();
    found.sort_by(|(a, x), (b, y)| (x.bound_at, &a.session_id).cmp(&(y.bound_at, &b.session_id)));
    found
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `session`'s binding, checked to still name a work tree of its repository.
fn binding_of(ctx: &Context, session: &str) -> Result<Binding> {
    let host = ctx.rex.host_id()?;
    let binding = ctx
        .host_bindings(&host)?
        .into_iter()
        .find(|(id, _)| id == session)
        .map(|(_, b)| b)
        .context(UNBOUND)?;
    match git::checkout_at(&binding.checkout_path) {
        Ok(c) if c.path == binding.checkout_path && c.repo_key == binding.repo_key => Ok(binding),
        _ => bail!(
            "This session is bound to {}, which is no longer a work tree of {}. Bind it again or remove the session.",
            binding.checkout_path.display(),
            binding.repo_name
        ),
    }
}

fn require_linked(binding: &Binding) -> Result<()> {
    if !binding.is_linked_worktree {
        bail!(
            "{} is the main checkout of {}; only linked worktrees can be removed or synced.",
            binding.checkout_path.display(),
            binding.repo_name
        );
    }
    Ok(())
}

// --- Action entry --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Open,
    New,
    Remove,
    Bind,
    Sync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRequest {
    pub verb: Verb,
    pub session: String,
    /// The block the action applies to (the focused one).
    pub block: Option<String>,
    /// The Rex app that ran the action, for focus changes.
    pub client: Option<String>,
}

/// What the Lua module should do once the action returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionOutcome {
    /// Switch the app to this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select: Option<String>,
}

/// Validate and launch; never prompt, never run the lifecycle. Prompts and
/// long work start in a terminal block, so the action returns at once (Rex
/// holds the user's keystrokes until it does).
pub fn action(ctx: &Context, request: &ActionRequest) -> Result<ActionOutcome> {
    let session = request.session.as_str();
    let client = request.client.as_deref();
    let with_client = |mut args: Vec<&'static str>, session: &str| -> Vec<String> {
        let mut command = ctx.command(&[]);
        command.extend(args.drain(..).map(str::to_string));
        command.push(session.to_string());
        if let Some(client) = client {
            command.extend(["--client".to_string(), client.to_string()]);
        }
        command
    };
    let layer = |cwd: PathBuf, command: Vec<String>, bounds| {
        let terminal = Terminal {
            cwd,
            command: Some(command),
        };
        ctx.rex
            .new_layer(session, None, bounds, &terminal, true)
            .map(|()| ActionOutcome { select: None })
    };

    if request.verb == Verb::Bind {
        let cwd = match &request.block {
            Some(block) => ctx.rex.foreground_cwd(session, block).ok().flatten(),
            None => None,
        };
        return layer(
            cwd.unwrap_or_else(|| ctx.home.clone()),
            ctx.command(&["bind", "--session", session]),
            DIALOG_BOUNDS,
        );
    }

    let binding = binding_of(ctx, session)?;
    match request.verb {
        Verb::Open => layer(
            binding.checkout_path,
            with_client(vec!["open", "--from"], session),
            DIALOG_BOUNDS,
        ),
        Verb::New => {
            let new = launch_new(
                ctx,
                &NewLaunch {
                    from: session.to_string(),
                    branch: None,
                    base: None,
                    client: request.client.clone(),
                },
            )?;
            Ok(ActionOutcome {
                select: Some(new.session_id),
            })
        }
        Verb::Remove => {
            require_linked(&binding)?;
            layer(
                binding.checkout_path,
                with_client(vec!["remove", "--session"], session),
                DIALOG_BOUNDS,
            )
        }
        Verb::Sync => {
            require_linked(&binding)?;
            layer(
                binding.checkout_path,
                ctx.command(&["sync", "--session", session]),
                SYNC_BOUNDS,
            )
        }
        Verb::Bind => unreachable!("handled above"),
    }
}

// --- bind / unbind -------------------------------------------------------------

/// Bind `session` to the work tree containing `path`, replacing any previous
/// binding. The session itself (label, layout) is left as the user made it.
pub fn bind(ctx: &Context, session: &str, path: &Path) -> Result<Binding> {
    let checkout = git::checkout_at(path)?;
    let binding = Binding {
        repo_key: checkout.repo_key,
        repo_name: checkout.repo_name,
        checkout_path: checkout.path,
        branch: checkout.branch,
        is_linked_worktree: checkout.is_linked_worktree,
        bound_at: now(),
    };
    let host = ctx.rex.host_id()?;
    ctx.bindings.update(&host, ctx.live_ids(), |h| {
        h.sessions.insert(session.to_string(), binding.clone());
        Ok(())
    })?;
    Ok(binding)
}

/// Drop `session`'s binding; the binding it had, if any.
pub fn unbind(ctx: &Context, session: &str) -> Result<Option<Binding>> {
    let host = ctx.rex.host_id()?;
    ctx.bindings
        .update(&host, ctx.live_ids(), |h| Ok(h.sessions.remove(session)))
}

// --- open ----------------------------------------------------------------------

/// Whether a workflow switches a Rex app to the session it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch<'a> {
    /// Switch this app, or the one Rex picks when `None`.
    App(Option<&'a str>),
    /// Leave every app where it is: an agent or script works in the background.
    Off,
}

/// How a workflow's request to show a session ended. Focus is best effort and
/// never fails a workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    /// No switch was needed, or [`Switch::Off`] asked for none.
    NotNeeded,
    Switched,
    /// Rex refused (Remote Control off, no app): its message.
    Refused(String),
}

/// Switch an app to `session` as `switch` says; best effort.
pub fn show(ctx: &Context, switch: Switch, session: &str) -> Focus {
    let Switch::App(client) = switch else {
        return Focus::NotNeeded;
    };
    match ctx.rex.select_session(client, session) {
        Ok(()) => Focus::Switched,
        Err(e) => Focus::Refused(format!("{e:#}")),
    }
}

/// A new session labelled after `checkout`'s folder, with one interactive
/// shell in it (in a window labelled `window_label`), bound to it. Nothing is
/// left behind if the binding fails.
fn create_bound_session(ctx: &Context, checkout: &Path, window_label: &str) -> Result<NewSession> {
    let terminal = Terminal {
        cwd: checkout.to_path_buf(),
        command: None,
    };
    let new = ctx
        .rex
        .create_session(&basename(checkout), window_label, &terminal)?;
    if let Err(e) = bind(ctx, &new.session_id, checkout) {
        let _ = ctx.rex.destroy_session(&new.session_id);
        return Err(e);
    }
    Ok(new)
}

/// A work tree `open` offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub checkout_path: PathBuf,
    /// Branch, or the folder name when detached.
    pub label: String,
    pub branch: Option<String>,
    pub is_linked_worktree: bool,
    /// Live sessions bound to it, earliest first.
    pub sessions: Vec<SessionInfo>,
}

/// The work trees of `from_session`'s repository (bare and prunable entries
/// excluded, as in Herdr), main checkout first.
pub fn open_candidates(ctx: &Context, from_session: &str) -> Result<Vec<Candidate>> {
    let from = binding_of(ctx, from_session)?;
    let host = ctx.rex.host_id()?;
    let live = ctx.rex.sessions()?;
    let bindings = ctx.host_bindings(&host)?;
    Ok(git::list_worktrees(&from.checkout_path)?
        .into_iter()
        .enumerate()
        .filter(|(_, e)| !e.is_bare && !e.is_prunable)
        .map(|(index, entry)| Candidate {
            sessions: live_bound(bindings.clone(), &live, |b| b.checkout_path == entry.path)
                .into_iter()
                .map(|(s, _)| s)
                .collect(),
            label: entry
                .branch
                .clone()
                .unwrap_or_else(|| basename(&entry.path)),
            is_linked_worktree: index > 0,
            branch: entry.branch,
            checkout_path: entry.path,
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub session_id: String,
    /// A session was created (and bound) for the checkout.
    pub created: bool,
    pub focus: Focus,
}

/// Show the session of the work tree containing `path`: its earliest bound
/// live session, or a new one (labelled after the folder, a login shell in
/// the checkout) bound to it.
pub fn open_checkout(
    ctx: &Context,
    path: &Path,
    current_session: Option<&str>,
    switch: Switch,
) -> Result<Opened> {
    let checkout = git::checkout_at(path)?;
    let host = ctx.rex.host_id()?;
    let live = ctx.rex.sessions()?;
    let existing = live_bound(ctx.host_bindings(&host)?, &live, |b| {
        b.checkout_path == checkout.path
    })
    .into_iter()
    .next();

    let (session_id, created) = match existing {
        Some((session, _)) => (session.session_id, false),
        None => {
            let label = basename(&checkout.path);
            let new = create_bound_session(ctx, &checkout.path, &label)?;
            (new.session_id, true)
        }
    };
    let focus = if current_session == Some(session_id.as_str()) {
        Focus::NotNeeded
    } else {
        show(ctx, switch, &session_id)
    };
    Ok(Opened {
        session_id,
        created,
        focus,
    })
}

// --- new -----------------------------------------------------------------------

/// What the `new` prompt starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDefaults {
    pub branch: String,
    pub base: String,
    pub repo_name: String,
}

pub fn new_defaults(ctx: &Context, from_session: &str, seed: u64) -> Result<NewDefaults> {
    let from = binding_of(ctx, from_session)?;
    Ok(NewDefaults {
        branch: git::generated_branch_slug(seed),
        base: "HEAD".to_string(),
        repo_name: from.repo_name,
    })
}

/// A `new` to run in a session of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLaunch {
    /// The session whose repository the worktree is created from.
    pub from: String,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub client: Option<String>,
}

/// Create the session that will own the new worktree, running
/// `rex new … --here` in it. The worktree is bound to it once created.
pub fn launch_new(ctx: &Context, launch: &NewLaunch) -> Result<NewSession> {
    let from = binding_of(ctx, &launch.from)?;
    let mut args = vec!["new"];
    if let Some(branch) = &launch.branch {
        args.push(branch);
    }
    if let Some(base) = &launch.base {
        args.extend(["--base", base]);
    }
    args.extend(["--from", &launch.from, "--here"]);
    if let Some(client) = &launch.client {
        args.extend(["--client", client]);
    }
    let terminal = Terminal {
        cwd: from.checkout_path,
        command: Some(ctx.command(&args)),
    };
    ctx.rex
        .create_session(NEW_SESSION_LABEL, NEW_SESSION_LABEL, &terminal)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    /// Canonical.
    pub path: PathBuf,
    pub branch: String,
    /// This run created the branch (so a rollback may delete it).
    pub new_branch: bool,
}

/// `git worktree add` at `<worktree_directory>/<repo>/<slug>`, from
/// `from_session`'s checkout (`HEAD` is that checkout's HEAD).
pub fn create_worktree(
    ctx: &Context,
    from_session: &str,
    branch: &str,
    base: &str,
) -> Result<CreatedWorktree> {
    let from = binding_of(ctx, from_session)?;
    git::validate_branch_name(&from.checkout_path, branch)?;
    let path =
        git::default_checkout_path(&ctx.settings.worktree_directory, &from.repo_name, branch);
    let added = git::add_worktree(&from.checkout_path, &path, branch, base)?;
    Ok(CreatedWorktree {
        path: path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()))?,
        branch: branch.to_string(),
        new_branch: added == git::Added::NewBranch,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    /// The tab running the setup.
    pub setup_window: String,
    /// The usable shell opened beside it.
    pub shell_window: String,
}

/// Make `session` (where `new --here` runs, in `block`) the worktree's
/// session: bind it, label it after the folder, rename this tab `setup`, and
/// open a focused shell in the worktree so it is usable while setup runs.
pub fn adopt_worktree(
    ctx: &Context,
    session: &str,
    block: &str,
    worktree: &CreatedWorktree,
) -> Result<Adopted> {
    bind(ctx, session, &worktree.path)?;
    ctx.rex
        .set_session_label(session, &basename(&worktree.path))?;
    let setup_window = ctx.rex.window_of_block(session, block)?;
    ctx.rex
        .set_window_label(session, &setup_window, SETUP_WINDOW_LABEL)?;
    let shell = Terminal {
        cwd: worktree.path.clone(),
        command: None,
    };
    let shell_window = ctx
        .rex
        .new_window(session, SHELL_WINDOW_LABEL, &shell, true)?;
    Ok(Adopted {
        setup_window,
        shell_window,
    })
}

#[derive(Debug)]
pub enum SetupOutcome {
    Succeeded(Summary),
    Failed {
        /// The whole error chain, one line.
        error: String,
        /// The written report, when writing it worked.
        report: Option<PathBuf>,
        /// `[notify] when` allows announcing the failure.
        announce: bool,
        /// The worktree was removed (`[failure] action = "remove"`) and this
        /// session closed.
        rolled_back: bool,
    },
}

/// Run the bootstrap lifecycle in the new worktree, with the config and
/// source files of the repository's main checkout.
///
/// Success: a toast in the shell tab, per `[notify] when`. Failure, in Herdr's
/// order: the report is written; with `keep`, the setup tab (which shows it)
/// is focused; with `remove`, the report opens in the origin session (which
/// outlives this one), then the worktree is force-removed, the branch deleted
/// if this run created it and it is merged, the binding dropped, and this
/// session closed last.
///
/// Without `adopted` tabs (`new --wait`), nothing is shown in Rex: no toast,
/// focus, or report; the caller reports the outcome.
pub fn run_setup(
    ctx: &Context,
    session: &str,
    from_session: &str,
    worktree: &CreatedWorktree,
    adopted: Option<&Adopted>,
) -> SetupOutcome {
    let prepared = git::main_checkout(&worktree.path).and_then(|main| {
        let config = config::load(&main, &ctx.settings.plugin_config())
            .context("failed to load worktree sync config")?;
        Ok((main, config))
    });
    let (main, config) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => return keep_failure(ctx, session, worktree, adopted, &err, true),
    };

    let err = match crate::run(&worktree.path, Some(&main), &config) {
        Ok(summary) => {
            let outcome = Outcome::Succeeded(&summary);
            let toast = notify::toast_for(config.notify.when, &worktree.branch, outcome);
            if let (Some(toast), Some(adopted)) = (toast, adopted) {
                let terminal = Terminal {
                    cwd: worktree.path.clone(),
                    command: Some(ui::toast_command(&toast.title, &toast.body, TOAST_SECONDS)),
                };
                let shown = ctx.rex.new_layer(
                    session,
                    Some(&adopted.shell_window),
                    TOAST_BOUNDS,
                    &terminal,
                    false,
                );
                if let Err(e) = shown {
                    crate::progress!("[worktree-sync] could not show the toast: {e:#}");
                }
            }
            return SetupOutcome::Succeeded(summary);
        }
        Err(err) => err,
    };

    let announce = config.notify.when != NotifyWhen::Never;
    if config.failure.action == OnFailure::Keep {
        return keep_failure(ctx, session, worktree, adopted, &err, announce);
    }

    let report = write_report(ctx, worktree, &err);
    if let (Some(report), Some(_)) = (&report, adopted) {
        let cwd = binding_of(ctx, from_session)
            .map(|b| b.checkout_path)
            .unwrap_or_else(|_| main.clone());
        let terminal = Terminal {
            cwd,
            command: Some(ui::report_command(report)),
        };
        if let Err(e) = ctx
            .rex
            .new_layer(from_session, None, DIALOG_BOUNDS, &terminal, true)
        {
            crate::progress!("[rollback] could not show the report: {e:#}");
        }
    }

    crate::progress!("[rollback] removing {}", worktree.path.display());
    if let Err(e) = git::remove_worktree(&main, &worktree.path, true) {
        crate::progress!(
            "[rollback] git refused to remove the worktree, leaving everything in place: {e}"
        );
        if let Some(adopted) = adopted {
            let _ = ctx.rex.focus_window(session, &adopted.setup_window);
        }
        return SetupOutcome::Failed {
            error: format!("{err:#}"),
            report,
            announce,
            rolled_back: false,
        };
    }
    if worktree.new_branch {
        match git::delete_merged_branch(&main, &worktree.branch) {
            Ok(()) => crate::progress!("[rollback] deleted branch {}", worktree.branch),
            Err(e) => crate::progress!("[rollback] kept branch {}: {e:#}", worktree.branch),
        }
    }
    if let Err(e) = unbind(ctx, session) {
        crate::progress!("[rollback] could not drop this session's binding: {e:#}");
    }
    if let Err(e) = ctx.rex.destroy_session(session) {
        crate::progress!("[rollback] could not close this session: {e:#}");
    }
    SetupOutcome::Failed {
        error: format!("{err:#}"),
        report,
        announce,
        rolled_back: true,
    }
}

fn write_report(ctx: &Context, worktree: &CreatedWorktree, err: &anyhow::Error) -> Option<PathBuf> {
    let body = report::compose(&worktree.branch, &worktree.path, err);
    match report::write(&ctx.reports_dir, &worktree.branch, &body) {
        Ok(path) => {
            crate::progress!("[report] full failure report: {}", path.display());
            Some(path)
        }
        Err(e) => {
            crate::progress!("[report] could not write the failure report: {e:#}");
            None
        }
    }
}

fn keep_failure(
    ctx: &Context,
    session: &str,
    worktree: &CreatedWorktree,
    adopted: Option<&Adopted>,
    err: &anyhow::Error,
    announce: bool,
) -> SetupOutcome {
    let report = write_report(ctx, worktree, err);
    if let Some(adopted) = adopted
        && let Err(e) = ctx.rex.focus_window(session, &adopted.setup_window)
    {
        crate::progress!("[worktree-sync] could not focus the setup tab: {e:#}");
    }
    SetupOutcome::Failed {
        error: format!("{err:#}"),
        report,
        announce,
        rolled_back: false,
    }
}

/// The user backed out of the `new` prompt: nothing was created, so the
/// session made for it closes.
pub fn cancel_new(ctx: &Context, session: &str) -> Result<()> {
    ctx.rex.destroy_session(session)
}

/// A worktree [`new_worktree`] set up, in a session of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewWorktree {
    pub session_id: String,
    /// The session's only block: an interactive shell in the worktree.
    pub block_id: String,
    pub checkout_path: PathBuf,
    pub branch: String,
}

/// `new --wait`, for agents and scripts: create the worktree from
/// `from_session`'s checkout (`base` defaults to its `HEAD`), a session in it
/// with one `shell` tab, bound to it, and run the setup here, showing nothing
/// in Rex. A failed setup is kept or rolled back per `[failure] action`, and
/// the error says what is left and where the report is.
pub fn new_worktree(
    ctx: &Context,
    from_session: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<NewWorktree> {
    let created = create_worktree(ctx, from_session, branch, base.unwrap_or("HEAD"))?;
    let path = created.path.display();
    let session = create_bound_session(ctx, &created.path, SHELL_WINDOW_LABEL)
        .with_context(|| format!("{path} was created, but no session could be made for it"))?;
    match run_setup(ctx, &session.session_id, from_session, &created, None) {
        SetupOutcome::Succeeded(_) => Ok(NewWorktree {
            session_id: session.session_id,
            block_id: session.block_id,
            checkout_path: created.path,
            branch: created.branch,
        }),
        SetupOutcome::Failed {
            error,
            report,
            rolled_back,
            ..
        } => {
            let report = match report {
                Some(report) => format!("report: {}", report.display()),
                None => "no report could be written".to_string(),
            };
            let left = if rolled_back {
                format!("{path} was removed")
            } else {
                format!("{path} and its session {} were kept", session.session_id)
            };
            bail!(
                "setting up {} failed: {error} ({report}); {left}",
                created.branch
            )
        }
    }
}

// --- remove --------------------------------------------------------------------

/// What `remove` will do, shown to the user before they confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePlan {
    pub checkout_path: PathBuf,
    pub branch: Option<String>,
    pub main_checkout: PathBuf,
    /// Live sessions bound to the checkout, earliest first; all will close.
    pub sessions: Vec<SessionInfo>,
    /// The session shown afterwards: the earliest bound to the main checkout.
    pub parent: Option<SessionInfo>,
}

pub fn plan_remove(ctx: &Context, session: &str) -> Result<RemovePlan> {
    let binding = binding_of(ctx, session)?;
    require_linked(&binding)?;
    let host = ctx.rex.host_id()?;
    let live = ctx.rex.sessions()?;
    let bindings = ctx.host_bindings(&host)?;
    let sessions = live_bound(bindings.clone(), &live, |b| {
        b.checkout_path == binding.checkout_path
    })
    .into_iter()
    .map(|(s, _)| s)
    .collect();
    let parent = live_bound(bindings, &live, |b| {
        b.repo_key == binding.repo_key && !b.is_linked_worktree
    })
    .into_iter()
    .map(|(s, _)| s)
    .next();
    Ok(RemovePlan {
        main_checkout: git::main_checkout(&binding.checkout_path)?,
        branch: git::current_branch(&binding.checkout_path)?,
        checkout_path: binding.checkout_path,
        sessions,
        parent,
    })
}

#[derive(Debug)]
pub enum RemoveError {
    /// Modified or untracked files: needs consent to force. Nothing changed.
    NeedsForce(String),
    /// Nothing changed unless the message says otherwise.
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// Sessions actually closed, in order.
    pub closed: Vec<String>,
    pub focus: Focus,
}

/// Remove the planned worktree: git first (one `--force` at most, only with
/// `force`), then drop every binding to that checkout, show the parent, and
/// close the confirmed sessions with `current_session` last. The branch is
/// never deleted. A git refusal changes nothing.
pub fn remove(
    ctx: &Context,
    plan: &RemovePlan,
    force: bool,
    current_session: Option<&str>,
    switch: Switch,
) -> Result<Removed, RemoveError> {
    let host = ctx.rex.host_id().map_err(RemoveError::Failed)?;
    git::remove_worktree(&plan.main_checkout, &plan.checkout_path, force).map_err(|e| match e {
        git::RemoveError::NeedsForce(message) => RemoveError::NeedsForce(message),
        git::RemoveError::Failed(message) => RemoveError::Failed(anyhow!(message)),
    })?;

    ctx.bindings
        .update(&host, ctx.live_ids(), |h| {
            h.sessions
                .retain(|_, b| b.checkout_path != plan.checkout_path);
            Ok(())
        })
        .map_err(|e| {
            RemoveError::Failed(e.context(format!(
                "{} was removed, but its sessions' bindings could not be dropped",
                plan.checkout_path.display()
            )))
        })?;

    let focus = match &plan.parent {
        Some(parent) => show(ctx, switch, &parent.session_id),
        None => Focus::NotNeeded,
    };

    let ids = plan.sessions.iter().map(|s| s.session_id.as_str());
    let others = ids.clone().filter(|id| Some(*id) != current_session);
    let current = ids.filter(|id| Some(*id) == current_session);
    let mut closed = Vec::new();
    for id in others.chain(current) {
        match ctx.rex.destroy_session(id) {
            Ok(()) => closed.push(id.to_string()),
            Err(e) => crate::progress!("[remove] could not close session {id}: {e:#}"),
        }
    }
    Ok(Removed { closed, focus })
}

// --- sync ----------------------------------------------------------------------

/// Re-apply copy / clone / symlink from the main checkout to `session`'s
/// linked worktree. Hooks and installs are not re-run, as in Herdr.
pub fn sync(ctx: &Context, session: &str) -> Result<Summary> {
    let binding = binding_of(ctx, session)?;
    require_linked(&binding)?;
    let main = git::main_checkout(&binding.checkout_path)?;
    let config = config::load(&main, &ctx.settings.plugin_config())
        .context("failed to load worktree sync config")?;
    crate::sync_files(&binding.checkout_path, Some(&main), &config)
}
