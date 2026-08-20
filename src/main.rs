//! bough — a minimalist terminal tree-diff viewer.

mod diff;
mod git;
mod highlight;
mod tree;

use diff::{DiffLine, FileHeader, HunkMeta, LineKind, ParsedDiff, WordRun};
use highlight::{HighlightError, Highlighter, TokenOwned};

use std::collections::HashSet;
use std::io::{self, Stdout};

use clap::Parser;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::{Color, Style},
    text::{Line, Span},
    Frame, Terminal,
};
use tree::Tree;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Diff,
}

struct App {
    rev: String,
    empty: bool,
    tree: Tree,
    flat: Vec<usize>,
    selection: usize,
    tree_scroll: usize,
    current_file: Option<String>,
    parsed: Option<ParsedDiff>,
    /// Per-body-line syntax tokens, cached when `parsed` is set. Indexed
    /// in lockstep with `parsed.lines`. HunkHeader slots hold the
    /// highlighted hunk section name (or empty when no grammar/no section).
    /// Empty Vec for grammar-miss lines or when no highlighter is
    /// available. Pre-computed top-to-bottom so syntect's
    /// multi-line state (block comments, strings) carries correctly across
    /// the whole file, and rendering does not depend on scroll position.
    highlighted: Vec<Vec<TokenOwned>>,
    diff_error: Option<String>,
    diff_scroll: usize,
    word_diff_on: bool,
    highlighter: Option<Highlighter>,
    last_header_height: u16,
    diff_width: u16,
    height: usize,
    focus: Focus,
    diff_col: usize,
    collapsed_hunks: HashSet<usize>,
    context_lines: u32,
    /// Pending `z`-prefix fold chord (vim fold controls).
    fold_key: bool,
    /// Pending `g`-prefix navigation chord (vim `gg` -> first chunk).
    g_key: bool,
    /// `parsed.lines` index of the sticky pinned hunk header (the one scrolled
    /// past the top of the view, rendered at body row 0). `None` when none.
    sticky_line: Option<usize>,
    /// Cursor for the current-chunk highlight: the chunk selected by chunk
    /// navigation (`j`/`k`), folding, or clicks. `None` before a chunk is
    /// selected.
    active_chunk: Option<usize>,
}

impl App {
    fn new(rev: String, tree: Tree, empty: bool, highlighter: Option<Highlighter>) -> Self {
        let flat = tree.flatten();
        let mut app = App {
            rev,
            empty,
            tree,
            flat,
            selection: 0,
            tree_scroll: 0,
            current_file: None,
            parsed: None,
            highlighted: Vec::new(),
            diff_error: None,
            diff_scroll: 0,
            word_diff_on: true,
            highlighter,
            last_header_height: 1,
            diff_width: 0,
            height: 0,
            focus: Focus::Tree,
            diff_col: 0,
            collapsed_hunks: HashSet::new(),
            context_lines: 3,
            fold_key: false,
            g_key: false,
            sticky_line: None,
            active_chunk: None,
        };
        app.load_diff();
        app
    }

    fn selected_node(&self) -> Option<usize> {
        self.flat.get(self.selection).copied()
    }

    /// Load a fresh diff for the selected file. Each file switch starts a
    /// clean pane: no horizontal scroll, nothing collapsed, default context.
    fn load_diff(&mut self) {
        self.diff_col = 0;
        self.collapsed_hunks.clear();
        self.context_lines = 3;
        self.load_diff_with_ctx();
    }

    /// Load the selected file using `self.context_lines` context. Called on
    /// file switch (via `load_diff`) and on `[`/`]` context changes.
    fn load_diff_with_ctx(&mut self) {
        match self.selected_node() {
            Some(idx) if self.tree.nodes[idx].kind == tree::Kind::File => {
                let path = self.tree.nodes[idx].path.clone();
                self.current_file = Some(path.clone());
                let raw = if self.empty {
                    git::file_diff_empty_ctx(&path, self.context_lines)
                } else {
                    git::file_diff_ctx(&self.rev, &path, self.context_lines)
                };
                let meta = if self.empty {
                    let added = git::empty_stats(std::slice::from_ref(&path))
                        .get(&path)
                        .map(|(a, _)| *a)
                        .unwrap_or(0);
                    Ok(git::FileMeta {
                        status: 'A',
                        added,
                        deleted: 0,
                        old_path: None,
                        rename_percent: None,
                        mode_old: None,
                        mode_new: None,
                        binary: false,
                    })
                } else {
                    git::file_meta(&self.rev, &path)
                };

                self.parsed = match (raw, meta) {
                    (Ok(raw), Ok(meta)) => {
                        let mut parsed = diff::parse_diff(&raw, &path, meta.status);
                        // Trust git metadata over the raw diff header fields.
                        parsed.header.path = path.clone();
                        parsed.header.added = meta.added;
                        parsed.header.deleted = meta.deleted;
                        parsed.header.old_path = meta.old_path;
                        parsed.header.rename_percent = meta.rename_percent;
                        parsed.header.mode_old = meta.mode_old;
                        parsed.header.mode_new = meta.mode_new;
                        parsed.header.binary = meta.binary;
                        self.diff_error = None;
                        Some(parsed)
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        self.diff_error = Some(e.to_string());
                        None
                    }
                };
                self.highlighted = match self.parsed.as_ref() {
                    Some(p) => Self::cache_highlights(p, &path, self.highlighter.as_mut()),
                    None => Vec::new(),
                };
                self.diff_scroll = 0;
                self.active_chunk = if self
                    .parsed
                    .as_ref()
                    .is_some_and(|p| !p.header.hunks.is_empty())
                {
                    Some(0)
                } else {
                    None
                };
            }
            _ => {
                self.current_file = None;
                self.parsed = None;
                self.highlighted.clear();
                self.diff_error = None;
                self.diff_scroll = 0;
                self.active_chunk = None;
            }
        }
    }

    /// Pre-compute the per-body-line syntax token cache for a parsed file.
    /// Walks lines top-to-bottom so syntect's multi-line state (block
    /// comments, strings) carries correctly within each hunk. Hunk headers
    /// reset the highlighter so no multi-line construct leaks across a hunk
    /// boundary, then hold the highlighted section name (or empty when no
    /// grammar matched / no section).
    fn cache_highlights(
        parsed: &ParsedDiff,
        path: &str,
        mut highlighter: Option<&mut Highlighter>,
    ) -> Vec<Vec<TokenOwned>> {
        let mut cache: Vec<Vec<TokenOwned>> = Vec::with_capacity(parsed.lines.len());
        for line in &parsed.lines {
            if line.kind == LineKind::HunkHeader {
                let section = parsed
                    .header
                    .hunks
                    .get(line.hunk_index)
                    .and_then(|m| m.section.as_deref());
                match highlighter.as_deref_mut() {
                    Some(h) => {
                        h.reset();
                        cache.push(match section {
                            Some(sec) => h.line(path, sec),
                            None => Vec::new(),
                        });
                    }
                    None => cache.push(Vec::new()),
                }
                continue;
            }
            match highlighter.as_deref_mut() {
                Some(h) => cache.push(h.line(path, &line.raw_text)),
                None => cache.push(Vec::new()),
            }
        }
        cache
    }

    fn move_down(&mut self, n: usize) {
        if self.flat.is_empty() {
            return;
        }
        let last = self.flat.len() - 1;
        self.selection = self.selection.saturating_add(n).min(last);
        self.load_diff();
    }

    fn move_up(&mut self, n: usize) {
        self.selection = self.selection.saturating_sub(n);
        self.load_diff();
    }

    fn collapse(&mut self) {
        let idx = match self.selected_node() {
            Some(i) => i,
            None => return,
        };
        let node = &mut self.tree.nodes[idx];
        if node.kind == tree::Kind::Dir && node.expanded {
            node.expanded = false;
        } else {
            return;
        }

        self.flat = self.tree.flatten();
        if let Some(pos) = self.flat.iter().position(|&x| x == idx) {
            self.selection = pos;
        }
        self.diff_scroll = 0;
    }

    fn expand_or_refresh(&mut self) {
        let idx = match self.selected_node() {
            Some(i) => i,
            None => return,
        };
        let node = &mut self.tree.nodes[idx];
        if node.kind == tree::Kind::Dir {
            node.expanded = true;
            self.flat = self.tree.flatten();
        } else {
            self.load_diff();
        }
    }

    /// Number of body lines visible under the locked header.
    fn diff_page_size(&self) -> usize {
        self.height
            .saturating_sub(self.last_header_height as usize)
            .max(1)
    }

    fn diff_scroll_down(&mut self, n: usize) {
        if self.diff_fits() {
            self.move_active_chunk(n as i32);
            return;
        }
        let max = max_scroll_for(self.visible_line_indices().len(), self.diff_page_size());
        self.diff_scroll = self.diff_scroll.saturating_add(n).min(max);
    }

    fn diff_scroll_up(&mut self, n: usize) {
        if self.diff_fits() {
            self.move_active_chunk(-(n as i32));
            return;
        }
        self.diff_scroll = self.diff_scroll.saturating_sub(n);
    }

    /// True when the whole diff fits and there is nothing to scroll.
    fn diff_fits(&self) -> bool {
        self.visible_line_indices()
            .len()
            .saturating_sub(self.diff_page_size())
            == 0
    }

