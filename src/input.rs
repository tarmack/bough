//! Terminal event loop and key/mouse dispatch.
//!
//! Translates raw terminal events into `App` operations. It owns no state; all
//! state lives in `crate::app::App` and all drawing in `crate::view`.

use std::io::{self, Stdout};
use std::time::Duration;

use crossbeam_channel::Receiver;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{App, Focus, CONTEXT_STEP, HSCROLL_STEP};
use crate::snapshot::ArcSnapshot;
use crate::view;

/// Drain the snapshot channel, keeping only the latest `ArcSnapshot`, and
/// apply it to `app`. Called between key events so the UI stays fresh.
fn drain_snapshots(rx: &Receiver<ArcSnapshot>, app: &mut App) {
    let mut latest: Option<ArcSnapshot> = None;
    while let Ok(snap) = rx.try_recv() {
        latest = Some(snap);
    }
    if let Some(snap) = latest {
        app.apply_snapshot(snap);
    }
}

/// Run the interactive loop until the user quits.
pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    snapshot_rx: &Receiver<ArcSnapshot>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| view::ui(f, app))?;
        drain_snapshots(snapshot_rx, app);
        if event::poll(Duration::from_millis(16))? {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileHeader, HunkMeta};
    use crate::test_support::*;
    use ratatui::backend::TestBackend;

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
        app.snapshot = snapshot_with_diff(sample_parsed(3));

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
        app.snapshot = snapshot_with_diff(sample_parsed(3));
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
    fn collapse_hunk_hides_body() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.diff_scroll = 0;
        app.snapshot = snapshot_with_diff(single_parsed(
            FileHeader {
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
            vec![
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::HunkHeader,
                    raw_text: "@@ -1 +1 @@".into(),
                    section: 0,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::Added,
                    raw_text: "foo".into(),
                    section: 0,
                    word_runs: None,
                },
                crate::diff::DiffLine {
                    kind: crate::diff::LineKind::Added,
                    raw_text: "bar".into(),
                    section: 0,
                    word_runs: None,
                },
            ],
        ));
        app.active_chunk = Some(0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.collapsed_hunks.contains(&0));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let text: String = cells.iter().map(|c| c.symbol().to_string()).collect();
        assert!(!text.contains("foo"));
        assert!(!text.contains("bar"));
        assert!(text.contains("+2 -0"));
    }

    #[test]
    fn space_toggles_dir() {
        let tree = crate::tree::Tree::build(vec![('M', "src/a.rs".to_string())]);
        let (tx, _rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = crate::app::App::new(crate::test_support::sample_snapshot(tree), tx, false);
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
        let tree = crate::tree::Tree::build(vec![('M', "Cargo.toml".to_string())]);
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = crate::app::App::new(crate::test_support::sample_snapshot(tree), tx, false);
        app.focus = Focus::Tree;
        app.selection = 0;

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        let req = rx.try_recv().unwrap();
        match req {
            crate::snapshot::Request::Diff { path, .. } => assert_eq!(path, "Cargo.toml"),
            _ => panic!("expected Diff request"),
        }
    }

    #[test]
    fn space_in_diff_focus_toggles_hunk() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.height = 1;
        app.last_header_height = 1;
        app.diff_scroll = 0;
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            vec![hunk_header(0), ctx_line(0, 0)],
        ));
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
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            vec![hunk_header(0), ctx_line(0, 0)],
        ));
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
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            vec![hunk_header(0), ctx_line(0, 0)],
        ));
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
        app.snapshot = snapshot_with_diff(single_parsed(
            crate::diff::FileHeader {
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
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                hunk_header(1),
                ctx_line(1, 1),
            ],
        ));
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
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                ctx_line(2, 0),
            ],
        ));

        // Render once so height/diff_width/last_header_height/sticky_line fill in.
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();

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
    fn selection_moves_when_diff_fits() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.snapshot = snapshot_with_diff(fitting_hunk());
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
    fn chunk_start_end_motions() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        // Tall expanded chunk 0 (header + 5 ctx) then chunk 1.
        app.snapshot = snapshot_with_diff(single_parsed(
            header_full(None, None),
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                ctx_line(2, 0),
                ctx_line(3, 0),
                ctx_line(4, 0),
                hunk_header(1),
                ctx_line(5, 1),
            ],
        ));
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
        app.snapshot = snapshot_with_diff(fitting_hunk());
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
        app.snapshot = snapshot_with_diff(single_parsed(
            crate::diff::FileHeader {
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
            std::iter::once(hunk_header(0))
                .chain((0..10).map(|i| ctx_line(i, 0)))
                .collect(),
        ));
        assert!(!app.diff_fits());

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.diff_scroll > 0);
        assert_eq!(app.active_chunk, Some(0));
    }
}
