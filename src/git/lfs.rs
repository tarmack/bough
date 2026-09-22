//! Git-LFS pointer detection.
//!
//! LFS keeps large files out of the object database: the blob is a tiny
//! pointer while the real content lives elsewhere. Running the full
//! blob-diff/filter pipeline on a pointer is pure overhead, so bough
//! classifies such files up front and shows them as binary stubs.

use gix::bstr::ByteSlice;

use crate::diff::LfsInfo;

/// The canonical first line of every LFS pointer.
const LFS_MAGIC: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// Largest blob treated as a pointer candidate. Real pointers are well under
/// 200 bytes; the cap keeps `find_blob` off large real content.
pub(crate) const POINTER_MAX: u64 = 512;

/// True when `bytes` opens with the LFS pointer spec-version line.
pub(crate) fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes.starts_with(LFS_MAGIC) && matches!(bytes.get(LFS_MAGIC.len()), None | Some(b'\n'))
}

/// Parse an LFS pointer's `oid sha256:` digest (without the prefix) and `size`.
/// Returns `None` when the bytes aren't a pointer or either field is missing.
pub(crate) fn lfs_pointer_info(bytes: &[u8]) -> Option<(String, u64)> {
    if !is_lfs_pointer(bytes) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut oid = None;
    let mut size = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("oid sha256:") {
            oid = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("size ") {
            size = value.trim().parse().ok();
        }
    }
    Some((oid?, size?))
}

/// The pointer metadata for the blob at `id`, or `None` when it isn't a
/// pointer (including when it is too large to be one).
fn pointer_info(repo: &gix::Repository, id: gix::ObjectId) -> Option<LfsInfo> {
    if repo.find_header(id).ok()?.size() > POINTER_MAX {
        return None;
    }
    let (oid, size) = lfs_pointer_info(&repo.find_blob(id).ok()?.data)?;
    Some(LfsInfo { oid, size })
}

/// Cached attributes stack for the `filter=lfs` lookup, mirroring the worker's
/// exclude matcher: built once per scan or diff operation and reused across
/// paths rather than rebuilt per file.
pub(crate) struct LfsMatcher<'repo> {
    stack: Option<gix::AttributeStack<'repo>>,
    out: gix::filter::plumbing::attributes::search::Outcome,
}

impl<'repo> LfsMatcher<'repo> {
    /// Build from `repo`. The matcher degrades to "not LFS" when the index or
    /// attributes are unavailable.
    pub(crate) fn new(repo: &'repo gix::Repository) -> Self {
        let stack = repo.index_or_empty().ok().and_then(|index| {
            repo.attributes_only(
                &index,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
            )
            .ok()
        });
        let mut out = gix::filter::plumbing::attributes::search::Outcome::default();
        // gix's diff pipeline selects the `diff` attribute, so query `filter`
        // explicitly to see the LFS driver assignment.
        out.initialize_with_selection(&Default::default(), Some("filter"));
        LfsMatcher { stack, out }
    }

    /// True when `.gitattributes` assigns `filter=lfs` to `path`.
    pub(crate) fn is_lfs_path(&mut self, path: &str) -> bool {
        let Some(stack) = self.stack.as_mut() else {
            return false;
        };
        let Ok(platform) = stack.at_path(path, None) else {
            return false;
        };
        platform.matching_attributes(&mut self.out);
        self.out.iter_selected().any(|m| {
            m.assignment.name.as_str() == "filter"
                && m.assignment
                    .state
                    .as_bstr()
                    .is_some_and(|value| value.as_bytes() == b"lfs".as_slice())
        })
    }
}

