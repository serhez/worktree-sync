//! Repository identity, Herdr-compatible naming, and the worktree add/remove
//! primitives the Rex workflows are built on.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Dir, git, init_repo};
use worktree_sync::git::{
    self as wgit, Added, RemoveError, branch_to_path_slug, default_checkout_path,
    generated_branch_slug, is_dirty_worktree_remove_error, parse_worktree_list_porcelain,
};

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

fn rev(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn commit_in(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            message,
        ],
    );
}

/// A committed repo plus a sibling dir for worktrees (both in one tempdir).
fn repo() -> (Dir, PathBuf) {
    let root = Dir::new();
    let repo = root.path().join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "core.excludesFile", "/dev/null"]);
    std::fs::write(repo.join("README"), "app\n").unwrap();
    commit_in(&repo, "init");
    (root, repo)
}

// --- Herdr-compatible naming -------------------------------------------------

/// Herdr's own vectors: a checkout created by either backend lands at the same
/// path for the same branch.
#[test]
fn branch_slugs_match_herdr() {
    assert_eq!(
        branch_to_path_slug("worktree/brave-river"),
        "worktree-brave-river"
    );
    assert_eq!(
        branch_to_path_slug("issue/137 Worktree Spaces"),
        "issue-137-worktree-spaces"
    );
    assert_eq!(branch_to_path_slug("///"), "worktree");
    assert_eq!(branch_to_path_slug(""), "worktree");
    assert_eq!(branch_to_path_slug("--Feat__X--"), "feat-x");
    assert_eq!(branch_to_path_slug("ünï/cödé"), "n-c-d");
}

#[test]
fn default_checkout_path_is_root_then_repo_then_slug() {
    assert_eq!(
        default_checkout_path(Path::new("/w"), "app", "feat/Login"),
        PathBuf::from("/w/app/feat-login")
    );
}

/// `worktree/<adjective>-<noun>-<hex4>` from Herdr's word lists.
#[test]
fn generated_branches_match_herdr() {
    assert_eq!(generated_branch_slug(0), "worktree/brave-river-0000");
    assert_eq!(generated_branch_slug(9), "worktree/calm-cloud-0009");
    assert_eq!(generated_branch_slug(63), "worktree/silver-valley-003f");
    assert_eq!(generated_branch_slug(0x12345), "worktree/quiet-river-2345");
}

#[test]
fn generated_branches_are_valid_branch_names() {
    let (_root, repo) = repo();
    for seed in [0, 1, 7, 8, 63, 64, 0xffff, u64::MAX] {
        let branch = generated_branch_slug(seed);
        wgit::validate_branch_name(&repo, &branch)
            .unwrap_or_else(|e| panic!("{branch} rejected: {e:#}"));
    }
}

// --- Porcelain parsing ---------------------------------------------------------

/// Herdr's porcelain vector: branches lose `refs/heads/`, detached and
/// prunable entries are flagged rather than dropped.
#[test]
fn porcelain_entries_are_parsed_with_their_flags() {
    let output = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo-issue
HEAD 2222222222222222222222222222222222222222
branch refs/heads/worktree/issue

worktree /repo-stale
HEAD 3333333333333333333333333333333333333333
detached
prunable stale

";
    let entries = parse_worktree_list_porcelain(output);
    assert_eq!(entries.len(), 3);

    assert_eq!(entries[0].path, PathBuf::from("/repo"));
    assert_eq!(entries[0].branch.as_deref(), Some("main"));
    assert!(!entries[0].is_detached && !entries[0].is_prunable && !entries[0].is_bare);

    assert_eq!(entries[1].path, PathBuf::from("/repo-issue"));
    assert_eq!(entries[1].branch.as_deref(), Some("worktree/issue"));

    assert_eq!(entries[2].path, PathBuf::from("/repo-stale"));
    assert_eq!(entries[2].branch, None);
    assert!(entries[2].is_detached);
    assert!(entries[2].is_prunable);
}

#[test]
fn porcelain_marks_the_bare_entry() {
    let entries = parse_worktree_list_porcelain(
        "worktree /proj/.bare\nbare\n\nworktree /proj/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n",
    );
    assert_eq!(entries.len(), 2);
    assert!(entries[0].is_bare);
    assert!(!entries[1].is_bare);
}

// --- Repository identity -------------------------------------------------------

#[test]
fn checkout_identity_of_a_main_checkout() {
    let (_root, repo) = repo();
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();

    let checkout = wgit::checkout_at(&repo.join("src/deep")).unwrap();

    assert_eq!(
        checkout.path,
        canonical(&repo),
        "the work-tree root, not the subdir"
    );
    assert_eq!(checkout.repo_key, canonical(&repo.join(".git")));
    assert_eq!(checkout.repo_name, "app");
    assert_eq!(checkout.branch.as_deref(), Some("main"));
    assert!(!checkout.is_linked_worktree);
}

