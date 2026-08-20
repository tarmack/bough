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
    if run(&["rev-parse", "--git-dir"], &[0]).is_err() {
        return RepoState::NotARepo;
    }
    if run(&["rev-parse", "--verify", "HEAD"], &[0]).is_err() {
        return RepoState::NoCommits;
    }
    RepoState::Ready
}

/// Run `git` with `args`, returning trimless stdout or the stderr text on
/// failure. Exit codes in `ok` are accepted (e.g. `--no-index` exits 1 when
/// files differ); locale is pinned so error/parseable text never leaks.
fn run(args: &[&str], ok: &[i32]) -> Result<String, GitError> {
    let output = Command::new("git")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| GitError {
            message: format!("failed to run git: {e}"),
        })?;

    if let Some(code) = output.status.code() {
        if ok.contains(&code) {
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }
    }

    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(GitError { message })
}

/// Current branch short name, or `None` when detached (or unresolved).
pub fn current_branch() -> Result<Option<String>, GitError> {
    let out = run(&["rev-parse", "--abbrev-ref", "HEAD"], &[0])?;
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
    if let Ok(out) = run(
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        &[0],
    ) {
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
        if run(&["show-ref", "--verify", "--quiet", &ref_name], &[0]).is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

/// List changed files as `(status, path)` pairs via
/// `git diff --name-status <revision>`.
pub fn changed_files(revision: &str) -> Result<Vec<(char, String)>, GitError> {
    let out = run(
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-status",
            revision,
        ],
        &[0],
    )?;
    Ok(parse_name_status(&out))
}

/// Fetch the per-file diff for `path` against `revision` with `ctx` context
/// lines via `git diff -U<ctx>`.
pub fn file_diff_ctx(revision: &str, path: &str, ctx: u32) -> Result<String, GitError> {
    let u_arg = format!("-U{ctx}");
    run(&["diff", &u_arg, revision, "--", path], &[0])
}

/// List files that would form the initial commit (staged + untracked, excluding
/// ignored), each as an added (`A`) entry. For a repository with no commits.
pub fn listing_empty() -> Result<Vec<(char, String)>, GitError> {
    let out = run(
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "-c",
            "-o",
            "--exclude-standard",
        ],
        &[0],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| ('A', l.to_string()))
        .collect())
}

/// Per-file `(added, deleted)` line counts via `git diff --numstat`. Renames
/// are keyed by their new path; binary entries are skipped.
pub fn diff_stats(revision: &str) -> Result<HashMap<String, (usize, usize)>, GitError> {
    let out = run(
        &["-c", "core.quotePath=false", "diff", "--numstat", revision],
        &[0],
    )?;
    let mut map = HashMap::new();
    for line in out.lines() {
        if let Some(NumstatEntry::Counts {
            added,
            deleted,
            path,
        }) = parse_numstat_line(line)
        {
            map.insert(path, (added, deleted));
        }
    }
    Ok(map)
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

/// Per-file metadata (status, line counts, rename/mode/binary info) for one
/// path against a revision. Complements [`file_diff_ctx`]: `file_meta` feeds
/// the tree/stats while `file_diff_ctx` returns the raw body.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    pub status: char,
    pub added: usize,
    pub deleted: usize,
    pub old_path: Option<String>,
    pub rename_percent: Option<u8>,
    pub mode_old: Option<u32>,
    pub mode_new: Option<u32>,
    pub binary: bool,
}

/// A `git diff --numstat` line: either concrete added/deleted counts for a
/// path or a binary marker (`-\t-\t<path>`).
#[derive(Debug, Clone, PartialEq)]
enum NumstatEntry {
    Binary {
        path: String,
    },
    Counts {
        added: usize,
        deleted: usize,
        path: String,
    },
}

/// A parsed `git diff --raw` line (modes, status, rename percentage, paths).
#[derive(Debug, Clone, PartialEq)]
struct RawEntry {
    pub old_mode: u32,
    pub new_mode: u32,
    pub status: String,
    pub rename_percent: Option<u8>,
    pub old_path: Option<String>,
    pub path: String,
}

/// Metadata for a single changed path against `revision`.
///
/// Runs a single `git diff --numstat --raw` for the path and combines both
/// sections. Returns an error when `revision` is empty/invalid or when git reports
/// no diff for the path.
pub fn file_meta(revision: &str, path: &str) -> Result<FileMeta, GitError> {
    if revision.trim().is_empty() {
        return Err(GitError {
            message: format!("file_meta: empty revision for {path}"),
        });
    }
    let out = run(
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--numstat",
            "--raw",
            revision,
            "--",
            path,
        ],
        &[0],
    )?;

    // Raw lines start with ':', numstat lines with a digit or '-': the two
    // sections are separated by nothing but their leading character.
    let numstat = out
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with(':'))
        .and_then(parse_numstat_line);
    let raw = out
        .lines()
        .find(|l| l.starts_with(':'))
        .and_then(parse_raw_line);

    if numstat.is_none() && raw.is_none() {
        return Err(GitError {
            message: format!("file_meta: no diff for {path} at {revision}"),
        });
    }

    let (added, deleted, binary) = match numstat {
        Some(NumstatEntry::Counts { added, deleted, .. }) => (added, deleted, false),
        Some(NumstatEntry::Binary { .. }) => (0, 0, true),
        None => (0, 0, false),
    };

    let (status, old_path, rename_percent, mode_old, mode_new) = match raw {
        Some(r) => (
            r.status.chars().next().unwrap_or('M'),
            r.old_path,
            r.rename_percent,
            Some(r.old_mode),
            Some(r.new_mode),
        ),
        None => ('M', None, None, None, None),
    };

    Ok(FileMeta {
        status,
        added,
        deleted,
        old_path,
        rename_percent,
        mode_old,
        mode_new,
        binary,
    })
}

