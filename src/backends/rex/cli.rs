//! `worktree-sync rex …`: the commands the Lua module and the terminal blocks
//! it opens run, also usable from any shell. Prompts live here; the decisions
//! live in [`super::workflows`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use super::api::RexCli;
use super::bindings::{self, Store};
use super::settings::{self, Settings};
use super::ui::{self, Status};
use super::workflows::{
    self as wf, ActionRequest, Context, Focus, NewLaunch, RemoveError, SetupOutcome, Verb,
};
use crate::notify::{self, Outcome};
use crate::{config, report};

/// The Lua module, shipped inside the binary so the two always match.
const LUA_MODULE: &str = include_str!("../../../lua/worktree_sync.lua");

/// Worktree lifecycle for Rex sessions.
#[derive(Parser)]
#[command(name = "rex")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the session of a checkout, creating and binding one if needed.
    /// Without PATH, pick among the worktrees of --from's repository.
    Open {
        path: Option<PathBuf>,
        /// The session whose repository to pick from.
        #[arg(long, env = "REX_SESSION")]
        from: Option<String>,
        /// The Rex app to switch.
        #[arg(long)]
        client: Option<String>,
    },
    /// Create a worktree from --from's repository, in a session of its own.
    New {
        branch: Option<String>,
        /// Start a new branch here (default HEAD of --from's checkout).
        #[arg(long)]
        base: Option<String>,
        #[arg(long, env = "REX_SESSION")]
        from: Option<String>,
        /// Run in the current session, which becomes the worktree's session.
        #[arg(long)]
        here: bool,
        #[arg(long)]
        client: Option<String>,
    },
    /// Remove a session's linked worktree and close its sessions. The branch
    /// is kept.
    Remove {
        #[arg(long, env = "REX_SESSION")]
        session: String,
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Also delete modified and untracked files in the worktree.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        client: Option<String>,
    },
    /// Bind a session to the checkout containing PATH (default: the cwd).
    Bind {
        path: Option<PathBuf>,
        #[arg(long, env = "REX_SESSION")]
        session: String,
    },
    /// Drop a session's binding.
    Unbind {
        #[arg(long, env = "REX_SESSION")]
        session: String,
    },
    /// Re-apply copy, clone, and symlink to a session's linked worktree.
    Sync {
        #[arg(long, env = "REX_SESSION")]
        session: String,
    },
    /// Print the Lua module for ~/.config/rex.
    Lua,
    /// Entry point of the Lua actions: validate, launch, print JSON.
    #[command(hide = true)]
    Action {
        verb: VerbArg,
        #[arg(long)]
        session: String,
        #[arg(long)]
        block: Option<String>,
        #[arg(long)]
        client: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum VerbArg {
    Open,
    New,
    Remove,
    Bind,
    Sync,
}

impl From<VerbArg> for Verb {
    fn from(verb: VerbArg) -> Self {
        match verb {
            VerbArg::Open => Verb::Open,
            VerbArg::New => Verb::New,
            VerbArg::Remove => Verb::Remove,
            VerbArg::Bind => Verb::Bind,
            VerbArg::Sync => Verb::Sync,
        }
    }
}

/// The `rex` command tree, for parsing and for tests.
pub fn command() -> clap::Command {
    Cli::command()
}

/// Everything the commands need, resolved from the environment.
struct Env {
    rex: RexCli,
    settings: Settings,
    store: Store,
    reports_dir: PathBuf,
    bin: PathBuf,
    home: PathBuf,
}

impl Env {
    fn load() -> Result<Self> {
        let home_var = std::env::var_os("HOME");
        let home = PathBuf::from(home_var.clone().context("HOME is not set")?);
        let config_dir = settings::config_dir_from_env(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            home_var.as_deref(),
        )?;
        let path = bindings::path_from_env(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            home_var.as_deref(),
        )?;
        let reports_dir = path
            .parent()
            .context("the bindings path has no directory")?
            .join("reports");
        Ok(Self {
            rex: RexCli::from_env()?,
            settings: Settings::load(&config_dir, &home)?,
            store: Store::new(path),
            reports_dir,
            bin: std::env::current_exe().context("locating this binary")?,
            home,
        })
    }

    fn ctx(&self) -> Context<'_> {
        Context {
            rex: &self.rex,
            settings: &self.settings,
            bindings: &self.store,
            reports_dir: self.reports_dir.clone(),
            bin: self.bin.clone(),
            home: self.home.clone(),
        }
    }
}