    /// Move the active-chunk selection by `delta` chunks (clamped). Used when the
    /// diff fits in the pane (no scrolling possible) so selection still moves.
    fn move_active_chunk(&mut self, delta: i32) {
        let max = self
            .parsed
            .as_ref()
            .map_or(0usize, |p| p.header.hunks.len().saturating_sub(1));
        let cur = self.active_chunk.unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, max as i32) as usize;
        self.active_chunk = Some(next);
    }

    /// Move the chunk cursor by `delta`: jump to the adjacent chunk when its
    /// header is already in view, otherwise scroll one line in the direction.
    /// Scrolling activates a chunk as soon as its header line enters the view.
    fn navigate_chunk(&mut self, delta: i32) {
        // 1. Jump to the adjacent chunk when its header is already in view.
        if let Some(cur) = self.active_chunk {
            let dest = cur as i32 + delta;
            let in_range = self
                .parsed
                .as_ref()
                .is_some_and(|p| dest >= 0 && (dest as usize) < p.header.hunks.len());
            if in_range && self.chunk_header_in_view(dest as usize) {
                self.active_chunk = Some(dest as usize);
                return;
            }
        }
        // 2. Otherwise scroll one line toward it.
        let before = self.diff_scroll;
        if delta > 0 {
            self.diff_scroll_down(1);
        } else {
            self.diff_scroll_up(1);
        }
        if self.diff_scroll == before {
            return; // at the end: nothing entered
        }
        // 3. Exemption: a header line entering the view activates its chunk.
        let entered = if delta > 0 {
            self.bottom_body_line()
        } else {
            self.top_body_line()
        };
        if let Some(li) = entered {
            if let Some(parsed) = self.parsed.as_ref() {
                if parsed.lines[li].kind == LineKind::HunkHeader {
                    self.active_chunk = Some(parsed.lines[li].hunk_index);
                }
            }
        }
    }

    /// Whether `chunk`'s hunk header is currently visible (in-flow window or the
    /// pinned sticky top header).
    fn chunk_header_in_view(&self, chunk: usize) -> bool {
        let Some(parsed) = self.parsed.as_ref() else {
            return false;
        };
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let end = start.saturating_add(self.diff_page_size());
        let sticky = visible.iter().take(start).rev().find_map(|&li| {
            (parsed.lines[li].kind == LineKind::HunkHeader).then_some(parsed.lines[li].hunk_index)
        });
        if sticky == Some(chunk) {
            return true;
        }
        let in_flow = visible[start..end.min(visible.len())].iter().any(|&li| {
            parsed.lines[li].kind == LineKind::HunkHeader && parsed.lines[li].hunk_index == chunk
        });
        in_flow
    }

    /// `parsed.lines` index of the top rendered body line: the sticky header if
    /// present, else the first in-flow line.
    fn top_body_line(&self) -> Option<usize> {
        let parsed = self.parsed.as_ref()?;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        if start > 0 {
            if let Some(li) = visible
                .iter()
                .take(start)
                .rev()
                .find(|&&li| parsed.lines[li].kind == LineKind::HunkHeader)
            {
                return Some(*li);
            }
        }
        visible.get(start).copied()
    }

    /// `parsed.lines` index of the last rendered body line.
    fn bottom_body_line(&self) -> Option<usize> {
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let win = self.diff_page_size();
        let rows = win.saturating_sub(if start > 0 { 1 } else { 0 }); // sticky row
        let last = start
            .saturating_add(rows)
            .min(visible.len())
            .saturating_sub(1);
        visible.get(last).copied()
    }

    fn diff_scroll_left(&mut self, n: usize) {
        self.diff_col = self.diff_col.saturating_sub(n);
    }

    fn diff_scroll_right(&mut self, n: usize) {
        let max = self
            .max_visible_line_width()
            .saturating_sub(self.diff_width as usize)
            .min(MAX_HSCROLL);
        self.diff_col = self.diff_col.saturating_add(n).min(max);
    }

    /// Approximate widest rendered line, in cells. Uses raw-text char count;
    /// good enough for scroll-clamp purposes at our scale.
    fn max_visible_line_width(&self) -> usize {
        self.parsed.as_ref().map_or(0, |p| {
            p.lines
                .iter()
                .map(|l| l.raw_text.chars().count())
                .max()
                .unwrap_or(0)
        })
    }

    /// Indices into `parsed.lines` that should render: every `HunkHeader`
    /// plus body lines whose hunk isn't collapsed. Empty when no diff.
    fn visible_line_indices(&self) -> Vec<usize> {
        let Some(parsed) = self.parsed.as_ref() else {
            return Vec::new();
        };
        parsed
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.kind == LineKind::HunkHeader || !self.collapsed_hunks.contains(&l.hunk_index)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Clamp `diff_scroll` so the last visible line can always be shown
    /// (accounting for the sticky header row consumed at any scroll > 0).
    fn clamp_diff_scroll(&mut self) {
        let max = max_scroll_for(self.visible_line_indices().len(), self.diff_page_size());
        self.diff_scroll = self.diff_scroll.min(max);
    }

    /// Collapse `hunk`, then snap the view so its header is at the top and keep
    /// it active (predictable when folding while scrolled down into it).
    fn collapse_hunk(&mut self, hunk: usize) {
        self.collapsed_hunks.insert(hunk);
        self.snap_view_to_chunk(hunk);
    }

    /// Expand `hunk`.
    fn expand_hunk(&mut self, hunk: usize) {
        self.collapsed_hunks.remove(&hunk);
        self.clamp_diff_scroll();
    }

    /// Toggle a chunk's collapsed state.
    fn toggle_hunk(&mut self, hunk: usize) {
        if self.collapsed_hunks.contains(&hunk) {
            self.expand_hunk(hunk);
        } else {
            self.collapse_hunk(hunk);
        }
    }

    /// Scroll so `hunk`'s header is the pinned sticky at the top and mark it
    /// active.
    fn snap_view_to_chunk(&mut self, hunk: usize) {
        let Some(parsed) = self.parsed.as_ref() else {
            return;
        };
        let visible = self.visible_line_indices();
        if let Some(pos) = visible.iter().position(|&li| {
            parsed.lines[li].kind == LineKind::HunkHeader && parsed.lines[li].hunk_index == hunk
        }) {
            self.diff_scroll = pos + 1;
        }
        self.active_chunk = Some(hunk);
        self.clamp_diff_scroll();
    }

    /// Position the view at the bottom of expanded chunk `dest` and mark it
    /// active, so moving up into a chunk shows it from its last line (its
    /// header stays pinned as the sticky at the top).
    fn enter_chunk_from_bottom(&mut self, dest: usize) {
        let Some(parsed) = self.parsed.as_ref() else {
            return;
        };
        let visible = self.visible_line_indices();
        let win = self.diff_page_size();
        if let Some(last) = visible
            .iter()
            .rposition(|&li| parsed.lines[li].hunk_index == dest)
        {
            // Put `dest`'s last visible line near the bottom of the viewport.
            self.diff_scroll = last.saturating_sub(win.saturating_sub(1));
        }
        self.active_chunk = Some(dest);
        self.clamp_diff_scroll();
    }

    /// Go to the first chunk and scroll it to the top (`gg`).
    fn go_first_chunk(&mut self) {
        self.snap_view_to_chunk(0);
    }

    /// Go to the last chunk and scroll it to the top (`G`).
    fn go_last_chunk(&mut self) {
        let max = self
            .parsed
            .as_ref()
            .map_or(0, |p| p.header.hunks.len().saturating_sub(1));
        self.snap_view_to_chunk(max);
    }

    /// Jump to the start (header) of the current chunk (`{`).
    fn go_chunk_start(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.snap_view_to_chunk(hunk);
        }
    }

    /// Jump to the end (last line) of the current chunk (`}`).
    fn go_chunk_end(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.enter_chunk_from_bottom(hunk);
        }
    }

    /// Toggle collapse on the currently-highlighted hunk (monotone cursor).
    fn toggle_hunk_at_cursor(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.toggle_hunk(hunk);
        }
    }

    /// The hunk whose header sits at diff-pane row `row`, if any (a hunk-header
    /// line). Respects the locked header and the sticky pinned header so clicks
    /// on a header toggle that hunk regardless of scroll position.
    fn hunk_at_screen_row(&self, row: u16) -> Option<usize> {
        let parsed = self.parsed.as_ref()?;
        let header_h = self.last_header_height as usize;
        if (row as usize) < header_h {
            return None; // clicked on the locked header, not a hunk
        }
        let body_row = (row as usize) - header_h;
        let visible = self.visible_line_indices();
        let start = self.diff_scroll;
        let line_idx = if let Some(sticky) = self.sticky_line {
            if body_row == 0 {
                return parsed.lines.get(sticky).map(|l| l.hunk_index);
            }
            visible.get(start + body_row - 1).copied()
        } else {
            visible.get(start + body_row).copied()
        }?;
        let line = parsed.lines.get(line_idx)?;
        (line.kind == LineKind::HunkHeader).then_some(line.hunk_index)
    }

    /// Open (uncollapse) the currently-highlighted chunk (monotone cursor).
    fn open_hunk_at_cursor(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.expand_hunk(hunk);
        }
    }

    /// Close (collapse) the currently-highlighted chunk (monotone cursor).
    fn close_hunk_at_cursor(&mut self) {
        if let Some(hunk) = self.active_chunk {
            self.collapse_hunk(hunk);
        }
    }

    /// Open every hunk.
    fn expand_all_hunks(&mut self) {
        self.collapsed_hunks.clear();
        self.clamp_diff_scroll();
    }

    /// Close every hunk.
    fn collapse_all_hunks(&mut self) {
        if let Some(parsed) = self.parsed.as_ref() {
            self.collapsed_hunks = (0..parsed.header.hunks.len()).collect();
        }
        self.clamp_diff_scroll();
    }

    /// Toggle expand/collapse of the focused tree dir (Space on a dir).
    fn toggle_dir(&mut self) {
        let idx = match self.selected_node() {
            Some(i) => i,
            None => return,
        };
        if self.tree.nodes[idx].kind != tree::Kind::Dir {
            return;
        }
        self.tree.nodes[idx].expanded = !self.tree.nodes[idx].expanded;
        self.flat = self.tree.flatten();
        if let Some(pos) = self.flat.iter().position(|&x| x == idx) {
            self.selection = pos;
        }
        self.diff_scroll = 0;
    }

    /// Space: toggle the selected tree dir / load the selected file diff, or
    /// toggle the active hunk when diff has focus.
    fn space_action(&mut self) {
        match self.focus {
            Focus::Tree => match self.selected_node() {
                Some(idx) if self.tree.nodes[idx].kind == tree::Kind::Dir => self.toggle_dir(),
                Some(_) => self.load_diff(),
                None => {}
            },
            Focus::Diff => self.toggle_hunk_at_cursor(),
        }
    }

    /// Grow (`delta > 0`) or shrink (`delta < 0`) context, clamped to
    /// `[0, 200]`. Re-fetches with a new `-U<N>` when the value changes.
    fn change_context(&mut self, delta: i32) {
        let new_ctx = (self.context_lines as i32 + delta).clamp(0, MAX_CONTEXT as i32) as u32;
        if new_ctx == self.context_lines {
            return;
        }
        self.context_lines = new_ctx;
        self.load_diff_with_ctx();
        self.clamp_diff_scroll();
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

    let highlighter = match Highlighter::new(&cli.theme) {
        Ok(h) => Some(h),
        Err(HighlightError::UnknownTheme {
            requested,
            available,
        }) => {
            eprintln!("bough: unknown theme {requested:?}. Available:");
            for t in available {
                eprintln!("  {t}");
            }
            eprintln!("bough: continuing without syntax highlighting.");
            None
        }
    };

    let empty = match git::repo_state() {
        git::RepoState::NotARepo => {
            eprintln!("bough: not a git repository.");
            std::process::exit(1);
        }
        git::RepoState::NoCommits => true,
        git::RepoState::Ready => false,
    };

    let revision = resolve_revision(cli.revision);

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

    let stats = if empty {
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
    let mut app = App::new(revision, tree, empty, highlighter);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

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

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        match event::read()? {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if handle_key(key, app) {
                    break;
                }
            }
            Event::Mouse(m) => handle_mouse(m, app),
            _ => {}
        }
    }
    Ok(())
}

