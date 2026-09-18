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

    // The initial snapshot is empty: the worker owns the full changed-file scan
    // and sends the real tree/diff on its own thread, so the UI starts without
    // blocking on git work whose cost grows with the size of the diff.
    let initial_snapshot: ArcSnapshot = Arc::new(Snapshot {
        empty,
        selected_diff: None,
        selected_highlight: Vec::new(),
        error: None,
        branch_state: crate::git::BranchState::default(),
        tree: Tree::build(Vec::new()),
    });

    let repo_root = std::env::current_dir()?;
    let base_ref = base_ref_from_revision(&revision, default);
    let worker = Worker::spawn(revision.clone(), empty, repo_root, base_ref, cli.theme)
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

    let result = input::run(&mut terminal, &mut app, &worker.snapshot_rx, &worker.image_rx);

    let restore = TerminalGuard::restore();
    drop(worker);
    match (result, restore) {
        (Ok(()), Err(e)) => Err(e),
        (r, _) => r,
    }
}

/// The ref the worker watches for live updates. For a `<base>...HEAD` revision
/// that is `<base>`, but only when it names a local branch (the watcher polls
/// `.git/refs/heads/<base>`); otherwise (e.g. a bare `HEAD`, or a base like
/// `origin/main`) the default branch.
fn base_ref_from_revision(revision: &str, default: Option<String>) -> Option<String> {
    if let Some((left, _)) = revision.split_once("...") {
        let left = left.trim();
        if !left.is_empty() && git::local_branch_exists(left) {
            return Some(left.to_string());
        }
    }
    default
}

fn resolve_revision(arg: Option<String>, default: Option<String>) -> String {
    match arg.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            let trunk = default.unwrap_or_else(|| "main".to_string());
            match git::current_branch() {
                Ok(Some(b)) if b == trunk => "HEAD".to_string(),
                _ => format!("{trunk}...HEAD"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_ref_prefers_local_branches_only() {
        // `origin/main` is a remote ref, not a local branch the watcher can
        // poll, so the default branch is used instead.
        assert_eq!(
            base_ref_from_revision("origin/main...HEAD", Some("main".into())),
            Some("main".into())
        );
        assert_eq!(
            base_ref_from_revision("main...HEAD", Some("develop".into())),
            Some("main".into())
        );
        assert_eq!(
            base_ref_from_revision("HEAD", Some("main".into())),
            Some("main".into())
        );
    }
}
