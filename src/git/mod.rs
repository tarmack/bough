//! In-process git engine backed by the `gix` crate.
//!
//! Replaces the previous subprocess-based `git` layer. Every operation is
//! served from `gix` directly — the changed-file scan and per-file diffs are
//! produced as structured data (no `git` binary is spawned and no unified-diff
//! text is generated).

mod diff;
mod error;
mod lfs;
mod refs;
mod status;

pub use diff::*;
pub use error::GitError;
pub use refs::*;
pub use status::*;

use std::path::Path;

use crate::diff::FileStatus;

/// Per-file metadata used to annotate the tree/stats and diff-view header.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    pub status: FileStatus,
    pub added: usize,
    pub deleted: usize,
    pub old_path: Option<String>,
    pub rename_percent: Option<u8>,
    pub mode_old: Option<u32>,
    pub mode_new: Option<u32>,
    pub binary: bool,
}

/// Open the repository containing `cwd` (searching upward). The callers run
/// from the repository root, so this mirrors the old behaviour of running
/// `git` in the process working directory.
pub(crate) fn open_repo() -> Result<gix::Repository, GitError> {
    let cwd = std::env::current_dir()
        .map_err(|e| GitError::message(format!("cannot read current directory: {e}")))?;
    open_repo_at(&cwd)
}

/// Open the repository containing `dir`, searching upward through parent
/// directories.
pub(crate) fn open_repo_at(dir: &Path) -> Result<gix::Repository, GitError> {
    gix::discover(dir).map_err(|e| GitError::message(format!("not a git repository: {e}")))
}

/// Resolve `spec` to a single object id (e.g. `HEAD`, a branch, `HEAD:path`).
pub(crate) fn resolve_id(repo: &gix::Repository, spec: &str) -> Result<gix::ObjectId, GitError> {
    let b = gix::bstr::BStr::new(spec);
    Ok(repo
        .rev_parse_single(b)
        .map_err(|e| GitError::message(format!("cannot resolve {spec:?}: {e}")))?
        .detach())
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
