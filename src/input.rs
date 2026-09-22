//! Terminal event loop and key/mouse dispatch.
//!
//! Translates raw terminal events into `App` operations. It owns no state; all
//! state lives in `crate::app::App` and all drawing in `crate::view`.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{App, CONTEXT_STEP, HSCROLL_STEP};
use crate::perf::{self, Counter};
use crate::preview::{self, PreviewImage};
use crate::snapshot::{ArcSnapshot, Snapshot};
use crate::theme::Focus;
use crate::view;

/// Poll timeout while the worker is busy: also the spinner-tick cadence, so the
/// "scanning…" affordances are refreshed as state settles.
const BUSY_POLL: Duration = Duration::from_millis(100);
/// Poll timeout while idle: long enough to keep the loop quiet, short enough to
/// pick up worker snapshots that arrive without a terminal event.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// Redraw gating for the event loop: the frame is drawn only when something
/// marked it dirty, plus a periodic tick while the worker is busy so the UI
/// stays alive through long scans.
struct Redraw {
    dirty: bool,
    last_tick: Instant,
}

impl Redraw {
    fn new() -> Self {
        Redraw {
            dirty: true,
            last_tick: Instant::now(),
        }
    }

    /// Mark the frame dirty (an event or state change happened).
    fn mark(&mut self) {
        self.dirty = true;
    }

    /// Force a redraw on the spinner tick while `busy`.
    fn tick(&mut self, busy: bool) {
        if busy && self.last_tick.elapsed() >= BUSY_POLL {
            self.dirty = true;
            self.last_tick = Instant::now();
        }
    }

