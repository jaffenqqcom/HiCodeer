//! cmd-agent daemon: a local proxy between business code (zcoder) and the
//! remote cmd-agent server.
//!
//! The daemon listens on a unix socket. A business process opens one
//! management connection (heartbeats, exit results, signals) and one data
//! connection per spawn. Data connections are pure byte relays: the daemon
//! copies bytes between the unix socket and the VM connection without
//! inspecting them. The daemon also keeps the VM server alive (heartbeat,
//! reconnect, auto re-deploy) and exits when its parent business process
//! dies or the management connection closes.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Weak;
use std::time::Duration;

use cmd_agent_protocol::{
    ClientMessage, ExecSpec, PROTOCOL_VERSION, RootMap, ServerMessage, frame,
};
use futures_util::FutureExt;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::TcpStream;
use smol::net::unix::{UnixListener, UnixStream};

use crate::client::{Client, Session};
use crate::deploy::{self, SshConfig};
use crate::error::{Error, Result, ResultContext};

/// Interval at which the daemon heartbeats the VM management connection.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Timeout for connecting to the VM server.
const VM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for the VM handshake (expecting HelloOk / SpawnOk).
const VM_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Delay between VM management reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How long a data relay waits for the VM agent server to become ready
/// before attempting the spawn (the first spawns race server deployment).
const VM_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle timeout for the business-side frame loop.
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Copy chunk size for the byte relay.
const RELAY_CHUNK_SIZE: usize = 64 * 1024;
/// Number of pre-warmed VM data connections kept in the pool. Sized to cover
/// the typical concurrent command load (git panel scans plus a few LSPs);
/// a spawn takes a ready, handshaken connection instead of paying a TCP
/// connect + Hello handshake per command.
const POOL_SIZE: usize = 16;
/// Number of executor workers for the data-plane relays (VM <-> business
/// socket byte copies). Heavy transfer load is confined to these workers so it
/// never delays the single business worker's handshakes or VM management.
const RELAY_WORKERS: usize = 2;
/// Delay between accept attempts after a transient accept error (e.g. EMFILE),
/// so a burst of fd exhaustion does not spin the accept task hot.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Fallback wake interval while waiting for the VM-ready notification, so a
/// missed broadcast cannot hang a spawn; real readiness is event-driven.
const WAIT_NOTIFY_FALLBACK: Duration = Duration::from_millis(100);
/// Interval at which pooled connections are heartbeated, so the server's
/// data-connection idle timeout (CONNECTION_IDLE_TIMEOUT) never fires.
const POOL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Timeout for a single pooled-connection heartbeat write, so a stalled
/// server cannot hold the pool lock while the heartbeat loop is writing.
const POOL_HEARTBEAT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A spawn handshake request queued to the daemon's executor. The client
/// enqueues it with a reply channel; the daemon runs the async handshake on its
/// own smol executor and resolves the reply once the `Session` is ready.
pub struct SpawnRequest {
    /// Client handle that runs the handshake (`spawn_async`). Holding the `Arc`
    /// keeps the daemon socket path, root map and shared tables alive for the
    /// duration of the handshake.
    pub client: Arc<Client>,
    /// The command spec to spawn.
    pub spec: ExecSpec,
    /// The caller's reply channel, resolved once the handshake completes.
    pub reply: std::sync::mpsc::Sender<io::Result<Session>>,
}

/// One file-sync operation addressed by its device-side path; the daemon maps
/// it to the VM side and (for `WriteContent`) streams the file body over the
/// connection.
#[derive(Clone, Debug)]
pub enum FileSyncOp {
    /// Stream `device_path`'s content to the VM as `path.ing` then rename it
    /// into place (atomic on the VM).
    WriteContent { device_path: String },
    /// Mirror a device-side rename: `.ing` -> final name on the VM.
    Rename { device_path: String },
    /// Mirror a device-side delete (file or directory, recursive).
    Delete { device_path: String },
    /// Mirror a device-side directory creation (parents included).
    CreateDir { device_path: String },
}

/// A file-sync request queued to the daemon's executor. The sync engine
/// enqueues it with a reply channel; the daemon relays the whole transfer to
/// the VM and resolves the reply once the session completes.
pub struct FileSyncRequest {
    /// Client handle that opens the business-side connection and streams the
    /// transfer (`file_sync_async`).
    pub client: Arc<Client>,
    /// Session id for this batch, surfaced in logs and the VM's session state.
    pub sync_id: u64,
    /// Operations to perform, in order.
    pub ops: Vec<FileSyncOp>,
    /// The caller's reply channel, resolved once the transfer completes.
    pub reply: std::sync::mpsc::Sender<io::Result<()>>,
}

/// Process-internal shared state between the daemon thread and the business
/// client. Because the daemon runs as a thread inside the host process, spawn
/// results and signals travel through shared tables and channels instead of a
/// management socket, so the client needs no reader/writer threads.
#[derive(Debug)]
pub struct SharedControl {
    /// Spawn confirmations, filled by the daemon's VM events task, consumed by
    /// the client's `wait_spawn_ok`.
    pub spawn_oks: std::sync::Mutex<HashMap<u64, std::result::Result<(), String>>>,
    /// Exit results, filled by the daemon, consumed by the client's
    /// `try_exit` / `wait_exit_async`.
    pub exec_results: std::sync::Mutex<HashMap<u64, (Option<i32>, bool)>>,
    /// Waiters notified the moment an ExecResult lands, registered by the
    /// client's `wait_exit_async`, fulfilled by the daemon's events task.
    pub exec_waiters: std::sync::Mutex<HashMap<u64, Vec<smol::channel::Sender<Option<i32>>>>>,
    /// Signal requests from the client; the daemon's VM writer consumes them.
    pub signal_tx: smol::channel::Sender<SignalRequest>,
    /// Receiver half of the signal channel.
    pub signal_rx: smol::channel::Receiver<SignalRequest>,
    /// Spawn handshake requests from the client; the daemon's executor runs the
    /// async handshake in place of a dedicated worker pool.
    pub spawn_req_tx: smol::channel::Sender<SpawnRequest>,
    /// Receiver half of the spawn-request channel.
    pub spawn_req_rx: smol::channel::Receiver<SpawnRequest>,
    /// File-sync requests from the sync engine; the daemon's executor relays
    /// each batch to the VM over a dedicated connection.
    pub file_sync_req_tx: smol::channel::Sender<FileSyncRequest>,
    /// Receiver half of the file-sync channel.
    pub file_sync_req_rx: smol::channel::Receiver<FileSyncRequest>,
}

impl SharedControl {
    pub fn new() -> Arc<Self> {
        let (signal_tx, signal_rx) = smol::channel::unbounded::<SignalRequest>();
        let (spawn_req_tx, spawn_req_rx) = smol::channel::unbounded::<SpawnRequest>();
        let (file_sync_req_tx, file_sync_req_rx) = smol::channel::unbounded::<FileSyncRequest>();
        Arc::new(Self {
            spawn_oks: std::sync::Mutex::new(HashMap::new()),
            exec_results: std::sync::Mutex::new(HashMap::new()),
            exec_waiters: std::sync::Mutex::new(HashMap::new()),
            signal_tx,
            signal_rx,
            spawn_req_tx,
            spawn_req_rx,
            file_sync_req_tx,
            file_sync_req_rx,
        })
    }
}

