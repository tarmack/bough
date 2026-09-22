//! Per-file structured diff construction.
//!
//! Builds a [`Diff`] directly from gix's blob diff — no unified-diff text is
//! produced. The `@@` hunk structure is walked from
//! [`gix::diff::blob::Diff`] via the [`ConsumeHunk`] machinery, and body lines
//! are stuffed straight into the renderer's data model.

use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, DiffLineKind, HunkHeader};
use gix::object::tree::EntryKind;

use crate::diff::{
    Diff, DiffLine, FileHeader, FileStatus, HunkMeta, LfsInfo, LineKind, Section, SectionKind,
};
use crate::git::lfs::{self, LfsMatcher};
use crate::git::{base_tree_id, open_repo, FileMeta, GitError};

/// Build the structured diff for `path` against `revision` with `ctx` context
/// lines. The base is resolved the same way as the changed-file listing — the
/// merge-base tree for a three-dot range (`main...HEAD`), else the revision's
/// own tree — so per-file diffs stay consistent with the tree.
pub fn diff_file(revision: &str, path: &str, meta: &FileMeta, ctx: u32) -> Result<Diff, GitError> {
    let repo = open_repo()?;
    diff_file_in(&repo, revision, path, meta, ctx)
}

/// Per-operation context for building several file diffs against one revision:
/// the repository, its resolved base tree, one diff resource platform, and one
/// LFS attribute matcher — all reused across children instead of rebuilt per
/// file.
pub(crate) struct DiffContext<'repo> {
    repo: &'repo gix::Repository,
    base_tree: Option<gix::ObjectId>,
    head_tree: Option<gix::ObjectId>,
    platform: gix::diff::blob::Platform,
    lfs: LfsMatcher<'repo>,
}

impl<'repo> DiffContext<'repo> {
    /// Resolve the revision once and build the shared platform and matcher.
    pub(crate) fn new(repo: &'repo gix::Repository, revision: &str) -> Result<Self, GitError> {
        Ok(DiffContext {
            repo,
            base_tree: resolve_base_tree(repo, revision)?,
            head_tree: resolve_head_tree(repo)?,
            platform: diff_resource_cache(repo)?,
            lfs: LfsMatcher::new(repo),
        })
    }

    /// Build the structured diff for `path`, reusing the shared context.
    pub(crate) fn diff_file(
        &mut self,
        path: &str,
        meta: &FileMeta,
        ctx: u32,
    ) -> Result<Diff, GitError> {
        // A rename/copy's new path does not exist in the base tree, so the old
        // side must be resolved at the source path instead.
        let old_path = match meta.status {
            FileStatus::Renamed | FileStatus::Copied => meta.old_path.as_deref().unwrap_or(path),
            _ => path,
        };
        let old_id = blob_id_in_tree(self.repo, self.base_tree, old_path)?;
        // The HEAD blob is the new side's content in the object database. It is
        // passed to LFS detection so a file committed as a pointer is stubbed
        // even though the diff's new side is read from the worktree (where an
        // LFS file holds the smudged real content).
        let new_id = blob_id_in_tree(self.repo, self.head_tree, path)?;
        let info = lfs::detect(self.repo, &mut self.lfs, old_id, new_id, path);
        let diff =
            build_file_diff_with(&mut self.platform, self.repo, old_id, path, meta, ctx, info)?;
        self.platform.clear_resource_cache_keep_allocation();
        Ok(diff)
    }
}

/// Build the structured diff for `path` against `revision` against an already
/// opened repository. The base is resolved the same way as the changed-file
/// listing — the merge-base tree for a three-dot range (`main...HEAD`), else
/// the revision's own tree — so per-file diffs stay consistent with the tree.
pub(crate) fn diff_file_in(
    repo: &gix::Repository,
    revision: &str,
    path: &str,
    meta: &FileMeta,
    ctx: u32,
) -> Result<Diff, GitError> {
    DiffContext::new(repo, revision)?.diff_file(path, meta, ctx)
}

/// A blob-diff resource platform rooted at the repository's working tree. The
/// source side always resolves from the object database by id; only the
/// destination side is read from the working tree.
pub(crate) fn diff_resource_cache(
    repo: &gix::Repository,
) -> Result<gix::diff::blob::Platform, GitError> {
    let roots = repo
        .workdir()
        .map(|p| gix::diff::blob::pipeline::WorktreeRoots {
            old_root: None,
            new_root: Some(p.to_path_buf()),
        })
        .unwrap_or_default();
    repo.diff_resource_cache(gix::diff::blob::pipeline::Mode::ToGit, roots)
        .map_err(|e| GitError::message(e.to_string()))
}

