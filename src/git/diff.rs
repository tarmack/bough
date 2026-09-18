//! Per-file structured diff construction.
//!
//! Builds a [`Diff`] directly from gix's blob diff — no unified-diff text is
//! produced. The `@@` hunk structure is walked from
//! [`gix::diff::blob::Diff`] via the [`ConsumeHunk`] machinery, and body lines
//! are stuffed straight into the renderer's data model.

use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, DiffLineKind, HunkHeader};
use gix::object::tree::EntryKind;

use crate::diff::{Diff, DiffLine, FileHeader, HunkMeta, LineKind, Section, SectionKind};
use crate::git::{base_tree_id, open_repo, FileMeta, GitError};

/// Build the structured diff for `path` against `revision` with `ctx` context
/// lines. The base is resolved the same way as the changed-file listing — the
/// merge-base tree for a three-dot range (`main...HEAD`), else the revision's
/// own tree — so per-file diffs stay consistent with the tree.
pub fn diff_file(revision: &str, path: &str, meta: &FileMeta, ctx: u32) -> Result<Diff, GitError> {
    let repo = open_repo()?;
    diff_file_in(&repo, revision, path, meta, ctx)
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
    // With an unborn HEAD (no commits yet) there is no base tree to resolve the
    // file against, so treat it as an empty base — every line an addition.
    let old_id = if repo.head().is_ok_and(|h| h.is_unborn()) {
        None
    } else {
        let base_tree = base_tree_id(repo, revision)?;
        let tree = repo
            .find_tree(base_tree)
            .map_err(|e| GitError::message(format!("cannot open base tree for {revision}: {e}")))?;
        // The blob for `path` in the base tree; `None` when the file is new there
        // (added on the branch), in which case every line is an addition.
        tree.lookup_entry_by_path(path)
            .map_err(|e| GitError::message(format!("cannot resolve {revision}:{path}: {e}")))?
            .map(|entry| entry.object_id())
    };
    build_file_diff(repo, old_id, path, meta, ctx)
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

/// Load the blob at `old_id` (or the empty blob when `None`) and the worktree
/// content at `path` into `cache` as the source/destination resources for a
/// blob diff. Shared by the per-file diff construction and the added/removed
/// line counting in the status scan.
pub(crate) fn set_blob_resources(
    cache: &mut gix::diff::blob::Platform,
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    path: &str,
) -> Result<(), GitError> {
    let rela: &gix::bstr::BStr = path.into();
    let null = gix::ObjectId::null(repo.object_hash());

    cache
        .set_resource(
            old_id.unwrap_or(null),
            EntryKind::Blob,
            rela,
            gix::diff::blob::ResourceKind::OldOrSource,
            &repo.objects,
        )
        .map_err(|e| GitError::message(format!("diff {path}: {e}")))?;
    cache
        .set_resource(
            null,
            EntryKind::Blob,
            rela,
            gix::diff::blob::ResourceKind::NewOrDestination,
            &repo.objects,
        )
        .map_err(|e| GitError::message(format!("diff {path}: {e}")))?;
    Ok(())
}

/// Build a single-file [`Diff`] comparing the blob at `old_id` (or empty
/// when `None`) against the worktree content at `path`.
fn build_file_diff(
    repo: &gix::Repository,
    old_id: Option<gix::ObjectId>,
    path: &str,
    meta: &FileMeta,
    ctx: u32,
) -> Result<Diff, GitError> {
    let roots = repo
        .workdir()
        .map(|p| gix::diff::blob::pipeline::WorktreeRoots {
            // The source side resolves from the object database by id; only the
            // destination side is read from the working tree.
            old_root: None,
            new_root: Some(p.to_path_buf()),
        })
        .unwrap_or_default();
    let mut cache = repo
        .diff_resource_cache(gix::diff::blob::pipeline::Mode::ToGit, roots)
        .map_err(|e| GitError::message(e.to_string()))?;

    set_blob_resources(&mut cache, repo, old_id, path)?;

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
}
