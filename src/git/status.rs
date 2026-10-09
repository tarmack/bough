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
    diff_resource_cache, entry_kind_for, odb_resource_cache, open_repo, resolve_id,
    set_blob_resources, set_odb_resources, split_range, worktree_entry_kind, GitError,
};

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
    all_changes_in(&repo, revision)
}

/// [`all_changes`] against an already-open repository, so a caller holding a
/// repository can scan without re-discovering it.
///
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
///
/// Precedence (scenario → status / diff sides):
/// - committed `D` present in index/worktree → the re-add's status, base → present content;
/// - committed `D` absent everywhere → `D`, base → empty (ODB);
/// - committed `A` removed from worktree → `D`, empty (no worktree read);
/// - other committed → committed status, base → worktree when dirty else HEAD;
/// - staged `rm --cached` (index `D`, file still on disk) → `A`, base → worktree;
/// - staged `D` with the file gone → `D`, base → empty (no worktree read);
/// - other staged → staged status, base → worktree;
/// - unstaged → its status, index → worktree;
/// - untracked → `A`, empty → worktree.
pub(crate) fn all_changes_in(
    repo: &gix::Repository,
    revision: &str,
) -> Result<Vec<ChangeInfo>, GitError> {
    all_changes_in_with_sets(repo, revision).map(|(changes, _)| changes)
}

/// [`all_changes_in`] plus the underlying [`ChangeSets`], so a caller that
/// needs a second listing against another base (the worker's uncommitted
/// pass) can reuse the expensive workdir scans instead of re-running them.
pub(crate) fn all_changes_in_with_sets(
    repo: &gix::Repository,
    revision: &str,
) -> Result<(Vec<ChangeInfo>, ChangeSets), GitError> {
    let base_tree = base_tree_id(repo, revision)?;
    let sets = change_sets(repo, &base_tree)?;
    let changes = count_and_merge(repo, sets.clone())?;
    Ok((changes, sets))
}

/// For a no-commit repo: every file is added; count its lines as the added
/// count and 0 deletions.
///
/// A repository that cannot be opened yields an empty map, so every path
/// degrades to zeroed stats at the consumer (`stats.get(..).unwrap_or`).
/// Counting relative to the process cwd instead — the previous fallback —
/// would fabricate counts read from the wrong directory. This branch is
/// defensive: `initial_listing`, which every caller runs first and which
/// needs the same repository, has already failed on that condition.
pub fn initial_stats(paths: &[String]) -> HashMap<String, (usize, usize)> {
    let Ok(repo) = open_repo() else {
        return HashMap::new();
    };
    paths
        .iter()
        .map(|path| (path.clone(), (line_count(&workdir_path(&repo, path)), 0)))
        .collect()
}

/// Metadata for an untracked file: a pure addition against the empty tree.
pub fn file_meta_untracked(path: &str) -> Result<FileMeta, GitError> {
    let repo = open_repo()?;
    let mut cache = diff_resource_cache(&repo)?;
    let mut lfs = LfsMatcher::new(&repo);
    let (added, deleted, binary) =
        count_worktree_changes(&mut cache, &mut lfs, &repo, (None, None), None, path)?;
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

/// A status platform that enumerates each untracked file individually. The
/// default collapses an untracked directory to a single entry (its path),
/// which reads as a file to the tree and has no line count. Shared setup for
/// [`untracked_paths`] and [`unstaged_changes`].
fn untracked_files_platform(
    repo: &gix::Repository,
) -> Result<gix::status::Platform<'_, gix::progress::Discard>, GitError> {
    let platform = repo
        .status(progress::Discard)
        .map_err(|e| GitError::message(e.to_string()))?;
    Ok(platform.untracked_files(gix::status::UntrackedFiles::Files))
}

