//! Structured diff data model consumed by the renderer.
//!
//! A [`Diff`] bundles a [`FileHeader`] (name, rename/mode/binary
//! metadata), the ordered [`Section`] list (files and hunks) and classified
//! body lines carrying optional word-level change runs. The git engine builds
//! these structures directly from gix's blob diff; no unified-diff text is
//! produced. The renderer draws lines without `+`/`-` prefix glyphs
//! and with lighter-shade backgrounds on changed word runs.

use std::sync::Arc;

/// Classification of a single body line in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
    /// A hunk header row.
    HunkHeader,
    /// A per-file section header inside a multi-file (folder) diff.
    FileHeader,
}

impl LineKind {
    /// Whether this line is a navigable header (`@@` hunk or file header).
    pub fn is_header(self) -> bool {
        matches!(self, LineKind::HunkHeader | LineKind::FileHeader)
    }
}

/// A single body line in a diff.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    /// How this line should be rendered.
    pub kind: LineKind,
    /// The line content. Empty for hunk headers (rendered from `HunkMeta`); the
    /// path for file headers.
    pub raw_text: String,
    /// Index into [`Diff::sections`] of the section this line belongs
    /// to. Header lines (`HunkHeader`/`FileHeader`) name their own section;
    /// body lines name the hunk (or file) section they sit in.
    pub section: usize,
    /// Word-level runs, set on Added/Removed lines when LCS word-diff was
    /// applied to the adjacent pair; `None` otherwise. The same shared vector
    /// is assigned to both members of a paired removed/added block.
    pub word_runs: Option<Vec<WordRun>>,
}

/// A navigable, foldable unit of a diff body: either one file's section (its
/// `FileHeader` row) or one hunk within a file. Drives chunk navigation and
/// folding, which treat files and hunks uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    File,
    Hunk,
}

/// One section of a diff: a file or a hunk. See [`Diff::sections`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    /// Index into [`Diff::files`] of the owning file.
    pub file: usize,
    /// For [`SectionKind::Hunk`], the index into `files[file].hunks`.
    pub hunk: Option<usize>,
}

/// One run of tokens in a word-level diff of a line pair.
#[derive(Debug, Clone, PartialEq)]
pub struct WordRun {
    pub text: String,
    /// `true` when this run is present on only one side of the pair.
    pub changed: bool,
    /// Which side this run came from (`Added`/`Removed`); `Context` when it is
    /// shared by both sides. The renderer only applies a lighter shade when
    /// `kind` matches the owning line's kind *and* `changed` is `true`.
    pub kind: LineKind,
}

/// Metadata for one hunk: added/removed line counts.
#[derive(Debug, Clone, PartialEq)]
pub struct HunkMeta {
    /// Added lines in this hunk's body.
    pub added: usize,
    /// Removed lines in this hunk's body.
    pub deleted: usize,
}

/// A single file's change status, replacing the raw git status letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

impl FileStatus {
    /// The single-letter git status code (`A`/`M`/`D`/`R`/`C`/`T`).
    pub fn as_char(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
            FileStatus::TypeChanged => 'T',
        }
    }

    /// Parse a status letter, or `None` for an unknown letter.
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'A' => Some(FileStatus::Added),
            'M' => Some(FileStatus::Modified),
            'D' => Some(FileStatus::Deleted),
            'R' => Some(FileStatus::Renamed),
            'C' => Some(FileStatus::Copied),
            'T' => Some(FileStatus::TypeChanged),
            _ => None,
        }
    }
}

/// Git-LFS object metadata for a file whose content is an LFS pointer.
#[derive(Debug, Clone, PartialEq)]
pub struct LfsInfo {
    /// The referenced object's SHA-256 hex digest, without the `sha256:`
    /// prefix. Empty when unknown (an added file with no stored pointer).
    pub oid: String,
    /// The object size in bytes: from the pointer's `size` line, or the
    /// worktree file's size for an added file with no stored pointer.
    pub size: u64,
}

