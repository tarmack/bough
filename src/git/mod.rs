//! In-process git engine backed by the `gix` crate.
//!
//! Replaces the previous subprocess-based `git` layer. Every operation is
//! served from `gix` directly — the changed-file scan and per-file diffs are
//! produced as structured data (no `git` binary is spawned and no unified-diff
//! text is generated).

mod error;
mod filediff;
mod lfs;
mod refs;
mod status;
mod worktrees;

pub use error::GitError;
pub use filediff::*;
pub use refs::*;
pub use status::*;
pub use worktrees::*;

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// The process-global repository root the worktree-switch mechanism pins
/// every `open_repo()` call to; `None` means "resolve from the process cwd"
/// (the startup behaviour).
static REPO_ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

fn repo_root_slot() -> &'static RwLock<Option<PathBuf>> {
    REPO_ROOT.get_or_init(|| RwLock::new(None))
}

/// Open the repository the git layer currently resolves from: the stored
/// root after a worktree switch, else the process cwd (searching upward).
pub(crate) fn open_repo() -> Result<gix::Repository, GitError> {
    open_repo_at(&current_repo_root())
}

/// Open the repository containing `dir`, searching upward through parent
/// directories.
pub(crate) fn open_repo_at(dir: &Path) -> Result<gix::Repository, GitError> {
    gix::discover(dir).map_err(|e| GitError::message(format!("not a git repository: {e}")))
}

/// Set the repository root every `open_repo()` call resolves from (the
/// worktree-switch mechanism; process cwd is left untouched). Canonicalizes
/// first and validates the canonical path, so the stored root is exactly the
/// path that was checked: git administration paths (anything with a `.git`
/// component) are rejected outright, and the rest must open and lie inside
/// a checked-out worktree.
pub(crate) fn set_repo_root(root: PathBuf) -> Result<(), GitError> {
    let canonical = std::fs::canonicalize(&root)
        .map_err(|e| GitError::message(format!("cannot canonicalize {}: {e}", root.display())))?;
    // A `.git` path (the main admin dir, a linked worktree's admin dir, …)
    // discovers to the enclosing repo and would look coherent, but is not a
    // checked-out worktree.
    if canonical.components().any(|c| c.as_os_str() == ".git") {
        return Err(GitError::message(format!(
            "{} is a git administration path, not a checked-out worktree",
            root.display()
        )));
    }
    let repo = open_repo_at(&canonical)?;
    let covered = repo.workdir().is_some_and(|workdir| {
        let workdir = std::fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
        canonical.starts_with(workdir)
    });
    if !covered {
        return Err(GitError::message(format!(
            "{} is not inside a checked-out worktree",
            root.display()
        )));
    }
    let mut slot = repo_root_slot()
        .write()
        .map_err(|_| GitError::message("repo root lock poisoned"))?;
    *slot = Some(canonical);
    Ok(())
}

/// Set the global repo root to the worktree named by `spec` (id or path),
/// validating it first. One atomic entry point for "view this worktree":
/// callers cannot resolve a spec and set the root in separate steps, which
/// would leave a window where a git call reads the wrong worktree between
/// them. `Ok(None)` means the spec matched no worktree (the caller lists
/// what exists); `Err` means a match was found but could not be opened.
pub fn select_worktree(spec: &str) -> Result<Option<crate::git::WorktreeInfo>, GitError> {
    match resolve_worktree_spec(spec) {
        Some(info) => set_repo_root(info.path.clone()).map(|_| Some(info)),
        None => Ok(None),
    }
}

/// The root the git layer currently resolves from: the last
/// `set_repo_root` value, or the process cwd before any switch. Reading the
/// stored value is poison-tolerant so a panicked writer cannot silently
/// retarget every `open_repo()` back at the startup repo.
pub(crate) fn current_repo_root() -> PathBuf {
    repo_root_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Test-only: drop the stored root so `current_repo_root()` falls back to
/// the process cwd again. Global-root tests hold a shared mutex and call
/// this when their guard drops (cargo runs tests in parallel threads).
#[cfg(test)]
pub(crate) fn clear_repo_root_for_tests() {
    if let Some(lock) = REPO_ROOT.get() {
        if let Ok(mut root) = lock.write() {
            *root = None;
        }
    }
}

/// Resolve `spec` to a single object id (e.g. `HEAD`, a branch, `HEAD:path`).
pub(crate) fn resolve_id(repo: &gix::Repository, spec: &str) -> Result<gix::ObjectId, GitError> {
    let b = gix::bstr::BStr::new(spec);
    Ok(repo
        .rev_parse_single(b)
        .map_err(|e| GitError::message(format!("cannot resolve {spec:?}: {e}")))?
        .detach())
}

/// Split a three-dot revision range (`base...HEAD`) into its two trimmed
/// sides. `None` for a plain revision — and for a two-dot (`base..HEAD`)
/// range, whose sides are not split off; callers that must reject two-dot
/// ranges do so separately.
pub(crate) fn split_range(rev: &str) -> Option<(&str, &str)> {
    if rev.contains("..") && !rev.contains("...") {
        return None;
    }
    let (base, head) = rev.split_once("...")?;
    Some((base.trim(), head.trim()))
}

/// Test fixtures: build throwaway repositories with real commits via the `git`
/// binary (test-only; `gix` cannot yet author commits through this API).
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::Path;
    use std::process::Command;

    /// `git init -b main`, then commit each `content` item as `file{i}.txt`.
    pub fn repo_with_commits(dir: &Path, contents: &[&str]) {
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        for (i, content) in contents.iter().enumerate() {
            std::fs::write(dir.join(format!("file{i}.txt")), content).unwrap();
            run_git(dir, &["add", "."]);
            run_git(dir, &["commit", "-m", &format!("commit {i}")]);
        }
    }

    /// Run `git` in `dir` and assert success. Exposed for other test modules
    /// that need to create branches or refs.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
}
