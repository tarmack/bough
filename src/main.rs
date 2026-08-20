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
mod snapshot;
#[cfg(test)]
mod test_support;
mod text;
mod theme;
mod tree;
mod view;
mod watch;
mod worker;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
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

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.theme == "list" {
        for t in Highlighter::themes() {
            println!("{t}");
        }
        std::process::exit(0);
    }

    if let Err(HighlightError::UnknownTheme { requested, available }) = Highlighter::new(&cli.theme)
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

    let revision = resolve_revision(cli.revision.clone());

    let entries = if empty {
        match git::listing_empty() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("bough: {e}");
                std::process::exit(1);
            }
        }
    } else {
        match git::changed_files(&revision) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("bough: {e}");
                std::process::exit(1);
            }
        }
    };

    let stats: HashMap<String, (usize, usize)> = if empty {
        let paths: Vec<String> = entries.iter().map(|(_, p)| p.clone()).collect();
        git::empty_stats(&paths)
    } else {
        match git::diff_stats(&revision) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bough: {e}");
                std::process::exit(1);
            }
        }
    };

    let mut tree = Tree::build(entries);
    tree.annotate_stats(&stats);

    let initial_snapshot: ArcSnapshot = Arc::new(Snapshot {
        empty,
        selected_diff: None,
        selected_highlight: Vec::new(),
        error: None,
        branch_state: crate::git::BranchState::default(),
        tree,
    });

    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base_ref = git::default_branch();
    let worker = Worker::spawn(revision, empty, repo_root, base_ref, cli.theme, initial_snapshot.tree.clone())
        .unwrap_or_else(|e| {
            eprintln!("bough: worker spawn failed: {e}");
            std::process::exit(1);
        });

    let mut app = App::new(initial_snapshot, worker.request_tx.clone(), worker.poll_mode.load(std::sync::atomic::Ordering::Relaxed));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = input::run(&mut terminal, &mut app, &worker.snapshot_rx);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    drop(worker);
    result
}

fn resolve_revision(arg: Option<String>) -> String {
    match arg.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            let trunk = git::default_branch().unwrap_or_else(|| "main".to_string());
            match git::current_branch() {
                Ok(Some(b)) if b == trunk => "HEAD".to_string(),
                _ => format!("{trunk}...HEAD"),
            }
        }
    }
}