/// Per-file metadata populated by the git engine.
#[derive(Debug, Clone, PartialEq)]
pub struct FileHeader {
    /// The (new) path, passed in by the caller.
    pub path: String,
    /// Old path; set only for renames.
    pub old_path: Option<String>,
    /// Change status (`Added`/`Modified`/`Deleted`/`Renamed`/`Copied`/
    /// `TypeChanged`) passed in by the caller.
    pub status: FileStatus,
    /// Number of added body lines (0 for binary files).
    pub added: usize,
    /// Number of removed body lines (0 for binary files).
    pub deleted: usize,
    /// Similarity percentage for renames/copies (0–100).
    pub rename_percent: Option<u8>,
    pub mode_old: Option<u32>,
    pub mode_new: Option<u32>,
    pub binary: bool,
    /// Set when the file is a Git-LFS object; the renderer shows a stub note
    /// instead of a content diff.
    pub lfs: Option<LfsInfo>,
    /// Per-hunk metadata, one entry per `@@` header, in file order.
    pub hunks: Vec<HunkMeta>,
}

/// A structured diff, built directly by the git engine, covering one file or
/// many (a directory diff produces one entry per changed file).
#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    /// Locked top header. For a single-file diff this is that file's header;
    /// for a multi-file (folder) diff it is a summary carrying the folder path
    /// and aggregate counts.
    pub header: FileHeader,
    /// Per-file headers in document order.
    pub files: Vec<FileHeader>,
    /// Ordered sections: a `File` section for each file, followed by its
    /// `Hunk` sections, in document order. Drives navigation and folding.
    pub sections: Vec<Section>,
    /// Classified body lines. File-metadata lines are excluded.
    pub lines: Vec<DiffLine>,
}

impl Diff {
    /// Index of the `File` section owning `s` (the first `File` section with
    /// the same `file`), or `None` when no file section exists (single-file
    /// diffs have hunk sections only).
    pub fn file_section_for(&self, s: usize) -> Option<usize> {
        let file = self.sections.get(s)?.file;
        self.sections
            .iter()
            .position(|x| x.kind == SectionKind::File && x.file == file)
    }

    /// Assemble a multi-file (directory) diff from a list of single-file
    /// diffs, in document order. Each `file_diffs` entry must be a single-file
    /// `Diff` (one file, hunk sections only); the result carries a `File`
    /// section plus a `FileHeader` body row per file under a summary header.
    /// Children are borrowed behind `Arc`, so streaming a growing directory
    /// aggregate never deep-clones the already-built child diffs.
    pub fn from_directory(path: &str, file_diffs: &[Arc<Diff>]) -> Diff {
        let mut files = Vec::new();
        let mut sections = Vec::new();
        let mut lines = Vec::new();

        for (file, d) in file_diffs.iter().enumerate() {
            debug_assert_eq!(d.files.len(), 1, "from_directory expects single-file diffs");
            debug_assert!(
                d.sections.iter().all(|s| s.kind == SectionKind::Hunk),
                "from_directory expects hunk-only sections"
            );
            files.push(d.header.clone());

            // A `File` section opens this file's block.
            let file_section = sections.len();
            sections.push(Section {
                kind: SectionKind::File,
                file,
                hunk: None,
            });
            lines.push(DiffLine {
                kind: LineKind::FileHeader,
                raw_text: d.header.path.clone(),
                section: file_section,
                word_runs: None,
            });

            // Re-home the file's hunk sections and body lines under this file:
            // hunk sections (originally 0..N) become file_section+1.. and every
            // line's `section` shifts by the same base.
            let base = sections.len();
            for &section in &d.sections {
                sections.push(Section { file, ..section });
            }
            for line in &d.lines {
                let mut line = line.clone();
                line.section += base;
                lines.push(line);
            }
        }

        let header = FileHeader {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added: files.iter().map(|f| f.added).sum(),
            deleted: files.iter().map(|f| f.deleted).sum(),
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: files.iter().any(|f| f.binary),
            lfs: None,
            hunks: Vec::new(),
        };

        Diff {
            header,
            files,
            sections,
            lines,
        }
    }
}

