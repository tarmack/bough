//! Working-tree status and per-file diff metadata: changed-file listing,
//! untracked-file enumeration, per-file added/removed line counts, and
//! per-file metadata used to annotate the tree/diff views.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    let repo = open_repo().ok();
    let mut map = HashMap::new();
    for path in paths {
        let lines = match repo.as_ref() {
            Some(repo) => line_count(&workdir_path(repo, path)),
            None => line_count(Path::new(path)),
        };
        map.insert(path.clone(), (lines, 0));
    }
    map
}

/// Metadata for an untracked file: a pure addition against the empty tree.
pub fn file_meta_untracked(path: &str) -> Result<FileMeta, GitError> {
    let repo = open_repo()?;
    let mut cache = resource_cache(&repo)?;
    let (added, deleted, binary) = count_changes(&mut cache, &repo, None, path)?;
    Ok(FileMeta {
        status: FileStatus::Added,
        added,
        deleted,
        old_path: None,
        rename_percent: None,
        mode_old: None,
        mode_new: None,
        binary,
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
    for item in iter {
        // A single unreadable path skips; the scan must not abort.
        let Ok(item) = item else {
            continue;
        };
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

    let mut cache = resource_cache(repo)?;

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
    for res in iter {
        // One failed scan item (e.g. a file that cannot be read) must not
        // blank the whole tree: skip it and keep scanning.
        let item = match res {
            Ok(item) => item,
            Err(_) => continue,
        };
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
            (IWItem::Rewrite { source, diff, .. }, Summary::Renamed) => (
                FileStatus::Renamed,
                Some(rewrite_source_path(source)),
                diff.as_ref().map(rename_percent),
                Some(rewrite_source_id(source)),
                None,
                None,
            ),
            (IWItem::Rewrite { source, diff, .. }, Summary::Copied) => (
                FileStatus::Copied,
                Some(rewrite_source_path(source)),
                diff.as_ref().map(rename_percent),
                Some(rewrite_source_id(source)),
                None,
                None,
            ),
            // A conflicted (unmerged) entry has no unambiguous base content to
            // diff against, so report it as modified with no line stats.
            (IWItem::Modification { .. }, Summary::Conflict) => {
                changes.push(ChangeInfo {
                    status: FileStatus::Modified,
                    path: path.clone(),
                    old_path: None,
                    rename_percent: None,
                    mode_old: None,
                    mode_new: None,
                    added: 0,
                    deleted: 0,
                    binary: false,
                });
                continue;
            }
            // An untracked or intent-to-add file is a pure addition. Count it
            // against the empty tree like the other statuses so binary files are
            // detected rather than counted by scanning for newline bytes.
            (IWItem::DirectoryContents { .. }, Summary::Added) if !is_directory_item(&item) => {
                (FileStatus::Added, None, None, None, None, None)
            }
            (IWItem::Modification { .. }, Summary::IntentToAdd) => {
                (FileStatus::Added, None, None, None, None, None)
            }
            // Untracked directories and nested repositories are not files; skip
            // them rather than emit a bogus tree node.
            (IWItem::DirectoryContents { .. }, Summary::Added) => continue,
            (_, _other) => {
                // Unmapped summary (e.g. an unhandled conflict/rename combo):
                // skip the entry rather than fail the whole scan — one odd
                // entry must not blank the tree.
                continue;
            }
        };

        // When a single file's content cannot be read (permissions, broken
        // state, ...), keep the entry with unknown stats — like a conflict —
        // so the tree still loads; the per-file diff surfaces the error when
        // the file is selected.
        let (added, deleted, binary) =
            count_changes(&mut cache, repo, old_id, &path).unwrap_or((0, 0, false));
        cache.clear_resource_cache_keep_allocation();
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

/// A blob-diff resource cache rooted at the repository's working tree. The
/// source side always resolves from the object database by id; only the
/// destination side is read from the working tree.
fn resource_cache(repo: &gix::Repository) -> Result<diff::blob::Platform, GitError> {
    let roots = repo
        .workdir()
        .map(|p| diff::blob::pipeline::WorktreeRoots {
            old_root: None,
            new_root: Some(p.to_path_buf()),
        })
        .unwrap_or_default();
    repo.diff_resource_cache(diff::blob::pipeline::Mode::ToGit, roots)
        .map_err(|e| GitError::message(e.to_string()))
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
    // The two-dot (`main..HEAD`) form has no single unambiguous base to diff
    // against; reject it clearly instead of surfacing an opaque gix error.
    if revision.contains("..") && !revision.contains("...") {
        return Err(GitError::message(format!(
            "unsupported revision {revision:?}: use base...HEAD"
        )));
    }
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
            Operation::SourceOrDestinationIsBinary => Ok((0, 0, true)),
            // gix only reports an external diff command after ruling out binary
            // content, so a path with a custom diff driver is text, not binary.
            // bough does not run the external command; report no line counts.
            Operation::ExternalCommand { .. } => Ok((0, 0, false)),
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

/// A rewrite's similarity as a whole-number percentage (0–100).
fn rename_percent(stats: &diff::blob::DiffLineStats) -> u8 {
    (stats.similarity * 100.0).round().clamp(0.0, 100.0) as u8
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

/// Read cap for line counting: enough for any source file, small enough that a
/// huge file cannot exhaust memory.
const MAX_COUNT_BYTES: u64 = 8 * 1024 * 1024;

/// Resolve a repository-relative path against `repo`'s working tree, falling
/// back to the path as-is when the repository is bare.
fn workdir_path(repo: &gix::Repository, path: &str) -> PathBuf {
    repo.workdir()
        .map(|root| root.join(path))
        .unwrap_or_else(|| PathBuf::from(path))
}

/// Count the lines (LF) in a regular file; 0 when it is missing, is not a
/// regular file (so symlinks, FIFOs and devices are never followed or read), or
/// cannot be read. At most [`MAX_COUNT_BYTES`] are read.
fn line_count(path: &Path) -> usize {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.file_type().is_file() {
        return 0;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut bytes = Vec::new();
    if file
        .by_ref()
        .take(MAX_COUNT_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return 0;
    }
    if bytes.is_empty() {
        0
    } else {
        bytes.iter().filter(|&&b| b == b'\n').count() + usize::from(!bytes.ends_with(b"\n"))
    }
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

    #[cfg(unix)]
    #[test]
    fn unreadable_worktree_file_does_not_abort_scan() {
        // One file whose content cannot be read must not blank the tree. The
        // untracked one degrades to a zero-stat entry; the modified one is
        // rejected by the scanner itself (no path info on the item error) and
        // is skipped — either way the scan must succeed for the rest.
        // (Runs as root both stay readable and the assertions still hold.)
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        let repo = gix::discover(dir.path()).unwrap();

        // A modified tracked file and an untracked file, both unreadable.
        std::fs::write(dir.path().join("file0.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.path().join("file0.txt"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        std::fs::set_permissions(
            dir.path().join("new.txt"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let changes = collect_changes(&repo, "HEAD").unwrap();
        let by_path: HashMap<_, _> = changes.into_iter().map(|c| (c.path.clone(), c)).collect();
        assert!(
            by_path.contains_key("new.txt"),
            "unreadable untracked file must still appear: {by_path:?}"
        );
        // When the modified file is listed at all, its stats degrade to zero
        // rather than aborting.
        if let Some(c) = by_path.get("file0.txt") {
            assert_eq!((c.added, c.deleted), (0, 0));
        }
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

    #[test]
    fn line_count_never_follows_links_or_reads_non_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "one\ntwo\n").unwrap();

        assert_eq!(line_count(&dir.path().join("real.txt")), 2);
        assert_eq!(line_count(&dir.path().join("missing")), 0);
        // A directory is not a regular file.
        assert_eq!(line_count(dir.path()), 0);

        #[cfg(unix)]
        {
            // A symlink to a device must not be followed or read.
            std::os::unix::fs::symlink("/dev/zero", dir.path().join("evil")).unwrap();
            assert_eq!(line_count(&dir.path().join("evil")), 0);
        }
    }

    #[test]
    fn untracked_binary_file_is_flagged_binary() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        let repo = gix::discover(dir.path()).unwrap();
        // PNG signature: contains a NUL byte, so gix detects it as binary.
        std::fs::write(
            dir.path().join("img.png"),
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00",
        )
        .unwrap();

        let changes = collect_changes(&repo, "HEAD").unwrap();
        let by_path: HashMap<_, _> = changes.into_iter().map(|c| (c.path.clone(), c)).collect();
        let img = &by_path["img.png"];
        assert_eq!(img.status, FileStatus::Added);
        assert!(img.binary);
        assert_eq!(img.added, 0);
        assert_eq!(img.deleted, 0);
    }

    #[test]
    fn two_dot_revision_is_rejected_clearly() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        let repo = gix::discover(dir.path()).unwrap();
        let err = base_tree_id(&repo, "HEAD..HEAD").unwrap_err();
        assert!(err.to_string().contains("unsupported revision"));
    }
}