/// Parse one `git diff --numstat` line.
///
/// Format: `<added>\t<deleted>\t<path>` (renames add an extra old path
/// column); binary files show `-\t-\t<path>`. Returns `None` on malformed
/// input.
fn parse_numstat_line(line: &str) -> Option<NumstatEntry> {
    let mut fields = line.split('\t');
    let added = fields.next()?;
    let deleted = fields.next()?;
    if added == "-" && deleted == "-" {
        let path = fields.next_back()?.to_string();
        return Some(NumstatEntry::Binary { path });
    }
    let added = added.parse::<usize>().ok()?;
    let deleted = deleted.parse::<usize>().ok()?;
    let path = fields.next_back()?.to_string();
    Some(NumstatEntry::Counts {
        added,
        deleted,
        path,
    })
}

/// Parse a single `git diff --raw` line.
///
/// Format: `:<old_mode> <new_mode> <old_sha> <new_sha> <status>\t<path>`. For
/// renames/copies the status carries the similarity (e.g. `R092`) and an extra
/// old path column precedes the new one. Returns `None` on malformed input.
fn parse_raw_line(line: &str) -> Option<RawEntry> {
    let line = line.strip_prefix(':')?;
    let mut fields = line.split('\t');
    let header = fields.next()?;
    let paths: Vec<&str> = fields.collect();

    let mut tokens = header.split_whitespace();
    let old_mode = u32::from_str_radix(tokens.next()?, 8).ok()?;
    let new_mode = u32::from_str_radix(tokens.next()?, 8).ok()?;
    tokens.next(); // old blob sha
    tokens.next(); // new blob sha
    let status = tokens.next()?.to_string();

    let path = paths.last().copied()?.to_string();
    let old_path = if paths.len() >= 2 {
        Some(paths[paths.len() - 2].to_string())
    } else {
        None
    };
    let rename_percent = status
        .strip_prefix('R')
        .or_else(|| status.strip_prefix('C'))
        .and_then(|digits| digits.parse::<u8>().ok());

    Some(RawEntry {
        old_mode,
        new_mode,
        status,
        rename_percent,
        old_path,
        path,
    })
}

/// Diff a single file against an empty target with `ctx` context lines via
/// `git diff --no-index -U<ctx>`. For a repository with no commits.
pub fn file_diff_empty_ctx(path: &str, ctx: u32) -> Result<String, GitError> {
    let u_arg = format!("-U{ctx}");
    // `--no-index` exits 1 when files differ, 0 when identical.
    run(
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-index",
            &u_arg,
            "--",
            "/dev/null",
            path,
        ],
        &[0, 1],
    )
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
        let path = fields.next_back().unwrap_or("");
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
    fn parses_numstat_line_counts() {
        let e = parse_numstat_line("12\t3\tsrc/a.rs").unwrap();
        match e {
            NumstatEntry::Counts {
                added,
                deleted,
                path,
            } => {
                assert_eq!(added, 12);
                assert_eq!(deleted, 3);
                assert_eq!(path, "src/a.rs");
            }
            _ => panic!("expected counts"),
        }
    }

    #[test]
    fn parses_numstat_line_binary() {
        let e = parse_numstat_line("-\t-\tsrc/b.bin").unwrap();
        match e {
            NumstatEntry::Binary { path } => assert_eq!(path, "src/b.bin"),
            _ => panic!("expected binary"),
        }
    }

    #[test]
    fn parses_raw_line_modified() {
        let r = parse_raw_line(":100644 100644 1234567 89abcde M\tsrc/a.rs").unwrap();
        assert_eq!(r.status, "M");
        assert_eq!(r.old_mode, 0o100644);
        assert_eq!(r.new_mode, 0o100644);
        assert_eq!(r.path, "src/a.rs");
        assert_eq!(r.old_path, None);
        assert_eq!(r.rename_percent, None);
    }

    #[test]
    fn parses_raw_line_rename() {
        let r = parse_raw_line(":100644 100644 1111111 2222222 R092\told/name.rs\tnew/name.rs")
            .unwrap();
        assert_eq!(r.status, "R092");
        assert_eq!(r.rename_percent, Some(92));
        assert_eq!(r.old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(r.path, "new/name.rs");
    }

    #[test]
    fn parses_numstat_line_rename_uses_new_path() {
        let e = parse_numstat_line("5\t2\told/name.rs\tnew/name.rs").unwrap();
        match e {
            NumstatEntry::Counts {
                added,
                deleted,
                path,
            } => {
                assert_eq!(added, 5);
                assert_eq!(deleted, 2);
                assert_eq!(path, "new/name.rs");
            }
            _ => panic!("expected counts"),
        }
    }
}
