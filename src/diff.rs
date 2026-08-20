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
}

/// A single body line in a parsed diff.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    /// How this line should be rendered.
    pub kind: LineKind,
    /// The line with its leading `+`/`-`/` ` prefix stripped. Hunk headers
    /// keep the full `@@ ... @@` text.
    pub raw_text: String,
    /// 0-based index of the hunk this line belongs to; increments on each
    /// `@@` line.
    pub hunk_index: usize,
    /// Word-level runs, set on Added/Removed lines when LCS word-diff was
    /// applied to the adjacent pair; `None` otherwise. The same shared vector
    /// is assigned to both members of a paired removed/added block.
    pub word_runs: Option<Vec<WordRun>>,
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

/// The parsed form of one file's `git diff`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDiff {
    pub header: FileHeader,
    /// Classified body lines. File-metadata lines are excluded.
    pub lines: Vec<DiffLine>,
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

/// Parse raw `git diff` output for a single file into a [`ParsedDiff`].
///
/// `path` and `status` come from the caller's tree data; `path` is the
/// fallback when the diff header does not name a (renamed) new path.
pub fn parse_diff(raw: &str, path: &str, status: char) -> ParsedDiff {
    let mut header = FileHeader {
        path: path.to_string(),
        old_path: None,
        status,
        added: 0,
        deleted: 0,
        rename_percent: None,
        mode_old: None,
        mode_new: None,
        binary: false,
        hunks: Vec::new(),
    };

    let mut lines: Vec<DiffLine> = Vec::new();
    let mut hunk_count = 0usize;

    for raw_line in raw.lines() {
        if is_meta_line(raw_line) {
            continue;
        }
        let (kind, raw_text) = classify(raw_line);
        // A hunk header opens the next hunk; its body lines belong to it. So
        // a header gets the pre-increment count, subsequent lines get the
        // current (incremented) count minus one.
        let hunk_index = if kind == LineKind::HunkHeader {
            hunk_count
        } else {
            hunk_count.saturating_sub(1)
        };
        if kind == LineKind::HunkHeader {
            header.hunks.push(parse_hunk_header(raw_line));
            hunk_count += 1;
        }
        lines.push(DiffLine {
            kind,
            raw_text,
            hunk_index,
            word_runs: None,
        });
        // Accumulate per-hunk body counts; body lines index the hunk opened by
        // the preceding `@@` header.
        if let Some(hunk) = header.hunks.get_mut(hunk_index) {
            match kind {
                LineKind::Added => hunk.added += 1,
                LineKind::Removed => hunk.deleted += 1,
                _ => {}
            }
        }
    }
    apply_word_pairs(&mut lines);

    ParsedDiff { header, lines }
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
        // File-level `added`/`deleted` are supplied by the caller (`git::FileMeta`);
        // `parse_diff` only accumulates them per-hunk.
        assert_eq!(parsed.header.added, 0);
        assert_eq!(parsed.header.deleted, 0);
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

        // hunk_index resets to 0 for the first hunk and becomes 1 after the
        // second `@@`.
        let indexes: Vec<usize> = parsed.lines.iter().map(|l| l.hunk_index).collect();
        assert_eq!(indexes, vec![0, 0, 0, 0, 0, 1, 1, 1, 1]);

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
        // Rename/similarity metadata is supplied by the caller (`git::FileMeta`);
        // the shadow parser no longer extracts it from the diff header.
        assert_eq!(parsed.header.path, "new.rs");
        assert_eq!(parsed.header.old_path, None);
        assert_eq!(parsed.header.rename_percent, None);
        assert!(!parsed.header.binary);
        assert!(parsed.lines.is_empty());
    }

    #[test]
    fn parse_diff_binary() {
        let raw = "diff --git a/x.bin b/x.bin\n\
                   Binary files a/x.bin and b/x.bin differ\n";
        let parsed = parse_diff(raw, "x.bin", 'M');
        // Binary detection is supplied by the caller (`git::FileMeta`); the
        // `Binary files ...` line is treated as file metadata and filtered out
        // of the body (consistent with the other header lines).
        assert!(!parsed.header.binary);
        assert!(parsed.lines.is_empty());
        assert_eq!(parsed.header.added, 0);
        assert_eq!(parsed.header.deleted, 0);
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