/// Upper bound on the number of changed word runs per line pair. Above this
/// the word-level diff is abandoned and the renderer falls back to a plain
/// tint (avoids noisy highlighting on heavily-rewritten lines).
const WORD_DIFF_MAX_RUNS: usize = 6;

/// Upper bound on the token count of either line side before word-level diff.
/// LCS fills a table quadratic in token count; above this the allocation and
/// runtime blow up on a single huge line (minified files, dumped structs), so
/// word diff is abandoned and the renderer falls back to a plain tint.
const WORD_DIFF_MAX_TOKENS: usize = 256;

/// Tokenize a line into word runs for LCS comparison.
///
/// Each character falls into one of three token classes — word
/// (`[0-9A-Za-z_]` plus Unicode word chars), whitespace, or other punctuation
/// — and each maximal run of a single class becomes one token. Splitting
/// whitespace out of punctuation keeps a line like `"fn main() {}"` split as
/// `["fn", " ", "main", "()", " ", "{}"]`, so whitespace boundaries are
/// preserved while punctuation clusters stay whole.
fn tokenize_words(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut cur_cat: Option<TokenCat> = None;
    for ch in text.chars() {
        let cat = classify_char(ch);
        match cur_cat {
            None => {
                cur.push(ch);
                cur_cat = Some(cat);
            }
            Some(c) if c == cat => cur.push(ch),
            Some(_) => {
                tokens.push(std::mem::take(&mut cur));
                cur.push(ch);
                cur_cat = Some(cat);
            }
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Character token class. Whitespace is split out of punctuation so that a
/// space between two punctuation clusters (`() {}`) forms its own token rather
/// than gluing the clusters together.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenCat {
    Word,
    Space,
    Punct,
}

fn classify_char(c: char) -> TokenCat {
    if c.is_alphanumeric() || c == '_' {
        TokenCat::Word
    } else if c.is_whitespace() {
        TokenCat::Space
    } else {
        TokenCat::Punct
    }
}

/// Compute an LCS-based word-level diff between two stripped lines.
///
/// Returns `Some(runs)` when the number of changed runs is at most
/// [`WORD_DIFF_MAX_RUNS`]; `None` (caller falls back to a plain tint) when the
/// line was rewritten too heavily.
fn word_diff(old: &str, new: &str) -> Option<Vec<WordRun>> {
    let old_tokens = tokenize_words(old);
    let new_tokens = tokenize_words(new);
    let n = old_tokens.len();
    let m = new_tokens.len();

    if n > WORD_DIFF_MAX_TOKENS || m > WORD_DIFF_MAX_TOKENS {
        return None;
    }

    // LCS length of old_tokens[i..] and new_tokens[j..], stored flat in a
    // single `(n + 1) * (m + 1)` table at `dp[i * stride + j]`. The LCS is at
    // most `min(n, m) <= WORD_DIFF_MAX_TOKENS`, so a `u16` entry suffices.
    let stride = m + 1;
    let mut dp = vec![0u16; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * stride + j] = if old_tokens[i] == new_tokens[j] {
                dp[(i + 1) * stride + (j + 1)] + 1
            } else {
                dp[(i + 1) * stride + j].max(dp[i * stride + (j + 1)])
            };
        }
    }

    let mut runs: Vec<WordRun> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_tokens[i] == new_tokens[j] {
            push_run(
                &mut runs,
                WordRun {
                    text: old_tokens[i].clone(),
                    changed: false,
                    kind: LineKind::Context,
                },
            );
            i += 1;
            j += 1;
        } else if dp[(i + 1) * stride + j] >= dp[i * stride + (j + 1)] {
            push_run(
                &mut runs,
                WordRun {
                    text: old_tokens[i].clone(),
                    changed: true,
                    kind: LineKind::Removed,
                },
            );
            i += 1;
        } else {
            push_run(
                &mut runs,
                WordRun {
                    text: new_tokens[j].clone(),
                    changed: true,
                    kind: LineKind::Added,
                },
            );
            j += 1;
        }
    }
    while i < n {
        push_run(
            &mut runs,
            WordRun {
                text: old_tokens[i].clone(),
                changed: true,
                kind: LineKind::Removed,
            },
        );
        i += 1;
    }
    while j < m {
        push_run(
            &mut runs,
            WordRun {
                text: new_tokens[j].clone(),
                changed: true,
                kind: LineKind::Added,
            },
        );
        j += 1;
    }

    let changed_runs = runs.iter().filter(|r| r.changed).count();
    if changed_runs > WORD_DIFF_MAX_RUNS {
        None
    } else {
        Some(runs)
    }
}

