//! Worktree discovery: listing every worktree of the current repository,
//! resolving `--worktree` specs, counting uncommitted changes per worktree,
//! and identifying the worktree the process is currently viewing.

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::git::{commit_time, open_repo_at, staged_changes, unstaged_changes};

use super::GitError;

/// One row in the worktree picker and one resolution candidate for
/// `--worktree`. The main checkout is a synthetic entry (`main == true`) so
/// the picker always offers "home"; linked worktrees carry their id (the
/// directory name under `.git/worktrees/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Worktree id: `main` for the main checkout, otherwise the
    /// `.git/worktrees/<id>` directory name.
    pub id: String,
    /// Canonical worktree directory (absolute).
    pub path: PathBuf,
    /// HEAD short name; `None` when detached or unreadable.
    pub branch: Option<String>,
    /// True for the main checkout (synthetic entry).
    pub main: bool,
    /// True when the workdir is gone (prunable) or its `gitdir`-sourced
    /// path cannot be trusted (repo-controlled metadata): listed, never
    /// selectable.
    pub missing: bool,
    /// Unix timestamp of the HEAD commit (commit time), for recency
    /// ordering in the picker. `None` when detached-without-commits or
    /// unreadable.
    pub tip_time: Option<i64>,
}

/// Uncommitted-change count for one worktree, for the picker's dirty chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirtyStats {
    /// Number of changed entries (staged + unstaged + untracked) vs HEAD.
    pub files: usize,
}

/// Upper bound on linked worktrees listed per repository (the main entry is
/// not counted). The list comes from `.git/worktrees/<id>` admin dirs, which
/// a crafted repository can fill with thousands of entries; processing each
/// one (canonicalize, trust check, repo open) on the UI thread would freeze
/// the picker. Truncation is surfaced through [`worktrees_with_error`].
const MAX_LINKED_WORKTREES: usize = 128;

/// [`worktrees`] with the failure reason attached: `Some` error when the
/// worktree enumeration itself failed (permissions, mid-iteration I/O
/// error), so the picker can surface `picker_error` instead of silently
/// showing a partial list. Entry-level failures (one unreadable worktree)
/// still degrade to that entry being `missing`/skipped, never blanking the
/// list.
pub fn worktrees_with_error() -> (Vec<WorktreeInfo>, Option<GitError>) {
    match super::open_repo() {
        Ok(repo) => worktrees_in_with_error(&repo),
        Err(e) => (Vec::new(), Some(e)),
    }
}

/// All worktrees of the repository the process is currently viewing,
/// including the synthetic main entry when a main checkout exists. Uses the
/// global repo root (see [`crate::git::set_repo_root`]).
pub fn worktrees() -> Vec<WorktreeInfo> {
    worktrees_with_error().0
}

