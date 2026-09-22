//! bough — a minimalist terminal tree-diff viewer.
//!
//! This module is the entry point: CLI parsing, git bootstrap, terminal setup,
//! and the main loop. Responsibilities are split into:
//! - `app` — application state and behavior
//! - `input` — event loop and key/mouse dispatch
//! - `view` — layout and rendering (diff + tree panes)
//! - `theme`, `text` — shared colors and span helpers
//! - `tree`, `diff`, `git`, `highlight` — data / engines
//! - `snapshot`, `worker`, `watch` — live-update machinery

mod app;
mod diff;
mod git;
mod highlight;
mod input;
mod kitty;
mod preview;
mod snapshot;
#[cfg(test)]
mod test_support;
mod text;
mod theme;
mod tree;
mod view;
mod watch;
mod worker;

use std::io;
use std::sync::Arc;

use clap::Parser;
use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use highlight::{HighlightError, Highlighter};
use snapshot::{ArcSnapshot, Snapshot};
use tree::Tree;
use worker::Worker;

#[derive(Parser)]
#[command(name = "bough", about = "Minimalist terminal tree-diff viewer")]
struct Cli {
    /// Git revision to diff against (e.g. `main...`, `main..HEAD`).
    /// Defaults to the merge-base of `main` and `HEAD`.
    revision: Option<String>,
    /// syntect theme name (default "Monokai Extended"). Use `--theme list` to
    /// print available themes and exit.
    #[arg(long, default_value = "Monokai Extended")]
    theme: String,
}

/// Restores the terminal on every exit path (normal exit, `?` early return, or
/// panic). Restoration is best-effort: every step runs even if an earlier one
/// fails, and the first error is returned. Dropping the guard repeats the
/// sequence, which is harmless because each step is idempotent.
struct TerminalGuard;

impl TerminalGuard {
    fn restore() -> io::Result<()> {
        let mut first_err = None;
        if let Err(e) = disable_raw_mode() {
            first_err = Some(e);
        }
        let mut out = io::stdout();
        // Delete every kitty image (all placements), ignoring the reply.
        let _ = preview::cleanup(&mut out);
        if let Err(e) = execute!(out, LeaveAlternateScreen, DisableMouseCapture, Show) {
            first_err.get_or_insert(e);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = Self::restore();
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.theme == "list" {
        for t in Highlighter::themes() {
            println!("{t}");
        }
        std::process::exit(0);
    }

    if let Err(HighlightError::UnknownTheme {
        requested,
        available,
    }) = Highlighter::new(&cli.theme)
    {
        eprintln!("bough: unknown theme {requested:?}. Available:");
        for t in available {
            eprintln!("  {t}");
        }
        eprintln!("bough: continuing without syntax highlighting.");
    }

    let empty = match git::repo_state() {
        git::RepoState::NotARepo => {
            eprintln!("bough: not a git repository.");
            std::process::exit(1);
        }
        git::RepoState::NoCommits => true,
        git::RepoState::Ready => false,
    };

    let default = git::default_branch();
    let revision = resolve_revision(cli.revision.clone(), default.clone());

    // Resolve branch state once up front: the uncommitted-view decision and the
    // worker's first snapshot both read it, so the startup rev-walks must not
    // run twice. Later recomputes refresh it in place.
    let branch_state = git::branch_state(&revision);

    // The initial snapshot is empty: the worker owns the full changed-file scan
    // and sends the real tree/diff on its own thread, so the UI starts without
    // blocking on git work whose cost grows with the size of the diff. It
    // carries the resolved branch state so the first frame can show it.
    let initial_snapshot = initial_snapshot(empty, branch_state.clone());

    let repo_root = std::env::current_dir()?;
    let worker = Worker::spawn(
        revision.clone(),
        empty,
        repo_root,
        cli.theme,
        branch_state.clone(),
    )
    .unwrap_or_else(|e| {
        eprintln!("bough: worker spawn failed: {e}");
        std::process::exit(1);
    });

    let mut app = App::new(
        initial_snapshot,
        worker.request_tx.clone(),
        worker.poll_mode.load(std::sync::atomic::Ordering::Relaxed),
        revision.clone(),
        "HEAD".to_string(),
    );
    // A degenerate comparison (no committed work since the origin: standing
    // on the trunk, or a branch still at the trunk's tip) is the uncommitted
    // view — start there instead of showing a branch view with no branch
    // work in it. The worker keeps the named revision: its content is the
    // same, and the label stays honest.
    if starts_uncommitted(&revision, branch_state.ahead) {
        app.mode = crate::app::Mode::Uncommitted;
    }

    enable_raw_mode()?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = TerminalGuard::restore();
        previous_hook(info);
    }));
    let _guard = TerminalGuard;
    let images = kitty::probe();
    app.set_preview_enabled(images);
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = input::run(
        &mut terminal,
        &mut app,
        &worker.snapshot_rx,
        &worker.image_rx,
    );

    let restore = TerminalGuard::restore();
    drop(worker);
    match (result, restore) {
        // A clean run still exits 0 when restore fails: the terminal is
        // usually already gone (EOF), and `Drop` retries best-effort anyway.
        (Ok(()), Err(e)) => {
            eprintln!("bough: terminal restore failed: {e}");
            Ok(())
        }
        (r, _) => r,
    }
}