/// Untracked, non-ignored files in the working tree (relative paths).
fn untracked_paths(repo: &gix::Repository) -> Result<Vec<String>, GitError> {
    let platform = untracked_files_platform(repo)?;
    let iter = match platform.into_index_worktree_iter(std::iter::empty()) {
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
#[derive(Clone)]
pub(crate) struct CommittedChange {
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
#[derive(Clone)]
pub(crate) struct StagedChange {
    status: FileStatus,
    old_id: Option<gix::ObjectId>,
    new_id: Option<gix::ObjectId>,
    old_path: Option<String>,
    rename_percent: Option<u8>,
    mode_old: Option<u32>,
    mode_new: Option<u32>,
}

/// A path whose worktree content differs from the index (an unstaged change).
#[derive(Clone)]
pub(crate) struct UnstagedChange {
    status: FileStatus,
    old_id: Option<gix::ObjectId>,
    old_path: Option<String>,
    rename_percent: Option<u8>,
    mode_old: Option<u32>,
    mode_new: Option<u32>,
    /// An unmerged entry has no unambiguous base content; its stats stay zero.
    conflicted: bool,
}

/// One path's resolved diff sides, before line counting: the precedence-
/// resolved merge of the path's committed/staged/unstaged/untracked records
/// (see [`all_changes_in`]). Field names match their use in the counting
/// step below.
struct ResolvedChange {
    status: FileStatus,
    old_id: Option<gix::ObjectId>,
    /// The new-side blob to diff entirely from the object database; `None`
    /// when the new side is read from the worktree (or absent).
    odb_new: Option<gix::ObjectId>,
    /// The new-side blob when known from the object database, used solely to
    /// classify LFS without a worktree read.
    new_hint: Option<gix::ObjectId>,
    old_path: Option<String>,
    rename_percent: Option<u8>,
    mode_old: Option<u32>,
    mode_new: Option<u32>,
    /// Diff the worktree content instead of the object database.
    use_worktree: bool,
    /// Report zeroed stats regardless of content (a conflicted entry, or a
    /// net-empty change reported as a deletion).
    force_zero: bool,
}

/// The cheap scans behind [`count_and_merge`]: the committed (base tree →
/// HEAD tree), staged (HEAD tree → index) and unstaged (index → worktree)
/// change sets plus the untracked files. Only `committed` depends on the
/// base tree, so a caller that needs two listings against different bases
/// (the worker's branch-mode recompute) can share the worktree-dirty scans
/// between them instead of re-running them.
#[derive(Clone)]
pub(crate) struct ChangeSets {
    pub(crate) committed: HashMap<String, CommittedChange>,
    pub(crate) staged: HashMap<String, StagedChange>,
    pub(crate) unstaged: HashMap<String, UnstagedChange>,
    pub(crate) untracked: BTreeSet<String>,
}

impl ChangeSets {
    /// The same scans viewed from HEAD: drops the committed set — a HEAD→HEAD
    /// diff is empty by construction — keeping the dirty sets a second
    /// listing against HEAD needs (the worker's uncommitted pass).
    pub(crate) fn into_against_head(self) -> Self {
        Self {
            committed: HashMap::new(),
            staged: self.staged,
            unstaged: self.unstaged,
            untracked: self.untracked,
        }
    }
}

/// Run the cheap scans (no content reads) for a listing against the tree at
/// `base_tree`.
pub(crate) fn change_sets(
    repo: &gix::Repository,
    base_tree: &gix::ObjectId,
) -> Result<ChangeSets, GitError> {
    let head_tree = repo
        .head_tree_id()
        .map_err(|e| GitError::message(format!("cannot resolve HEAD tree: {e}")))?
        .detach();
    let committed = committed_changes(repo, base_tree, &head_tree)?;
    let staged = staged_changes(repo, &head_tree);
    let (unstaged, untracked) = unstaged_changes(repo)?;
    Ok(ChangeSets {
        committed,
        staged,
        unstaged,
        untracked: untracked.into_iter().collect(),
    })
}

/// Merge [`ChangeSets`] into one [`ChangeInfo`] per path, counting lines for
/// each (the only part of a scan that reads content). Paths present in
/// several sets are precedence-resolved; see [`all_changes_in`].
pub(crate) fn count_and_merge(
    repo: &gix::Repository,
    sets: ChangeSets,
) -> Result<Vec<ChangeInfo>, GitError> {
    let ChangeSets {
        mut committed,
        mut staged,
        mut unstaged,
        untracked,
    } = sets;
    let mut wt_cache = diff_resource_cache(repo)?;
    let mut odb_cache = odb_resource_cache(repo)?;
    let mut lfs = LfsMatcher::new(repo);

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(committed.keys().cloned());
    paths.extend(staged.keys().cloned());
    paths.extend(unstaged.keys().cloned());
    paths.extend(untracked.iter().cloned());

    // The on-disk index distinguishes an absent path (a real committed
    // deletion) from one re-added to the index.
    let index = repo.index_or_empty().ok();

    let mut changes = Vec::with_capacity(paths.len());
    for path in paths {
        let c = committed.remove(&path);
        let s = staged.remove(&path);
        let u = unstaged.remove(&path);
        let is_untracked = untracked.contains(&path);
        let worktree_present =
            is_untracked || u.as_ref().is_some_and(|u| u.status != FileStatus::Deleted);
        let worktree_dirty = s.is_some() || u.is_some();
        let index_has = index.as_ref().is_some_and(|index| {
            index
                .entry_by_path(gix::bstr::BStr::new(path.as_bytes()))
                .is_some()
        });

        // Resolve the status and the diff sides. The committed kind wins when
        // present (a worktree edit on a file committed as `A` stays `A`), except
        // where the committed record would hide content that is actually
        // present: a committed deletion whose path was re-added/recreated, or a
        // committed addition whose file was removed from the worktree. A path in
        // the dirty set but not committed compares HEAD against the worktree; a
        // committed path compares base against the worktree when dirty, else
        // entirely from the object database.
        let resolved = if let Some(c) = c {
            match c.status {
                FileStatus::Deleted if index_has || worktree_present => {
                    // Re-added to the index or recreated in the worktree: diff
                    // the base content against what is present now.
                    if let Some(s) = s {
                        let use_worktree = worktree_present;
                        ResolvedChange {
                            status: s.status,
                            old_id: c.old_id,
                            odb_new: if use_worktree { None } else { s.new_id },
                            new_hint: s.new_id,
                            old_path: s.old_path,
                            rename_percent: None,
                            mode_old: s.mode_old.or(c.mode_old),
                            mode_new: s.mode_new,
                            use_worktree,
                            force_zero: false,
                        }
                    } else if let Some(u) = u {
                        ResolvedChange {
                            status: u.status,
                            old_id: c.old_id,
                            odb_new: None,
                            new_hint: None,
                            old_path: u.old_path,
                            rename_percent: u.rename_percent,
                            mode_old: u.mode_old.or(c.mode_old),
                            mode_new: u.mode_new,
                            use_worktree: true,
                            force_zero: u.conflicted,
                        }
                    } else {
                        ResolvedChange {
                            status: FileStatus::Added,
                            old_id: c.old_id,
                            odb_new: None,
                            new_hint: None,
                            old_path: None,
                            rename_percent: None,
                            mode_old: c.mode_old,
                            mode_new: worktree_mode(repo, &path),
                            use_worktree: true,
                            force_zero: false,
                        }
                    }
                }
                FileStatus::Deleted => ResolvedChange {
                    status: c.status,
                    old_id: c.old_id,
                    odb_new: None,
                    new_hint: c.new_id,
                    old_path: c.old_path,
                    rename_percent: c.rename_percent,
                    mode_old: c.mode_old,
                    mode_new: c.mode_new,
                    use_worktree: false,
                    force_zero: false,
                },
                // Added in HEAD but removed from the worktree: the net
                // base→worktree change is empty, so report the worktree
                // deletion rather than a phantom addition with zero stats.
                FileStatus::Added
                    if u.as_ref().is_some_and(|u| u.status == FileStatus::Deleted) =>
                {
                    ResolvedChange {
                        status: FileStatus::Deleted,
                        old_id: c.old_id,
                        odb_new: None,
                        new_hint: c.new_id,
                        old_path: c.old_path,
                        rename_percent: c.rename_percent,
                        mode_old: c.mode_old,
                        mode_new: c.mode_new,
                        use_worktree: false,
                        force_zero: true,
                    }
                }
                _ => {
                    let use_worktree = worktree_dirty;
                    ResolvedChange {
                        status: c.status,
                        old_id: c.old_id,
                        odb_new: if use_worktree { None } else { c.new_id },
                        new_hint: c.new_id,
                        old_path: c.old_path,
                        rename_percent: c.rename_percent,
                        mode_old: c.mode_old,
                        mode_new: c.mode_new,
                        use_worktree,
                        force_zero: false,
                    }
                }
            }
        } else if let Some(s) = s {
            if s.status == FileStatus::Deleted {
                if is_untracked {
                    // `git rm --cached`: the file is still on disk and shows as
                    // untracked; keep one row as the added content the user
                    // actually sees instead of a deletion that would read the
                    // worktree.
                    ResolvedChange {
                        status: FileStatus::Added,
                        old_id: s.old_id,
                        odb_new: if worktree_present { None } else { s.new_id },
                        new_hint: s.new_id,
                        old_path: s.old_path,
                        rename_percent: s.rename_percent,
                        mode_old: s.mode_old,
                        mode_new: worktree_mode(repo, &path),
                        use_worktree: worktree_present,
                        force_zero: false,
                    }
                } else {
                    // A real staged deletion: diff entirely from the object
                    // database, with no worktree read.
                    ResolvedChange {
                        status: s.status,
                        old_id: s.old_id,
                        odb_new: None,
                        new_hint: s.new_id,
                        old_path: s.old_path,
                        rename_percent: s.rename_percent,
                        mode_old: s.mode_old,
                        mode_new: s.mode_new,
                        use_worktree: false,
                        force_zero: false,
                    }
                }
            } else {
                ResolvedChange {
                    status: s.status,
                    old_id: s.old_id,
                    odb_new: None,
                    new_hint: s.new_id,
                    old_path: s.old_path,
                    rename_percent: s.rename_percent,
                    mode_old: s.mode_old,
                    mode_new: s.mode_new,
                    use_worktree: true,
                    force_zero: false,
                }
            }
        } else if let Some(u) = u {
            ResolvedChange {
                status: u.status,
                old_id: u.old_id,
                odb_new: None,
                new_hint: None,
                old_path: u.old_path,
                rename_percent: u.rename_percent,
                mode_old: u.mode_old,
                mode_new: u.mode_new,
                use_worktree: true,
                force_zero: u.conflicted,
            }
        } else if is_untracked {
            ResolvedChange {
                status: FileStatus::Added,
                old_id: None,
                odb_new: None,
                new_hint: None,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: worktree_mode(repo, &path),
                use_worktree: true,
                force_zero: false,
            }
        } else {
            // The path list is the union of the four change sets, and the arms
            // above consult each of them in turn, so a path matching none of
            // them cannot exist.
            unreachable!("path {path} is in the change union but matched no change set")
        };

        // When a single file's content cannot be read (permissions, broken
        // state, ...), keep the entry with unknown stats so the tree still
        // loads; the per-file diff surfaces the error when the file is selected.
        let counted = if resolved.force_zero {
            Ok((0, 0, false))
        } else if resolved.use_worktree {
            count_worktree_changes(
                &mut wt_cache,
                &mut lfs,
                repo,
                (resolved.old_id, resolved.mode_old),
                resolved.new_hint,
                &path,
            )
        } else {
            count_odb_changes(
                &mut odb_cache,
                &mut lfs,
                repo,
                (resolved.old_id, resolved.mode_old),
                (resolved.odb_new, resolved.mode_new),
                &path,
            )
        };
        let (added, deleted, binary) = counted.unwrap_or((0, 0, false));
        wt_cache.clear_resource_cache_keep_allocation();
        odb_cache.clear_resource_cache_keep_allocation();
        changes.push(ChangeInfo {
            status: resolved.status,
            path,
            old_path: resolved.old_path,
            rename_percent: resolved.rename_percent,
            mode_old: resolved.mode_old,
            mode_new: resolved.mode_new,
            added,
            deleted,
            binary,
        });
    }
    Ok(changes)
}

/// The committed set: changes needed to turn the base tree into HEAD's tree,
/// read entirely from the object database. Directory entries are skipped (bough
/// treats every change as a file), and rename tracking mirrors
/// [`unstaged_changes`]'s `Rewrites::default()`.
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
                let status = if type_changed(
                    u32::from(previous_entry_mode.value()),
                    u32::from(entry_mode.value()),
                ) {
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
                map.insert(
                    location.to_string(),
                    CommittedChange {
                        status: rewrite_status(copy),
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

/// The staged set: changes between HEAD's tree and the on-disk index. When the
/// comparison stops early (e.g. path validation rejects a tracked name), the
/// entries already produced are kept — a failure degrades per-path rather than
/// blanking the whole listing.
pub(crate) fn staged_changes(
    repo: &gix::Repository,
    head_tree: &gix::ObjectId,
) -> HashMap<String, StagedChange> {
    let mut map = HashMap::new();
    let Ok(index) = repo.index_or_empty() else {
        return map;
    };
    // Built on the first staged rewrite: computing a rewrite's similarity
    // needs a blob-diff platform over the object database.
    let mut odb_cache: Option<gix::diff::blob::Platform> = None;
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
                        rename_percent: None,
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
                        rename_percent: None,
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
                        rename_percent: None,
                        old_path: None,
                        mode_old: Some(previous_entry_mode.bits()),
                        mode_new: Some(entry_mode.bits()),
                    },
                );
            }
            diff::index::Change::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                entry_mode,
                location,
                id,
                copy,
                ..
            } => {
                // The tree-index rewrite callback carries no similarity stats,
                // so the percentage is computed from the two blobs — the same
                // measure gix's rewrite tracker applies internally.
                let (source_id, id) = (source_id.into_owned(), id.into_owned());
                let rename_percent = staged_rename_percent(
                    repo,
                    &mut odb_cache,
                    (source_id, source_entry_mode),
                    (id, entry_mode),
                    &location.to_string(),
                );
                map.insert(
                    location.to_string(),
                    StagedChange {
                        status: rewrite_status(copy),
                        old_id: Some(source_id),
                        new_id: Some(id),
                        old_path: Some(source_location.to_string()),
                        rename_percent,
                        mode_old: Some(source_entry_mode.bits()),
                        mode_new: Some(entry_mode.bits()),
                    },
                );
            }
        }
        Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
    });
    // Keep whatever was collected before an early stop: one rejected path must
    // not discard the staged changes already found.
    let _ = result;
    map
}

