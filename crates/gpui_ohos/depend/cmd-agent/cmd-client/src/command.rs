//! Translates an `ExecSpec` into a POSIX sh command string for zcoderd.
//!
//! The SSH session env is empty, so the spec's own env is injected as an exec
//! prefix. No PATH is
//! injected: the shell zcoderd spawns inherits the device PATH, and started
//! LSPs are addressed by absolute path. Paths are passed through verbatim (no
//! mapping:
//! zcoderd and zcoder share the same device filesystem). The cwd is created
//! before cd-ing. A reserved first line carries the session id that zcoderd
//! uses to associate the exec with its in-memory session table.

use crate::types::{ExecSpec, FdMode};

/// Environment key set for git to suppress "dubious ownership" (the command may
/// run under a uid different from the file owner).
const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
const GIT_CONFIG_KEY: &str = "GIT_CONFIG_KEY_0";
const GIT_CONFIG_VALUE: &str = "GIT_CONFIG_VALUE_0";

/// Builds the shell command string for one exec request.
pub fn build_command(spec: &ExecSpec, session_id: u64) -> String {
    // Working directory: created then entered. Missing cwd must not fail the
    // whole command (a stale path would otherwise make git/LSP unusable).
    let mut parts: Vec<String> = Vec::new();
    if let Some(cwd) = &spec.cwd_path {
        if !cwd.is_empty() {
            parts.push(format!("mkdir -p {}", sh_quote(cwd)));
            parts.push(format!("cd {}", sh_quote(cwd)));
        }
    }

    // Environment: apply the caller's env verbatim. The shell zcoderd spawns
    // already inherits the device PATH (LSPs are started by absolute path, and
    // `which` only searches PATH dirs directly, so injecting the LSP download
    // dir here would be a no-op) and its own HOME from the guest environment.
    let mut envs: Vec<String> = Vec::new();
    if spec.source_program == "git" {
        envs.push(format!("{GIT_CONFIG_COUNT}=1"));
        envs.push(format!("{GIT_CONFIG_KEY}={}", sh_quote("safe.directory")));
        envs.push(format!("{GIT_CONFIG_VALUE}={}", sh_quote("*")));
    }
    for (key, value) in &spec.env {
        envs.push(format!("{}={}", key, sh_quote(value)));
    }

    let mut command = vec![sh_quote(&spec.binary)];
    command.extend(spec.args.iter().map(|arg| sh_quote(arg)));
    let mut line = format!("{} exec {}", envs.join(" "), command.join(" "));

    // Fd redirection: /dev/null for Null modes, otherwise the SSH channel.
    match spec.stdin_mode {
        FdMode::Null => line.push_str(" </dev/null"),
        FdMode::Piped => {}
    }
    match spec.stdout_mode {
        FdMode::Null => line.push_str(" >/dev/null"),
        FdMode::Piped => {}
    }
    match spec.stderr_mode {
        FdMode::Null => line.push_str(" 2>/dev/null"),
        FdMode::Piped => {}
    }
    parts.push(line);

    crate::protocol::sid_payload(session_id, &parts.join(" && "))
}

/// Quotes a value for a POSIX shell: single quotes with `'\''` escaping.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