/// A blob-diff resource platform whose two sides both resolve from the object
/// database by id. No worktree root is set, so `set_resource` never reads or
/// filters worktree content: an ODB-only diff spawns no filters and touches no
/// files.
pub(crate) fn odb_resource_cache(
    repo: &gix::Repository,
) -> Result<gix::diff::blob::Platform, GitError> {
    repo.diff_resource_cache_for_tree_diff()
        .map_err(|e| GitError::message(e.to_string()))
}

/// The tree to diff against, or `None` for an unborn HEAD (no base tree, so
/// every line is an addition).
fn resolve_base_tree(
    repo: &gix::Repository,
    revision: &str,
) -> Result<Option<gix::ObjectId>, GitError> {
    if repo.head().is_ok_and(|h| h.is_unborn()) {
        return Ok(None);
    }
    Ok(Some(base_tree_id(repo, revision)?))
}

/// HEAD's tree, or `None` for an unborn HEAD. Used to give LFS detection the
/// new-side blob without reading the worktree. An unborn HEAD is expected to
/// have no tree; any other failure is a real error and is propagated.
fn resolve_head_tree(repo: &gix::Repository) -> Result<Option<gix::ObjectId>, GitError> {
    match repo.head_tree_id() {
        Ok(id) => Ok(Some(id.detach())),
        Err(_) if repo.head().is_ok_and(|h| h.is_unborn()) => Ok(None),
        Err(e) => Err(GitError::message(format!("cannot resolve HEAD tree: {e}"))),
    }
}

/// The blob id of `path` in `tree`, or `None` when the tree is absent or the
/// file is new there.
fn blob_id_in_tree(
    repo: &gix::Repository,
    tree: Option<gix::ObjectId>,
    path: &str,
) -> Result<Option<gix::ObjectId>, GitError> {
    let Some(tree) = tree else {
        return Ok(None);
    };
    let tree = repo
        .find_tree(tree)
        .map_err(|e| GitError::message(format!("cannot open base tree: {e}")))?;
    Ok(tree
        .lookup_entry_by_path(path)
        .map_err(|e| GitError::message(format!("cannot resolve base:{path}: {e}")))?
        .map(|entry| entry.object_id()))
}

/// A single-file diff body assembled from gix hunks: the hunk sections, body
/// lines (hunk headers + classified content), and per-hunk counts.
struct HunkCollector {
    sections: Vec<Section>,
    lines: Vec<DiffLine>,
    hunks: Vec<HunkMeta>,
}

impl ConsumeHunk for HunkCollector {
    type Out = (Vec<Section>, Vec<DiffLine>, Vec<HunkMeta>);

    fn consume_hunk(
        &mut self,
        _header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        let sec = self.sections.len();
        let hunk = self.hunks.len();
        self.sections.push(Section {
            kind: SectionKind::Hunk,
            file: 0,
            hunk: Some(hunk),
        });
        self.lines.push(DiffLine {
            kind: LineKind::HunkHeader,
            raw_text: String::new(),
            section: sec,
            word_runs: None,
        });

        let mut added = 0usize;
        let mut deleted = 0usize;
        for (kind, content) in lines {
            let kind = match kind {
                DiffLineKind::Context => LineKind::Context,
                DiffLineKind::Add => {
                    added += 1;
                    LineKind::Added
                }
                DiffLineKind::Remove => {
                    deleted += 1;
                    LineKind::Removed
                }
            };
            self.lines.push(DiffLine {
                kind,
                raw_text: String::from_utf8_lossy(content).into_owned(),
                section: sec,
                word_runs: None,
            });
        }
        self.hunks.push(HunkMeta { added, deleted });
        Ok(())
    }

    fn finish(self) -> (Vec<Section>, Vec<DiffLine>, Vec<HunkMeta>) {
        (self.sections, self.lines, self.hunks)
    }
}

