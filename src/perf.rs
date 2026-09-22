//! Optional performance instrumentation for diagnosing CPU/latency issues.
//!
//! Disabled by default: set `BOUGH_PERF_LOG` to a file path and bough truncates
//! (creating) that file at startup and appends one line per event. When the
//! variable is unset every entry point is a single relaxed atomic load plus a
//! branch — no file, no allocation, no logging.
//!
//! Usage:
//!
//! ```text
//! BOUGH_PERF_LOG=/tmp/bough-perf.log bough <repo>
//! ```
//!
//! Each line is machine-parseable:
//!
//! ```text
//! <elapsed_ms> <thread> <kind> <payload>
//! ```
//!
//! Kinds:
//! - `evt` — a discrete event (request, snapshot, state flip). Payload is free
//!   text.
//! - `span` — a timed region, emitted on drop. Payload is
//!   `<name>=<ms>ms[ <detail>]`.
//! - `tally` — a per-second dump of the counter set. Payload is a
//!   space-separated `Name=count` list (only non-zero counters; an all-zero
//!   beat emits nothing).
//!
//! The header line (kind `hdr`) carries the start wall time, pid, and argv.

use std::fmt;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Log a discrete event, like [`evt`] but with `format!`-style arguments.
#[macro_export]
macro_rules! perf_evt {
    ($($arg:tt)*) => {
        $crate::perf::evt(format_args!($($arg)*))
    };
}

/// Fixed counter set. Kept as a closed enum (not a map) so `tally` is a plain
/// array index and an atomic increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    Draws,
    PollWakes,
    TerminalProbes,
    PreviewSyncs,
    SnapshotsApplied,
    SnapshotsSent,
    RequestsSeen,
    RequestsCoalescedOut,
    Recomputes,
    Scans,
    DiffBuilds,
    CacheHits,
    CacheMisses,
    DirChunks,
    HighlightFills,
    IgnoreSkips,
    WatchRawEvents,
    WatchReadFiltered,
    WatchWorkdir,
    WatchRefs,
    ReadRetries,
    KeyEvents,
}

impl Counter {
    /// Every counter, in dump order.
    pub const ALL: [Counter; 22] = [
        Counter::Draws,
        Counter::PollWakes,
        Counter::TerminalProbes,
        Counter::PreviewSyncs,
        Counter::SnapshotsApplied,
        Counter::SnapshotsSent,
        Counter::RequestsSeen,
        Counter::RequestsCoalescedOut,
        Counter::Recomputes,
        Counter::Scans,
        Counter::DiffBuilds,
        Counter::CacheHits,
        Counter::CacheMisses,
        Counter::DirChunks,
        Counter::HighlightFills,
        Counter::IgnoreSkips,
        Counter::WatchRawEvents,
        Counter::WatchReadFiltered,
        Counter::WatchWorkdir,
        Counter::WatchRefs,
        Counter::ReadRetries,
        Counter::KeyEvents,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            Counter::Draws => "Draws",
            Counter::PollWakes => "PollWakes",
            Counter::TerminalProbes => "TerminalProbes",
            Counter::PreviewSyncs => "PreviewSyncs",
            Counter::SnapshotsApplied => "SnapshotsApplied",
            Counter::SnapshotsSent => "SnapshotsSent",
            Counter::RequestsSeen => "RequestsSeen",
            Counter::RequestsCoalescedOut => "RequestsCoalescedOut",
            Counter::Recomputes => "Recomputes",
            Counter::Scans => "Scans",
            Counter::DiffBuilds => "DiffBuilds",
            Counter::CacheHits => "CacheHits",
            Counter::CacheMisses => "CacheMisses",
            Counter::DirChunks => "DirChunks",
            Counter::HighlightFills => "HighlightFills",
            Counter::IgnoreSkips => "IgnoreSkips",
            Counter::WatchRawEvents => "WatchRawEvents",
            Counter::WatchReadFiltered => "WatchReadFiltered",
            Counter::WatchWorkdir => "WatchWorkdir",
            Counter::WatchRefs => "WatchRefs",
            Counter::ReadRetries => "ReadRetries",
            Counter::KeyEvents => "KeyEvents",
        }
    }
}

/// Fast path flag: set once when a logger is installed.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The installed logger, or unset when instrumentation is off.
static LOGGER: OnceLock<Logger> = OnceLock::new();

/// True when `BOUGH_PERF_LOG` was set and a log is being written.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Open the log named by `BOUGH_PERF_LOG`, if set. Truncates any existing file.
/// Safe to call once at startup; a later call is a no-op.
pub fn init() {
    let Ok(path) = std::env::var("BOUGH_PERF_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("bough: perf log {path}: {e}");
            return;
        }
    };
    let logger = Logger::new(Box::new(file), Instant::now());
    logger.header();
    let _ = LOGGER.set(logger);
    ENABLED.store(true, Ordering::Relaxed);
}

/// Log a discrete event. No-op when disabled.
pub fn evt(msg: fmt::Arguments) {
    if !enabled() {
        return;
    }
    if let Some(logger) = LOGGER.get() {
        logger.evt(&msg.to_string());
    }
}

/// Increment one counter. No-op when disabled.
pub fn tally(counter: Counter) {
    if !enabled() {
        return;
    }
    if let Some(logger) = LOGGER.get() {
        logger.tally(counter);
    }
}

/// Flush all non-zero counters as one `tally` line and reset them. Call about
/// once per second per instrumented thread. No-op when disabled.
pub fn beat() {
    if !enabled() {
        return;
    }
    if let Some(logger) = LOGGER.get() {
        logger.beat();
    }
}

