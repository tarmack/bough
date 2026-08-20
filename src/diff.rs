//! Structured diff parser for raw `git diff` output.
//!
//! Turns raw `git diff` text into a [`ParsedDiff`]: a [`FileHeader`] (name,
//! rename/mode/binary metadata) plus a list of classified body lines carrying
//! optional word-level change runs. The renderer consumes this data to draw
//! diffs without `+`/`-` prefix glyphs and with lighter-shade backgrounds on
//! changed word runs.
//!
//! All functions here are total (infallible) — parsing is defensive and never
//! panics on malformed input.

/// Classification of a single body line in a parsed diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context line (git prefixes it with a single space).
    Context,
    /// Added line (prefixed with `+`).
    Added,
    /// Removed line (prefixed with `-`).
    Removed,
    /// A hunk header (`@@ -a,b +c,d @@`).
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

/// A single body line in a parsed diff.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    /// How this line should be rendered.
    pub kind: LineKind,
    /// The line with its leading `+`/`-`/` ` prefix stripped. Hunk headers
    /// keep the full `@@ ... @@` text; file headers keep their raw text.
    pub raw_text: String,
    /// Index into [`ParsedDiff::sections`] of the section this line belongs
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

/// One section of a diff: a file or a hunk. See [`ParsedDiff::sections`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    /// Index into [`ParsedDiff::files`] of the owning file.
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

/// Metadata for one hunk, parsed from its `@@ -a,b +c,d @@ <section>` header.
#[derive(Debug, Clone, PartialEq)]
pub struct HunkMeta {
    /// Section context after the closing `@@` (e.g. `fn main()`), trimmed.
    /// `None` if absent.
    pub section: Option<String>,
    /// Added lines in this hunk's body.
    pub added: usize,
    /// Removed lines in this hunk's body.
    pub deleted: usize,
}

/// File-level metadata extracted from the extended diff header.
#[derive(Debug, Clone, PartialEq)]
pub struct FileHeader {
    /// New path. Defaults to the caller-supplied `path`; overridden by
    /// `+++ b/<path>` / `rename to <new>` when present.
    pub path: String,
    /// Old path; set only for renames.
    pub old_path: Option<String>,
    /// Status letter (`A`/`M`/`D`/`R`/`C`/`T`) passed in by the caller.
    pub status: char,
    /// Number of added body lines (0 for binary files).
    pub added: usize,
    /// Number of removed body lines (0 for binary files).
    pub deleted: usize,
    /// Similarity percentage for renames/copies (0–100).
    pub rename_percent: Option<u8>,
    pub mode_old: Option<u32>,
    pub mode_new: Option<u32>,
    pub binary: bool,
    /// Per-hunk metadata, one entry per `@@` header, in file order.
    pub hunks: Vec<HunkMeta>,
}

/// The parsed form of a `git diff`, covering one file or many (a directory
/// diff produces one entry per changed file).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDiff {
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

