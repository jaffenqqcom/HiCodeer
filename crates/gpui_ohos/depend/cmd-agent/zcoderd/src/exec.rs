//! Command execution for the zcoderd SSH exec channels.
//!
//! Every exec request runs `sh -c <command>` as its own process-group leader
//! (`process_group(0)`, so pid == pgid). A global in-memory session table maps
//! each client-chosen session id to the group leader pid (DESIGN.md section 5:
//! pids live in memory, never in pid files). Signaling a running group goes
//! through the reserved command `__zcoderd_signal__ <session_id> <signal>`,
//! intercepted here rather than spawned. Stdout is bridged to the channel's
//! Data stream and stderr to ExtendedData; the exit status is reported over
//! exit-status, and a signal-killed child over exit-signal.
//!
//! The stdio bridge keeps BOTH directions open until the child actually exits:
//! a long-lived LSP server (clangd) reads its stdin and writes its stdout for
//! the whole session, so the SSH channel must not close either direction on a
//! transient condition. Only `child.wait()` completion ends the bridge.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::{Arc, LazyLock, Mutex};

use russh::server::Handle;
use russh::{ChannelId, CryptoVec};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::protocol;

/// Bytes read from the child's stdout/stderr per select iteration.
const IO_CHUNK_SIZE: usize = 8192;
/// Pause after a transient EOF so a truly-closed pipe does not busy-loop the
/// select, while still keeping the direction polled for an LSP that resumes.
const SLEEP_AFTER_EOF: std::time::Duration = std::time::Duration::from_millis(10);
/// Exit status used when a child dies without a status (not signaled, e.g. the
/// channel closed early) -- maps to util::command's None -> 128 convention.
const EXIT_UNKNOWN: u32 = 128;
/// Exit status reported when spawning the shell itself fails.
const EXIT_SPAWN_FAILED: u32 = 127;
/// Exit status reported when a signal targets an unknown session.
const EXIT_SIGNAL_UNKNOWN_SESSION: u32 = 1;
/// How many chars of the shell command to include in the spawn log line.
const CMD_LOG_PREVIEW_CHARS: usize = 300;

/// session id -> process group leader pid of the running command.
static SESSIONS: LazyLock<Mutex<HashMap<u64, i32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Records a newly spawned command's group leader pid under its session id.
fn register_session(session_id: u64, pid: i32) {
    SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(session_id, pid);
    log::info!("exec: registered session={session_id} pgid={pid}");
}

/// Drops a session entry once its command has fully exited.
fn unregister_session(session_id: u64) {
    let removed = SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&session_id)
        .is_some();
    if removed {
        log::info!("exec: unregistered session={session_id}");
    }
}

/// Returns the recorded process-group leader pid for a session, if any.
fn session_pgid(session_id: u64) -> Option<i32> {
    SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&session_id)
        .copied()
}

/// Per-command stdin handle, kept between handler callbacks so the client's
/// stdin Data packets can be forwarded to the running child.
pub struct ChildHandle {
    pub stdin: ChildStdin,
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // The stdin pipe write end closes here; an LSP (clangd) reads EOF on
        // stdin and reports a transport error, so this must only happen after
        // the child truly exits.
        log::debug!("exec: ChildStdin dropped (stdin closed for command)");
    }
}

/// Forwards client stdin bytes into the child's stdin pipe.
pub async fn forward_stdin(
    children: &Arc<AsyncMutex<HashMap<ChannelId, ChildHandle>>>,
    channel: ChannelId,
    data: &[u8],
) {
    use tokio::io::AsyncWriteExt;
    let mut map = children.lock().await;
    if let Some(child) = map.get_mut(&channel) {
        match child.stdin.write_all(data).await {
            Ok(()) => log::info!("exec: forwarded {} bytes stdin to channel={channel}", data.len()),
            Err(err) => {
                log::error!("exec: write child stdin channel={channel} failed: {err}");
            }
        }
    } else {
        log::warn!("exec: no live child for stdin channel={channel}");
    }
}

