//! Logging sink for the daemon.
//!
//! the daemon runs as an OHOS native process on the device, so `cfg(ohos)` routes
//! every `log::xxx!` record through `OH_LOG_Print` into hilog; when built for a
//! plain host (local two-process bring-up on the VM) it logs to stderr instead.
//!
//! Logging is opt-in: `init(false)` (the default, no `--log` argument) installs
//! nothing and every `log::xxx!` call becomes a no-op; `--log` enables both
//! sinks. Startup failure still prints to stderr unconditionally from `main`.
//!
//! Under `--log` the records are also mirrored to a file under the data root
//! (see `attach_file`). hilog keeps its own buffer that rolls over, and the
//! device console is not always to hand, so a file beside the application's own
//! logs is what makes a session's records readable after the fact.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use log::Level;

/// Subdirectory of the data root the mirrored file goes in: the same `logs`
/// directory the application keeps its own logs in.
const LOG_SUBDIR: &str = "logs";
/// Name of the mirrored file.
const LOG_FILE: &str = "hicodeerd.log";
/// Offset applied to UTC when stamping a line. The device runs on China
/// Standard Time and this build carries no time-zone database.
const TZ_OFFSET: u64 = 8 * 3600;
/// How many records are held while there is still no file to put them in.
///
/// The daemon learns the data root from the management request that follows a
/// client's authentication, so the line saying that client connected is always
/// made before there can be a file. Holding it -- and the rest of the run-up --
/// lets the file open with the story already in it rather than starting
/// mid-sentence.
const BACKLOG_LIMIT: usize = 256;

/// Whether `--log` was given: only then is there a logger to mirror from, so
/// only then is a file ever opened.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The mirrored file, once the data root is known.
static SINK: Mutex<Option<File>> = Mutex::new(None);
/// Records made while `SINK` is still empty, replayed into the file on open.
static BACKLOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Installs the process-wide logger when `enabled` (the `--log` flag); with
/// `enabled == false` no logger is installed, so all log records are dropped.
pub fn init(enabled: bool) {
    if !enabled {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    #[cfg(target_env = "ohos")]
    ohos::init();
    #[cfg(not(target_env = "ohos"))]
    stderr_logger::init();
}

/// Mirrors records into `<root>/logs/<file>` from here on, creating the
/// directory as needed.
///
/// The daemon runs under its own account and learns the root only when a client
/// connects, so this is the first moment it knows a directory of its own to
/// write to; before that there is nowhere to put a file. Called without `--log`
/// it does nothing, there being no logger to mirror from.
pub fn attach_file(root: &Path) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let dir = root.join(LOG_SUBDIR);
    let opened = {
        let mut slot = SINK.lock().unwrap_or_else(|poison| poison.into_inner());
        if slot.is_some() {
            return;
        }
        std::fs::create_dir_all(&dir)
            .and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(LOG_FILE))
            })
            .map(|file| *slot = Some(file))
    };
    if let Err(err) = opened {
        log::warn!("log: cannot open {}: {err}", dir.join(LOG_FILE).display());
        return;
    }
    replay_backlog();
}

/// Writes out whatever was recorded before the file existed. The two locks are
/// taken one after the other, never together, so this cannot deadlock against
/// [`mirror`] taking them in the other order.
fn replay_backlog() {
    let pending: Vec<String> = {
        let mut backlog = BACKLOG.lock().unwrap_or_else(|poison| poison.into_inner());
        std::mem::take(&mut *backlog)
    };
    if pending.is_empty() {
        return;
    }
    let Ok(mut slot) = SINK.lock() else {
        return;
    };
    let Some(file) = slot.as_mut() else {
        return;
    };
    for line in pending {
        let _ = writeln!(file, "{line}");
    }
}

/// Appends one record to the mirrored file, holding it back until there is one.
/// File trouble is swallowed: logging must never be able to fail a command.
fn mirror(level: Level, args: &std::fmt::Arguments<'_>) {
    let line = format!("{} {level} {args}", stamp());
    {
        let Ok(mut slot) = SINK.lock() else {
            return;
        };
        if let Some(file) = slot.as_mut() {
            let _ = writeln!(file, "{line}");
            return;
        }
    }
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut backlog) = BACKLOG.lock() else {
        return;
    };
    if backlog.len() >= BACKLOG_LIMIT {
        backlog.remove(0);
    }
    backlog.push(line);
}

