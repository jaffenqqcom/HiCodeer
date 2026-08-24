//! cmd-agent server: a generic remote command executor.
//!
//! Listens for TCP connections from the cmd-agent daemon. A data connection
//! is handshaken with `Hello`/`Spawn`, then the connection socket is handed
//! to the child process as its stdio (`dup2`), so output data never passes
//! through this process. A management connection carries heartbeats, exit
//! results, and signals; it is also the liveness marker that ties the
//! server's lifetime to its client.

mod error;
mod install;
mod spawn;

use std::collections::HashMap;
use std::env;
use std::os::fd::AsRawFd as _;
use std::sync::Arc;
use std::time::Duration;

use cmd_agent_protocol::{
    ClientMessage, ExecSpec, FdMode, PROTOCOL_VERSION, RootMap, ServerMessage, Signal, frame,
};
use log::error;
use smol::lock::Mutex;
use smol::net::{TcpListener, TcpStream};

use crate::error::{Error, Result, ResultContext};

/// Default listen address when none is supplied.
const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:4040";

/// Idle timeout for the frame loop: a data connection that sends nothing for
/// this long is dropped, so a stalled or hostile peer cannot hold a session.
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the server waits for a heartbeat on the management connection
/// before it concludes the client is gone and shuts down.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the spawn handler waits for the dedicated stderr connection to
/// register before falling back to /dev/null.
const STDERR_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval while waiting for the stderr connection.
const STDERR_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long an orphaned stderr connection stays registered before it is
/// reaped: a `Spawn` that never arrives (crashed client) would otherwise leave
/// the socket in `pending_stderr` for the server's lifetime.
const STDERR_LEAK_TIMEOUT: Duration = Duration::from_secs(30);

/// A session lifecycle event forwarded to the management connection.
///
/// These travel over the management connection (never the data connection,
/// which is pure bytes once a child is spawned) so confirmation and exit
/// codes never race with child output.
enum SessionEvent {
    SpawnOk {
        session_id: u64,
    },
    SpawnError {
        session_id: u64,
        message: String,
    },
    ExecResult {
        session_id: u64,
        exit_code: Option<i32>,
        timed_out: bool,
    },
}

/// Shared server state handed to every connection handler.
struct State {
    /// Active children keyed by session id, so signals can address them.
    sessions: Mutex<HashMap<u64, Arc<Mutex<std::process::Child>>>>,
    /// Dedicated stderr connections awaiting their session's `Spawn`. The
    /// stream lives here so its socket fd stays valid until `run_spawn` takes
    /// it and hands it to the child as fd 2.
    pending_stderr: Mutex<HashMap<u64, smol::net::TcpStream>>,
    /// Session-event channel. The state keeps one sender alive so the channel
    /// never closes between sessions; data handlers clone this.
    results_tx: smol::channel::Sender<SessionEvent>,
}

/// Command-line arguments for the server.
struct Args {
    listen_addr: String,
}

fn parse_args() -> Args {
    let mut listen_addr = DEFAULT_LISTEN_ADDR.to_string();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                listen_addr = args
                    .next()
                    .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_string());
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cmd-agent-server [--listen ADDR]\n\n\
                     Listens for cmd-agent client connections and executes commands.\n\n\
                     Options:\n  \
                     --listen ADDR  listen address (default {DEFAULT_LISTEN_ADDR})"
                );
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown argument: {arg}");
                std::process::exit(2);
            }
        }
    }
    Args { listen_addr }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    log::info!("cmd-agent-server starting, listen={}", args.listen_addr);

    smol::block_on(async {
        let listener = TcpListener::bind(&args.listen_addr)
            .await
            .with_context(|| format!("failed to bind {}", args.listen_addr))?;
        log::info!("listening on {}", args.listen_addr);

        // The server lives exactly as long as its client. The management
        // connection, carrying heartbeats, marks the client as alive; when it
        // ends (heartbeat timeout or disconnect) the server shuts down so no
        // orphaned agent process piles up on the VM.
        let (results_tx, results_rx) = smol::channel::unbounded::<SessionEvent>();
        let state = Arc::new(State {
            sessions: Mutex::new(HashMap::new()),
            pending_stderr: Mutex::new(HashMap::new()),
            results_tx,
        });
        let (shutdown_tx, shutdown_rx) = smol::channel::unbounded::<()>();

        loop {
            let accepted = accept_or_shutdown(&listener, &shutdown_rx).await?;
            let Some((stream, peer)) = accepted else {
                log::info!("management connection ended, shutting down");
                return Ok(());
            };
            log::info!("connection accepted from {peer}");

            let state = state.clone();
            let results_rx = results_rx.clone();
            let shutdown_tx = shutdown_tx.clone();
            smol::spawn(async move {
                match handle_connection(stream, state, results_rx, shutdown_tx).await {
                    Ok(()) => log::info!("connection {peer} closed"),
                    Err(err) if is_peer_closed(&err) => {
                        log::info!("connection {peer} closed by peer");
                    }
                    Err(err) => error!("connection {peer} error: {err}"),
                }
            })
            .detach();
        }
    })
}