impl ParsedDiff {
    /// Index of the `File` section owning `s` (the first `File` section with
    /// the same `file`), or `None` when no file section exists (single-file
    /// diffs have hunk sections only).
    pub fn file_section_for(&self, s: usize) -> Option<usize> {
        let file = self.sections.get(s)?.file;
        self.sections
            .iter()
            .position(|x| x.kind == SectionKind::File && x.file == file)
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

/// Parse raw `git diff` output into a [`ParsedDiff`].
///
/// A diff may cover one file (the common case) or several, when the caller
/// selected a whole directory. Each file becomes a [`FileHeader`] in `files`
/// plus a `File` section whose body is its `Hunk` sections.
///
/// `path` and `status` come from the caller's tree data; `path` is the
/// fallback when the diff header does not name a (renamed) new path and the
/// summary header's path for a multi-file diff.
pub fn parse_diff(raw: &str, path: &str, status: char) -> ParsedDiff {
    let blocks = split_blocks(raw);
    let multi = blocks.len() > 1;

    let mut files = Vec::new();
    let mut sections = Vec::new();
    let mut lines = Vec::new();

    for block in &blocks {
        let file = files.len();
        let header = parse_block(block, multi, file, &mut sections, &mut lines, path, status);
        files.push(header);
    }
    apply_word_pairs(&mut lines);

    let header = summary_header(&files, path, multi);
    ParsedDiff {
        header,
        files,
        sections,
        lines,
    }
}

/// Build the locked top header: the single file's own header, or a folder
/// summary (folder path + aggregate counts) for a multi-file diff.
fn summary_header(files: &[FileHeader], path: &str, multi: bool) -> FileHeader {
    if !multi {
        return files[0].clone();
    }
    FileHeader {
        path: path.to_string(),
        old_path: None,
        status: 'M',
        added: files.iter().map(|f| f.added).sum(),
        deleted: files.iter().map(|f| f.deleted).sum(),
        rename_percent: None,
        mode_old: None,
        mode_new: None,
        binary: files.iter().any(|f| f.binary),
        hunks: Vec::new(),
    }
}

/// Parse one file's raw block into its [`FileHeader`], emitting its `Hunk`
/// sections (plus a `File` section when the diff spans multiple files) and
/// their body lines into `sections`/`lines`.
///
/// `file` is this block's index into the diff's `files` list.
fn parse_block(
    block: &str,
    multi: bool,
    file: usize,
    sections: &mut Vec<Section>,
    lines: &mut Vec<DiffLine>,
    fallback_path: &str,
    fallback_status: char,
) -> FileHeader {
    // A `File` section is emitted only for multi-file diffs, where each file
    // shows an in-body header row. A single-file diff keeps hunks as the only
    // sections so hunk navigation is unchanged.
    let mut cur_section = sections.len();
    if multi {
        let file_section = sections.len();
        sections.push(Section {
            kind: SectionKind::File,
            file,
            hunk: None,
        });
        cur_section = file_section;

        // Emit the file-header row.
        lines.push(DiffLine {
            kind: LineKind::FileHeader,
            raw_text: block_new_path(block)
                .unwrap_or(fallback_path)
                .to_string(),
            section: file_section,
            word_runs: None,
        });
    }

    let mut header = FileHeader {
        path: block_new_path(block)
            .unwrap_or(fallback_path)
            .to_string(),
        old_path: block_old_path(block),
        status: block_status(block).unwrap_or(if multi { 'M' } else { fallback_status }),
        added: 0,
        deleted: 0,
        rename_percent: block_rename_percent(block),
        mode_old: block_mode_old(block),
        mode_new: block_mode_new(block),
        binary: block_binary(block),
        hunks: Vec::new(),
    };

    // Walk the body: each `@@` opens a new `Hunk` section; body lines hang
    // off the current hunk (or the file section when the file has no hunks).
    for raw_line in block.lines() {
        if is_meta_line(raw_line) {
            continue;
        }
        let (kind, raw_text) = classify(raw_line);
        if kind == LineKind::HunkHeader {
            let hunk_section = sections.len();
            let hunk = header.hunks.len();
            sections.push(Section {
                kind: SectionKind::Hunk,
                file,
                hunk: Some(hunk),
            });
            header.hunks.push(parse_hunk_header(raw_line));
            cur_section = hunk_section;
        }
        lines.push(DiffLine {
            kind,
            raw_text,
            section: cur_section,
            word_runs: None,
        });
        // Accumulate added/deleted on the owning file and (when inside a
        // hunk) on that hunk's metadata.
        match kind {
            LineKind::Added => {
                header.added += 1;
                if let Some(hunk) = header.hunks.last_mut() {
                    hunk.added += 1;
                }
            }
            LineKind::Removed => {
                header.deleted += 1;
                if let Some(hunk) = header.hunks.last_mut() {
                    hunk.deleted += 1;
                }
            }
            _ => {}
        }
    }

    header
}

/// Split raw diff output into per-file blocks at each `diff --git` line. When
/// the text contains no such marker (empty or synthetic diff) the whole input
/// is one block.
fn split_blocks(raw: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            blocks.push(String::new());
        } else if blocks.is_empty() && !line.trim().is_empty() {
            // Leading non-`diff --git` content (e.g. an error/header line):
            // seed a single block so nothing is dropped.
            blocks.push(String::new());
        }
        if let Some(last) = blocks.last_mut() {
            last.push_str(line);
            last.push('\n');
        }
    }
    if blocks.is_empty() {
        blocks.push(raw.to_string());
    }
    blocks
}

