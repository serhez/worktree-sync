//! What the Rex backend shows: program status (OSC 7501), layer geometry, and
//! the commands behind toasts and failure reports.

use std::path::Path;
use std::process::Command;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use worktree_sync::backends::rex::ui::{
    DIALOG_BOUNDS, SYNC_BOUNDS, Status, TOAST_BOUNDS, program_status, report_command, toast_command,
};

fn b64(text: &str) -> String {
    STANDARD.encode(text)
}

#[test]
fn program_status_uses_rexs_protocol() {
    assert_eq!(
        program_status(Status::Working, None),
        "\x1b]7501;state=working:app=worktree-sync\x1b\\"
    );
    assert_eq!(
        program_status(Status::Error, Some("hook failed")),
        format!(
            "\x1b]7501;state=error:app=worktree-sync:msg={}\x1b\\",
            b64("hook failed")
        )
    );
    assert_eq!(
        program_status(Status::Done, Some("ok")),
        format!(
            "\x1b]7501;state=done:app=worktree-sync:msg={}\x1b\\",
            b64("ok")
        )
    );
    assert_eq!(
        program_status(Status::Clear, None),
        "\x1b]7501;state=clear:app=worktree-sync\x1b\\"
    );
}

/// The message is a one-line summary; anything that could end or corrupt the
/// escape sequence (newlines, ESC, BEL) never reaches the terminal raw, and
/// only the first line is kept.
#[test]
fn program_status_messages_are_one_clean_line() {
    assert_eq!(
        program_status(Status::Error, Some("first line\nsecond line")),
        format!(
            "\x1b]7501;state=error:app=worktree-sync:msg={}\x1b\\",
            b64("first line")
        )
    );
    assert_eq!(
        program_status(Status::Error, Some("a\x1b]0;evil\x07b")),
        format!(
            "\x1b]7501;state=error:app=worktree-sync:msg={}\x1b\\",
            b64("a]0;evilb")
        )
    );
}

#[test]
fn layers_fit_on_screen() {
    for (name, [x, y, w, h]) in [
        ("toast", TOAST_BOUNDS),
        ("dialog", DIALOG_BOUNDS),
        ("sync", SYNC_BOUNDS),
    ] {
        assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0, "{name}");
        assert!(x + w <= 1.0 && y + h <= 1.0, "{name} overflows");
    }
}

/// A toast sits in the top-right corner, out of the way of the prompt.
#[test]
fn the_toast_is_top_right() {
    let [x, y, _, _] = TOAST_BOUNDS;
    assert!(x >= 0.5 && y < 0.1);
}

/// Title and body are arguments, never spliced into shell text: a branch or an
/// error message can contain anything.
#[test]
fn the_toast_prints_title_and_body_verbatim() {
    let title = "Worktree sync done · feat/$(touch pwned)";
    let body = "copied 2 files\n'quoted' \"double\" `tick` $HOME";

    let argv = toast_command(title, body, 0);

    assert!(
        !argv.iter().any(|a: &String| a != title
            && a != body
            && (a.contains("pwned") || a.contains("$HOME"))),
        "title/body must only appear as whole arguments: {argv:?}"
    );
    let out = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{title}\n\n{body}\n")
    );
}

#[test]
fn the_toast_stays_for_the_given_time() {
    let argv = toast_command("t", "b", 5).join(" ");
    assert!(argv.contains("sleep 5"), "{argv}");
}

/// The report path is an argument too (state dirs can contain spaces).
#[test]
fn the_report_command_takes_the_path_as_one_argument() {
    let path = Path::new("/tmp/state dir/$x/failure-feat-x.log");
    let argv = report_command(path);
    assert!(
        argv.iter().any(|a: &String| a == path.to_str().unwrap()),
        "{argv:?}"
    );
    assert!(
        !argv
            .iter()
            .any(|a: &String| a != path.to_str().unwrap() && a.contains("failure-feat-x")),
        "{argv:?}"
    );
}