/// Accepts a connection, or returns `None` when a shutdown signal arrives.
async fn accept_or_shutdown(
    listener: &TcpListener,
    shutdown_rx: &smol::channel::Receiver<()>,
) -> Result<Option<(TcpStream, std::net::SocketAddr)>> {
    let accepted: std::io::Result<Option<(TcpStream, std::net::SocketAddr)>> = smol::future::or(
        async { listener.accept().await.map(Some) },
        async {
            shutdown_rx.recv().await.ok();
            Ok(None)
        },
    )
    .await;
    accepted.map_err(Error::from)
}

/// True when the error is an EOF from a peer that closed the connection.
fn is_peer_closed(err: &Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(source) = current {
        if let Some(io_err) = source.downcast_ref::<std::io::Error>() {
            if io_err.kind() == std::io::ErrorKind::UnexpectedEof {
                return true;
            }
        }
        current = source.source();
    }
    false
}

/// Reads one client message, dropping the connection if it goes idle.
async fn read_message_idle_timeout(stream: &mut TcpStream) -> Result<ClientMessage> {
    smol::future::or(
        async { frame::read_message(stream).await.map_err(Error::from) },
        async {
            smol::Timer::after(CONNECTION_IDLE_TIMEOUT).await;
            Err(Error::message("connection idle timeout"))
        },
    )
    .await
}

