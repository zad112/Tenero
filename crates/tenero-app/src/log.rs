//! Logging: a line per event to standard error and, if asked, to a file, each with a UTC time and a level.
//!
//! The file is renamed to `<name>.old` (replacing the previous `.old`) when it grows past a size, so a node left
//! running for months cannot fill a disk with its own log. **Nothing secret is ever passed to a logger**: the
//! callers log heights, ids, counts and addresses of peers, never a key, a seed or a passphrase.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ui::{error_event, Event as UiEvent, Screen};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
}

/// The default size at which the log file is rotated: 20 MiB.
pub const ROTATE_AT: u64 = 20 * 1024 * 1024;

struct FileSink {
    path: PathBuf,
    /// `None` only while the file is being rotated (a file must be closed before it is renamed on Windows).
    file: Option<File>,
    written: u64,
    rotate_at: u64,
}

pub struct Logger {
    level: Level,
    file: Option<Mutex<FileSink>>,
    /// Print to standard error too (tests do not).
    stderr: bool,
    /// The screen of a program with one (`ui.rs`): it shows the summary, this logger keeps the detail.
    screen: Option<Arc<Screen>>,
}

/// `2026-10-02T09:05:03Z` for a time in seconds since 1970-01-01 (UTC), by the civil-calendar algorithm of Howard
/// Hinnant (days to year, month, day), with no time zone and no leap seconds.
pub fn utc_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

impl Logger {
    pub fn new(level: Level, file: Option<&Path>, stderr: bool) -> std::io::Result<Logger> {
        Logger::with_rotation(level, file, stderr, ROTATE_AT)
    }

    pub fn with_rotation(
        level: Level,
        file: Option<&Path>,
        stderr: bool,
        rotate_at: u64,
    ) -> std::io::Result<Logger> {
        let sink = match file {
            Some(path) => {
                let f = OpenOptions::new().create(true).append(true).open(path)?;
                let written = f.metadata()?.len();
                Some(Mutex::new(FileSink {
                    path: path.to_path_buf(),
                    file: Some(f),
                    written,
                    rotate_at,
                }))
            }
            None => None,
        };
        Ok(Logger {
            level,
            file: sink,
            stderr,
            screen: None,
        })
    }

    pub fn enabled(&self, level: Level) -> bool {
        level <= self.level
    }

    pub fn log(&self, level: Level, msg: &str) {
        self.emit(level, msg, None);
    }

    /// A line for the log file AND an event for the screen, which shows it in its own words. (The screen shows its events whatever the
    /// log level is; the file keeps the line as it always was.)
    pub fn log_event(&self, level: Level, msg: &str, event: UiEvent) {
        self.emit(level, msg, Some(event));
    }

    fn emit(&self, level: Level, msg: &str, event: Option<UiEvent>) {
        let file_on = self.enabled(level);
        if !file_on && event.is_none() {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // one line per message: a newline in the text would let a peer-supplied string forge a log line
        let clean: String = msg
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let line = format!("{} {} {clean}\n", utc_timestamp(now), level.name());
        match (&self.screen, event) {
            // the screen shows what the caller made of it; warnings and errors without an event are shown as such (an error with what to do
            // about it); the rest is detail, for verbose mode
            (Some(sc), Some(e)) => sc.event(&e),
            (Some(sc), None) if file_on => match level {
                Level::Error => sc.event(&error_event(&clean)),
                Level::Warn => sc.event(&UiEvent::Warn(clean.clone())),
                _ => sc.detail(line.trim_end()),
            },
            (None, _) if self.stderr && file_on => eprint!("{line}"),
            _ => {}
        }
        if !file_on {
            return;
        }
        if let Some(sink) = &self.file {
            if let Ok(mut s) = sink.lock() {
                if s.written + line.len() as u64 > s.rotate_at && s.written > 0 {
                    let mut old = s.path.as_os_str().to_owned();
                    old.push(".old");
                    let _ = std::fs::remove_file(&old);
                    s.file = None;
                    let _ = std::fs::rename(&s.path, &old);
                    s.file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&s.path)
                        .ok();
                    s.written = 0;
                }
                if let Some(f) = s.file.as_mut() {
                    if f.write_all(line.as_bytes()).is_ok() {
                        s.written += line.len() as u64;
                    }
                }
            }
        }
    }

    /// The screen this logger also writes to, if it has one.
    pub fn screen(&self) -> Option<&Arc<Screen>> {
        self.screen.as_ref()
    }

    /// Also show warnings, errors and the events given to `log_event` on `screen` (and, in verbose mode, every line).
    pub fn with_screen(mut self, screen: Arc<Screen>) -> Logger {
        self.screen = Some(screen);
        self
    }

    pub fn error(&self, msg: &str) {
        self.log(Level::Error, msg);
    }
    pub fn warn(&self, msg: &str) {
        self.log(Level::Warn, msg);
    }
    pub fn info(&self, msg: &str) {
        self.log(Level::Info, msg);
    }
    pub fn debug(&self, msg: &str) {
        self.log(Level::Debug, msg);
    }
}