/// The gix entry kind for a git file mode, or [`EntryKind::Blob`] when the
/// mode is absent. Lets callers pass the real kind (link, submodule) instead of
/// assuming a content-read is safe.
pub(crate) fn entry_kind_for(mode: Option<u32>) -> EntryKind {
    match mode.map(|m| m & 0o170000) {
        Some(0o120000) => EntryKind::Link,
        Some(0o160000) => EntryKind::Commit,
        Some(0o100000) => {
            if mode.is_some_and(|m| m & 0o111 != 0) {
                EntryKind::BlobExecutable
            } else {
                EntryKind::Blob
            }
        }
        _ => EntryKind::Blob,
    }
}

/// The entry kind of a worktree path, or `None` when it must not be opened as
/// content: symlinks are read via the link itself (never followed), regular
/// files are content-read, a missing path degrades to the empty file, and every
/// other kind (FIFO, device, directory, socket) is refused so a hung or
/// unbounded read is impossible.
pub(crate) fn worktree_entry_kind(repo: &gix::Repository, path: &str) -> Option<EntryKind> {
    let full = repo
        .workdir()
        .map(|root| root.join(path))
        .unwrap_or_else(|| std::path::PathBuf::from(path));
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        // Missing as identified by the failed stat: gix handles the missing
        // destination as the empty side, so a deletion still counts.
        Err(_) => return Some(EntryKind::Blob),
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Some(EntryKind::Link);
    }
    if file_type.is_file() {
        return Some(worktree_blob_kind(&meta));
    }
    None
}

/// Blob vs executable from a regular file's metadata.
fn worktree_blob_kind(meta: &std::fs::Metadata) -> EntryKind {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return EntryKind::BlobExecutable;
        }
    }
    EntryKind::Blob
}

/// Load the blob at `old_id` (or the empty blob when `None`) and the worktree
/// content at `path` into `cache` as the source/destination resources for a
/// blob diff. Shared by the per-file diff construction and the added/removed
/// line counting in the status scan.
pub(crate) fn set_blob_resources(
    cache: &mut gix::diff::blob::Platform,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    old_kind: EntryKind,
    path: &str,
    new_kind: EntryKind,
) -> Result<(), GitError> {
    let rela: &gix::bstr::BStr = path.into();
    let null = gix::ObjectId::null(repo.object_hash());

    cache
        .set_resource(
            old_id.unwrap_or(null),
            old_kind,
            rela,
            gix::diff::blob::ResourceKind::OldOrSource,
            &repo.objects,
        )
        .map_err(|e| GitError::message(format!("diff {path}: {e}")))?;
    cache
        .set_resource(
            null,
            new_kind,
            rela,
            gix::diff::blob::ResourceKind::NewOrDestination,
            &repo.objects,
        )
        .map_err(|e| GitError::message(format!("diff {path}: {e}")))?;
    Ok(())
}

/// Load both blobs from the object database into `cache` (built by
/// [`odb_resource_cache`]) as the source/destination resources for a diff. No
/// worktree content is read, so no content filter ever runs.
pub(crate) fn set_odb_resources(
    cache: &mut gix::diff::blob::Platform,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    old_kind: EntryKind,
    new_id: Option<gix::ObjectId>,
    new_kind: EntryKind,
    path: &str,
) -> Result<(), GitError> {
    let rela: &gix::bstr::BStr = path.into();
    let null = gix::ObjectId::null(repo.object_hash());
    for (id, kind, res_kind) in [
        (old_id, old_kind, gix::diff::blob::ResourceKind::OldOrSource),
        (
            new_id,
            new_kind,
            gix::diff::blob::ResourceKind::NewOrDestination,
        ),
    ] {
        cache
            .set_resource(id.unwrap_or(null), kind, rela, res_kind, &repo.objects)
            .map_err(|e| GitError::message(format!("diff {path}: {e}")))?;
    }
    Ok(())
}

/// Build a single-file [`Diff`] comparing the blob at `old_id` (or empty
/// when `None`) against the worktree content at `path`, with a fresh
/// platform and LFS matcher.
#[cfg(test)]
fn build_file_diff(
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    path: &str,
    meta: &FileMeta,
    ctx: u32,
) -> Result<Diff, GitError> {
    let mut cache = diff_resource_cache(repo)?;
    let mut lfs = LfsMatcher::new(repo);
    let info = lfs::detect(repo, &mut lfs, old_id, None, path);
    build_file_diff_with(&mut cache, repo, old_id, path, meta, ctx, info)
}