/// New path from the `diff --git a/<old> b/<new>` or `+++ b/<path>` line.
fn block_new_path(block: &str) -> Option<&str> {
    // `+++ b/<path>` (or `/dev/null` for deletions).
    let mut plus = None;
    let mut header = None;
    for line in block.lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            if p != "/dev/null" {
                plus = Some(p);
            }
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if p != "/dev/null" {
                plus = Some(p);
            }
        } else if let Some(p) = line.strip_prefix("diff --git ") {
            header = Some(p);
        }
    }
    if let Some(p) = plus {
        return Some(p);
    }
    // `diff --git a/old b/new` → the part after ` b/`.
    let h = header?;
    let (_, new) = h.rsplit_once(" b/")?;
    Some(new)
}

/// Old path from the `diff --git a/<old>` prefix, when present.
fn block_old_path(block: &str) -> Option<String> {
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(old) = rest.strip_prefix("a/") {
                let old = old.split(" b/").next().unwrap_or(old);
                if old != "/dev/null" {
                    return Some(old.to_string());
                }
            }
        }
    }
    None
}

/// Status letter inferred from the block's meta lines, or `None` (caller's).
fn block_status(block: &str) -> Option<char> {
    let mut rename = false;
    for line in block.lines() {
        if line.starts_with("new file mode ") {
            return Some('A');
        }
        if line.starts_with("deleted file mode ") {
            return Some('D');
        }
        if line.starts_with("rename from ") || line.starts_with("rename to ") {
            rename = true;
        }
    }
    if rename {
        Some('R')
    } else {
        None
    }
}

/// Similarity percentage from the `similarity index N%` line.
fn block_rename_percent(block: &str) -> Option<u8> {
    block.lines().find_map(|l| {
        let digits = l.strip_prefix("similarity index ")?.strip_suffix('%')?;
        digits.parse().ok()
    })
}

/// Old mode from `old mode <octal>`.
fn block_mode_old(block: &str) -> Option<u32> {
    for line in block.lines() {
        if let Some(m) = line.strip_prefix("old mode ") {
            return parse_mode(m);
        }
    }
    None
}

/// New mode from `new mode <octal>`, else the `index ... <octal>` mode.
fn block_mode_new(block: &str) -> Option<u32> {
    for line in block.lines() {
        if let Some(m) = line.strip_prefix("new mode ") {
            return parse_mode(m);
        }
    }
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("index ") {
            if let Some(m) = rest.split(' ').next_back() {
                return parse_mode(m);
            }
        }
    }
    None
}

/// Parse an octal file mode string (e.g. `100644`).
fn parse_mode(m: &str) -> Option<u32> {
    u32::from_str_radix(m.trim(), 8).ok()
}

/// Whether the block marks a binary file change.
fn block_binary(block: &str) -> bool {
    block.lines().any(|l| l.starts_with("Binary files "))
}

/// Parse a single `@@ -a,b +c,d @@ <section>` hunk header into [`HunkMeta`].
///
/// Returns a zeroed [`HunkMeta`] (no section, zero counts) when the line does
/// not match — never panics. `added`/`deleted` are filled later by
/// [`parse_diff`] as it walks the hunk's body lines.
fn parse_hunk_header(raw: &str) -> HunkMeta {
    let meta = HunkMeta {
        section: None,
        added: 0,
        deleted: 0,
    };
    let Some(rest) = raw.strip_prefix("@@") else {
        return meta;
    };
    let Some(close) = rest.find("@@") else {
        return meta;
    };
    let (counts, rest2) = rest.split_at(close);
    // Section follows the closing `@@`; skip past it.
    let section = &rest2[2..];
    // `counts` is ` -a,b +c,d `; drop everything from the `+` on to isolate
    // the old-side range, then take the new-side range.
    let Some((old, new)) = counts.split_once('+') else {
        return meta;
    };
    let (Some(_), Some(_)) = (first_u32(old), first_u32(new)) else {
        return meta;
    };
    let section = section.trim();
    HunkMeta {
        section: if section.is_empty() {
            None
        } else {
            Some(section.to_string())
        },
        added: 0,
        deleted: 0,
    }
}

