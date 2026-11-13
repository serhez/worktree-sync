//! Consistency between the two manifests.
//!
//! `Cargo.toml` is read by cargo, `herdr-plugin.toml` by herdr, and neither
//! tool reads the other's file. The overlap between them is therefore
//! hand-maintained, and drifts silently: a version bumped in one place only, or
//! a binary renamed in `Cargo.toml`, still builds fine and only breaks when
//! herdr next fires the event. These tests are the handshake.

use serde::Deserialize;

const PLUGIN_MANIFEST: &str = include_str!("../herdr-plugin.toml");
const CARGO_MANIFEST: &str = include_str!("../Cargo.toml");

#[derive(Deserialize)]
struct Plugin {
    id: String,
    version: String,
    description: String,
    platforms: Vec<String>,
    #[serde(default)]
    build: Vec<Command>,
    #[serde(default)]
    events: Vec<Command>,
    #[serde(default)]
    actions: Vec<Action>,
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct Command {
    command: Vec<String>,
}

#[derive(Deserialize)]
struct Action {
    id: String,
    command: Vec<String>,
}

#[derive(Deserialize)]
struct Pane {
    id: String,
    command: Vec<String>,
}

#[derive(Deserialize)]
struct Cargo {
    package: Package,
    bin: Vec<Bin>,
}

#[derive(Deserialize)]
struct Package {
    description: Option<String>,
}

#[derive(Deserialize)]
struct Bin {
    name: String,
}

fn plugin() -> Plugin {
    toml::from_str(PLUGIN_MANIFEST).expect("herdr-plugin.toml should parse")
}

fn cargo() -> Cargo {
    toml::from_str(CARGO_MANIFEST).expect("Cargo.toml should parse")
}

/// The binary `[[bin]]` produces, of which there must be exactly one — the
/// event command below hardcodes its path.
fn binary_name() -> String {
    let cargo = cargo();
    assert_eq!(
        cargo.bin.len(),
        1,
        "the plugin manifest invokes a single binary by path"
    );
    cargo.bin.into_iter().next().unwrap().name
}

#[test]
fn the_two_manifests_agree_on_the_version() {
    assert_eq!(
        plugin().version,
        env!("CARGO_PKG_VERSION"),
        "herdr-plugin.toml and Cargo.toml must be bumped together"
    );
}

/// `[[events]]` names the binary by path, so renaming `[[bin]]` in Cargo.toml
/// would leave a manifest that builds cleanly and fails at event time.
#[test]
fn the_event_invokes_the_binary_cargo_builds() {
    let plugin = plugin();
    let expected = format!("./target/release/{}", binary_name());

    assert!(!plugin.events.is_empty(), "the plugin declares no events");
    for event in &plugin.events {
        let program = event
            .command
            .first()
            .expect("an event command cannot be empty");
        assert_eq!(
            program, &expected,
            "the event should invoke the binary Cargo.toml builds"
        );
    }
}

#[test]
fn the_sync_action_invokes_the_binary_cargo_builds() {
    let plugin = plugin();
    let expected = format!("./target/release/{}", binary_name());
    let sync = plugin
        .actions
        .iter()
        .find(|action| action.id == "sync")
        .expect("the manifest should expose the sync action");

    assert_eq!(sync.command.first(), Some(&expected));
}

/// The binary hosts several backends and picks one from its first argument,
/// so a command that names the binary but not `herdr` reaches no backend.
#[test]
fn every_herdr_command_selects_the_herdr_backend() {
    let plugin = plugin();
    let expected = [
        format!("./target/release/{}", binary_name()),
        "herdr".to_string(),
    ];
    let commands = plugin
        .events
        .iter()
        .map(|event| &event.command)
        .chain(plugin.actions.iter().map(|action| &action.command));

    for command in commands {
        assert_eq!(command.as_slice(), expected.as_slice());
    }
}

/// herdr keys installs, plugin config directories and keybindings
/// (`serhez.herdr.worktree.sync.sync`) on the id, so changing it would orphan
/// every existing install's settings.
#[test]
fn the_plugin_id_is_unchanged() {
    assert_eq!(plugin().id, "serhez.herdr.worktree.sync");
}

/// The event path points into `target/release`, which only exists if the
/// manifest actually asks for a release build.
#[test]
fn the_manifest_builds_the_release_binary_it_invokes() {
    let plugin = plugin();
    assert!(
        plugin
            .build
            .iter()
            .any(|step| step.command == ["cargo", "build", "--release"]),
        "no `cargo build --release` step, but the event runs target/release/"
    );
}

/// The pane the plugin opens on a failure, named by a string on both sides:
/// `herdr plugin pane open --entrypoint` in Rust, `[[panes]] id` here. herdr
/// resolves them at runtime, so a rename shows up as "no such entrypoint" in a
/// log nobody reads, at the one moment the user most needs the pane.
#[test]
fn the_failure_pane_entrypoint_exists_in_the_manifest() {
    let plugin = plugin();
    assert!(
        plugin
            .panes
            .iter()
            .any(|pane| pane.id == worktree_sync::backends::herdr::pane::PANE_ENTRYPOINT),
        "no `[[panes]]` entry with id `{}`",
        worktree_sync::backends::herdr::pane::PANE_ENTRYPOINT
    );
}

/// The pane command is fixed in the manifest, so the only way it learns which
/// report to show is this env var — which the plugin sets and the manifest
/// expands, with nothing but spelling holding the two together.
#[test]
fn the_failure_pane_reads_the_env_var_the_plugin_sets() {
    let var = worktree_sync::backends::herdr::pane::REPORT_PATH_VAR;
    let pane = plugin()
        .panes
        .into_iter()
        .find(|pane| pane.id == worktree_sync::backends::herdr::pane::PANE_ENTRYPOINT)
        .expect("the failure pane should exist");

    assert!(
        pane.command.iter().any(|arg| arg.contains(var)),
        "the pane command never mentions ${var}: {:?}",
        pane.command
    );
}

/// Windows was dropped deliberately: the integration tests shell out to `sh`,
/// and the event command has no `.exe` suffix to offer it.
#[test]
fn the_declared_platforms_are_the_ones_ci_covers() {
    assert_eq!(plugin().platforms, ["linux", "macos"]);
}

/// Two audiences see a description — crates.io readers see Cargo.toml's, herdr
/// users see the plugin manifest's — and nothing else makes them match.
///
/// `description` stays optional here because it is optional to cargo: a fork
/// that drops it should not fail the suite, only lose the check.
#[test]
fn the_descriptions_agree_once_cargo_declares_one() {
    if let Some(description) = cargo().package.description {
        assert_eq!(description, plugin().description);
    }
}