fn handle_mouse(mouse: MouseEvent, app: &mut App) {
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
        // Left-click inside the diff pane on a hunk header toggles it.
        if mouse.column < app.diff_width {
            if let Some(hunk) = app.hunk_at_screen_row(mouse.row) {
                app.toggle_hunk(hunk);
                return;
            }
        }
        return; // clicks outside a toggleable header do nothing
    }
    match mouse.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {}
        _ => return,
    }
    let in_tree = mouse.column >= app.diff_width.saturating_add(1);
    let direction = matches!(mouse.kind, MouseEventKind::ScrollUp);
    if in_tree {
        if direction {
            app.move_up(1);
        } else {
            app.move_down(1);
        }
    } else if direction {
        app.diff_scroll_up(1);
    } else {
        app.diff_scroll_down(1);
    }
}

fn handle_key(key: KeyEvent, app: &mut App) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if app.fold_key {
        app.fold_key = false;
        return handle_fold_key(key.code, app);
    }
    if app.g_key {
        app.g_key = false;
        if key.code == KeyCode::Char('g') {
            app.go_first_chunk();
        }
        return false; // the prefix consumed this key
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('w') => {
            app.word_diff_on = !app.word_diff_on;
            return false;
        }
        KeyCode::Char('z') => {
            app.fold_key = true;
            return false;
        }
        KeyCode::Char('g') => {
            app.g_key = true;
            return false;
        }
        KeyCode::Char('G') => {
            app.go_last_chunk();
            return false;
        }
        KeyCode::Char(' ') => {
            app.space_action();
            return false;
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Tree => Focus::Diff,
                Focus::Diff => Focus::Tree,
            };
            return false;
        }
        _ => {}
    }

    // Shift overrides scroll the diff even from tree focus.
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Char('H') => app.diff_scroll_left(HSCROLL_STEP),
            KeyCode::Char('J') => app.diff_scroll_down(1),
            KeyCode::Char('K') => app.diff_scroll_up(1),
            KeyCode::Char('L') => app.diff_scroll_right(HSCROLL_STEP),
            _ => return focus_key(key, app),
        }
        return false;
    }

    focus_key(key, app)
}

/// Handle the second key of a `z`-prefixed fold chord (vim fold controls).
fn handle_fold_key(code: KeyCode, app: &mut App) -> bool {
    match code {
        KeyCode::Char('a') => app.toggle_hunk_at_cursor(),
        KeyCode::Char('o') => app.open_hunk_at_cursor(),
        KeyCode::Char('c') => app.close_hunk_at_cursor(),
        KeyCode::Char('R') | KeyCode::Char('r') => app.expand_all_hunks(),
        KeyCode::Char('M') | KeyCode::Char('m') => app.collapse_all_hunks(),
        _ => {}
    }
    false
}

fn focus_key(key: KeyEvent, app: &mut App) -> bool {
    match app.focus {
        Focus::Tree => match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                app.move_down(1);
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.move_up(1);
                false
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                app.collapse();
                false
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                app.expand_or_refresh();
                false
            }
            _ => false,
        },
        Focus::Diff => match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                app.navigate_chunk(1);
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.navigate_chunk(-1);
                false
            }
            KeyCode::Char('h') | KeyCode::Left => {
                app.diff_scroll_left(HSCROLL_STEP);
                false
            }
            KeyCode::Char('l') | KeyCode::Right => {
                app.diff_scroll_right(HSCROLL_STEP);
                false
            }
            KeyCode::Enter => {
                app.toggle_hunk_at_cursor();
                false
            }
            KeyCode::Char('{') => {
                app.go_chunk_start();
                false
            }
            KeyCode::Char('}') => {
                app.go_chunk_end();
                false
            }
            KeyCode::Char('[') => {
                app.change_context(-CONTEXT_STEP);
                false
            }
            KeyCode::Char(']') => {
                app.change_context(CONTEXT_STEP);
                false
            }
            _ => false,
        },
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 6 || area.height == 0 {
        return;
    }

    let tree_w = (area.width / 3).clamp(1, area.width.saturating_sub(2));
    let diff_w = area.width.saturating_sub(tree_w).saturating_sub(1);

    app.diff_width = diff_w;
    app.height = area.height as usize;

    let diff = ratatui::layout::Rect::new(area.x, area.y, diff_w, area.height);
    let tree = ratatui::layout::Rect::new(area.x + diff_w + 1, area.y, tree_w, area.height);

    render_diff(f, app, diff);
    render_tree(f, app, tree);
}

fn render_tree(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    // Keep the selection within the visible window.
    let visible = area.height as usize;
    if app.flat.is_empty() {
        let line = Line::styled("no changes", Style::default().fg(Color::DarkGray));
        f.buffer_mut().set_line(area.x, area.y, &line, area.width);
        return;
    }

    if app.selection > app.flat.len() - 1 {
        app.selection = app.flat.len() - 1;
    }
    if app.tree_scroll > app.selection {
        app.tree_scroll = app.selection;
    }
    if app.selection >= app.tree_scroll + visible && visible > 0 {
        app.tree_scroll = app.selection - visible + 1;
    }

    let end = (app.tree_scroll + visible).min(app.flat.len());
    for i in app.tree_scroll..end {
        let node_idx = app.flat[i];
        let selected = i == app.selection;
        let node = &app.tree.nodes[node_idx];
        let row_bg = if selected {
            if app.focus == Focus::Tree {
                BG_HUNK_ACTIVE
            } else {
                // Unfocused tree selection matches the inactive chunk header.
                BG_HUNK
            }
        } else {
            Color::Reset
        };
        let left = tree_left(app, node_idx, row_bg);
        let stat = stat_line(node, row_bg);
        let y = area.y + (i - app.tree_scroll) as u16;

        // Fill the whole row with the row background so the selection spans the
        // full line width, then draw the text on top.
        f.buffer_mut().set_style(
            ratatui::layout::Rect::new(area.x, y, area.width, 1),
            Style::default().bg(row_bg),
        );

        match &stat {
            Some(s) => {
                let sw = s.width() as u16;
                let left_w = area.width.saturating_sub(sw).saturating_sub(1);
                f.buffer_mut().set_line(area.x, y, &left, left_w);
                let sx = area.x + area.width.saturating_sub(sw);
                f.buffer_mut().set_line(sx, y, s, sw);
            }
            None => {
                f.buffer_mut().set_line(area.x, y, &left, area.width);
            }
        }
    }
}

fn tree_left(app: &App, node_idx: usize, bg: Color) -> Line<'static> {
    let node = &app.tree.nodes[node_idx];

    let indent = "  ".repeat(node.depth.saturating_sub(1));

    let name_color = match node.kind {
        tree::Kind::Dir => DIR_FG,
        tree::Kind::File => node
            .status
            .chars()
            .next()
            .map(status_color)
            .unwrap_or(Color::Reset),
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(indent, Style::default().bg(bg)));

    match node.kind {
        tree::Kind::Dir => {
            // Open/closed folder icon; colored like the name.
            let glyph = if node.expanded {
                "\u{f07c}"
            } else {
                "\u{f07b}"
            };
            spans.push(Span::styled(
                format!("{glyph} "),
                Style::default().fg(DIR_FG).bg(bg),
            ));
        }
        tree::Kind::File => {
            // File-type icon plus a space keep names aligned with the dir
            // glyph + space width.
            spans.push(Span::styled(
                format!("{} ", file_icon(&node.name)),
                Style::default().fg(name_color).bg(bg),
            ));
        }
    }

    spans.push(Span::styled(
        node.name.clone(),
        Style::default().fg(name_color).bg(bg),
    ));

    if node.kind == tree::Kind::Dir {
        spans.push(Span::styled(
            format!(" {}", node.file_count),
            Style::default().fg(Color::DarkGray).bg(bg),
        ));
    }

    Line::from(spans)
}

