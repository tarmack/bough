//! Working-tree status and per-file diff metadata: changed-file listing,
//! untracked-file enumeration, per-file added/removed line counts, and
//! per-file metadata used to annotate the tree/diff views.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use gix::diff::blob::platform::prepare_diff::Operation;
use gix::object::tree::diff::ChangeDetached;
use gix::status::index_worktree::iter::Summary;
use gix::status::index_worktree::{Item as IWItem, RewriteSource};
use gix::{diff, progress};

use crate::diff::FileStatus;
use crate::git::lfs::{self, LfsMatcher};
use crate::git::{
    diff_resource_cache, odb_resource_cache, open_repo, resolve_id, set_blob_resources,
    set_odb_resources, FileMeta, GitError,
};

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
    let mut cache = diff_resource_cache(&repo)?;
    let mut lfs = LfsMatcher::new(&repo);
    let (added, deleted, binary) =
        count_worktree_changes(&mut cache, &mut lfs, &repo, None, None, path)?;
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

/// A path's committed change (base tree → HEAD tree): blob ids, kind and
/// rename details, with no content diff computed yet.
struct CommittedChange {
    status: FileStatus,
    old_id: Option<gix::ObjectId>,
    new_id: Option<gix::ObjectId>,
    old_path: Option<String>,
    rename_percent: Option<u8>,
    mode_old: Option<u32>,
    mode_new: Option<u32>,
}

/// A path whose index content differs from HEAD (a staged change). The old side
/// is HEAD's blob, the new side the index blob.
struct StagedChange {
    status: FileStatus,
    old_id: Option<gix::ObjectId>,
    new_id: Option<gix::ObjectId>,
    old_path: Option<String>,
    mode_old: Option<u32>,
    mode_new: Option<u32>,
}

/// A path whose worktree content differs from the index (an unstaged change).
struct UnstagedChange {
    status: FileStatus,
    old_id: Option<gix::ObjectId>,
    old_path: Option<String>,
    rename_percent: Option<u8>,
    mode_old: Option<u32>,
    mode_new: Option<u32>,
    /// An unmerged entry has no unambiguous base content; its stats stay zero.
    conflicted: bool,
}

/// Enumerate all changes against `revision`'s tree (including untracked),
/// computing per-file status and line counts.
///
/// `revision` may be a plain tree-ish (`HEAD`) or a three-dot range
/// (`main...HEAD`), in which case the base is the merge-base of the two sides.
///
/// The result is the union of three cheap scans describing the worktree content
/// relative to the base tree:
///
/// - the committed set (base tree → HEAD tree, pure object-database diff),
/// - the worktree-dirty set (HEAD index → worktree for unstaged edits, HEAD tree
///   → index for staged edits), and
/// - untracked files.
///
/// Stats compare the base (or HEAD, when the file was not committed) against the
/// worktree only for paths that actually differ from HEAD; a committed-only
/// change is diffed entirely from the object database, so a clean worktree
/// reads no file content and runs no content filters.
pub(crate) fn collect_changes(
    repo: &gix::Repository,
    revision: &str,
) -> Result<Vec<ChangeInfo>, GitError> {
    let base_tree = base_tree_id(repo, revision)?;
    let head_tree = repo
        .head_tree_id()
        .map_err(|e| GitError::message(format!("cannot resolve HEAD tree: {e}")))?
        .detach();

    let mut wt_cache = diff_resource_cache(repo)?;
    let mut odb_cache = odb_resource_cache(repo)?;
    let mut lfs = LfsMatcher::new(repo);

    let mut committed = committed_changes(repo, &base_tree, &head_tree)?;
    let mut staged = staged_changes(repo, &head_tree);
    let (mut unstaged, untracked) = unstaged_changes(repo)?;
    let untracked: BTreeSet<String> = untracked.into_iter().collect();

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(committed.keys().cloned());
    paths.extend(staged.keys().cloned());
    paths.extend(unstaged.keys().cloned());
    paths.extend(untracked.iter().cloned());

    let mut changes = Vec::with_capacity(paths.len());
    for path in paths {
        let c = committed.remove(&path);
        let s = staged.remove(&path);
        let u = unstaged.remove(&path);
        let worktree_dirty = s.is_some() || u.is_some();

        // Resolve the status and the diff sides. The committed kind wins when
        // present (a worktree edit on a file committed as `A` stays `A`). A path
        // in the dirty set but not committed compares HEAD against the worktree;
        // a committed path compares base against the worktree when dirty, else
        // entirely from the object database.
        let (
            status,
            old_id,
            odb_new,
            hint,
            old_path,
            rename_pct,
            mode_old,
            mode_new,
            use_worktree,
            force_zero,
        ) = if let Some(c) = c {
            let deleted = c.status == FileStatus::Deleted;
            let use_worktree = !deleted && worktree_dirty;
            let odb_new = if use_worktree { None } else { c.new_id };
            (
                c.status,
                c.old_id,
                odb_new,
                c.new_id,
                c.old_path,
                c.rename_percent,
                c.mode_old,
                c.mode_new,
                use_worktree,
                false,
            )
        } else if let Some(s) = s {
            (
                s.status, s.old_id, None, s.new_id, s.old_path, None, s.mode_old, s.mode_new, true,
                false,
            )
        } else if let Some(u) = u {
            (
                u.status,
                u.old_id,
                None,
                None,
                u.old_path,
                u.rename_percent,
                u.mode_old,
                u.mode_new,
                true,
                u.conflicted,
            )
        } else {
            (
                FileStatus::Added,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                true,
                false,
            )
        };

        // When a single file's content cannot be read (permissions, broken
        // state, ...), keep the entry with unknown stats so the tree still
        // loads; the per-file diff surfaces the error when the file is selected.
        let counted = if force_zero {
            Ok((0, 0, false))
        } else if use_worktree {
            count_worktree_changes(&mut wt_cache, &mut lfs, repo, old_id, hint, &path)
        } else {
            count_odb_changes(&mut odb_cache, &mut lfs, repo, old_id, odb_new, &path)
        };
        let (added, deleted, binary) = counted.unwrap_or((0, 0, false));
        wt_cache.clear_resource_cache_keep_allocation();
        odb_cache.clear_resource_cache_keep_allocation();
        changes.push(ChangeInfo {
            status,
            path,
            old_path,
            rename_percent: rename_pct,
            mode_old,
            mode_new,
            added,
            deleted,
            binary,
        });
    }
    Ok(changes)
}