    /// Take the dirty flag, clearing it: `true` means draw this iteration.
    fn take(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

/// Drain the snapshot channel, keeping only the latest `ArcSnapshot`, and
/// apply it to `app`. Returns `true` when the app state changed (a snapshot was
/// applied, or the worker was found dead), so the caller can redraw.
fn drain_snapshots(rx: &Receiver<ArcSnapshot>, app: &mut App, worker_dead: &mut bool) -> bool {
    let mut latest: Option<ArcSnapshot> = None;
    let mut drained = 0usize;
    while let Ok(snap) = rx.try_recv() {
        latest = Some(snap);
        drained += 1;
    }
    let mut changed = false;
    if let Some(snap) = latest {
        app.apply_snapshot(snap);
        perf::tally(Counter::SnapshotsApplied);
        if drained > 1 {
            crate::perf_evt!("snapshot drain n={drained}");
        }
        changed = true;
    }
    if !*worker_dead && matches!(rx.try_recv(), Err(TryRecvError::Disconnected)) {
        *worker_dead = true;
        app.snapshot = Arc::new(Snapshot {
            seq: app.snapshot.seq,
            busy: false,
            empty: app.snapshot.empty,
            tree: app.snapshot.tree.clone(),
            selected_diff: None,
            selected_highlight: Arc::new(Vec::new()),
            error: Some("worker stopped: live updates disabled".into()),
            branch_state: app.snapshot.branch_state.clone(),
            uncommitted_stats: app.snapshot.uncommitted_stats,
        });
        changed = true;
    }
    changed
}

/// How one wait for terminal input resolved.
#[derive(Debug)]
enum Wake {
    /// An event arrived from the reader thread.
    Event(Event),
    /// No event before the timeout; the loop should re-check liveness.
    Idle,
    /// The input reader ended (read error) or vanished: leave the loop.
    Closed,
}

/// Map a reader-channel receive into a [`Wake`]. Pure, so the loop's exit
/// decisions stay unit-testable without a live terminal.
fn classify_wake(result: Result<io::Result<Event>, RecvTimeoutError>) -> Wake {
    match result {
        Ok(Ok(event)) => Wake::Event(event),
        // Only terminal-fatal read errors end the loop; a transient one is
        // retried on the next iteration.
        Ok(Err(e)) if is_fatal_read_error(&e) => Wake::Closed,
        Ok(Err(_)) => Wake::Idle,
        Err(RecvTimeoutError::Timeout) => Wake::Idle,
        // The reader thread is gone; nothing can arrive again.
        Err(RecvTimeoutError::Disconnected) => Wake::Closed,
    }
}

/// True for read errors that mean the terminal is permanently gone: EOF, or a
/// raw `ENXIO`/`EIO`/`ENODEV`. Transient errors (`EINTR`, `WouldBlock`, any
/// unknown kind) are not fatal — the `terminal_gone()` probe still catches a
/// genuinely dead tty.
fn is_fatal_read_error(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        return true;
    }
    matches!(
        e.raw_os_error(),
        Some(libc::ENXIO | libc::EIO | libc::ENODEV)
    )
}

/// Spawn the terminal-input reader. Crossterm's event source owns a single
/// global handle and, once the pty is closed, spins inside `event::read`
/// without ever returning an error; a dedicated thread keeps that spin off the
/// run loop, which detects the dead terminal via [`terminal_gone`] instead.
fn spawn_reader() -> io::Result<Receiver<io::Result<Event>>> {
    let (tx, rx) = crossbeam_channel::unbounded::<io::Result<Event>>();
    thread::Builder::new()
        .name("bough-input".into())
        .spawn(move || loop {
            match event::read() {
                Ok(event) => {
                    if tx.send(Ok(event)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    // Transient errors are retried; only fatal ones end the
                    // reader, so the channel never disconnects spuriously.
                    if is_fatal_read_error(&e) {
                        crate::perf_evt!("input read fatal error kind={:?}", e.kind());
                        let _ = tx.send(Err(e));
                        break;
                    }
                    perf::tally(Counter::ReadRetries);
                }
            }
        })?;
    Ok(rx)
}

/// True when the terminal is no longer reachable. A closed pty makes crossterm's
/// reader spin rather than surface an error, and its window-size ioctl keeps
/// returning a stale size, so the loop probes the write path instead: a
/// save/restore cursor pair changes no state but fails once the far end is gone.
fn terminal_gone() -> bool {
    use std::io::Write;
    let mut out = io::stdout().lock();
    out.write_all(b"\x1b7\x1b8")
        .and_then(|_| out.flush())
        .is_err()
}

/// Apply one terminal event. Returns `true` when the app should quit.
fn handle_event(event: Event, app: &mut App, redraw: &mut Redraw) -> bool {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Release {
                perf::tally(Counter::KeyEvents);
                redraw.mark();
                return handle_key(key, app);
            }
        }
        Event::Mouse(m) => {
            app.fold_key = false;
            app.g_key = false;
            handle_mouse(m, app);
            redraw.mark();
        }
        Event::Resize(..) => {
            app.fold_key = false;
            app.g_key = false;
            redraw.mark();
        }
        Event::FocusGained | Event::FocusLost => {
            redraw.mark();
        }
        _ => {}
    }
    false
}