/// File-type glyph for a tree row, chosen by extension (Nerd Font glyphs).
/// Unknown extensions fall back to a generic file icon.
fn file_icon(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "rs" => "\u{e7a8}",                             // rust
        "toml" | "yaml" | "yml" | "json" => "\u{e615}", // config gear
        "py" => "\u{e795}",                             // python
        "js" | "jsx" | "mjs" | "cjs" => "\u{e8c8}",     // javascript
        "ts" | "tsx" => "\u{e9a0}",                     // typescript
        "c" | "h" | "cpp" | "hpp" => "\u{e8a0}",        // c/c++
        "go" => "\u{e8bd}",                             // go
        "md" | "markdown" => "\u{e609}",                // markdown
        "sh" | "bash" | "zsh" | "fish" => "\u{f245}",   // shell
        _ => "\u{e612}",                                // generic file
    }
}

fn status_color(ch: char) -> Color {
    match ch {
        'A' => Color::Green,
        'M' => Color::Yellow,
        'D' => Color::Red,
        'R' => Color::Magenta,
        'C' => Color::Cyan,
        'T' => Color::Blue,
        _ => Color::Reset,
    }
}

fn stat_line(node: &tree::TreeNode, bg: Color) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if node.added > 0 {
        spans.push(Span::styled(
            format!("+{}", node.added),
            Style::default().fg(Color::Green).bg(bg),
        ));
    }
    if node.deleted > 0 {
        if !spans.is_empty() {
            spans.push(Span::styled(" ", Style::default().bg(bg)));
        }
        spans.push(Span::styled(
            format!("-{}", node.deleted),
            Style::default().fg(Color::Red).bg(bg),
        ));
    }
    if spans.is_empty() {
        None
    } else {
        Some(Line::from(spans))
    }
}

fn render_diff(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    // No file selected: placeholder, no locked header.
    if app.current_file.is_none() {
        let style = Style::default().fg(Color::DarkGray);
        render_centered(f, area, "select a file to preview its diff", style);
        return;
    }
    // Error path: no header, message in the body area.
    let Some(parsed) = app.parsed.as_ref() else {
        let msg = app.diff_error.as_deref().unwrap_or("diff failed");
        let line = Line::styled(format!("error: {msg}"), Style::default().fg(Color::Red));
        f.buffer_mut().set_line(area.x, area.y, &line, area.width);
        return;
    };

    let has_rename = parsed.header.old_path.is_some();
    let header_height = 1 + has_rename as u16;
    app.last_header_height = header_height;

    let visible = app.visible_line_indices();
    let body_height = area.height.saturating_sub(header_height);
    let max_scroll = max_scroll_for(visible.len(), body_height as usize);
    if app.diff_scroll > max_scroll {
        app.diff_scroll = max_scroll;
    }

    let active = app.active_chunk;
    let current_hunk = active.map(|h| h + 1).unwrap_or(0);

    // Locked header: line 1 always, line 2 rename. Mode change (if any)
    // right-aligns on line 1.
    let header_spans = build_header_line(&parsed.header, current_hunk, area.width as usize);
    f.buffer_mut().set_line(
        area.x,
        area.y,
        &elide(header_spans, app.diff_col, area.width, None),
        area.width,
    );
    let mut y = area.y + 1;
    if has_rename {
        let rename = build_rename_line(&parsed.header);
        f.buffer_mut().set_line(
            area.x,
            y,
            &elide(rename, app.diff_col, area.width, None),
            area.width,
        );
        y += 1;
    }

    // Empty body: centered messages below the header.
    if parsed.lines.is_empty() {
        let msg = if parsed.header.binary {
            "binary file"
        } else if parsed.header.status == 'R' {
            "rename — no content changes"
        } else {
            "no content changes"
        };
        let body = ratatui::layout::Rect::new(
            area.x,
            y,
            area.width,
            area.height.saturating_sub(header_height),
        );
        let style = Style::default().fg(Color::DarkGray);
        render_centered(f, body, msg, style);
        return;
    }

    // Scrollable body.
    let start = app.diff_scroll;

    // Sticky pins the header of the chunk scrolled past the top of the view
    // (start > 0) to the body top, keeping flow_height constant (no 2-line
    // jump). It is the "scrolled past" context header and may differ from the
    // active-chunk cursor.
    let sticky_header = if start > 0 {
        visible
            .iter()
            .take(start)
            .rev()
            .find_map(|&li| (parsed.lines[li].kind == LineKind::HunkHeader).then_some(li))
    } else {
        None
    };
    app.sticky_line = sticky_header;
    let sticky = sticky_header.is_some();
    let flow_height = if sticky {
        body_height.saturating_sub(1)
    } else {
        body_height
    };
    let end = (start + flow_height as usize).min(visible.len());

    if let Some(idx) = sticky_header {
        y = render_hunk_header(f, app, area, y, idx, parsed);
    }

    for &line_idx in &visible[start..end] {
        let line = &parsed.lines[line_idx];
        if line.kind == LineKind::HunkHeader {
            y = render_hunk_header(f, app, area, y, line_idx, parsed);
        } else {
            let base_bg = bg_for(line.kind);
            let tokens = app
                .highlighted
                .get(line_idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let spans = diff_body_line(tokens, line, base_bg, app.word_diff_on);
            let l = elide(spans, app.diff_col, area.width, None);
            f.buffer_mut().set_line(area.x, y, &l, area.width);
            y += 1;
        }
    }
}

const BG_ADDED: Color = Color::Rgb(0x1a, 0x33, 0x26);
const BG_REMOVED: Color = Color::Rgb(0x3a, 0x1f, 0x1f);
// Monokai chunk headers: inactive/folded uses the inactive-tab color
// (#34352f), the active chunk uses the brighter selection shade (#464741).
// Both are darker than the #272822 terminal background (see docs/monokai-classic.md).
const BG_HUNK: Color = Color::Rgb(0x34, 0x35, 0x2f);
const BG_HUNK_ACTIVE: Color = Color::Rgb(0x46, 0x47, 0x41);
// Dimmed/unfocused active color: midpoint of active #464741 and inactive
// #34352f. Used when a pane's active item sits in the unfocused pane.
const BG_HUNK_DIM: Color = Color::Rgb(0x3d, 0x3e, 0x38);
const BG_RUN_ADDED: Color = Color::Rgb(0x2f, 0x55, 0x44);

const BG_RUN_REMOVED: Color = Color::Rgb(0x5a, 0x2f, 0x2f);

/// Monokai blue (#66d9ef) for directory nodes in the tree.
const DIR_FG: Color = Color::Rgb(0x66, 0xd9, 0xef);

/// Hard cap for horizontal scroll (cells), applied in `diff_scroll_right`.
const MAX_HSCROLL: usize = 10_000;
/// Upper bound for the `-U<N>` context window.
const MAX_CONTEXT: u32 = 200;
/// Context-line delta applied by the `[`/`]` keys.
const CONTEXT_STEP: i32 = 3;
/// Cells scrolled horizontally per `h`/`l` press.
const HSCROLL_STEP: usize = 2;

/// Background tint for a body line, depending on its diff kind.
fn bg_for(kind: LineKind) -> Color {
    match kind {
        LineKind::Added => BG_ADDED,
        LineKind::Removed => BG_REMOVED,
        LineKind::HunkHeader => BG_HUNK,
        LineKind::Context => Color::Reset,
    }
}

/// Lighter background for a changed word run.
fn run_bg(kind: LineKind) -> Color {
    match kind {
        LineKind::Added => BG_RUN_ADDED,
        LineKind::Removed => BG_RUN_REMOVED,
        _ => Color::Reset,
    }
}

/// Locked header line 1: `<path>  +A -D  <status>  [cur/total]`, plus a
/// right-aligned mode change (`old → new`, octal) when the modes differ.
fn build_header_line(header: &FileHeader, current_hunk: usize, width: usize) -> Vec<Span<'static>> {
    // Left content: path, status, and hunk position.
    let mut left = vec![
        Span::styled(header.path.clone(), Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(
            header.status.to_string(),
            Style::default().fg(status_color(header.status)),
        ),
    ];
    if !header.hunks.is_empty() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            format!("[{current_hunk}/{}]", header.hunks.len()),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right block: optional mode change, then the diff stat at the far right
    // so it aligns with the chunk-header diffstat below. `binary` moves with
    // the diff stat.
    let mut right: Vec<Span<'static>> = Vec::new();
    if let (Some(old), Some(new)) = (header.mode_old, header.mode_new) {
        if old != new {
            right.push(Span::styled(
                format!("{old:o} → {new:o}"),
                Style::default().fg(Color::DarkGray),
            ));
            right.push(Span::raw("  "));
        }
    }
    if header.binary {
        right.push(Span::styled("binary", Style::default().fg(Color::DarkGray)));
    } else {
        right.push(Span::styled(
            format!("+{}", header.added),
            Style::default().fg(Color::Green),
        ));
        right.push(Span::styled(
            format!(" -{}", header.deleted),
            Style::default().fg(Color::Red),
        ));
    }

    let left_w: usize = left.iter().map(|s| s.width()).sum();
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    // Right-align the block when there is room (at least one space of pad);
    // otherwise append it and let elide truncate the overflow.
    if width > left_w + 1 + right_w {
        left.push(Span::raw(" ".repeat(width - left_w - right_w)));
    }
    left.extend(right);
    left
}

/// Locked header line 2: rename `old → new  R <percent>%`.
fn build_rename_line(header: &FileHeader) -> Vec<Span<'static>> {
    let old = header.old_path.clone().unwrap_or_default();
    let mut spans = vec![
        Span::styled(old, Style::default().fg(Color::DarkGray)),
        Span::styled(" → ", Style::default().fg(Color::DarkGray)),
        Span::styled(header.path.clone(), Style::default().fg(Color::DarkGray)),
    ];
    if let Some(p) = header.rename_percent {
        spans.push(Span::styled(
            format!("  R {p}%"),
            Style::default().fg(Color::Magenta),
        ));
    }
    spans
}

