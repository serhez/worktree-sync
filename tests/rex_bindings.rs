//! The binding file: the contract other tools (Canopy) read, and the writer
//! rules that keep it intact under concurrent writers.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use common::{Dir, git};
use worktree_sync::backends::rex::bindings::{
    Binding, BindingsFile, HostBindings, Store, VERSION, path_from_env, to_json,
};

const FIXTURE: &str = include_str!("fixtures/bindings-v1.json");
const HOST: &str = "host-a";

fn binding(checkout: &str, linked: bool, bound_at: u64) -> Binding {
    Binding {
        repo_key: PathBuf::from("/r/app/.git"),
        repo_name: "app".into(),
        checkout_path: PathBuf::from(checkout),
        branch: Some("main".into()),
        is_linked_worktree: linked,
        bound_at,
    }
}

fn store(dir: &Dir) -> Store {
    Store::new(dir.path().join("rex/bindings.json")).with_lock_timeout(Duration::from_secs(5))
}

fn no_listing() -> anyhow::Result<Vec<String>> {
    Err(anyhow!("rex is not reachable"))
}

fn insert(store: &Store, host: &str, session: &str, b: Binding) {
    store
        .update(host, no_listing, |h| {
            h.sessions.insert(session.into(), b);
            Ok(())
        })
        .unwrap();
}

fn sessions(store: &Store, host: &str) -> Vec<String> {
    store
        .read()
        .unwrap()
        .hosts
        .get(host)
        .map(|h| h.sessions.keys().cloned().collect())
        .unwrap_or_default()
}

// --- Schema --------------------------------------------------------------------

#[test]
fn the_schema_version_is_1() {
    assert_eq!(VERSION, 1);
    assert_eq!(BindingsFile::default().version, 1);
    assert!(BindingsFile::default().hosts.is_empty());
}

/// The fixture is the published example: it must parse to exactly these
/// values and serialize back byte for byte.
#[test]
fn the_fixture_round_trips_exactly() {
    let file: BindingsFile = serde_json::from_str(FIXTURE).unwrap();

    let host = &file.hosts["fcf58739-fba5-4b01-b6cf-022cd6ea6b06"];
    assert_eq!(
        host.sessions["session:01a1d1dc-1bdf-7111-bc71-452288dfffa8"],
        Binding {
            repo_key: "/Users/ser/dev/app/.git".into(),
            repo_name: "app".into(),
            checkout_path: "/Users/ser/dev/.worktrees/app/feat-login".into(),
            branch: Some("feat/login".into()),
            is_linked_worktree: true,
            bound_at: 1794522240,
        }
    );
    let parent = &host.sessions["session:01a1d1dc-2c4e-7a10-9d3b-1f0e5a6c7b8d"];
    assert_eq!(parent.branch, None);
    assert!(!parent.is_linked_worktree);

    assert_eq!(to_json(&file), FIXTURE);
}

/// `docs/rex-bindings.md` is the contract readers implement against; its
/// example must be the tested fixture, not a hand-edited copy.
#[test]
fn the_contract_doc_embeds_the_fixture_verbatim() {
    let doc =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/rex-bindings.md"))
            .expect("docs/rex-bindings.md should exist");
    assert!(
        doc.contains(&format!("```json\n{FIXTURE}```")),
        "docs/rex-bindings.md must contain tests/fixtures/bindings-v1.json in a ```json block"
    );
}

/// Readers rely on the key being present: a detached checkout is `null`, not
/// a missing field.
#[test]
fn a_detached_branch_is_written_as_null() {
    let mut file = BindingsFile::default();
    let mut b = binding("/r/app", false, 1);
    b.branch = None;
    file.hosts.insert(
        HOST.into(),
        HostBindings {
            sessions: BTreeMap::from([("s".to_string(), b)]),
        },
    );
    assert!(to_json(&file).contains("\"branch\": null"));
}