/// Whether two git modes crossed entry types (file ↔ symlink ↔ submodule),
/// which reads as a type change rather than a content modification.
fn type_changed(previous_bits: u32, current_bits: u32) -> bool {
    (previous_bits & 0o170000) != (current_bits & 0o170000)
}

/// [`type_changed`] for index entry modes.
fn index_type_changed(previous: gix::index::entry::Mode, current: gix::index::entry::Mode) -> bool {
    type_changed(previous.bits(), current.bits())
}

/// A rewrite's status: a copy keeps the source (`C`), everything else is a
/// rename (`R`).
fn rewrite_status(copy: bool) -> FileStatus {
    if copy {
        FileStatus::Copied
    } else {
        FileStatus::Renamed
    }
}

/// A staged rewrite's similarity as a whole-number percentage (0–100), or
/// `None` when it cannot be computed (non-blob or binary content) or when the
/// two sides are identical: gix's rewrite tracker pairs exact matches without
/// a similarity check and reports no stats for them, so staged renames agree
/// with committed and unstaged ones. The tree-index rewrite callback carries
/// no stats of its own, so the measure is taken with the same byte-level blob
/// diff gix's rewrite tracker uses internally.
fn staged_rename_percent(
    repo: &gix::Repository,
    cache: &mut Option<gix::diff::blob::Platform>,
    old: (gix::ObjectId, gix::index::entry::Mode),
    new: (gix::ObjectId, gix::index::entry::Mode),
    path: &str,
) -> Option<u8> {
    let (old_id, old_mode) = old;
    let (new_id, new_mode) = new;
    if old_id == new_id {
        return None;
    }
    let old_kind = entry_kind_for(old_mode.bits().into());
    let new_kind = entry_kind_for(new_mode.bits().into());
    // Symlinks and submodules have no meaningful blob similarity.
    if !matches!(
        old_kind,
        gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable
    ) || !matches!(
        new_kind,
        gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable
    ) {
        return None;
    }
    if cache.is_none() {
        *cache = odb_resource_cache(repo).ok();
    }
    let cache = cache.as_mut()?;
    set_odb_resources(
        cache,
        repo,
        Some(old_id),
        old_kind,
        Some(new_id),
        new_kind,
        path,
    )
    .ok()?;
    let outcome = cache.prepare_diff().ok()?;
    let algorithm = match outcome.operation {
        Operation::InternalDiff { algorithm } => algorithm,
        // Binary or driver-diffed content has no line-based similarity.
        _ => return None,
    };
    let input =
        diff::blob::InternedInput::new(outcome.old.intern_source(), outcome.new.intern_source());
    let blob_diff = diff::blob::Diff::compute(algorithm, &input);
    let old_len = outcome.old.data.as_slice().unwrap_or_default().len();
    let new_len = outcome.new.data.as_slice().unwrap_or_default().len();
    let similarity =
        (old_len - removed_line_bytes(&blob_diff, &input)) as f32 / old_len.max(new_len) as f32;
    Some((similarity * 100.0).round().clamp(0.0, 100.0) as u8)
}

