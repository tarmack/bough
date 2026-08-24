//! Working-tree status and per-file diff metadata: changed-file listing,
//! untracked-file enumeration, per-file added/removed line counts, and
//! per-file metadata used to annotate the tree/diff views.

use std::collections::HashMap;
use std::path::PathBuf;

use gix::diff::blob::platform::prepare_diff::Operation;
use gix::status::index_worktree::iter::Summary;
use gix::status::index_worktree::{Item as IWItem, RewriteSource};
use gix::{diff, progress};

use crate::diff::FileStatus;
use crate::git::{open_repo, resolve_id, set_blob_resources, FileMeta, GitError};

/// A single changed file's details, enough to derive the tree listing, the
/// per-file stats and the diff-view header annotation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChangeInfo {
    pub status: FileStatus,
    pub path: String,
    pub old_path: Option<String>,
    pub rename_percent: Option<u8>,
    pub mode_old: Option<u32>,
    pub mode_new: Option<u32>,
    pub added: usize,
    pub deleted: usize,
    pub binary: bool,
}

impl ChangeInfo {
    pub(crate) fn into_meta(self) -> FileMeta {
        FileMeta {
            status: self.status,
            added: self.added,
            deleted: self.deleted,
            old_path: self.old_path,
            rename_percent: self.rename_percent,
            mode_old: self.mode_old,
            mode_new: self.mode_new,
            binary: self.binary,
        }
    }
}

/// All changed files with their full per-file metadata, from a single scan.
///
/// Backs both the tree listing and the per-file diffs in one pass so that
/// selecting a file does not re-scan the whole repository.
pub(crate) fn all_changes(revision: &str) -> Result<Vec<ChangeInfo>, GitError> {
    let repo = open_repo()?;
    collect_changes(&repo, revision)
}

/// For a no-commit repo: every file is added; count its lines as the added
/// count and 0 deletions.
pub fn initial_stats(paths: &[String]) -> HashMap<String, (usize, usize)> {
    let mut map = HashMap::new();
    for path in paths {
        let lines = line_count(path);
        map.insert(path.clone(), (lines, 0));
    }
    map
}

/// Metadata for an untracked file: a pure addition against the empty tree.
pub fn file_meta_untracked(path: &str) -> Result<FileMeta, GitError> {
    let added = line_count(path);
    Ok(FileMeta {
        status: FileStatus::Added,
        added,
        deleted: 0,
        old_path: None,
        rename_percent: None,
        mode_old: None,
        mode_new: None,
        binary: false,
    })
}

/// List files that would form the initial commit (staged + untracked, excluding
/// ignored), each as an added (`A`) entry. For a repository with no commits.
pub fn initial_listing() -> Result<Vec<(char, String)>, GitError> {
    let repo = open_repo()?;
    let mut entries = Vec::new();

    let index = repo
        .index_or_empty()
        .map_err(|e| GitError::message(e.to_string()))?;
    for entry in index.entries() {
        let path = entry.path(&index).to_string();
        entries.push(('A', path));
    }
    for path in untracked_paths(&repo)? {
        entries.push(('A', path));
    }
    Ok(entries)
}

/// Untracked, non-ignored files in the working tree (relative paths).
fn untracked_paths(repo: &gix::Repository) -> Result<Vec<String>, GitError> {
    let platform = match repo.status(progress::Discard) {
        Ok(p) => p,
        Err(e) => return Err(GitError::message(format!("untracked scan: {e}"))),
    };
    let iter = match platform
        // Enumerate each untracked file individually; the default collapses an
        // untracked directory to a single entry (its path), which reads as a
        // file to the tree.
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(std::iter::empty())
    {
        Ok(i) => i,
        Err(e) => return Err(GitError::message(format!("untracked scan: {e}"))),
    };
    let mut out = Vec::new();
    for item in iter.flatten() {
        if matches!(item.summary(), Some(Summary::Added)) && !is_directory_item(&item) {
            out.push(item.rela_path().to_string());
        }
    }
    Ok(out)
}