/// A zero-line binary stub for a path whose content must not be read (a
/// submodule, symlink-only target, FIFO, device, ...). Mirrors the LFS stub:
/// correct header, no hunks, no body.
fn stub_diff(path: &str, meta: &FileMeta) -> Diff {
    let file_header = FileHeader {
        path: path.to_string(),
        old_path: meta.old_path.clone(),
        status: meta.status,
        added: 0,
        deleted: 0,
        rename_percent: meta.rename_percent,
        mode_old: meta.mode_old,
        mode_new: meta.mode_new,
        binary: true,
        lfs: None,
        hunks: Vec::new(),
    };
    Diff {
        header: file_header.clone(),
        files: vec![file_header],
        sections: Vec::new(),
        lines: Vec::new(),
    }
}

/// Build a single-file [`Diff`] using the caller's diff platform. An LFS object
/// is stubbed with a zero-line binary header before any diff/filter work runs.
fn build_file_diff_with(
    cache: &mut gix::diff::blob::Platform,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    path: &str,
    meta: &FileMeta,
    ctx: u32,
    lfs: Option<LfsInfo>,
) -> Result<Diff, GitError> {
    if let Some(lfs) = lfs {
        let file_header = FileHeader {
            path: path.to_string(),
            old_path: meta.old_path.clone(),
            status: meta.status,
            added: 0,
            deleted: 0,
            rename_percent: meta.rename_percent,
            mode_old: meta.mode_old,
            mode_new: meta.mode_new,
            binary: true,
            lfs: Some(lfs),
            hunks: Vec::new(),
        };
        return Ok(Diff {
            header: file_header.clone(),
            files: vec![file_header],
            sections: Vec::new(),
            lines: Vec::new(),
        });
    }

    let old_kind = entry_kind_for(meta.mode_old);
    // A submodule's object is a commit, not a blob, and a symlink/FIFO/device
    // worktree entry must never be content-read: stub those as binary instead.
    let Some(new_kind) = worktree_entry_kind(repo, path) else {
        return Ok(stub_diff(path, meta));
    };
    if old_kind == EntryKind::Commit {
        return Ok(stub_diff(path, meta));
    }

    set_blob_resources(cache, repo, old_id, old_kind, path, new_kind)?;

    let (sections, mut lines, hunks) = match cache.prepare_diff() {
        Ok(outcome) => match outcome.operation {
            Operation::InternalDiff { algorithm } => {
                let input = outcome.interned_input();
                let diff = gix::diff::blob::Diff::compute(algorithm, &input);
                gix::diff::blob::UnifiedDiff::new(
                    &diff,
                    &input,
                    HunkCollector {
                        sections: Vec::new(),
                        lines: Vec::new(),
                        hunks: Vec::new(),
                    },
                    gix::diff::blob::unified_diff::ContextSize::symmetrical(ctx),
                )
                .consume()
                .map_err(|e| GitError::message(format!("diff {path}: {e}")))?
            }
            Operation::SourceOrDestinationIsBinary => (Vec::new(), Vec::new(), Vec::new()),
            // gix only selects an external diff command after ruling out binary
            // content, so the file is not binary. bough does not run the
            // external command; render no hunks.
            Operation::ExternalCommand { .. } => (Vec::new(), Vec::new(), Vec::new()),
        },
        Err(e) => return Err(GitError::message(format!("diff {path}: {e}"))),
    };

    crate::diff::apply_word_pairs(&mut lines);

    let file_header = FileHeader {
        path: path.to_string(),
        old_path: meta.old_path.clone(),
        status: meta.status,
        added: meta.added,
        deleted: meta.deleted,
        rename_percent: meta.rename_percent,
        mode_old: meta.mode_old,
        mode_new: meta.mode_new,
        binary: meta.binary,
        lfs: None,
        hunks,
    };

    Ok(Diff {
        header: file_header.clone(),
        files: vec![file_header],
        sections,
        lines,
    })
}

#[cfg(test)]
mod tests {
    use crate::diff::FileStatus;
    use crate::git::resolve_id;
    use crate::git::test_util::repo_with_commits;

    use super::*;

    fn modified_meta() -> FileMeta {
        FileMeta {
            status: FileStatus::Modified,
            added: 2,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
        }
    }

    #[test]
    fn modified_file_produces_structured_diff() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one\n"]);
        let repo = gix::discover(dir.path()).unwrap();
        std::fs::write(dir.path().join("file0.txt"), "one\ntwo\nthree\n").unwrap();