/// Compose a body line's spans: syntax tokens tinted with the line's
/// background, word-level runs overlaid when enabled. Caller elides.
fn diff_body_line(
    tokens: &[TokenOwned],
    line: &DiffLine,
    base_bg: Color,
    word_diff_on: bool,
) -> Vec<Span<'static>> {
    if !(word_diff_on && line.word_runs.is_some()) {
        return spans_from_tokens(tokens, &line.raw_text, base_bg);
    }
    let runs = line.word_runs.as_deref().unwrap_or(&[]);
    overlay_word_runs(&line.raw_text, tokens, runs, line.kind, base_bg)
}

/// Convert syntax tokens to spans with a uniform background. If no grammar
/// matched (empty tokens), render the raw text as a single uncolored span.
fn spans_from_tokens(tokens: &[TokenOwned], raw: &str, bg: Color) -> Vec<Span<'static>> {
    if tokens.is_empty() {
        return vec![Span::styled(raw.to_string(), Style::default().bg(bg))];
    }
    tokens
        .iter()
        .map(|t| Span::styled(t.text.clone(), Style::default().fg(t.color).bg(bg)))
        .collect()
}

/// Overlay word-level run backgrounds on top of syntax tokens.
///
/// The kept runs (context runs plus runs matching this line's own kind) tile
/// the line's `raw_text` exactly, as do the syntax tokens, so we merge them by
/// byte offset: split tokens at run boundaries and repaint each segment's
/// background. Runs from the paired opposite line (e.g. Added runs on a
/// Removed line) are skipped — they are not part of this line's text.
fn overlay_word_runs(
    raw: &str,
    tokens: &[TokenOwned],
    runs: &[WordRun],
    line_kind: LineKind,
    base_bg: Color,
) -> Vec<Span<'static>> {
    let owned;
    let tokens = if tokens.is_empty() {
        owned = vec![TokenOwned {
            text: raw.to_string(),
            color: Color::Reset,
        }];
        owned.as_slice()
    } else {
        tokens
    };

    // Byte spans of the runs that belong to this line, with their bg color.
    // The shared run vector holds runs from both the removed and added sides;
    // advance the offset only over runs this line actually contains (Context or
    // its own kind), so the kept spans tile `raw` exactly. Counting the skipped
    // opposite-side runs would push the byte offsets past the end of `raw`.
    let mut kept: Vec<(usize, usize, Color)> = Vec::new();
    let mut run_pos = 0usize;
    for run in runs {
        if run.kind == LineKind::Context || run.kind == line_kind {
            let rs = run_pos;
            let bg = if run.changed && run.kind == line_kind {
                run_bg(run.kind)
            } else {
                base_bg
            };
            run_pos += run.text.len();
            kept.push((rs, run_pos, bg));
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut tok_start = 0usize;
    let mut ti = 0usize;
    for &(rs, re, bg) in &kept {
        let mut seg = rs;
        while seg < re {
            while ti + 1 < tokens.len() && tok_start + tokens[ti].text.len() <= seg {
                tok_start += tokens[ti].text.len();
                ti += 1;
            }
            if ti >= tokens.len() {
                // Tokens ran out before this kept run ended; render the rest of
                // the run with a fallback foreground so the line still tiles
                // `raw` exactly. Never indexes out of bounds.
                spans.push(Span::styled(
                    raw[seg..re].to_string(),
                    Style::default().fg(Color::Reset).bg(bg),
                ));
                seg = re;
                continue;
            }
            let te = tok_start + tokens[ti].text.len();
            let s = seg.max(tok_start);
            let e = re.min(te);
            if s >= e {
                tok_start = te;
                ti += 1;
                continue;
            }
            spans.push(Span::styled(
                raw[s..e].to_string(),
                Style::default().fg(tokens[ti].color).bg(bg),
            ));
            seg = e;
            if e == te {
                tok_start = te;
                ti += 1;
            }
        }
    }
    spans
}

/// Background for a chunk header: active is the selection color (light) when
/// the diff pane is focused, the dimmed midpoint when unfocused, and the
/// inactive-tab color otherwise.
fn hunk_header_bg(is_active: bool, focus: Focus) -> Color {
    if is_active {
        if focus == Focus::Diff {
            BG_HUNK_ACTIVE
        } else {
            BG_HUNK_DIM
        }
    } else {
        BG_HUNK
    }
}

/// The `@@` and syntax-highlighted section name; scrolls horizontally.
fn hunk_name_spans(meta: &HunkMeta, name_tokens: &[TokenOwned], bg: Color) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        "@@".to_string(),
        Style::default().fg(Color::Blue).bg(bg),
    )];
    if let Some(section) = &meta.section {
        spans.push(Span::styled(" ".to_string(), Style::default().bg(bg)));
        if name_tokens.is_empty() {
            spans.push(Span::styled(
                section.clone(),
                Style::default().fg(Color::DarkGray).bg(bg),
            ));
        } else {
            for t in name_tokens {
                spans.push(Span::styled(
                    t.text.clone(),
                    Style::default().fg(t.color).bg(bg),
                ));
            }
        }
    }
    spans
}

/// The `+A -D` diffstat, a spacer, and the fold arrow. Pinned to the pane's
/// right edge (does not scroll).
fn hunk_stat_spans(meta: &HunkMeta, bg: Color, collapsed: bool) -> Vec<Span<'static>> {
    let glyph = if collapsed { "▶" } else { "▼" };
    vec![
        Span::styled(
            format!("+{}", meta.added),
            Style::default().fg(Color::Green).bg(bg),
        ),
        Span::styled(
            format!(" -{}", meta.deleted),
            Style::default().fg(Color::Red).bg(bg),
        ),
        Span::styled(" ".to_string(), Style::default().bg(bg)),
        Span::styled(glyph.to_string(), Style::default().fg(Color::Blue).bg(bg)),
    ]
}

/// Render one chunk header: the name scrolls/elides on the left, the diffstat
/// and arrow stay pinned to the right edge of the pane. Returns the `y`
/// directly below the rendered row.
fn render_hunk_header(
    f: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    y: u16,
    line_idx: usize,
    parsed: &ParsedDiff,
) -> u16 {
    let line = &parsed.lines[line_idx];
    let name_tokens = app
        .highlighted
        .get(line_idx)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let collapsed = app.collapsed_hunks.contains(&line.hunk_index);
    let bg = hunk_header_bg(app.active_chunk == Some(line.hunk_index), app.focus);
    let Some(meta) = parsed.header.hunks.get(line.hunk_index) else {
        let l = Line::styled(
            line.raw_text.clone(),
            Style::default().fg(Color::Blue).bg(bg),
        );
        f.buffer_mut().set_line(area.x, y, &l, area.width);
        return y + 1;
    };
    // Diffstat + arrow pinned to the pane's right edge, with one cell of
    // breathing room between the elided name and the stat.
    let stat = hunk_stat_spans(meta, bg, collapsed);
    let stat_w: u16 = stat.iter().map(|s| s.width()).sum::<usize>() as u16;
    let name_w = area.width.saturating_sub(stat_w);
    let mut name_line = elide(
        hunk_name_spans(meta, name_tokens, bg),
        app.diff_col,
        name_w.saturating_sub(1),
        Some(bg),
    );
    // Pad so the background spans the full name region (ending one cell
    // short of the diffstat).
    let used: usize = name_line.spans.iter().map(|s| s.width()).sum();
    let pad = (name_w as usize).saturating_sub(used);
    if pad > 0 {
        name_line
            .spans
            .push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    f.buffer_mut().set_line(area.x, y, &name_line, name_w);
    if name_w > 0 {
        f.buffer_mut()
            .set_line(area.x + name_w, y, &Line::from(stat), area.width - name_w);
    }
    y + 1
}

/// Elide `spans` to `width` cells, starting at horizontal offset `col`.
/// Cell count is `Span::width`; wide (CJK) chars are an approximation. The
/// `bg`, when given, is applied to the `…` markers so they inherit the
/// background of the line they sit on.
fn elide(spans: Vec<Span<'static>>, col: usize, width: u16, bg: Option<Color>) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let marker = match bg {
        Some(bg) => Span::styled("…".to_string(), Style::default().fg(Color::DarkGray).bg(bg)),
        None => Span::styled("…".to_string(), Style::default().fg(Color::DarkGray)),
    };
    let avail = width as usize;
    let mut result: Vec<Span<'static>> = Vec::new();

    if col > 0 {
        // Left marker eats one cell, then skip `col` cells from the left.
        result.push(marker.clone());
        result.extend(drop_left(spans, col));
    } else {
        result.extend(spans);
    }

    if result.iter().map(|s| s.width()).sum::<usize>() <= avail {
        return Line::from(result);
    }

    // Leave one cell for the right marker.
    let target = avail.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in result {
        let w = span.width();
        if used + w <= target {
            used += w;
            out.push(span);
        } else if used < target {
            out.push(keep_left(&span, target - used));
            break;
        } else {
            break;
        }
    }
    out.push(marker);
    Line::from(out)
}

