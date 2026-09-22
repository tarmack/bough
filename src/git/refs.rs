//! Reference/head state: repo classification, branch name, divergence vs a
//! base ref, upstream tracking, and default-branch detection.

use crate::git::{open_repo, resolve_id};

/// All local branch names, sorted alphabetically (e.g. for the branch
/// picker).
pub fn local_branches() -> Vec<String> {
    match open_repo() {
        Ok(repo) => local_branches_in(&repo),
        Err(_) => Vec::new(),
    }
}

/// [`local_branches`] against an already-open repository.
pub(crate) fn local_branches_in(repo: &gix::Repository) -> Vec<String> {
    let Ok(platform) = repo.references() else {
        return Vec::new();
    };
    let Ok(iter) = platform.local_branches() else {
        return Vec::new();
    };
    let mut names: Vec<String> = iter
        .filter_map(Result::ok)
        .map(|r| r.name().shorten().to_string())
        .collect();
    names.sort();
    names
}

/// The `n` most recent commits from `HEAD`, newest first: full oid hex plus
/// the commit subject (first message line).
pub(crate) fn recent_commits_in(repo: &gix::Repository, n: usize) -> Vec<(String, String)> {
    let Ok(head_id) = repo.head_id() else {
        return Vec::new();
    };
    let Ok(walker) = repo.rev_walk([head_id.detach()]).all() else {
        return Vec::new();
    };
    walker
        .take(n)
        .filter_map(|info| {
            let info = info.ok()?;
            let id = info.id;
            // A commit that fails to resolve/parse still contributes its oid
            // with an empty subject.
            let subject = repo
                .find_commit(id)
                .ok()
                .and_then(|c| c.message().ok().map(|m| m.summary().to_string()))
                .unwrap_or_default();
            Some((id.to_string(), subject))
        })
        .collect()
}

/// All tag names, sorted alphabetically.
pub(crate) fn tags_in(repo: &gix::Repository) -> Vec<String> {
    let Ok(platform) = repo.references() else {
        return Vec::new();
    };
    let Ok(iter) = platform.tags() else {
        return Vec::new();
    };
    let mut names: Vec<String> = iter
        .filter_map(Result::ok)
        .map(|r| r.name().shorten().to_string())
        .collect();
    names.sort();
    names
}

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
    let repo = match open_repo() {
        Ok(r) => r,
        Err(_) => return RepoState::NotARepo,
    };
    match repo.head() {
        Ok(head) if head.is_unborn() => RepoState::NoCommits,
        Ok(_) => RepoState::Ready,
        Err(_) => RepoState::NotARepo,
    }
}

/// Branch-state summary for the footer: current branch, its divergence from
/// the base ref and from its upstream. Every field degrades to `None`/0 when
/// it can't be determined (detached HEAD, no upstream, no base).
#[derive(Debug, Clone, Default)]
pub struct BranchState {
    /// Current branch short name, or `None` when detached (or unresolved).
    pub name: Option<String>,
    /// Base ref short name the diff is against; `None` when comparing `HEAD`
    /// to the working tree only (e.g. on the trunk).
    pub base: Option<String>,
    /// Commits `HEAD` has that `base` does not.
    pub ahead: Option<u32>,
    /// Commits `base` has that `HEAD` does not.
    pub behind: Option<u32>,
    /// Upstream/tracking branch short name; `None` when no upstream is set.
    pub upstream: Option<String>,
    /// Commits not yet pushed to `upstream`; `None` when no upstream.
    pub unpushed: Option<u32>,
}

/// Gather the current branch-state: branch name, divergence from the base ref
/// embedded in `rev`, and unpushed commits vs the upstream. Every field is
/// optional and computed defensively (never returns an error).
///
/// `rev` is the revision string the diff was built against, e.g. `main...HEAD`
/// (base is `main`) or `HEAD` (no base comparison).
pub fn branch_state(rev: &str) -> BranchState {
    match open_repo() {
        Ok(repo) => branch_state_in(&repo, rev),
        Err(_) => BranchState {
            name: None,
            ..BranchState::default()
        },
    }
}