/// Additive fields don't bump the version, so this version must read files
/// that carry fields it doesn't know.
#[test]
fn unknown_fields_are_ignored_when_reading() {
    let dir = Dir::new();
    let store = store(&dir);
    dir.write(
        "rex/bindings.json",
        r#"{"version":1,"written_by":"future","hosts":{"h":{"color":"red","sessions":{"s":{
            "repo_key":"/r/.git","repo_name":"r","checkout_path":"/r","branch":"main",
            "is_linked_worktree":false,"bound_at":5,"pinned":true}}}}}"#,
    );

    let file = store.read().unwrap();

    assert_eq!(
        file.hosts["h"].sessions["s"].checkout_path,
        PathBuf::from("/r")
    );
    assert_eq!(file.hosts["h"].sessions["s"].bound_at, 5);
}

#[test]
fn a_missing_file_reads_as_no_bindings() {
    let dir = Dir::new();
    assert_eq!(store(&dir).read().unwrap(), BindingsFile::default());
}

// --- Path ----------------------------------------------------------------------

#[test]
fn the_path_follows_xdg_state_home_only_when_absolute() {
    let home = Some(OsStr::new("/home/me"));
    assert_eq!(
        path_from_env(Some(OsStr::new("/xdg/state")), home).unwrap(),
        PathBuf::from("/xdg/state/worktree-sync/rex/bindings.json")
    );
    for ignored in [
        Some(OsStr::new("relative/state")),
        Some(OsStr::new("")),
        None,
    ] {
        assert_eq!(
            path_from_env(ignored, home).unwrap(),
            PathBuf::from("/home/me/.local/state/worktree-sync/rex/bindings.json"),
            "XDG_STATE_HOME={ignored:?}"
        );
    }
    assert_eq!(
        path_from_env(Some(OsStr::new("/xdg/state")), None).unwrap(),
        PathBuf::from("/xdg/state/worktree-sync/rex/bindings.json")
    );
    assert!(path_from_env(None, None).is_err());
}

// --- Writing -------------------------------------------------------------------

#[test]
fn an_update_returns_the_edits_value_and_persists_it() {
    let dir = Dir::new();
    let store = store(&dir);

    let value = store
        .update(HOST, no_listing, |h| {
            h.sessions.insert("s1".into(), binding("/r/app", false, 10));
            Ok(42)
        })
        .unwrap();

    assert_eq!(value, 42);
    let file = store.read().unwrap();
    assert_eq!(file.version, VERSION);
    assert_eq!(
        file.hosts[HOST].sessions["s1"],
        binding("/r/app", false, 10)
    );
    assert_eq!(
        std::fs::read_to_string(store.path()).unwrap(),
        to_json(&file),
        "the writer emits the canonical format"
    );
}

#[test]
fn a_failed_edit_writes_nothing() {
    let dir = Dir::new();
    let store = store(&dir);
    insert(&store, HOST, "s1", binding("/r/app", false, 10));
    let before = std::fs::read(store.path()).unwrap();

    let err = store
        .update(HOST, no_listing, |h| -> anyhow::Result<()> {
            h.sessions.clear();
            Err(anyhow!("changed my mind"))
        })
        .unwrap_err();

    assert!(format!("{err:#}").contains("changed my mind"));
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn the_state_dir_and_file_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new();
    let store = store(&dir);
    insert(&store, HOST, "s1", binding("/r/app", false, 1));

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(store.path()), 0o600);
    assert_eq!(mode(store.path().parent().unwrap()), 0o700);
}

