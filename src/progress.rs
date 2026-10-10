//! Transfer progress on stderr, in the spirit of `pv`: bytes transferred,
//! elapsed time and speed. Nothing is ever written to stdout, which carries
//! the restore stream.

use anyhow::{bail, Result};
use std::io::{IsTerminal, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::task::JoinHandle;

static TRANSFERRED: AtomicU64 = AtomicU64::new(0);
/// Estimated total bytes; 0 while unknown.
static TOTAL: AtomicU64 = AtomicU64::new(0);

/// Sets the estimated total, used only for percentages.
pub fn set_total(bytes: u64) {
    TOTAL.store(bytes, Ordering::Relaxed);
}

fn total() -> Option<u64> {
    Some(TOTAL.load(Ordering::Relaxed)).filter(|total| *total > 0)
}

/// Interval used when progress is automatic and stderr is not a terminal.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
const TTY_REFRESH: Duration = Duration::from_millis(500);
const BAR_WIDTH: usize = 24;

/// Records transferred bytes.
pub fn add(bytes: u64) {
    TRANSFERRED.fetch_add(bytes, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Live single line on the terminal.
    Tty,
    /// One line every interval.
    Interval(Duration),
}

impl Mode {
    /// Parses `--progress`: `auto`, `tty`, `off` or a number of seconds.
    pub fn parse(value: &str, stderr_is_tty: bool) -> Result<Self> {
        match value {
            "auto" => Ok(if stderr_is_tty {
                Self::Tty
            } else {
                Self::Interval(DEFAULT_INTERVAL)
            }),
            "tty" => Ok(Self::Tty),
            "off" => Ok(Self::Off),
            seconds => match seconds.parse::<u64>() {
                Ok(seconds) if seconds > 0 => Ok(Self::Interval(Duration::from_secs(seconds))),
                _ => bail!("expected auto, tty, off or a positive number of seconds"),
            },
        }
    }

    pub fn from_cli(value: Option<&str>) -> Result<Self> {
        Self::parse(value.unwrap_or("auto"), std::io::stderr().is_terminal())
    }
}

/// Periodically reports the global byte counter until finished.
pub struct Reporter {
    task: Option<JoinHandle<()>>,
    mode: Mode,
    started: Instant,
    label: &'static str,
}

impl Reporter {
    pub fn start(mode: Mode, label: &'static str) -> Self {
        TRANSFERRED.store(0, Ordering::Relaxed);
        TOTAL.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let task = match mode {
            Mode::Off => None,
            Mode::Tty => Some(tokio::spawn(report(mode, label, started, TTY_REFRESH))),
            Mode::Interval(every) => Some(tokio::spawn(report(mode, label, started, every))),
        };
        Self {
            task,
            mode,
            started,
            label,
        }
    }

    /// Stops reporting and prints a final summary line.
    pub async fn finish(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        if self.mode == Mode::Off {
            return;
        }
        let bytes = TRANSFERRED.load(Ordering::Relaxed);
        let elapsed = self.started.elapsed();
        let line = format!(
            "{}: {} in {} (avg {}/s)",
            self.label,
            human_bytes(bytes),
            clock(elapsed),
            human_bytes(per_second(bytes, elapsed)),
        );
        let mut err = std::io::stderr().lock();
        if self.mode == Mode::Tty {
            let _ = write!(err, "\r\x1b[K");
        }
        let _ = writeln!(err, "{line}");
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn report(mode: Mode, label: &'static str, started: Instant, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    ticker.tick().await;
    let mut last = (started, 0u64);
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let bytes = TRANSFERRED.load(Ordering::Relaxed);
        let current = per_second(bytes - last.1.min(bytes), now - last.0);
        let line = format_line(
            label,
            bytes,
            now - started,
            current,
            total(),
            mode == Mode::Tty,
        );
        last = (now, bytes);
        let mut err = std::io::stderr().lock();
        let _ = if mode == Mode::Tty {
            write!(err, "\r\x1b[K{line}")
        } else {
            writeln!(err, "{line}")
        };
        let _ = err.flush();
    }
}

/// One progress line: size, elapsed time, current and average speed, and,
/// when the total is estimated, a percentage (with a bar on a terminal).
pub fn format_line(
    label: &str,
    bytes: u64,
    elapsed: Duration,
    current_rate: u64,
    total: Option<u64>,
    bar: bool,
) -> String {
    let mut line = format!(
        "{label}: {} {} [{}/s] (avg {}/s)",
        human_bytes(bytes),
        clock(elapsed),
        human_bytes(current_rate),
        human_bytes(per_second(bytes, elapsed)),
    );
    if let Some(total) = total.filter(|total| *total > 0) {
        let percent = (bytes.saturating_mul(100) / total).min(99);
        if bar {
            let filled = (percent as usize * BAR_WIDTH) / 100;
            line.push_str(&format!(
                " [{}{}]",
                "=".repeat(filled),
                " ".repeat(BAR_WIDTH - filled)
            ));
        }
        line.push_str(&format!(" ~{percent}%"));
    }
    line
}

fn per_second(bytes: u64, elapsed: Duration) -> u64 {
    let millis = elapsed.as_millis().max(1);
    (u128::from(bytes) * 1000 / millis).min(u128::from(u64::MAX)) as u64
}

pub fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.2}{}", UNITS[unit])
    }
}

/// Counts bytes read through it.
pub struct Counting<R>(pub R);

impl<R: AsyncRead + Unpin> AsyncRead for Counting<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.0).poll_read(cx, buf);
        if poll.is_ready() {
            add((buf.filled().len() - before) as u64);
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes() {
        assert_eq!(Mode::parse("auto", true).unwrap(), Mode::Tty);
        assert_eq!(
            Mode::parse("auto", false).unwrap(),
            Mode::Interval(DEFAULT_INTERVAL)
        );
        assert_eq!(Mode::parse("tty", false).unwrap(), Mode::Tty);
        assert_eq!(Mode::parse("off", true).unwrap(), Mode::Off);
        assert_eq!(
            Mode::parse("7", true).unwrap(),
            Mode::Interval(Duration::from_secs(7))
        );
        assert!(Mode::parse("0", true).is_err());
        assert!(Mode::parse("soon", true).is_err());
    }

    #[test]
    fn formats_size_time_and_speed() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(3 * 1024 * 1024 / 2), "1.50MiB");
        assert_eq!(clock(Duration::from_secs(3725)), "1:02:05");
        let line = format_line(
            "upload",
            10 * 1024 * 1024,
            Duration::from_secs(2),
            5 * 1024 * 1024,
            Some(20 * 1024 * 1024),
            true,
        );
        assert!(line.contains("10.00MiB 0:00:02 [5.00MiB/s] (avg 5.00MiB/s)"));
        assert!(line.contains("~50%"));
        assert!(line.contains('='));
    }

    #[tokio::test]
    async fn counting_reader_counts_bytes() {
        use tokio::io::AsyncReadExt;
        let before = TRANSFERRED.load(Ordering::Relaxed);
        let mut reader = Counting(&b"abcdef"[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        assert!(TRANSFERRED.load(Ordering::Relaxed) >= before + 6);
    }
}
