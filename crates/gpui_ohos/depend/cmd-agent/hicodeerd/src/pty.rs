//! Interactive shell sessions for the SSH pty-req path.
//!
//! When a cmd-client terminal requests a pseudo-terminal on a channel and then
//! asks to execute a shell, the daemon allocates a pty (posix_openpt), runs the
//! exec payload through `/bin/sh -c` with the slave as its controlling stdio,
//! and relays the master in both directions over the SSH channel (master
//! output -> channel Data, channel input -> master). Window-change requests
//! resize the pty via TIOCSWINSZ so full-screen apps behave.
//!
//! The child gets its own session (`setsid`) and takes the slave as its
//! controlling terminal (`TIOCSCTTY`), which is what makes job control and
//! Ctrl-C work for the shell and everything it spawns.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use russh::server::Handle;
use russh::{ChannelId, CryptoVec};
use tokio::io::unix::AsyncFd;
use tokio::sync::Mutex as AsyncMutex;

/// Delay between master reads while idle (the channel keeps a resident shell).
const RELAY_POLL: Duration = Duration::from_millis(20);
/// IO chunk size for pty relay.
const PTY_CHUNK: usize = 8192;
/// Program the exec payload is handed to (`sh -c <payload>`); the payload
/// itself decides which interactive shell ends up on the pty.
const PTY_SHELL: &str = "/bin/sh";
/// Largest window dimension a client may request: `winsize` fields are `u16`.
const MAX_PTY_DIMENSION: u32 = u16::MAX as u32;
/// Number of standard streams wired to the pty slave (stdin, stdout, stderr).
const STDIO_STREAMS: usize = 3;

/// Master ends (one per live pty channel), registered as `AsyncFd` so client
/// input packets are written without blocking the runtime.
///
/// Keyed by `(conn_id, channel)`: channel ids are allocated per SSH connection,
/// so a bare `ChannelId` collides across connections -- a terminal pty on one
/// connection would otherwise swallow the stdin of an exec child on another.
static MASTERS: LazyLock<Mutex<HashMap<(u64, ChannelId), Arc<AsyncMutex<AsyncFd<File>>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Allocates a pty, returns (master_fd, slave_path) after setting its size.
fn open_pty(cols: u16, rows: u16) -> std::io::Result<(RawFd, String)> {
    // SAFETY: plain libc calls; the returned master fd is owned by the caller.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: grantpt/unlockpt on a freshly opened master.
    if unsafe { libc::grantpt(master) } != 0 || unsafe { libc::unlockpt(master) } != 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: close the master we opened.
        unsafe { libc::close(master) };
        return Err(err);
    }
    let name_ptr = unsafe { libc::ptsname(master) };
    if name_ptr.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(master) };
        return Err(err);
    }
    // SAFETY: ptsname returns a NUL-terminated static string.
    let slave_path = unsafe { std::ffi::CStr::from_ptr(name_ptr) }
        .to_string_lossy()
        .into_owned();
    if let Err(err) = set_size(master, cols, rows) {
        // SAFETY: close the master we opened.
        unsafe { libc::close(master) };
        return Err(err);
    }
    Ok((master, slave_path))
}

/// One slave handle per standard stream, so the child's stdin/stdout/stderr all
/// refer to the pty.
fn slave_stdio(slave: &File) -> std::io::Result<[File; STDIO_STREAMS]> {
    let mut streams = Vec::with_capacity(STDIO_STREAMS);
    for _ in 0..STDIO_STREAMS {
        streams.push(slave.try_clone()?);
    }
    streams
        .try_into()
        .map_err(|_| std::io::Error::other("slave stdio dup count"))
}

/// Sets the pty window size with TIOCSWINSZ on the given fd (master or slave).
fn set_size(fd: RawFd, cols: u16, rows: u16) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl TIOCSWINSZ on a real pty fd.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Puts `fd` in nonblocking mode, which `AsyncFd` requires.
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl F_GETFL/F_SETFL on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Clamps a client-supplied dimension into the `winsize` range.
fn clamp_dimension(value: u32) -> u16 {
    value.clamp(1, MAX_PTY_DIMENSION) as u16
}