/// A signal-delivery request queued to the daemon's VM management writer.
pub struct SignalRequest {
    pub session_id: u64,
    pub signal: cmd_agent_protocol::Signal,
    /// The caller's reply channel, resolved once the write to the VM completes.
    pub reply: std::sync::mpsc::Sender<io::Result<()>>,
}

/// Command-line arguments for the daemon.
#[derive(Debug, Clone)]
pub struct Args {
    pub unix_socket: PathBuf,
    pub vm_addr: String,
    pub ssh: Option<SshConfig>,
    pub server_binary: Option<PathBuf>,
    pub agent_port: u16,
    /// Process-internal shared state handed to the daemon; the business client
    /// holds another clone of the same `SharedControl`.
    pub shared: Arc<SharedControl>,
    /// Device-side sandbox base path (from the OHOS app context). The sync
    /// engine mirrors `{base}/zcoder/<subdir>` onto the VM; when None, the sync
    /// engine is not started.
    pub sandbox_base: Option<String>,
}

const DEFAULT_UNIX_SOCKET: &str = "/tmp/cmd-agent.sock";
const DEFAULT_VM_ADDR: &str = "127.0.0.1:4040";
const DEFAULT_AGENT_PORT: u16 = 4040;

/// Parses daemon arguments from a token iterator. Shared by the executable
/// entry (`main.rs`) and the native child-process entry
/// (`CmdAgentDaemonMain`), so both accept the same `--flag value` syntax.
pub fn parse_args_from(
    args: &mut impl Iterator<Item = String>,
) -> std::result::Result<Args, String> {
    let mut unix_socket = PathBuf::from(DEFAULT_UNIX_SOCKET);
    let mut vm_addr = DEFAULT_VM_ADDR.to_string();
    let mut ssh_host: Option<String> = None;
    let mut ssh_port: u16 = 22;
    let mut ssh_user: Option<String> = None;
    let mut ssh_pass: Option<String> = None;
    let mut remote_dir = "/home/user/cmd-agent".to_string();
    let mut server_binary: Option<PathBuf> = None;
    let mut agent_port: u16 = DEFAULT_AGENT_PORT;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--unix-socket" => {
                unix_socket = PathBuf::from(args.next().ok_or("--unix-socket requires a value")?);
            }
            "--vm-addr" => {
                vm_addr = args.next().ok_or("--vm-addr requires a value")?;
            }
            "--ssh-host" => {
                ssh_host = Some(args.next().ok_or("--ssh-host requires a value")?);
            }
            "--ssh-port" => {
                ssh_port = parse_u16(args, "--ssh-port")?;
            }
            "--ssh-user" => {
                ssh_user = Some(args.next().ok_or("--ssh-user requires a value")?);
            }
            "--ssh-pass" => {
                ssh_pass = Some(args.next().ok_or("--ssh-pass requires a value")?);
            }
            "--remote-dir" => {
                remote_dir = args.next().ok_or("--remote-dir requires a value")?;
            }
            "--server-binary" => {
                server_binary = Some(PathBuf::from(
                    args.next().ok_or("--server-binary requires a value")?,
                ));
            }
            "--agent-port" => {
                agent_port = parse_u16(args, "--agent-port")?;
            }
            other => {
                return Err(format!("unknown argument or missing command: {other}"));
            }
        }
    }

    let ssh = match (ssh_host, ssh_user, ssh_pass) {
        (Some(host), Some(user), Some(password)) => Some(SshConfig {
            host,
            port: ssh_port,
            user,
            password,
            remote_dir,
        }),
        _ => None,
    };

    Ok(Args {
        unix_socket,
        vm_addr,
        ssh,
        server_binary,
        agent_port,
        // Standalone daemon process: the shared state is created here and no
        // in-process client is attached, so the tables simply go unused. No
        // sandbox base either: a standalone daemon has no device context.
        shared: SharedControl::new(),
        sandbox_base: None,
    })
}

