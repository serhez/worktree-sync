//! Deliver the end-of-run notification as a herdr toast.
//!
//! herdr captures the plugin's stdout rather than printing it, so a run is
//! invisible unless someone goes digging with `herdr plugin log list`. A toast
//! is the only channel that reaches the user at the moment the worktree
//! appears, which is the moment they can still act on a half-bootstrapped one.

use serde::Deserialize;

use super::bin;
use crate::notify::Toast;

/// Post the toast.
///
/// Never fails the bootstrap: herdr being unreachable, or refusing the request,
/// is worth a line in the log but not a worktree left unusable over a cosmetic
/// step that runs after all the real work is done.
pub fn show(toast: &Toast) {
    let output = bin::command()
        .args(["notification", "show", &toast.title])
        .args(["--body", &toast.body])
        .args(["--sound", toast.sound])
        .output();

    match output {
        Err(err) => println!("[notify] could not run `herdr`: {err}"),
        Ok(output) if !output.status.success() => println!(
            "[notify] herdr rejected the toast: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Ok(output) => report_suppression(&output.stdout),
    }
}

#[derive(Deserialize)]
struct ShowResponse {
    result: ShowResult,
}

#[derive(Deserialize)]
struct ShowResult {
    shown: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// herdr answers a suppressed toast with `shown: false` *and exit code 0*, so
/// without this the feature looks broken rather than switched off.
///
/// The reason is echoed verbatim because herdr owns that vocabulary and we
/// would only go stale guessing at it — `disabled` is the one we can act on
/// (toasts are off in herdr's default config, so it is the expected first
/// experience), while `busy` and friends are herdr deciding the user is
/// already looking at the thing.
fn report_suppression(stdout: &[u8]) {
    let Ok(response) = serde_json::from_slice::<ShowResponse>(stdout) else {
        return;
    };
    if response.result.shown {
        return;
    }

    let reason = response.result.reason.as_deref().unwrap_or("unknown");
    let hint = match reason {
        "disabled" => " — set `[ui.toast] delivery` in herdr's config.toml to see it",
        _ => "",
    };
    println!("[notify] herdr did not show the toast (reason: {reason}){hint}");
}