/// Looks up the master registered for a live pty channel of `conn_id`. The std
/// lock is released before returning so the caller may await on the master
/// without holding a non-`Send` guard across the await point.
fn master_for(conn_id: u64, channel: ChannelId) -> Option<Arc<AsyncMutex<AsyncFd<File>>>> {
    let masters = MASTERS.lock().unwrap_or_else(|p| p.into_inner());
    masters.get(&(conn_id, channel)).cloned()
}

/// Handles a window-change request for a live pty channel.
pub async fn resize(conn_id: u64, channel: ChannelId, cols: u32, rows: u32) {
    let Some(master) = master_for(conn_id, channel) else {
        return;
    };
    let master_guard = master.lock().await;
    let fd = master_guard.get_ref().as_raw_fd();
    if let Err(err) = set_size(fd, clamp_dimension(cols), clamp_dimension(rows)) {
        log::warn!("pty: resize channel={channel} to {cols}x{rows}: {err}");
    }
}

/// Forwards client stdin bytes into the pty master for a live channel. Returns
/// false when the channel is not a pty session (caller falls back to the pipe
/// executor path).
pub async fn forward_input(conn_id: u64, channel: ChannelId, data: &[u8]) -> bool {
    let Some(master) = master_for(conn_id, channel) else {
        return false;
    };
    let master_guard = master.lock().await;
    if let Err(err) = write_master(&master_guard, data).await {
        log::warn!("pty: write master channel={channel}: {err}");
    }
    true
}

