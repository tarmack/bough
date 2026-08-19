//! bough — a minimalist terminal tree-diff viewer.

mod git;
mod tree;

use std::io::{self, Stdout};

use clap::Parser;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
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
}

struct App {
    rev: String,
    empty: bool,
    tree: Tree,
    flat: Vec<usize>,
    selection: usize,
    tree_scroll: usize,
    current_file: Option<String>,
    diff_lines: Vec<String>,
    diff_scroll: usize,
    diff_width: u16,
    height: usize,
}

impl App {
    fn new(rev: String, tree: Tree, empty: bool) -> Self {
        let flat = tree.flatten();
        let mut app = App {
            rev,
            empty,
            tree,
            flat,
            selection: 0,
            tree_scroll: 0,
            current_file: None,
            diff_lines: Vec::new(),
            diff_scroll: 0,
            diff_width: 0,
            height: 0,
        };
        app.load_diff();
        app
    }

    fn selected_node(&self) -> Option<usize> {
        self.flat.get(self.selection).copied()
    }

    fn load_diff(&mut self) {
        match self.selected_node() {
            Some(idx) if self.tree.nodes[idx].kind == tree::Kind::File => {
                let path = self.tree.nodes[idx].path.clone();
                self.current_file = Some(path.clone());
                let fetched = if self.empty {
                    git::file_diff_empty(&path)
                } else {
                    git::file_diff(&self.rev, &path)
                };
                self.diff_lines = match fetched {
                    Ok(d) => d.lines().map(|s| s.to_string()).collect(),
                    Err(e) => vec![format!("error: {e}")],
                };
            }
            _ => {
                self.current_file = None;
                self.diff_lines.clear();
            }
        }
    }

    fn move_down(&mut self, n: usize) {
        if self.flat.is_empty() {
            return;
        }
        let last = self.flat.len() - 1;
        self.selection = self.selection.saturating_add(n).min(last);
        self.diff_scroll = 0;
        self.load_diff();
    }

    fn move_up(&mut self, n: usize) {
        self.selection = self.selection.saturating_sub(n);
        self.diff_scroll = 0;
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

    fn page_size(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }

    fn diff_page_down(&mut self) {
        let max = self.diff_lines.len().saturating_sub(1);
        self.diff_scroll = self.diff_scroll.saturating_add(self.page_size()).min(max);
    }

    fn diff_page_up(&mut self) {
        self.diff_scroll = self.diff_scroll.saturating_sub(self.page_size());
    }

    fn diff_scroll_down(&mut self, n: usize) {
        let max = self.diff_lines.len().saturating_sub(1);
        self.diff_scroll = self.diff_scroll.saturating_add(n).min(max);
    }

    fn diff_scroll_up(&mut self, n: usize) {
        self.diff_scroll = self.diff_scroll.saturating_sub(n);
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

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
        git::diff_stats(&revision).unwrap_or_default()
    };

    let mut tree = Tree::build(entries);
    tree.annotate_stats(&stats);
    let mut app = App::new(revision, tree, empty);

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
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => true,
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
        KeyCode::Char('d') => {
            app.diff_page_down();
            false
        }
        KeyCode::Char('u') => {
            app.diff_page_up();
            false
        }
        _ => false,
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
        let left = tree_left(app, node_idx, selected);
        let stat = stat_line(node, selected);
        let y = area.y + (i - app.tree_scroll) as u16;

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

fn tree_left(app: &App, node_idx: usize, selected: bool) -> Line<'static> {
    let node = &app.tree.nodes[node_idx];

    let bg = if selected {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let indent = "  ".repeat(node.depth.saturating_sub(1));

    let name_color = match node.kind {
        tree::Kind::Dir => Color::Cyan,
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
            let glyph = if node.expanded { "▼" } else { "▶" };
            spans.push(Span::styled(
                format!("{glyph} "),
                Style::default().fg(Color::Cyan).bg(bg),
            ));
        }
        tree::Kind::File => {
            // Two spaces align file names with the dir glyph width.
            spans.push(Span::styled("  ", Style::default().bg(bg)));
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

    if !node.status.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(bg)));
        for ch in node.status.chars() {
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(status_color(ch)).bg(bg),
            ));
        }
    }

    Line::from(spans)
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

fn stat_line(node: &tree::TreeNode, selected: bool) -> Option<Line<'static>> {
    let bg = if selected {
        Color::DarkGray
    } else {
        Color::Reset
    };
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
    // Header line: current file path.
    let header = match &app.current_file {
        Some(path) => Line::styled(path.clone(), Style::default().fg(Color::DarkGray)),
        None => Line::styled(
            "select a file to preview its diff",
            Style::default().fg(Color::DarkGray),
        ),
    };
    f.buffer_mut().set_line(area.x, area.y, &header, area.width);

    if app.diff_lines.is_empty() {
        return;
    }

    let body_height = area.height.saturating_sub(1);
    let max_scroll = app.diff_lines.len().saturating_sub(1);
    if app.diff_scroll > max_scroll {
        app.diff_scroll = max_scroll;
    }

    let start = app.diff_scroll;
    let end = (start + body_height as usize).min(app.diff_lines.len());
    let mut y = area.y + 1;
    for i in start..end {
        let text = &app.diff_lines[i];
        let line = Line::styled(text.clone(), diff_style(text));
        f.buffer_mut().set_line(area.x, y, &line, area.width);
        y += 1;
    }
}

fn diff_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") {
        return Style::default().fg(Color::DarkGray);
    }
    if line.starts_with("@@") {
        return Style::default().fg(Color::Yellow);
    }
    if line.starts_with('+') {
        return Style::default().fg(Color::Green);
    }
    if line.starts_with('-') {
        return Style::default().fg(Color::Red);
    }
    Style::default()
}
