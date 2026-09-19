//! Remote command execution over the daemon SSH connection pool.
//!
//! `SshCommandExecutor` implements the self-contained `RemoteCommandExecutor`
//! contract. Each spawn allocates a pooled SSH connection, opens a session
//! channel, runs the translated shell command, and bridges stdio through
//! socketpairs: the caller sees smol `Async<UnixStream>` ends, while a tokio
//! pump task (on the pool runtime) relays channel Data/ExtendedData into the
//! socketpairs and reports the exit status.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use russh::ChannelMsg;
use smol::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::command;
use crate::endpoint::CommandEndpoint;
use crate::pool::{Pool, SshSession};
use crate::types::{
    ExecSpec, ExitFuture, RemoteChild, RemoteCommandExecutor, ShellPtyFuture, Signal,
};

/// Bytes read from the socketpair per select iteration.
const IO_CHUNK_SIZE: usize = 8192;

/// Per-session exit state, shared between the tokio pump task and waiters on
/// the smol executor.
pub struct SessionState {
    exit: Mutex<Option<Option<i32>>>,
    tx: tokio::sync::watch::Sender<Option<Option<i32>>>,
}

impl SessionState {
    fn new() -> Self {
        let (tx, _) = tokio::sync::watch::channel(None);
        Self {
            exit: Mutex::new(None),
            tx,
        }
    }

    /// Records the exit result once (later calls are ignored) and broadcasts it.
    /// `None` means the command died without a status (signal or connection
    /// loss), mapping to util::command's None -> 128.
    ///
    /// A `watch` (not a one-shot `Notify`) is used because it keeps the latest
    /// value: a waiter that subscribes after the broadcast still observes the
    /// terminal state, so the wake-up can never be lost to a timing race.
    fn set_exit(&self, exit: Option<i32>) {
        let mut guard = self.exit.lock().unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            *guard = Some(exit);
        }
        let value = *guard;
        drop(guard);
        // Store the terminal value unconditionally: `send()` rejects the value
        // when no receiver is alive yet (the initial receiver was dropped), so
        // a fast-exiting child would lose its exit status and a later
        // subscriber of `wait_exit_async` would block forever. `send_replace`
        // always stores, so late waiters observe the value via `borrow()`.
        self.tx.send_replace(value);
    }
}

/// Remote command executor over the SSH pool.
pub struct SshCommandExecutor {
    pool: Arc<Pool>,
    sessions: Mutex<HashMap<u64, Arc<SessionState>>>,
    next_session: AtomicU64,
}

impl SshCommandExecutor {
    /// Creates the pool, starts the management bootstrap thread against
    /// `endpoint` (dynamic command keys -> pool config) and returns the
    /// executor.
    ///
    /// One identity is minted here for the executor's whole life and presented
    /// on every connection, so the daemon can tell this instance's process tree
    /// from a predecessor's (see `protocol::new_client_id`).
    pub fn new(
        endpoint: CommandEndpoint,
        mgmt_client_priv_pem: String,
        mgmt_host_pub_pem: String,
    ) -> std::io::Result<Self> {
        let pool = Pool::new()?;
        let executor = Self {
            pool: pool.clone(),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
        };
        let client_id = crate::protocol::new_client_id();
        // Bootstrap thread: re-fetch the dynamic command keys and reconfigure
        // the pool whenever the daemon restarts. Runs off the calling thread.
        let bootstrap_pool = pool.clone();
        std::thread::Builder::new()
            .name("cmd-client-bootstrap".to_string())
            .spawn(move || {
                crate::bootstrap::start(
                    bootstrap_pool,
                    endpoint,
                    mgmt_client_priv_pem,
                    mgmt_host_pub_pem,
                    client_id,
                );
            })
            .map_err(std::io::Error::other)?;
        Ok(executor)
    }
}

impl RemoteCommandExecutor for SshCommandExecutor {
    fn spawn(&self, spec: ExecSpec) -> std::io::Result<RemoteChild> {
        let session_id = self.next_session.fetch_add(1, Ordering::SeqCst);
        let command = command::build_command(&spec, session_id);
        let conn = self.pool.allocate()?;

        // Socketpairs: caller side is smol Async<UnixStream>, pump side is a
        // tokio UnixStream on the pool runtime.
        let (stdout_reader, stdout_pump) = UnixStream::pair()?;
        let (stderr_reader, stderr_pump) = UnixStream::pair()?;
        let (stdin_pump, stdin_writer) = UnixStream::pair()?;

        let stdout: Box<dyn AsyncRead + Unpin + Send + Sync> =
            Box::new(smol::Async::new(stdout_reader)?);
        let stderr: Box<dyn AsyncRead + Unpin + Send + Sync> =
            Box::new(smol::Async::new(stderr_reader)?);
        let stdin: Box<dyn AsyncWrite + Unpin + Send + Sync> =
            Box::new(smol::Async::new(stdin_writer)?);

        let state = Arc::new(SessionState::new());
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id, state.clone());