/// If `command` is a bare `which` invocation, returns the program names it was
/// asked to locate (flags and the `which` keyword itself stripped). Returns
/// `None` for any command that is not a plain `which` call -- a shell pipeline,
/// compound, redirection, or anything else whose exit status would not reflect
/// `which` alone -- so a missing-program message is never fabricated for it.
fn parse_which_programs(command: &str) -> Option<Vec<String>> {
    let trimmed = command.trim();
    if trimmed.contains('|')
        || trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains('>')
        || trimmed.contains('<')
        || trimmed.contains('`')
        || trimmed.contains('$')
        || trimmed.contains('(')
        || trimmed.contains(')')
    {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    if parts.next() != Some("which") {
        return None;
    }
    let programs: Vec<String> = parts
        .filter(|tok| !tok.starts_with('-'))
        .map(|tok| tok.to_string())
        .collect();
    Some(programs)
}

/// Spawns `sh -c <command>` in a fresh process group and bridges its stdio to
/// the channel until the child exits. A reserved signal command is intercepted
/// instead of spawned. The bridging loop and exit reporting run to completion
/// in a background task so the exec_request callback returns immediately.
pub async fn spawn_command(
    children: Arc<AsyncMutex<HashMap<ChannelId, ChildHandle>>>,
    channel: ChannelId,
    handle: Handle,
    command: &str,
) {
    // Reserved signal command: signal the recorded process group, never spawn.
    if let Some((session_id, signal)) = protocol::parse_signal_command(command) {
        signal_session(channel, &handle, session_id, signal).await;
        return;
    }

    let (session_id, shell_command) = protocol::split_sid_payload(command);
    // Detect a plain `which` call so we can surface the missing program name(s)
    // on stdout when nothing is found (see the bridging task below).
    let which_programs = parse_which_programs(&shell_command);
    // WARN level so per-command execution is visible on the device even when the
    // standalone zcoderd process's INFO records are suppressed (observed on OHOS).
    // Preview is captured before shell_command is moved into the Command below.
    let cmd_preview: String = shell_command.chars().take(CMD_LOG_PREVIEW_CHARS).collect();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(shell_command);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Make the child its own process-group leader (pid == pgid) so signaling
    // `-pgid` (see SESSIONS) reaches the whole command group.
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::error!("exec: spawn sh -c failed channel={channel}: {err}");
            let _ = handle.channel_failure(channel).await;
            let _ = handle.exit_status_request(channel, EXIT_SPAWN_FAILED).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
            return;
        }
    };
    let pid = child.id().unwrap_or(0);
    log::warn!("exec: spawned session={session_id} pid={pid} cmd={cmd_preview}");

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Some(stdin) = child.stdin.take() {
        children.lock().await.insert(channel, ChildHandle { stdin });
    } else {
        log::error!("exec: no stdin pipe for channel={channel} pid={pid}");
    }

    if session_id != 0 {
        register_session(session_id, pid as i32);
    }

    tokio::spawn(async move {
        // Bridge both directions until the child exits; never close a
        // direction while the channel is alive (LSP servers stay resident).
        let exit = bridge_until_exit(&mut child, &mut stdout, &mut stderr, &handle, channel).await;
        // For a `which` call that found nothing, print an explicit miss line
        // to stdout so callers (e.g. the command panel probing PATH) can see
        // exactly what is absent. `which` prints nothing to stdout on a miss,
        // so this line is the only stdout content in that case.
        if let (Some(programs), Some(status)) = (&which_programs, exit) {
            if status.code() != Some(0) && !programs.is_empty() {
                for prog in programs {
                    // Unconditional stdout notice, independent of the --log
                    // flag: a missed `which` must always be visible on the
                    // device console.
                    println!("zcoderd: which command do not found \"{prog}\"");
                    let line = format!("which: not found: {prog}\n");
                    let _ = handle.data(channel, CryptoVec::from(line.as_bytes())).await;
                }
            }
        }
        report_exit(exit, &cmd_preview, &handle, channel).await;
        let _ = handle.eof(channel).await;
        let _ = handle.close(channel).await;
        children.lock().await.remove(&channel);
        if session_id != 0 {
            unregister_session(session_id);
        }
        log::info!("exec: channel={channel} closed");
    });
}