#[test]
fn linked_worktrees_share_the_main_checkouts_identity() {
    let (root, repo) = repo();
    let linked = root.path().join("wt/feat");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/x",
            linked.to_str().unwrap(),
        ],
    );

    let checkout = wgit::checkout_at(&linked).unwrap();

    assert_eq!(checkout.path, canonical(&linked));
    assert_eq!(checkout.repo_key, canonical(&repo.join(".git")));
    assert_eq!(checkout.repo_name, "app");
    assert_eq!(checkout.branch.as_deref(), Some("feat/x"));
    assert!(checkout.is_linked_worktree);
}

#[test]
fn a_detached_checkout_has_no_branch() {
    let (root, repo) = repo();
    let linked = root.path().join("wt/detached");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
        ],
    );

    assert_eq!(wgit::checkout_at(&linked).unwrap().branch, None);
}

/// Herdr names an embedded `.bare` layout after its container.
#[test]
fn an_embedded_bare_repo_is_named_after_its_container() {
    let src = Dir::new();
    init_repo(&src);
    let root = Dir::new();
    let container = root.path().join("proj");
    let bare = container.join(".bare");
    let status = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(src.path())
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success());
    let main = container.join("main");
    git(
        &bare,
        &["worktree", "add", "-q", main.to_str().unwrap(), "main"],
    );

    let checkout = wgit::checkout_at(&main).unwrap();

    assert_eq!(checkout.repo_name, "proj");
    assert_eq!(checkout.repo_key, canonical(&bare));
}

#[test]
fn any_other_bare_repo_is_named_after_its_own_dir() {
    let src = Dir::new();
    init_repo(&src);
    let root = Dir::new();
    let bare = root.path().join("proj.git");
    let status = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(src.path())
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success());
    let wt = root.path().join("wt");
    git(
        &bare,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "main"],
    );

    assert_eq!(wgit::checkout_at(&wt).unwrap().repo_name, "proj.git");
}

/// Binding needs a work tree; a bare repo or a plain directory has none.
#[test]
fn a_path_outside_any_work_tree_has_no_checkout() {
    let plain = Dir::new();
    assert!(wgit::checkout_at(plain.path()).is_err());

    let src = Dir::new();
    init_repo(&src);
    let root = Dir::new();
    let bare = root.path().join("bare.git");
    assert!(
        Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(src.path())
            .arg(&bare)
            .status()
            .unwrap()
            .success()
    );
    assert!(wgit::checkout_at(&bare).is_err());
}

// --- Listing -------------------------------------------------------------------

/// Paths are canonical even when the repo was reached through a symlinked
/// prefix (macOS `/var` → `/private/var`), so they compare equal to bindings.
#[test]
fn worktrees_are_listed_main_first_with_canonical_paths() {
    let (root, repo) = repo();
    let linked = root.path().join("wt/feat");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            linked.to_str().unwrap(),
        ],
    );

    let entries = wgit::list_worktrees(&linked).unwrap();

    let paths: Vec<_> = entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(paths, [canonical(&repo), canonical(&linked)]);
    assert_eq!(wgit::main_checkout(&linked).unwrap(), canonical(&repo));
}

// --- Branch names ----------------------------------------------------------------