/// [`worktrees`] against an already-open repository, with the enumeration
/// error attached: `Some` reason when listing the linked worktrees failed
/// outright (the main entry is still returned) or the list was truncated at
/// [`MAX_LINKED_WORKTREES`]. Per-entry failures degrade that entry to
/// `missing`/skipped, never blanking the list.
pub fn worktrees_in_with_error(repo: &gix::Repository) -> (Vec<WorktreeInfo>, Option<GitError>) {
    let mut out = Vec::new();

    // Synthetic main entry: the workdir of the repo owning the main
    // checkout. Absent when the main repo is bare (linked worktrees only).
    let main_repo = match repo.kind() {
        gix::repository::Kind::LinkedWorkTree => repo.main_repo().ok(),
        _ => Some(repo.clone()),
    };
    if let Some(main) = main_repo {
        if let Some(dir) = main.workdir().map(Path::to_path_buf) {
            let path = canonical_or(dir);
            out.push(WorktreeInfo {
                id: "main".to_string(),
                missing: !path.exists(),
                branch: head_branch(&main),
                path,
                main: true,
                tip_time: head_commit_time(&main),
            });
        }
    }

    let linked = match repo.worktrees() {
        Ok(linked) => linked,
        // `NotFound` already comes back as `Ok(vec![])`, so an error is a
        // real FS failure (permissions, mid-iteration I/O): surface it
        // instead of silently collapsing the list to the main entry.
        Err(e) => {
            return (
                sort_worktrees(out),
                Some(GitError::message(format!("cannot list worktrees: {e}"))),
            );
        }
    };

    for (listed, proxy) in linked.into_iter().enumerate() {
        if listed == MAX_LINKED_WORKTREES {
            return (
                sort_worktrees(out),
                Some(GitError::message(format!(
                    "worktree list capped at {MAX_LINKED_WORKTREES} entries"
                ))),
            );
        }
        // A proxy whose gitdir file is unreadable has no recoverable path.
        let Ok(base) = proxy.base() else {
            continue;
        };
        let path = canonical_or(base);
        let opened = proxy
            .clone()
            .into_repo_with_possibly_inaccessible_worktree()
            .ok();
        let branch = opened.as_ref().and_then(head_branch);
        out.push(WorktreeInfo {
            id: disambiguate_id(&proxy.id().to_string(), &path),
            // The gitdir file is repo-controlled metadata: only trust the
            // path when it is a directory that discovers back into this
            // repository; anything else is listed but flagged missing
            // (never selectable, never scanned).
            missing: !worktree_path_is_trusted(repo, &path),
            branch,
            path,
            main: false,
            tip_time: opened.as_ref().and_then(head_commit_time),
        });
    }
    (sort_worktrees(out), None)
}

/// Resolve a `--worktree` spec: exact worktree id first, then an exact
/// (canonicalized) path. `None` when nothing matches.
pub fn resolve_worktree_spec(spec: &str) -> Option<WorktreeInfo> {
    let list = worktrees();
    if let Some(wt) = list.iter().find(|wt| wt.id == spec) {
        return Some(wt.clone());
    }
    let target = std::fs::canonicalize(spec).ok()?;
    list.into_iter().find(|wt| wt.path == target)
}

/// Count uncommitted changes in the worktree rooted at `root`. `None` when
/// the worktree cannot be opened or scanned (e.g. its dir vanished).
pub fn dirty_count(root: &Path) -> Option<DirtyStats> {
    let repo = open_repo_at(root).ok()?;
    // Unborn HEAD (no commits yet): every index entry counts as an addition.
    // Any other HEAD failure (corrupt/missing commit) aborts the scan
    // instead of faking a huge "everything staged" diff.
    let head_tree = repo.head_tree_id_or_empty().ok()?.detach();

    let staged = staged_changes(&repo, &head_tree);
    let (unstaged, untracked) = unstaged_changes(&repo).ok()?;

    // A path can be staged *and* carry further unstaged edits; count it once,
    // matching the distinct entries the uncommitted view shows.
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(staged.into_keys());
    paths.extend(unstaged.into_keys());
    paths.extend(untracked);
    Some(DirtyStats { files: paths.len() })
}

/// The id of the linked worktree the process is currently viewing, or
/// `None` when viewing the main checkout (or when the current root cannot be
/// classified).
pub fn current_worktree_id() -> Option<String> {
    let repo = open_repo_at(&super::current_repo_root()).ok()?;
    if repo.kind() != gix::repository::Kind::LinkedWorkTree {
        return None;
    }
    // A linked worktree's git dir is `<common>/worktrees/<id>`; apply the
    // listing's id mapping so a disambiguated `main` id matches on both
    // sides.
    let raw = repo.git_dir().file_name()?.to_str()?;
    let workdir = repo.workdir()?.to_path_buf();
    Some(disambiguate_id(raw, &canonical_or(workdir)))
}

/// The short branch name `HEAD` points at; `None` when detached or unreadable.
fn head_branch(repo: &gix::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    Some(head.referent_name()?.shorten().to_string())
}

/// Unix commit time of the repository's `HEAD` commit; `None` when HEAD is
/// unborn or the commit cannot be read (one lookup, no history walk).
fn head_commit_time(repo: &gix::Repository) -> Option<i64> {
    repo.head_id()
        .ok()
        .and_then(|id| commit_time(repo, id.detach()))
}

