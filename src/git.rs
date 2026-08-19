//! Git subprocess engine.
//!
//! Every operation shells out to `git` via a synchronous subprocess. Errors
//! carry the git stderr text so the caller can print a clear message when the
//! current directory is not a valid repository.

use std::collections::HashMap;
use std::process::Command;

/// A command failed or `git` exited non-zero. `message` holds the raw stderr.
#[derive(Debug)]
pub struct GitError {
    pub message: String,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GitError {}

/// Coarse state of the current working directory with respect to git.
#[derive(Debug)]
pub enum RepoState {
    NotARepo,
    NoCommits,
    Ready,
}

/// Classify the current directory: not a repository, a repository with no
/// commits yet, or a usable repository.
pub fn repo_state() -> RepoState {
    if run(&["rev-parse", "--git-dir"]).is_err() {
        return RepoState::NotARepo;
    }
    if run(&["rev-parse", "--verify", "HEAD"]).is_err() {
        return RepoState::NoCommits;
    }
    RepoState::Ready
}

/// Run `git` with `args`, returning trimless stdout or the stderr text on
/// failure.
fn run(args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| GitError {
            message: format!("failed to run git: {e}"),
        })?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(GitError { message });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Current branch short name, or `None` when detached (or unresolved).
pub fn current_branch() -> Result<Option<String>, GitError> {
    let out = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = out.trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(branch))
    }
}

/// Detect the repository's default branch: `origin/HEAD` first, then a local
/// `main` or `master`. Returns `None` when none can be determined.
pub fn default_branch() -> Option<String> {
    if let Ok(out) = run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let b = out.trim();
        if let Some(name) = b.strip_prefix("origin/") {
            return Some(name.to_string());
        }
        if !b.is_empty() {
            return Some(b.to_string());
        }
    }
    for name in ["main", "master"] {
        let ref_name = format!("refs/heads/{name}");
        if run(&["show-ref", "--verify", "--quiet", &ref_name]).is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

/// List changed files as `(status, path)` pairs via
/// `git diff --name-status <revision>`.
pub fn changed_files(revision: &str) -> Result<Vec<(char, String)>, GitError> {
    let out = run(&[
        "-c",
        "core.quotePath=false",
        "diff",
        "--name-status",
        revision,
    ])?;
    Ok(parse_name_status(&out))
}

/// Fetch the per-file diff for `path` against `revision`.
pub fn file_diff(revision: &str, path: &str) -> Result<String, GitError> {
    run(&["diff", revision, "--", path])
}

/// List files that would form the initial commit (staged + untracked, excluding
/// ignored), each as an added (`A`) entry. For a repository with no commits.
pub fn listing_empty() -> Result<Vec<(char, String)>, GitError> {
    let out = run(&[
        "-c",
        "core.quotePath=false",
        "ls-files",
        "-c",
        "-o",
        "--exclude-standard",
    ])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| ('A', l.to_string()))
        .collect())
}

/// Per-file `(added, deleted)` line counts via `git diff --numstat`.
pub fn diff_stats(revision: &str) -> Result<HashMap<String, (usize, usize)>, GitError> {
    let out = run(&["-c", "core.quotePath=false", "diff", "--numstat", revision])?;
    Ok(parse_numstat(&out))
}

fn parse_numstat(raw: &str) -> HashMap<String, (usize, usize)> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let mut fields = line.split('\t');
        let added = fields
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let deleted = fields
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        if let Some(path) = fields.next() {
            if !path.is_empty() {
                map.insert(path.to_string(), (added, deleted));
            }
        }
    }
    map
}

/// For a no-commit repo: every file is added; count its lines as the added
/// count and 0 deletions.
pub fn empty_stats(paths: &[String]) -> HashMap<String, (usize, usize)> {
    let mut map = HashMap::new();
    for path in paths {
        let lines = std::fs::read(path)
            .map(|bytes| bytes.iter().filter(|&&b| b == b'\n').count())
            .unwrap_or(0);
        map.insert(path.clone(), (lines, 0));
    }
    map
}

/// Diff a single file against an empty base, yielding the full file content as
/// added lines. For a repository with no commits.
pub fn file_diff_empty(path: &str) -> Result<String, GitError> {
    let output = Command::new("git")
        .args(["diff", "--no-index", "--", "/dev/null", path])
        .output()
        .map_err(|e| GitError {
            message: format!("failed to run git: {e}"),
        })?;

    // `--no-index` exits 1 when files differ, 0 when identical.
    match output.status.code() {
        Some(0) | Some(1) => Ok(String::from_utf8_lossy(&output.stdout).to_string()),
        _ => Err(GitError {
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }),
    }
}

/// Parse `git diff --name-status` output.
///
/// Each line is `<status>\t<path>` except renames/copies which carry an extra
/// column: `R100\told/path\tnew/path`. We keep the new (last) path and the
/// leading status letter only.
fn parse_name_status(raw: &str) -> Vec<(char, String)> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split('\t');
        let status = fields.next().unwrap_or("");
        let path = fields.last().unwrap_or("");
        if status.is_empty() || path.is_empty() {
            continue;
        }
        if let Some(flag) = status.chars().next() {
            entries.push((flag, path.to_string()));
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_modified_deleted() {
        let raw = "M\tsrc/core/mod.rs\nA\tsrc/new.rs\nD\tsrc/gone.rs\n";
        let entries = parse_name_status(raw);
        assert_eq!(
            entries,
            vec![
                ('M', "src/core/mod.rs".to_string()),
                ('A', "src/new.rs".to_string()),
                ('D', "src/gone.rs".to_string()),
            ]
        );
    }

    #[test]
    fn parses_rename_to_new_path() {
        let raw = "R100\told/name.rs\tnew/name.rs\n";
        let entries = parse_name_status(raw);
        assert_eq!(entries, vec![('R', "new/name.rs".to_string())]);
    }

    #[test]
    fn skips_blank_lines() {
        assert_eq!(parse_name_status("\n\n"), Vec::<(char, String)>::new());
    }

    #[test]
    fn parses_path_with_spaces() {
        let raw = "M\tsrc/foo bar.rs\n";
        let entries = parse_name_status(raw);
        assert_eq!(entries, vec![('M', "src/foo bar.rs".to_string())]);
    }

    #[test]
    fn parses_numstat() {
        let raw = "12\t3\tsrc/a.rs\n0\t5\tsrc/b.rs\n";
        let map = parse_numstat(raw);
        assert_eq!(map.get("src/a.rs"), Some(&(12, 3)));
        assert_eq!(map.get("src/b.rs"), Some(&(0, 5)));
    }
}