/// The session this process runs in, when it runs inside Rex.
fn own_session() -> Option<String> {
    std::env::var("REX_SESSION").ok().filter(|s| !s.is_empty())
}

/// Run `worktree-sync rex …`; `args` starts with the `rex` word.
pub fn main(args: impl IntoIterator<Item = OsString>) -> Result<()> {
    let cli = Cli::parse_from(args);
    if let Cmd::Lua = cli.command {
        print!("{LUA_MODULE}");
        return Ok(());
    }
    let env = Env::load()?;
    let ctx = env.ctx();
    match cli.command {
        Cmd::Lua => unreachable!("handled above"),
        Cmd::Action {
            verb,
            session,
            block,
            client,
        } => {
            let outcome = wf::action(
                &ctx,
                &ActionRequest {
                    verb: verb.into(),
                    session,
                    block,
                    client,
                },
            )?;
            println!("{}", serde_json::to_string(&outcome)?);
            Ok(())
        }
        Cmd::Open { path, from, client } => open(&ctx, path, from, client),
        Cmd::New {
            branch,
            base,
            from,
            here,
            client,
        } => {
            let from = from.context("--from is required outside Rex")?;
            if here {
                new_here(&ctx, branch, base, &from)
            } else {
                new_elsewhere(&ctx, branch, base, from, client)
            }
        }
        Cmd::Remove {
            session,
            yes,
            force,
            client,
        } => remove(&ctx, &session, yes, force, client.as_deref()),
        Cmd::Bind { path, session } => bind(&ctx, path, &session),
        Cmd::Unbind { session } => {
            match wf::unbind(&ctx, &session)? {
                Some(old) => println!("Unbound {session} from {}", old.checkout_path.display()),
                None => println!("{session} was not bound"),
            }
            Ok(())
        }
        Cmd::Sync { session } => sync(&ctx, &session),
    }
}

/// Tell the user where to go when Rex refused to switch.
fn report_focus(focus: &Focus, label: &str) {
    if let Focus::Refused(message) = focus {
        println!("Go to session \"{label}\": {message}");
    }
}

fn session_label(ctx: &Context, session: &str) -> String {
    ctx.rex
        .sessions()
        .ok()
        .and_then(|all| all.into_iter().find(|s| s.session_id == session))
        .map(|s| s.label)
        .unwrap_or_else(|| session.to_string())
}

fn open(
    ctx: &Context,
    path: Option<PathBuf>,
    from: Option<String>,
    client: Option<String>,
) -> Result<()> {
    let current = own_session();
    let path = match path {
        Some(path) => path,
        None => {
            let from =
                from.context("give a PATH, or --from SESSION to pick from its repository")?;
            if !ui::is_interactive() {
                bail!("picking a worktree needs a terminal; give a PATH");
            }
            let candidates = wf::open_candidates(ctx, &from)?;
            let options = candidates
                .iter()
                .map(|c| {
                    let sessions: Vec<_> = c.sessions.iter().map(|s| s.label.as_str()).collect();
                    let bound = if sessions.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", sessions.join(", "))
                    };
                    format!("{}  {}{bound}", c.label, c.checkout_path.display())
                })
                .collect();
            match ui::select("Open worktree", options)? {
                Some(index) => candidates[index].checkout_path.clone(),
                None => return Ok(()),
            }
        }
    };
    let opened = wf::open_checkout(ctx, &path, current.as_deref(), client.as_deref())?;
    if let Focus::Refused(_) = &opened.focus {
        report_focus(&opened.focus, &session_label(ctx, &opened.session_id));
        if ui::is_interactive() {
            ui::wait_for_enter();
        }
    } else if current.is_none() {
        println!("{}", opened.session_id);
    }
    Ok(())
}