        let runtime = self.pool.runtime();
        runtime.spawn(async move {
            pump(conn, command, stdout_pump, stderr_pump, stdin_pump, state).await;
        });

        Ok(RemoteChild {
            session_id,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }

    fn signal(&self, session_id: u64, signal: Signal) -> std::io::Result<()> {
        // Signal the recorded process group via the reserved command; the daemon
        // kills the whole group (`kill(-pgid, sig)`).
        let command = crate::protocol::signal_command(session_id, signal.code());
        let conn = self.pool.allocate()?;
        let runtime = self.pool.runtime();
        runtime.spawn(async move {
            if let Err(err) = run_ssh_command(conn, &command).await {
                log::warn!("cmd-client: signal session={session_id}: {err}");
            }
        });
        Ok(())
    }

    fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        let state = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&session_id)
            .cloned()?;
        let exit = state.exit.lock().unwrap_or_else(|poison| poison.into_inner());
        *exit
    }

    fn open_shell_pty<'a>(
        &self,
        cols: u32,
        rows: u32,
        cwd: Option<&'a str>,
        program: &'a str,
        args: &'a [String],
    ) -> ShellPtyFuture<'a> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let command = crate::pty::shell_command(program, args, cwd)?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let allocate_pool = pool.clone();
            // Connection allocation blocks (bounded retry budget), so it runs on
            // a blocking worker instead of stalling the runtime; the pty setup
            // itself is async and stays on the pool runtime.
            pool.runtime().spawn(async move {
                let result =
                    match tokio::task::spawn_blocking(move || allocate_pool.allocate()).await {
                        Ok(Ok(conn)) => {
                            crate::pty::open_shell_pty(conn, cols, rows, &command).await
                        }
                        Ok(Err(err)) => Err(err),
                        Err(join) => Err(std::io::Error::other(format!(
                            "shell pty allocate task: {join}"
                        ))),
                    };
                let _ = tx.send(result);
            });
            rx.await
                .map_err(|_| std::io::Error::other("shell pty setup task dropped"))?
        })
    }

    fn wait_exit_async(&self, session_id: u64) -> ExitFuture<'_> {
        Box::pin(async move {
            let state = self
                .sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "unknown session")
                })?;
            let mut exit_rx = state.tx.subscribe();
            loop {
                if let Some(exit) = *exit_rx.borrow() {
                    // Terminal state recorded: drop the map entry so a long-lived
                    // editor does not accumulate one SessionState per completed
                    // command. The Arc was cloned above and the watch keeps the
                    // value, so readers that already hold the Arc stay correct.
                    self.remove_session(session_id);
                    return Ok(exit);
                }
                if exit_rx.changed().await.is_err() {
                    // The sender only drops with its SessionState; if that ever
                    // happens without a terminal value, fall back to the recorded
                    // state (flattening Some(None) -> None, unset -> None).
                    let recorded = *state
                        .exit
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    return Ok(recorded.flatten());
                }
            }
        })
    }
}

impl SshCommandExecutor {
    /// Runs one short shell command synchronously on the pool and waits for its
    /// exit status. Used for guest-side housekeeping (mkdir + virtiofs mount,
    /// guest clock sync) from a non-command context without going through the
    /// global executor (which would recurse into `spawn`). Blocks the calling
    /// thread for up to the pool's allocate budget, so call it from a background
    /// thread, never from a tokio runtime or the GPUI main thread.
    pub fn run_shell(&self, command: &str) -> std::io::Result<()> {
        let conn = self.pool.allocate()?;
        self.pool.runtime().block_on(run_ssh_command(conn, command))
    }

    /// Drops a session entry once its terminal exit has been consumed by a
    /// waiter. Repeated removals are a no-op.
    fn remove_session(&self, session_id: u64) {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&session_id);
    }
}

/// Runs one short SSH command and waits for its exit status (0 = success).
pub async fn run_ssh_command(conn: SshSession, command: &str) -> std::io::Result<()> {
    let mut channel = conn
        .channel_open_session()
        .await
        .map_err(|err| std::io::Error::other(format!("open channel: {err}")))?;
    channel
        .exec(true, command)
        .await
        .map_err(|err| std::io::Error::other(format!("exec: {err}")))?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                if exit_status == 0 {
                    return Ok(());
                }
                return Err(std::io::Error::other(format!(
                    "remote command failed with status {exit_status}"
                )));
            }
            Some(_) => continue,
            None => return Err(std::io::Error::other("channel closed without exit status")),
        }
    }
}