/// Enumerate all changes against `revision`'s tree (including untracked),
/// computing per-file status and line counts.
///
/// `revision` may be a plain tree-ish (`HEAD`) or a three-dot range
/// (`main...HEAD`), in which case the base is the merge-base of the two sides.
pub(crate) fn collect_changes(
    repo: &gix::Repository,
    revision: &str,
) -> Result<Vec<ChangeInfo>, GitError> {
    let base_tree = base_tree_id(repo, revision)?;
    let index = index_from_tree_permissive(repo, &base_tree)?;

    let roots = repo
        .workdir()
        .map(|p| diff::blob::pipeline::WorktreeRoots {
            // The source side is always resolved from the object database by id;
            // only the destination is read from the working tree.
            old_root: None,
            new_root: Some(p.to_path_buf()),
        })
        .unwrap_or_default();
    let mut cache = repo
        .diff_resource_cache(diff::blob::pipeline::Mode::ToGit, roots)
        .map_err(|e| GitError::message(e.to_string()))?;

    let platform = repo
        .status(progress::Discard)
        .map_err(|e| GitError::message(e.to_string()))?;
    let platform = platform
        .index(gix::worktree::IndexPersistedOrInMemory::InMemory(index))
        .index_worktree_rewrites(Some(diff::Rewrites::default()))
        // Report each untracked file individually; the default collapses an
        // untracked directory to a single entry (its path), which reads as a
        // file to the tree and has no line count.
        .untracked_files(gix::status::UntrackedFiles::Files);
    let iter = platform
        .into_index_worktree_iter(std::iter::empty())
        .map_err(|e| GitError::message(e.to_string()))?;

    let mut changes = Vec::new();
    let workdir = repo
        .workdir()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    for res in iter {
        let item = res.map_err(|e| GitError::message(e.to_string()))?;
        let summary = match item.summary() {
            Some(s) => s,
            None => continue,
        };
        let path = item.rela_path().to_string();

        let (status, old_path, rename_percent, old_id, mode_old, mode_new) = match (&item, summary)
        {
            (IWItem::Modification { entry, .. }, Summary::Removed) => (
                FileStatus::Deleted,
                None,
                None,
                Some(entry.id),
                Some(entry.mode.bits()),
                Some(entry.mode.bits()),
            ),
            (IWItem::Modification { entry, .. }, Summary::TypeChange) => (
                FileStatus::TypeChanged,
                None,
                None,
                Some(entry.id),
                Some(entry.mode.bits()),
                Some(entry.mode.bits()),
            ),
            (IWItem::Modification { entry, .. }, Summary::Modified) => (
                FileStatus::Modified,
                None,
                None,
                Some(entry.id),
                Some(entry.mode.bits()),
                Some(entry.mode.bits()),
            ),
            (IWItem::Rewrite { source, .. }, Summary::Renamed) => (
                FileStatus::Renamed,
                Some(rewrite_source_path(source)),
                None,
                Some(rewrite_source_id(source)),
                None,
                None,
            ),
            (IWItem::Rewrite { source, .. }, Summary::Copied) => (
                FileStatus::Copied,
                Some(rewrite_source_path(source)),
                None,
                Some(rewrite_source_id(source)),
                None,
                None,
            ),
            _ => {
                if summary == Summary::Added && !is_directory_item(&item) {
                    changes.push(ChangeInfo {
                        status: FileStatus::Added,
                        path: path.clone(),
                        old_path: None,
                        rename_percent: None,
                        mode_old: None,
                        mode_new: None,
                        added: line_count(workdir.join(&path).to_str().unwrap_or(path.as_str())),
                        deleted: 0,
                        binary: false,
                    });
                }
                // The remaining summaries that can reach this fall-through are
                // `Added` (handled above for files; directories are skipped
                // here), `IntentToAdd` (staged for intent-to-add only, nothing
                // changed), and `Conflict` (unmerged entry, out of scope for a
                // diff viewer). None of those are emitted here.
                debug_assert!(
                    matches!(
                        summary,
                        Summary::Added | Summary::IntentToAdd | Summary::Conflict
                    ),
                    "unexpected summary {summary:?} reached the catch-all arm"
                );
                continue;
            }
        };

        let (added, deleted, binary) = count_changes(&mut cache, repo, old_id, &path)?;
        changes.push(ChangeInfo {
            status,
            path,
            old_path,
            rename_percent,
            mode_old,
            mode_new,
            added,
            deleted,
            binary,
        });
    }
    Ok(changes)
}