/// `new` from outside the worktree's session: create that session and run
/// `new --here` in it.
fn new_elsewhere(
    ctx: &Context,
    branch: Option<String>,
    base: Option<String>,
    from: String,
    client: Option<String>,
) -> Result<()> {
    if let Some(branch) = &branch {
        let defaults = wf::new_defaults(ctx, &from, 0)?;
        let target = crate::git::default_checkout_path(
            &ctx.settings.worktree_directory,
            &defaults.repo_name,
            branch,
        );
        if target.exists() {
            bail!("{} already exists", target.display());
        }
    }
    let new = wf::launch_new(
        ctx,
        &NewLaunch {
            from,
            branch,
            base,
            client: client.clone(),
        },
    )?;
    let focus = match ctx.rex.select_session(client.as_deref(), &new.session_id) {
        Ok(()) => Focus::Switched,
        Err(e) => Focus::Refused(format!("{e:#}")),
    };
    println!("{}", new.session_id);
    report_focus(&focus, "new worktree");
    Ok(())
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or_default()
}

/// `new --here`: runs in the session that will own the worktree. Prompts for
/// what wasn't given, creates the worktree (re-prompting on a git error), adopts
/// it, and runs the setup.
fn new_here(
    ctx: &Context,
    mut branch: Option<String>,
    mut base: Option<String>,
    from: &str,
) -> Result<()> {
    let session = own_session().context("--here runs inside a Rex session")?;
    let block = std::env::var("REX_BLOCK").context("--here runs inside a Rex block")?;
    let interactive = ui::is_interactive();
    let defaults = wf::new_defaults(ctx, from, seed())?;
    let pattern = ctx
        .settings
        .worktree_directory
        .join(&defaults.repo_name)
        .join("<branch>");

    let created = loop {
        let Some(chosen_branch) = (match branch.take() {
            Some(branch) => Some(branch),
            None if interactive => ui::text(
                "Branch",
                &defaults.branch,
                Some(&format!("created at {}", pattern.display())),
            )?,
            None => bail!("give a BRANCH (no terminal to ask)"),
        }) else {
            return wf::cancel_new(ctx, &session);
        };
        let path = crate::git::default_checkout_path(
            &ctx.settings.worktree_directory,
            &defaults.repo_name,
            &chosen_branch,
        );
        let Some(chosen_base) = (match base.take() {
            Some(base) => Some(base),
            None if interactive => ui::text(
                "Base (for a new branch)",
                &defaults.base,
                Some(&format!("worktree: {}", path.display())),
            )?,
            None => Some(defaults.base.clone()),
        }) else {
            return wf::cancel_new(ctx, &session);
        };
        match wf::create_worktree(ctx, from, &chosen_branch, &chosen_base) {
            Ok(created) => break created,
            Err(e) if interactive => println!("\n{e:#}\n"),
            Err(e) => return Err(e),
        }
    };
    println!("[worktree-sync] worktree: {}", created.path.display());
    println!("[worktree-sync] branch:   {}", created.branch);
    if !created.new_branch {
        println!("[worktree-sync] checked out the existing branch (base ignored)");
    }

    let adopted = match wf::adopt_worktree(ctx, &session, &block, &created) {
        Ok(adopted) => adopted,
        Err(e) => {
            println!("\nThe worktree was created but this session could not take it over: {e:#}");
            println!(
                "Bind it by hand: worktree-sync rex bind {} --session {session}",
                created.path.display()
            );
            if interactive {
                ui::wait_for_enter();
            }
            bail!("{e:#}");
        }
    };

    ui::emit(Status::Working, None);
    match wf::run_setup(ctx, &session, from, &created, &adopted) {
        SetupOutcome::Succeeded(_) => {
            ui::emit(Status::Clear, None);
            println!("[worktree-sync] done");
            Ok(())
        }
        SetupOutcome::Failed {
            error,
            report,
            announce,
            rolled_back,
        } => {
            // A rollback closed this session; nothing below would be seen.
            if !rolled_back {
                show_failure(report.as_deref(), &error);
                if announce {
                    ui::emit(Status::Error, Some(&error));
                } else {
                    ui::emit(Status::Clear, None);
                }
                if interactive {
                    ui::wait_for_enter();
                }
            }
            bail!("{error}");
        }
    }
}

