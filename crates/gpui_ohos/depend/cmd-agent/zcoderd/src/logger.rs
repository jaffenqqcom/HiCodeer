//! zcoderd logging sink.
//!
//! zcoderd runs as an OHOS native process on the device, so `cfg(ohos)` routes
//! every `log::xxx!` record through `OH_LOG_Print` into hilog; when built for a
//! plain host (local two-process bring-up on the VM) it logs to stderr instead.
//!
//! Logging is opt-in: `init(false)` (the default, no `--log` argument) installs
//! nothing and every `log::xxx!` call becomes a no-op; `--log` enables both
//! sinks. Startup failure still prints to stderr unconditionally from `main`.

/// Installs the process-wide logger when `enabled` (the `--log` flag); with
/// `enabled == false` no logger is installed, so all log records are dropped.
pub fn init(enabled: bool) {
    if !enabled {
        return;
    }
    #[cfg(target_env = "ohos")]
    ohos::init();
    #[cfg(not(target_env = "ohos"))]
    stderr_logger::init();
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
    const ZCODER_DOMAIN: u32 = 0x0001;
    /// hilog tag, kept short enough (<31 bytes) to avoid truncation.
    const ZCODERD_TAG: &CStr = c"Zcoderd";
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
            let message = format!("[zcoderd:{}] {}", record.target(), record.args());
            // printf-style direct copy to stdout, so the log is visible wherever
            // zcoderd's stdout goes (hdc console / redirect file). hilog stays
            // the primary sink; failures are ignored because a daemon may run
            // with stdout closed and a panic here would kill the process.
            {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "zcoderd {} {}", record.level(), message);
                let _ = out.flush();
            }
            let Ok(message_c) = CString::new(message) else {
                return;
            };
            // SAFETY: message_c is a NUL-terminated C string alive for the call;
            // the variadic argument is consumed by the `%{public}s` format.
            unsafe {
                OH_LOG_Print(
                    LOG_APP,
                    hilog_level(record.level()),
                    ZCODER_DOMAIN,
                    ZCODERD_TAG.as_ptr(),
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
            eprintln!("zcoderd {}: {}", record.level(), record.args());
        }

        fn flush(&self) {}
    }

    /// Installs the stderr logger as the process-wide `log` logger.
    pub fn init() {
        let _ = log::set_logger(&StderrLogger);
        log::set_max_level(LevelFilter::Info);
    }
}
