//! What the Rex backend shows: program status (OSC 7501), floating-layer
//! geometry, the commands behind toasts and reports, and the prompts that run
//! inside terminal blocks.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use inquire::InquireError;

use super::api::Bounds;

/// Program states Rex shows on tabs and in the session picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Spinner while long work runs.
    Working,
    Done,
    /// Stays until the terminal closes.
    Error,
    Clear,
}

/// Rex's Program Status Protocol sequence for this tool. The message is
/// reduced to its first line without control characters, so it can't end or
/// corrupt the escape sequence.
pub fn program_status(status: Status, message: Option<&str>) -> String {
    let state = match status {
        Status::Working => "working",
        Status::Done => "done",
        Status::Error => "error",
        Status::Clear => "clear",
    };
    let mut sequence = format!("\x1b]7501;state={state}:app=worktree-sync");
    if let Some(message) = message {
        let line: String = message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        sequence.push_str(":msg=");
        sequence.push_str(&STANDARD.encode(line));
    }
    sequence.push_str("\x1b\\");
    sequence
}

/// Print a program status to this terminal.
pub fn emit(status: Status, message: Option<&str>) {
    print!("{}", program_status(status, message));
    let _ = std::io::stdout().flush();
}

/// Top right, out of the way of the shell prompt.
pub const TOAST_BOUNDS: Bounds = [0.55, 0.03, 0.42, 0.30];
/// Centered, for pickers, confirmations, and reports.
pub const DIALOG_BOUNDS: Bounds = [0.15, 0.2, 0.7, 0.6];
/// Lower band, for a short sync log.
pub const SYNC_BOUNDS: Bounds = [0.2, 0.6, 0.6, 0.35];

/// Print `title` and `body`, then wait `seconds`. Both travel as arguments,
/// never as shell text: a branch or an error message can contain anything.
pub fn toast_command(title: &str, body: &str, seconds: u32) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(r#"printf '%s\n\n%s\n' "$1" "$2"; sleep {seconds}"#),
        "sh".into(),
        title.into(),
        body.into(),
    ]
}

/// Show a report file scrollably, as Herdr's failure pane does: `less`, or
/// `cat` plus "press enter" where `less` is missing.
pub fn report_command(report: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        r#"less -R -- "$1" || { cat -- "$1"; printf '\npress enter to close '; read _; }"#.into(),
        "sh".into(),
        report.to_string_lossy().into_owned(),
    ]
}

/// Whether a person can answer prompts here.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// `None` when the user backs out (Esc or Ctrl-C).
fn answered<T>(result: Result<T, InquireError>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A line of text, prefilled with `initial`.
pub fn text(message: &str, initial: &str, help: Option<&str>) -> Result<Option<String>> {
    let mut prompt = inquire::Text::new(message).with_initial_value(initial);
    if let Some(help) = help {
        prompt = prompt.with_help_message(help);
    }
    answered(prompt.prompt())
}

/// A filterable choice; the index of the picked option.
pub fn select(message: &str, options: Vec<String>) -> Result<Option<usize>> {
    answered(inquire::Select::new(message, options).raw_prompt()).map(|o| o.map(|o| o.index))
}

/// A `[y/N]` question.
pub fn confirm(message: &str) -> Result<bool> {
    Ok(answered(inquire::Confirm::new(message).with_default(false).prompt())?.unwrap_or(false))
}

/// Keep a terminal (and its message) on screen until Enter.
pub fn wait_for_enter() {
    print!("\npress enter to close ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut String::new());
}