fn show_failure(report: Option<&Path>, error: &str) {
    let body = report.and_then(|path| std::fs::read_to_string(path).ok());
    println!("\n{}", body.as_deref().unwrap_or(error));
}

fn remove(
    ctx: &Context,
    session: &str,
    yes: bool,
    force: bool,
    client: Option<&str>,
) -> Result<()> {
    let plan = wf::plan_remove(ctx, session)?;
    let interactive = ui::is_interactive();
    if !yes {
        if !interactive {
            bail!(
                "refusing to remove {} without confirmation (no terminal): pass --yes",
                plan.checkout_path.display()
            );
        }
        println!("Remove worktree {}", plan.checkout_path.display());
        println!(
            "  branch:   {} (kept)",
            plan.branch.as_deref().unwrap_or("(detached)")
        );
        if !plan.sessions.is_empty() {
            println!("  These sessions will be closed and their programs ended:");
            for s in &plan.sessions {
                println!("    {}", s.label);
            }
        }
        if !ui::confirm("Remove it?")? {
            return Ok(());
        }
    }
    let current = own_session();
    let result = match wf::remove(ctx, &plan, force, current.as_deref(), client) {
        Err(RemoveError::NeedsForce(message)) => {
            if !interactive {
                bail!(
                    "{} has modified or untracked files ({message}); pass --force to delete them",
                    plan.checkout_path.display()
                );
            }
            let question = format!(
                "{} has modified or untracked files. Delete them permanently?",
                plan.checkout_path.display()
            );
            if !ui::confirm(&question)? {
                return Ok(());
            }
            wf::remove(ctx, &plan, true, current.as_deref(), client)
        }
        other => other,
    };
    match result {
        Ok(removed) => {
            if let Some(parent) = &plan.parent {
                report_focus(&removed.focus, &parent.label);
            }
            println!("Removed {}", plan.checkout_path.display());
            Ok(())
        }
        Err(RemoveError::NeedsForce(message)) => bail!("{message}"),
        Err(RemoveError::Failed(e)) => Err(e),
    }
}

fn bind(ctx: &Context, path: Option<PathBuf>, session: &str) -> Result<()> {
    let prompted = path.is_none() && ui::is_interactive();
    let path = match path {
        Some(path) => path,
        None => {
            let cwd = std::env::current_dir().context("reading the current directory")?;
            if prompted {
                let help = "the checkout (or any folder in it) this session works on";
                match ui::text("Bind this session to", &cwd.to_string_lossy(), Some(help))? {
                    Some(answer) => PathBuf::from(answer),
                    None => return Ok(()),
                }
            } else {
                cwd
            }
        }
    };
    let host = ctx.rex.host_id()?;
    let old = ctx
        .bindings
        .read()?
        .hosts
        .remove(&host)
        .and_then(|mut h| h.sessions.remove(session));
    let binding = wf::bind(ctx, session, &path)?;
    match old {
        Some(old) if old.checkout_path != binding.checkout_path => println!(
            "Rebound {} → {}",
            old.checkout_path.display(),
            binding.checkout_path.display()
        ),
        _ => println!("Bound to {}", binding.checkout_path.display()),
    }
    if prompted {
        std::thread::sleep(Duration::from_millis(1500));
    }
    Ok(())
}

fn sync(ctx: &Context, session: &str) -> Result<()> {
    let interactive = ui::is_interactive();
    match wf::sync(ctx, session) {
        Ok(summary) => {
            match notify::toast_for(
                config::NotifyWhen::Always,
                "file sync",
                Outcome::Succeeded(&summary),
            ) {
                Some(toast) => println!("{}\n\n{}", toast.title, toast.body),
                None => println!("Nothing to sync: no copy, clone, or symlink is configured."),
            }
            if interactive {
                std::thread::sleep(Duration::from_secs(3));
            }
            Ok(())
        }
        Err(e) => {
            println!(
                "\n{}",
                report::compose(
                    "file sync",
                    &std::env::current_dir().unwrap_or_default(),
                    &e
                )
            );
            if interactive {
                ui::wait_for_enter();
            }
            Err(e)
        }
    }
}
