//! Guest-side command execution for SSH exec channels.
//!
//! Every exec request runs `sh -c <command>` as its own process-group leader
//! (`process_group(0)`, so pid == pgid). This lets the zcoder host signal the
//! whole group with `kill -KILL -<pid>` (its session table stores the pid the
//! command echoed into a pid file). Stdout is bridged to the channel's Data
//! stream and stderr to ExtendedData; the exit status is reported over
//! exit-status, and a signal-killed child over exit-signal.
//!
//! The stdio bridge keeps BOTH directions open until the child actually exits:
//! a long-lived LSP server (clangd) reads its stdin and writes its stdout for
//! the whole session, so the SSH channel must not close either direction on a
//! transient condition. Only `child.wait()` completion ends the bridge.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Arc;

use russh::server::Handle;
use russh::{ChannelId, CryptoVec};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

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
        log::warn!("[diag] exec: ChildStdin dropped (stdin closed for command)");
    }
}

/// Forwards client stdin bytes into the child's stdin pipe.
pub async fn forward_stdin(
    children: &Arc<Mutex<HashMap<ChannelId, ChildHandle>>>,
    channel: ChannelId,
    data: &[u8],
) {
    use tokio::io::AsyncWriteExt;
    let mut map = children.lock().await;
    if let Some(child) = map.get_mut(&channel) {
        match child.stdin.write_all(data).await {
            Ok(()) => log::info!(
                "[diag] exec: forwarded {} bytes stdin to channel={channel}",
                data.len()
            ),
            Err(err) => {
                log::error!("[diag] exec: write child stdin channel={channel} failed: {err}");
            }
        }
    } else {
        log::warn!("[diag] exec: no live child for stdin channel={channel}");
    }
}

/// Spawns `sh -c <command>` in a fresh process group and bridges its stdio to
/// the channel until the child exits. The bridging loop and exit reporting run
/// to completion in a background task so the exec_request callback returns
/// immediately.
pub async fn spawn_command(
    children: Arc<Mutex<HashMap<ChannelId, ChildHandle>>>,
    channel: ChannelId,
    handle: Handle,
    command: &str,
) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Make the child its own process-group leader (pid == pgid) so the host's
    // `kill -KILL -<pid>` signals the whole command group.
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
    log::info!("exec: spawned channel={channel} pid={pid} cmd={command}");
    // [diag] After the exec wrapper, check the child's stdio fds: a closed or
    // wrong fd 0 here makes a long-lived LSP read stdin with EIO.
    for fd in [0, 1, 2] {
        let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).unwrap_or_default();
        log::info!("[diag] exec: child {pid} fd{fd} -> {link:?}");
    }

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Some(stdin) = child.stdin.take() {
        children.lock().await.insert(channel, ChildHandle { stdin });
        log::info!("[diag] exec: inserted child channel={channel} pid={pid}");
    } else {
        log::error!("[diag] exec: no stdin pipe for channel={channel} pid={pid}");
    }

    tokio::spawn(async move {
        // Bridge both directions until the child exits; never close a
        // direction while the channel is alive (LSP servers stay resident).
        let exit = bridge_until_exit(&mut child, &mut stdout, &mut stderr, &handle, channel).await;
        report_exit(exit, &handle, channel).await;
        let _ = handle.eof(channel).await;
        let _ = handle.close(channel).await;
        children.lock().await.remove(&channel);
        log::info!("[diag] exec: removed child channel={channel}");
        log::info!("exec: channel={channel} closed");
    });
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
                log::info!("[diag] exec: child exited channel={channel}");
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
                        // The child closed its stdout: signal EOF to the client
                        // per the standard, so util enters its normal handling
                        // rather than waiting forever. Keep polling (the child
                        // may still be alive emitting stderr / not yet reaped).
                        log::warn!("[diag] exec: stdout EOF channel={channel}, sending eof");
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
                        log::warn!("[diag] exec: stderr EOF channel={channel}");
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
async fn report_exit(exit: Option<ExitStatus>, handle: &Handle, channel: ChannelId) {
    match exit {
        Some(status) => {
            if let Some(code) = status.code() {
                log::info!("exec: channel={channel} exit code={code}");
                let _ = handle.exit_status_request(channel, code as u32).await;
            } else if let Some(signal) = status.signal() {
                log::info!("exec: channel={channel} killed by signal={signal}");
                let sig = signal_name(signal);
                let _ = handle
                    .exit_signal_request(channel, sig, false, String::new(), String::new())
                    .await;
            } else {
                log::warn!("exec: channel={channel} exited without status");
                let _ = handle.exit_status_request(channel, EXIT_UNKNOWN).await;
            }
        }
        None => {
            log::error!("exec: channel={channel} wait failed");
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