/// Writes the whole buffer to the master, waiting for writability as needed.
async fn write_master(master: &AsyncFd<File>, data: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let mut ready = master.writable().await?;
        match ready.try_io(|fd| fd.get_ref().write(&data[written..])) {
            Ok(Ok(0)) => return Err(std::io::Error::other("pty master write returned 0")),
            Ok(Ok(n)) => written += n,
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(Err(err)) => return Err(err),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// Runs `command` (the client's exec payload, e.g. `cd <dir>; exec sh`) on a
/// freshly allocated pty for `channel` and relays the master both ways until
/// the shell exits. `term` is the terminal type from the pty request and is
/// exported as `TERM` for the child. `client_id` names the instance that opened
/// the terminal, so its shell goes down with the rest of that client's tree
/// (see `peers`).
pub async fn run_pty_shell(
    conn_id: u64,
    channel: ChannelId,
    handle: Handle,
    cols: u32,
    rows: u32,
    command: &str,
    term: &str,
    client_id: &str,
) {
    let (master_fd, slave_path) = match open_pty(clamp_dimension(cols), clamp_dimension(rows)) {
        Ok(pair) => pair,
        Err(err) => {
            log::error!("pty: openpty channel={channel}: {err}");
            let _ = handle.channel_failure(channel).await;
            return;
        }
    };
    // SAFETY: we own master_fd; wrap it for the write map and dup a second for
    // the read side below.
    let master_for_input = unsafe { File::from_raw_fd(libc::dup(master_fd)) };

    // The slave must be opened read-write. `File::open` is O_RDONLY: handing
    // the child three read-only stdio fds would make every shell write (echo,
    // prompt, command output) fail with EBADF, so the terminal sees no output
    // at all while input still works. An O_RDWR slave is what lets the shell's
    // stdout/stderr reach the master read side.
    let slave = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
    {
        Ok(f) => f,
        Err(err) => {
            log::error!("pty: open slave {slave_path}: {err}");
            // SAFETY: close the raw fds we own.
            unsafe { libc::close(master_fd) };
            let _ = handle.channel_failure(channel).await;
            return;
        }
    };
    let slave_fd = slave.as_raw_fd();

    let [child_stdin, child_stdout, child_stderr] = match slave_stdio(&slave) {
        Ok(streams) => streams,
        Err(err) => {
            log::error!("pty: dup slave channel={channel}: {err}");
            // SAFETY: close the raw fds we own.
            unsafe { libc::close(master_fd) };
            let _ = handle.channel_failure(channel).await;
            return;
        }
    };

    let mut cmd = tokio::process::Command::new(PTY_SHELL);
    cmd.arg("-c").arg(command);
    cmd.stdin(std::process::Stdio::from(child_stdin));
    cmd.stdout(std::process::Stdio::from(child_stdout));
    cmd.stderr(std::process::Stdio::from(child_stderr));
    if !term.is_empty() {
        cmd.env("TERM", term);
    }
    crate::shim::apply(&mut cmd);
    // SAFETY: the closure runs in the forked child before exec and only calls
    // async-signal-safe libc functions; `slave_fd` stays open in the child
    // until exec (it is still referenced by the stdio setup).
    unsafe {
        cmd.pre_exec(move || {
            // Own session: the shell becomes a process group leader, which is
            // the precondition for acquiring a controlling terminal.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::error!("pty: spawn {PTY_SHELL} channel={channel}: {err}");
            // SAFETY: close raw fds owned here.
            unsafe { libc::close(master_fd) };
            let _ = handle.channel_failure(channel).await;
            return;
        }
    };
    // The child owns the slave now; holding a copy here would keep the pty from
    // reporting EOF to the master read side.
    drop(slave);

    // The shell leads its own session (see the pre_exec above), so its pid is
    // also its process-group id. Recording that group is what lets a terminal's
    // whole tree go down with the client that opened it.
    let shell_pgid = child.id().unwrap_or(0) as i32;
    crate::peers::add_group(client_id, shell_pgid);

    let master_for_input =
        match set_nonblocking(master_for_input.as_raw_fd()).and_then(|()| AsyncFd::new(master_for_input)) {
        Ok(master) => master,
        Err(err) => {
            log::error!("pty: master write side channel={channel}: {err}");
            let _ = child.kill().await;
            crate::peers::drop_group(client_id, shell_pgid);
            // SAFETY: close the raw fd we own.
            unsafe { libc::close(master_fd) };
            let _ = handle.channel_failure(channel).await;
            return;
        }
    };

    MASTERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert((conn_id, channel), Arc::new(AsyncMutex::new(master_for_input)));

    let _ = handle.channel_success(channel).await;

    // SAFETY: dup the master for the async read side; owned by us.
    let read_fd = unsafe { libc::dup(master_fd) };
    unsafe { libc::close(master_fd) };

    // Master read side -> channel Data.
    let reader = match set_nonblocking(read_fd).and_then(|()| {
        // SAFETY: read_fd is a fresh fd owned here.
        AsyncFd::new(unsafe { File::from_raw_fd(read_fd) })
    }) {
        Ok(f) => f,
        Err(err) => {
            log::error!("pty: async master read channel={channel}: {err}");
            let _ = child.kill().await;
            crate::peers::drop_group(client_id, shell_pgid);
            MASTERS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&(conn_id, channel));
            return;
        }
    };

    let mut buf = [0u8; PTY_CHUNK];
    loop {
        tokio::select! {
            _ = child.wait() => break,
            r = reader.readable() => {
                let mut ready = match r {
                    Ok(ready) => ready,
                    Err(err) => {
                        log::warn!("pty: master poll channel={channel}: {err}");
                        break;
                    }
                };
                match ready.try_io(|fd| fd.get_ref().read(&mut buf)) {
                    Ok(Ok(n)) if n > 0 => {
                        if let Err(err) = handle.data(channel, CryptoVec::from(&buf[..n])).await {
                            log::warn!(
                                "pty: send data channel={channel} failed ({} bytes dropped)",
                                err.len()
                            );
                        }
                    }
                    Ok(Ok(_)) => tokio::time::sleep(RELAY_POLL).await,
                    Ok(Err(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(RELAY_POLL).await;
                    }
                    Ok(Err(err)) => {
                        log::warn!("pty: read master channel={channel}: {err}");
                        break;
                    }
                    Err(_) => tokio::time::sleep(RELAY_POLL).await,
                }
            }
        }
    }
    // The shell exited on its own or the channel died: reap it so a client
    // disconnect never leaves an orphan shell behind. A kill that fails here
    // has already exited, which is the case being handled.
    let _ = child.kill().await;
    crate::peers::drop_group(client_id, shell_pgid);
    MASTERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&(conn_id, channel));
    let _ = handle.exit_status_request(channel, 0).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}