/// Consumes the value of a `--flag` argument as a `u16`.
fn parse_u16(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> std::result::Result<u16, String> {
    let value = args.next().ok_or(format!("{flag} requires a value"))?;
    value
        .parse()
        .map_err(|err| format!("invalid {flag} value {value}: {err}"))
}

/// Pre-warmed VM data connections. Each is fully handshaken (Hello/HelloOk)
/// and idles in the queue until a spawn takes it; once taken the connection is
/// consumed by the child's stdio (dup2) and never returned. A background
/// maintain task refills the pool and heartbeats idle connections so the
/// server's data-connection idle timeout never closes them.
pub struct VmConnectionPool {
    /// Weak self-reference so `take` can hand the pool to a background rebuild
    /// task after consuming a connection.
    self_arc: Weak<VmConnectionPool>,
    /// The shared executor the rebuild task runs on.
    executor: Arc<smol::Executor<'static>>,
    idle: smol::lock::Mutex<VecDeque<TcpStream>>,
    addr: String,
    root_map: smol::lock::Mutex<Option<RootMap>>,
    /// Bumped on every state change that invalidates in-flight handshakes
    /// (root-map change or VM reconnect). A handshake that started before the
    /// bump is checked against this before its connection is pushed back, so a
    /// stale connection is never handed to a spawn.
    generation: AtomicU64,
}

impl VmConnectionPool {
    fn new(addr: String, executor: Arc<smol::Executor<'static>>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            self_arc: weak.clone(),
            executor,
            idle: smol::lock::Mutex::new(VecDeque::new()),
            addr,
            root_map: smol::lock::Mutex::new(None),
            generation: AtomicU64::new(0),
        })
    }

    /// Records the root map the business side negotiates in its Hello. When it
    /// actually changes, pre-warmed connections were handshaken with the old
    /// map and cannot remap paths, so they are dropped for a rebuild.
    async fn set_root_map(&self, root_map: Option<RootMap>) {
        let mut current = self.root_map.lock().await;
        if *current != root_map {
            *current = root_map;
            self.generation.fetch_add(1, Ordering::AcqRel);
            drop(current);
            self.idle.lock().await.clear();
            log::info!("pool: root map changed, dropped pre-warmed connections");
        }
    }

    /// Drops all pooled connections. Called when the VM management connection
    /// is re-established: connections handshaken against a previous server
    /// incarnation are dead and must not be handed to spawns.
    async fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let mut idle = self.idle.lock().await;
        let count = idle.len();
        idle.clear();
        log::info!("pool: invalidated {count} connections after vm reconnect");
    }

    /// Takes a ready connection; if the pool is empty, falls back to a fresh
    /// handshake so a burst of commands never blocks on pre-warming. Either
    /// way one replacement connection is rebuilt immediately, so the pool does
    /// not sit empty until the next heartbeat tick.
    async fn take(&self) -> Result<TcpStream> {
        let stream = if let Some(stream) = self.idle.lock().await.pop_front() {
            stream
        } else {
            let root_map = self.root_map.lock().await.clone();
            let generation = self.generation.load(Ordering::Acquire);
            let stream = connect_vm_handshake(&self.addr, root_map.as_ref())
                .await
                .with_context(|| "vm handshake for pooled connection".to_string())?;
            // A root-map change or VM reconnect during the handshake means this
            // connection was negotiated against stale state; drop it rather than
            // hand a wrongly-mapped or dead connection to the caller.
            if self.generation.load(Ordering::Acquire) != generation {
                log::warn!("pool: state changed during handshake, dropping connection");
                return Err(Error::message("vm state changed during connection handshake"));
            }
            stream
        };
        self.rebuild_one();
        Ok(stream)
    }

    /// Hands one freshly handshaken connection back to the pool in the
    /// background, so a consumed slot is replaced without blocking `take`.
    fn rebuild_one(&self) {
        let Some(this) = self.self_arc.upgrade() else {
            return;
        };
        let executor = self.executor.clone();
        executor.spawn(async move {
            let root_map = this.root_map.lock().await.clone();
            let generation = this.generation.load(Ordering::Acquire);
            match connect_vm_handshake(&this.addr, root_map.as_ref()).await {
                Ok(stream) => {
                    // A root-map change or VM reconnect happened while we were
                    // handshaking; this connection is stale, so drop it instead
                    // of pushing a dead or wrongly-mapped connection.
                    if this.generation.load(Ordering::Acquire) != generation {
                        log::info!("pool: dropping connection handshaken against stale state");
                        return;
                    }
                    let mut idle = this.idle.lock().await;
                    if idle.len() < POOL_SIZE {
                        idle.push_back(stream);
                        log::info!("pool: rebuilt connection (idle={})", idle.len());
                    }
                    // The pool is already full; drop the extra connection.
                }
                Err(err) => log::warn!("pool: rebuild failed: {err}"),
            }
        })
        .detach();
    }

    /// Tops the pool up to `POOL_SIZE` and heartbeats idle connections so they
    /// survive the server idle timeout. `take` already rebuilds consumed slots
    /// promptly; this loop is the startup pre-warm plus a slow refill fallback
    /// (e.g. after the VM was briefly unreachable).
    async fn maintain(&self) {
        loop {
            self.refill_to_size().await;
            // Heartbeat each idle connection without holding the pool lock
            // while writing: drain the pool into a local batch, write each
            // heartbeat bounded by a short timeout (so a stalled server cannot
            // wedge `take`), then push the survivors back. Draining keeps a
            // Spawn write from racing a heartbeat on the same socket.
            let batch: Vec<TcpStream> = self.idle.lock().await.drain(..).collect();
            let mut alive = Vec::with_capacity(batch.len());
            let mut dropped = 0usize;
            for mut stream in batch {
                let ok = smol::future::or(
                    async {
                        frame::write_message(&mut stream, &ClientMessage::Heartbeat)
                            .await
                            .is_ok()
                    },
                    async {
                        smol::Timer::after(POOL_HEARTBEAT_WRITE_TIMEOUT).await;
                        false
                    },
                )
                .await;
                if ok {
                    alive.push(stream);
                } else {
                    dropped += 1;
                }
            }
            let mut idle = self.idle.lock().await;
            for stream in alive {
                if idle.len() < POOL_SIZE {
                    idle.push_back(stream);
                }
                // The pool is already full (a rebuild topped it up while we
                // heartbeated); drop the extra connection.
            }
            drop(idle);
            if dropped > 0 {
                log::warn!("pool: dropped {dropped} connections that failed to heartbeat");
                self.refill_to_size().await;
            }
            smol::Timer::after(POOL_HEARTBEAT_INTERVAL).await;
        }
    }

    /// Handshakes connections until the pool holds `POOL_SIZE` of them.
    async fn refill_to_size(&self) {
        let shortage = {
            let idle = self.idle.lock().await;
            POOL_SIZE.saturating_sub(idle.len())
        };
        for _ in 0..shortage {
            let root_map = self.root_map.lock().await.clone();
            let generation = self.generation.load(Ordering::Acquire);
            match connect_vm_handshake(&self.addr, root_map.as_ref()).await {
                Ok(stream) => {
                    // Drop a connection handshaken against stale state (a root
                    // map change or VM reconnect happened mid-handshake).
                    if self.generation.load(Ordering::Acquire) != generation {
                        log::info!("pool: dropping connection handshaken against stale state");
                        continue;
                    }
                    let mut idle = self.idle.lock().await;
                    if idle.len() < POOL_SIZE {
                        idle.push_back(stream);
                        log::info!("pool: pre-warmed connection (idle={})", idle.len());
                    }
                    // The pool is already full (concurrent rebuilds topped it
                    // up); drop the extra connection.
                }
                Err(err) => {
                    log::warn!("pool: pre-warm failed, retrying later: {err}");
                    break;
                }
            }
        }
    }
}

/// Shared daemon context handed to connection handlers.
#[derive(Clone)]
pub struct Ctx {
    vm_addr: String,
    ssh: Option<SshConfig>,
    server_binary: Option<PathBuf>,
    agent_port: u16,
    /// Pre-warmed VM data connections used by spawn relays.
    pool: Arc<VmConnectionPool>,
    /// Internal channel to the VM management task: signals from business
    /// connections are sent here and written to the VM.
    control_tx: smol::channel::Sender<ClientMessage>,
    /// The VM management task emits server messages (exit results, errors)
    /// here for forwarding to the business management connection.
    events_rx: smol::channel::Receiver<ServerMessage>,
    /// Spawn confirmations routed by session id. The VM management task
    /// records SpawnOk/Error here; data relays poll it so confirmation never
    /// races with child output on the data connection.
    spawn_oks: Arc<smol::lock::Mutex<HashMap<u64, std::result::Result<(), String>>>>,
    /// Set once the VM management connection is established, i.e. the agent
    /// server on the VM is reachable. Data relays wait on this flag so the
    /// first spawns do not race the daemon's server deployment.
    vm_ready: Arc<AtomicBool>,
    /// Broadcasts the moment `vm_ready` becomes true, so `proxy_data` /
    /// `proxy_stderr` wait on the notification instead of polling the flag on
    /// the business worker.
    vm_ready_notify: Arc<tokio::sync::Notify>,
    /// The current business management connection's write half. A new
    /// management connection shuts down the previous one so the single
    /// `events_rx` receiver is never contested by two readers, which would
    /// split SpawnOk/ExecResult routing across stale connections.
    active_management: Arc<smol::lock::Mutex<Option<UnixStream>>>,
    /// Process-internal shared tables and channels shared with the business
    /// client; the daemon fills them from VM events and consumes client
    /// signals from them.
    shared: Arc<SharedControl>,
    /// Executor for the business plane (accept loop, spawn handshakes, VM
    /// management); driven by a single worker so control messages are never
    /// delayed by heavy transfer load.
    executor: Arc<smol::Executor<'static>>,
    /// Executor for the data-plane relays (VM <-> business socket byte
    /// copies); driven by `RELAY_WORKERS` workers that share the transfer load.
    relay_executor: Arc<smol::Executor<'static>>,
}

