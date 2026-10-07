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
use ratatui::layout::Rect;
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
/// Backoff steps (milliseconds) for consecutive transient reader errors: a
/// persistent non-fatal error must not spin the reader thread at 100% CPU.
const READ_BACKOFF_MS: [u64; 3] = [1, 10, 100];
/// Consecutive transient reader errors before the reader treats the terminal
/// as effectively gone and ends the loop.
const READ_GIVE_UP_AFTER: u32 = 1000;
/// Message on the synthetic error sent when the reader gives up; recognized by
/// [`classify_wake`] as fatal.
const READER_GAVE_UP: &str = "input reader gave up after repeated transient errors";
/// Maximum terminal events handled per loop iteration. A mouse-motion storm
/// otherwise forces one redraw per queued event.
const MAX_EVENTS_PER_FRAME: usize = 64;

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
        // Terminal-fatal read errors — and the reader's synthetic "gave up
        // after N transient errors" — end the loop.
        Ok(Err(e)) if is_fatal_read_error(&e) || is_reader_gave_up(&e) => Wake::Closed,
        // Defense-in-depth: the reader retries transient errors itself and only
        // sends fatal ones, but any other queued error is treated as transient
        // so a stray one can't spuriously quit.
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

/// The synthetic error the reader sends after giving up on repeated transient
/// errors; [`classify_wake`] treats it as fatal.
fn gave_up_error() -> io::Error {
    io::Error::other(READER_GAVE_UP)
}

/// True when `e` is the sentinel from [`gave_up_error`].
fn is_reader_gave_up(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string() == READER_GAVE_UP
}

/// Backoff after `consecutive` transient read errors: 1ms, 10ms, then 100ms.
fn read_backoff(consecutive: u32) -> Duration {
    let step = (consecutive.saturating_sub(1) as usize).min(READ_BACKOFF_MS.len() - 1);
    Duration::from_millis(READ_BACKOFF_MS[step])
}