/// Classify a changed path.
///
/// A path is LFS when either known side's blob is a pointer — the old/base blob
/// or the new-side blob (HEAD or index content when it is in the object
/// database) — or, failing that, when the `filter=lfs` attribute matches the
/// path. The attribute is the ground truth for LFS-tracking and is the only
/// signal available for untracked or added files with no blobs.
///
/// `new_id` is the new-side blob when it is known from the object database;
/// pass `None` for worktree-side entries whose content is read from disk. When
/// both sides are pointers the new side wins, so a converted file reports the
/// freshly-converted pointer's metadata.
pub(crate) fn detect(
    repo: &gix::Repository,
    matcher: &mut LfsMatcher<'_>,
    old_id: Option<gix::ObjectId>,
    new_id: Option<gix::ObjectId>,
    path: &str,
) -> Option<LfsInfo> {
    let old = old_id.and_then(|id| pointer_info(repo, id));
    let new = new_id.and_then(|id| pointer_info(repo, id));
    if new.is_some() || old.is_some() {
        return new.or(old);
    }
    if !matcher.is_lfs_path(path) {
        return None;
    }
    let size = repo
        .workdir()
        .and_then(|root| std::fs::metadata(root.join(path)).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);
    Some(LfsInfo {
        oid: String::new(),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const POINTER: &[u8] = b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
size 12345678\n";

    #[test]
    fn parses_valid_pointer() {
        assert!(is_lfs_pointer(POINTER));
        let (oid, size) = lfs_pointer_info(POINTER).unwrap();
        assert_eq!(
            oid,
            "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393"
        );
        assert_eq!(size, 12345678);
    }

    #[test]
    fn parses_pointer_with_extra_lines() {
        let mut bytes = POINTER.to_vec();
        bytes.extend_from_slice(b"extra: ignored\n");
        let (_, size) = lfs_pointer_info(&bytes).unwrap();
        assert_eq!(size, 12345678);
    }

    #[test]
    fn rejects_non_pointers() {
        assert!(!is_lfs_pointer(b"hello world\n"));
        assert!(lfs_pointer_info(b"hello world\n").is_none());
        assert!(lfs_pointer_info(b"\x00\x01\x02binary").is_none());
        // The magic line alone is not a pointer without oid/size.
        assert!(lfs_pointer_info(b"version https://git-lfs.github.com/spec/v1\n").is_none());
    }

    #[test]
    fn attribute_marks_added_file_as_lfs() {
        // The `filter=lfs` attribute is the signal for a file whose old side
        // is absent; no filter actually runs.
        let dir = tempfile::tempdir().unwrap();
        crate::git::test_util::repo_with_commits(dir.path(), &["one"]);
        std::fs::write(dir.path().join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "attrs"]);

        let content = "real content, not a pointer\n";
        std::fs::write(dir.path().join("data.bin"), content).unwrap();

        let repo = gix::discover(dir.path()).unwrap();
        let mut matcher = LfsMatcher::new(&repo);
        assert!(matcher.is_lfs_path("data.bin"));
        assert!(!matcher.is_lfs_path("file0.txt"));

        let info = detect(&repo, &mut matcher, None, None, "data.bin").unwrap();
        assert!(info.oid.is_empty());
        assert_eq!(info.size, content.len() as u64);
    }

    #[test]
    fn new_side_pointer_wins_for_converted_file() {
        // A file converted from plain text to an LFS pointer has no pointer on
        // the old side; the new side must be detected and its metadata used.
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        crate::git::test_util::run_git(dir.path(), &["config", "user.email", "t@e"]);
        crate::git::test_util::run_git(dir.path(), &["config", "user.name", "T"]);
        std::fs::write(dir.path().join("data.bin"), "real content, not a pointer\n").unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "text"]);

        const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 999\n");
        std::fs::write(dir.path().join("data.bin"), pointer).unwrap();
        crate::git::test_util::run_git(dir.path(), &["add", "."]);
        crate::git::test_util::run_git(dir.path(), &["commit", "-m", "pointer"]);

        let repo = gix::discover(dir.path()).unwrap();
        let old = crate::git::resolve_id(&repo, "HEAD~1:data.bin").unwrap();
        let new = crate::git::resolve_id(&repo, "HEAD:data.bin").unwrap();
        let mut matcher = LfsMatcher::new(&repo);

        let info = detect(&repo, &mut matcher, Some(old), Some(new), "data.bin").unwrap();
        assert_eq!(info.oid, OID);
        assert_eq!(info.size, 999);
    }
}