/// The per-command tokio pump task: opens the channel, runs the command, and
/// relays Data/ExtendedData into the socketpairs until the channel closes.
async fn pump(
    conn: SshSession,
    command: String,
    stdout_pump: UnixStream,
    stderr_pump: UnixStream,
    stdin_pump: UnixStream,
    state: Arc<SessionState>,
) {
    let mut channel = match conn.channel_open_session().await {
        Ok(channel) => channel,
        Err(err) => {
            log::error!("cmd-client pump: channel_open_session: {err}");
            state.set_exit(None);
            return;
        }
    };
    if let Err(err) = channel.exec(true, command.as_bytes()).await {
        log::error!("cmd-client pump: exec: {err}");
        state.set_exit(None);
        return;
    }
    // tokio requires non-blocking sockets: set each socketpair end before
    // wrapping, otherwise from_std panics ("Registering a blocking socket").
    let stdout_pump = stdout_pump;
    if let Err(err) = stdout_pump.set_nonblocking(true) {
        log::error!("cmd-client pump: set stdout nonblocking: {err}");
        state.set_exit(None);
        return;
    }
    let mut stdout_w = match tokio::net::UnixStream::from_std(stdout_pump) {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("cmd-client pump: wrap stdout: {err}");
            state.set_exit(None);
            return;
        }
    };
    let stderr_pump = stderr_pump;
    if let Err(err) = stderr_pump.set_nonblocking(true) {
        log::error!("cmd-client pump: set stderr nonblocking: {err}");
        state.set_exit(None);
        return;
    }
    let mut stderr_w = match tokio::net::UnixStream::from_std(stderr_pump) {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("cmd-client pump: wrap stderr: {err}");
            state.set_exit(None);
            return;
        }
    };
    let stdin_pump = stdin_pump;
    if let Err(err) = stdin_pump.set_nonblocking(true) {
        log::error!("cmd-client pump: set stdin nonblocking: {err}");
        state.set_exit(None);
        return;
    }
    let mut stdin_r = match tokio::net::UnixStream::from_std(stdin_pump) {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("cmd-client pump: wrap stdin: {err}");
            state.set_exit(None);
            return;
        }
    };
    let mut stdin_writer = channel.make_writer();
    let mut buf = [0u8; IO_CHUNK_SIZE];
    let mut stdin_open = true;

    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if let Err(err) = stdout_w.write_all(&data).await {
                            log::warn!("cmd-client pump: write stdout failed: {err}");
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if let Err(err) = stderr_w.write_all(&data).await {
                            log::warn!("cmd-client pump: write stderr failed: {err}");
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        state.set_exit(Some(exit_status as i32));
                    }
                    Some(ChannelMsg::ExitSignal { .. }) => {
                        state.set_exit(None);
                    }
                    Some(ChannelMsg::Eof) => {
                        // The remote side has no more stdout/stderr data, but
                        // the exit-status message for a finished command is sent
                        // AFTER this EOF: the daemon EOFs on a closed child stdout
                        // pipe, then reports the exit status once the child is
                        // reaped. Breaking here made every quick command resolve
                        // with exit_code=None. Keep looping until the
                        // ExitStatus and Close arrive.
                    }
                    Some(ChannelMsg::Close) => {
                        break;
                    }
                    None => {
                        break;
                    }
                    Some(_other) => {}
                }
            }
            read = stdin_r.read(&mut buf), if stdin_open => {
                match read {
                    Ok(0) => {
                        // Send the channel EOF per the SSH standard: the caller
                        // (util) closed its stdin, so the daemon must learn that
                        // and enter its normal error handling instead of a
                        // long-lived LSP blocking forever waiting for input.
                        stdin_open = false;
                        let _ = stdin_writer.shutdown().await;
                    }
                    Ok(n) => {
                        if let Err(err) = stdin_writer.write_all(&buf[..n]).await {
                            log::warn!("cmd-client pump: write stdin: {err}");
                            stdin_open = false;
                        }
                    }
                    Err(err) => {
                        log::warn!("cmd-client pump: read stdin: {err}");
                        stdin_open = false;
                    }
                }
            }
        }
    }
    // Ensure a terminal state: if no ExitStatus/ExitSignal arrived (channel
    // dropped early), record None so wait_exit_async resolves.
    state.set_exit(None);
    // Close the caller's ends so downstream readers see EOF.
    let _ = stdout_w.shutdown().await;
    let _ = stderr_w.shutdown().await;
}