/// Spawn the terminal-input reader. Crossterm's event source owns a single
/// global handle and, once the pty is closed, spins inside `event::read`
/// without ever returning an error; a dedicated thread keeps that spin off the
/// run loop, which detects the dead terminal via [`terminal_gone`] instead.
fn spawn_reader() -> io::Result<Receiver<io::Result<Event>>> {
    let (tx, rx) = crossbeam_channel::unbounded::<io::Result<Event>>();
    thread::Builder::new()
        .name("bough-input".into())
        .spawn(move || {
            let mut consecutive = 0u32;
            loop {
                match event::read() {
                    Ok(event) => {
                        consecutive = 0;
                        if tx.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        // Transient errors are retried with escalating backoff
                        // (a persistent unknown errno must not spin this
                        // thread); only fatal ones end the reader.
                        if is_fatal_read_error(&e) {
                            crate::perf_evt!("input read fatal error kind={:?}", e.kind());
                            let _ = tx.send(Err(e));
                            break;
                        }
                        consecutive += 1;
                        if consecutive >= READ_GIVE_UP_AFTER {
                            crate::perf_evt!(
                                "input read gave up after {consecutive} transient errors"
                            );
                            let _ = tx.send(Err(gave_up_error()));
                            break;
                        }
                        perf::tally(Counter::ReadRetries);
                        thread::sleep(read_backoff(consecutive));
                    }
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
    let mut flash_live = false;
    loop {
        if last_beat.elapsed() >= Duration::from_secs(1) {
            perf::beat();
            last_beat = Instant::now();
        }
        redraw.tick(app.is_busy());
        if drain_snapshots(snapshot_rx, app, &mut worker_dead) {
            redraw.mark();
        }
        // The worktree picker's dirty chips fill in asynchronously.
        if app.poll_dirty_counts() {
            redraw.mark();
        }
        if let Some(img) = preview::drain_previews(image_rx) {
            app.preview.accept(img);
            redraw.mark();
        }
        app.maybe_request_highlight();

        // `status_line` drops a stale flash only on the next draw, and the idle
        // wait never wakes for its expiry. Catch the fresh→stale transition to
        // force that draw, and cap the wait so it lands right after expiry.
        let flash_fresh = app
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < view::FLASH_TTL);
        if flash_live && !flash_fresh {
            redraw.mark();
        }
        flash_live = flash_fresh;

        if redraw.take() {
            terminal.draw(|f| view::ui(f, app))?;
            perf::tally(Counter::Draws);
            let _ = preview::sync(&mut io::stdout(), &mut app.preview);
            perf::tally(Counter::PreviewSyncs);
        }

        let mut timeout = if app.is_busy() { BUSY_POLL } else { IDLE_POLL };
        if flash_fresh {
            if let Some((_, at)) = &app.flash {
                timeout = timeout.min(view::FLASH_TTL.saturating_sub(at.elapsed()));
            }
        }
        let wake = classify_wake(events.recv_timeout(timeout));
        perf::tally(Counter::PollWakes);
        match wake {
            Wake::Event(event) => {
                let mut quit = false;
                let mut drained = 0usize;
                let mut next = Some(event);
                // Bulk-drain a burst so a mouse-motion storm cannot force one
                // redraw per event: pure motion is dropped, everything else is
                // handled (scrolling applies per event), and the frame is
                // marked dirty by the handlers (a single boolean).
                while let Some(e) = next.take() {
                    drained += 1;
                    if !matches!(&e, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Moved)) {
                        quit = handle_event(e, app, &mut redraw);
                    }
                    if quit || drained >= MAX_EVENTS_PER_FRAME {
                        break;
                    }
                    match events.try_recv() {
                        Ok(Ok(e)) => next = Some(e),
                        // The reader only sends fatal errors.
                        Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                            crate::perf_evt!("input closed");
                            return Ok(());
                        }
                        Err(TryRecvError::Empty) => break,
                    }
                }
                if quit {
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

/// True when `column` lies in the tree pane. The diff pane owns
/// `[0, diff_width)` and the one-column divider sits at `diff_width`, so the
/// tree starts after it.
fn in_tree_pane(app: &App, column: u16) -> bool {
    column >= app.diff_width.saturating_add(1)
}

/// True when `(column, row)` falls inside `rect`. `None` is never inside.
fn rect_contains(rect: Option<Rect>, column: u16, row: u16) -> bool {
    rect.is_some_and(|r| {
        column >= r.x
            && column < r.x.saturating_add(r.width)
            && row >= r.y
            && row < r.y.saturating_add(r.height)
    })
}

fn handle_mouse(mouse: MouseEvent, app: &mut App) {
    // An open base picker owns the mouse: every event is consumed while it is
    // up, so nothing falls through to the panes underneath.
    if app.base_picker.is_some() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                app.click_picker(mouse.column, mouse.row);
            }
            MouseEventKind::ScrollUp => app.picker_scroll(true),
            MouseEventKind::ScrollDown => app.picker_scroll(false),
            MouseEventKind::Down(MouseButton::Right) => app.close_picker(),
            // A picker opened mid-drag swallows the release; cancel the drag
            // so no later Up can copy a stale range.
            MouseEventKind::Up(MouseButton::Left) => app.cancel_diff_selection(),
            _ => {}
        }
        return;
    }

    let column = mouse.column;
    let row = mouse.row;
    let in_footer = rect_contains(app.mouse.footer, column, row);

    // A release ends an in-progress diff drag even if a preview appeared
    // mid-drag: the preview gate below would otherwise swallow it and leave the
    // anchor live, so the next Up anywhere would copy a stale range.
    if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
        && app.diff_selection.anchor.is_some()
    {
        app.finish_diff_selection();
        return;
    }

    // A preview covers the diff pane: hunk hit-testing and diff scrolling
    // would target invisible content. The footer row and the tree pane are
    // outside the preview, so they stay interactive.
    if app.preview.active() && column < app.diff_width && !in_footer {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if in_footer {
                app.click_footer(column, false);
            } else if in_tree_pane(app, column) {
                // Any click in the tree pane — header strip, entry, or empty
                // space below — focuses the tree pane.
                app.focus = Focus::Tree;
                if rect_contains(app.mouse.tree_list, column, row) {
                    app.click_tree_row(row);
                }
            } else if column < app.diff_width {
                // A click on a hunk header toggles it; otherwise it starts a
                // linewise text selection (or, past the last line, just
                // focuses the pane).
                if let Some(hunk) = app.hunk_at_screen_row(row) {
                    app.toggle_hunk(hunk);
                } else if !app.begin_diff_selection(row) {
                    app.focus = Focus::Diff;
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if column < app.diff_width && !in_footer {
                app.extend_diff_selection(row);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // Releases reaching here have no live drag anchor (the pre-gate
            // handler above finishes those); nothing to do.
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if in_footer {
                app.click_footer(column, true);
            } else if in_tree_pane(app, column) && rect_contains(app.mouse.tree_list, column, row) {
                app.copy_tree_path(row);
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if in_footer {
                return;
            }
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            // Shift+wheel scrolls the diff horizontally: most terminals
            // translate a horizontal wheel (or Shift+wheel) into
            // Shift+vertical events, never native `ScrollLeft`/`ScrollRight`.
            if mouse.modifiers.contains(KeyModifiers::SHIFT) && column < app.diff_width {
                if up {
                    app.diff_scroll_left(HSCROLL_STEP);
                } else {
                    app.diff_scroll_right(HSCROLL_STEP);
                }
                return;
            }
            let over_tree =
                rect_contains(app.mouse.tree_list, column, row) || in_tree_pane(app, column);
            if over_tree {
                if up {
                    app.move_up(1);
                } else {
                    app.move_down(1);
                }
            } else if up {
                app.diff_scroll_up(1);
            } else {
                app.diff_scroll_down(1);
            }
        }
        MouseEventKind::ScrollLeft if column < app.diff_width => {
            app.diff_scroll_left(HSCROLL_STEP);
        }
        MouseEventKind::ScrollRight if column < app.diff_width => {
            app.diff_scroll_right(HSCROLL_STEP);
        }
        _ => {}
    }
}

fn handle_key(key: KeyEvent, app: &mut App) -> bool {
    // Ctrl-C always quits, regardless of focus.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }

    // An open base picker captures every key (including `q`) until closed.
    if app.base_picker.is_some() {
        return handle_picker_key(key, app);
    }

    // Esc always quits, even with a pending `z`/`g` chord: clear the chord
    // first so it cannot swallow the key.
    if key.code == KeyCode::Esc {
        app.fold_key = false;
        app.g_key = false;
        return true;
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
        KeyCode::Char('q') => true,
        KeyCode::Char('w') => {
            app.open_worktree_picker();
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
        KeyCode::Char('W') => {
            app.word_diff_on = !app.word_diff_on;
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
/// popup owns focus until it closes. With an empty filter the close keys
/// (`b`, `t`, `w`, `q`, Esc) close the popup; once the filter is non-empty
/// they type into it instead (filtering by name is the whole point), and Esc
/// clears the filter first — a second Esc closes. Ctrl/Alt chords (e.g.
/// Ctrl+U) are consumed but never typed into the filter.
fn handle_picker_key(key: KeyEvent, app: &mut App) -> bool {
    let filtering = app
        .base_picker
        .as_ref()
        .is_some_and(|p| !p.filter.is_empty());
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => app.picker_step(true),
        KeyCode::Char('k') | KeyCode::Up => app.picker_step(false),
        KeyCode::Enter => app.select_picked_base(),
        KeyCode::Esc => {
            if filtering {
                app.picker_clear_filter();
            } else {
                app.close_picker();
            }
        }
        KeyCode::Backspace => app.picker_filter_backspace(),
        KeyCode::Char('b') | KeyCode::Char('t') | KeyCode::Char('w') | KeyCode::Char('q')
            if !filtering =>
        {
            app.close_picker()
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.picker_push_filter(c)
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
    use crate::diff::{DiffLine, FileHeader, FileStatus, HunkMeta, LineKind};
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
    fn classify_wake_reader_gave_up_is_fatal() {
        // The reader's synthetic error after repeated transient failures must
        // end the loop rather than being treated as transient.
        let err: io::Result<Event> = Err(gave_up_error());
        assert!(matches!(classify_wake(Ok(err)), Wake::Closed));
    }

    #[test]
    fn read_backoff_escalates_then_caps() {
        assert_eq!(read_backoff(1), Duration::from_millis(1));
        assert_eq!(read_backoff(2), Duration::from_millis(10));
        assert_eq!(read_backoff(3), Duration::from_millis(100));
        // Stays capped however long the failure persists.
        assert_eq!(read_backoff(1_000), Duration::from_millis(100));
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
    fn esc_quits_even_with_pending_chord() {
        // A pending `z`/`g` chord must not swallow Esc: Esc clears the chord
        // and quits.
        let mut app = sample_app();
        app.fold_key = true;
        assert!(handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.fold_key);

        app.g_key = true;
        assert!(handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.g_key);
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
    fn mouse_ignores_diff_pane_when_preview_active() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![hunk_header(0), ctx_line(0, 0), ctx_line(1, 0)],
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();
        assert!(app.collapsed_hunks.is_empty());

        // A preview covering the diff pane suppresses hunk hit-testing.
        app.preview.set_enabled(true);
        app.preview.current = Some(PreviewImage {
            path: "x.rs".into(),
            png: Vec::new(),
            width: 1,
            height: 1,
        });
        assert!(app.preview.active());

        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
        );
        assert!(
            app.collapsed_hunks.is_empty(),
            "a preview must suppress hunk toggling"
        );

        // Once the preview is gone the header toggles again.
        app.preview.set_enabled(false);
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
        );
        assert!(app.collapsed_hunks.contains(&0));
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
    fn picker_ctrl_chords_do_not_type_into_filter() {
        let (mut app, _rx) = picker_app();
        // A Ctrl-chord is consumed but must not type into the filter
        // (Ctrl+C quitting is handled earlier in `handle_key`).
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut app
        ));
        assert!(app.base_picker.is_some());
        assert!(
            app.base_picker.as_ref().unwrap().filter.is_empty(),
            "Ctrl+U must not type into the filter"
        );
        // Plain characters still type.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().filter, "u");
    }

    #[test]
    fn t_opens_commit_picker() {
        // Real opener: reads the global repo root, so it must serialize
        // against tests that point it at temporary worktrees.
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
    fn w_while_picker_open_closes_it() {
        // The picker owns focus: `w` must not steal it (it closes, like `b`).
        let (mut app, _rx) = picker_app();
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
    }

    #[test]
    fn filter_letters_type_into_the_filter_and_narrow() {
        // With an empty filter, `t` would close the popup; once the filter is
        // non-empty, close keys type into it instead.
        let (mut app, rx) = picker_app();
        // `u`/`r`/`e` are not close keys; they build the filter "ure", which
        // only `feature` contains.
        for code in ['u', 'r', 'e'] {
            assert!(
                !handle_key(
                    KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE),
                    &mut app
                ),
                "typing never quits"
            );
            assert!(app.base_picker.is_some(), "typing keeps the picker open");
        }
        // "ure" matches only `feature`; the selection re-anchors onto it.
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.filter, "ure");
        assert_eq!(picker.filtered_indices(), vec![1]);
        assert_eq!(picker.selection, 1);

        // j/k stay inside the filtered subset; Enter picks the match.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1, "clamped");
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none());
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "feature...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn filter_close_keys_type_then_backspace_restores_close_keys() {
        let (mut app, _rx) = picker_app();
        // "e" matches both rows, so the filter is non-empty and `b`, `t`,
        // `w`, `q` type into it instead of closing.
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut app
        ));
        for code in ['b', 't', 'w', 'q'] {
            assert!(!handle_key(
                KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE),
                &mut app
            ));
        }
        assert_eq!(app.base_picker.as_ref().unwrap().filter, "ebtwq");
        assert!(app.base_picker.is_some(), "close keys typed, not closed");

        // Backspace strips the filter; once empty, `w` closes again.
        for expected in ["ebtw", "ebt", "eb", "e", ""] {
            assert!(!handle_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                &mut app
            ));
            assert_eq!(app.base_picker.as_ref().unwrap().filter, expected);
        }
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none(), "empty filter: `w` closes");
    }

    #[test]
    fn filter_esc_clears_then_second_esc_closes() {
        let (mut app, _rx) = picker_app();
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.base_picker.as_ref().unwrap().filter, "zz");

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app
        ));
        let picker = app.base_picker.as_ref().unwrap();
        assert!(picker.filter.is_empty(), "first Esc clears the filter");
        assert_eq!(picker.filtered_indices(), vec![0, 1]);

        assert!(!handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app
        ));
        assert!(app.base_picker.is_none(), "second Esc closes");
    }

    #[test]
    fn filter_matching_is_case_insensitive_via_keys() {
        let (mut app, _rx) = picker_app();
        for code in ['D', 'E', 'V'] {
            assert!(!handle_key(
                KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE),
                &mut app
            ));
        }
        let picker = app.base_picker.as_ref().unwrap();
        assert_eq!(picker.filter, "DEV");
        assert_eq!(picker.filtered_indices(), vec![0]);
    }

    #[test]
    fn w_opens_worktree_picker_not_word_diff() {
        // Real opener: reads the global repo root (see t_opens_commit_picker).
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut app = sample_app();
        let word_diff = app.word_diff_on;
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            &mut app
        ));
        assert_eq!(app.word_diff_on, word_diff, "w must not toggle word diff");
        assert!(app.base_picker.is_some(), "w opens the worktree picker");
        assert_eq!(
            app.base_picker.as_ref().unwrap().kind,
            crate::app::PickerKind::Worktree
        );
    }

    #[test]
    fn shift_w_toggles_word_diff() {
        let mut app = sample_app();
        let word_diff = app.word_diff_on;
        assert!(!handle_key(
            KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT),
            &mut app
        ));
        assert_ne!(app.word_diff_on, word_diff, "Shift-W toggles word diff now");
        assert!(app.base_picker.is_none(), "Shift-W no longer opens pickers");
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

    /// A synthetic mouse event with no modifiers.
    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// An `App` with two files and a live request channel; the initial `Diff`
    /// request is drained so later requests are unambiguous.
    fn routed_app() -> (App, Receiver<crate::snapshot::Request>) {
        let tree =
            crate::tree::Tree::build(vec![('M', "a.rs".to_string()), ('M', "b.rs".to_string())]);
        let (tx, rx) = crossbeam_channel::bounded::<crate::snapshot::Request>(8);
        let mut app = App::new(
            crate::test_support::sample_snapshot(tree),
            tx,
            false,
            "main...HEAD".to_string(),
            "HEAD".to_string(),
        );
        app.selection = 0;
        let _ = rx.try_recv();
        (app, rx)
    }

    #[test]
    fn mouse_click_tree_row_routes_to_selection() {
        let (mut app, rx) = routed_app();
        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        // row 2 → idx = tree_scroll(0) + (2 - 1) = 1 → b.rs.
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 2),
            &mut app,
        );
        assert_eq!(app.selection, 1);
        assert_eq!(app.focus, Focus::Tree);
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::Diff { path, .. } => assert_eq!(path, "b.rs"),
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn mouse_wheel_over_tree_moves_selection() {
        let (mut app, _rx) = routed_app();
        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        handle_mouse(mouse(MouseEventKind::ScrollDown, 45, 3), &mut app);
        assert_eq!(app.selection, 1);
        handle_mouse(mouse(MouseEventKind::ScrollUp, 45, 3), &mut app);
        assert_eq!(app.selection, 0);
    }

    #[test]
    fn mouse_wheel_over_diff_scrolls() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            std::iter::once(hunk_header(0))
                .chain((0..20).map(|i| ctx_line(i, 0)))
                .collect(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();
        app.diff_scroll = 0;

        handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 5), &mut app);
        assert!(app.diff_scroll > 0);
        let down = app.diff_scroll;
        handle_mouse(mouse(MouseEventKind::ScrollUp, 0, 5), &mut app);
        assert!(app.diff_scroll < down);
    }

    #[test]
    fn mouse_horizontal_scroll_over_diff() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![
                hunk_header(0),
                DiffLine {
                    kind: LineKind::Context,
                    raw_text: "y".repeat(300),
                    section: 0,
                    word_runs: None,
                },
                ctx_line(1, 0),
            ],
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();
        app.diff_col = 0;

        handle_mouse(mouse(MouseEventKind::ScrollRight, 0, 5), &mut app);
        assert!(app.diff_col > 0, "ScrollRight shows content further right");
        handle_mouse(mouse(MouseEventKind::ScrollLeft, 0, 5), &mut app);
        assert_eq!(app.diff_col, 0);
        handle_mouse(mouse(MouseEventKind::ScrollLeft, 0, 5), &mut app);
        assert_eq!(app.diff_col, 0, "clamped at the left edge");
    }

    /// A body line with the given kind and raw text, in hunk 0.
    fn marked(kind: LineKind, raw: &str) -> DiffLine {
        DiffLine {
            kind,
            raw_text: raw.into(),
            section: 0,
            word_runs: None,
        }
    }

    /// A rendered app with a four-line single-file diff, so `line_at_screen_row`
    /// is live: screen row 1 is the hunk header, row 2.. line 1..3.
    fn selection_app() -> App {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![
                hunk_header(0),
                marked(LineKind::Added, "one"),
                marked(LineKind::Context, "two"),
                marked(LineKind::Removed, "three"),
            ],
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();
        app.diff_scroll = 0;
        app.diff_selection = Default::default();
        app
    }

    #[test]
    fn mouse_drag_over_diff_selects_and_up_copies() {
        let mut app = selection_app();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 2),
            &mut app,
        );
        assert_eq!(app.diff_selection.range, Some((1, 1)));

        handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, 4),
            &mut app,
        );
        assert_eq!(app.diff_selection.range, Some((1, 3)));

        handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 4), &mut app);
        assert_eq!(app.diff_selection.range, Some((1, 3)), "highlight survives");
        let (msg, _) = app.flash.clone().expect("flash set");
        assert_eq!(msg, "copied 3 lines");
    }

    #[test]
    fn mouse_plain_click_over_diff_clears_selection() {
        let mut app = selection_app();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 2),
            &mut app,
        );
        handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 2), &mut app);
        assert!(app.diff_selection.range.is_none());
        assert!(app.flash.is_none());
    }

    #[test]
    fn mouse_drag_while_preview_active_is_ignored() {
        let mut app = selection_app();
        app.preview.enabled = true;
        app.preview.current = Some(PreviewImage {
            path: "x.png".into(),
            png: Vec::new(),
            width: 1,
            height: 1,
        });
        app.diff_selection.range = Some((1, 1));
        app.diff_selection.anchor = Some(1);

        handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, 4),
            &mut app,
        );
        assert_eq!(app.diff_selection.range, Some((1, 1)), "preview consumes");
        assert!(!app.diff_selection.dragged);

        // A release over the diff pane still ends the (non-dragged) selection
        // even though the preview gate would otherwise swallow it.
        handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 4), &mut app);
        assert!(app.diff_selection.anchor.is_none());
        assert!(app.diff_selection.range.is_none());
        assert!(
            app.flash.is_none(),
            "a non-dragged selection copies nothing"
        );
    }

    #[test]
    fn mouse_up_over_diff_with_preview_finishes_drag() {
        let mut app = selection_app();
        // Start and extend a real drag while the preview is inactive.
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 2),
            &mut app,
        );
        handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, 4),
            &mut app,
        );
        assert_eq!(app.diff_selection.range, Some((1, 3)));
        assert!(app.diff_selection.dragged);
        assert!(app.diff_selection.anchor.is_some());

        // A preview arrives mid-drag: the release over the diff pane must still
        // finish the drag instead of leaving the anchor live for a later Up to
        // copy a stale range.
        app.preview.enabled = true;
        app.preview.current = Some(PreviewImage {
            path: "x.png".into(),
            png: Vec::new(),
            width: 1,
            height: 1,
        });
        handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 4), &mut app);
        assert!(app.diff_selection.anchor.is_none(), "no stale anchor");
        assert!(!app.diff_selection.dragged);
        assert_eq!(app.diff_selection.range, Some((1, 3)), "highlight survives");
        let (msg, _) = app.flash.clone().expect("flash set");
        assert_eq!(msg, "copied 3 lines");
    }

    #[test]
    fn mouse_up_outside_diff_still_finishes_drag() {
        let mut app = selection_app();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 2),
            &mut app,
        );
        handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, 4),
            &mut app,
        );
        // Release over the tree pane (column >= diff_width + 1).
        handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), 60, 4),
            &mut app,
        );
        let (msg, _) = app.flash.clone().expect("flash set");
        assert_eq!(msg, "copied 3 lines");
    }

    #[test]
    fn mouse_right_click_tree_copies_path() {
        let (mut app, _rx) = routed_app();
        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        // row 2 → idx 1 → b.rs.
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Right), 45, 2),
            &mut app,
        );
        let (msg, _) = app.flash.clone().expect("flash set");
        assert!(msg.contains("b.rs"), "flash names the copied path: {msg}");
    }

    #[test]
    fn mouse_footer_left_click_mode_cycles() {
        let (mut app, rx) = routed_app();
        app.mouse.footer = Some(Rect::new(0, 19, 80, 1));
        app.mouse.footer_zones = vec![(crate::app::FooterZone::Mode, Rect::new(60, 19, 8, 1))];
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 62, 19),
            &mut app,
        );
        assert_eq!(app.mode, crate::app::Mode::Uncommitted);
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn mouse_footer_right_click_source_opens_commit_picker() {
        // Real opener: reads the global repo root (see t_opens_commit_picker).
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (mut app, _rx) = routed_app();
        app.mouse.footer = Some(Rect::new(0, 19, 80, 1));
        app.mouse.footer_zones = vec![(crate::app::FooterZone::Source, Rect::new(0, 19, 20, 1))];
        assert!(app.base_picker.is_none());
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Right), 5, 19),
            &mut app,
        );
        assert_eq!(
            app.base_picker.as_ref().map(|p| p.title.as_str()),
            Some(" compare against a commit or tag ")
        );
    }

    #[test]
    fn mouse_click_inside_picker_picks_and_closes() {
        let (mut app, rx) = picker_app();
        app.mouse.picker = Some(crate::app::PickerGeometry {
            popup: Rect::new(0, 0, 20, 6),
            inner: Rect::new(1, 1, 18, 4),
            first_row: 0,
        });
        // inner.y = 1: row 2 is the filter row, row 3 is entry 1 (`feature`).
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 3),
            &mut app,
        );
        assert!(app.base_picker.is_none());
        match rx.try_recv().unwrap() {
            crate::snapshot::Request::SetRev { rev } => assert_eq!(rev, "feature...HEAD"),
            other => panic!("expected SetRev, got {other:?}"),
        }
    }

    #[test]
    fn mouse_click_outside_picker_closes_without_request() {
        let (mut app, rx) = picker_app();
        app.mouse.picker = Some(crate::app::PickerGeometry {
            popup: Rect::new(5, 5, 10, 4),
            inner: Rect::new(6, 6, 8, 2),
            first_row: 0,
        });
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 50, 15),
            &mut app,
        );
        assert!(app.base_picker.is_none());
        assert!(rx.try_recv().is_err(), "outside click sends no request");
    }

    #[test]
    fn mouse_wheel_inside_picker_moves_selection() {
        let (mut app, _rx) = picker_app();
        app.mouse.picker = Some(crate::app::PickerGeometry {
            popup: Rect::new(0, 0, 20, 6),
            inner: Rect::new(1, 1, 18, 4),
            first_row: 0,
        });
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);
        handle_mouse(mouse(MouseEventKind::ScrollDown, 3, 2), &mut app);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 1);
        handle_mouse(mouse(MouseEventKind::ScrollUp, 3, 2), &mut app);
        assert_eq!(app.base_picker.as_ref().unwrap().selection, 0);
    }

    #[test]
    fn mouse_click_tree_while_picker_open_is_consumed() {
        let (mut app, _rx) = picker_app();
        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        app.mouse.picker = Some(crate::app::PickerGeometry {
            popup: Rect::new(0, 0, 20, 6),
            inner: Rect::new(1, 1, 18, 4),
            first_row: 0,
        });
        let sel = app.selection;
        // A click over the tree is outside the popup: it closes the picker and
        // must not select a tree entry.
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 2),
            &mut app,
        );
        assert!(app.base_picker.is_none());
        assert_eq!(app.selection, sel);
    }

    #[test]
    fn mouse_footer_click_while_picker_open_is_consumed() {
        let (mut app, _rx) = picker_app();
        app.mouse.footer = Some(Rect::new(0, 19, 80, 1));
        app.mouse.footer_zones = vec![(crate::app::FooterZone::Mode, Rect::new(60, 19, 8, 1))];
        // A popup covering the footer: the click lands on its border, so the
        // picker consumes it without acting.
        app.mouse.picker = Some(crate::app::PickerGeometry {
            popup: Rect::new(0, 0, 80, 20),
            inner: Rect::new(1, 1, 78, 18),
            first_row: 0,
        });
        let mode = app.mode.clone();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 62, 19),
            &mut app,
        );
        assert_eq!(app.mode, mode, "picker consumes the footer click");
        assert!(app.base_picker.is_some(), "picker stays open");
    }

    #[test]
    fn mouse_click_routes_focus() {
        let (mut app, _rx) = routed_app();
        app.focus = Focus::Tree;
        app.diff_width = 40;

        // A diff-pane body click (no hunk header there) focuses the diff.
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 5),
            &mut app,
        );
        assert_eq!(app.focus, Focus::Diff);

        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 2),
            &mut app,
        );
        assert_eq!(app.focus, Focus::Tree);

        // Clicking the tree header strip (row 0) or empty space below the
        // entries focuses the tree without selecting anything.
        app.focus = Focus::Diff;
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 0),
            &mut app,
        );
        assert_eq!(app.focus, Focus::Tree, "tree header click focuses the tree");
        app.focus = Focus::Diff;
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 30),
            &mut app,
        );
        assert_eq!(
            app.focus,
            Focus::Tree,
            "empty tree space click focuses the tree"
        );
    }

    #[test]
    fn mouse_shift_wheel_over_diff_scrolls_horizontally() {
        let mut app = sample_app();
        app.focus = Focus::Diff;
        app.selected_path = Some("x.rs".into());
        app.snapshot = snapshot_with_diff(single_diff(
            header_full(None, None),
            vec![
                hunk_header(0),
                DiffLine {
                    kind: LineKind::Context,
                    raw_text: "y".repeat(300),
                    section: 0,
                    word_runs: None,
                },
                ctx_line(1, 0),
            ],
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| view::ui(f, &mut app)).unwrap();
        app.diff_col = 0;

        let shift_down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(shift_down, &mut app);
        assert!(app.diff_col > 0, "Shift+wheel-down scrolls right");
        let shifted = app.diff_col;

        let shift_up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(shift_up, &mut app);
        assert!(app.diff_col < shifted, "Shift+wheel-up scrolls back left");
    }

    #[test]
    fn mouse_preview_suppresses_diff_but_not_footer_or_tree() {
        // Real opener: reads the global repo root (see t_opens_commit_picker).
        let _guard = crate::test_support::GIT_ROOT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (mut app, _rx) = routed_app();
        app.diff_width = 40;
        app.preview.set_enabled(true);
        app.preview.current = Some(PreviewImage {
            path: "a.rs".into(),
            png: Vec::new(),
            width: 1,
            height: 1,
        });
        assert!(app.preview.active());

        // A diff-pane click is ignored while the preview covers it.
        app.focus = Focus::Tree;
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 5),
            &mut app,
        );
        assert_eq!(app.focus, Focus::Tree);

        // A footer click still works (Source left opens the branch picker).
        app.mouse.footer = Some(Rect::new(0, 19, 80, 1));
        app.mouse.footer_zones = vec![(crate::app::FooterZone::Source, Rect::new(0, 19, 20, 1))];
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 5, 19),
            &mut app,
        );
        assert!(app.base_picker.is_some());

        // A tree click still selects.
        app.base_picker = None;
        app.mouse.tree_list = Some(Rect::new(40, 1, 39, 10));
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 2),
            &mut app,
        );
        assert_eq!(app.selection, 1);
    }
}
