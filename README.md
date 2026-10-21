# bough

Diff each git worktree against main — a terminal review tool for parallel (AI)
work.

`bough` is a terminal UI for reviewing changes in a git repository: a
syntax-highlighted diff on the left, a tree of changed files on the right, and
both kept current as you edit, commit, or switch worktrees. Git runs in-process
through `gix`, so no `git` binary is needed at runtime.

## Why bough

AI agents often work in several git worktrees at once. The question that
matters is not what an agent said it did — it is what actually changed relative
to main. `bough` is built around exactly that:

- **Default view is `<default-branch>...HEAD`** — the merge-base diff, i.e. the
  worktree's changes on top of main.
- **`w` switches between the worktrees** you are reviewing, with dirty-change
  counts and your view state restored in each one.
- **Live updates.** The view refreshes as the agent writes — debounced, not
  manual.
- **Re-aim any view** with `m` (uncommitted only) or `b` / `t` (another branch,
  commit, or tag).

## Features

**Review**

- **Changed-file tree** with directory rollups, per-file `+N −N` stats,
  status-coloured names, and a per-status count summary in the pane header.
- **Diff pane** with syntax highlighting (`syntect`/`two-face`, `--theme`),
  word-level diffs, hunk folding, sticky file/hunk headers, adjustable context,
  and rename %, mode-change, LFS and binary notes.
- **Compare against anything** — the default branch, or any branch, commit, or
  tag, from filterable pickers.

**Worktrees**

- **Live worktree switching** (`w`) with dirty-change counts, activity
  ordering, and per-worktree view memory (mode, selection, expanded
  directories, scroll).

**Live and interactive**

- **Live updates** from a filesystem watcher, with a ref-polling backstop.
- **Full mouse support**: click to select and fold, drag to select diff lines
  (release copies), wheel to scroll, click footer pills to change mode/source.
- **OSC52 copy** (works over SSH): dragged diff lines, or a file's
  repo-relative path via right-click.
- **Image previews** for binary images through the kitty graphics protocol;
  auto-disabled on terminals that do not support it.
- Works in repos with no commits yet, and in linked worktrees.

## Install

```sh
cargo install --path .

# or build a release binary
cargo build --release   # -> target/release/bough
```

Requires a recent stable Rust toolchain and a C compiler (Oniguruma is built
from source). Unix only — the input reader and kitty probe use `libc`.

## Usage

```sh
bough                              # changes since the merge-base of the default branch and HEAD
bough main...HEAD                  # explicit revision range
bough --worktree feature-x         # open a worktree by id (or path) instead of the cwd
bough --worktree feature-x v1      # ...and diff it against a tag
bough --theme list                 # list themes and exit
bough --theme "Monokai Extended"   # pick a theme
```

Run it inside a git repository; otherwise `bough` prints
`not a git repository.` and exits 1.

The default comparison is `<default-branch>...HEAD`, where the default branch
is `origin/HEAD` when set, otherwise a local `main` or `master`, otherwise
`main`. On the trunk itself the merge-base is `HEAD`, so the view reduces to
uncommitted changes.

## Keys

| Key | Scope | Action |
|---|---|---|
| `q`, `Esc`, `Ctrl-C` | Any | Quit (`Ctrl-C` also works while a picker is open) |
| `Tab` | Any | Toggle focus between tree and diff |
| `m` | Any | Switch between branch changes and uncommitted changes |
| `b` / `t` / `w` | Any | Branch / commit-and-tag / worktree picker |
| `W` | Any | Toggle word-level diff |
| `z a` / `z o` / `z c` | Any | Fold hunk: toggle / open / close |
| `z r` or `z R` | Any | Expand all hunks |
| `z m` or `z M` | Any | Collapse all hunks |
| `g g` / `G` | Any | First / last chunk |
| `Shift-H` `Shift-J` `Shift-K` `Shift-L` | Any | Scroll the diff |
| `Space` | Both | Tree: toggle dir or load diff · Diff: toggle hunk |
| `Enter` | Both | Tree: expand dir or reload diff · Diff: toggle hunk |
| `j` `k` / `↓` `↑` | Both | Tree: move · Diff: previous/next chunk |
| `h` `l` / `←` `→` | Both | Tree: collapse / expand · Diff: horizontal scroll |
| `PgUp` `PgDn` `Home` `End` | Tree | Page and jump |
| `Backspace` | Tree | Collapse directory |
| `{` `}` | Diff | Jump to chunk start / end |
| `[` `]` | Diff | Decrease / increase context lines |

In a picker: `j`/`k` navigate, typing filters, `Enter` selects, `Backspace`
edits the filter. `Esc` clears a non-empty filter first and closes on the next
press; with an empty filter `q`, `b`, `t`, and `w` also close it.

## Environment

| Variable | Effect |
|---|---|
| `BOUGH_NO_IMAGES` | Disable kitty image previews (any value except `0`) |
| `BOUGH_PERF_LOG=<path>` | Write opt-in performance events to the file |
| `BOUGH_CRASH_TEST` | Debug builds only: force a panic to test terminal restore |

## Development

```sh
cargo test
cargo clippy
cargo fmt --check
```

See [AGENTS.md](AGENTS.md) for architecture invariants and contribution rules.

## Notes

- Styling is Monokai-oriented; `docs/monokai-classic.md` records the background
  ramp the diff headers are tuned against.
- `Cargo.lock` is tracked on purpose — this is a binary crate.

## License

MIT — see [LICENSE](LICENSE).