/// The committed set: changes needed to turn the base tree into HEAD's tree,
/// read entirely from the object database. Directory entries are skipped (bough
/// treats every change as a file), and rename tracking mirrors the previous
/// status scan (`Rewrites::default()`).
fn committed_changes(
    repo: &gix::Repository,
    base_tree: &gix::ObjectId,
    head_tree: &gix::ObjectId,
) -> Result<HashMap<String, CommittedChange>, GitError> {
    let base = repo
        .find_tree(*base_tree)
        .map_err(|e| GitError::message(format!("cannot open base tree: {e}")))?;
    let head = repo
        .find_tree(*head_tree)
        .map_err(|e| GitError::message(format!("cannot open HEAD tree: {e}")))?;

    let mut options = diff::Options::default();
    options.track_path();
    options.track_rewrites(Some(diff::Rewrites::default()));
    let changes = repo
        .diff_tree_to_tree(Some(&base), Some(&head), options)
        .map_err(|e| GitError::message(format!("committed diff: {e}")))?;

    let mut map = HashMap::new();
    for change in changes {
        match change {
            ChangeDetached::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                map.insert(
                    location.to_string(),
                    CommittedChange {
                        status: FileStatus::Added,
                        old_id: None,
                        new_id: Some(id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: None,
                        mode_new: Some(u32::from(entry_mode.value())),
                    },
                );
            }
            ChangeDetached::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                map.insert(
                    location.to_string(),
                    CommittedChange {
                        status: FileStatus::Deleted,
                        old_id: Some(id),
                        new_id: None,
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(u32::from(entry_mode.value())),
                        mode_new: None,
                    },
                );
            }
            ChangeDetached::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
            } => {
                if entry_mode.is_tree() || previous_entry_mode.is_tree() {
                    continue;
                }
                let status = if previous_entry_mode.kind() != entry_mode.kind() {
                    FileStatus::TypeChanged
                } else {
                    FileStatus::Modified
                };
                map.insert(
                    location.to_string(),
                    CommittedChange {
                        status,
                        old_id: Some(previous_id),
                        new_id: Some(id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(u32::from(previous_entry_mode.value())),
                        mode_new: Some(u32::from(entry_mode.value())),
                    },
                );
            }
            ChangeDetached::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                diff,
                entry_mode,
                location,
                id,
                copy,
                ..
            } => {
                if entry_mode.is_tree() || source_entry_mode.is_tree() {
                    continue;
                }
                let status = if copy {
                    FileStatus::Copied
                } else {
                    FileStatus::Renamed
                };
                map.insert(
                    location.to_string(),
                    CommittedChange {
                        status,
                        old_id: Some(source_id),
                        new_id: Some(id),
                        old_path: Some(source_location.to_string()),
                        rename_percent: diff.as_ref().map(rename_percent),
                        mode_old: Some(u32::from(source_entry_mode.value())),
                        mode_new: Some(u32::from(entry_mode.value())),
                    },
                );
            }
        }
    }
    Ok(map)
}