/// The byte length of all removed lines in `blob_diff`. Mirrors gix's private
/// `rewrites::tracker::diff::removed_bytes`, which defines the similarity
/// measure for rewrites.
fn removed_line_bytes(
    blob_diff: &diff::blob::Diff,
    input: &diff::blob::InternedInput<&[u8]>,
) -> usize {
    blob_diff
        .hunks()
        .map(|hunk| {
            input.before[hunk.before.start as usize..hunk.before.end as usize]
                .iter()
                .map(|token| input.interner[*token].len())
                .sum::<usize>()
        })
        .sum()
}

/// The unstaged set (index → worktree, using the on-disk index so unchanged
/// files stat-skip and are never hashed) plus the untracked files. One failed
/// item is skipped rather than blanking the scan.
pub(crate) fn unstaged_changes(
    repo: &gix::Repository,
) -> Result<(HashMap<String, UnstagedChange>, Vec<String>), GitError> {
    let platform =
        untracked_files_platform(repo)?.index_worktree_rewrites(Some(diff::Rewrites::default()));
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
                        mode_new: None,
                        conflicted: false,
                    },
                );
            }
            (IWItem::Modification { entry, .. }, Summary::TypeChange) => {
                let mode_new = worktree_mode(repo, &path);
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::TypeChanged,
                        old_id: Some(entry.id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(entry.mode.bits()),
                        mode_new,
                        conflicted: false,
                    },
                );
            }
            (IWItem::Modification { entry, .. }, Summary::Modified) => {
                let mode_new = worktree_mode(repo, &path);
                map.insert(
                    path,
                    UnstagedChange {
                        status: FileStatus::Modified,
                        old_id: Some(entry.id),
                        old_path: None,
                        rename_percent: None,
                        mode_old: Some(entry.mode.bits()),
                        mode_new,
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
    if let Some((base, head)) = split_range(revision) {
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

/// One side of a blob diff: its object id (absent when empty) and git mode.
type DiffSide = (Option<gix::ObjectId>, Option<u32>);

/// Count added/deleted lines and detect binary by diffing the blob on `old`
/// (or the empty blob when its id is absent) against the worktree content at
/// `path`. This is the only scan path that reads worktree content, so it may
/// run content filters (e.g. `git lfs clean`). `new_hint` is the new-side blob
/// when known from the object database, used solely to classify LFS without a
/// worktree read.
fn count_worktree_changes(
    cache: &mut diff::blob::Platform,
    lfs: &mut LfsMatcher<'_>,
    repo: &gix::Repository,
    old: DiffSide,
    new_hint: Option<gix::ObjectId>,
    path: &str,
) -> Result<(usize, usize, bool), GitError> {
    let (old_id, mode_old) = old;
    if lfs::detect(repo, lfs, old_id, new_hint, path).is_some() {
        return Ok((0, 0, true));
    }
    let old_kind = entry_kind_for(mode_old);
    let Some(new_kind) = worktree_entry_kind(repo, path) else {
        return Ok((0, 0, true));
    };
    // A submodule is a commit, not a blob; never read it as content.
    if old_kind == gix::object::tree::EntryKind::Commit
        || new_kind == gix::object::tree::EntryKind::Commit
    {
        return Ok((0, 0, true));
    }
    set_blob_resources(cache, repo, old_id, old_kind, path, new_kind)?;
    finish_count(cache, path)
}

/// Count added/deleted lines and detect binary for an object-database-only diff
/// of the blobs on `old` and `new` (each the empty blob when its id is absent).
/// No worktree content is read and no content filter runs. LFS pointers and
/// submodules are reported as `(0, 0, binary)`.
fn count_odb_changes(
    cache: &mut diff::blob::Platform,
    lfs: &mut LfsMatcher<'_>,
    repo: &gix::Repository,
    old: DiffSide,
    new: DiffSide,
    path: &str,
) -> Result<(usize, usize, bool), GitError> {
    let (old_id, mode_old) = old;
    let (new_id, mode_new) = new;
    if lfs::detect(repo, lfs, old_id, new_id, path).is_some() {
        return Ok((0, 0, true));
    }
    let old_kind = entry_kind_for(mode_old);
    let new_kind = entry_kind_for(mode_new);
    if old_kind == gix::object::tree::EntryKind::Commit
        || new_kind == gix::object::tree::EntryKind::Commit
    {
        return Ok((0, 0, true));
    }
    set_odb_resources(cache, repo, old_id, old_kind, new_id, new_kind, path)?;
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

/// The git mode bits of a worktree path's destination side: the worktree's real
/// mode, not the index entry's, so a `chmod +x` is not reported as unchanged.
/// A non-regular or missing entry yields `None`.
fn worktree_mode(repo: &gix::Repository, path: &str) -> Option<u32> {
    worktree_entry_kind(repo, path).map(|kind| u32::from(kind as u16))
}

/// Count the lines (LF) in a regular file; 0 when it is missing, is not a
/// regular file (so symlinks, FIFOs and devices are never followed or read), or
/// cannot be read. At most [`MAX_COUNT_BYTES`] are read.
///
/// The cap is a *read* cap, not a line guarantee: a file larger than
/// [`MAX_COUNT_BYTES`] is counted only up to it. The count is display-only for
/// the initial-commit listing; binary detection is done elsewhere, so counting
/// `\n` bytes here is not a claim that the file is text.
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

        let changes = all_changes_in(&repo, "HEAD").unwrap();
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
        // An untracked file whose content cannot be read must still be listed,
        // with zero stats, rather than aborting the scan. The fixture is
        // verified actually unreadable; only a privileged runner (where 0o000
        // is bypassed) skips.
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        let repo = gix::discover(dir.path()).unwrap();

        let path = dir.path().join("new.txt");
        std::fs::write(&path, "hello\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            eprintln!(
                "skipping unreadable_worktree_file_does_not_abort_scan: could not make the fixture unreadable (privileged runner)"
            );
            return;
        }

        let changes = all_changes_in(&repo, "HEAD").unwrap();
        let by_path = by_path(changes);
        let entry = by_path
            .get("new.txt")
            .expect("an unreadable untracked file must still appear");
        assert_eq!((entry.added, entry.deleted), (0, 0));
    }

    #[test]
    fn windows_illegal_filename_does_not_abort_listing() {
        // A name with a `:` is legal on Linux but rejected by gix's default
        // (protect_windows) path validation. bough must still list changes,
        // including the illegal name itself.
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

        // Modify both so each has a change to report.
        std::fs::write(dir.path().join("a:b.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("plain.txt"), "one\ntwo\n").unwrap();

        let changes = all_changes_in(&repo, "HEAD").unwrap();
        let by_path = by_path(changes);
        assert!(by_path.contains_key("plain.txt"));
        assert_eq!(by_path["plain.txt"].status, FileStatus::Modified);
        assert!(
            by_path.contains_key("a:b.txt"),
            "the illegal filename must still be listed: {by_path:?}"
        );
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
        let changes = all_changes_in(&repo, "main...HEAD").unwrap();
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

        let changes = all_changes_in(&repo, "HEAD").unwrap();
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
        let changes = all_changes_in(&repo, "HEAD").unwrap();
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
        let changes = all_changes_in(&repo, "HEAD").unwrap();
        assert!(changes.is_empty(), "clean worktree: {changes:?}");
        assert!(
            !marker.exists(),
            "clean scan must not hash worktree content"
        );

        // Sanity: a real modification does compare content.
        std::fs::write(dir.path().join("file0.txt"), "two\n").unwrap();
        let changes = all_changes_in(&repo, "HEAD").unwrap();
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
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
        let changes = all_changes_in(&repo, "HEAD").unwrap();
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
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
        let renamed = &changes["new.txt"];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
    }

    /// `main` deletes `file0.txt` on `feature`; callers then re-add it. Returns
    /// the repo on `feature` with the deletion committed.
    fn repo_with_committed_deletion(dir: &Path) {
        crate::git::test_util::run_git(dir, &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir, &["config", "user.name", "T"]);
        std::fs::write(dir.join("file0.txt"), "one\n").unwrap();
        crate::git::test_util::run_git(dir, &["add", "."]);
        crate::git::test_util::run_git(dir, &["commit", "-m", "base"]);
        crate::git::test_util::run_git(dir, &["branch", "-m", "main"]);
        crate::git::test_util::run_git(dir, &["checkout", "-b", "feature"]);
        crate::git::test_util::run_git(dir, &["rm", "file0.txt"]);
        crate::git::test_util::run_git(dir, &["commit", "-m", "delete"]);
    }

    #[test]
    fn committed_deletion_readded_to_index_reports_present_content() {
        // A committed deletion must not win when the path was re-added to the
        // index: diff base content against the present content instead.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        repo_with_committed_deletion(dir.path());
        std::fs::write(dir.path().join("file0.txt"), "one\ntwo\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "file0.txt"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!((f.added, f.deleted), (1, 0));
    }

    #[test]
    fn committed_deletion_recreated_in_worktree_reports_present_content() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        repo_with_committed_deletion(dir.path());
        // Recreated but not staged: it shows up as untracked.
        std::fs::write(dir.path().join("file0.txt"), "one\ntwo\n").unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!((f.added, f.deleted), (1, 0));
    }

    #[test]
    fn committed_addition_removed_from_worktree_reports_deleted() {
        // Net base→worktree is empty; report the worktree deletion rather than a
        // phantom addition with zero stats.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "base"]);
        crate::git::test_util::run_git(dir.path(), &["branch", "-m", "main"]);
        crate::git::test_util::run_git(dir.path(), &["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "new.txt"]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "add"]);
        std::fs::remove_file(dir.path().join("new.txt")).unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "main...HEAD").unwrap());
        let f = &changes["new.txt"];
        assert_eq!(f.status, FileStatus::Deleted);
        assert_eq!((f.added, f.deleted), (0, 0));
    }

    #[test]
    fn rm_cached_file_reports_added_with_content() {
        // `git rm --cached` leaves the file on disk; the single row should show
        // the added content the user sees, not a deletion.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        std::fs::write(dir.path().join("file0.txt"), "one\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "base"]);
        crate::git::test_util::run_git(dir.path(), &["rm", "--cached", "file0.txt"]);
        std::fs::write(dir.path().join("file0.txt"), "one\ntwo\n").unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!((f.added, f.deleted), (1, 0));
    }

    #[cfg(unix)]
    #[test]
    fn unstaged_mode_change_reports_worktree_mode() {
        // chmod +x is a worktree-only change; the destination mode must come
        // from the worktree, otherwise both sides read as 100644.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        std::fs::set_permissions(
            dir.path().join("file0.txt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "HEAD").unwrap());
        let f = &changes["file0.txt"];
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.mode_old.unwrap() & 0o111, 0);
        assert_ne!(
            f.mode_new.expect("worktree mode") & 0o111,
            0,
            "the executable bit must be visible in the destination mode"
        );
    }

    #[test]
    fn staged_rename_carries_both_modes() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        crate::git::test_util::run_git(dir.path(), &["mv", "file0.txt", "renamed.txt"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "HEAD").unwrap());
        let f = &changes["renamed.txt"];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.mode_old, Some(0o100644));
        assert_eq!(f.mode_new, Some(0o100644));
    }

    #[test]
    fn staged_rename_reports_similarity() {
        // A staged rename whose content also changed must carry the similarity
        // percentage like committed and unstaged renames do (the tree-index
        // rewrite callback itself carries no stats, so it is recomputed from
        // the two blobs).
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("old.txt"), body).unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "base"]);

        crate::git::test_util::run_git(dir.path(), &["mv", "old.txt", "new.txt"]);
        let mut body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        body.push_str("extra\n");
        std::fs::write(dir.path().join("new.txt"), body).unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "HEAD").unwrap());
        let f = &changes["new.txt"];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("old.txt"));
        // All 10 original lines survive; only "extra" is added: 71 of 77
        // bytes → 92% by gix's byte-level similarity measure.
        assert_eq!(f.rename_percent, Some(92));
    }

    #[test]
    fn staged_pure_rename_has_no_similarity_like_the_other_scans() {
        // gix's rewrite tracker pairs identical blobs without a similarity
        // check and reports no stats; committed and unstaged renames show no
        // percentage for a pure rename, so the staged scan must not either.
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one"]);
        crate::git::test_util::run_git(dir.path(), &["mv", "file0.txt", "renamed.txt"]);

        let repo = gix::discover(dir.path()).unwrap();
        let changes = by_path(all_changes_in(&repo, "HEAD").unwrap());
        assert_eq!(changes["renamed.txt"].rename_percent, None);
    }
}