#[test]
fn branch_names_are_validated_by_git_rules() {
    let (_root, repo) = repo();
    wgit::validate_branch_name(&repo, "feat/login").unwrap();
    for bad in [
        "",
        "bad..name",
        "-leading-dash",
        "has space",
        "ends.lock",
        "a~b",
    ] {
        assert!(
            wgit::validate_branch_name(&repo, bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

// --- Adding ----------------------------------------------------------------------

#[test]
fn adding_a_new_branch_starts_it_at_the_base() {
    let (root, repo) = repo();
    let first = rev(&repo, "HEAD");
    commit_in(&repo, "second");
    let path = root.path().join("wt/new");

    let added = wgit::add_worktree(&repo, &path, "feat/new", "HEAD~1").unwrap();

    assert!(matches!(added, Added::NewBranch));
    assert_eq!(rev(&path, "HEAD"), first);
    assert_eq!(
        wgit::checkout_at(&path).unwrap().branch.as_deref(),
        Some("feat/new")
    );
}

/// Herdr parity: an existing local branch is checked out as is; the base is
/// ignored rather than resetting someone's work.
#[test]
fn adding_an_existing_branch_checks_it_out_and_ignores_the_base() {
    let (root, repo) = repo();
    git(&repo, &["branch", "existing"]);
    let tip = rev(&repo, "existing");
    commit_in(&repo, "main moves on");
    let path = root.path().join("wt/existing");

    let added = wgit::add_worktree(&repo, &path, "existing", "main").unwrap();

    assert!(matches!(added, Added::ExistingBranch));
    assert_eq!(rev(&path, "HEAD"), tip);
}

#[test]
fn adding_onto_an_existing_path_fails_and_creates_no_branch() {
    let (root, repo) = repo();
    let path = root.path().join("wt/taken");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("keep.txt"), "mine").unwrap();

    assert!(wgit::add_worktree(&repo, &path, "feat/taken", "HEAD").is_err());

    assert!(!branch_exists(&repo, "feat/taken"));
    assert_eq!(
        std::fs::read_to_string(path.join("keep.txt")).unwrap(),
        "mine"
    );
}

// --- Removing --------------------------------------------------------------------

fn linked(repo: &Path, root: &Dir, branch: &str) -> PathBuf {
    let path = root.path().join("wt").join(branch);
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            path.to_str().unwrap(),
        ],
    );
    path
}

#[test]
fn removing_a_clean_worktree_keeps_its_branch() {
    let (root, repo) = repo();
    let path = linked(&repo, &root, "clean");

    wgit::remove_worktree(&repo, &path, false).unwrap();

    assert!(!path.exists());
    assert!(branch_exists(&repo, "clean"));
}

#[test]
fn removing_a_worktree_with_untracked_files_needs_force() {
    let (root, repo) = repo();
    let path = linked(&repo, &root, "untracked");
    std::fs::write(path.join("notes.txt"), "precious").unwrap();

    let err = wgit::remove_worktree(&repo, &path, false).unwrap_err();

    assert!(matches!(err, RemoveError::NeedsForce(_)), "{err}");
    assert_eq!(
        std::fs::read_to_string(path.join("notes.txt")).unwrap(),
        "precious"
    );

    wgit::remove_worktree(&repo, &path, true).unwrap();
    assert!(!path.exists());
    assert!(branch_exists(&repo, "untracked"));
}

#[test]
fn removing_a_worktree_with_modified_files_needs_force() {
    let (root, repo) = repo();
    let path = linked(&repo, &root, "modified");
    std::fs::write(path.join("README"), "changed\n").unwrap();

    let err = wgit::remove_worktree(&repo, &path, false).unwrap_err();

    assert!(matches!(err, RemoveError::NeedsForce(_)), "{err}");
    assert!(path.join("README").exists());
}

/// One `--force` is the most this tool ever passes: a locked worktree (which
/// needs `-f -f`) is reported, not overridden.
#[test]
fn a_locked_worktree_is_not_removed_even_with_force() {
    let (root, repo) = repo();
    let path = linked(&repo, &root, "locked");
    git(&repo, &["worktree", "lock", path.to_str().unwrap()]);

    for force in [false, true] {
        let err = wgit::remove_worktree(&repo, &path, force).unwrap_err();
        assert!(
            matches!(err, RemoveError::Failed(_)),
            "force={force}: {err}"
        );
        assert!(path.exists());
    }
}

/// Herdr's classifier: only git's "dirty" and "submodules" refusals offer a
/// forced retry.
#[test]
fn dirty_remove_errors_are_classified_like_herdr() {
    assert!(is_dirty_worktree_remove_error(
        "fatal: '/w/x' contains modified or untracked files, use --force to delete it"
    ));
    assert!(is_dirty_worktree_remove_error(
        "FATAL: '/w/x' CONTAINS MODIFIED OR UNTRACKED FILES, USE --FORCE TO DELETE IT"
    ));
    assert!(is_dirty_worktree_remove_error(
        "fatal: working trees containing submodules cannot be moved or removed"
    ));
    assert!(!is_dirty_worktree_remove_error(
        "fatal: cannot remove a locked working tree, lock reason: x\nuse 'remove -f -f' to override or unlock first"
    ));
    assert!(!is_dirty_worktree_remove_error(
        "fatal: '/w/x' contains modified or untracked files"
    ));
    assert!(!is_dirty_worktree_remove_error(
        "fatal: '/w/x' is not a working tree"
    ));
}

// --- Deleting branches (rollback only) -------------------------------------------

#[test]
fn a_merged_branch_is_deleted() {
    let (_root, repo) = repo();
    git(&repo, &["branch", "merged"]);

    wgit::delete_merged_branch(&repo, "merged").unwrap();

    assert!(!branch_exists(&repo, "merged"));
}

/// `git branch -d`, never `-D`: unmerged work survives a rollback.
#[test]
fn an_unmerged_branch_is_kept() {
    let (root, repo) = repo();
    let path = linked(&repo, &root, "unmerged");
    commit_in(&path, "work");
    git(&repo, &["worktree", "remove", path.to_str().unwrap()]);

    assert!(wgit::delete_merged_branch(&repo, "unmerged").is_err());
    assert!(branch_exists(&repo, "unmerged"));
}