/// Per-connection message loop. Routes a connection to either the management
/// handler or a spawn handler based on the first instruction.
async fn handle_connection(
    mut stream: TcpStream,
    state: Arc<State>,
    results_rx: smol::channel::Receiver<SessionEvent>,
    shutdown_tx: smol::channel::Sender<()>,
) -> Result<()> {
    let mut root_map: Option<RootMap> = None;
    loop {
        let message: ClientMessage = read_message_idle_timeout(&mut stream).await?;
        match message {
            ClientMessage::Hello {
                version,
                root_map: incoming_map,
            } => {
                if version != PROTOCOL_VERSION {
                    frame::write_message(
                        &mut stream,
                        &ServerMessage::Error {
                            session_id: None,
                            message: format!(
                                "protocol version mismatch: client {version}, server {PROTOCOL_VERSION}"
                            ),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                root_map = incoming_map;
                log::info!(
                    "hello from client, root_map={:?}",
                    root_map
                        .as_ref()
                        .map(|map| format!("{} -> {}", map.ohos_root, map.vm_root))
                );
                frame::write_message(
                    &mut stream,
                    &ServerMessage::HelloOk {
                        server_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                )
                .await?;
            }
            ClientMessage::Manage => {
                return run_management(stream, state, results_rx, shutdown_tx).await;
            }
            ClientMessage::Spawn { session_id, spec } => {
                return run_spawn(stream, session_id, spec, root_map, state).await;
            }
            ClientMessage::SpawnStderr { session_id } => {
                return run_stderr_attach(stream, session_id, state).await;
            }
            ClientMessage::Heartbeat => {
                // Only meaningful on the management connection; ignore it on a
                // data connection.
                log::debug!("heartbeat received on a data connection");
            }
            ClientMessage::Signal { .. } => {
                log::debug!("signal received on a data connection");
            }
            ClientMessage::Query => {
                frame::write_message(
                    &mut stream,
                    &ServerMessage::Error {
                        session_id: None,
                        message: "query is not implemented".to_string(),
                    },
                )
                .await?;
            }
            ClientMessage::Shutdown => {
                log::info!("shutdown requested, closing connection");
                return Ok(());
            }
        }
    }
}

/// Management connection: reads heartbeats and signals, forwards exit results
/// from spawned sessions to the client, and triggers server shutdown when the
/// connection ends.
async fn run_management(
    mut stream: TcpStream,
    state: Arc<State>,
    results_rx: smol::channel::Receiver<SessionEvent>,
    shutdown_tx: smol::channel::Sender<()>,
) -> Result<()> {
    log::info!("management connection established");
    // Results are forwarded on a cloned socket; the main loop only reads
    // client messages, so the two directions never contend.
    let results_stream = stream.clone();
    let results_task = smol::spawn(async move {
        if let Err(err) = forward_results(results_stream, results_rx).await {
            log::warn!("management results forward ended: {err}");
        }
    });

    let outcome = heartbeat_loop(&mut stream, &state).await;
    results_task.cancel().await;
    let _ = shutdown_tx.send(()).await;
    match &outcome {
        Ok(()) => log::info!("management connection closed normally"),
        Err(err) => log::info!("management connection ended: {err}"),
    }
    outcome
}

/// Reads heartbeats on the management connection until one is missed or the
/// connection drops. Signals addressed to running sessions are dispatched.
async fn heartbeat_loop(stream: &mut TcpStream, state: &State) -> Result<()> {
    loop {
        let message = smol::future::or(
            async { frame::read_message(stream).await.map_err(Error::from) },
            async {
                smol::Timer::after(HEARTBEAT_TIMEOUT).await;
                Err(Error::message("heartbeat timeout"))
            },
        )
        .await?;
        match message {
            ClientMessage::Heartbeat => {}
            ClientMessage::Signal { session_id, signal } => {
                dispatch_signal(state, session_id, signal).await?;
            }
            other => log::debug!(
                "ignoring message on management connection: {}",
                kind_of(&other)
            ),
        }
    }
}

/// Forwards exit results from spawned sessions to the management connection.
async fn forward_results(
    mut stream: TcpStream,
    results_rx: smol::channel::Receiver<SessionEvent>,
) -> Result<()> {
    loop {
        match results_rx.recv().await {
            Ok(SessionEvent::SpawnOk { session_id }) => {
                log::info!("forwarding SpawnOk for session {session_id}");
                frame::write_message(&mut stream, &ServerMessage::SpawnOk { session_id }).await?;
            }
            Ok(SessionEvent::SpawnError {
                session_id,
                message,
            }) => {
                log::info!("forwarding spawn error for session {session_id}: {message}");
                frame::write_message(
                    &mut stream,
                    &ServerMessage::Error {
                        session_id: Some(session_id),
                        message,
                    },
                )
                .await?;
            }
            Ok(SessionEvent::ExecResult {
                session_id,
                exit_code,
                timed_out,
            }) => {
                frame::write_message(
                    &mut stream,
                    &ServerMessage::ExecResult {
                        session_id,
                        exit_code,
                        timed_out,
                    },
                )
                .await?;
            }
            Err(_) => {
                // All senders dropped; the state keeps one alive so this is
                // unreachable. Park so the heartbeat loop decides liveness.
                std::future::pending().await
            }
        }
    }
}

/// Sends a signal to a running session's process group.
async fn dispatch_signal(state: &State, session_id: u64, signal: Signal) -> Result<()> {
    let sessions = state.sessions.lock().await;
    let Some(child) = sessions.get(&session_id) else {
        log::warn!("signal for unknown session {session_id}");
        return Ok(());
    };
    let child = child.lock().await;
    let pid = child.id() as i32;
    if pid <= 1 {
        return Ok(());
    }
    let sig = match signal {
        Signal::SigKill => libc::SIGKILL,
        Signal::SigTerm => libc::SIGTERM,
        Signal::SigInterrupt => libc::SIGINT,
    };
    // SAFETY: `-pid` targets the process group whose leader is the child.
    unsafe {
        libc::kill(-pid, sig);
    }
    log::info!("sent signal {sig} to session {session_id} process group {pid}");
    Ok(())
}

/// Dedicated stderr connection: register the stream as the session's stderr
/// channel. The stream stays in `State::pending_stderr` so its socket fd
/// remains valid until `run_spawn` takes it for the child's fd 2.
async fn run_stderr_attach(
    stream: TcpStream,
    session_id: u64,
    state: Arc<State>,
) -> Result<()> {
    log::info!("stderr connection registered for session {session_id}");
    state.pending_stderr.lock().await.insert(session_id, stream);
    // Reap an orphaned registration: if the matching `Spawn` never arrives
    // (crashed client), the socket would otherwise linger in `pending_stderr`
    // for the server's lifetime. A normal spawn removes it within
    // `STDERR_WAIT_TIMEOUT`, so the long delay only catches true orphans.
    let state = state.clone();
    smol::spawn(async move {
        smol::Timer::after(STDERR_LEAK_TIMEOUT).await;
        if let Some(orphaned) = state.pending_stderr.lock().await.remove(&session_id) {
            log::warn!("stderr connection for session {session_id} orphaned, cleaned up");
            drop(orphaned);
        }
    })
    .detach();
    Ok(())
}

/// Data connection: spawn the child with the socket as its stdio, confirm with
/// SpawnOk, drop the socket so the client sees EOF exactly when the child
/// exits, then wait and report the exit result.
async fn run_spawn(
    mut stream: TcpStream,
    session_id: u64,
    spec: cmd_agent_protocol::ExecSpec,
    root_map: Option<RootMap>,
    state: Arc<State>,
) -> Result<()> {
    let socket_fd = stream.as_raw_fd();
    // Take any stderr connection registered for this session. The stream must
    // stay alive until spawn_direct dup2s its fd: mapping to as_raw_fd() and
    // dropping the stream inside the closure closes the fd, which surfaced as
    // a spurious EBADF under concurrent spawns.
    let mut stderr_stream = state.pending_stderr.lock().await.remove(&session_id);
    // The business side opens the stderr connection before the main spawn
    // connection, but the daemon forwards the two concurrently: when the
    // spawn arrives first, the stderr registration may not have landed yet.
    // The protocol guarantees a SpawnStderr for a Piped stderr, so wait
    // briefly rather than wiring the child's stderr to /dev/null (which
    // would leave the daemon's stderr relay without an EOF to observe).
    if stderr_stream.is_none() && spec.stderr_mode == FdMode::Piped {
        let deadline = std::time::Instant::now() + STDERR_WAIT_TIMEOUT;
        while stderr_stream.is_none() && std::time::Instant::now() < deadline {
            smol::Timer::after(STDERR_POLL_INTERVAL).await;
            stderr_stream = state.pending_stderr.lock().await.remove(&session_id);
        }
        if stderr_stream.is_none() {
            log::warn!(
                "stderr connection for session {session_id} did not arrive within {STDERR_WAIT_TIMEOUT:?}, using /dev/null"
            );
        }
    }
    let stderr_fd = stderr_stream.as_ref().map(|s| s.as_raw_fd());
    // [diag] confirm the stderr socket is wired to the child; a None here means
    // the child wrote stderr to /dev/null, so the client's stderr relay would
    // never see EOF from the child side.
    log::info!("[diag] run_spawn session {session_id}: stderr_fd={stderr_fd:?}");
    let child = match spawn::spawn_direct(
        socket_fd,
        stderr_fd,
        &spec,
        root_map.as_ref(),
        session_id,
    ) {
            Ok(child) => child,
            Err(err) => {
                error!("spawn session {session_id} failed: {err}");
                // Report the failure over the management connection, matching
                // how the successful path reports SpawnOk, so the client
                // learns the spawn failed immediately instead of timing out
                // waiting for a confirmation that never arrives. The data
                // connection must stay clean: it carries raw child bytes once
                // a child takes it over.
                if state
                    .results_tx
                    .send(SessionEvent::SpawnError {
                        session_id,
                        message: format!("{err}"),
                    })
                    .await
                    .is_err()
                {
                    log::warn!("session {session_id} no management connection for spawn error");
                }
                return Ok(());
            }
        };

    let shared = Arc::new(Mutex::new(child));
    state.sessions.lock().await.insert(session_id, shared.clone());

    // Confirm the spawn over the management connection. The data connection
    // never carries frames after the handshake, so confirmation cannot race
    // with child output.
    if state
        .results_tx
        .send(SessionEvent::SpawnOk { session_id })
        .await
        .is_err()
    {
        // No management connection to confirm the spawn; kill and clean up.
        log::warn!("session {session_id} no management connection for SpawnOk");
        let mut child = shared.lock().await;
        spawn::kill_process_group(&mut child).await?;
        state.sessions.lock().await.remove(&session_id);
        return Ok(());
    }
    log::info!("session {session_id} spawned, handing socket to child");

    // Drop the server's socket copy: the child is the only remaining writer,
    // so the client observes EOF exactly when the child exits.
    drop(stream);

    let (exit_code, timed_out) = spawn::wait_child_exit_shared(&shared, spec.timeout_ms).await?;
    let _ = state
        .results_tx
        .send(SessionEvent::ExecResult {
            session_id,
            exit_code,
            timed_out,
        })
        .await;
    state.sessions.lock().await.remove(&session_id);
    // [diag] confirm the stderr stream is still held at session end; its drop
    // (on return) is what closes the child's stderr socket so the daemon's
    // stderr relay observes EOF.
    log::info!(
        "[diag] run_spawn session {session_id}: ending, stderr_stream held={}",
        stderr_stream.is_some()
    );
    log::info!(
        "session {session_id} done, exit_code={exit_code:?}, timed_out={timed_out}"
    );
    // A `which <program>` that exits non-zero means the queried program is
    // missing, and the caller intends to use it. Queue a background install
    // (dnf on the OpenEuler VM) so a later `which` finds it. The install is
    // asynchronous, so this session still reports the miss to the caller.
    if exit_code.map_or(false, |code| code != 0) {
        if let Some(program) = which_program(&spec) {
            install::ensure_program_installed(&program);
        }
    }
    Ok(())
}

/// Human-readable kind of a client message, used for the not-implemented error.
fn kind_of(message: &ClientMessage) -> &'static str {
    match message {
        ClientMessage::Hello { .. } => "hello",
        ClientMessage::Manage => "manage",
        ClientMessage::Heartbeat => "heartbeat",
        ClientMessage::Spawn { .. } => "spawn",
        ClientMessage::SpawnStderr { .. } => "spawn_stderr",
        ClientMessage::Signal { .. } => "signal",
        ClientMessage::Query => "query",
        ClientMessage::Shutdown => "shutdown",
    }
}

/// Extracts the queried program from a `which <program>` spawn: either a
/// direct `which <program>` (binary == "which") or a shell one-liner such as
/// `bash -l -c "which rust-analyzer"`. Returns None for non-which commands so
/// an unrelated failing command never triggers an install.
fn which_program(spec: &ExecSpec) -> Option<String> {
    if spec.binary == "which" {
        return spec.args.first().cloned();
    }
    let shell_like = spec.binary == "bash"
        || spec.binary == "sh"
        || spec.binary.ends_with("/bash")
        || spec.binary.ends_with("/sh");
    if !shell_like {
        return None;
    }
    for arg in &spec.args {
        let trimmed = arg.trim();
        if let Some(rest) = trimmed.strip_prefix("which") {
            if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
                let program = rest.trim();
                if !program.is_empty() {
                    return Some(program.to_string());
                }
            }
        }
    }
    None
}