/// Run the interactive loop until the user quits.
pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    snapshot_rx: &Receiver<ArcSnapshot>,
    image_rx: &Receiver<PreviewImage>,
) -> io::Result<()> {
    let events = spawn_reader()?;
    let mut worker_dead = false;
    let mut redraw = Redraw::new();
    let mut last_beat = Instant::now();
    loop {
        if last_beat.elapsed() >= Duration::from_secs(1) {
            perf::beat();
            last_beat = Instant::now();
        }
        redraw.tick(app.is_busy());
        if drain_snapshots(snapshot_rx, app, &mut worker_dead) {
            redraw.mark();
        }
        if let Some(img) = preview::drain_previews(image_rx) {
            app.preview.accept(img);
            redraw.mark();
        }
        app.maybe_request_highlight();

        if redraw.take() {
            terminal.draw(|f| view::ui(f, app))?;
            perf::tally(Counter::Draws);
            let _ = preview::sync(&mut io::stdout(), &mut app.preview);
            perf::tally(Counter::PreviewSyncs);
        }

        let timeout = if app.is_busy() { BUSY_POLL } else { IDLE_POLL };
        let wake = classify_wake(events.recv_timeout(timeout));
        perf::tally(Counter::PollWakes);
        match wake {
            Wake::Event(event) => {
                if handle_event(event, app, &mut redraw) {
                    crate::perf_evt!("quit key");
                    break;
                }
            }
            Wake::Closed => {
                crate::perf_evt!("input closed");
                break;
            }
            Wake::Idle => {
                perf::tally(Counter::TerminalProbes);
                if terminal_gone() {
                    crate::perf_evt!("terminal gone");
                    break;
                }
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
    let tree_start = app.diff_width.saturating_add(1);
    let in_tree = mouse.column >= tree_start;
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
    // Ctrl-C always quits, regardless of focus.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }

    // An open base picker captures every key (including `q`) until closed.
    if app.base_picker.is_some() {
        return handle_picker_key(key.code, app);
    }

    // Modifier-prefix state: a second `z` (fold) or `g` (goto) chord consumes
    // this very key before normal dispatch.
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
        KeyCode::Char('q') | KeyCode::Esc => true,
        KeyCode::Char('w') => {
            app.word_diff_on = !app.word_diff_on;
            false
        }
        KeyCode::Char('m') => {
            app.toggle_mode();
            false
        }
        KeyCode::Char('b') => {
            app.open_branch_picker();
            false
        }
        KeyCode::Char('t') => {
            app.open_commit_picker();
            false
        }
        KeyCode::Char('z') => {
            app.fold_key = true;
            false
        }
        KeyCode::Char('g') => {
            app.g_key = true;
            false
        }
        KeyCode::Char('G') => {
            app.go_last_chunk();
            false
        }
        KeyCode::Char(' ') => {
            app.space_action();
            false
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Tree => Focus::Diff,
                Focus::Diff => Focus::Tree,
            };
            false
        }
        // Any other key: shift overrides the diff even from tree focus, then
        // fall through to focus-specific keys.
        _ => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::Char('H') => app.diff_scroll_left(HSCROLL_STEP),
                    KeyCode::Char('J') => app.diff_scroll_down(1),
                    KeyCode::Char('K') => app.diff_scroll_up(1),
                    KeyCode::Char('L') => app.diff_scroll_right(HSCROLL_STEP),
                    _ => return focus_key(key, app),
                }
                false
            } else {
                focus_key(key, app)
            }
        }
    }
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

