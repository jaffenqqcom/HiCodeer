//! Business-side client: connects to the local cmd-agent daemon and spawns
//! remote processes on the VM.
//!
//! The client runs entirely inside the host process alongside the daemon
//! thread. Spawn confirmations and exit results are read from the process-wide
//! `SharedControl` tables the daemon fills from VM events, and signals are
//! delivered through the shared channel the daemon's VM writer consumes. The
//! client therefore creates no management connection and no background
//! threads. Each `spawn` opens a fresh unix-socket data connection (plus a
//! dedicated stderr connection when the stderr descriptor is piped) and waits
//! for the SpawnOk confirmation that lands in the shared table.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use cmd_agent_protocol::{
    ClientMessage, ExecSpec, FdMode, PROTOCOL_VERSION, RootMap, ServerMessage, Signal, frame,
};
use smol::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use smol::net::unix::UnixStream;

use crate::daemon::{FileSyncOp, FileSyncRequest, SharedControl, SignalRequest, SpawnRequest};

/// How long a spawn waits for its SpawnOk confirmation.
const SPAWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for the HelloOk the daemon replies on a fresh data connection;
/// bounds how long the handshake waits for a stalled daemon.
const SPAWN_HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// Defensive backstop for how long `spawn` waits for the daemon's handshake
/// reply. The handshake itself has timeouts (SPAWN_HELLO_TIMEOUT /
/// SPAWN_CONFIRM_TIMEOUT), so this only fires if the daemon's executor is
/// wedged or the request was never consumed.
const SPAWN_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `wait_exit` waits for an ExecResult.
const EXIT_RESULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Poll interval when waiting for a routed result.
const RESULT_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Read/write chunk size for streaming a file body during a file sync.
const FILE_SYNC_CHUNK_SIZE: u64 = 64 * 1024;

/// A connected daemon. Cheap to clone: the shared tables and channel are
/// process-wide.
#[derive(Clone)]
pub struct Client {
    /// Weak self-reference, upgraded by `spawn` so the handshake runs on the
    /// daemon's executor without borrowing the caller.
    self_arc: Weak<Client>,
    socket_path: String,
    root_map: Option<RootMap>,
    next_session_id: Arc<AtomicU64>,
    /// Process-wide tables and channel shared with the daemon.
    shared: Arc<SharedControl>,
}

/// A running remote process: raw byte streams wired to the child's stdio.
pub struct Session {
    pub session_id: u64,
    /// Writer for the child's stdin (the main connection).
    pub stdin: Box<dyn AsyncWrite + Unpin + Send>,
    /// Reader for the child's stdout (the main connection).
    pub stdout: Box<dyn AsyncRead + Unpin + Send>,
    /// Reader for the child's stderr, when the stderr mode is piped.
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
}

/// A stdin writer that half-closes the underlying unix socket on drop, so the
/// daemon's stdin relay observes EOF exactly when the caller finishes writing.
/// This mirrors std::process::ChildStdin, whose drop closes the pipe write end;
/// without it, interactive commands (git cat-file --batch-check, git blame
/// --contents) never observe stdin EOF and hang waiting for more input.
struct StdinWriter {
    inner: UnixStream,
}

impl Drop for StdinWriter {
    fn drop(&mut self) {
        let _ = self.inner.shutdown(smol::net::Shutdown::Write);
    }
}

impl AsyncWrite for StdinWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

impl cmd_agent_linker::RemoteCommandExecutor for Client {
    fn spawn(&self, spec: ExecSpec) -> io::Result<cmd_agent_linker::RemoteChild> {
        let session = Client::spawn(self, spec)?;
        Ok(cmd_agent_linker::RemoteChild {
            session_id: session.session_id,
            stdin: Some(session.stdin),
            stdout: Some(session.stdout),
            stderr: session.stderr,
        })
    }

    fn signal(&self, session_id: u64, signal: cmd_agent_protocol::Signal) -> io::Result<()> {
        Client::signal(self, session_id, signal)
    }

    fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        Client::try_exit(self, session_id)
    }

    fn wait_exit_async(&self, session_id: u64) -> cmd_agent_linker::ExitFuture<'_> {
        Box::pin(Client::wait_exit_async(self, session_id))
    }
}

impl Client {
    /// Records the daemon socket path and the process-wide shared state. The
    /// daemon thread is already running (started by launch-zed before the
    /// client connects), so no management connection is established and no
    /// background thread is spawned here: spawn confirmations and exit results
    /// are read from `shared`, and signals are delivered through `shared`.
    pub fn connect(
        socket_path: &str,
        root_map: Option<RootMap>,
        shared: Arc<SharedControl>,
    ) -> io::Result<Arc<Client>> {
        let client = Arc::new_cyclic(|weak| Client {
            self_arc: weak.clone(),
            socket_path: socket_path.to_string(),
            root_map,
            next_session_id: Arc::new(AtomicU64::new(1)),
            shared,
        });
        log::info!("client connected to daemon at {socket_path}");
        Ok(client)
    }

    /// Spawns a remote process. Blocking: the whole handshake (main
    /// connection, optional stderr connection, SpawnOk wait) completes here,
    /// so the returned `Session` is immediately usable.
    pub fn spawn(&self, spec: ExecSpec) -> io::Result<Session> {
        // The handshake runs on the daemon's smol executor, never on the
        // caller's thread: running `smol::block_on` on the GPUI main thread
        // (e.g. LSP startup in a foreground task) would nest an async-io
        // reactor and deadlock. The request is queued to the daemon's shared
        // spawn channel and the caller waits on a plain channel, which never
        // touches a reactor.
        let Some(this) = self.self_arc.upgrade() else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "cmd-agent client dropped before spawn",
            ));
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.shared
            .spawn_req_tx
            .try_send(SpawnRequest {
                client: this,
                spec,
                reply: tx,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "cmd-agent spawn channel closed")
            })?;
        rx.recv_timeout(SPAWN_REPLY_TIMEOUT)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "cmd-agent spawn reply timed out")
            })?
    }

    /// Runs the async spawn handshake to completion. Called by the daemon's
    /// executor from the shared spawn-request loop; never from a thread that
    /// may already be driving an async-io reactor (nested block_on deadlocks).
    pub async fn spawn_async(&self, spec: ExecSpec) -> io::Result<Session> {
        let session_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);
        log::info!(
            "client::spawn_async: session_id={session_id}, program={}, args={:?}, cwd={:?}",
            spec.source_program,
            spec.args,
            spec.cwd_path
        );

        // The stderr connection must be registered with the server before the
        // main Spawn arrives, so the child's fd 2 can be wired to it.
        let stderr = if spec.stderr_mode == FdMode::Piped {
            let stream = self.connect_stderr(session_id).await?;
            log::info!("client::spawn_async: session_id={session_id} stderr connection opened");
            Some(Box::new(stream) as Box<dyn AsyncRead + Unpin + Send>)
        } else {
            None
        };

        let mut main = UnixStream::connect(&self.socket_path).await?;
        frame::write_message(
            &mut main,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                root_map: self.root_map.clone(),
            },
        )
        .await
        .map_err(to_io_error)?;
        // The server replies HelloOk on this same connection. Consume it now so
        // its bytes are not still buffered on the socket when the server dup2s
        // the fd into the child's stdio; otherwise the HelloOk frame becomes a
        // prefix of the child's stdout and corrupts command output parsing. A
        // timeout keeps a stalled daemon from wedging the worker forever.
        let hello_ok = read_frame_with_timeout(&mut main, SPAWN_HELLO_TIMEOUT).await?;
        debug_assert!(matches!(hello_ok, ServerMessage::HelloOk { .. }));
        frame::write_message(
            &mut main,
            &ClientMessage::Spawn { session_id, spec },
        )
        .await
        .map_err(to_io_error)?;

        self.wait_spawn_ok(session_id).await?;
        log::info!("client::spawn_async: session_id={session_id} SpawnOk confirmed");

        // The main connection is full-duplex: write stdin on `main`, read
        // stdout on the clone. Both point at the same socket. The stdin half is
        // wrapped so dropping it half-closes the socket (stdin EOF to the
        // child); a bare drop would leave the write direction open forever and
        // commands reading their stdin would never observe EOF.
        let stdout = main.clone();
        Ok(Session {
            session_id,
            stdin: Box::new(StdinWriter { inner: main }),
            stdout: Box::new(stdout),
            stderr,
        })
    }

    /// Runs a file-sync transfer to completion on the daemon's executor. Opens
    /// a fresh business-side connection and streams each op to the VM; the
    /// daemon relays it byte-for-byte, so the VM sees the same frame/raw-stream
    /// sequence written here.
    pub async fn file_sync_async(&self, sync_id: u64, ops: Vec<FileSyncOp>) -> io::Result<()> {
        log::info!(
            "client::file_sync_async: sync_id={sync_id}, op_count={}",
            ops.len()
        );
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        frame::write_message(
            &mut stream,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                root_map: self.root_map.clone(),
            },
        )
        .await
        .map_err(to_io_error)?;
        // Consume HelloOk like spawn_async so its bytes do not linger on the
        // connection; the daemon switches this connection to a byte relay.
        let hello_ok = read_frame_with_timeout(&mut stream, SPAWN_HELLO_TIMEOUT).await?;
        debug_assert!(matches!(hello_ok, ServerMessage::HelloOk { .. }));
        frame::write_message(&mut stream, &ClientMessage::FileSyncStart { sync_id })
            .await
            .map_err(to_io_error)?;
        for op in ops {
            match op {
                FileSyncOp::WriteContent { device_path } => {
                    // Stat the device file, then stream its raw content. The
                    // frame is only the header; the body is `len` raw bytes.
                    let len = match std::fs::metadata(&device_path) {
                        Ok(meta) => meta.len(),
                        // The source file vanished after the engine queued it
                        // (a download tree being replaced in place). Skip it:
                        // failing the whole batch on one gone file would block
                        // every other op in the batch forever.
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {
                            log::info!(
                                "file sync: source vanished before stat, skipping {device_path}"
                            );
                            continue;
                        }
                        Err(err) => {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("file sync: stat {device_path}: {err}"),
                            ))
                        }
                    };
                    // [diag] record the pushed path+size for sync cross-checking.
                    log::info!(
                        "[diag] file_sync WriteContent(sync_id={sync_id}): {device_path} ({len} bytes)"
                    );
                    frame::write_message(
                        &mut stream,
                        &ClientMessage::FileBegin {
                            sync_id,
                            path: device_path.clone(),
                            len,
                        },
                    )
                    .await
                    .map_err(to_io_error)?;
                    // Stream the file body in chunks so large files (node
                    // runtime, LSP binaries) are not buffered whole; reads run
                    // on smol::unblock so the executor is never blocked.
                    let open_path = device_path.clone();
                    let file = match smol::unblock(move || std::fs::File::open(&open_path)).await {
                        Ok(file) => file,
                        // Same vanish-after-stat race as above: skip, do not
                        // fail the batch.
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {
                            log::info!(
                                "file sync: source vanished before open, skipping {device_path}"
                            );
                            continue;
                        }
                        Err(err) => {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("file sync: open {device_path}: {err}"),
                            ))
                        }
                    };
                    let file = Arc::new(Mutex::new(file));
                    let mut remaining = len;
                    while remaining > 0 {
                        let want = remaining.min(FILE_SYNC_CHUNK_SIZE) as usize;
                        let file = file.clone();
                        let chunk = smol::unblock(move || {
                            use std::io::Read as _;
                            let mut guard = file.lock().unwrap();
                            let mut buf = vec![0u8; want];
                            let n = guard.read(&mut buf).unwrap_or(0);
                            buf.truncate(n);
                            buf
                        })
                        .await;
                        if chunk.is_empty() {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                "file sync: content truncated",
                            ));
                        }
                        stream.write_all(&chunk).await.map_err(to_io_error)?;
                        remaining -= chunk.len() as u64;
                    }
                    // Content complete: rename `.ing` -> final name on the VM.
                    frame::write_message(
                        &mut stream,
                        &ClientMessage::FileRename {
                            sync_id,
                            path: device_path,
                        },
                    )
                    .await
                    .map_err(to_io_error)?;
                }
                FileSyncOp::Rename { device_path } => {
                    log::info!("[diag] file_sync Rename(sync_id={sync_id}): {device_path}");
                    frame::write_message(
                        &mut stream,
                        &ClientMessage::FileRename {
                            sync_id,
                            path: device_path,
                        },
                    )
                    .await
                    .map_err(to_io_error)?;
                }
                FileSyncOp::Delete { device_path } => {
                    log::info!("[diag] file_sync Delete(sync_id={sync_id}): {device_path}");
                    frame::write_message(
                        &mut stream,
                        &ClientMessage::FileDelete {
                            sync_id,
                            path: device_path,
                        },
                    )
                    .await
                    .map_err(to_io_error)?;
                }
                FileSyncOp::CreateDir { device_path } => {
                    log::info!("[diag] file_sync CreateDir(sync_id={sync_id}): {device_path}");
                    frame::write_message(
                        &mut stream,
                        &ClientMessage::FileCreateDir {
                            sync_id,
                            path: device_path,
                        },
                    )
                    .await
                    .map_err(to_io_error)?;
                }
            }
        }
        frame::write_message(&mut stream, &ClientMessage::FileSyncEnd { sync_id })
            .await
            .map_err(to_io_error)?;
        log::info!("client::file_sync_async: sync_id={sync_id} done");
        Ok(())
    }

    /// Opens a dedicated stderr connection for a session.
    async fn connect_stderr(&self, session_id: u64) -> io::Result<UnixStream> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        frame::write_message(
            &mut stream,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                root_map: self.root_map.clone(),
            },
        )
        .await
        .map_err(to_io_error)?;
        // Consume the server's HelloOk like in spawn_async; its bytes must not
        // stay on the socket that becomes the child's stderr fd. A timeout
        // keeps a stalled daemon from wedging the worker forever.
        let hello_ok = read_frame_with_timeout(&mut stream, SPAWN_HELLO_TIMEOUT).await?;
        debug_assert!(matches!(hello_ok, ServerMessage::HelloOk { .. }));
        frame::write_message(&mut stream, &ClientMessage::SpawnStderr { session_id })
            .await
            .map_err(to_io_error)?;
        Ok(stream)
    }

    async fn wait_spawn_ok(&self, session_id: u64) -> io::Result<()> {
        let deadline = Instant::now() + SPAWN_CONFIRM_TIMEOUT;
        loop {
            if let Some(result) = self.shared.spawn_oks.lock().unwrap().remove(&session_id) {
                log::info!("[diag] client wait_spawn_ok: session {session_id} result={result:?}");
                return result.map_err(|message| {
                    io::Error::new(io::ErrorKind::Other, message)
                });
            }
            if Instant::now() >= deadline {
                log::warn!("client::wait_spawn_ok: session_id={session_id} confirmation timed out");
                // Drop any late confirmation so the entry cannot leak.
                self.shared.spawn_oks.lock().unwrap().remove(&session_id);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("spawn {session_id} confirmation timed out"),
                ));
            }
            smol::Timer::after(RESULT_POLL_INTERVAL).await;
        }
    }

    /// Waits for the session's exit result (the child has exited).
    pub fn wait_exit(&self, session_id: u64) -> io::Result<Option<i32>> {
        smol::block_on(self.wait_exit_async(session_id))
    }

    /// Non-blocking query: `Some(exit_code)` once the result has arrived,
    /// `None` while the child is still running.
    pub fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        self.shared
            .exec_results
            .lock()
            .unwrap()
            .remove(&session_id)
            .map(|(exit_code, _)| exit_code)
    }

    /// Waits for the session's exit result (the child has exited).
    pub async fn wait_exit_async(&self, session_id: u64) -> io::Result<Option<i32>> {
        // Fast path: the result already arrived before this wait began.
        if let Some((exit_code, _)) = self.shared.exec_results.lock().unwrap().remove(&session_id)
        {
            log::info!("client::wait_exit_async: session_id={session_id} exit_code={exit_code:?}");
            return Ok(exit_code);
        }

        // Event-driven wait: register a waiter that the daemon fulfils the
        // moment ExecResult lands, instead of polling every RESULT_POLL_INTERVAL.
        // A capacity-1 channel stands in for a oneshot: the sender is stored in
        // exec_waiters and the receiver is awaited here.
        let (tx, rx) = smol::channel::bounded::<Option<i32>>(1);
        self.shared
            .exec_waiters
            .lock()
            .unwrap()
            .entry(session_id)
            .or_default()
            .push(tx);

        let outcome = smol::future::or(
            async { rx.recv().await.ok() },
            async {
                smol::Timer::after(EXIT_RESULT_TIMEOUT).await;
                None
            },
        )
        .await;
        match outcome {
            Some(exit_code) => {
                log::info!("client::wait_exit_async: session_id={session_id} exit_code={exit_code:?}");
                Ok(exit_code)
            }
            None => {
                log::warn!("client::wait_exit_async: session_id={session_id} exit result timed out");
                // Drop the registered waiter so it cannot leak if the result
                // never arrives.
                self.shared.exec_waiters.lock().unwrap().remove(&session_id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("exit result for session {session_id} timed out"),
                ))
            }
        }
    }

    /// Delivers a signal to a running session. The request is queued to the
    /// daemon's shared signal channel; the daemon writes it to the VM
    /// management connection and replies through the oneshot, so this call
    /// blocks only on a plain channel (never nesting a reactor block_on).
    pub fn signal(&self, session_id: u64, signal: Signal) -> io::Result<()> {
        log::info!("client::signal: session_id={session_id}, signal={signal:?}");
        let (tx, rx) = std::sync::mpsc::channel();
        // try_send on the unbounded shared channel: this is a synchronous,
        // non-blocking enqueue (the channel never fills), so `signal` cannot
        // nest a reactor block_on on the caller's thread.
        self.shared
            .signal_tx
            .try_send(SignalRequest {
                session_id,
                signal,
                reply: tx,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "cmd-agent signal channel closed")
            })?;
        rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "cmd-agent signal writer terminated")
        })?
    }

    /// Pushes a batch of file-sync operations to the VM. Blocking: the whole
    /// transfer completes here. Like `spawn`, the request is queued to the
    /// daemon's executor and the caller waits on a plain channel, never nesting
    /// a reactor block_on on the caller's thread.
    pub fn file_sync(&self, sync_id: u64, ops: Vec<FileSyncOp>) -> io::Result<()> {
        log::info!("client::file_sync: sync_id={sync_id}, op_count={}", ops.len());
        let Some(this) = self.self_arc.upgrade() else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "cmd-agent client dropped before file sync",
            ));
        };
        let (tx, rx) = std::sync::mpsc::channel();
        // try_send on the unbounded shared channel, like `signal`: a synchronous
        // enqueue that cannot nest a reactor block_on on the caller's thread.
        self.shared
            .file_sync_req_tx
            .try_send(FileSyncRequest {
                client: this,
                sync_id,
                ops,
                reply: tx,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "cmd-agent file sync channel closed")
            })?;
        rx.recv_timeout(SPAWN_REPLY_TIMEOUT).map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "cmd-agent file sync reply timed out")
        })?
    }
}

/// Reads one frame with a timeout, mapping frame errors to io::Error so a
/// stalled daemon cannot wedge the spawn handshake forever.
async fn read_frame_with_timeout<S, M>(stream: &mut S, timeout: Duration) -> io::Result<M>
where
    S: smol::io::AsyncRead + Unpin,
    M: serde::de::DeserializeOwned,
{
    smol::future::or(
        async { frame::read_message(stream).await.map_err(to_io_error) },
        async {
            smol::Timer::after(timeout).await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out after {}s", timeout.as_secs()),
            ))
        },
    )
    .await
}

fn to_io_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}