/// Intercepts `__zcoderd_signal__ <session_id> <signal>`: kills the recorded
/// process group and replies with the command exit status.
async fn signal_session(channel: ChannelId, handle: &Handle, session_id: u64, signal: i32) {
    let pgid = session_pgid(session_id);
    match pgid {
        Some(pgid) => {
            // SAFETY: kill on a negative pid targets the process group; the
            // group leader exists and belongs to this server until unregistered.
            let result = unsafe { libc::kill(-pgid, signal) };
            if result == 0 {
                log::warn!("exec: signaled session={session_id} pgid={pgid} sig={signal}");
                let _ = handle.exit_status_request(channel, 0).await;
            } else {
                log::warn!(
                    "exec: kill session={session_id} pgid={pgid} sig={signal}: {}",
                    std::io::Error::last_os_error()
                );
                let _ = handle.exit_status_request(channel, EXIT_SIGNAL_UNKNOWN_SESSION).await;
            }
        }
        None => {
            log::warn!("exec: signal for unknown session={session_id}");
            let _ = handle.exit_status_request(channel, EXIT_SIGNAL_UNKNOWN_SESSION).await;
        }
    }
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// Reads child stdout/stderr into channel Data/ExtendedData until the child
/// exits. A long-lived LSP writes stdout continuously; we keep reading it and
/// never end on an EOF alone -- only `child.wait()` completes the bridge.
async fn bridge_until_exit(
    child: &mut Child,
    stdout: &mut Option<ChildStdout>,
    stderr: &mut Option<ChildStderr>,
    handle: &Handle,
    channel: ChannelId,
) -> Option<ExitStatus> {
    let mut out_buf = [0u8; IO_CHUNK_SIZE];
    let mut err_buf = [0u8; IO_CHUNK_SIZE];
    loop {
        tokio::select! {
            status = child.wait() => {
                log::debug!("exec: child exited channel={channel}");
                match status {
                    Ok(st) => {
                        // Drain any remaining stdio after exit before reporting.
                        drain_remaining(stdout, stderr, handle, channel).await;
                        return Some(st);
                    }
                    Err(_) => return None,
                }
            }
            // stdout/stderr stay polled for the whole session: a long-lived LSP
            // may temporarily return EOF then resume, so a transient 0 is NOT a
            // reason to drop the direction. Only child.wait() ends the bridge.
            result = read_chunk(stdout.as_mut(), &mut out_buf), if stdout.is_some() => {
                match result {
                    Ok(0) => {
                        log::debug!("exec: stdout EOF channel={channel}, sending eof");
                        let _ = handle.eof(channel).await;
                        tokio::time::sleep(SLEEP_AFTER_EOF).await;
                    }
                    Ok(n) => {
                        if let Err(err) = handle.data(channel, CryptoVec::from(&out_buf[..n])).await {
                            log::warn!("exec: send stdout channel={channel}: {err:?}");
                        }
                    }
                    Err(err) => {
                        log::warn!("exec: read stdout channel={channel}: {err}");
                    }
                }
            }
            result = read_chunk(stderr.as_mut(), &mut err_buf), if stderr.is_some() => {
                match result {
                    Ok(0) => {
                        log::debug!("exec: stderr EOF channel={channel}");
                        let _ = handle.eof(channel).await;
                        tokio::time::sleep(SLEEP_AFTER_EOF).await;
                    }
                    Ok(n) => {
                        if let Err(err) = handle.extended_data(channel, 1, CryptoVec::from(&err_buf[..n])).await {
                            log::warn!("exec: send stderr channel={channel}: {err:?}");
                        }
                    }
                    Err(err) => {
                        log::warn!("exec: read stderr channel={channel}: {err}");
                    }
                }
            }
        }
    }
}

/// Reads one chunk from a child pipe, returning the byte count (0 on EOF).
async fn read_chunk<R: AsyncReadExt + Unpin>(
    reader: Option<&mut R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(r) => r.read(buf).await,
        None => Ok(0),
    }
}

/// Drains any bytes still buffered in the child pipes after it exits, so no
/// output is lost before the exit status is reported.
async fn drain_remaining(
    stdout: &mut Option<ChildStdout>,
    stderr: &mut Option<ChildStderr>,
    handle: &Handle,
    channel: ChannelId,
) {
    use tokio::io::AsyncReadExt as _;
    let mut buf = [0u8; IO_CHUNK_SIZE];
    if let Some(out) = stdout.as_mut() {
        loop {
            match out.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Err(err) = handle.data(channel, CryptoVec::from(&buf[..n])).await {
                        log::warn!("exec: drain stdout channel={channel}: {err:?}");
                        break;
                    }
                }
            }
        }
    }
    if let Some(err) = stderr.as_mut() {
        loop {
            match err.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Err(e2) = handle
                        .extended_data(channel, 1, CryptoVec::from(&buf[..n]))
                        .await
                    {
                        log::warn!("exec: drain stderr channel={channel}: {e2:?}");
                        break;
                    }
                }
            }
        }
    }
}

/// Reports the child's exit status or signal to the client.
/// Reports how a command ended on the channel, pairing every outcome with the
/// command preview so each executed command can be traced start-to-end in the
/// log (spawn line + result line carry the same `cmd=` text).
async fn report_exit(
    exit: Option<ExitStatus>,
    cmd: &str,
    handle: &Handle,
    channel: ChannelId,
) {
    match exit {
        Some(status) => {
            if let Some(code) = status.code() {
                if code == 0 {
                    log::warn!("exec: cmd={cmd} exit=0 success");
                } else {
                    log::warn!("exec: cmd={cmd} exit={code} failed");
                }
                let _ = handle.exit_status_request(channel, code as u32).await;
            } else if let Some(signal) = status.signal() {
                let sig = signal_name(signal);
                log::warn!("exec: cmd={cmd} killed by signal={sig:?} failed");
                let _ = handle
                    .exit_signal_request(channel, sig, false, String::new(), String::new())
                    .await;
            } else {
                log::warn!("exec: cmd={cmd} exited without status failed");
                let _ = handle.exit_status_request(channel, EXIT_UNKNOWN).await;
            }
        }
        None => {
            log::error!("exec: cmd={cmd} wait failed");
            let _ = handle.exit_status_request(channel, EXIT_UNKNOWN).await;
        }
    }
}

/// Maps a raw signal number to a russh `Sig` (falling back to a generic term
/// when the number is unknown to russh).
fn signal_name(signal: i32) -> russh::Sig {
    match signal {
        1 => russh::Sig::HUP,
        2 => russh::Sig::INT,
        9 => russh::Sig::KILL,
        15 => russh::Sig::TERM,
        _ => russh::Sig::TERM,
    }
}