        let old_id = resolve_id(&repo, "HEAD:file0.txt").unwrap();
        let diff = build_file_diff(&repo, Some(old_id), "file0.txt", &modified_meta(), 3).unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.header.added, 2);
        assert_eq!(diff.header.deleted, 0);
        assert_eq!(diff.header.hunks.len(), 1);
        assert_eq!(diff.header.hunks[0].added, 2);
        assert_eq!(diff.header.hunks[0].deleted, 0);
        assert!(diff
            .lines
            .iter()
            .any(|l| l.raw_text == "two" && l.kind == LineKind::Added));
        assert!(diff
            .lines
            .iter()
            .any(|l| l.raw_text == "one" && l.kind == LineKind::Context));
    }

    #[test]
    fn untracked_diff_is_all_added() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one\n"]);
        let repo = gix::discover(dir.path()).unwrap();
        std::fs::write(dir.path().join("new.rs"), "fn main() {}\n").unwrap();

        let meta = FileMeta {
            status: FileStatus::Added,
            added: 1,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
        };
        let diff = build_file_diff(&repo, None, "new.rs", &meta, 3).unwrap();
        assert_eq!(diff.header.added, 1);
        assert_eq!(diff.header.deleted, 0);
        assert_eq!(diff.files.len(), 1);
    }

    #[test]
    fn diff_file_resolves_merge_base_for_three_dot_rev() {
        // A `main...HEAD` revision must diff each file against the merge-base
        // tree, not try to parse `main...HEAD:path` (which gix cannot resolve).
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

        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        run(&["branch", "-m", "main"]);
        run(&["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "feature"]);

        // `diff_file_in` resolves the base from the explicit repo.
        let repo = gix::discover(dir.path()).unwrap();
        let meta = FileMeta {
            status: FileStatus::Modified,
            added: 1,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
        };
        let diff = diff_file_in(&repo, "main...HEAD", "f.txt", &meta, 3).unwrap();
        assert_eq!(diff.header.added, 1);
        assert_eq!(diff.header.deleted, 0);
        assert!(diff
            .lines
            .iter()
            .any(|l| l.raw_text == "two" && l.kind == LineKind::Added));
    }

    #[test]
    fn diff_file_unborn_head_yields_empty_base() {
        // A fresh repo (unborn HEAD) has no base tree; `diff_file` must treat
        // it as an empty base and report every line as an addition rather than
        // failing to resolve the base tree.
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

        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();

        // `diff_file_in` treats an explicit unborn repo as an empty base.
        let repo = gix::discover(dir.path()).unwrap();
        let meta = FileMeta {
            status: FileStatus::Added,
            added: 3,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
        };
        let diff = diff_file_in(&repo, "HEAD", "f.txt", &meta, 3).unwrap();
        assert_eq!(diff.header.added, 3);
        assert_eq!(diff.header.deleted, 0);
        assert!(diff
            .lines
            .iter()
            .all(|l| l.kind.is_header() || l.kind == LineKind::Added));
    }

    #[test]
    fn lfs_pointer_file_is_stubbed() {
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
        std::fs::write(dir.path().join("big.bin"), pointer(12345678)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "add pointer"]);

        let repo = gix::discover(dir.path()).unwrap();
        let old_id = resolve_id(&repo, "HEAD:big.bin").unwrap();
        let diff = build_file_diff(&repo, Some(old_id), "big.bin", &modified_meta(), 3).unwrap();

        assert!(diff.header.binary);
        assert!(diff.lines.is_empty());
        assert!(diff.sections.is_empty());
        let info = diff.header.lfs.as_ref().expect("lfs metadata");
        assert_eq!(info.size, 12345678);
        assert_eq!(info.oid, OID);
    }

    #[test]
    fn converted_to_lfs_file_is_stubbed_with_new_pointer() {
        // A file converted from plain text (base) to an LFS pointer (HEAD):
        // the new-side blob decides, and its metadata is reported.
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
        run(&["commit", "-m", "text"]);
        run(&["branch", "-m", "main"]);
        run(&["checkout", "-b", "feature"]);

        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 555\n");
        std::fs::write(dir.path().join("big.bin"), pointer).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "pointer"]);

        let repo = gix::discover(dir.path()).unwrap();
        let diff = diff_file_in(&repo, "main...HEAD", "big.bin", &modified_meta(), 3).unwrap();
        assert!(diff.header.binary);
        assert!(diff.lines.is_empty());
        let info = diff.header.lfs.as_ref().expect("lfs metadata");
        assert_eq!(info.size, 555);
        assert_eq!(info.oid, OID);
    }

    #[cfg(unix)]
    #[test]
    fn worktree_symlink_is_not_followed() {
        // The worktree side must read the link itself, never its target: a
        // symlink to /dev/zero or /etc/shadow must not be read in full.
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one\n"]);
        let repo = gix::discover(dir.path()).unwrap();
        std::os::unix::fs::symlink("file0.txt", dir.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", dir.path().join("evil")).unwrap();

        let meta = FileMeta {
            status: FileStatus::Added,
            added: 1,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
        };
        let diff = build_file_diff(&repo, None, "link.txt", &meta, 3).unwrap();
        assert!(
            diff.lines
                .iter()
                .any(|l| l.raw_text == "file0.txt" && l.kind == LineKind::Added),
            "link target, not linked content: {:?}",
            diff.lines
        );
        assert!(!diff.lines.iter().any(|l| l.raw_text.contains("one")));

        // A link to a device is likewise just its target string.
        let evil = build_file_diff(&repo, None, "evil", &meta, 3).unwrap();
        assert!(evil
            .lines
            .iter()
            .any(|l| l.raw_text == "/dev/zero" && l.kind == LineKind::Added));
    }

    #[test]
    fn worktree_non_regular_entry_is_stubbed() {
        // A directory entry (standing in for a FIFO/device, which must never be
        // opened) yields an empty binary stub rather than a hang.
        let dir = tempfile::tempdir().unwrap();
        repo_with_commits(dir.path(), &["one\n"]);
        let repo = gix::discover(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let meta = FileMeta {
            status: FileStatus::Added,
            added: 0,
            deleted: 0,
            old_path: None,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
        };
        let diff = build_file_diff(&repo, None, "sub", &meta, 3).unwrap();
        assert!(diff.header.binary);
        assert!(diff.lines.is_empty());
        assert!(diff.sections.is_empty());
    }

    /// Create `main` with a 10-line `old.txt`, then a `feature` branch that
    /// renames it to `new.txt` with `extra` appended. Returns on `feature`.
    #[cfg(test)]
    fn repo_with_renamed_file(dir: &std::path::Path, append_extra: bool) {
        let run = |args: &[&str]| {
            let s = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .status()
                .unwrap();
            assert!(s.success());
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@e"]);
        run(&["config", "user.name", "T"]);
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("old.txt"), body).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        run(&["checkout", "-b", "feature"]);
        run(&["mv", "old.txt", "new.txt"]);
        if append_extra {
            let mut body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
            body.push_str("extra\n");
            std::fs::write(dir.join("new.txt"), body).unwrap();
        }
        run(&["add", "."]);
        run(&["commit", "-m", "rename"]);
    }

    fn renamed_meta(added: usize) -> FileMeta {
        FileMeta {
            status: FileStatus::Renamed,
            added,
            deleted: 0,
            old_path: Some("old.txt".to_string()),
            rename_percent: Some(90),
            mode_old: Some(0o100644),
            mode_new: Some(0o100644),
            binary: false,
        }
    }

    #[test]
    fn rename_with_modification_diffs_old_path() {
        // The old side lives at the *source* path in the base tree; resolving
        // it at the new path would make the body a pure addition.
        let dir = tempfile::tempdir().unwrap();
        repo_with_renamed_file(dir.path(), true);
        let repo = gix::discover(dir.path()).unwrap();

        let diff = diff_file_in(&repo, "main...HEAD", "new.txt", &renamed_meta(1), 3).unwrap();
        assert_eq!(diff.header.old_path.as_deref(), Some("old.txt"));
        assert!(
            diff.lines
                .iter()
                .any(|l| l.raw_text == "extra" && l.kind == LineKind::Added),
            "modified line must show as an addition: {:?}",
            diff.lines
        );
        assert!(
            !diff.lines.is_empty(),
            "a rename of modified content must not be an empty diff"
        );
    }

    #[test]
    fn pure_rename_has_empty_body_and_zero_stats() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_renamed_file(dir.path(), false);
        let repo = gix::discover(dir.path()).unwrap();

        let diff = diff_file_in(&repo, "main...HEAD", "new.txt", &renamed_meta(0), 3).unwrap();
        assert_eq!(diff.header.added, 0);
        assert_eq!(diff.header.deleted, 0);
        assert!(
            diff.lines.is_empty(),
            "a pure rename has no bogus additions: {:?}",
            diff.lines
        );
    }
}