/// Handle a key while the base picker is open. Every key is consumed so the
/// popup owns focus until it closes (`b`, `t`, `q`, or Esc) — `q` must close
/// the popup rather than quit the app.
fn handle_picker_key(code: KeyCode, app: &mut App) -> bool {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(p) = app.base_picker.as_mut() {
                if !p.entries.is_empty() {
                    p.selection = (p.selection + 1).min(p.entries.len() - 1);
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(p) = app.base_picker.as_mut() {
                p.selection = p.selection.saturating_sub(1);
            }
        }
        KeyCode::Enter => app.select_picked_base(),
        KeyCode::Char('b') | KeyCode::Char('t') | KeyCode::Char('q') | KeyCode::Esc => {
            app.close_picker()
        }
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
            KeyCode::PageDown => {
                app.tree_page_down();
                false
            }
            KeyCode::PageUp => {
                app.tree_page_up();
                false
            }
            KeyCode::Home => {
                app.tree_first();
                false
            }
            KeyCode::End => {
                app.tree_last();
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
    use crate::diff::{FileHeader, FileStatus, HunkMeta};
    use crate::test_support::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn classify_wake_fatal_read_error_exits() {
        // ENXIO: the tty's far end is gone, and crossterm's EOF kind.
        let enxio: io::Result<Event> = Err(io::Error::from_raw_os_error(libc::ENXIO));
        assert!(matches!(classify_wake(Ok(enxio)), Wake::Closed));
        let eof: io::Result<Event> = Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        assert!(matches!(classify_wake(Ok(eof)), Wake::Closed));
    }

    #[test]
    fn classify_wake_transient_read_error_retries() {
        // A transient read error must not spuriously quit the app; the loop
        // re-checks liveness on the next iteration.
        for kind in [
            io::ErrorKind::Interrupted,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Other,
        ] {
            let err: io::Result<Event> = Err(io::Error::from(kind));
            assert!(
                matches!(classify_wake(Ok(err)), Wake::Idle),
                "{kind:?} must be treated as transient"
            );
        }
    }

    #[test]
    fn classify_wake_disconnect_exits() {
        assert!(matches!(
            classify_wake(Err(RecvTimeoutError::Disconnected)),
            Wake::Closed
        ));
    }

    #[test]
    fn classify_wake_timeout_is_idle() {
        assert!(matches!(
            classify_wake(Err(RecvTimeoutError::Timeout)),
            Wake::Idle
        ));
    }

    #[test]
    fn classify_wake_event_passes_through() {
        assert!(matches!(
            classify_wake(Ok(Ok(Event::FocusGained))),
            Wake::Event(_)
        ));
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
        app.snapshot = snapshot_with_diff(sample_diff(3));

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
        app.snapshot = snapshot_with_diff(sample_diff(3));
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
        app.snapshot = snapshot_with_diff(single_diff(
            FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                lfs: None,
                hunks: vec![HunkMeta {
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
        let mut app = crate::app::App::new(
            crate::test_support::sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.focus = Focus::Tree;
        app.selection = 0;
        let idx = app.flat[0];
        assert!(!app.tree.nodes[idx].expanded);

        // First Space expands the collapsed dir.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.tree.nodes[idx].expanded);
        // Second Space collapses it again.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.tree.nodes[idx].expanded);
    }

    #[test]
    fn space_on_file_loads_diff() {
        let tree = crate::tree::Tree::build(vec![('M', "Cargo.toml".to_string())]);
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(4);
        let mut app = crate::app::App::new(
            crate::test_support::sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
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
        app.snapshot = snapshot_with_diff(single_diff(
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
        app.snapshot = snapshot_with_diff(single_diff(
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
        app.snapshot = snapshot_with_diff(single_diff(
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
        app.snapshot = snapshot_with_diff(single_diff(
            crate::diff::FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 1,
                deleted: 1,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                lfs: None,
                hunks: vec![
                    HunkMeta {
                        added: 1,
                        deleted: 1,
                    },
                    HunkMeta {
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
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![
                hunk_header(0),
                ctx_line(0, 0),
                ctx_line(1, 0),
                ctx_line(2, 0),
            ],
        ));

        // Render once so height/diff_width/last_header_height/sticky_file/sticky_hunk fill in.
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
        app.snapshot = snapshot_with_diff(single_diff(
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
        app.snapshot = snapshot_with_diff(single_diff(
            crate::diff::FileHeader {
                path: "x.rs".into(),
                status: FileStatus::Modified,
                added: 10,
                deleted: 0,
                old_path: None,
                rename_percent: None,
                mode_old: None,
                mode_new: None,
                binary: false,
                lfs: None,
                hunks: vec![HunkMeta {
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

    /// A picker entry with `rev` used as both revision spec and label.
    fn entry(rev: &str) -> crate::app::PickerEntry {
        crate::app::PickerEntry {
            rev: rev.to_string(),
            label: rev.to_string(),
        }
    }

    /// An `App` with the base picker open on a fixed branch list. The initial
    /// `Diff` request from `App::new` is drained so later requests are
    /// unambiguous.
    fn picker_app() -> (App, Receiver<crate::snapshot::Request>) {
        let tree = crate::tree::Tree::build(vec![('M', "a.rs".to_string())]);
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(8);
        let mut app = App::new(
            crate::test_support::sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_picker_with(
            " compare against a branch ",
            vec![entry("develop"), entry("feature")],
        );
        (app, rx)
    }

    #[test]
    fn picker_b_closes_popup() {
        let (mut app, _rx) = picker_app();
        assert!(app.base_picker.is_some());
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
    }

    #[test]
    fn picker_t_closes_popup() {
        let (mut app, _rx) = picker_app();
        assert!(app.base_picker.is_some());
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
    }

    #[test]
    fn t_opens_commit_picker() {
        let mut app = sample_app();
        assert!(app.base_picker.is_none());
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_some());
    }

    #[test]
    fn picker_j_k_move_selection() {
        let (mut app, _rx) = picker_app();
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1);
        // Clamped at the end.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);
        // Clamped at the start.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);
    }

    #[test]
    fn picker_enter_picks_and_sends_setrev() {
        let (mut app, rx) = picker_app();
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
        assert_eq!(
            app.mode,
            crate::app::Mode::Branch(crate::app::Origin::Picked("feature".to_string()))
        );
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "feature...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn picker_enter_on_commit_entry_sends_oid_setrev() {
        let tree = crate::tree::Tree::build(vec![('M', "a.rs".to_string())]);
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(8);
        let mut app = App::new(
            crate::test_support::sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        let _ = rx.try_recv();
        app.open_commit_picker_with(vec![crate::app::PickerEntry {
            rev: "abcdef1234567890".to_string(),
            label: "abcdef1 subject".to_string(),
        }]);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
        assert_eq!(
            app.mode,
            crate::app::Mode::Branch(crate::app::Origin::Picked("abcdef1234567890".to_string()))
        );
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => {
                assert_eq!(rev, "abcdef1234567890...HEAD")
            }
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn picker_esc_closes() {
        let (mut app, _rx) = picker_app();
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
    }

    #[test]
    fn picker_q_closes_popup_not_app() {
        let (mut app, _rx) = picker_app();
        // `q` while the popup is open closes it and must not quit.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
    }

    #[test]
    fn drain_snapshots_reports_state_change() {
        let mut app = sample_app();
        let (tx, rx) = crossbeam_channel::unbounded::<ArcSnapshot>();
        let mut dead = false;

        // Nothing queued: no redraw warranted.
        assert!(!drain_snapshots(&rx, &mut app, &mut dead));
        assert!(!dead);

        tx.send(snapshot_with_diff(sample_diff(2))).unwrap();
        assert!(drain_snapshots(&rx, &mut app, &mut dead));
        // Drained: a second pass changes nothing.
        assert!(!drain_snapshots(&rx, &mut app, &mut dead));

        // A closed channel marks the worker dead and warrants one more redraw.
        drop(tx);
        assert!(drain_snapshots(&rx, &mut app, &mut dead));
        assert!(dead);
        assert!(!drain_snapshots(&rx, &mut app, &mut dead));
    }

    #[test]
    fn redraw_gates_idle_frames() {
        let mut redraw = Redraw::new();
        // The first frame always draws.
        assert!(redraw.take());
        // Idle: no draw until something marks the frame dirty.
        assert!(!redraw.take());
        redraw.mark();
        assert!(redraw.take());
        assert!(!redraw.take());
    }

    #[test]
    fn redraw_ticks_only_while_busy() {
        let mut redraw = Redraw::new();
        let _ = redraw.take();
        redraw.tick(false);
        assert!(!redraw.take(), "idle never forces a tick");

        redraw.last_tick = Instant::now() - BUSY_POLL;
        redraw.tick(true);
        assert!(redraw.take(), "busy forces a redraw on the tick");
        // The tick resets, so an immediate check is not yet due.
        redraw.tick(true);
        assert!(!redraw.take());
    }

    #[test]
    fn tree_page_keys_via_input() {
        let tree = crate::tree::Tree::build(
            (0..20)
                .map(|i| ('M', format!("f{i:02}.rs")))
                .collect::<Vec<_>>(),
        );
        let (tx, _rx) = crossbeam_channel::unbounded::<crate::snapshot::Request>();
        let mut app = App::new(
            crate::test_support::sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.height = 6; // tree page = 5 rows
        app.focus = Focus::Tree;

        assert!(!handle_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.selection, 5);
        assert_eq!(app.tree_scroll, 1);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.selection, 19);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.selection, 0);
        assert_eq!(app.tree_scroll, 0);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.selection, 0);
    }
}
