//! Interactive shell (pty) sessions against a daemon backend.
//!
//! A terminal that wants an interactive shell asks the backend for a pty and
//! then execs a command on it; the daemon runs that command on a backend-side
//! openpty (`/bin/sh` on the device or in the QEMU guest, depending on which
//! backend the launch layer selected). This module opens the channel, requests
//! the pty, execs the command, and relays the master data to the caller through
//! socketpairs (mirroring the exec `RemoteChild` plumbing): shell output
//! arrives on `stdout`, keystrokes go to `stdin`, and `resize` forwards window
//! changes over SSH so full-screen programs keep their layout.

use std::io::Cursor;
use std::os::unix::net::UnixStream;

use russh::ChannelMsg;
use smol::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::pool::SshSession;

/// Terminal type advertised in the pty request; Zed's terminal emulates it.
const TERM_TYPE: &str = "xterm-256color";
/// Bytes read from the socketpair per relay iteration.
const PTY_CHUNK: usize = 8192;

/// A live interactive shell session on a remote pty.
pub struct RemotePty {
    /// Caller-side stdin: write keystrokes here.
    pub stdin: Option<Box<dyn AsyncWrite + Unpin + Send>>,
    /// Caller-side stdout: shell output arrives here.
    pub stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    resize_tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>,
}

impl RemotePty {
    /// Forwards a terminal resize to the remote shell.
    pub fn resize(&self, cols: u32, rows: u32) {
        let _ = self.resize_tx.send((cols, rows));
    }

    /// Returns a cheap cloneable handle for forwarding resizes from a thread
    /// that does not own the session.
    pub fn resize_handle(&self) -> ResizeHandle {
        ResizeHandle {
            tx: self.resize_tx.clone(),
        }
    }
}

/// Cloneable resize forwarder for a live remote pty.
#[derive(Clone)]
pub struct ResizeHandle {
    tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>,
}

impl ResizeHandle {
    /// Forwards a terminal resize to the remote shell.
    pub fn resize(&self, cols: u32, rows: u32) {
        let _ = self.tx.send((cols, rows));
    }
}

/// Builds the shell command run on the backend pty: enter the caller's
/// directory when it exists there (best effort: a stale path must not block the
/// shell), then replace the wrapper shell with the program and arguments the
/// caller specified. Nothing is substituted: whatever the caller asks for is
/// what the pty runs. An empty program is refused rather than defaulted, so a
/// caller that names no shell is told instead of silently getting one.
pub(crate) fn shell_command(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
) -> std::io::Result<String> {
    if program.is_empty() {
        log::error!("cmd-client pty: shell_command called with an empty program");
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pty shell: caller specified no program",
        ));
    }
    let mut command = vec![crate::command::sh_quote(program)];
    command.extend(args.iter().map(|arg| crate::command::sh_quote(arg)));
    let exec = format!("exec {}", command.join(" "));
    Ok(match cwd.filter(|dir| !dir.is_empty()) {
        Some(dir) => format!("cd {} 2>/dev/null; {exec}", crate::command::sh_quote(dir)),
        None => exec,
    })
}

/// Opens an interactive shell channel on `conn`: pty request, then `command`
/// exec (the caller passes `exec <program> <args>`, optionally preceded by a `cd`). Both
/// steps complete before this future resolves, so a backend that cannot serve
/// a pty surfaces an error and the caller falls back to a local shell. The
/// relay task then runs on the current tokio runtime until the channel closes.
pub(crate) async fn open_shell_pty(
    conn: SshSession,
    cols: u32,
    rows: u32,
    command: &str,
) -> std::io::Result<RemotePty> {
    let mut channel = conn
        .channel_open_session()
        .await
        .map_err(|err| std::io::Error::other(format!("open channel: {err}")))?;
    channel
        .request_pty(true, TERM_TYPE, cols, rows, 0, 0, &[])
        .await
        .map_err(|err| std::io::Error::other(format!("request pty: {err}")))?;
    channel
        .exec(true, command)
        .await
        .map_err(|err| std::io::Error::other(format!("exec shell: {err}")))?;

    let (stdout_reader, stdout_pump) = UnixStream::pair()?;
    let (stdin_pump, stdin_writer) = UnixStream::pair()?;

    let stdout: Box<dyn AsyncRead + Unpin + Send> = Box::new(smol::Async::new(stdout_reader)?);
    let stdin: Box<dyn AsyncWrite + Unpin + Send> = Box::new(smol::Async::new(stdin_writer)?);

    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, u32)>();

    tokio::spawn(async move {
        let mut stdout_pump = stdout_pump;
        let mut stdin_pump = stdin_pump;
        for s in [&stdout_pump, &stdin_pump] {
            if let Err(err) = s.set_nonblocking(true) {
                log::error!("cmd-client pty: set nonblocking: {err}");
                return;
            }
        }
        let mut stdout_w = match tokio::net::UnixStream::from_std(stdout_pump) {
            Ok(s) => s,
            Err(err) => {
                log::error!("cmd-client pty: wrap stdout: {err}");
                return;
            }
        };
        let mut stdin_r = match tokio::net::UnixStream::from_std(stdin_pump) {
            Ok(s) => s,
            Err(err) => {
                log::error!("cmd-client pty: wrap stdin: {err}");
                return;
            }
        };

        let mut buf = [0u8; PTY_CHUNK];
        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            if stdout_w.write_all(&data).await.is_err() {
                                log::warn!("cmd-client pty: write stdout failed");
                                break;
                            }
                        }
                        Some(ChannelMsg::ExtendedData { data, .. }) => {
                            if stdout_w.write_all(&data).await.is_err() {
                                log::warn!("cmd-client pty: write stderr-as-out failed");
                                break;
                            }
                        }
                        Some(ChannelMsg::Close) | None => break,
                        Some(_) => {}
                    }
                }
                n = stdin_r.read(&mut buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            if channel.data(Cursor::new(&buf[..n])).await.is_err() {
                                log::warn!("cmd-client pty: send input failed");
                                break;
                            }
                        }
                        Err(err) => {
                            log::warn!("cmd-client pty: read stdin: {err}");
                            break;
                        }
                    }
                }
                resize = resize_rx.recv() => {
                    match resize {
                        Some((cols, rows)) => {
                            if channel.window_change(cols, rows, 0, 0).await.is_err() {
                                log::warn!("cmd-client pty: window_change failed");
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        let _ = stdout_w.shutdown().await;
    });

    Ok(RemotePty {
        stdin: Some(stdin),
        stdout: Some(stdout),
        resize_tx,
    })
}