/// [`branch_state`] against an already-open repository.
pub(crate) fn branch_state_in(repo: &gix::Repository, rev: &str) -> BranchState {
    let mut state = BranchState {
        name: current_branch_in(repo),
        ..BranchState::default()
    };

    // Ahead/behind vs base: `base...HEAD` prints `<behind>\t<ahead>`. The
    // revision's left side is the base ref, the right side the head.
    if let Some((base, head)) = rev.split_once("...") {
        let base = base.trim();
        let head = head.trim();
        if !base.is_empty() && base != "HEAD" {
            state.base = Some(base.to_string());
            if let (Ok(base_id), Ok(head_id)) = (resolve_id(repo, base), resolve_id(repo, head)) {
                state.behind = count_reachable(repo, &base_id, &head_id);
                state.ahead = count_reachable(repo, &head_id, &base_id);
            }
        }
    }

    // Upstream short name and unpushed count vs the upstream.
    if let Ok(rp) = repo.rev_parse("@{u}") {
        let (from_ref, _) = rp.into_references();
        state.upstream = from_ref.map(|r| r.name().shorten().to_string());
        if let (Ok(up_id), Ok(head_id)) = (resolve_id(repo, "@{u}"), repo.head_id()) {
            state.unpushed = count_reachable(repo, &head_id.detach(), &up_id);
        }
    }

    state
}

/// Branch short name, or `None` when detached/unborn.
fn current_branch_in(repo: &gix::Repository) -> Option<String> {
    repo.head()
        .ok()
        .and_then(|h| h.referent_name().map(|n| n.shorten().to_string()))
}

/// Number of commits reachable from `tip` but not from `hidden`.
fn count_reachable(
    repo: &gix::Repository,
    tip: &gix::ObjectId,
    hidden: &gix::ObjectId,
) -> Option<u32> {
    let walker = repo.rev_walk([*tip]).with_hidden([*hidden]).all().ok()?;
    // Count in `u64` and convert checked so an enormous walk is reported as
    // not-computable rather than silently truncated to `u32`.
    let count = u64::try_from(walker.count()).ok()?;
    u32::try_from(count).ok()
}

/// Detect the repository's default branch: `origin/HEAD` first, then a local
/// `main` or `master`. Returns `None` when none can be determined.
pub fn default_branch() -> Option<String> {
    let repo = open_repo().ok()?;

    // `origin/HEAD` symbolic ref, e.g. `refs/remotes/origin/main`.
    if let Ok(re) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let Some(target) = re.target().try_name() {
            let b = target.as_bstr();
            if let Some(rest) = b.strip_prefix(b"refs/remotes/origin/") {
                return Some(String::from_utf8_lossy(rest).into_owned());
            }
            return Some(target.shorten().to_string());
        }
    }

    for name in ["main", "master"] {
        if repo.find_reference(&format!("refs/heads/{name}")).is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::git::test_util::{repo_with_commits, run_git};

    use super::*;

    #[test]
    fn unborn_repo_is_no_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        assert!(repo.head().unwrap().is_unborn());
    }

    #[test]
    fn branch_state_counts_ahead_behind() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a", "b", "c"]);
        let repo = gix::discover(dir.path()).unwrap();

        // One commit ahead of the second-to-last commit.
        let st = branch_state_in(&repo, "HEAD~1...HEAD");
        assert_eq!(st.name.as_deref(), Some("main"));
        assert_eq!(st.base.as_deref(), Some("HEAD~1"));
        assert_eq!(st.ahead, Some(1));
        assert_eq!(st.behind, Some(0));
    }

    #[test]
    fn local_branches_lists_only_local_heads_sorted() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a", "b"]);
        run_git(dir.path(), &["branch", "feature"]);
        let repo = gix::discover(dir.path()).unwrap();

        assert_eq!(
            local_branches_in(&repo),
            vec!["feature".to_string(), "main".to_string()]
        );
    }

    #[test]
    fn recent_commits_newest_first_with_subjects() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a", "b", "c"]);
        let repo = gix::discover(dir.path()).unwrap();

        let commits = recent_commits_in(&repo, 10);
        assert_eq!(commits.len(), 3);
        // Newest (highest index) first; subjects are non-empty.
        assert_eq!(commits[0].1, "commit 2");
        assert_eq!(commits[1].1, "commit 1");
        assert_eq!(commits[2].1, "commit 0");
        assert_eq!(commits[0].0.len(), 40, "full oid hex");
    }

    #[test]
    fn recent_commits_honours_limit() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a", "b", "c"]);
        let repo = gix::discover(dir.path()).unwrap();

        assert_eq!(recent_commits_in(&repo, 2).len(), 2);
    }

    #[test]
    fn tags_lists_all_sorted() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["a", "b"]);
        run_git(dir.path(), &["tag", "v1"]);
        run_git(dir.path(), &["tag", "v0"]);
        let repo = gix::discover(dir.path()).unwrap();

        assert_eq!(tags_in(&repo), vec!["v0".to_string(), "v1".to_string()]);
    }
}