/// Spawns the daemon's executor workers inside the calling process.
///
/// There is no dedicated daemon thread and no independent async-io reactor
/// thread: a shared `smol::Executor` hosts every task (accept loop, VM
/// management, spawn handshakes, relays) and is driven by `WORKER_COUNT`
/// worker threads that alternate ready work via async-executor work stealing.
/// A worker panic is caught and logged so it cannot take down the host process
/// (zcoder); the workers live as long as the host process does, at which point
/// the VM server exits (management-connection heartbeat timeout).
pub fn spawn_daemon(args: Args) -> Result<()> {
    log::info!(
        "cmd-agent daemon starting, unix_socket={}, vm_addr={}",
        args.unix_socket.display(),
        args.vm_addr
    );

    // Bind the unix socket once, on the caller's thread; a stale socket file
    // is removed first so a crash-restart does not fail to bind.
    let _ = std::fs::remove_file(&args.unix_socket);
    let listener = Arc::new(
        UnixListener::bind(&args.unix_socket)
            .with_context(|| format!("binding unix socket {}", args.unix_socket.display()))?,
    );
    log::info!("daemon listening on {}", args.unix_socket.display());

    // Two executors: the business plane (accept, spawn handshakes, VM
    // management) and the data plane (VM <-> socket byte relays), so heavy
    // transfer load never delays control messages.
    let business = Arc::new(smol::Executor::new());
    let relay = Arc::new(smol::Executor::new());

    let (control_tx, control_rx) = smol::channel::unbounded::<ClientMessage>();
    let (events_tx, events_rx) = smol::channel::unbounded::<ServerMessage>();
    let pool = VmConnectionPool::new(args.vm_addr.clone(), relay.clone());
    let ctx = Ctx {
        vm_addr: args.vm_addr.clone(),
        ssh: args.ssh.clone(),
        server_binary: args.server_binary.clone(),
        agent_port: args.agent_port,
        pool: pool.clone(),
        control_tx,
        events_rx,
        spawn_oks: Arc::new(smol::lock::Mutex::new(HashMap::new())),
        vm_ready: Arc::new(AtomicBool::new(false)),
        vm_ready_notify: Arc::new(tokio::sync::Notify::new()),
        active_management: Arc::new(smol::lock::Mutex::new(None)),
        shared: args.shared.clone(),
        executor: business.clone(),
        relay_executor: relay.clone(),
    };

    // Accept business connections. This loop is itself a task on the shared
    // executor (not a dedicated accept thread): it yields on `accept` and any
    // worker drives it. The daemon lives as long as the host process does: no
    // parent watchdog, no PDEATHSIG, no shutdown channel.
    {
        let ex = business.clone();
        let listener = listener.clone();
        let ctx = ctx.clone();
        let ex_for_task = business.clone();
        ex.spawn(async move {
            loop {
                // A transient accept error (e.g. EMFILE from an fd burst) must
                // not take down the accept loop: log and retry after a pause.
                let (stream, peer) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(err) => {
                        log::error!("accept failed: {err}");
                        smol::Timer::after(ACCEPT_RETRY_DELAY).await;
                        continue;
                    }
                };
                log::info!("business connection accepted from {peer:?}");
                let ctx = ctx.clone();
                let ex = ex_for_task.clone();
                ex.spawn(async move {
                    if let Err(err) = handle_conn(stream, ctx).await {
                        log::warn!("business connection error: {err}");
                    }
                })
                .detach();
            }
        })
        .detach();
    }

    // VM management connection: heartbeat, forward signals, relay exit
    // results, reconnect and redeploy on failure.
    let vm_ctx = ctx.clone();
    business
        .spawn(vm_manager(vm_ctx, control_rx, events_tx))
        .detach();

    // Pre-warm the VM connection pool once the server is reachable, then keep
    // it filled and heartbeated so spawns skip connect + handshake.
    let pool_ctx = ctx.clone();
    let vm_ready_notify = ctx.vm_ready_notify.clone();
    business
        .spawn(async move {
            while !pool_ctx.vm_ready.load(Ordering::Acquire) {
                let notified = vm_ready_notify.notified();
                let _ = smol::future::or(
                    async { notified.await },
                    async {
                        smol::Timer::after(WAIT_NOTIFY_FALLBACK).await;
                    },
                )
                .await;
            }
            pool_ctx.pool.maintain().await;
        })
        .detach();

    // Run spawn handshakes on this executor, replacing the client-side worker
    // pool: each handshake is pure async (connect + HelloOk + Spawn + SpawnOk
    // wait). Requests arrive over an event-driven channel, never a poll.
    {
        let spawn_ctx = ctx.clone();
        let ex = business.clone();
        let ex_for_task = business.clone();
        ex.spawn(async move {
            while let Ok(request) = spawn_ctx.shared.spawn_req_rx.recv().await {
                let client = request.client;
                let spec = request.spec;
                let reply = request.reply;
                let ex = ex_for_task.clone();
                ex.spawn(async move {
                    // A panic inside the handshake must not take down the
                    // workers: an uncaught panic in a detached task propagates
                    // into the driving `block_on` and kills the worker. Catch
                    // it and resolve the reply with an error so the caller's
                    // `recv` never hangs.
                    let result = std::panic::AssertUnwindSafe(client.spawn_async(spec))
                        .catch_unwind()
                        .await;
                    let reply_result: io::Result<Session> = match result {
                        Ok(Ok(session)) => Ok(session),
                        Ok(Err(err)) => Err(err),
                        Err(_) => Err(io::Error::new(
                            io::ErrorKind::Other,
                            "cmd-agent spawn handshake panicked",
                        )),
                    };
                    let _ = reply.send(reply_result);
                })
                .detach();
            }
        })
        .detach();
    }

    // Run file-sync transfers on this executor, mirroring the spawn handshake
    // loop: each batch opens a fresh business-side connection and streams its
    // ops to the VM. A panic inside the transfer must not take down the
    // workers, so it is caught and surfaced through the reply channel.
    {
        let spawn_ctx = ctx.clone();
        let ex = business.clone();
        let ex_for_task = business.clone();
        ex.spawn(async move {
            while let Ok(request) = spawn_ctx.shared.file_sync_req_rx.recv().await {
                let client = request.client;
                let sync_id = request.sync_id;
                let ops = request.ops;
                let reply = request.reply;
                let ex = ex_for_task.clone();
                ex.spawn(async move {
                    let result = std::panic::AssertUnwindSafe(client.file_sync_async(sync_id, ops))
                        .catch_unwind()
                        .await;
                    let reply_result: io::Result<()> = match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(err)) => Err(err),
                        Err(_) => Err(io::Error::new(
                            io::ErrorKind::Other,
                            "cmd-agent file sync panicked",
                        )),
                    };
                    let _ = reply.send(reply_result);
                })
                .detach();
            }
        })
        .detach();
    }

    // Single business worker: accepts connections, runs spawn handshakes and
    // VM management. Control messages are never delayed by transfer load.
    {
        let ex = business.clone();
        std::thread::Builder::new()
            .name("cmd-agent-bus".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    async_io::block_on(ex.run(std::future::pending::<()>()))
                }));
                match outcome {
                    Ok(()) => log::info!("cmd-agent business worker exited cleanly"),
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        log::error!("cmd-agent business worker panicked: {message}");
                    }
                }
            })
            .map_err(|err| Error::message(format!("failed to spawn business worker: {err}")))?;
    }

    // Relay workers share the data-plane byte copies. They all drive the same
    // relay executor, whose work stealing hands each ready relay to whichever
    // worker is idle, so a busy transfer never strands another worker idle.
    for i in 0..RELAY_WORKERS {
        let ex = relay.clone();
        std::thread::Builder::new()
            .name(format!("cmd-agent-rel{i}"))
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    async_io::block_on(ex.run(std::future::pending::<()>()))
                }));
                match outcome {
                    Ok(()) => log::info!("cmd-agent relay worker {i} exited cleanly"),
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        log::error!("cmd-agent relay worker {i} panicked: {message}");
                    }
                }
            })
            .map_err(|err| Error::message(format!("failed to spawn relay worker {i}: {err}")))?;
    }
    log::info!("cmd-agent daemon started with 1 business + {RELAY_WORKERS} relay workers");

    // Start the device-sandbox -> VM mirror sync engine when the host provided
    // a sandbox base path. The engine runs on its own thread and pushes
    // download changes to the VM over the daemon's file-sync channel. The
    // roots are derived here from the base path (never via paths::data_dir), so
    // the paths crate stays uninitialized until Zed's start_zed_main calls
    // set_custom_data_dir.
    if let Some(sandbox_base) = args.sandbox_base.as_deref() {
        let sync_roots = sync_roots_from_base(sandbox_base);
        if !sync_roots.is_empty() {
            let socket_path = args.unix_socket.to_string_lossy().into_owned();
            crate::sync_engine::spawn_sync_engine(socket_path, args.shared.clone(), sync_roots);
        }
    }
    Ok(())
}