/// Start timing a named region; the line is emitted when the returned guard is
/// dropped. No-op when disabled (no clock read, no allocation).
pub fn span(name: &'static str) -> Span<'static> {
    if !enabled() {
        return Span::disabled(name);
    }
    match LOGGER.get() {
        Some(logger) => logger.span(name),
        None => Span::disabled(name),
    }
}

/// A timed region guard. See [`span`].
pub struct Span<'a> {
    logger: Option<&'a Logger>,
    name: &'static str,
    start: Option<Instant>,
    detail: Option<String>,
}

impl<'a> Span<'a> {
    fn disabled(name: &'static str) -> Self {
        Span {
            logger: None,
            name,
            start: None,
            detail: None,
        }
    }

    /// Attach detail to the emitted line. Takes lazy `format_args!` so a
    /// disabled span allocates nothing. No-op when disabled.
    pub fn detail(&mut self, detail: fmt::Arguments) {
        if self.logger.is_some() {
            self.detail = Some(detail.to_string());
        }
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        let (Some(logger), Some(start)) = (self.logger, self.start) else {
            return;
        };
        let ms = start.elapsed().as_millis();
        let payload = match &self.detail {
            Some(detail) => format!("{}={ms}ms {detail}", self.name),
            None => format!("{}={ms}ms", self.name),
        };
        logger.write("span", &payload);
    }
}

/// The log sink plus its counter slots and start instant. One instance lives in
/// [`LOGGER`] in production; tests build their own over a temp file.
struct Logger {
    out: Mutex<Box<dyn Write + Send>>,
    start: Instant,
    counters: [AtomicU64; Counter::ALL.len()],
}

impl Logger {
    fn new(out: Box<dyn Write + Send>, start: Instant) -> Self {
        Logger {
            out: Mutex::new(out),
            start,
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Write the startup header: wall time, pid, argv.
    fn header(&self) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let argv = std::env::args().collect::<Vec<_>>().join(" ");
        self.write(
            "hdr",
            &format!("start={secs} pid={} argv={argv}", std::process::id()),
        );
    }

    fn write(&self, kind: &str, payload: &str) {
        let elapsed = self.start.elapsed().as_millis();
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{elapsed} {name} {kind} {payload}");
        }
    }

    fn evt(&self, msg: &str) {
        self.write("evt", msg);
    }

    fn tally(&self, counter: Counter) {
        self.counters[counter.index()].fetch_add(1, Ordering::Relaxed);
    }

    fn beat(&self) {
        let mut payload = String::new();
        for counter in Counter::ALL {
            let n = self.counters[counter.index()].swap(0, Ordering::Relaxed);
            if n == 0 {
                continue;
            }
            if !payload.is_empty() {
                payload.push(' ');
            }
            let _ = write!(payload, "{}={n}", counter.name());
        }
        if !payload.is_empty() {
            self.write("tally", &payload);
        }
    }

    fn span(&self, name: &'static str) -> Span<'_> {
        Span {
            logger: Some(self),
            name,
            start: Some(Instant::now()),
            detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_logger() -> (Logger, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let logger = Logger::new(Box::new(file.reopen().unwrap()), Instant::now());
        (logger, file)
    }

    #[test]
    fn disabled_by_default() {
        // No `init` runs in tests, so the flag stays off and every entry point
        // is a no-op that reads nothing and writes nothing.
        assert!(!enabled());
        evt(format_args!("ignored"));
        tally(Counter::Draws);
        beat();
        let mut span = span("noop");
        span.detail(format_args!("ignored"));
        assert!(span.start.is_none());
    }

    #[test]
    fn enabled_logger_emits_documented_line_format() {
        let (logger, file) = string_logger();
        logger.evt("hello world");
        let mut span = logger.span("recompute");
        span.detail(format_args!("entries=945"));
        drop(span);
        logger.tally(Counter::Draws);
        logger.tally(Counter::Draws);
        logger.beat();
        drop(logger);

        let text = std::fs::read_to_string(file.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);

        // Every line starts with an integer elapsed and a non-empty thread name.
        for line in &lines {
            let mut parts = line.splitn(4, ' ');
            assert!(parts.next().unwrap().parse::<u128>().is_ok(), "{line}");
            assert!(!parts.next().unwrap().is_empty(), "{line}");
        }
        assert!(lines[0].contains(" evt hello world"), "{}", lines[0]);
        assert!(lines[1].contains(" span recompute="), "{}", lines[1]);
        assert!(lines[1].contains("entries=945"), "{}", lines[1]);
        assert!(lines[1].contains("ms"), "{}", lines[1]);
        assert!(lines[2].contains(" tally Draws=2"), "{}", lines[2]);
    }

    #[test]
    fn beat_with_no_counts_writes_nothing() {
        let (logger, file) = string_logger();
        logger.beat();
        logger.tally(Counter::Draws);
        let _ = logger.counters[Counter::Draws.index()].swap(0, Ordering::Relaxed);
        logger.beat();
        drop(logger);
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "");
    }

    #[test]
    fn header_carries_pid_and_argv() {
        let (logger, file) = string_logger();
        logger.header();
        drop(logger);
        let text = std::fs::read_to_string(file.path()).unwrap();
        let line = text.lines().next().unwrap();
        assert!(line.contains(" hdr "), "{line}");
        assert!(line.contains("pid="), "{line}");
        assert!(line.contains("argv="), "{line}");
    }

    #[test]
    fn span_without_detail_omits_the_detail_field() {
        let (logger, file) = string_logger();
        let span = logger.span("git_scan");
        drop(span);
        drop(logger);
        let text = std::fs::read_to_string(file.path()).unwrap();
        let payload = text.lines().next().unwrap().splitn(4, ' ').nth(3).unwrap();
        assert!(payload.starts_with("git_scan="), "{payload}");
        assert!(payload.ends_with("ms"), "{payload}");
    }
}