/// Canonical form of `dir`, or `dir` itself when it cannot be canonicalized
/// (e.g. the worktree dir vanished — it is still listed, flagged missing).
fn canonical_or(dir: PathBuf) -> PathBuf {
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// A linked worktree whose admin dir is named `main` collides with the
/// synthetic main entry's reserved id, so it gets a short deterministic hash
/// of its canonical path as a suffix. Applied identically in
/// [`worktrees_in_with_error`] and [`current_worktree_id`] so both agree.
fn disambiguate_id(raw: &str, path: &Path) -> String {
    if raw != "main" {
        return raw.to_string();
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!("main-{:04x}", hasher.finish() & 0xffff)
}

/// Whether a linked worktree's base path may be trusted: the `gitdir` file
/// under `.git/worktrees/<id>/` is repo-controlled metadata, so require the
/// path to exist as a directory and to discover back into this repository's
/// common git dir. Anything else (a plain dir, another repo, `$HOME`) lists
/// as `missing` instead of being displayed, opened or scanned.
fn worktree_path_is_trusted(repo: &gix::Repository, path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(discovered) = gix::discover(path) else {
        return false;
    };
    let common = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    common(discovered.common_dir()) == common(repo.common_dir())
}

/// Main checkout first, then linked worktrees by id.
fn sort_worktrees(mut list: Vec<WorktreeInfo>) -> Vec<WorktreeInfo> {
    list.sort_by(|a, b| b.main.cmp(&a.main).then_with(|| a.id.cmp(&b.id)));
    list
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::MutexGuard;

    use crate::git::test_util::{repo_with_commits, run_git};
    use crate::git::{current_repo_root, open_repo, set_repo_root};

    use super::*;

    /// A main repo with one commit plus a branch worktree (`wt-feature` on
    /// `feature`) and a detached one (`wt-detach`), all inside the tempdir.
    /// Returns (tempdir, canonical main, feature, detached) paths.
    fn repo_with_worktrees() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a"]);
        run_git(
            dir.path(),
            &["worktree", "add", "-b", "feature", "wt-feature"],
        );
        run_git(dir.path(), &["worktree", "add", "--detach", "wt-detach"]);
        let main = std::fs::canonicalize(dir.path()).unwrap();
        let feature = std::fs::canonicalize(dir.path().join("wt-feature")).unwrap();
        let detached = std::fs::canonicalize(dir.path().join("wt-detach")).unwrap();
        (dir, main, feature, detached)
    }

    /// Serialize tests that read or mutate the global repo root (cargo runs
    /// tests in parallel threads); on drop the root is restored to the
    /// default (process cwd fallback). Uses the crate-wide lock so the
    /// `worker` module's root-mutating tests and the `input` module's
    /// real-picker tests serialize against these too.
    fn root_lock() -> RootGuard {
        let lock = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        RootGuard { _lock: lock }
    }

    struct RootGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for RootGuard {
        fn drop(&mut self) {
            super::super::clear_repo_root_for_tests();
        }
    }

    #[test]
    fn worktrees_in_lists_main_and_linked() {
        let (dir, main, feature, detached) = repo_with_worktrees();
        let repo = gix::discover(dir.path()).unwrap();

        let wts = worktrees_in_with_error(&repo).0;

        assert_eq!(wts.len(), 3);
        // Main entry first, carrying the main repo's branch.
        assert!(wts[0].main);
        assert_eq!(wts[0].id, "main");
        assert_eq!(wts[0].path, main);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert!(!wts[0].missing);

        let feat = wts.iter().find(|w| w.id == "wt-feature").unwrap();
        assert_eq!(feat.path, feature);
        assert_eq!(feat.branch.as_deref(), Some("feature"));
        assert!(!feat.main);
        assert!(!feat.missing);

        let det = wts.iter().find(|w| w.id == "wt-detach").unwrap();
        assert_eq!(det.path, detached);
        assert_eq!(det.branch, None, "detached HEAD has no branch name");
        assert!(!det.main);

        // Linked worktrees follow the main entry, sorted by id.
        let linked_ids: Vec<&str> = wts[1..].iter().map(|w| w.id.as_str()).collect();
        assert_eq!(linked_ids, vec!["wt-detach", "wt-feature"]);
    }

    #[test]
    fn worktrees_carry_head_commit_times() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        run_git(dir.path(), &["config", "user.email", "t@e"]);
        run_git(dir.path(), &["config", "user.name", "T"]);

        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        commit_at(
            dir.path(),
            "2000-01-01T00:00:00+0000",
            &["commit", "-m", "A"],
        );

        run_git(
            dir.path(),
            &["worktree", "add", "-b", "feature", "wt-feature"],
        );
        std::fs::write(dir.path().join("wt-feature/f.txt"), "f\n").unwrap();
        run_git(&dir.path().join("wt-feature"), &["add", "f.txt"]);
        commit_at(
            &dir.path().join("wt-feature"),
            "2000-01-03T00:00:00+0000",
            &["commit", "-m", "F"],
        );

        let repo = gix::discover(dir.path()).unwrap();
        let wts = worktrees_in_with_error(&repo).0;

        // Each entry reports its *own* HEAD commit time, as unix seconds.
        let main = wts.iter().find(|w| w.main).unwrap();
        assert_eq!(main.tip_time, Some(946_684_800), "2000-01-01T00:00:00Z");
        let feat = wts.iter().find(|w| w.id == "wt-feature").unwrap();
        assert_eq!(feat.tip_time, Some(946_857_600), "2000-01-03T00:00:00Z");
        let _ = dir;
    }

    #[test]
    fn unborn_head_has_no_tip_time() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        let repo = gix::discover(dir.path()).unwrap();

        let wts = worktrees_in_with_error(&repo).0;

        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].tip_time, None, "no commits yet");
    }

    #[test]
    fn worktrees_in_flags_missing_workdir() {
        let (dir, _main, _feature, _detached) = repo_with_worktrees();
        std::fs::remove_dir_all(dir.path().join("wt-detach")).unwrap();
        let repo = gix::discover(dir.path()).unwrap();

        let wts = worktrees_in_with_error(&repo).0;

        let det = wts.iter().find(|w| w.id == "wt-detach").unwrap();
        assert!(
            det.missing,
            "vanished worktree stays listed, flagged missing"
        );
        assert!(!det.path.exists());
        assert!(!wts.iter().find(|w| w.id == "wt-feature").unwrap().missing);
        assert!(!wts.iter().find(|w| w.id == "main").unwrap().missing);
    }

    #[test]
    fn worktrees_and_resolve_follow_global_root() {
        let (dir, _main, feature, _detached) = repo_with_worktrees();
        let _guard = root_lock();
        set_repo_root_checked(dir.path());

        let wts = worktrees();
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].id, "main");

        let by_id = resolve_worktree_spec("wt-feature").unwrap();
        assert_eq!(by_id.path, feature);
        assert_eq!(by_id.branch.as_deref(), Some("feature"));

        let by_path = resolve_worktree_spec(feature.to_str().unwrap()).unwrap();
        assert_eq!(by_path.id, "wt-feature");

        let by_main_id = resolve_worktree_spec("main").unwrap();
        assert!(by_main_id.main);

        assert!(resolve_worktree_spec("wt-nope").is_none());
    }

    #[test]
    fn dirty_count_counts_staged_unstaged_and_untracked() {
        let (dir, main, feature, _detached) = repo_with_worktrees();

        // Clean start everywhere.
        assert_eq!(dirty_count(&main).unwrap().files, 0);
        assert_eq!(dirty_count(&feature).unwrap().files, 0);

        // One unstaged edit, one staged addition, one untracked file.
        std::fs::write(feature.join("file0.txt"), "edited").unwrap();
        std::fs::write(feature.join("file9.txt"), "staged").unwrap();
        run_git(&feature, &["add", "file9.txt"]);
        std::fs::write(feature.join("stray.txt"), "untracked").unwrap();

        assert_eq!(dirty_count(&feature).unwrap().files, 3);
        // The main checkout is unaffected by edits in the linked worktree.
        assert_eq!(dirty_count(&main).unwrap().files, 0);

        // A vanished worktree dir cannot be scanned.
        assert_eq!(dirty_count(Path::new("/nonexistent/wt")), None);
        let _ = dir;
    }

    #[test]
    fn current_worktree_id_reflects_global_root() {
        let (dir, _main, feature, _detached) = repo_with_worktrees();
        let _guard = root_lock();

        set_repo_root_checked(dir.path());
        assert_eq!(current_worktree_id(), None, "main checkout has no id");

        set_repo_root_checked(&feature);
        assert_eq!(current_worktree_id().as_deref(), Some("wt-feature"));
    }

    #[test]
    fn set_repo_root_validates_and_redirects_open_repo() {
        let (_dir, _main, feature, _detached) = repo_with_worktrees();
        let _guard = root_lock();

        set_repo_root_checked(&feature);
        assert_eq!(current_repo_root(), feature);

        // `open_repo()` now resolves the linked worktree, not the cwd repo.
        let repo = open_repo().unwrap();
        assert_eq!(
            std::fs::canonicalize(repo.workdir().unwrap()).unwrap(),
            feature
        );

        // A bad root is rejected and leaves the stored root untouched.
        let bad = tempfile::tempdir().unwrap();
        assert!(set_repo_root(bad.path().to_path_buf()).is_err());
        assert_eq!(current_repo_root(), feature);
    }

    #[test]
    fn set_repo_root_rejects_git_admin_paths() {
        let (dir, main, _feature, _detached) = repo_with_worktrees();
        let _guard = root_lock();
        set_repo_root_checked(dir.path());
        let root = current_repo_root();

        // `.git` itself, paths inside it, and a linked worktree's admin dir
        // all discover to the enclosing repo but are not checkouts.
        assert!(super::super::set_repo_root(main.join(".git")).is_err());
        assert!(super::super::set_repo_root(main.join(".git/objects")).is_err());
        assert!(
            super::super::set_repo_root(main.join(".git/worktrees/wt-feature")).is_err(),
            "linked-worktree admin dir is also a git-admin path"
        );
        assert_eq!(
            current_repo_root(),
            root,
            "rejections leave the root untouched"
        );
        let _ = dir;
    }

    #[test]
    fn set_repo_root_stores_the_canonical_path() {
        let (dir, main, _feature, _detached) = repo_with_worktrees();
        let _guard = root_lock();

        // Reach the main worktree through a symlink: the stored root must be
        // the canonical path that was validated, not the raw input.
        let link = dir.path().join("link-to-root");
        std::os::unix::fs::symlink(&main, &link).unwrap();
        super::super::set_repo_root(link).unwrap();

        assert_eq!(current_repo_root(), main);
    }

    #[test]
    fn enumeration_failure_is_reported_not_blanked() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a"]);
        // A file where `.git/worktrees` belongs makes enumeration fail
        // outright (read_dir on a file): the main entry and the error must
        // both come back instead of a silently blanked linked list.
        std::fs::write(dir.path().join(".git/worktrees"), b"").unwrap();
        let repo = gix::discover(dir.path()).unwrap();

        let (wts, err) = worktrees_in_with_error(&repo);

        assert_eq!(wts.len(), 1, "main entry still listed");
        assert!(wts[0].main);
        assert!(err.is_some(), "enumeration failure must surface an error");
    }

    #[test]
    fn dirty_count_on_unborn_head_counts_staged_additions() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        run_git(dir.path(), &["config", "user.email", "t@e"]);
        run_git(dir.path(), &["config", "user.name", "T"]);
        // Materialize the empty tree object so the staged diff has a real
        // tree to compare against.
        run_git(dir.path(), &["write-tree"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        run_git(dir.path(), &["add", "."]);

        let stats = dirty_count(dir.path()).unwrap();

        assert_eq!(
            stats.files, 1,
            "unborn HEAD: staged file counts as addition"
        );
    }

    #[test]
    fn dirty_count_on_unreadable_head_commit_is_none() {
        let (dir, main, _feature, _detached) = repo_with_worktrees();
        // Remove HEAD's commit object (loose objects are write-protected, so
        // removing is the portable way to make it unreadable): the scan must
        // report failure instead of counting every tracked file as staged.
        let oid = rev_parse_head(&main);
        let obj = main.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        std::fs::remove_file(obj).unwrap();

        assert_eq!(
            dirty_count(&main),
            None,
            "unreadable HEAD must not fake a scan"
        );
        let _ = dir;
    }

    #[test]
    fn linked_worktree_named_main_gets_distinct_id() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a"]);
        // A worktree named `main` gets admin dir `.git/worktrees/main`,
        // colliding with the synthetic main entry's reserved id.
        run_git(dir.path(), &["worktree", "add", "-b", "feature", "main"]);
        let wt = std::fs::canonicalize(dir.path().join("main")).unwrap();
        let repo = gix::discover(dir.path()).unwrap();

        let wts = worktrees_in_with_error(&repo).0;

        assert_eq!(wts.iter().filter(|w| w.main).count(), 1);
        let linked = wts.iter().find(|w| !w.main).unwrap();
        assert_ne!(linked.id, "main", "reserved id must be disambiguated");
        assert!(linked.id.starts_with("main-"), "got {:?}", linked.id);

        // The listing and current-worktree detection must agree on the id.
        let _guard = root_lock();
        set_repo_root_checked(&wt);
        assert_eq!(current_worktree_id().as_deref(), Some(linked.id.as_str()));
        let _ = dir;
    }

    #[test]
    fn untrusted_gitdir_path_lists_as_missing() {
        let (dir, _main, _feature, _detached) = repo_with_worktrees();
        // A crafted gitdir file (repo-controlled metadata) pointing at a
        // plain directory outside this repository: the entry must be listed
        // as missing instead of exposing that path.
        let stray = tempfile::tempdir().unwrap();
        let stray_path = std::fs::canonicalize(stray.path()).unwrap();
        let admin = dir.path().join(".git/worktrees/evil");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}/.git\n", stray_path.display()),
        )
        .unwrap();
        let repo = gix::discover(dir.path()).unwrap();

        let wts = worktrees_in_with_error(&repo).0;

        let evil = wts.iter().find(|w| w.id == "evil").unwrap();
        assert_eq!(evil.path, stray_path, "the crafted target is the path");
        assert!(evil.missing, "outside-the-repo gitdir target is untrusted");
        assert!(!wts.iter().find(|w| w.id == "wt-feature").unwrap().missing);
        assert!(!wts.iter().find(|w| w.id == "main").unwrap().missing);
    }

    #[test]
    fn worktree_listing_is_capped() {
        let (dir, _main, _feature, _detached) = repo_with_worktrees();
        // Fabricate admin dirs past the cap; gitdir files pointing nowhere
        // make each entry listed-but-missing, which is enough to count.
        for i in 0..MAX_LINKED_WORKTREES {
            let admin = dir
                .path()
                .join(".git/worktrees")
                .join(format!("wt-fake{i}"));
            std::fs::create_dir_all(&admin).unwrap();
            std::fs::write(admin.join("gitdir"), format!("/nonexistent/wt{i}/.git\n")).unwrap();
        }
        let repo = gix::discover(dir.path()).unwrap();

        let (wts, err) = worktrees_in_with_error(&repo);

        assert_eq!(
            wts.len(),
            MAX_LINKED_WORKTREES + 1,
            "main entry plus capped linked entries"
        );
        assert_eq!(wts.iter().filter(|w| w.main).count(), 1);
        assert!(
            err.is_some_and(|e| e
                .to_string()
                .contains(&format!("capped at {MAX_LINKED_WORKTREES} entries"))),
            "truncation must surface an error"
        );
    }

    /// `set_repo_root` takes an owned path; assert success so a failing
    /// validation fails the test instead of panicking at the next unwrap.
    fn set_repo_root_checked(root: &Path) {
        super::super::set_repo_root(root.to_path_buf())
            .unwrap_or_else(|e| panic!("set_repo_root({}): {e}", root.display()));
    }

    fn rev_parse_head(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Run `git` in `dir` with author/committer dates pinned so commit
    /// times are deterministic.
    fn commit_at(dir: &Path, date: &str, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} at {date} failed");
    }
}