/// Device-side download directories mirrored onto the VM. Only language-server
/// binaries (LSPs) are ever spawned on the VM by zcoder, so only the
/// `{sandbox_base}/zcoder/languages` directory is mirrored. Other downloads
/// (extensions run locally as wasm, node/copilot/prettier etc. are not used on
/// the VM) are intentionally excluded to avoid useless large transfers.
///
/// The sync engine pushes changes to `/home/user/cmd-agent/zcoder/languages/...`
/// on the VM: Rule A maps `<DEVICE_APP_FILES_ROOT>` (= `{sandbox_base}`, the
/// app files dir) to `<VM_AGENT_ROOT>`, so the remaining `zcoder/...` suffix
/// matches the spawn-side `SYNC_DIR_RELATIVES` prefix verbatim. Both sides must
/// stay in sync, or VM-side chmod/wait/delete-guard logic silently no-ops.
fn sync_roots_from_base(sandbox_base: &str) -> Vec<PathBuf> {
    let data = PathBuf::from(sandbox_base).join("zcoder");
    vec![data.join("languages")]
}


/// Routes a business connection: a management connection, or a data (spawn)
/// connection which is relayed to the VM as raw bytes.
async fn handle_conn(mut stream: UnixStream, ctx: Ctx) -> Result<()> {
    let first = read_frame_idle(&mut stream).await?;
    // [diag] trace what the business connection asked for; a missing
    // manage_connection here means the management connection never formed.
    log::info!("handle_conn[diag]: first message = {}", kind_of(&first));
    match first {
        ClientMessage::Hello {
            version,
            root_map,
        } => {
            if version != PROTOCOL_VERSION {
                frame::write_message(
                    &mut stream,
                    &ServerMessage::Error {
                        session_id: None,
                        message: format!(
                            "protocol version mismatch: business {version}, daemon {PROTOCOL_VERSION}"
                        ),
                    },
                )
                .await?;
                return Ok(());
            }
            frame::write_message(
                &mut stream,
                &ServerMessage::HelloOk {
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            )
            .await?;

            // The pre-warmed pool must handshake with the same root map the
            // business side negotiates, so path arguments map correctly.
            ctx.pool.set_root_map(root_map).await;

            let second = read_frame_idle(&mut stream).await?;
            log::info!("handle_conn[diag]: second message = {}", kind_of(&second));
            match second {
                ClientMessage::Spawn { session_id, spec } => {
                    return proxy_data(stream, session_id, spec, ctx).await;
                }
                ClientMessage::SpawnStderr { session_id } => {
                    return proxy_stderr(stream, session_id, ctx).await;
                }
                ClientMessage::FileSyncStart { sync_id } => {
                    log::info!(
                        "handle_conn[diag]: FileSyncStart after hello, entering proxy_file_sync"
                    );
                    return proxy_file_sync(stream, sync_id, ctx).await;
                }
                ClientMessage::Manage => {
                    log::info!("handle_conn[diag]: Manage after hello, entering manage_connection");
                    return manage_connection(stream, ctx).await;
                }
                other => {
                    frame::write_message(
                        &mut stream,
                        &ServerMessage::Error {
                            session_id: None,
                            message: format!(
                                "unexpected instruction after hello: {}",
                                kind_of(&other)
                            ),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
        ClientMessage::Manage => {
            log::info!("handle_conn[diag]: first=Manage, entering manage_connection");
            return manage_connection(stream, ctx).await;
        }
        other => {
            frame::write_message(
                &mut stream,
                &ServerMessage::Error {
                    session_id: None,
                    message: format!("first message must be hello or manage: {}", kind_of(&other)),
                },
            )
            .await?;
            return Ok(());
        }
    }
}

/// Business management connection: forwards VM exit results to the business
/// side and forwards business signals to the VM. When this connection closes
/// the daemon shuts down, matching the business process lifetime.
async fn manage_connection(stream: UnixStream, ctx: Ctx) -> Result<()> {
    log::info!("business management connection established");
    // A new management connection replaces any previous one: the shared
    // `events_rx` receiver must have a single reader, otherwise SpawnOk and
    // ExecResult would be split across two stale connections.
    {
        let mut active = ctx.active_management.lock().await;
        if let Some(previous) = active.take() {
            log::warn!("closing previous business management connection");
            let _ = previous.shutdown(smol::net::Shutdown::Both);
        }
        *active = Some(stream.clone());
    }
    let event_stream = stream.clone();
    let event_ctx = ctx.clone();
    let events_task = ctx.executor.spawn(async move {
        loop {
            let Some(message) = event_ctx.events_rx.recv().await.ok() else {
                break;
            };
            // [diag] confirm this business management connection actually
            // forwards events to the client's manage_reader.
            log::info!("[diag] business events forward: {message:?}");
            if frame::write_message(&mut event_stream.clone(), &message)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let outcome = read_business_messages(stream, ctx.clone()).await;
    events_task.cancel().await;
    // The daemon is a thread inside the host process, so a closed management
    // connection is not fatal: keep accepting new business connections.
    log::warn!("business management connection closed, daemon keeps running");
    outcome
}

/// Reads business management messages: heartbeats are ignored, signals are
/// forwarded to the VM management connection.
async fn read_business_messages(mut stream: UnixStream, ctx: Ctx) -> Result<()> {
    loop {
        // Blocking read: the management connection is a low-frequency channel
        // (the business side only writes signals), so an idle timeout would
        // wrongly drop it after 60s of silence; EOF is the liveness signal.
        let message = frame::read_message(&mut stream).await?;
        match message {
            ClientMessage::Heartbeat => {}
            ClientMessage::Signal { session_id, signal } => {
                log::info!("forwarding signal {signal:?} for session {session_id} to vm");
                let _ = ctx
                    .control_tx
                    .send(ClientMessage::Signal { session_id, signal })
                    .await;
            }
            ClientMessage::Shutdown => break,
            other => log::debug!(
                "ignoring message on business management connection: {}",
                kind_of(&other)
            ),
        }
    }
    Ok(())
}

/// Data connection: handshake with the VM, spawn the child, confirm with
/// SpawnOk, then relay raw bytes between the unix socket and the VM
/// connection until one side ends.
async fn proxy_data(
    mut stream: UnixStream,
    session_id: u64,
    spec: cmd_agent_protocol::ExecSpec,
    ctx: Ctx,
) -> Result<()> {
    // Wait for the VM agent server to become ready: the first spawns (e.g.
    // shell-env capture) race the daemon's server deployment, so block here
    // invisibly to the caller until the management connection is up.
    let vm_ready = ctx.vm_ready.clone();
    let vm_ready_notify = ctx.vm_ready_notify.clone();
    let ready_deadline = std::time::Instant::now() + VM_READY_TIMEOUT;
    while !vm_ready.load(Ordering::Acquire) {
        if std::time::Instant::now() >= ready_deadline {
            log::warn!(
                "session {session_id}: vm agent server not ready within {VM_READY_TIMEOUT:?}, attempting spawn"
            );
            break;
        }
        // Event-driven wait: the VM manager broadcasts on vm_ready_notify when
        // the server becomes reachable; the fallback wake is only a deadlock
        // backstop so a missed notification cannot hang the spawn.
        let notified = vm_ready_notify.notified();
        let _ = smol::future::or(
            async { notified.await },
            async {
                smol::Timer::after(WAIT_NOTIFY_FALLBACK).await;
            },
        )
        .await;
    }

    let mut vm = ctx
        .pool
        .take()
        .await
        .with_context(|| format!("vm connection for session {session_id}"))?;
    frame::write_message(&mut vm, &ClientMessage::Spawn { session_id, spec })
        .await
        .with_context(|| format!("vm spawn for session {session_id}"))?;

    // Wait for the spawn confirmation over the management connection; the
    // data connection carries only raw bytes after Spawn, so confirmation
    // cannot race with child output.
    if let Err(message) = wait_spawn_ok(&ctx, session_id).await {
        frame::write_message(
            &mut stream,
            &ServerMessage::Error {
                session_id: Some(session_id),
                message: message.to_string(),
            },
        )
        .await?;
        return Ok(());
    }

    // The business-side client confirms SpawnOk on its management connection
    // (forwarded by vm_manager), not on this data connection. Writing SpawnOk
    // here would leak a frame into the child's stdio relay.
    // Run the byte relay on the data-plane executor so this business worker is
    // not blocked waiting on transfer completion; the relay owns both
    // connections and closes them when the child exits.
    log::info!("session {session_id} relay started");
    let relay = ctx.relay_executor.clone();
    relay
        .spawn(async move {
            // A panic inside the relay must not take down the relay workers:
            // catch it so a bad transfer cannot kill the data-plane threads.
            let _ = std::panic::AssertUnwindSafe(relay_duplex(stream, vm, session_id, ctx))
                .catch_unwind()
                .await;
        })
        .detach();
    Ok(())
}

/// Relays a dedicated stderr connection to the VM. Unlike `proxy_data`, no
/// SpawnOk is awaited here: the server merely registers the socket as the
/// session's stderr (fd 2) and the child writes to it once it is spawned, so
/// the relay just bridges raw bytes. The business side opens this connection
/// before the main spawn connection.
async fn proxy_stderr(
    stream: UnixStream,
    session_id: u64,
    ctx: Ctx,
) -> Result<()> {
    // Wait for the VM agent server to become ready, matching proxy_data so the
    // first stderr spawn does not race the daemon's server deployment.
    let vm_ready_notify = ctx.vm_ready_notify.clone();
    let ready_deadline = std::time::Instant::now() + VM_READY_TIMEOUT;
    while !ctx.vm_ready.load(Ordering::Acquire) {
        if std::time::Instant::now() >= ready_deadline {
            log::warn!(
                "session {session_id}: vm agent server not ready within {VM_READY_TIMEOUT:?}, attempting stderr attach"
            );
            break;
        }
        // Event-driven wait like proxy_data; the fallback wake is a backstop
        // so a missed notification cannot hang the stderr attach.
        let notified = vm_ready_notify.notified();
        let _ = smol::future::or(
            async { notified.await },
            async {
                smol::Timer::after(WAIT_NOTIFY_FALLBACK).await;
            },
        )
        .await;
    }

    let mut vm = ctx
        .pool
        .take()
        .await
        .with_context(|| format!("vm connection for stderr session {session_id}"))?;
    frame::write_message(&mut vm, &ClientMessage::SpawnStderr { session_id })
        .await
        .with_context(|| format!("vm spawn_stderr for session {session_id}"))?;
    log::info!("session {session_id} stderr relay started");
    let relay = ctx.relay_executor.clone();
    relay
        .spawn(async move {
            // A panic inside the relay must not take down the relay workers:
            // catch it so a bad transfer cannot kill the data-plane threads.
            let _ = std::panic::AssertUnwindSafe(relay_duplex(stream, vm, session_id, ctx))
                .catch_unwind()
                .await;
        })
        .detach();
    Ok(())
}

/// Relays a file-sync session to the VM. The business-side connection carries
/// the whole transfer (control frames plus the raw file bodies); the daemon is
/// a transparent byte relay, exactly like a spawn data connection, so it does
/// not need to understand the file-sync protocol. The VM's `run_file_sync`
/// consumes the stream until `FileSyncEnd`.
async fn proxy_file_sync(
    stream: UnixStream,
    sync_id: u64,
    ctx: Ctx,
) -> Result<()> {
    // Wait for the VM agent server to become ready, matching proxy_data so the
    // first sync does not race the daemon's server deployment.
    let vm_ready_notify = ctx.vm_ready_notify.clone();
    let ready_deadline = std::time::Instant::now() + VM_READY_TIMEOUT;
    while !ctx.vm_ready.load(Ordering::Acquire) {
        if std::time::Instant::now() >= ready_deadline {
            log::warn!(
                "file sync {sync_id}: vm agent server not ready within {VM_READY_TIMEOUT:?}, attempting sync"
            );
            break;
        }
        // Event-driven wait like proxy_data; the fallback wake is a backstop so
        // a missed notification cannot hang the sync.
        let notified = vm_ready_notify.notified();
        let _ = smol::future::or(
            async { notified.await },
            async {
                smol::Timer::after(WAIT_NOTIFY_FALLBACK).await;
            },
        )
        .await;
    }

    // Take a pooled, already-handshaken VM connection and start the sync
    // session on it. The relay then bridges raw bytes until the client closes
    // the session; the VM connection is dropped when the relay ends.
    let mut vm = ctx
        .pool
        .take()
        .await
        .with_context(|| format!("vm connection for file sync {sync_id}"))?;
    frame::write_message(&mut vm, &ClientMessage::FileSyncStart { sync_id })
        .await
        .with_context(|| format!("vm file sync start for {sync_id}"))?;
    log::info!("file sync {sync_id} relay started");
    let relay = ctx.relay_executor.clone();
    relay
        .spawn(async move {
            // A panic inside the relay must not take down the relay workers:
            // catch it so a bad transfer cannot kill the data-plane threads.
            let _ = std::panic::AssertUnwindSafe(relay_duplex(stream, vm, sync_id, ctx))
                .catch_unwind()
                .await;
        })
        .detach();
    Ok(())
}

/// Waits for a spawn confirmation by session id, polling the shared table.
async fn wait_spawn_ok(ctx: &Ctx, session_id: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + VM_HANDSHAKE_TIMEOUT;
    loop {
        if let Some(result) = ctx.spawn_oks.lock().await.remove(&session_id) {
            return result.map_err(Error::message);
        }
        if std::time::Instant::now() >= deadline {
            // Drop any late confirmation so the entry cannot leak in the table.
            ctx.spawn_oks.lock().await.remove(&session_id);
            return Err(Error::message(format!(
                "session {session_id} spawn confirmation timed out"
            )));
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
}

/// Relays bytes both ways between the business unix socket and the VM
/// connection. The relay lifetime follows the child: once the VM side hits
/// EOF (the child exited), both connections are torn down and the stdin
/// relay is cancelled, so the VM connection is released immediately even when
/// the business side never closes its stdin (which would otherwise leave it
/// in CLOSE-WAIT until the business side also closes). No signal is sent
/// here: a business-side close is a normal protocol step (e.g. LSP shutdown
/// half-closes stdin), so killing the child would race with its own clean
/// exit. Explicit termination goes through a `Signal` on the management
/// connection, and a crashed business process is covered by the
/// management-connection lifetime.
async fn relay_duplex(uni: UnixStream, vm: TcpStream, session_id: u64, ctx: Ctx) -> Result<()> {
    let uni_r = uni.clone();
    let uni_w = uni.clone();
    let vm_r = vm.clone();
    let vm_w = vm.clone();

    let to_vm = ctx.relay_executor.spawn(relay_unix_to_tcp(uni_r, vm_w, session_id, "to_vm"));
    let to_uni = ctx.relay_executor.spawn(relay_tcp_to_unix(vm_r, uni_w, session_id, "to_uni"));

    // The child's output EOF ends the relay: tear down both connections and
    // cancel the stdin relay, which may be blocked reading a business stdin
    // that never closes. Releasing the VM connection here frees its fd
    // immediately instead of leaving it in CLOSE-WAIT after the child exits.
    let vm_result = to_uni.await;
    let _ = vm.shutdown(smol::net::Shutdown::Both);
    let _ = uni.shutdown(smol::net::Shutdown::Both);
    let _ = to_vm.cancel().await;

    // [diag] attribute a stuck relay to one direction; this line only runs
    // after the VM direction completes, so its absence means to_uni is pending.
    log::info!(
        "[diag] relay_duplex session {session_id}: vm_result={vm_result:?}, stdin relay cancelled"
    );
    log::info!("session {session_id} relay ended");
    vm_result
}

/// Copies bytes from the business unix socket to the VM connection; EOF on
/// the unix side closes the VM write direction (child stdin EOF).
async fn relay_unix_to_tcp(
    mut from: UnixStream,
    mut to: TcpStream,
    session_id: u64,
    dir: &str,
) -> Result<()> {
    let mut buf = vec![0u8; RELAY_CHUNK_SIZE];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            // [diag] the business side closed its unix write end (child stdin
            // EOF) and this relay direction is done.
            log::info!("[diag] relay {dir} session {session_id}: uni EOF");
            break;
        }
        to.write_all(&buf[..n]).await?;
        to.flush().await?;
    }
    let _ = to.shutdown(smol::net::Shutdown::Write);
    log::info!("[diag] relay {dir} session {session_id}: done");
    Ok(())
}

/// Copies bytes from the VM connection to the business unix socket; EOF on
/// the VM side (child exited) closes the unix write direction.
async fn relay_tcp_to_unix(
    mut from: TcpStream,
    mut to: UnixStream,
    session_id: u64,
    dir: &str,
) -> Result<()> {
    let mut buf = vec![0u8; RELAY_CHUNK_SIZE];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            // [diag] the VM connection hit EOF (child exited / server closed)
            // and this relay direction is done.
            log::info!("[diag] relay {dir} session {session_id}: tcp EOF");
            break;
        }
        to.write_all(&buf[..n]).await?;
        to.flush().await?;
    }
    let _ = to.shutdown(smol::net::Shutdown::Write);
    log::info!("[diag] relay {dir} session {session_id}: done");
    Ok(())
}

/// Maintains the VM management connection: heartbeat, forward signals,
/// relay exit results, and recover (reconnect then redeploy) when the link
/// is lost.
async fn vm_manager(
    ctx: Ctx,
    control_rx: smol::channel::Receiver<ClientMessage>,
    _events_tx: smol::channel::Sender<ServerMessage>,
) {
    loop {
        let mut stream = match connect_vm_handshake(&ctx.vm_addr, None).await {
            Ok(stream) => stream,
            Err(err) => {
                log::warn!("vm management connect failed: {err}, recovering");
                recover_vm(&ctx).await;
                continue;
            }
        };
        if let Err(err) = frame::write_message(&mut stream, &ClientMessage::Manage).await {
            log::warn!("vm manage failed: {err}, recovering");
            recover_vm(&ctx).await;
            continue;
        }
        log::info!("vm management connection established");
        // Mark the VM agent server reachable so pending data relays stop
        // waiting and proceed with their spawn. Notify the event-driven
        // waiters instead of leaving them polling the flag on the business
        // worker.
        ctx.vm_ready.store(true, Ordering::Release);
        ctx.vm_ready_notify.notify_waiters();
        // Connections pooled against the previous server incarnation (if any)
        // may be dead after a reconnect; drop them so spawns never get one.
        ctx.pool.invalidate().await;
        // Any spawn confirmations recorded against the previous server are
        // stale too; clear them so a dead session id cannot leak in the table.
        ctx.spawn_oks.lock().await.clear();

        // Heartbeat, control forwarding, and event reading run concurrently
        // on clones of the socket; writes are serialized by a shared lock.
        let write_lock = Arc::new(smol::lock::Mutex::new(()));

        let mut hb_stream = stream.clone();
        let hb_lock = write_lock.clone();
        let heartbeat = ctx.executor.spawn(async move {
            loop {
                smol::Timer::after(HEARTBEAT_INTERVAL).await;
                let _guard = hb_lock.lock().await;
                if frame::write_message(&mut hb_stream, &ClientMessage::Heartbeat)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok::<(), Error>(())
        });

        let mut ctrl_stream = stream.clone();
        let ctrl_lock = write_lock.clone();
        let ctrl_rx = control_rx.clone();
        let control = ctx.executor.spawn(async move {
            loop {
                let Some(message) = ctrl_rx.recv().await.ok() else {
                    break;
                };
                let _guard = ctrl_lock.lock().await;
                if frame::write_message(&mut ctrl_stream, &message).await.is_err() {
                    break;
                }
                log::info!("sent control message {message:?} to vm");
            }
            Ok::<(), Error>(())
        });

        let mut ev_stream = stream.clone();
        let ev_spawn_oks = ctx.spawn_oks.clone();
        let shared = ctx.shared.clone();
        let events = ctx.executor.spawn(async move {
            loop {
                let message = frame::read_message::<_, ServerMessage>(&mut ev_stream).await?;
                match message {
                    ServerMessage::SpawnOk { session_id } => {
                        log::info!("vm SpawnOk for session {session_id}");
                        // The daemon-internal table feeds proxy_data's
                        // wait_spawn_ok; the shared table feeds the client's
                        // wait_spawn_ok, so the client no longer needs a
                        // management reader thread.
                        ev_spawn_oks.lock().await.insert(session_id, Ok(()));
                        shared
                            .spawn_oks
                            .lock()
                            .unwrap()
                            .insert(session_id, Ok(()));
                        log::info!(
                            "[diag] vm SpawnOk written to shared spawn_oks: session={session_id}"
                        );
                    }
                    ServerMessage::Error {
                        session_id: Some(sid),
                        message,
                    } => {
                        ev_spawn_oks.lock().await.insert(sid, Err(message.clone()));
                        shared
                            .spawn_oks
                            .lock()
                            .unwrap()
                            .insert(sid, Err(message.clone()));
                    }
                    ServerMessage::ExecResult {
                        session_id,
                        exit_code,
                        timed_out,
                    } => {
                        // [diag] confirm ExecResult reaches the shared table; a
                        // missing log here while the server sent it means the
                        // management reader is stuck or the connection broke.
                        log::info!(
                            "[diag] vm ExecResult session={session_id}, exit_code={exit_code:?}"
                        );
                        shared
                            .exec_results
                            .lock()
                            .unwrap()
                            .insert(session_id, (exit_code, timed_out));
                        // Fulfil any event-driven waiter immediately; this is
                        // what lets the client's wait_exit_async return as soon
                        // as the result lands.
                        let waiters = shared.exec_waiters.lock().unwrap().remove(&session_id);
                        if let Some(waiters) = waiters {
                            for tx in waiters {
                                let _ = tx.send(exit_code).await;
                            }
                        }
                    }
                    other => {
                        log::debug!("ignoring vm event: {other:?}");
                    }
                }
            }
        });

        // Consume client signal requests from the shared channel and write them
        // to the VM management connection, serialized by the same write lock as
        // heartbeats and control messages. This replaces the client's former
        // signal-writer thread.
        let sig_rx = ctx.shared.signal_rx.clone();
        let mut sig_stream = stream.clone();
        let sig_lock = write_lock.clone();
        let signal_task = ctx.executor.spawn(async move {
            while let Ok(request) = sig_rx.recv().await {
                let _guard = sig_lock.lock().await;
                let result = frame::write_message(
                    &mut sig_stream,
                    &ClientMessage::Signal {
                        session_id: request.session_id,
                        signal: request.signal,
                    },
                )
                .await
                .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()));
                let _ = request.reply.send(result);
            }
            Ok::<(), Error>(())
        });

        let outcome: Result<()> = smol::future::or(
            smol::future::or(smol::future::or(heartbeat, control), events),
            signal_task,
        )
        .await;
        log::warn!("vm management connection lost: {outcome:?}, recovering");
        recover_vm(&ctx).await;
        smol::Timer::after(RECONNECT_DELAY).await;
    }
}

/// Connects to the VM server and performs the Hello handshake.
async fn connect_vm_handshake(
    addr: &str,
    root_map: Option<&RootMap>,
) -> Result<TcpStream> {
    log::debug!("connect_vm_handshake: connecting to {addr}");
    let mut stream = smol::future::or(
        async { TcpStream::connect(addr).await },
        async {
            smol::Timer::after(VM_CONNECT_TIMEOUT).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("connect timeout to {addr}"),
            ))
        },
    )
    .await
    .with_context(|| format!("vm connect to {addr}"))?;
    log::info!("connect_vm_handshake: tcp connected to {addr}");

    frame::write_message(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            root_map: root_map.cloned(),
        },
    )
    .await?;

    loop {
        let message = read_frame_timeout(&mut stream, VM_HANDSHAKE_TIMEOUT).await?;
        match message {
            ServerMessage::HelloOk { .. } => {
                log::info!("connect_vm_handshake: HelloOk from {addr}");
                return Ok(stream);
            }
            ServerMessage::Error { message, .. } => {
                log::warn!("connect_vm_handshake: {addr} rejected handshake: {message}");
                return Err(Error::message(format!("vm handshake failed: {message}")));
            }
            _ => continue,
        }
    }
}

/// Tries a quick connect to the VM; if it fails and SSH deployment is
/// configured, redeploys a fresh server binary.
async fn recover_vm(ctx: &Ctx) {
    if connect_vm_handshake(&ctx.vm_addr, None).await.is_ok() {
        log::info!("vm already reachable, no redeploy needed");
        return;
    }
    let Some(ssh) = ctx.ssh.clone() else {
        log::warn!("no ssh config, skipping deploy recovery");
        return;
    };
    let Some(binary_path) = ctx.server_binary.clone() else {
        log::warn!("no server binary path, skipping deploy recovery");
        return;
    };
    let binary = match std::fs::read(&binary_path) {
        Ok(binary) => binary,
        Err(err) => {
            log::warn!("reading server binary {}: {err}", binary_path.display());
            return;
        }
    };
    let port = ctx.agent_port;
    log::warn!("vm not reachable, deploying fresh cmd-agentd");
    if let Err(err) = smol::unblock(move || deploy::deploy(&ssh, &binary, port)).await {
        log::warn!("deploy recovery failed: {err}");
    } else {
        log::info!("deploy recovery done");
    }
}

/// Reads one business message with an idle timeout.
async fn read_frame_idle<S: smol::io::AsyncRead + Unpin>(stream: &mut S) -> Result<ClientMessage> {
    smol::future::or(
        async { frame::read_message(stream).await.map_err(Error::from) },
        async {
            smol::Timer::after(CONNECTION_IDLE_TIMEOUT).await;
            Err(Error::message("connection idle timeout"))
        },
    )
    .await
}

/// Reads a frame with an explicit timeout.
async fn read_frame_timeout<S, M>(stream: &mut S, timeout: Duration) -> Result<M>
where
    S: smol::io::AsyncRead + Unpin,
    M: serde::de::DeserializeOwned,
{
    smol::future::or(
        async { frame::read_message(stream).await.map_err(Error::from) },
        async {
            smol::Timer::after(timeout).await;
            Err(Error::message(format!(
                "timed out after {}s",
                timeout.as_secs()
            )))
        },
    )
    .await
}

/// Human-readable kind of a client message, for logging.
fn kind_of(message: &ClientMessage) -> &'static str {
    match message {
        ClientMessage::Hello { .. } => "hello",
        ClientMessage::Manage => "manage",
        ClientMessage::Heartbeat => "heartbeat",
        ClientMessage::Spawn { .. } => "spawn",
        ClientMessage::SpawnStderr { .. } => "spawn_stderr",
        ClientMessage::Signal { .. } => "signal",
        ClientMessage::FileSyncStart { .. } => "file_sync_start",
        ClientMessage::FileBegin { .. } => "file_begin",
        ClientMessage::FileRename { .. } => "file_rename",
        ClientMessage::FileDelete { .. } => "file_delete",
        ClientMessage::FileCreateDir { .. } => "file_create_dir",
        ClientMessage::FileSyncEnd { .. } => "file_sync_end",
        ClientMessage::Query => "query",
        ClientMessage::Shutdown => "shutdown",
    }
}