/// Build an in-memory index from `tree`'s contents using git's permissive
/// (non-Windows) path validation. `Repository::index_from_tree()` enables the
/// Windows/HFS/NTFS protections by default, which reject any tracked name that
/// is merely illegal on those filesystems (e.g. a `:`, `"`, `*` or trailing
/// dot) — bough only *reads* the index to enumerate changes and never writes to
/// the working tree, so those protections would spuriously fail the whole
/// listing on otherwise-valid Linux trees.
fn index_from_tree_permissive(
    repo: &gix::Repository,
    tree: &gix::ObjectId,
) -> Result<gix::index::File, GitError> {
    let state = gix::index::State::from_tree(
        tree.as_ref(),
        repo,
        gix::validate::path::component::Options {
            protect_windows: false,
            protect_hfs: false,
            protect_ntfs: false,
        },
    )
    .map_err(|e| GitError::message(format!("cannot build index from tree {tree}: {e}")))?;
    Ok(gix::index::File::from_state(
        state,
        repo.git_dir().join("index"),
    ))
}

/// The tree to diff the working tree against: the merge-base of a three-dot
/// range, else the single revision's tree.
pub(crate) fn base_tree_id(
    repo: &gix::Repository,
    revision: &str,
) -> Result<gix::ObjectId, GitError> {
    if let Some((base, head)) = revision.split_once("...") {
        let base = base.trim();
        let head = head.trim();
        if !base.is_empty() && !head.is_empty() {
            let base_id = resolve_id(repo, base)?;
            let head_id = resolve_id(repo, head)?;
            let base = repo.merge_base(base_id, head_id).map_err(|e| {
                GitError::message(format!("cannot find merge-base for {revision}: {e}"))
            })?;
            // `merge_base` yields a commit; peel it to its tree so the caller
            // can build an index from it.
            let commit = repo.find_commit(base).map_err(|e| {
                GitError::message(format!("cannot resolve merge-base for {revision}: {e}"))
            })?;
            let tree = commit.tree_id().map_err(|e| {
                GitError::message(format!(
                    "cannot resolve merge-base for {revision} to a tree: {e}"
                ))
            })?;
            return Ok(tree.detach());
        }
    }
    let id = resolve_id(repo, revision)?;
    let commit = repo
        .find_commit(id)
        .map_err(|e| GitError::message(format!("cannot resolve revision {revision}: {e}")))?;
    let tree = commit.tree_id().map_err(|e| {
        GitError::message(format!("cannot resolve revision {revision} to a tree: {e}"))
    })?;
    Ok(tree.detach())
}

/// Count added/deleted lines and detect binary by diffing the blob at `old_id`
/// (or the empty blob when `None`) against the worktree content at `path`.
/// Returns `Ok((added, deleted, binary))`.
fn count_changes(
    cache: &mut diff::blob::Platform,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    path: &str,
) -> Result<(usize, usize, bool), GitError> {
    set_blob_resources(cache, repo, old_id, path)?;

    match cache.prepare_diff() {
        Ok(outcome) => match outcome.operation {
            Operation::InternalDiff { algorithm } => {
                let input = outcome.interned_input();
                let diff = diff::blob::Diff::compute(algorithm, &input);
                Ok((
                    diff.count_additions() as usize,
                    diff.count_removals() as usize,
                    false,
                ))
            }
            Operation::SourceOrDestinationIsBinary | Operation::ExternalCommand { .. } => {
                Ok((0, 0, true))
            }
        },
        Err(e) => Err(GitError::message(format!("diff {path}: {e}"))),
    }
}

/// The id of a rewrite's source.
fn rewrite_source_id(source: &RewriteSource) -> gix::ObjectId {
    match source {
        RewriteSource::RewriteFromIndex { source_entry, .. } => source_entry.id,
        RewriteSource::CopyFromDirectoryEntry {
            source_dirwalk_entry_id,
            ..
        } => *source_dirwalk_entry_id,
    }
}

/// The path of a rewrite's source.
fn rewrite_source_path(source: &RewriteSource) -> String {
    match source {
        RewriteSource::RewriteFromIndex {
            source_rela_path, ..
        } => source_rela_path.to_string(),
        RewriteSource::CopyFromDirectoryEntry {
            source_dirwalk_entry,
            ..
        } => source_dirwalk_entry.rela_path.to_string(),
    }
}