/// The staged set: changes between HEAD's tree and the on-disk index. Degrades
/// to an empty set when the comparison is unavailable (e.g. path validation
/// rejects a tracked name) rather than blanking the whole listing.
fn staged_changes(
    repo: &gix::Repository,
    head_tree: &gix::ObjectId,
) -> HashMap<String, StagedChange> {
    let mut map = HashMap::new();
    let Ok(index) = repo.index_or_empty() else {
        return map;
    };
    let renames = gix::status::tree_index::TrackRenames::Given(diff::Rewrites::default());
    let result = repo.tree_index_status(head_tree, &index, None, renames, |change, _, _| {
        match change.into_owned() {
            diff::index::Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                map.insert(
                    location.to_string(),
                    StagedChange {
                        status: FileStatus::Added,
                        old_id: None,
                        new_id: Some(id.into_owned()),
                        old_path: None,
                        mode_old: None,
                        mode_new: Some(entry_mode.bits()),
                    },
                );
            }
            diff::index::Change::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                map.insert(
                    location.to_string(),
                    StagedChange {
                        status: FileStatus::Deleted,
                        old_id: Some(id.into_owned()),
                        new_id: None,
                        old_path: None,
                        mode_old: Some(entry_mode.bits()),
                        mode_new: None,
                    },
                );
            }
            diff::index::Change::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
                ..
            } => {
                let status = if index_type_changed(previous_entry_mode, entry_mode) {
                    FileStatus::TypeChanged
                } else {
                    FileStatus::Modified
                };
                map.insert(
                    location.to_string(),
                    StagedChange {
                        status,
                        old_id: Some(previous_id.into_owned()),
                        new_id: Some(id.into_owned()),
                        old_path: None,
                        mode_old: Some(previous_entry_mode.bits()),
                        mode_new: Some(entry_mode.bits()),
                    },
                );
            }
            diff::index::Change::Rewrite {
                source_location,
                source_id,
                location,
                id,
                copy,
                ..
            } => {
                map.insert(
                    location.to_string(),
                    StagedChange {
                        status: if copy {
                            FileStatus::Copied
                        } else {
                            FileStatus::Renamed
                        },
                        old_id: Some(source_id.into_owned()),
                        new_id: Some(id.into_owned()),
                        old_path: Some(source_location.to_string()),
                        mode_old: None,
                        mode_new: None,
                    },
                );
            }
        }
        Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
    });
    if result.is_err() {
        map.clear();
    }
    map
}

/// Whether an index mode change crossed entry types (file ↔ symlink ↔
/// submodule), which reads as a type change rather than a content modification.
fn index_type_changed(previous: gix::index::entry::Mode, current: gix::index::entry::Mode) -> bool {
    (previous.bits() & 0o170000) != (current.bits() & 0o170000)
}