/// First unsigned number in `s` (leading sign/digits ignored).
fn first_u32(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

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

    // dp[i][j] = LCS length of old_tokens[i..] and new_tokens[j..].
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_tokens[i] == new_tokens[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
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
        } else if dp[i + 1][j] >= dp[i][j + 1] {
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

/// Classify a body line into a kind plus its stripped `raw_text`.
///
/// `+++`/`---` header lines never reach here — [`is_meta_line`] filters them
/// out first — so any line starting with `+`/`-` is a genuine added/removed
/// body line.
fn classify(raw_line: &str) -> (LineKind, String) {
    if raw_line.starts_with("@@") {
        (LineKind::HunkHeader, raw_line.to_string())
    } else if let Some(rest) = raw_line.strip_prefix('+') {
        (LineKind::Added, rest.to_string())
    } else if let Some(rest) = raw_line.strip_prefix('-') {
        (LineKind::Removed, rest.to_string())
    } else if let Some(rest) = raw_line.strip_prefix(' ') {
        (LineKind::Context, rest.to_string())
    } else {
        // Git always prefixes context lines with a space; be defensive.
        (LineKind::Context, raw_line.to_string())
    }
}

/// Whether `line` is a diff file-metadata header line (filtered from the body).
fn is_meta_line(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("new file mode ")
        || line.starts_with("Binary files ")
}

/// Word-level pairing.
///
/// Word-diff applies per adjacent removed/added block: a maximal run of
/// `Removed` lines immediately followed by a maximal run of `Added` lines is
/// paired positionally (`removed[k]` with `added[k]`). Both lines of a pair
/// share the same `Vec<WordRun>`. Lines that fall outside an equal-length
/// pairing (e.g. a 2-removed/3-added block) keep `word_runs = None`.
fn apply_word_pairs(lines: &mut [DiffLine]) {
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
    fn parse_diff_two_hunks() {
        let raw = "diff --git a/src/a.rs b/src/a.rs\n\
                   index 1111111 2222222 100644\n\
                   --- a/src/a.rs\n\
                   +++ b/src/a.rs\n\
                   @@ -1,3 +1,3 @@\n\
                    fn foo() {\n\
                   -    let x = 1;\n\
                   +    let x = 2;\n\
                    }\n\
                   @@ -5,3 +5,3 @@\n\
                   -fn bar() {}\n\
                   +fn bar() {\n\
                   +}\n";
        let parsed = parse_diff(raw, "src/a.rs", 'M');

        assert_eq!(parsed.header.status, 'M');
        assert_eq!(parsed.header.path, "src/a.rs");
        // `parse_diff` accumulates per-file added/deleted from the body.
        assert_eq!(parsed.header.added, 3);
        assert_eq!(parsed.header.deleted, 2);
        assert!(!parsed.header.binary);

        let kinds: Vec<LineKind> = parsed.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::HunkHeader,
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context,
                LineKind::HunkHeader,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
            ]
        );

        // A single file: no in-body `File` section — hunks are the sections,
        // so `line.section` equals the hunk index.
        let indexes: Vec<usize> = parsed.lines.iter().map(|l| l.section).collect();
        assert_eq!(indexes, vec![0, 0, 0, 0, 0, 1, 1, 1, 1]);

        assert_eq!(parsed.sections.len(), 2);
        assert_eq!(parsed.sections[0].kind, SectionKind::Hunk);
        assert_eq!(parsed.sections[0].file, 0);
        assert_eq!(parsed.sections[0].hunk, Some(0));
        assert_eq!(parsed.sections[1].kind, SectionKind::Hunk);
        assert_eq!(parsed.sections[1].file, 0);
        assert_eq!(parsed.sections[1].hunk, Some(1));

        // Per-hunk metadata: sections from the `@@` headers, added/deleted
        // from each hunk's own body lines.
        assert_eq!(parsed.header.hunks.len(), 2);
        assert_eq!(parsed.header.hunks[0].section, None);
        assert_eq!(parsed.header.hunks[0].added, 1);
        assert_eq!(parsed.header.hunks[0].deleted, 1);
        assert_eq!(parsed.header.hunks[1].section, None);
        assert_eq!(parsed.header.hunks[1].added, 2);
        assert_eq!(parsed.header.hunks[1].deleted, 1);
    }

    #[test]
    fn parse_hunk_header_extracts_section() {
        assert_eq!(
            parse_hunk_header("@@ -10,5 +12,7 @@ fn main()"),
            HunkMeta {
                section: Some("fn main()".into()),
                added: 0,
                deleted: 0,
            }
        );
    }

    #[test]
    fn parse_hunk_header_accepts_pure_deletion() {
        assert_eq!(
            parse_hunk_header("@@ -5,3 +0,0 @@"),
            HunkMeta {
                section: None,
                added: 0,
                deleted: 0,
            }
        );
    }

    #[test]
    fn parse_hunk_header_no_section() {
        assert_eq!(
            parse_hunk_header("@@ -1 +1 @@"),
            HunkMeta {
                section: None,
                added: 0,
                deleted: 0,
            }
        );
    }

    #[test]
    fn parse_hunk_header_malformed_returns_zeros() {
        assert_eq!(
            parse_hunk_header("not a hunk"),
            HunkMeta {
                section: None,
                added: 0,
                deleted: 0,
            }
        );
    }

    #[test]
    fn parse_diff_populates_hunks_with_counts() {
        let raw = "diff --git a/src/a.rs b/src/a.rs\n\
                   index 1111111 2222222 100644\n\
                   --- a/src/a.rs\n\
                   +++ b/src/a.rs\n\
                   @@ -1,3 +1,3 @@\n\
                    fn foo() {\n\
                   -    let x = 1;\n\
                   +    let x = 2;\n\
                    }\n\
                   @@ -5,3 +5,3 @@\n\
                   -fn bar() {}\n\
                   +fn bar() {\n\
                   +}\n";
        let parsed = parse_diff(raw, "src/a.rs", 'M');

        assert_eq!(parsed.header.hunks.len(), 2);
        // Hunk 1: `@@ -1,3 +1,3 @@`, body = Context, Removed, Added, Context.
        assert_eq!(parsed.header.hunks[0].added, 1);
        assert_eq!(parsed.header.hunks[0].deleted, 1);
        // Hunk 2: `@@ -5,3 +5,3 @@`, body = Removed, Added, Added.
        assert_eq!(parsed.header.hunks[1].added, 2);
        assert_eq!(parsed.header.hunks[1].deleted, 1);
    }

    #[test]
    fn parse_diff_rename_only() {
        let raw = "diff --git a/old.rs b/new.rs\n\
                   similarity index 92%\n\
                   rename from old.rs\n\
                   rename to new.rs\n";
        let parsed = parse_diff(raw, "new.rs", 'R');
        // Rename/similarity metadata is parsed from the diff header lines.
        assert_eq!(parsed.header.path, "new.rs");
        assert_eq!(parsed.header.old_path.as_deref(), Some("old.rs"));
        assert_eq!(parsed.header.rename_percent, Some(92));
        assert_eq!(parsed.header.status, 'R');
        assert!(!parsed.header.binary);
        assert!(parsed.lines.is_empty());
        // A single file, no hunks: no in-body section.
        assert_eq!(parsed.files.len(), 1);
        assert!(parsed.sections.is_empty());
    }

    #[test]
    fn parse_diff_binary() {
        let raw = "diff --git a/x.bin b/x.bin\n\
                   Binary files a/x.bin and b/x.bin differ\n";
        let parsed = parse_diff(raw, "x.bin", 'M');
        // Binary detection is parsed from the `Binary files ...` meta line.
        assert!(parsed.header.binary);
        assert!(parsed.lines.is_empty());
        assert_eq!(parsed.header.added, 0);
        assert_eq!(parsed.header.deleted, 0);
    }

    #[test]
    fn parse_diff_folder_splits_into_files() {
        let raw = "diff --git a/src/a.rs b/src/a.rs\n\
                   index 1111111 2222222 100644\n\
                   --- a/src/a.rs\n\
                   +++ b/src/a.rs\n\
                   @@ -1 +1 @@\n\
                   -old\n\
                   +new\n\
                   diff --git a/src/b.rs b/src/b.rs\n\
                   index 3333333 4444444 100644\n\
                   --- a/src/b.rs\n\
                   +++ b/src/b.rs\n\
                   @@ -2 +2 @@\n\
                   -foo\n\
                   +bar\n";
        let parsed = parse_diff(raw, "src", 'M');

        // Two files, each with its own File section and one Hunk section.
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].path, "src/a.rs");
        assert_eq!(parsed.files[1].path, "src/b.rs");
        assert_eq!(parsed.sections.len(), 4);
        assert_eq!(parsed.sections[0].kind, SectionKind::File);
        assert_eq!(parsed.sections[0].file, 0);
        assert_eq!(parsed.sections[1].kind, SectionKind::Hunk);
        assert_eq!(parsed.sections[1].hunk, Some(0));
        assert_eq!(parsed.sections[2].kind, SectionKind::File);
        assert_eq!(parsed.sections[2].file, 1);
        assert_eq!(parsed.sections[3].kind, SectionKind::Hunk);
        assert_eq!(parsed.sections[3].hunk, Some(0));

        // Body lines: each file's header row plus a hunk; file headers open
        // their file's section.
        let kinds: Vec<LineKind> = parsed.lines.iter().map(|l| l.kind).collect();
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
        assert_eq!(parsed.header.path, "src");
        assert_eq!(parsed.header.added, 2);
        assert_eq!(parsed.header.deleted, 2);
        assert!(parsed.header.hunks.is_empty());
    }

    #[test]
    fn parse_diff_adjacent_pair_word_runs() {
        let raw = "diff --git a/b.rs b/b.rs\n\
                   index 1 2 100644\n\
                   --- a/b.rs\n\
                   +++ b/b.rs\n\
                   @@ -1 +1 @@\n\
                   -foo\n\
                   +bar\n";
        let parsed = parse_diff(raw, "b.rs", 'M');
        let removed = &parsed.lines[1];
        let added = &parsed.lines[2];
        assert_eq!(removed.kind, LineKind::Removed);
        assert_eq!(added.kind, LineKind::Added);
        assert!(removed.word_runs.is_some());
        assert!(added.word_runs.is_some());
        // Both lines share the same run vector.
        assert_eq!(removed.word_runs, added.word_runs);
    }

    #[test]
    fn parse_diff_4_line_block_pairs_positionally() {
        // removed[0] pairs with added[0], removed[1] with added[1].
        let raw = "diff --git a/a.rs b/a.rs\n\
                   index 1 2 100644\n\
                   --- a/a.rs\n\
                   +++ b/a.rs\n\
                   @@ -1,4 +1,4 @@\n\
                   -aaa\n\
                   -bbb\n\
                   +AAA\n\
                   +BBB\n";
        let parsed = parse_diff(raw, "a.rs", 'M');
        let kinds: Vec<LineKind> = parsed.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::HunkHeader,
                LineKind::Removed,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
            ]
        );
        for line in &parsed.lines[1..] {
            assert!(line.word_runs.is_some(), "line should carry word runs");
        }
        // Positional pairing: first removed with first added, etc.
        assert_eq!(parsed.lines[1].word_runs, parsed.lines[3].word_runs);
        assert_eq!(parsed.lines[2].word_runs, parsed.lines[4].word_runs);
    }
}