/// Local time as `MM-DD HH:MM:SS`, for a log line.
fn stamp() -> String {
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let seconds = utc + TZ_OFFSET;
    let clock = seconds % 86_400;
    let (month, day) = month_day(seconds / 86_400);
    format!(
        "{month:02}-{day:02} {:02}:{:02}:{:02}",
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

/// Month and day of month for a day count since 1970-01-01, in the proleptic
/// Gregorian calendar.
fn month_day(days: u64) -> (u64, u64) {
    const LENGTHS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut year = 1970;
    let mut left = days;
    loop {
        let length = if is_leap(year) { 366 } else { 365 };
        if left < length {
            break;
        }
        left -= length;
        year += 1;
    }
    for (index, length) in LENGTHS.iter().enumerate() {
        let length = if index == 1 && is_leap(year) {
            29
        } else {
            *length
        };
        if left < length {
            return (index as u64 + 1, left + 1);
        }
        left -= length;
    }
    (12, 31)
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar.
fn is_leap(year: u64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[cfg(target_env = "ohos")]
mod ohos {
    use std::ffi::{CStr, CString, c_char};

    use log::{Level, LevelFilter, Log, Metadata, Record};

    /// LOG_APP: third-party applications always use this log type.
    const LOG_APP: i32 = 0;
    /// hilog LogLevel enum: DEBUG=3 / INFO=4 / WARN=5 / ERROR=6.
    const LOG_LEVEL_DEBUG: i32 = 3;
    const LOG_LEVEL_INFO: i32 = 4;
    const LOG_LEVEL_WARN: i32 = 5;
    const LOG_LEVEL_ERROR: i32 = 6;
    /// App service domain, 0x0001 and up are user-defined.
    const HILOG_DOMAIN: u32 = 0x0001;
    /// hilog tag, kept short enough (<31 bytes) to avoid truncation.
    const HILOG_TAG: &CStr = c"Hicodeerd";
    /// Single `%{public}s` format: the whole message is one public plain-text arg.
    const HILOG_FORMAT: &CStr = c"%{public}s";

    #[link(name = "hilog_ndk.z")]
    unsafe extern "C" {
        fn OH_LOG_Print(
            log_type: i32,
            log_level: i32,
            domain: u32,
            tag: *const c_char,
            fmt: *const c_char,
            ...
        ) -> i32;
    }

    /// Maps a `log` level to its hilog LogLevel value.
    fn hilog_level(level: Level) -> i32 {
        match level {
            Level::Error => LOG_LEVEL_ERROR,
            Level::Warn => LOG_LEVEL_WARN,
            Level::Info => LOG_LEVEL_INFO,
            Level::Debug | Level::Trace => LOG_LEVEL_DEBUG,
        }
    }

    struct HilogLogger;

    impl Log for HilogLogger {
        fn enabled(&self, _metadata: &Metadata) -> bool {
            true
        }

        fn log(&self, record: &Record) {
            let message = format!("[hicodeerd:{}] {}", record.target(), record.args());
            // printf-style direct copy to stdout, so the log is visible wherever
            // the daemon's stdout goes (hdc console / redirect file). hilog stays
            // the primary sink; failures are ignored because a daemon may run
            // with stdout closed and a panic here would kill the process.
            {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "hicodeerd {} {}", record.level(), message);
                let _ = out.flush();
            }
            super::mirror(record.level(), record.args());
            let Ok(message_c) = CString::new(message) else {
                return;
            };
            // SAFETY: message_c is a NUL-terminated C string alive for the call;
            // the variadic argument is consumed by the `%{public}s` format.
            unsafe {
                OH_LOG_Print(
                    LOG_APP,
                    hilog_level(record.level()),
                    HILOG_DOMAIN,
                    HILOG_TAG.as_ptr(),
                    HILOG_FORMAT.as_ptr(),
                    message_c.as_ptr(),
                );
            }
        }

        fn flush(&self) {}
    }

    static LOGGER: HilogLogger = HilogLogger;

    /// Installs the hilog logger as the process-wide `log` logger.
    pub fn init() {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(LevelFilter::Info);
    }
}

#[cfg(not(target_env = "ohos"))]
mod stderr_logger {
    use log::{LevelFilter, Log, Metadata, Record};

    struct StderrLogger;

    impl Log for StderrLogger {
        fn enabled(&self, _metadata: &Metadata) -> bool {
            true
        }

        fn log(&self, record: &Record) {
            eprintln!("hicodeerd {}: {}", record.level(), record.args());
            super::mirror(record.level(), record.args());
        }

        fn flush(&self) {}
    }

    /// Installs the stderr logger as the process-wide `log` logger.
    pub fn init() {
        let _ = log::set_logger(&StderrLogger);
        log::set_max_level(LevelFilter::Info);
    }
}