/// The unstaged set (index → worktree, using the on-disk index so unchanged
/// files stat-skip and are never hashed) plus the untracked files. One failed
/// item is skipped rather than blanking the scan.
fn unstaged_changes(
    repo: &gix::Repository,
) -> Result<(HashMap<String, UnstagedChange>, Vec<String>), GitError> {
    let platform = repo
        .status(progress::Discard)
        .map_err(|e| GitError::message(e.to_string()))?;
    let platform = platform
        .index_worktree_rewrites(Some(diff::Rewrites::default()))
        // Report each untracked file individually; the default collapses an
        // untracked directory to a single entry (its path), which reads as a
        // file to the tree and has no line count.
        .untracked_files(gix::status::UntrackedFiles::Files);
    let iter = platform
        .into_index_worktree_iter(std::iter::empty())
        .map_err(|e| GitError::message(e.to_string()))?;

    let mut map = HashMap::new();
    let mut untracked = Vec::new();
    for res in iter {
        let item = match res {
            Ok(item) => item,
            Err(_) => continue,
        };
        let summary = match item.summary() {
            Some(s) => s,
            None => continue,
        };
        let path = item.rela_path().to_string();
        match (&item, summary) {
            (IWItem::Modification { entry, .. }, Summary::Removed) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Deleted,
                        old_id: Some(entry.id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(entry.mode.bits()),
                        mode_new: Some(entry.mode.bits()),
                        conflicted: false,
                    },
                );
            }
            (IWItem::Modification { entry, .. }, Summary::TypeChange) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::TypeChanged,
                        old_id: Some(entry.id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(entry.mode.bits()),
                        mode_new: Some(entry.mode.bits()),
                        conflicted: false,
                    },
                );
            }
            (IWItem::Modification { entry, .. }, Summary::Modified) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Modified,
                        old_id: Some(entry.id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(entry.mode.bits()),
                        mode_new: Some(entry.mode.bits()),
                        conflicted: false,
                    },
                );
            }
            (IWItem::Rewrite { source, diff, .. }, Summary::Renamed) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Renamed,
                        old_id: Some(rewrite_source_id(source)),
                        old_path: Some(rewrite_source_path(source)),
                        rename_percent: diff.as_ref().map(rename_percent),
                        mode_old: None,
                        mode_new: None,
                        conflicted: false,
                    },
                );
            }
            (IWItem::Rewrite { source, diff, .. }, Summary::Copied) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Copied,
                        old_id: Some(rewrite_source_id(source)),
                        old_path: Some(rewrite_source_path(source)),
                        rename_percent: diff.as_ref().map(rename_percent),
                        mode_old: None,
                        mode_new: None,
                        conflicted: false,
                    },
                );
            }
            // A conflicted (unmerged) entry has no unambiguous base content to
            // diff against, so report it as modified with no line stats.
            (IWItem::Modification { .. }, Summary::Conflict) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Modified,
                        old_id: None,
                        old_path: None,
                        rename_percent: None,
                        mode_old: None,
                        mode_new: None,
                        conflicted: true,
                    },
                );
            }
            // An intent-to-add file is a pure addition against the empty tree.
            (IWItem::Modification { .. }, Summary::IntentToAdd) => {
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Added,
                        old_id: None,
                        old_path: None,
                        rename_percent: None,
                        mode_old: None,
                        mode_new: None,
                        conflicted: false,
                    },
                );
            }
            // An untracked file is a pure addition. Count it against the empty
            // tree like the other statuses so binary files are detected rather
            // than counted by scanning for newline bytes.
            (IWItem::DirectoryContents { .. }, Summary::Added) if !is_directory_item(&item) => {
                untracked.push(path);
            }
            // Untracked directories and nested repositories are not files; skip
            // them rather than emit a bogus tree node. Unmapped summaries are
            // skipped too: one odd entry must not blank the tree.
            _ => continue,
        }
    }
    Ok((map, untracked))
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
/// (or the empty blob when `None`) against the worktree content at `path`. This
/// is the only scan path that reads worktree content, so it may run content
/// filters (e.g. `git lfs clean`). `new_hint` is the new-side blob when known
/// from the object database, used solely to classify LFS without a worktree
/// read.
fn count_worktree_changes(
    cache: &mut diff::blob::Platform,
    lfs: &mut LfsMatcher<'_>,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    new_hint: Option<gix::ObjectId>,
    path: &str,
) -> Result<(usize, usize, bool), GitError> {
    if lfs::detect(repo, lfs, old_id, new_hint, path).is_some() {
        return Ok((0, 0, true));
    }
    set_blob_resources(cache, repo, old_id, path)?;
    finish_count(cache, path)
}

/// Count added/deleted lines and detect binary for an object-database-only diff
/// of the blobs at `old_id` and `new_id` (each the empty blob when `None`). No
/// worktree content is read and no content filter runs. LFS pointers are
/// reported as `(0, 0, binary)`.
fn count_odb_changes(
    cache: &mut diff::blob::Platform,
    lfs: &mut LfsMatcher<'_>,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    new_id: Option<gix::ObjectId>,
    path: &str,
) -> Result<(usize, usize, bool), GitError> {
    if lfs::detect(repo, lfs, old_id, new_id, path).is_some() {
        return Ok((0, 0, true));
    }
    set_odb_resources(cache, repo, old_id, new_id, path)?;
    finish_count(cache, path)
}

