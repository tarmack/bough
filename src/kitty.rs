//! Kitty graphics protocol primitives: a capability probe plus the raw
//! transmit/delete escape sequences.
//!
//! Pure output except for the one-shot stdin poll in [`probe`]; no terminal
//! state is owned here. The caller must have raw mode active for the probe's
//! reply to arrive unmodified.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ratatui::layout::Rect;

/// Max base64 payload characters per escape sequence. Keeps each chunk well
/// under the terminal's ~4096-byte limit after the control prefix.
const CHUNK_B64: usize = 3072;

/// Probe whether the terminal supports the kitty graphics protocol.
///
/// Honours `BOUGH_NO_IMAGES` (any value but `"0"` disables), short-circuits
/// when env vars identify kitty, and otherwise round-trips a 1-pixel query
/// escape through stdin with a ~250ms budget. The trailing DA1 reply is
/// consumed so it cannot leak into crossterm's event queue.
pub fn probe() -> bool {
    if let Ok(v) = std::env::var("BOUGH_NO_IMAGES") {
        if v != "0" {
            return false;
        }
    }
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return true;
    }
    if std::env::var("TERM")
        .map(|t| t.contains("kitty"))
        .unwrap_or(false)
    {
        return true;
    }
    probe_escape()
}

/// Escape-sequence probe: send a graphics query + DA1, then read until quiet or
/// the budget expires.
fn probe_escape() -> bool {
    let mut out = io::stdout();
    if out
        .write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c")
        .is_err()
    {
        return false;
    }
    if out.flush().is_err() {
        return false;
    }

    const BUDGET: Duration = Duration::from_millis(250);
    const QUIET: Duration = Duration::from_millis(60);
    let start = Instant::now();
    let mut last_read = start;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let elapsed = start.elapsed();
        if elapsed >= BUDGET {
            break;
        }
        let since_read = last_read.elapsed();
        if !buf.is_empty() && since_read >= QUIET {
            break;
        }
        let wait = BUDGET
            .saturating_sub(elapsed)
            .min(QUIET.saturating_sub(since_read));
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, wait.as_millis() as libc::c_int) };
        if ready <= 0 {
            if last_read.elapsed() >= QUIET {
                break;
            }
            continue;
        }
        let mut chunk = [0u8; 256];
        let n = unsafe { libc::read(0, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        last_read = Instant::now();
    }

    has_ok_response(&buf)
}

/// True when the accumulated reply carries a kitty graphics response (`_Gi=31;`)
/// that is not an immediate error (`;E…`).
fn has_ok_response(buf: &[u8]) -> bool {
    let needle = b"_Gi=31;";
    let Some(pos) = find_subslice(buf, needle) else {
        return false;
    };
    !buf[pos + needle.len()..].starts_with(b"E")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Transmit and display `png` as image `id` at `area`, preserving aspect ratio
/// within `area` using the terminal's `cell` pixel size.
pub fn place(
    w: &mut impl Write,
    id: u32,
    png: &[u8],
    src_w: u32,
    src_h: u32,
    area: Rect,
    cell: (u32, u32),
) -> io::Result<()> {
    write!(w, "\x1b[{};{}H", area.y + 1, area.x + 1)?;

    let (cw, ch) = (cell.0.max(1), cell.1.max(1));
    let cols = area.width as u32;
    let rows = area.height as u32;
    let s = src_w.max(1) as f64;
    let v = src_h.max(1) as f64;
    let avail_w = cols.saturating_mul(cw) as f64;
    let avail_h = rows.saturating_mul(ch) as f64;
    let scale = (avail_w / s).min(avail_h / v);
    let c = (((s * scale / cw as f64).floor() as u32).max(1)).min(cols.max(1));
    let r = (((v * scale / ch as f64).floor() as u32).max(1)).min(rows.max(1));

    let b64 = STANDARD.encode(png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(CHUNK_B64).collect();
    if chunks.is_empty() {
        let control = format!(
            "a=T,f=100,t=d,C=1,i={id},s={src_w},v={src_h},c={c},r={r},m=0"
        );
        write!(w, "\x1b_G{};\x1b\\", control)?;
        return Ok(());
    }

    let n = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        let control = if i == 0 {
            let m = if n > 1 { 1 } else { 0 };
            format!("a=T,f=100,t=d,C=1,i={id},s={src_w},v={src_h},c={c},r={r},m={m}")
        } else {
            let m = if i + 1 < n { 1 } else { 0 };
            format!("m={m}")
        };
        w.write_all(b"\x1b_G")?;
        w.write_all(control.as_bytes())?;
        w.write_all(b";")?;
        w.write_all(chunk)?;
        w.write_all(b"\x1b\\")?;
    }
    Ok(())
}

/// Delete a single placed image by id.
pub fn delete(w: &mut impl Write, id: u32) -> io::Result<()> {
    write!(w, "\x1b_Ga=d,d=I,i={id}\x1b\\")
}

/// Terminal cell size in pixels, falling back to `(10, 20)` when the ioctl
/// yields no pixel geometry (e.g. a serial console or non-tty).
pub fn cell_size() -> (u32, u32) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 {
        return (10, 20);
    }
    px_to_cells(&ws)
}

/// Pixel geometry of a `winsize`, normalized to per-cell pixels.
fn px_to_cells(ws: &libc::winsize) -> (u32, u32) {
    if ws.ws_xpixel > 0 && ws.ws_ypixel > 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (
            (ws.ws_xpixel / ws.ws_col) as u32,
            (ws.ws_ypixel / ws.ws_row) as u32,
        )
    } else {
        (10, 20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_emits_exact_sequence() {
        let mut out = Vec::new();
        delete(&mut out, 7).unwrap();
        assert_eq!(out, b"\x1b_Ga=d,d=I,i=7\x1b\\");
    }

    #[test]
    fn place_single_chunk_header_and_payload() {
        let mut out = Vec::new();
        place(&mut out, 5, b"hello", 4, 3, Rect::new(0, 0, 10, 10), (10, 20)).unwrap();
        let s = String::from_utf8(out).unwrap();
        // scale = min(100/4, 200/3) = 25 -> c = 10, r = 3.
        assert_eq!(
            s,
            "\x1b[1;1H\x1b_Ga=T,f=100,t=d,C=1,i=5,s=4,v=3,c=10,r=3,m=0;aGVsbG8=\x1b\\"
        );
    }

    #[test]
    fn place_multi_chunk_marks_middle_and_last() {
        let png = vec![0u8; 5000];
        let mut out = Vec::new();
        place(&mut out, 9, &png, 100, 100, Rect::new(0, 0, 10, 10), (10, 20)).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b[1;1H\x1b_Ga=T,f=100,t=d,C=1,i=9"));
        // More than one chunk: first ends with m=1, last with m=0.
        assert!(s.contains(",m=1;"));
        assert!(s.ends_with("\x1b\\"));
        let parts: Vec<&str> = s.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert!(parts.len() > 1);
        assert!(parts[1].starts_with("\x1b_Gm=1;"));
        assert!(parts.last().unwrap().contains("m=0;"));
    }

    #[test]
    fn cell_pixels_normalize_and_fall_back() {
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 800,
            ws_ypixel: 480,
        };
        assert_eq!(px_to_cells(&ws), (10, 20));

        let zero = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(px_to_cells(&zero), (10, 20));
    }

    #[test]
    fn ok_response_detection() {
        assert!(has_ok_response(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(!has_ok_response(b"\x1b_Gi=31;ENOENT\x1b\\"));
        assert!(!has_ok_response(b"\x1b[?1;2c"));
    }
}