/// Drop up to `col` cells from the front of `spans`, splitting the partial
/// span. Cell width ≈ char count (approximation for wide chars).
fn drop_left(spans: Vec<Span<'static>>, col: usize) -> Vec<Span<'static>> {
    let mut skip = col;
    let mut i = 0usize;
    while i < spans.len() && skip > 0 {
        let w = spans[i].width();
        if w <= skip {
            skip -= w;
            i += 1;
        } else {
            let mut kept = Vec::with_capacity(spans.len() - i);
            kept.push(span_tail(&spans[i], skip));
            kept.extend(spans.iter().skip(i + 1).cloned());
            return kept;
        }
    }
    spans.into_iter().skip(i).collect()
}

/// Keep the leading `cells` chars of `span` (approx cell count).
fn keep_left(span: &Span<'static>, cells: usize) -> Span<'static> {
    let s = span.content.as_ref();
    let take = cells.min(s.chars().count());
    Span::styled(s.chars().take(take).collect::<String>(), span.style)
}

/// Keep the tail of `span` after dropping `cells` leading chars (approx).
fn span_tail(span: &Span<'static>, cells: usize) -> Span<'static> {
    let s = span.content.as_ref();
    let drop = cells.min(s.chars().count());
    Span::styled(s.chars().skip(drop).collect::<String>(), span.style)
}

/// Highest `diff_scroll` that keeps the last visible line on screen, accounting
/// for the sticky header consuming one row at any `diff_scroll > 0`.
fn max_scroll_for(visible_len: usize, win: usize) -> usize {
    if visible_len <= win {
        0
    } else {
        visible_len - win + 1
    }
}

/// Draw `text` centered within `area`, using a single `style`.
fn render_centered(f: &mut Frame, area: ratatui::layout::Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let w = text.chars().count() as u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(1) / 2;
    let line = Line::styled(text.to_string(), style);
    f.buffer_mut().set_line(x, y, &line, area.width);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Tree;
    use diff::HunkMeta;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_app() -> App {
        let tree = Tree::build(vec![('M', "a.rs".to_string()), ('M', "b.rs".to_string())]);
        App::new(
            "HEAD".into(),
            tree,
            false,
            Highlighter::new("Monokai Extended").ok(),
        )
    }

    fn sample_parsed(n: usize) -> ParsedDiff {
        ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
                    added: 1,
                    deleted: 1,
                }],
            },
            lines: (0..n)
                .map(|i| DiffLine {
                    kind: LineKind::Context,
                    raw_text: format!("l{i}"),
                    hunk_index: 0,
                    word_runs: None,
                })
                .collect(),
        }
    }

    fn header_full(mode_old: Option<u32>, mode_new: Option<u32>) -> FileHeader {
        FileHeader {
            path: "x.rs".into(),
            status: 'M',
            added: 1,
            deleted: 1,
            old_path: None,
            rename_percent: None,
            mode_old,
            mode_new,
            binary: false,
            hunks: vec![HunkMeta {
                section: None,
                added: 1,
                deleted: 1,
            }],
        }
    }

    fn hunk_header(idx: usize) -> DiffLine {
        DiffLine {
            kind: LineKind::HunkHeader,
            raw_text: format!("@@ -{idx} +{idx} @@"),
            hunk_index: idx,
            word_runs: None,
        }
    }

    fn ctx_line(idx: usize, hunk: usize) -> DiffLine {
        DiffLine {
            kind: LineKind::Context,
            raw_text: format!("l{idx}"),
            hunk_index: hunk,
            word_runs: None,
        }
    }

    /// A 3-hunk diff whose 6 visible lines all fit in the pane.
    fn fitting_hunk() -> ParsedDiff {
        ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 3,
                deleted: 3,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: (0..3)
                    .map(|_| HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                hunk_header(1),
                ctx_line(1, 1),
                hunk_header(2),
                ctx_line(2, 2),
            ],
        }
    }

    #[test]
    fn focus_tab_cycles() {
        let mut app = sample_app();
        assert_eq!(app.focus, Focus::Tree);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.focus, Focus::Diff);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn shift_overrides_in_tree_focus() {
        let mut app = sample_app();
        app.focus = Focus::Tree;
        app.selection = 0;
        app.diff_scroll = 0;
        app.diff_col = 0;
        app.height = 1;
        app.last_header_height = 1;
        app.parsed = Some(sample_parsed(3));
        app.highlighted = std::iter::repeat_with(Vec::new).take(3).collect();

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert!(app.diff_scroll > 0);
        assert_eq!(app.selection, 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert_eq!(app.diff_scroll, 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert!(app.diff_col > 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert_eq!(app.diff_col, 0);

        // Plain `j` still moves the tree.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.selection, 1);
    }

    #[test]
    fn diff_focus_scrolls() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.selection = 0;
        app.diff_scroll = 0;
        app.diff_col = 0;
        app.height = 1;
        app.last_header_height = 1;
        app.parsed = Some(sample_parsed(3));
        app.highlighted = std::iter::repeat_with(Vec::new).take(3).collect();
        app.active_chunk = Some(0);

        let sel = app.selection;
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.diff_scroll > 0);
        assert_eq!(app.selection, sel);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.diff_scroll, 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.diff_col > 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.diff_col, 0);
    }

    #[test]
    fn du_removed() {
        let mut app = sample_app();
        app.focus = Focus::Tree;
        let before = (app.selection, app.diff_scroll, app.diff_col);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!((app.selection, app.diff_scroll, app.diff_col), before);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!((app.selection, app.diff_scroll, app.diff_col), before);

        app.focus = Focus::Diff;
        let before = (app.selection, app.diff_scroll, app.diff_col);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!((app.selection, app.diff_scroll, app.diff_col), before);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!((app.selection, app.diff_scroll, app.diff_col), before);
    }

    #[test]
    fn elide_truncates_right() {
        let line = elide(vec![Span::raw("abcdefghijklm")], 0, 10, None);
        let spans = line.spans;
        assert_eq!(spans.last().unwrap().content.as_ref(), "…");
        let total: usize = spans.iter().map(|s| s.width()).sum();
        assert!(total <= 10);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with('…'));
        assert!(text.chars().count() <= 10);
    }

    #[test]
    fn elide_left_marker() {
        let line = elide(vec![Span::raw("abcdefghijklm")], 5, 10, None);
        let spans = line.spans;
        assert_eq!(spans[0].content.as_ref(), "…");
        assert_eq!(spans[0].style.fg, Some(Color::DarkGray));
        let rest: String = spans.iter().skip(1).map(|s| s.content.as_ref()).collect();
        assert!(rest.starts_with('f'));
        let total: usize = spans.iter().map(|s| s.width()).sum();
        assert!(total <= 10);
    }

    #[test]
    fn collapse_hunk_hides_body() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.diff_scroll = 0;
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
                    added: 2,
                    deleted: 0,
                }],
            },
            lines: vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "foo".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "bar".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(3).collect();
        app.active_chunk = Some(0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&0));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let text: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(!text.contains("foo"));
        assert!(!text.contains("bar"));
        assert!(text.contains("+2 -0"));
    }

    #[test]
    fn context_change_refetches() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        assert_eq!(app.context_lines, 3);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.context_lines, 6);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.context_lines, 3);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.context_lines, 0);

        for _ in 0..5 {
            assert!(!handle_key(
                KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
                &mut app
            ));
        }
        assert_eq!(app.context_lines, 0);

        for _ in 0..300 {
            assert!(!handle_key(
                KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
                &mut app
            ));
        }
        assert_eq!(app.context_lines, 200);
    }

    #[test]
    fn render_diff_smoke() {
        let tree = Tree::build(vec![('M', "x.rs".to_string())]);
        let highlighter = Highlighter::new("Monokai Extended").ok();
        let mut app = App::new("HEAD".into(), tree, false, highlighter);
        app.current_file = Some("x.rs".into());
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
                    added: 1,
                    deleted: 1,
                }],
            },
            lines: vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "new".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "old".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
            ],
        });
        app.highlighted = match app.parsed.as_ref() {
            Some(p) => App::cache_highlights(p, "x.rs", app.highlighter.as_mut()),
            None => Vec::new(),
        };

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let cells = terminal.backend().buffer().content();
        let row: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(row.contains("x.rs"));
    }

    #[test]
    fn render_diff_with_word_runs() {
        let parsed = ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 2,
                deleted: 2,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
                    added: 2,
                    deleted: 2,
                }],
            },
            lines: vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "old".into(),
                    hunk_index: 0,
                    // Single changed run covering the whole line.
                    word_runs: Some(vec![WordRun {
                        text: "old".into(),
                        changed: true,
                        kind: LineKind::Removed,
                    }]),
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "newlonger".into(),
                    hunk_index: 0,
                    // Pair whose sides differ in length; used to panic when
                    // syntect tokens ran out before the kept runs did.
                    word_runs: Some(vec![WordRun {
                        text: "newlonger".into(),
                        changed: true,
                        kind: LineKind::Added,
                    }]),
                },
                DiffLine {
                    kind: LineKind::Removed,
                    raw_text: "fn foo()".into(),
                    hunk_index: 0,
                    // Mixed Context + changed runs.
                    word_runs: Some(vec![
                        WordRun {
                            text: "fn ".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                        WordRun {
                            text: "foo".into(),
                            changed: true,
                            kind: LineKind::Removed,
                        },
                        WordRun {
                            text: "()".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                    ]),
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "fn bar()".into(),
                    hunk_index: 0,
                    word_runs: Some(vec![
                        WordRun {
                            text: "fn ".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                        WordRun {
                            text: "bar".into(),
                            changed: true,
                            kind: LineKind::Added,
                        },
                        WordRun {
                            text: "()".into(),
                            changed: false,
                            kind: LineKind::Context,
                        },
                    ]),
                },
            ],
        };

        // Word-diff on: exercises the overlay path.
        let tree = Tree::build(vec![('M', "x.rs".to_string())]);
        let highlighter = Highlighter::new("Monokai Extended").ok();
        let mut app = App::new("HEAD".into(), tree, false, highlighter);
        app.current_file = Some("x.rs".into());
        app.word_diff_on = true;
        app.last_header_height = 1;
        app.parsed = Some(parsed);
        app.highlighted = match app.parsed.as_ref() {
            Some(p) => App::cache_highlights(p, "x.rs", app.highlighter.as_mut()),
            None => Vec::new(),
        };

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let row: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(row.contains("x.rs"));

        // Word-diff off: the non-overlay path still renders.
        app.word_diff_on = false;
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        // Directly exercise the guard: tokens that don't tile `raw` exactly
        // must not panic and must still tile `raw` exactly (fallback spans).
        let spans = overlay_word_runs(
            "newlonger",
            &[TokenOwned {
                text: "new".to_string(),
                color: Color::Reset,
            }],
            &[WordRun {
                text: "newlonger".into(),
                changed: true,
                kind: LineKind::Added,
            }],
            LineKind::Added,
            Color::Reset,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "newlonger");
    }

    #[test]
    fn mode_hidden_when_unchanged() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        app.parsed = Some(ParsedDiff {
            header: header_full(Some(0o100644), Some(0o100644)),
            lines: vec![hunk_header(0), ctx_line(0, 0)],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(2).collect();

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(!text.contains("100644 → 100644"));
        assert_eq!(app.last_header_height, 1);
    }

    #[test]
    fn mode_shown_when_changed() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        app.parsed = Some(ParsedDiff {
            header: header_full(Some(0o100644), Some(0o100755)),
            lines: vec![hunk_header(0), ctx_line(0, 0)],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(2).collect();

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("x.rs"));
        assert!(text.contains("100644 → 100755"));
        assert_eq!(app.last_header_height, 1);
    }

    #[test]
    fn mode_emitted_when_narrow() {
        let header = header_full(Some(0o100644), Some(0o100755));
        let spans = build_header_line(&header, 1, 20);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // The mode + diff-stat block is always appended; narrow widths rely on
        // elide to truncate the overflow, so both are present in raw spans.
        assert!(text.contains("100644 → 100755"));
        assert!(text.contains("+1"));
        assert!(text.contains("-1"));
        // The diff stat sits at the far right of the line.
        assert!(text.ends_with("-1"));
    }

    #[test]
    fn sticky_header_pinned() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        let n = 30;
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..n).map(|i| ctx_line(i, 0)))
            .collect();
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines,
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(n + 1).collect();
        // Scroll the `@@` header off above the body top.
        app.diff_scroll = 25;

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("▼"));
        // The `@@` header renders only in the sticky line, not in flow.
        assert_eq!(text.matches("@@").count(), 1);
    }

    #[test]
    fn sticky_disappears_when_header_visible() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        let n = 30;
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..n).map(|i| ctx_line(i, 0)))
            .collect();
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines,
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(n + 1).collect();
        app.diff_scroll = 0; // header at top, in flow.

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // In-flow header is expanded, so the arrow renders as `▼`; no
        // sticky duplicate header, and nothing is collapsed (`▶` absent).
        assert!(text.contains("▼"));
        assert!(!text.contains("▶"));
        assert_eq!(text.matches("@@").count(), 1);
    }

    #[test]
    fn hunk_header_restyle() {
        let meta = HunkMeta {
            section: Some("fn main()".into()),
            added: 3,
            deleted: 1,
        };
        let name_tokens = vec![TokenOwned {
            text: "fn".into(),
            color: Color::Rgb(1, 2, 3),
        }];
        let name = hunk_name_spans(&meta, &name_tokens, BG_HUNK);
        assert!(name.iter().any(|s| s.content.as_ref() == "fn")); // syntax-highlighted name
        assert!(name.iter().any(|s| s.content.as_ref() == "@@"));
        let stat = hunk_stat_spans(&meta, BG_HUNK, false);
        let has = |fg: Color, content: &str| {
            stat.iter()
                .any(|s| s.style.fg == Some(fg) && s.content.as_ref().contains(content))
        };
        assert!(has(Color::Green, "+3"));
        assert!(has(Color::Red, "-1"));
        assert!(has(Color::Blue, "▼"));
        let collapsed = hunk_stat_spans(&meta, BG_HUNK, true);
        assert!(collapsed
            .iter()
            .any(|s| s.style.fg == Some(Color::Blue) && s.content.as_ref() == "▶"));
    }

    #[test]
    fn cache_fills_hunk_section() {
        let parsed = ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: Some("fn main()".into()),
                    added: 1,
                    deleted: 1,
                }],
            },
            lines: vec![
                DiffLine {
                    kind: LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    raw_text: "foo".into(),
                    hunk_index: 0,
                    word_runs: None,
                },
            ],
        };
        let mut hl = Highlighter::new("Monokai Extended").ok();
        let cache = App::cache_highlights(&parsed, "x.rs", hl.as_mut());
        assert!(!cache[0].is_empty()); // hunk section got highlighted
    }

    #[test]
    fn active_highlight_bg() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.active_chunk = Some(0);
        // Focus the diff pane so the active chunk uses the full selection
        // color (#464741); otherwise focus-based dimming would render it dim.
        app.focus = Focus::Diff;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Top visible chunk (hunk 0's header, ACTIVE = brighter selection
        // #464741) at buffer row 1; hunk 1 is INACTIVE (inactive-tab #34352f)
        // at row 4.
        assert_eq!(buf.cell((0, 1)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));
        assert_eq!(buf.cell((0, 4)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn focus_dims_active_chunk() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.active_chunk = Some(0);
        // Focus on the tree: the active chunk header is the unfocused pane's
        // active item, so it renders at the dimmed color #3d3e38.
        app.focus = Focus::Tree;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((0, 1)).unwrap().bg, Color::Rgb(0x3d, 0x3e, 0x38));
        assert_eq!(buf.cell((0, 4)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn tree_row_focus_colors() {
        let tree = Tree::build(vec![('M', "x.rs".to_string())]);
        let mut app = App::new(
            "HEAD".into(),
            tree,
            false,
            Highlighter::new("Monokai Extended").ok(),
        );
        app.current_file = Some("x.rs".into());
        app.selection = 0;

        // Tree column for an 80-wide terminal: diff (53) + 1 gap = col 54.
        // The selected row is the first tree row; its name cell bg reflects
        // the focus-dependent selection color.
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 0)).unwrap().bg, Color::Rgb(0x46, 0x47, 0x41));

        // Focus the diff pane: the tree's selected row dims.
        app.focus = Focus::Diff;
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((54, 0)).unwrap().bg, Color::Rgb(0x34, 0x35, 0x2f));
    }

    #[test]
    fn toggle_uses_active_hunk() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 1;
        app.last_header_height = 1;
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                ctx_line(2, 0),
                hunk_header(1),
                ctx_line(3, 1),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();

        // Select hunk 1 and toggle it via Enter.
        app.active_chunk = Some(1);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&1));

        // Select hunk 0 and toggle it via Enter.
        app.active_chunk = Some(0);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&0));
    }

    #[test]
    fn space_toggles_dir() {
        let tree = Tree::build(vec![('M', "src/a.rs".to_string())]);
        let mut app = App::new(
            "HEAD".into(),
            tree,
            false,
            Highlighter::new("Monokai Extended").ok(),
        );
        app.focus = Focus::Tree;
        app.selection = 0;
        let idx = app.flat[0];
        assert!(app.tree.nodes[idx].expanded);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.tree.nodes[idx].expanded);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.tree.nodes[idx].expanded);
    }

    #[test]
    fn space_on_file_loads_diff() {
        let tree = Tree::build(vec![('M', "Cargo.toml".to_string())]);
        let mut app = App::new(
            "HEAD".into(),
            tree,
            false,
            Highlighter::new("Monokai Extended").ok(),
        );
        app.focus = Focus::Tree;
        app.selection = 0;
        app.parsed = None;
        app.current_file = None;

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.parsed.is_some());
    }

    #[test]
    fn space_in_diff_focus_toggles_hunk() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 1;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![hunk_header(0), ctx_line(0, 0)],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(2).collect();
        app.active_chunk = Some(0);
        assert!(app.collapsed_hunks.is_empty());

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&0));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.collapsed_hunks.contains(&0));
    }

    #[test]
    fn fold_chord_toggles_hunk() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 1;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![hunk_header(0), ctx_line(0, 0)],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(2).collect();
        app.active_chunk = Some(0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.fold_key);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&0));
        assert!(!app.fold_key);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.collapsed_hunks.contains(&0));
    }

    #[test]
    fn fold_chord_open_close() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 1;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![hunk_header(0), ctx_line(0, 0)],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(2).collect();
        app.active_chunk = Some(0);

        app.collapsed_hunks.insert(0);
        // `zo` opens the collapsed active hunk.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.collapsed_hunks.contains(&0));

        // `zc` closes it again.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&0));
    }

    #[test]
    fn fold_chord_zr_zm() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 1;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![
                    HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    },
                    HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    },
                ],
            },
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                hunk_header(1),
                ctx_line(1, 1),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(4).collect();
        assert!(app.collapsed_hunks.is_empty());

        // zM closes all hunks.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.collapsed_hunks.len(), 2);
        assert!(app.collapsed_hunks.contains(&0));
        assert!(app.collapsed_hunks.contains(&1));

        // zR opens all hunks.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.is_empty());
    }

    #[test]
    fn fold_key_resets() {
        let mut app = sample_app();
        app.focus = Focus::Tree;
        app.selection = 0;
        let sel = app.selection;

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.fold_key);
        // A non-fold second key consumes the chord and does not act (or move).
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.fold_key);
        assert_eq!(app.selection, sel);
    }

    #[test]
    fn mouse_click_toggles_hunk() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.current_file = Some("x.rs".into());
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                ctx_line(2, 0),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(4).collect();

        // Render once so height/diff_width/last_header_height/sticky_line fill in.
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        assert!(app.collapsed_hunks.is_empty());

        let click = |row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Row 0 is the locked header; row 1 is hunk 0's header.
        handle_mouse(click(1), &mut app);
        assert!(app.collapsed_hunks.contains(&0));
        handle_mouse(click(1), &mut app);
        assert!(!app.collapsed_hunks.contains(&0));

        // Clicking a body line (row 2, a ctx line) does not toggle.
        handle_mouse(click(2), &mut app);
        assert!(!app.collapsed_hunks.contains(&0));
    }

    #[test]
    fn toggle_hunk_helper() {
        let mut app = sample_app();
        assert!(app.collapsed_hunks.is_empty());
        app.toggle_hunk(3);
        assert!(app.collapsed_hunks.contains(&3));
        app.toggle_hunk(3);
        assert!(!app.collapsed_hunks.contains(&3));
    }

    #[test]
    fn selection_moves_when_diff_fits() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(fitting_hunk());
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.height = 20;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(0);
        assert!(app.diff_fits());

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(1));
        assert_eq!(app.diff_scroll, 0);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(2));
        // Clamped at the last chunk.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(2));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(1));

        // Shift+J also scrolls down, so it moves the selection when it fits.
        app.active_chunk = Some(0);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(1));
    }

    #[test]
    fn render_leaves_active_chunk_alone() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(fitting_hunk());
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.height = 20;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(2);

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        // Rendering never mutates the active-chunk cursor.
        assert_eq!(app.active_chunk, Some(2));
    }

    #[test]
    fn navigate_chunk_jumps_when_on_screen() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(fitting_hunk());
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.height = 20;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(0);

        app.navigate_chunk(1);
        // Chunk 1's header is in the visible window: jump to it without scrolling.
        assert_eq!(app.active_chunk, Some(1));
        assert_eq!(app.diff_scroll, 0);
    }

    #[test]
    fn navigate_chunk_scrolls_when_off_screen() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: (0..2)
                    .map(|_| HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            lines: std::iter::once(hunk_header(0))
                .chain((0..10).map(|i| ctx_line(i, 0)))
                .chain(std::iter::once(hunk_header(1)))
                .chain(std::iter::once(ctx_line(10, 1)))
                .collect(),
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(13).collect();
        app.height = 3;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(0);

        // page_size = 3 - 1 = 2, so chunk 1's header (visible index 11) is
        // below the view: navigate scrolls one step instead of activating.
        app.navigate_chunk(1);
        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.active_chunk, Some(0));
    }

    #[test]
    fn collapse_snaps_view_to_chunk() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(fitting_hunk());
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.height = 3;
        app.last_header_height = 1;
        app.diff_scroll = 3; // chunk 1's body visible, header not at the top
        app.active_chunk = Some(1);

        app.collapse_hunk(1);

        // After collapsing chunk 1 the visible lines are [0,1,2,4,5]; chunk 1's
        // header (hunk index 1) sits at visible index 2, so the view scrolls to
        // pos+1 = 3, pinning the header as the sticky at the top.
        assert_eq!(app.diff_scroll, 3);
        assert_eq!(app.active_chunk, Some(1));
    }

    #[test]
    fn up_scrolls_smoothly() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        // Tall expanded chunk 0 above chunk 2. Moving up from chunk 2 scrolls one
        // line instead of jumping to chunk 1's header (which is not in view).
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: (0..3)
                    .map(|_| HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                hunk_header(1),
                ctx_line(1, 1),
                ctx_line(2, 1),
                ctx_line(3, 1),
                ctx_line(4, 1),
                ctx_line(5, 1),
                hunk_header(2),
                ctx_line(6, 2),
                ctx_line(7, 2),
                ctx_line(8, 2),
                ctx_line(9, 2),
                ctx_line(10, 2),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(14).collect();
        app.height = 3;
        app.last_header_height = 1; // page_size = 2
        app.diff_scroll = 10;
        app.active_chunk = Some(2);

        app.navigate_chunk(-1);

        // Chunk 1's header is above the view but not pinned, so no jump: just
        // scroll up one line. The active chunk stays chunk 2.
        assert_eq!(app.diff_scroll, 9);
        assert_eq!(app.active_chunk, Some(2));
    }

    #[test]
    fn up_selects_and_reveals_in_one_move() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(fitting_hunk());
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        // Collapse chunks 0 and 1 so they are single header lines above chunk 2.
        app.collapsed_hunks = [0, 1].into_iter().collect();
        // visible = [hdr0, hdr1, hdr2, ctx2]; scroll to the bottom.
        app.height = 2;
        app.last_header_height = 1;
        app.diff_scroll = 3;
        app.active_chunk = Some(2);

        app.navigate_chunk(-1);

        // Scrolling up pulls chunk 1's collapsed header line into the top of
        // the view, activating it (the exemption).
        assert_eq!(app.active_chunk, Some(1));
        assert_eq!(app.diff_scroll, 2); // chunk 1's header pinned at the top
    }

    #[test]
    fn scroll_activates_header_when_enters() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        // Two chunks, no active chunk. Scrolling down one line brings chunk 1's
        // header to the bottom of the view, activating it.
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: (0..2)
                    .map(|_| HunkMeta {
                        section: None,
                        added: 1,
                        deleted: 1,
                    })
                    .collect(),
            },
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                hunk_header(1),
                ctx_line(2, 1),
                ctx_line(3, 1),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.height = 5;
        app.last_header_height = 1; // page_size = 4
        app.diff_scroll = 0;
        app.active_chunk = None;

        app.navigate_chunk(1);

        // The line that scrolled into view at the bottom is chunk 1's header,
        // so chunk 1 becomes active.
        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.active_chunk, Some(1));
    }

    #[test]
    fn max_scroll_accounts_for_sticky_row() {
        assert_eq!(max_scroll_for(21, 6), 16); // len > win: len - win + 1
        assert_eq!(max_scroll_for(6, 6), 0); // exactly fits
        assert_eq!(max_scroll_for(3, 6), 0); // fits
    }

    #[test]
    fn last_line_visible_at_max_scroll_with_sticky() {
        let mut app = sample_app();
        app.current_file = Some("x.rs".into());
        let n = 20;
        let lines: Vec<DiffLine> = std::iter::once(hunk_header(0))
            .chain((0..n).map(|i| ctx_line(i, 0)))
            .collect();
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines,
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(n + 1).collect();
        // Scroll to the absolute bottom; render must clamp so the last line
        // (raw_text `l19`) is visible despite the sticky header row.
        app.diff_scroll = usize::MAX;

        let mut terminal = Terminal::new(TestBackend::new(80, 7)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("l19"), "last line was cut off: {text}");
    }

    #[test]
    fn chunk_start_end_motions() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        // Tall expanded chunk 0 (header + 5 ctx) then chunk 1.
        app.parsed = Some(ParsedDiff {
            header: header_full(None, None),
            lines: vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                ctx_line(2, 0),
                ctx_line(3, 0),
                ctx_line(4, 0),
                hunk_header(1),
                ctx_line(5, 1),
            ],
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(8).collect();
        app.height = 3;
        app.last_header_height = 1;
        app.active_chunk = Some(0);
        app.diff_scroll = 3; // middle of expanded chunk 0

        // `{` -> start of the current chunk (header pinned at the top).
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('{'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(0));
        assert_eq!(app.diff_scroll, 1); // chunk 0 header at the top

        // `}` -> end of the current chunk (last line near the bottom).
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(0));
        assert_eq!(app.diff_scroll, 4); // chunk 0 last line at the bottom
    }

    #[test]
    fn gg_goes_first_and_g_last() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.parsed = Some(fitting_hunk());
        app.highlighted = std::iter::repeat_with(Vec::new).take(6).collect();
        app.height = 2;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(2);

        // `gg`: two `g` presses go to the first chunk.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.g_key);
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.g_key);
        assert_eq!(app.active_chunk, Some(0));
        // Chunk 0's header (visible index 0) is pinned sticky at the top.
        assert_eq!(app.diff_scroll, 1);

        // `G` (Shift+g) goes straight to the last chunk.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert_eq!(app.active_chunk, Some(2));
        // Chunk 2's header (visible index 4) is pinned sticky at the top.
        assert_eq!(app.diff_scroll, 5);
    }

    #[test]
    fn scroll_still_scrolls_when_diff_overflows() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 3;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.active_chunk = Some(0);
        app.parsed = Some(ParsedDiff {
            header: FileHeader {
                path: "x.rs".into(),
                status: 'M',
                added: 10,
                deleted: 0,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                hunks: vec![HunkMeta {
                    section: None,
                    added: 10,
                    deleted: 0,
                }],
            },
            lines: std::iter::once(hunk_header(0))
                .chain((0..10).map(|i| ctx_line(i, 0)))
                .collect(),
        });
        app.highlighted = std::iter::repeat_with(Vec::new).take(11).collect();
        assert!(!app.diff_fits());

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.diff_scroll > 0);
        assert_eq!(app.active_chunk, Some(0));
    }

    #[test]
    fn current_hunk_is_the_cursor() {
        let mut app = sample_app();
        app.active_chunk = Some(2);
        assert_eq!(app.active_chunk, Some(2));
        app.active_chunk = None;
        assert_eq!(app.active_chunk, None);
    }
}
