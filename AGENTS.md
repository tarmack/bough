# AGENTS.md

Rules for working on `bough`, a Rust TUI that shows a git branch's or
worktree's changes as a file tree beside a syntax-highlighted diff. Primary
workflow: reviewing one or more worktrees against the main branch — typically
AI-agent output. Binary crate: `src/main.rs` is the entry point, there is no
`lib.rs`.

## Verify before claiming done

```sh
cargo test                 # 428 tests, inline #[cfg(test)] modules
cargo clippy               # default lints, no clippy.toml
cargo fmt --check          # default rustfmt, no rustfmt.toml
```

There is no CI config, no lint attributes, and no `tests/` or `benches/`
directory. One test is `#[ignore]`d (a perf harness); run it deliberately:

```sh
cargo test --release directory_chunk_cost -- --ignored --nocapture
```

## How the app is built

Two threads split the work:

- **The diff pipeline runs on the worker thread** (`src/worker.rs`). Scans,
  diffs, highlighting, and preview decoding all happen there. The worker sends
  immutable `ArcSnapshot`s over a crossbeam channel and the UI just swaps in
  the latest. Keep `Snapshot` pure data.
- **Short, synchronous git reads run on the UI thread**: the branch / commit /
  tag / worktree pickers (`b`, `t`, `w`) and startup git. The worktree
  dirty-count scan runs on a third `bough-dirty` thread. Do not push expensive
  git work onto the UI thread.

## Architecture invariants

- **`Highlighter` is single-threaded.** It is created once on the main thread
  and moved into the worker; it is `Send`, but syntect is `&mut self`, so it
  must only ever be *used* on the worker thread. Never share it between threads
  or move it into `App` or the view.
- **`ResumeState` requires contiguous lines.** It resumes syntect's multi-line
  scope stack; feeding gapped hunk bodies silently drifts highlighting. If
  `parse_only` fails, do not cache the checkpoint.
- **Preview escapes go after `terminal.draw`.** Kitty paints at the cursor, so
  writing image escapes before the draw lets ratatui overwrite them. Every
  image must be deleted on exit via `preview::cleanup` (wired into the terminal
  guard).
- **`PANE_DIVIDER_W` is shared** between layout (`src/view/mod.rs`) and mouse
  routing (`src/input.rs`). If they disagree, clicks land in the wrong pane.
- **The render layer clamps scroll** (`tree_scroll`, `diff_scroll`) as a side
  effect of drawing. That is deliberate — don't move it into event handling.
  Selection clamping lives in navigation code, not in render.
- **`perf_evt!` arguments are always evaluated.** Keep them cheap — a path, an
  integer — never a scan or allocation. The enabled check runs after argument
  evaluation.

## Global state and threading

- **The repository root is process-global**: `static REPO_ROOT:
  OnceLock<RwLock<Option<PathBuf>>>` in `src/git/mod.rs`. `set_repo_root`
  canonicalizes the path, rejects anything containing a `.git` component, and
  requires the path to be inside a checked-out worktree.
- **Repo handles do not survive a worktree switch.** `set_repo_root` re-aims
  the global root; handles that embed a repo (e.g. the `IgnoreMatcher` in
  `WorkerState`) are rebuilt on switch. Never reuse one across switches. The
  process cwd never changes.
- **`--worktree` is resolved before any other git call**, on purpose — it must
  aim the root before the first snapshot.
- **Tests that touch the global root** must hold `test_support::GIT_ROOT_LOCK`
  and use `clear_repo_root_for_tests`, or they race under `cargo test`'s
  default parallelism.
- **The terminal is restored by a guard** that is idempotent and
  mutex-serialised; the panic hook restores from any thread then exits 101.
  Keep restore steps best-effort: run them all, return the first error.
- **`src/input.rs` reads events on a dedicated `bough-input` thread** because
  crossterm's event source spins forever once the pty closes; terminal liveness
  is detected separately. Preserve that split.

## Error handling

- Hand-rolled errors: `struct XError { pub message: String }` with `Display`
  and `std::error::Error`. No `anyhow`, no `thiserror`.
- Startup failures print `bough: <message>` to stderr and exit 1. Everything
  else degrades gracefully: per-file git errors never blank the tree, an
  unknown theme warns and continues, and `branch_state()` is total.
- Unwraps are fine after a checked invariant. There is no lint forbidding them
  and no blanket rule either.

## Testing

- Inline tests at the bottom of the module they test.
- Reuse `crate::test_support` fixtures (`sample_app`, `sample_diff`,
  `sample_snapshot`, `wtinfo`, …).
- Real-repo tests use `tempfile::tempdir()` and author commits with the `git`
  CLI (`gix` cannot author commits through this API), then read with gix.
- Render tests use `ratatui::backend::TestBackend` and assert buffer cells.
- Reproduce UI bugs at the state level first (selection, folds, snapshot
  sequence) — most regressions were lifecycle bugs, not layout bugs.

## Gotchas

- **The watcher's degraded flag is shared.** When `notify` fails, a poll mode
  is enabled through an `Arc<AtomicBool>` the UI already holds; re-arming must
  reuse that Arc, never mint a new one.
- **`Watcher::drop` waits 2 s, then leaks the thread** rather than block exit.
  Keep it that way.
- **Kitty `probe()` reads stdin directly** with `libc::poll` and a 250 ms
  budget, and must consume the trailing DA1 reply so it does not leak into
  crossterm.
- **Picker `entries[i]` ↔ `worktrees[i]` alignment is structural.** Worktree
  switching is two-phase (save state, send `SwitchWorktree`, stash target,
  restore in `apply_snapshot`); a same-worktree switch must stay a visual
  no-op.
- **Worker limits are intentional**: 32-entry diff LRU, 25-file directory
  chunks, 32 MiB / 16384 px / 64 MiB preview caps. Debounce is 150–400 ms by
  default, but the adaptive floor can grow up to 30 s under load. Do not raise
  caps without measuring (see `src/perf.rs`).
- **`Snapshot` is built from struct literals** in several places; adding a
  field means updating every constructor.

## Style

- Every file opens with a `//!` doc explaining responsibility and non-obvious
  invariants; aim to give public items `///` docs. Explain *why*, not what.
- Imports: `std`, blank line, external crates, blank line,
  `crate::`/`super::`.
- `src/app.rs` uses `// ── Section ───` dividers; keep new code inside the
  relevant section.
- `src/git/mod.rs` re-exports its public items, so moved types keep their old
  paths compiling.
- Prefer editing an existing module over adding a new one; extract only when
  the file is unmanageable or the unit is independently testable.
- Match the surrounding code. Beyond clippy and rustfmt there is no enforcer.

## Repo hygiene

- `Cargo.lock` is tracked intentionally (binary crate) — commit it.
- `target/` is large build output; never stage it.
- Do not add `.opencode/` or the `.codegraph` symlink — local tooling state.
- `docs/monokai-classic.md` is a palette reference, not a theme-file spec.
- Feature commits use `bough: <imperative summary>`, with a body listing
  behaviour changes and ending in test/lint status (e.g. "428 tests, clippy and
  fmt clean"). Cross-cutting and documentation commits use a plain sentence
  summary instead.