/// The pre-scan snapshot the UI opens with: no tree or diff yet, but the
/// already-resolved branch state so the first frame can show branch info before
/// the worker's first snapshot lands.
fn initial_snapshot(empty: bool, branch_state: crate::git::BranchState) -> ArcSnapshot {
    Arc::new(Snapshot {
        seq: 0,
        busy: false,
        empty,
        selected_diff: None,
        selected_highlight: Arc::new(Vec::new()),
        error: None,
        branch_state,
        uncommitted_stats: None,
        tree: Arc::new(Tree::build(Vec::new())),
    })
}

fn resolve_revision(arg: Option<String>, default: Option<String>) -> String {
    match arg.filter(|s| !s.is_empty()) {
        Some(s) => s,
        // Always name the trunk as the origin — even when standing on it.
        // `main...HEAD` degenerates gracefully there (the merge-base is
        // `HEAD` itself, so the view reduces to uncommitted edits), while a
        // bare `HEAD` would leave the origin unnamed: the footer would read
        // `vs HEAD` and picking `main` could not canonicalize back to the
        // default view.
        None => format!("{}...HEAD", default.unwrap_or_else(|| "main".to_string())),
    }
}

/// True when the startup revision contains no committed work (nothing ahead
/// of the origin, or an explicit comparison against `HEAD` itself), making
/// the branch view identical to the uncommitted view.
fn starts_uncommitted(revision: &str, ahead: Option<u32>) -> bool {
    ahead == Some(0) || revision == "HEAD"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_revision_always_names_the_trunk() {
        assert_eq!(
            resolve_revision(None, Some("main".to_string())),
            "main...HEAD"
        );
        // Even standing on the trunk, and when no default can be detected.
        assert_eq!(resolve_revision(None, None), "main...HEAD");
        // An explicit revision passes through verbatim.
        assert_eq!(
            resolve_revision(Some("v1.2...HEAD".to_string()), Some("main".to_string())),
            "v1.2...HEAD"
        );
    }

    #[test]
    fn starts_uncommitted_when_nothing_ahead_of_origin() {
        // Standing on the trunk (or a branch still at its tip): the branch
        // view has no committed work.
        assert!(starts_uncommitted("main...HEAD", Some(0)));
        // An explicit comparison against HEAD is the uncommitted view.
        assert!(starts_uncommitted("HEAD", None));
        // Real branch work, or an unknown ahead count, stays a branch view.
        assert!(!starts_uncommitted("main...HEAD", Some(3)));
        assert!(!starts_uncommitted("v1.2...HEAD", None));
    }

    #[test]
    fn initial_snapshot_carries_resolved_branch_state() {
        let state = crate::git::BranchState {
            name: Some("feature/x".into()),
            base: Some("main".into()),
            ahead: Some(2),
            ..Default::default()
        };
        let snap = initial_snapshot(false, state);
        assert_eq!(snap.branch_state.name.as_deref(), Some("feature/x"));
        assert_eq!(snap.branch_state.base.as_deref(), Some("main"));
        assert_eq!(snap.branch_state.ahead, Some(2));
    }
}