/// Prepare and run the diff for the resources already set on `cache`, returning
/// `(added, deleted, binary)`.
fn finish_count(
    cache: &mut diff::blob::Platform,
    path: &str,
) -> Result<(usize, usize, bool), GitError> {
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

    #[test]
    fn lfs_pointer_change_counts_as_binary() {
        // A stored pointer must be reported as a binary stub with no line
        // stats, without running the content diff.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
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

        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer = |n: u64| {
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize {n}\n")
        };

        std::fs::write(dir.path().join("big.bin"), pointer(100)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "add pointer"]);
        std::fs::write(dir.path().join("big.bin"), pointer(200)).unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = collect_changes(&repo, "HEAD").unwrap();
        let by_path: HashMap<_, _> = changes.into_iter().map(|c| (c.path.clone(), c)).collect();
        let big = &by_path["big.bin"];
        assert_eq!(big.status, FileStatus::Modified);
        assert!(big.binary);
        assert_eq!((big.added, big.deleted), (0, 0));
    }

    /// Configure a content-clean filter named `name` for `glob` that appends a
    /// line to `marker` whenever gix converts worktree content while passing the
    /// content through unchanged. The marker is the observable that proves
    /// whether a scan read (and thus hashed) file content. The attribute change
    /// is committed so a subsequently-added file is stat-consistent.
    #[cfg(unix)]
    fn install_marker_filter(dir: &Path, marker: &Path, glob: &str, name: &str) {
        let cmd = format!("sh -c 'echo run >> {}; cat'", marker.display());
        crate::git::test_util::run_git(dir, &["config", &format!("filter.{name}.clean"), &cmd]);
        std::fs::write(
            dir.join(".gitattributes"),
            format!("{glob} filter={name}\n"),
        )
        .unwrap();
        crate::git::test_util::run_git(dir, &["add", ".gitattributes"]);
        crate::git::test_util::run_git(dir, &["commit", "-m", "marker attrs"]);
    }

    /// Write `name` with `content`, stage it, and commit. Runs after the marker
    /// filter is installed so the index stat and blob stay consistent. The file
    /// mtime is backdated before staging so a later index write is not
    /// racy-clean, which would force a content check even for a clean file.
    #[cfg(unix)]
    fn commit_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        backdate(&dir.join(name), 2);
        crate::git::test_util::run_git(dir, &["add", name]);
        crate::git::test_util::run_git(dir, &["commit", "-m", &format!("add {name}")]);
    }

    /// Set a file's mtime into the past so a subsequent index write is strictly
    /// newer, defeating gix's racy-git content re-check for a clean file.
    #[cfg(unix)]
    fn backdate(path: &Path, seconds: u32) {
        let status = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("{seconds} seconds ago"))
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Create a `feature` branch whose HEAD modifies `file0.txt` to `committed`,
    /// leaving `main` at `base`. Returns once HEAD is on `feature`.
    #[cfg(unix)]
    fn commit_on_feature(dir: &Path, base: &str, committed: &str) {
        std::fs::write(dir.join("file0.txt"), base).unwrap();
        backdate(&dir.join("file0.txt"), 3);
        crate::git::test_util::run_git(dir, &["add", "."]);
        crate::git::test_util::run_git(dir, &["commit", "-m", "base"]);
        crate::git::test_util::run_git(dir, &["branch", "-m", "main"]);
        crate::git::test_util::run_git(dir, &["checkout", "-b", "feature"]);
        std::fs::write(dir.join("file0.txt"), committed).unwrap();
        backdate(&dir.join("file0.txt"), 2);
        crate::git::test_util::run_git(dir, &["add", "."]);
        crate::git::test_util::run_git(dir, &["commit", "-m", "feature"]);
    }

    fn by_path(changes: Vec<ChangeInfo>) -> HashMap<String, ChangeInfo> {
        changes.into_iter().map(|c| (c.path.clone(), c)).collect()
    }

    #[cfg(unix)]
    #[test]
    fn clean_scan_does_not_read_worktree_content() {
        // A clean worktree must be enumerated from stat data alone: no content
        // hashing, so the configured clean filter never runs.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("marker");
        install_marker_filter(dir.path(), &marker, "*.txt", "marker");
        commit_file(dir.path(), "file0.txt", "one\n");
        let _ = std::fs::remove_file(&marker);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = collect_changes(&repo, "HEAD").unwrap();
        assert!(changes.is_empty(), "clean worktree: {changes:?}");
        assert!(
            !marker.exists(),
            "clean scan must not hash worktree content"
        );

        // Sanity: a real modification does compare content.
        std::fs::write(dir.path().join("file0.txt"), "two\n").unwrap();
        let changes = collect_changes(&repo, "HEAD").unwrap();
        assert_eq!(changes.len(), 1);
        assert!(marker.exists(), "a modified file must be content-compared");
    }

    #[cfg(unix)]
    #[test]
    fn committed_only_change_uses_odb_diff_without_worktree_read() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("marker");
        install_marker_filter(dir.path(), &marker, "*.txt", "marker");

        commit_on_feature(dir.path(), "one\n", "one\ntwo\n");
        let _ = std::fs::remove_file(&marker);
        // Clean worktree: the committed change is diffed from the object
        // database, so the filter must not run.
        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!((f.added, f.deleted), (1, 0));
        assert!(
            !marker.exists(),
            "committed-only diff must not read the worktree"
        );
    }

    #[test]
    fn doubly_changed_file_diffs_base_against_worktree() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        commit_on_feature(dir.path(), "A\n", "B\n");
        // A further worktree edit on top of the committed change.
        std::fs::write(dir.path().join("file0.txt"), "C\n").unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Modified);
        // base "A\n" vs worktree "C\n", not HEAD "B\n".
        assert_eq!((f.added, f.deleted), (1, 1));
    }

    #[test]
    fn staged_change_uses_worktree_vs_base_stats() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        commit_on_feature(dir.path(), "l1\n", "l1\nl2\n");
        // Stage a third line; the worktree matches the index (clean).
        std::fs::write(dir.path().join("file0.txt"), "l1\nl2\nl3\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "file0.txt"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Modified);
        // base "l1\n" vs staged worktree: two additions, not the one the
        // committed base→HEAD diff alone would give.
        assert_eq!((f.added, f.deleted), (2, 0));
    }

    #[test]
    fn committed_lfs_pointer_change_is_binary_stub() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
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

        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer = |n: u64| {
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize {n}\n")
        };
        std::fs::write(dir.path().join("big.bin"), pointer(100)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        run(&["branch", "-m", "main"]);
        run(&["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("big.bin"), pointer(200)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "pointer"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let big = &changes["big.bin"];
        assert_eq!(big.status, FileStatus::Modified);
        assert!(big.binary);
        assert_eq!((big.added, big.deleted), (0, 0));
    }

    #[test]
    fn converted_to_lfs_committed_change_is_binary_stub() {
        // Old side is plain text, new side a pointer: the LFS detection gap.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
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
        std::fs::write(dir.path().join("big.bin"), "plain text\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        run(&["branch", "-m", "main"]);
        run(&["checkout", "-b", "feature"]);

        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 42\n");
        std::fs::write(dir.path().join("big.bin"), pointer).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "pointer"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let big = &changes["big.bin"];
        assert_eq!(big.status, FileStatus::Modified);
        assert!(big.binary);
        assert_eq!((big.added, big.deleted), (0, 0));
    }

    #[test]
    fn doubly_changed_lfs_file_is_binary_stub() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
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
        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer = |n: u64| {
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize {n}\n")
        };
        std::fs::write(dir.path().join("big.bin"), pointer(100)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        run(&["branch", "-m", "main"]);
        run(&["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("big.bin"), pointer(200)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "pointer"]);
        // A further worktree edit on the committed pointer.
        std::fs::write(dir.path().join("big.bin"), pointer(300)).unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let big = &changes["big.bin"];
        assert_eq!(big.status, FileStatus::Modified);
        assert!(big.binary);
        assert_eq!((big.added, big.deleted), (0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn clean_lfs_worktree_scan_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one\n"]);
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("marker");
        install_marker_filter(dir.path(), &marker, "*.bin", "lfs");

        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 100\n");
        std::fs::write(dir.path().join("big.bin"), pointer).unwrap();
        backdate(&dir.path().join("big.bin"), 2);
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "pointer"]);
        let _ = std::fs::remove_file(&marker);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = collect_changes(&repo, "HEAD").unwrap();
        assert!(changes.is_empty(), "clean LFS worktree: {changes:?}");
        assert!(!marker.exists(), "no LFS filter must run on a clean scan");
    }

    #[test]
    fn committed_rename_reports_renamed() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("old.txt"), body).unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "base"]);
        crate::git::test_util::run_git(dir.path(), &["branch", "-m", "main"]);
        crate::git::test_util::run_git(dir.path(), &["checkout", "-b", "feature"]);
        crate::git::test_util::run_git(dir.path(), &["mv", "old.txt", "new.txt"]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "rename"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(collect_changes(&repo, "main...HEAD").unwrap());
        let renamed = &changes["new.txt"];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
    }
}