/// Append a run, merging it into the previous run when both have the same
/// `(changed, kind)` tuple.
fn push_run(runs: &mut Vec<WordRun>, run: WordRun) {
    if let Some(last) = runs.last_mut() {
        if last.changed == run.changed && last.kind == run.kind {
            last.text.push_str(&run.text);
            return;
        }
    }
    runs.push(run);
}

/// Word-level pairing.
///
/// Word-diff applies per adjacent removed/added block: a maximal run of
/// `Removed` lines immediately followed by a maximal run of `Added` lines is
/// paired positionally (`removed[k]` with `added[k]`). Each line of a pair
/// owns a separate `Vec<WordRun>` built from the same pairing. Lines that fall outside an equal-length
/// pairing (e.g. a 2-removed/3-added block) keep `word_runs = None`.
pub(crate) fn apply_word_pairs(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        let removed_len = lines[i..]
            .iter()
            .take_while(|l| l.kind == LineKind::Removed)
            .count();
        let add_start = i + removed_len;
        let added_len = lines[add_start..]
            .iter()
            .take_while(|l| l.kind == LineKind::Added)
            .count();

        for k in 0..removed_len.min(added_len) {
            let runs = word_diff(&lines[i + k].raw_text, &lines[add_start + k].raw_text);
            lines[i + k].word_runs = runs.clone();
            lines[add_start + k].word_runs = runs;
        }

        i = add_start + added_len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_fn_main() {
        // `()` and `{}` are each a single non-word run, hence kept whole.
        assert_eq!(
            tokenize_words("fn main() {}"),
            vec!["fn", " ", "main", "()", " ", "{}"]
        );
    }

    #[test]
    fn word_diff_single_swap() {
        assert_eq!(
            word_diff("a", "b"),
            Some(vec![
                WordRun {
                    text: "a".to_string(),
                    changed: true,
                    kind: LineKind::Removed,
                },
                WordRun {
                    text: "b".to_string(),
                    changed: true,
                    kind: LineKind::Added,
                },
            ])
        );
    }

    #[test]
    fn word_diff_identical() {
        assert_eq!(
            word_diff("same", "same"),
            Some(vec![WordRun {
                text: "same".to_string(),
                changed: false,
                kind: LineKind::Context,
            }])
        );
    }

    #[test]
    fn word_diff_too_many_changes_returns_none() {
        // Eight distinct word swaps, each an interleaved changed run, exceed
        // WORD_DIFF_MAX_RUNS.
        assert_eq!(word_diff("a b c d e f g h", "x y z w q r s t"), None);
    }

    #[test]
    fn word_diff_at_token_cap() {
        // 128 word tokens + 128 space tokens == the exact cap; the flattened
        // table allocation must work at the boundary.
        let line = "a ".repeat(WORD_DIFF_MAX_TOKENS / 2);
        assert_eq!(tokenize_words(&line).len(), WORD_DIFF_MAX_TOKENS);
        assert_eq!(
            word_diff(&line, &line),
            Some(vec![WordRun {
                text: line.clone(),
                changed: false,
                kind: LineKind::Context,
            }])
        );
    }

    fn line(kind: LineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            raw_text: text.to_string(),
            section: 0,
            word_runs: None,
        }
    }

    #[test]
    fn apply_word_pairs_shares_runs_between_pair() {
        let mut lines = vec![line(LineKind::Removed, "foo"), line(LineKind::Added, "bar")];
        apply_word_pairs(&mut lines);
        assert_eq!(lines[0].word_runs, lines[1].word_runs);
        assert!(lines[0].word_runs.is_some());
    }

    #[test]
    fn apply_word_pairs_pairs_positionally() {
        // removed[0] pairs with added[0], removed[1] with added[1].
        let mut lines = vec![
            line(LineKind::Removed, "aaa"),
            line(LineKind::Removed, "bbb"),
            line(LineKind::Added, "AAA"),
            line(LineKind::Added, "BBB"),
        ];
        apply_word_pairs(&mut lines);
        for l in &lines {
            assert!(l.word_runs.is_some(), "line should carry word runs");
        }
        assert_eq!(lines[0].word_runs, lines[2].word_runs);
        assert_eq!(lines[1].word_runs, lines[3].word_runs);
    }

    fn single_file(path: &str, added: usize, deleted: usize) -> Diff {
        let header = FileHeader {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added,
            deleted,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: false,
            lfs: None,
            hunks: vec![HunkMeta { added, deleted }],
        };
        Diff {
            header: header.clone(),
            files: vec![header],
            sections: vec![Section {
                kind: SectionKind::Hunk,
                file: 0,
                hunk: Some(0),
            }],
            lines: vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@".to_string(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "old".to_string(),
                    section: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "new".to_string(),
                    section: 0,
                    word_runs: None,
                },
            ],
        }
    }

    #[test]
    fn from_directory_combines_files() {
        let diff = Diff::from_directory(
            "src",
            &[
                Arc::new(single_file("src/a.rs", 1, 1)),
                Arc::new(single_file("src/b.rs", 1, 1)),
            ],
        );

        // Two files, each with its own File section and one Hunk section.
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].path, "src/a.rs");
        assert_eq!(diff.files[1].path, "src/b.rs");
        assert_eq!(diff.sections.len(), 4);
        assert_eq!(diff.sections[0].kind, SectionKind::File);
        assert_eq!(diff.sections[0].file, 0);
        assert_eq!(diff.sections[1].kind, SectionKind::Hunk);
        assert_eq!(diff.sections[1].hunk, Some(0));
        assert_eq!(diff.sections[2].kind, SectionKind::File);
        assert_eq!(diff.sections[2].file, 1);
        assert_eq!(diff.sections[3].kind, SectionKind::Hunk);
        assert_eq!(diff.sections[3].hunk, Some(0));

        // Body lines section indices re-home to the per-file sections.
        let kinds: Vec<LineKind> = diff.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::FileHeader,
                LineKind::HunkHeader,
                LineKind::Removed,
                LineKind::Added,
                LineKind::FileHeader,
                LineKind::HunkHeader,
                LineKind::Removed,
                LineKind::Added,
            ]
        );

        // Summary header: folder path + aggregate counts, no hunk counter.
        assert_eq!(diff.header.path, "src");
        assert_eq!(diff.header.added, 2);
        assert_eq!(diff.header.deleted, 2);
        assert!(diff.header.hunks.is_empty());
    }

    #[test]
    fn from_directory_propagates_lfs_headers() {
        let text = single_file("src/a.rs", 2, 1);
        let lfs_header = FileHeader {
            path: "src/b.bin".into(),
            old_path: None,
            status: FileStatus::Modified,
            added: 0,
            deleted: 0,
            rename_percent: None,
            mode_old: None,
            mode_new: None,
            binary: true,
            lfs: Some(LfsInfo {
                oid: "abc".into(),
                size: 42,
            }),
            hunks: Vec::new(),
        };
        let lfs = Diff {
            header: lfs_header.clone(),
            files: vec![lfs_header],
            sections: Vec::new(),
            lines: Vec::new(),
        };

        let diff = Diff::from_directory("src", &[Arc::new(text), Arc::new(lfs)]);
        assert_eq!(diff.files.len(), 2);
        assert!(diff.files[0].lfs.is_none());
        assert_eq!(diff.files[1].lfs.as_ref().unwrap().size, 42);
        // The LFS stub contributes no lines: aggregates sum only the text file.
        assert_eq!(diff.header.added, 2);
        assert_eq!(diff.header.deleted, 1);
        assert!(diff.header.binary);
    }
}