/// Atomic replace: the temp file is renamed over the target, so none is left
/// behind, and a stale one from a crashed writer doesn't get in the way.
#[test]
fn writes_leave_no_temp_files() {
    let dir = Dir::new();
    let store = store(&dir);
    dir.write("rex/bindings.json.99999.tmp", "{ half a docum");

    insert(&store, HOST, "s1", binding("/r/app", false, 1));
    insert(&store, HOST, "s2", binding("/r/app", false, 2));

    let ours = format!("bindings.json.{}.tmp", std::process::id());
    let names: Vec<String> = std::fs::read_dir(dir.path().join("rex"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!names.contains(&ours), "{names:?}");
    assert_eq!(sessions(&store, HOST), ["s1", "s2"]);
}

// --- Refusing what it can't read ----------------------------------------------

fn assert_refused(contents: &str, expect: &str) {
    let dir = Dir::new();
    let store = store(&dir);
    dir.write("rex/bindings.json", contents);

    let read_err = store.read().unwrap_err();
    assert!(format!("{read_err:#}").contains(expect), "{read_err:#}");

    let edited = AtomicBool::new(false);
    let err = store
        .update(HOST, no_listing, |h| {
            edited.store(true, Ordering::SeqCst);
            h.sessions.insert("s".into(), binding("/r/app", false, 1));
            Ok(())
        })
        .unwrap_err();
    assert!(format!("{err:#}").contains(expect), "{err:#}");
    assert!(!edited.load(Ordering::SeqCst), "the edit must not run");
    assert_eq!(
        std::fs::read_to_string(store.path()).unwrap(),
        contents,
        "a file it can't read must never be overwritten"
    );
}

#[test]
fn a_corrupt_file_is_refused_and_left_untouched() {
    assert_refused("{\"version\": 1, \"hosts\": {", "bindings.json");
}

#[test]
fn an_unknown_version_is_refused_and_left_untouched() {
    assert_refused(r#"{"version": 2, "hosts": {}}"#, "version");
}

// --- Pruning -------------------------------------------------------------------

#[test]
fn entries_of_gone_sessions_on_this_host_are_pruned() {
    let dir = Dir::new();
    let store = store(&dir);
    insert(&store, HOST, "live", binding("/r/app", false, 1));
    insert(&store, HOST, "gone", binding("/r/app", false, 2));
    insert(&store, "host-b", "elsewhere", binding("/r/app", false, 3));

    store
        .update(
            HOST,
            || Ok(vec!["live".into(), "new".into()]),
            |h| {
                h.sessions.insert("new".into(), binding("/r/app", false, 4));
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(sessions(&store, HOST), ["live", "new"]);
    assert_eq!(
        sessions(&store, "host-b"),
        ["elsewhere"],
        "other hosts' sessions can't be checked here and are left alone"
    );
}

/// A failed listing is not an empty listing.
#[test]
fn nothing_is_pruned_when_sessions_cannot_be_listed() {
    let dir = Dir::new();
    let store = store(&dir);
    insert(&store, HOST, "maybe-gone", binding("/r/app", false, 1));

    insert(&store, HOST, "new", binding("/r/app", false, 2));

    assert_eq!(sessions(&store, HOST), ["maybe-gone", "new"]);
}

/// The listing happens under the lock, so a session can't be bound between
/// the listing and the write and then pruned as "gone".
#[test]
fn sessions_are_listed_while_the_lock_is_held() {
    let dir = Dir::new();
    let store = store(&dir);
    let other = Store::new(store.path()).with_lock_timeout(Duration::from_millis(100));

    store
        .update(
            HOST,
            || {
                let blocked = other.update(HOST, no_listing, |_| Ok(()));
                assert!(
                    blocked.is_err(),
                    "a second writer got in during the listing"
                );
                Ok(vec![])
            },
            |_| Ok(()),
        )
        .unwrap();
}

// --- Branch refresh --------------------------------------------------------------

/// `branch` is a cache of the checkout's HEAD, refreshed on every write.
#[test]
fn branches_are_refreshed_on_every_write() {
    let repo = Dir::new();
    common::init_repo(&repo);
    let checkout = repo.path().canonicalize().unwrap();
    let dir = Dir::new();
    let store = store(&dir);
    let mut b = binding(checkout.to_str().unwrap(), false, 1);
    b.branch = Some("main".into());
    insert(&store, HOST, "s1", b);

    git(&checkout, &["switch", "-q", "-c", "renamed"]);
    insert(&store, HOST, "s2", binding("/r/elsewhere", false, 2));
    assert_eq!(
        store.read().unwrap().hosts[HOST].sessions["s1"]
            .branch
            .as_deref(),
        Some("renamed")
    );

    git(&checkout, &["switch", "-q", "--detach"]);
    insert(&store, HOST, "s3", binding("/r/elsewhere", false, 3));
    assert_eq!(
        store.read().unwrap().hosts[HOST].sessions["s1"].branch,
        None
    );
}

/// A checkout that vanished keeps its last known branch; it doesn't fail the
/// write.
#[test]
fn a_missing_checkout_keeps_its_last_branch() {
    let dir = Dir::new();
    let store = store(&dir);
    let mut b = binding("/definitely/not/here", true, 1);
    b.branch = Some("feat/gone".into());
    insert(&store, HOST, "s1", b);

    insert(&store, HOST, "s2", binding("/r/app", false, 2));

    assert_eq!(
        store.read().unwrap().hosts[HOST].sessions["s1"]
            .branch
            .as_deref(),
        Some("feat/gone")
    );
}

// --- Locking -------------------------------------------------------------------

/// An action must never wait indefinitely: a held lock is an error after the
/// deadline, and the file is left as it was.
#[test]
fn a_held_lock_times_out_with_a_clear_error() {
    let dir = Dir::new();
    let store = store(&dir);
    insert(&store, HOST, "s1", binding("/r/app", false, 1));
    let before = std::fs::read(store.path()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("rex/bindings.lock"))
        .expect("the writer locks the sidecar bindings.lock");
    lock.lock().unwrap();

    let impatient = Store::new(store.path()).with_lock_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let edited = AtomicBool::new(false);
    let err = impatient
        .update(HOST, no_listing, |_| {
            edited.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        format!("{err:#}").to_lowercase().contains("lock"),
        "{err:#}"
    );
    assert!(!edited.load(Ordering::SeqCst));
    assert_eq!(std::fs::read(store.path()).unwrap(), before);

    drop(lock);
    insert(&impatient, HOST, "s2", binding("/r/app", false, 2));
    assert_eq!(sessions(&store, HOST), ["s1", "s2"]);
}

#[test]
fn concurrent_threads_lose_no_update() {
    let dir = Dir::new();
    let path = dir.path().join("rex/bindings.json");
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let path = path.clone();
            std::thread::spawn(move || {
                let store = Store::new(path).with_lock_timeout(Duration::from_secs(30));
                for i in 0..10 {
                    insert(
                        &store,
                        HOST,
                        &format!("t{t}-{i:02}"),
                        binding("/r/app", false, i),
                    );
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }

    assert_eq!(sessions(&Store::new(&path), HOST).len(), 80);
}

const CHILD_ENV: &str = "WORKTREE_SYNC_TEST_BINDINGS_CHILD";

/// Body of `concurrent_processes_lose_no_update`'s child processes; a no-op
/// in a normal test run.
#[test]
fn child_writer() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let (path, prefix) = spec.split_once('|').unwrap();
    let store = Store::new(path).with_lock_timeout(Duration::from_secs(30));
    for i in 0..10 {
        insert(
            &store,
            HOST,
            &format!("{prefix}-{i:02}"),
            binding("/r/app", false, i),
        );
    }
}

/// The lock is a real OS file lock, not just an in-process mutex.
#[test]
fn concurrent_processes_lose_no_update() {
    let dir = Dir::new();
    let path = dir.path().join("rex/bindings.json");
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|p| {
            Command::new(&exe)
                .args(["--exact", "child_writer", "--test-threads=1"])
                .env(CHILD_ENV, format!("{}|p{p}", path.display()))
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    assert_eq!(sessions(&Store::new(&path), HOST).len(), 40);
}

#[test]
fn concurrent_writers_never_expose_a_partial_file() {
    let dir = Dir::new();
    let path = dir.path().join("rex/bindings.json");
    insert(
        &Store::new(&path),
        HOST,
        "seed",
        binding("/r/app", false, 0),
    );
    let done = Arc::new(AtomicBool::new(false));

    let writer = {
        let (path, done) = (path.clone(), done.clone());
        std::thread::spawn(move || {
            let store = Store::new(path);
            for i in 0..50 {
                insert(
                    &store,
                    HOST,
                    &format!("w{i:02}"),
                    binding("/r/app", false, i),
                );
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    // Readers don't lock (the contract says they needn't).
    let reader = Store::new(&path);
    while !done.load(Ordering::SeqCst) {
        reader
            .read()
            .expect("a reader always sees a complete document");
    }
    writer.join().unwrap();
}