/// Whether an untracked item is a directory entry (an untracked directory or a
/// nested repository). bough treats every change as a file, so directories must
/// be skipped rather than emitted as bogus file tree-nodes.
fn is_directory_item(item: &IWItem) -> bool {
    matches!(
        item,
        IWItem::DirectoryContents {
            entry: gix::dir::Entry {
                disk_kind: Some(
                    gix::dir::entry::Kind::Directory | gix::dir::entry::Kind::Repository
                ),
                ..
            },
            ..
        }
    )
}

/// Count the lines (LF) in a file; 0 when it can't be read.
fn line_count(path: &str) -> usize {
    std::fs::read(path)
        .map(|bytes| {
            if bytes.is_empty() {
                0
            } else {
                bytes.iter().filter(|&&b| b == b'\n').count() + usize::from(!bytes.ends_with(b"\n"))
            }
        })
        .unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use crate::git::test_util::repo_with_commits;

    use super::*;

    #[test]
    fn collects_modified_added_and_untracked() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one", "two"]);
        let repo = gix::discover(dir.path()).unwrap();

        // Modify an existing file and add a new (untracked) file.
        std::fs::write(dir.path().join("file0.txt"), "changed\nchanged\nchanged\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "hello\nworld\n").unwrap();

        let changes = collect_changes(&repo, "HEAD").unwrap();
        let by_path: HashMap<_, _> = changes.into_iter().map(|c| (c.path.clone(), c)).collect();

        let modified = &by_path["file0.txt"];
        assert_eq!(modified.status, FileStatus::Modified);
        assert_eq!(modified.added, 3);
        assert_eq!(modified.deleted, 1);

        let untracked = &by_path["new.txt"];
        assert_eq!(untracked.status, FileStatus::Added);
        assert_eq!(untracked.added, 2);
    }

    #[test]
    fn detects_untracked_path() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        let repo = gix::discover(dir.path()).unwrap();
        std::fs::write(dir.path().join("stray.rs"), "fn main() {}\n").unwrap();

        let index = repo.index_or_empty().unwrap();
        assert!(index
            .entry_by_path(gix::bstr::BStr::new("stray.rs"))
            .is_none());
        assert!(index
            .entry_by_path(gix::bstr::BStr::new("file0.txt"))
            .is_some());
    }

    #[test]
    fn windows_illegal_filename_does_not_abort_listing() {
        // A name with a `:` is legal on Linux but rejected by gix's default
        // (protect_windows) path validation. bough must still list changes.
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        let run = |args: &[&str]| {
            let s = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(s.success());
        };
        run(&["config", "user.email", "t@e"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(dir.path().join("a:b.txt"), "one\n").unwrap();
        std::fs::write(dir.path().join("plain.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c"]);

        // Modify a tracked file so there is a change to report.
        std::fs::write(dir.path().join("plain.txt"), "one\ntwo\n").unwrap();

        let changes = collect_changes(&repo, "HEAD").unwrap();
        let by_path: HashMap<_, _> = changes.into_iter().map(|c| (c.path.clone(), c)).collect();
        assert!(by_path.contains_key("plain.txt"));
        assert_eq!(by_path["plain.txt"].status, FileStatus::Modified);
    }

    #[test]
    fn three_dot_revision_peels_merge_base_to_tree() {
        // bough on a feature branch diffs against `main...HEAD`; the merge-base
        // is a *commit*, which must be peeled to a tree before building the
        // index. This previously failed with "expected tree but got commit".
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        let run = |args: &[&str]| {
            let s = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(s.success());
        };
        run(&["config", "user.email", "t@e"]);
        run(&["config", "user.name", "T"]);

        // main commit.
        std::fs::write(dir.path().join("base.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        run(&["branch", "-m", "main"]);

        // feature branch with an extra commit.
        run(&["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("base.txt"), "one\ntwo\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "feature"]);

        // An uncommitted change on top.
        std::fs::write(dir.path().join("base.txt"), "one\ntwo\nthree\n").unwrap();

        // `main...HEAD` is exactly what bough uses on a feature branch.
        let changes = collect_changes(&repo, "main...HEAD").unwrap();
        let by_path: HashMap<_, _> = changes.into_iter().map(|c| (c.path.clone(), c)).collect();
        assert_eq!(by_path["base.txt"].status, FileStatus::Modified);
        // 2 additions vs main's merge-base: the committed "two" plus the
        // uncommitted "three".
        assert_eq!(by_path["base.txt"].added, 2);
        assert_eq!(by_path["base.txt"].deleted, 0);
    }
}
