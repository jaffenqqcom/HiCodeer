//! cmd-agentd: the QEMU guest-side command agent.
//!
//! The main thread runs an event loop over the virtio-serial port pool (one
//! mgmt port, N data ports, N err ports) and spawns a dedicated worker *thread*
//! per command and per mount. Each worker thread forks the concrete command
//! process via `Command::spawn` (glibc posix_spawn, no full mm copy) and keeps
//! the port fds while it forwards stdio. All potentially blocking work (path
//! mapping, exec, stdio forwarding, mount) lives in the worker thread / command
//! process, so a slow or stuck command can never stall the agent's event loop
//! and, through the spawn handshake, the zcoder side. A per-command process
//! fork (which copies the whole agent) is deliberately avoided: under guest
//! memory/swap pressure that copy blocks the event loop for seconds.
//!
//! Port lifecycle: each data/err port is `Handshake` while the agent reads the
//! protocol frames; once a command is spawned the port fds are handed to the
//! worker thread and the agent stops polling them, flipping the port back to
//! handshake when the worker thread reports the command's exit.

mod exec;

use qemu_cmd_agent_protocol::messages::{
    ClientMessage, FdMode, RootMap, ServerMessage, Signal, PROTOCOL_VERSION,
};
use qemu_cmd_agentd::path_map::PathMap;
use serde::de::DeserializeOwned;
use serde::Serialize;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Management port name (matches ohos-qemu build_argv).
const MGMT_PORT_NAME: &str = "zcoder.mgmt";
/// Data port name prefix (`zcoder.cmd.<n>`).
const DATA_PORT_PREFIX: &str = "zcoder.cmd.";
/// stderr port name prefix (`zcoder.err.<n>`).
const ERR_PORT_PREFIX: &str = "zcoder.err.";
/// Directory where S41virtioports symlinks ports as `/dev/virtio-ports/<name>`.
const VIRTIO_PORTS_DIR: &str = "/dev/virtio-ports";
/// Delay before reopening a port after an error or host disconnect.
const REOPEN_DELAY: Duration = Duration::from_millis(500);
/// How long the agent waits for the Spawn frame after sending HelloOk before
/// abandoning the handshake. The host may time out waiting for HelloOk (its own
/// handshake timeout) and then never send Spawn; blocking forever in the
/// Upper bound on a single frame payload, matching the protocol's guard.
const MAX_FRAME_SIZE: usize = 256 * 1024 * 1024;
/// Chunk size for draining the management port into its persistent frame
/// buffer, so a split frame accumulates across poll rounds without loss.
const MGMT_READ_CHUNK: usize = 4096;
/// Number of data/err port pairs (== the host's PORT_POOL_SIZE). The vectors
/// are indexed by the port number parsed from the device name, so cmd-agent
/// connecting to `cmd.<n>.sock` maps directly to slot n.
const PORT_POOL_SIZE: usize = 14;
/// Upper bound on concurrently running workers (== the host port pool size).
const MAX_WORKERS: usize = 14;
/// Exit code reported when a worker dies without writing a result.
const UNKNOWN_EXIT: i32 = -1;
/// How long a finished session keeps its ports while waiting for the host's
/// ExecResultAck before they are force-reclaimed, so a hung host never leaks
/// ports (matches the protocol's two-phase reclaim contract).
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Consecutive heartbeat reconciliations where a session is missing on the host
/// before this side kills the process and recycles its port. Two rounds absorb
/// the normal transient mismatch (host removes a session on ExecResult, this
/// side only on ExecResultAck).
const RECONCILE_THRESHOLD: u32 = 2;

/// Minimal stderr logger: cmd-agentd output lands on the guest console, which
/// the host forwards into hilog when qemu_debug_assertions is enabled.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        eprintln!("[cmd-agentd][{}] {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logger() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

/// Opens a virtio-serial port device for both directions.
fn open_port(dev: &PathBuf) -> std::io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)?;
    // Every virtio-serial port is marked non-blocking so the single-threaded
    // event loop can bound every read/write. A blocking data/err fd lets a
    // full QEMU chardev buffer (guest stdout the host has not drained) wedge
    // handle_data / handle_err, freezing the loop and dropping heartbeats.
    set_nonblocking(file.as_raw_fd());
    Ok(file)
}

/// Marks an fd non-blocking. Used for the management port, whose frame I/O is
/// polled with a bound so a stalled peer cannot wedge the event loop.
fn set_nonblocking(fd: i32) {
    // SAFETY: fcntl F_GETFL/F_SETFL on a valid, open port fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
}

/// Appends one frame's bytes to a pending-write buffer without blocking, so
/// the event loop can queue output and flush it on POLLOUT instead of blocking
/// the single thread per frame on a full connection (P1-3). Returns false only
/// if serialization fails.
fn queue_frame(pending: &mut Vec<u8>, message: &impl Serialize) -> bool {
    let payload = match serde_json::to_vec(message) {
        Ok(p) => p,
        Err(err) => {
            log::error!("[diag] queue_frame: serialize failed: {err}");
            return false;
        }
    };
    // Pack length prefix + payload into one buffer so a partial flush never
    // leaves a split frame on the wire that misaligns the peer's parser.
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    pending.extend_from_slice(&frame);
    true
}

/// Flushes a pending-write buffer to `fd` as far as it will go (non-blocking).
/// Returns true when everything was written; false when the fd is not writable
/// yet (WouldBlock) or the write failed, leaving the remaining bytes buffered
/// for a later flush or a reconnect replay (P0-2). The fd must be non-blocking.
fn flush_pending(fd: i32, pending: &mut Vec<u8>) -> bool {
    while !pending.is_empty() {
        // SAFETY: write the leading slice of the pending buffer to the
        // non-blocking port fd.
        let n = unsafe { libc::write(fd, pending.as_ptr() as *const libc::c_void, pending.len()) };
        if n > 0 {
            pending.drain(..n as usize);
            continue;
        }
        let err = if n < 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        match err {
            Some(err) if err.kind() == std::io::ErrorKind::WouldBlock => return false,
            Some(err) => log::error!("[diag] flush_pending: {err}"),
            None => {}
        }
        // n == 0 or a hard error: keep the bytes buffered for a reconnect
        // replay; the caller decides whether the connection is dead.
        return false;
    }
    true
}

/// Flushes queued management frames; with no connection the buffer is kept so
/// a reconnect replays it (P0-2).
fn flush_mgmt_writes(mgmt: &mut Option<File>, pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    let Some(f) = mgmt.as_ref() else {
        return;
    };
    flush_pending(f.as_raw_fd(), pending);
}

/// Flushes every handshake-phase data port's queued outbound frames.
fn flush_data_writes(data_ports: &mut Vec<Option<DataPort>>) {
    for port in data_ports.iter_mut().flatten() {
        if port.write_pending.is_empty() {
            continue;
        }
        if let Some(fd) = &port.fd {
            flush_pending(fd.as_raw_fd(), &mut port.write_pending);
        }
    }
}

/// Whether the persistent management buffer holds at least one complete frame.
/// The event loop keeps servicing mgmt within the same round when this is true,
/// because `drain_fd_into` may have pulled several frames at once (P0-3) while
/// `handle_mgmt` consumes one; once the bytes are in the buffer the socket no
/// longer fires POLLIN, so without this check the remaining frames would sit
/// unprocessed.
fn mgmt_has_complete_frame(pending: &[u8]) -> bool {
    if pending.len() < 4 {
        return false;
    }
    let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
    len <= MAX_FRAME_SIZE && pending.len() >= 4 + len
}

/// Same completeness check for a data port's persistent frame buffer.
fn data_port_has_complete_frame(port_index: usize, data_ports: &[Option<DataPort>]) -> bool {
    let Some(port) = data_ports.get(port_index).and_then(|p| p.as_ref()) else {
        return false;
    };
    if port.pending.len() < 4 {
        return false;
    }
    let len = u32::from_le_bytes([
        port.pending[0],
        port.pending[1],
        port.pending[2],
        port.pending[3],
    ]) as usize;
    len <= MAX_FRAME_SIZE && port.pending.len() >= 4 + len
}

/// Reads all currently-available bytes from a non-blocking fd into `buf`.
/// Returns false when the read hits EOF or a non-WouldBlock error: the
/// connection is unusable and the caller should reopen the port.
fn drain_fd_into(fd: i32, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; MGMT_READ_CHUNK];
    loop {
        // SAFETY: read into a stack buffer from the non-blocking port fd.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            continue;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return true;
            }
            log::error!("[diag] drain_fd_into: {err}");
            return false;
        }
        // n == 0: EOF. A virtio-serial chardev port with no connected peer
        // reads as EOF, so with nothing buffered that is just an idle port,
        // not a disconnect -- return true so idle data ports do not flood the
        // event loop with reset/hangup handling every round. Only a peer that
        // closed mid-frame (bytes already buffered) is reported as gone.
        return buf.is_empty();
    }
}

/// Parses one length-prefixed JSON frame from the head of `pending`, draining
/// the consumed bytes. Returns None when the buffer holds less than one full
/// frame; an oversized or unparseable frame is dropped so the stream cannot
/// wedge on garbage.
fn parse_frame<M: DeserializeOwned>(pending: &mut Vec<u8>) -> Option<M> {
    if pending.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
    if len > MAX_FRAME_SIZE {
        log::warn!("[diag] parse_frame: oversize frame len={len}, clearing buffer");
        pending.clear();
        return None;
    }
    if pending.len() < 4 + len {
        return None;
    }
    let payload = &pending[4..4 + len];
    match serde_json::from_slice(payload) {
        Ok(msg) => {
            pending.drain(..4 + len);
            Some(msg)
        }
        Err(err) => {
            log::warn!("[diag] parse_frame: parse failed: {err}, dropping frame");
            pending.drain(..4 + len);
            None
        }
    }
}

/// Reopens the management port after its connection was lost, so a fresh host
/// connection is accepted again. QEMU's socket chardev accepts exactly one
/// client: a stale open fd leaves every later connect parked in the backlog.
fn reopen_mgmt_port(mgmt: &mut Option<File>, dev: Option<&PathBuf>) {
    *mgmt = None;
    let Some(dev) = dev else {
        return;
    };
    match open_port(dev) {
        Ok(fd) => {
            log::info!("[diag] reopen_mgmt_port: mgmt port reopened");
            *mgmt = Some(fd);
        }
        Err(err) => log::error!("[diag] reopen_mgmt_port: open {dev:?}: {err}"),
    }
}

/// Enumerates `zcoder.*` ports under `/dev/virtio-ports`, resolving each
/// symlink to its `/dev/vport*` device node.
fn scan_virtio_ports() -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    match std::fs::read_dir(VIRTIO_PORTS_DIR) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("zcoder.") {
                    let dev = std::fs::canonicalize(entry.path())
                        .unwrap_or_else(|_| entry.path());
                    result.push((name, dev));
                }
            }
        }
        Err(err) => log::warn!("[diag] scan_virtio_ports: {VIRTIO_PORTS_DIR}: {err}"),
    }
    result
}

/// One data port. `Handshake` while the agent reads protocol frames; `Running`
/// once the fds moved into a worker.
struct DataPort {
    dev: PathBuf,
    fd: Option<File>,
    state: DataPortState,
    /// Persistent inbound frame buffer (P0-3): bytes accumulate across poll
    /// rounds and complete frames are sliced off by `parse_frame`, so a frame
    /// split across virtio-serial deliveries is never dropped or misaligned.
    pending: Vec<u8>,
    /// Outbound frames queued for the handshake (HelloOk, SpawnOk, ...), flushed
    /// by the event loop on POLLOUT so a full connection never blocks the loop
    /// (P1-3).
    write_pending: Vec<u8>,
    /// Held Hello frame bytes while a `CleanupPending` session is being torn
    /// down (the old process is killed and its worker is exiting). Replayed
    /// into `pending` once the port returns to `Handshake`, so a reused port
    /// whose previous long-lived process was still Running never loses the new
    /// host's handshake.
    pending_hello: Option<Vec<u8>>,
}

#[derive(Clone)]
enum DataPortState {
    /// Waiting for the handshake Hello.
    Handshake,
    /// HelloOk already sent; waiting for Spawn. `err_used` records whether the
    /// host signalled stderr (SpawnStderr on the data connection, kept for
    /// protocol compatibility; the guest otherwise reads it from
    /// `spec.stderr_mode`).
    HelloDone { err_used: bool },
    Running { session_id: u64 },
    /// A Hello arrived while the previous session's process was still Running
    /// (the host reused the port before this side finished). The old process is
    /// killed; once its worker exits the port returns to `Handshake` and the
    /// held Hello is replayed.
    CleanupPending { old_session_id: u64 },
}

/// One stderr port. `handed_over` becomes true once the err fd moved into a
/// worker (the worker forwards the child's stderr over it).
struct ErrPort {
    dev: PathBuf,
    fd: Option<File>,
    handed_over: bool,
    /// Persistent inbound frame buffer (P0-3), like the data port's.
    pending: Vec<u8>,
}

/// A spawned command, tracked so signals can reach the command's process group.
/// `cmd_pid` is written by the worker thread once the command is spawned; until
/// then it is -1, so signals arriving before the command exists are dropped.
struct Session {
    session_id: u64,
    data_index: usize,
    err_index: Option<usize>,
    cmd_pid: Arc<AtomicI32>,
    /// Set by the management handler on StdinEof so the command worker drops
    /// the child's stdin pipe and the process observes EOF. Shared with the
    /// worker thread; the session only stores a clone to flip it.
    stdin_closed: Arc<AtomicBool>,
    /// Consecutive heartbeat reconciliations where this session was missing on
    /// the host side. Reaching RECONCILE_THRESHOLD means the host has abandoned
    /// it, so the process is killed and the port recycled.
    inconsistent_rounds: u32,
}

/// A worker thread; kind decides how its result is interpreted on exit. The
/// command process pid lives in `cmd_pid` (set by the thread), used only for
/// signalling -- the thread itself reaps the command before reporting.
struct Worker {
    cmd_pid: Arc<AtomicI32>,
    result_rx: i32,
    kind: WorkerKind,
}

/// A finished command whose ports are held open until the host's ExecResultAck
/// (or the ack timeout) reclaims them, so a reused data connection never
/// carries stale bytes into the next handshake.
struct PendingAck {
    data_index: usize,
    err_index: Option<usize>,
    deadline: std::time::Instant,
}

enum WorkerKind {
    Command { session_id: u64 },
    Mount { uri: String, guest_path: String, mount_tag: String },
}

/// What a pollfd in the event loop stands for.
enum PollTarget {
    Mgmt,
    DataHandshake(usize),
    Worker(usize),
}

fn main() {
    init_logger();
    log::info!("[diag] cmd-agentd starting");

    let path_map = Arc::new(Mutex::new(PathMap::new()));

    let ports = scan_virtio_ports();
    log::info!("[diag] main: scanned {} virtio-serial ports", ports.len());
    for (name, dev) in &ports {
        log::info!("[diag] main: virtio port {name} -> {dev:?}");
    }

    let mut mgmt: Option<File> = None;
    // Device path of the management port, kept so a lost connection can reopen
    // it (see reopen_mgmt_port); None if the port never opened.
    let mut mgmt_dev: Option<PathBuf> = None;
    // Indexed by the port number parsed from the device name, so slot n is
    // `cmd.<n>` / `err.<n>` and cmd-agent's `cmd.<n>.sock` maps straight here.
    let mut data_ports: Vec<Option<DataPort>> = (0..PORT_POOL_SIZE).map(|_| None).collect();
    let mut err_ports: Vec<Option<ErrPort>> = (0..PORT_POOL_SIZE).map(|_| None).collect();
    for (name, dev) in ports {
        if name == MGMT_PORT_NAME {
            match open_port(&dev) {
                Ok(fd) => {
                    log::info!("[diag] main: mgmt port open");
                    // open_port already marked every port non-blocking; the
                    // mgmt channel's bounded frame I/O is what keeps a stalled
                    // peer from wedging the event loop.
                    mgmt_dev = Some(dev.clone());
                    mgmt = Some(fd);
                }
                Err(err) => log::error!("[diag] main: open mgmt port {dev:?}: {err}"),
            }
        } else if let Some(index) = name
            .strip_prefix(DATA_PORT_PREFIX)
            .and_then(|suffix| suffix.parse::<usize>().ok())
        {
            if index < PORT_POOL_SIZE {
                let fd = open_port(&dev).ok();
                if fd.is_none() {
                    log::warn!("[diag] main: open data port {dev:?} failed");
                }
                data_ports[index] = Some(DataPort {
                    dev,
                    fd,
                    state: DataPortState::Handshake,
                    pending: Vec::new(),
                    write_pending: Vec::new(),
                    pending_hello: None,
                });
            }
        } else if let Some(index) = name
            .strip_prefix(ERR_PORT_PREFIX)
            .and_then(|suffix| suffix.parse::<usize>().ok())
        {
            if index < PORT_POOL_SIZE {
                let fd = open_port(&dev).ok();
                if fd.is_none() {
                    log::warn!("[diag] main: open err port {dev:?} failed");
                }
                err_ports[index] = Some(ErrPort {
                    dev,
                    fd,
                    handed_over: false,
                    pending: Vec::new(),
                });
            }
        } else {
            log::debug!("[diag] main: ignored non-zcoder port {name}");
        }
    }
    log::info!("[diag] main: mgmt={} data_ports={} err_ports={}",
        mgmt.is_some(),
        data_ports.iter().filter(|p| p.is_some()).count(),
        err_ports.iter().filter(|p| p.is_some()).count()
    );

    event_loop(mgmt, mgmt_dev, data_ports, err_ports, path_map);
}

/// Single-threaded event loop. Rebuilds the poll set every round (cheap: at
/// most ~40 fds) and blocks in poll until a port needs attention or a worker
/// exited.
fn event_loop(
    mut mgmt: Option<File>,
    mgmt_dev: Option<PathBuf>,
    mut data_ports: Vec<Option<DataPort>>,
    mut err_ports: Vec<Option<ErrPort>>,
    path_map: Arc<Mutex<PathMap>>,
) {
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    // Bytes of an in-flight management frame that did not fully arrive yet.
    // Kept across rounds so a split or timed-out frame never loses its prefix.
    let mut mgmt_pending: Vec<u8> = Vec::new();
    // Outbound management frames queued by handlers and flushed on POLLOUT.
    // Survives a reconnect so an ExecResult written while the host was away is
    // replayed once the connection is back (P0-2).
    let mut mgmt_write_pending: Vec<u8> = Vec::new();
    // Sessions whose ExecResult was sent but ExecResultAck has not yet arrived.
    let mut pending_ack: HashMap<u64, PendingAck> = HashMap::new();
    let mut workers: Vec<Worker> = Vec::new();

    loop {
        let mut targets: Vec<PollTarget> = Vec::new();
        let mut pollfds: Vec<libc::pollfd> = Vec::new();

        if let Some(f) = &mgmt {
            targets.push(PollTarget::Mgmt);
            pollfds.push(libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN
                    | if mgmt_write_pending.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            });
        }
        for (index, port) in data_ports.iter().enumerate() {
            if let Some(port) = port {
                if let Some(fd) = &port.fd {
                    // Poll both handshake phases: Hello (Handshake) and the
                    // later Spawn (HelloDone). Both are driven by the event
                    // loop, never by a blocking in-handler wait, so many ports
                    // can handshake in parallel. POLLOUT is armed when this
                    // port has queued outbound frames to flush (P1-3).
                    if matches!(
                        port.state,
                        DataPortState::Handshake | DataPortState::HelloDone { .. }
                    ) {
                        targets.push(PollTarget::DataHandshake(index));
                        pollfds.push(libc::pollfd {
                            fd: fd.as_raw_fd(),
                            events: libc::POLLIN
                                | if port.write_pending.is_empty() {
                                    0
                                } else {
                                    libc::POLLOUT
                                },
                            revents: 0,
                        });
                    }
                }
            }
        }
        // (P2-9) err ports are not polled here: SpawnStderr rides the data
        // connection, and the err port is guest->host only (the worker forwards
        // the child's stderr over it), so there is no inbound frame to read.
        for (index, worker) in workers.iter().enumerate() {
            targets.push(PollTarget::Worker(index));
            // Worker exit is signalled through the result pipe (the worker
            // writes a result byte then closes the write end) rather than a
            // pidfd: pidfd poll is unreliable in this QEMU guest. A pipe read
            // end always wakes poll on data or EOF, even if the worker is
            // killed by a signal and never writes.
            pollfds.push(libc::pollfd {
                fd: worker.result_rx,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // SAFETY: poll over stable fds that stay alive for this round.
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            log::warn!("[diag] event_loop: poll: {}", std::io::Error::last_os_error());
            std::thread::sleep(REOPEN_DELAY);
            continue;
        }
        // Force-reclaim sessions whose ack never arrived, so a hung host does
        // not leak ports. Checked every round; bounded by the next poll event.
        reclaim_expired_acks(&mut pending_ack, &mut data_ports, &mut err_ports);
        if rc == 0 {
            continue;
        }
        // Flush frames queued by previous rounds before handling reads: POLLOUT
        // is armed for connections with pending output, so this round makes
        // progress on them instead of only serving new input (P1-3).
        flush_mgmt_writes(&mut mgmt, &mut mgmt_write_pending);
        flush_data_writes(&mut data_ports);

        // Track whether this round consumed any real event, so the spin-cap
        // sleep below only fires on idle rounds (P2-10).
        let mut handled_any = false;
        // Handle every ready event in one pass so a worker exit never starves
        // the heartbeats/commands of the same round. Workers are processed in
        // reverse index order because handle_worker_exit removes them, keeping
        // the remaining indices valid; the data/err/mgmt handlers run against
        // the same pollfd snapshot and never reorder the workers vector.
        for (index, pfd) in pollfds.iter().enumerate().rev() {
            if pfd.revents == 0 {
                continue;
            }
            if let PollTarget::Worker(worker_index) = targets[index] {
                handle_worker_exit(
                    worker_index,
                    &mut workers,
                    &mut sessions,
                    &mut pending_ack,
                    &mut data_ports,
                    &mut err_ports,
                    &mut mgmt_write_pending,
                    &path_map,
                );
                handled_any = true;
            }
        }
        for (index, pfd) in pollfds.iter().enumerate() {
            if pfd.revents == 0 || matches!(targets[index], PollTarget::Worker(_)) {
                continue;
            }
            match targets[index] {
                PollTarget::Mgmt => {
                    if pfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                        handled_any |= handle_mgmt(
                            &mut mgmt,
                            mgmt_dev.as_ref(),
                            &mut mgmt_pending,
                            &mut mgmt_write_pending,
                            &path_map,
                            &mut sessions,
                            &mut workers,
                            &mut pending_ack,
                            &mut data_ports,
                            &mut err_ports,
                        );
                        // P0-3: drain may have pulled several frames; keep
                        // servicing until the buffered frames are consumed.
                        while mgmt_has_complete_frame(&mgmt_pending) {
                            handled_any |= handle_mgmt(
                                &mut mgmt,
                                mgmt_dev.as_ref(),
                                &mut mgmt_pending,
                                &mut mgmt_write_pending,
                                &path_map,
                                &mut sessions,
                                &mut workers,
                                &mut pending_ack,
                                &mut data_ports,
                                &mut err_ports,
                            );
                        }
                    }
                }
                PollTarget::DataHandshake(port_index) => {
                    // Poll whenever the port is readable. Idle chardev ports
                    // poll as POLLIN|POLLHUP with nothing to read; handle_data
                    // reads the frame and returns quietly on WouldBlock /
                    // UnexpectedEof, so an idle port costs one cheap read per
                    // round, not a busy loop. A reused host connection (the
                    // executor keeps one connection per port across commands)
                    // delivers its Hello right here.
                    if pfd.revents & libc::POLLIN != 0 {
                        handled_any |= handle_data(
                            port_index,
                            &mut data_ports,
                            &mut sessions,
                            &path_map,
                            &mut workers,
                            &mut err_ports,
                        );
                        // P0-3: drain may have pulled several frames (e.g.
                        // SpawnStderr followed by Spawn) but handle_data
                        // consumes one; the socket no longer fires POLLIN once
                        // the bytes are in `pending`, so keep servicing until
                        // the buffered frames are consumed.
                        while data_port_has_complete_frame(port_index, &*data_ports) {
                            handled_any |= handle_data(
                                port_index,
                                &mut data_ports,
                                &mut sessions,
                                &path_map,
                                &mut workers,
                                &mut err_ports,
                            );
                        }
                    }
                }
                PollTarget::Worker(_) => {}
            }
        }
        // Flush frames queued by this round's handlers so output goes out now
        // instead of waiting for the next poll round (P1-3).
        flush_mgmt_writes(&mut mgmt, &mut mgmt_write_pending);
        flush_data_writes(&mut data_ports);
        // Cap the spin rate (P2-10): idle virtio-serial chardev ports poll as
        // POLLIN|POLLHUP with nothing to read, so poll() returns immediately.
        // Sleep only when this round consumed no real event -- frames, worker
        // exits and flushed output get no artificial 1ms delay.
        if !handled_any {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Reads one client message on the management port and routes it.
fn handle_mgmt(
    mgmt: &mut Option<File>,
    mgmt_dev: Option<&PathBuf>,
    pending: &mut Vec<u8>,
    mgmt_write_pending: &mut Vec<u8>,
    path_map: &Arc<Mutex<PathMap>>,
    sessions: &mut HashMap<u64, Session>,
    workers: &mut Vec<Worker>,
    pending_ack: &mut HashMap<u64, PendingAck>,
    data_ports: &mut Vec<Option<DataPort>>,
    err_ports: &mut Vec<Option<ErrPort>>,
) -> bool {
    let Some(port) = mgmt.as_mut() else {
        return false;
    };
    let mgmt_fd = port.as_raw_fd();
    // Drain whatever is available into the persistent buffer (a frame split
    // across rounds keeps its bytes), then parse one complete frame.
    if !drain_fd_into(mgmt_fd, pending) {
        // EOF or hard read error: the host connection is gone. Reopen the port
        // (and drop the stale bytes) so the next host connection is accepted
        // instead of parking in the chardev backlog.
        log::warn!("[diag] mgmt: connection gone, reopening port");
        pending.clear();
        reopen_mgmt_port(mgmt, mgmt_dev);
        return false;
    }
    let message: ClientMessage = match parse_frame(pending) {
        Some(message) => message,
        None => {
            log::debug!("[diag] mgmt: no complete frame (partial or empty)");
            return false;
        }
    };
    match message {
        ClientMessage::Hello { version, .. } => {
            log::info!("[diag] mgmt: hello version={version}");
            // Reply so the host's handshake unblocks; a fresh host mgmt
            // connection is served only after we reopen the port.
            if !queue_frame(
                mgmt_write_pending,
                &ServerMessage::HelloOk {
                    server_version: PROTOCOL_VERSION.to_string(),
                },
            ) {
                log::error!("[diag] mgmt: serialize HelloOk reply failed");
            } else {
                log::info!("[diag] mgmt: HelloOk reply queued");
            }
        }
        ClientMessage::Manage => log::info!("[diag] mgmt: manage received"),
        ClientMessage::Heartbeat { sessions: host_sessions } => {
            log::info!("[diag] mgmt: heartbeat");
            // Reconcile: sessions we track but the host no longer does are
            // stale (the host released them); after RECONCILE_THRESHOLD rounds
            // kill the process and recycle the port.
            let host_set: HashSet<u64> = host_sessions.iter().copied().collect();
            let mut to_cleanup = Vec::new();
            for (session_id, s) in sessions.iter_mut() {
                if host_set.contains(session_id) {
                    s.inconsistent_rounds = 0;
                } else {
                    s.inconsistent_rounds += 1;
                    if s.inconsistent_rounds >= RECONCILE_THRESHOLD {
                        to_cleanup.push(*session_id);
                    }
                }
            }
            for session_id in to_cleanup {
                cleanup_stale_session(session_id, sessions, data_ports, err_ports);
            }
            // Reply with this side's active sessions so the host can reconcile
            // the other direction (A \ B).
            let guest_active: Vec<u64> = sessions.keys().copied().collect();
            if !queue_frame(
                mgmt_write_pending,
                &ServerMessage::HeartbeatOk { sessions: guest_active },
            ) {
                log::warn!("[diag] mgmt: serialize HeartbeatOk failed");
            }
        }
        ClientMessage::Signal { session_id, signal } => {
            log::info!("[diag] mgmt: signal session_id={session_id} signal={signal:?}");
            let Some(session) = sessions.get(&session_id) else {
                log::warn!("[diag] mgmt: signal session_id={session_id} not found");
                return true;
            };
            let sig = match signal {
                Signal::SigInterrupt => libc::SIGINT,
                Signal::SigTerm => libc::SIGTERM,
                Signal::SigKill => libc::SIGKILL,
            };
            // The command is its own process group (Command::process_group(0)
            // in build_command); signal the whole group so a shell's children
            // die with it. -1 means the worker thread has not spawned it yet.
            let cmd_pid = session.cmd_pid.load(Ordering::SeqCst);
            if cmd_pid <= 0 {
                log::warn!("[diag] mgmt: signal session_id={session_id} before command spawned (cmd_pid={cmd_pid}), skipping"
                );
                return true;
            }
            // SAFETY: kill(2) the command's process group.
            let rc = unsafe { libc::kill(-cmd_pid, sig) };
            if rc != 0 {
                log::error!("[diag] mgmt: kill command group {} sig={sig}: {}",
                    cmd_pid,
                    std::io::Error::last_os_error()
                );
            } else {
                log::info!("[diag] mgmt: signal sent to group {cmd_pid} sig={sig}");
            }
        }
        ClientMessage::StdinEof { session_id } => {
            log::info!("[diag] mgmt: stdin eof session_id={session_id}");
            let Some(session) = sessions.get(&session_id) else {
                log::warn!("[diag] mgmt: stdin eof session_id={session_id} not found");
                return true;
            };
            // Flag the worker to drop the child's stdin pipe; the process then
            // observes EOF on its stdin and can exit (resident batch commands).
            session.stdin_closed.store(true, Ordering::SeqCst);
        }
        ClientMessage::ExecResultAck { session_id } => {
            log::info!("[diag] mgmt: ExecResultAck session_id={session_id}");
            if let Some(ack) = pending_ack.remove(&session_id) {
                reclaim_ports(ack.data_index, ack.err_index, data_ports, err_ports);
                log::info!("[diag] mgmt: reclaimed ports for session_id={session_id} on ack");
            } else {
                log::debug!("[diag] mgmt: ExecResultAck for unknown/expired session_id={session_id}");
            }
        }
        ClientMessage::MountFolder2QEMU {
            uri,
            mount_tag,
            guest_path,
        } => {
            log::info!("[diag] mgmt: mount {mount_tag} -> {guest_path} for {uri}");
            if workers.len() >= MAX_WORKERS {
                log::error!("[diag] mgmt: mount {uri} rejected: too many workers");
                if !queue_frame(
                    mgmt_write_pending,
                    &ServerMessage::Error {
                        session_id: None,
                        message: "too many workers".to_string(),
                    },
                ) {
                    log::warn!("[diag] mgmt: serialize Error (too many workers) failed");
                }
                return true;
            }
            let result_rx = spawn_mount_worker(&mount_tag, &guest_path);
            if result_rx < 0 {
                log::error!("[diag] mgmt: spawn mount worker failed for {uri}");
                if !queue_frame(
                    mgmt_write_pending,
                    &ServerMessage::Error {
                        session_id: None,
                        message: "spawn mount worker failed".to_string(),
                    },
                ) {
                    log::warn!("[diag] mgmt: serialize Error (mount worker spawn failed) failed");
                }
                return true;
            }
            log::info!("[diag] mgmt: mount worker spawned for {uri}");
            workers.push(Worker {
                cmd_pid: Arc::new(AtomicI32::new(-1)),
                result_rx,
                kind: WorkerKind::Mount {
                    uri,
                    guest_path,
                    mount_tag,
                },
            });
        }
        ClientMessage::UnmountFolder2QEMU { uri } => {
            log::info!("[diag] mgmt: unmount {uri}");
            path_map.lock().unwrap_or_else(|e| e.into_inner()).remove(&uri);
        }
        ClientMessage::Query => log::debug!("[diag] mgmt: query"),
        ClientMessage::Shutdown => log::info!("[diag] mgmt: shutdown requested (ignored)"),
        // Spawn/SpawnStderr belong on data/stderr ports, not mgmt.
        _ => log::warn!("[diag] mgmt: unexpected message"),
    }
    // A frame was parsed and handled.
    true
}

/// Data-port handshake: Hello -> HelloOk -> Spawn -> fork worker -> SpawnOk.
/// The port fds move into the worker; the agent reopens the device after the
/// worker exits.
fn handle_data(
    port_index: usize,
    data_ports: &mut Vec<Option<DataPort>>,
    sessions: &mut HashMap<u64, Session>,
    path_map: &Arc<Mutex<PathMap>>,
    workers: &mut Vec<Worker>,
    err_ports: &mut Vec<Option<ErrPort>>,
) -> bool {
    // Read one frame. The port/fd borrow ends here so the state machine below
    // can re-borrow the port freely.
    let message = {
        let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
            return false;
        };
        let Some(fd) = port.fd.as_mut() else {
            return false;
        };
        // Drain whatever arrived into the port's persistent buffer, then parse
        // one complete frame (P0-3): a frame split across deliveries keeps its
        // bytes across rounds, and an idle port (WouldBlock) yields no frame.
        if !drain_fd_into(fd.as_raw_fd(), &mut port.pending) {
            log::warn!("[diag] data {port_index}: connection gone, resetting to handshake");
            port.pending.clear();
            port.state = DataPortState::Handshake;
            return false;
        }
        match parse_frame::<ClientMessage>(&mut port.pending) {
            Some(message) => message,
            None => {
                return false;
            }
        }
    };

    // Advance the handshake state machine by one step. The event loop serves
    // every port each round, so a Spawn on one port never stalls another port's
    // Hello/Spawn: no blocking wait lives inside this handler.
    let state = match data_ports.get(port_index).and_then(|p| p.as_ref()) {
        Some(port) => port.state.clone(),
        None => return true,
    };
    match state {
        DataPortState::Handshake => {
            let version = match message {
                ClientMessage::Hello { version, .. } => version,
                other => {
                    log::error!("[diag] data {port_index}: expected Hello, got {other:?}");
                    return true;
                }
            };
            log::info!("[diag] data {port_index}: hello version={version}");
            let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
                return true;
            };
            if !queue_frame(&mut port.write_pending, &ServerMessage::HelloOk {
                    server_version: PROTOCOL_VERSION.to_string(),
                })
            {
                log::error!("[diag] data {port_index}: serialize HelloOk failed");
                return true;
            }
            log::info!("[diag] data {port_index}: HelloOk queued");
            port.state = DataPortState::HelloDone { err_used: false };
        }
        DataPortState::HelloDone { err_used } => match message {
            ClientMessage::Hello { version, .. } => {
                // The host restarted the handshake on a reused connection
                // (e.g. after an abandoned one). Re-acknowledge and keep
                // waiting for Spawn.
                log::info!("[diag] data {port_index}: re-hello version={version} in HelloDone");
                let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
                    return true;
                };
                if !queue_frame(&mut port.write_pending, &ServerMessage::HelloOk {
                        server_version: PROTOCOL_VERSION.to_string(),
                    })
                {
                    log::error!("[diag] data {port_index}: serialize HelloOk (re-hello) failed");
                    return true;
                }
                port.state = DataPortState::HelloDone { err_used: false };
            }
            ClientMessage::SpawnStderr { session_id } => {
                // SpawnStderr rides the data connection (the err port is
                // guest->host only), so record it and keep waiting for Spawn.
                log::info!("[diag] data {port_index}: SpawnStderr session_id={session_id} (via data connection)");
                let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
                    return true;
                };
                port.state = DataPortState::HelloDone { err_used: true };
            }
            ClientMessage::Spawn { session_id, spec } => {
                log::info!("[diag] data {port_index}: spawn session_id={session_id} program={}",
                    spec.source_program
                );
                log::info!(
                    "[diag] data {port_index}: Spawn spec binary={} args={:?} cwd={:?} env_keys={:?} stdin_bytes={}",
                    spec.binary,
                    spec.args,
                    spec.cwd_path,
                    spec.env.keys().collect::<Vec<_>>(),
                    spec.stdin.len()
                );
                // The err fd is the matching err port's own fd (same index as
                // the data port). It is used when the host signalled stderr via
                // SpawnStderr or the spec asks for it.
                let want_err = err_used || spec.stderr_mode != FdMode::Null;
                let err_fd = if want_err {
                    err_ports
                        .get(port_index)
                        .and_then(|p| p.as_ref())
                        .and_then(|p| p.fd.as_ref())
                        .map(|f| f.as_raw_fd())
                } else {
                    None
                };
                let err_index = port_index;
                let has_err = err_fd.is_some();

                if workers.len() >= MAX_WORKERS {
                    log::error!("[diag] data {port_index}: too many workers, rejecting spawn");
                    let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut())
                        else { return true; };
                    let _ = queue_frame(
                        &mut port.write_pending,
                        &ServerMessage::Error {
                            session_id: Some(session_id),
                            message: "too many workers".to_string(),
                        },
                    );
                    return true;
                }

                let data_fd = {
                    let Some(port) = data_ports.get(port_index).and_then(|p| p.as_ref()) else {
                        return true;
                    };
                    let Some(fd) = port.fd.as_ref() else {
                        return true;
                    };
                    fd.as_raw_fd()
                };
                let path_map_snapshot = path_map.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let spawned = spawn_command_worker(
                    data_fd,
                    err_fd,
                    spec,
                    path_map_snapshot,
                    session_id,
                );
                let (cmd_pid, result_rx, start_w, stdin_closed) = match spawned {
                    Some(spawned) => spawned,
                    None => {
                        // Thread spawn failed: keep the data port in handshake
                        // state (fd stays ours) and return the parked err fd to
                        // its port.
                        log::error!("[diag] data {port_index}: worker thread spawn failed for session_id={session_id}"
                        );
                        let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut())
                            else { return true; };
                        let _ = queue_frame(
                            &mut port.write_pending,
                            &ServerMessage::Error {
                                session_id: Some(session_id),
                                message: "worker thread spawn failed".to_string(),
                            },
                        );
                        if has_err {
                            unsafe { libc::close(err_fd.expect("checked")) };
                            reopen_err_port(err_index, err_ports);
                        }
                        return true;
                    }
                };

                // SpawnOk, then let the worker start forwarding (never
                // interleaves).
                let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
                    return true;
                };
                let spawn_ok = queue_frame(&mut port.write_pending, &ServerMessage::SpawnOk { session_id });
                notify_start(start_w);
                if !spawn_ok {
                    log::warn!("[diag] data {port_index}: serialize SpawnOk failed");
                } else {
                    log::info!("[diag] data {port_index}: SpawnOk queued session_id={session_id}");
                }
                // Long-lived port: the worker thread shares the data fd while
                // the command runs, and the agent keeps its own open reference
                // in `port.fd` across the command. Reopening after a command
                // would close the virtio-serial port and drop the QEMU chardev
                // link (the host's next connect is then never read), so the
                // port keeps this fd permanently. No close here: `port.fd`
                // owns it.
                port.state = DataPortState::Running { session_id };
                sessions.insert(
                    session_id,
                    Session {
                        session_id,
                        data_index: port_index,
                        err_index: if has_err { Some(err_index) } else { None },
                        cmd_pid: cmd_pid.clone(),
                        stdin_closed,
                        inconsistent_rounds: 0,
                    },
                );
                workers.push(Worker {
                    cmd_pid,
                    result_rx,
                    kind: WorkerKind::Command { session_id },
                });
                log::info!("[diag] data {port_index}: session_id={session_id} worker thread spawned"
                );
            }
            other => {
                log::error!("[diag] data {port_index}: expected Spawn, got {other:?}");
            }
        },
        DataPortState::Running { session_id } => match message {
            ClientMessage::Hello { version, .. } => {
                // The host reused this port before the previous long-lived
                // process exited (e.g. an LSP the host had abandoned). Force
                // the old session down so the new handshake can proceed without
                // reading stale bytes from the old process's stdio.
                log::warn!(
                    "[diag] data {port_index}: Hello while Running session {session_id}, force-cleaning"
                );
                if let Some(session) = sessions.get(&session_id) {
                    let pid = session.cmd_pid.load(Ordering::SeqCst);
                    if pid > 0 {
                        // SIGKILL the whole process group so the old worker
                        // stops forwarding and exits promptly.
                        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                        log::info!("[diag] data {port_index}: killed old process group {pid}");
                    }
                }
                let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
                    return true;
                };
                // Replay the new Hello once the port returns to Handshake.
                let mut hello_bytes = Vec::new();
                if queue_frame(&mut hello_bytes, &ClientMessage::Hello { version }) {
                    port.pending_hello = Some(hello_bytes);
                }
                // Drop any bytes buffered from the old session so the new
                // handshake never parses stale output.
                port.pending.clear();
                port.state = DataPortState::CleanupPending { old_session_id: session_id };
            }
            _ => {
                log::warn!("[diag] data {port_index}: frame while Running, ignoring");
            }
        },
        DataPortState::CleanupPending { .. } => match message {
            // The host reused the port while this side is still tearing down the
            // old process. Hold the new Hello; once the worker exits and the
            // port returns to Handshake it is replayed, so the host's handshake
            // is never lost.
            ClientMessage::Hello { version, .. } => {
                log::warn!("[diag] data {port_index}: Hello while CleanupPending, holding");
                let Some(port) = data_ports.get_mut(port_index).and_then(|p| p.as_mut()) else {
                    return true;
                };
                let mut hello_bytes = Vec::new();
                if queue_frame(&mut hello_bytes, &ClientMessage::Hello { version }) {
                    port.pending_hello = Some(hello_bytes);
                }
                port.pending.clear();
            }
            _ => {
                log::warn!("[diag] data {port_index}: frame while CleanupPending, ignoring");
            }
        },
    }
    // A frame was parsed and handled.
    true
}

/// Flips a session's data/err ports back to handshake so a reused host
/// connection starts a fresh protocol exchange. The fds themselves are long
/// lived (reopening would drop the QEMU chardev link), only the states change.
fn reclaim_ports(
    data_index: usize,
    err_index: Option<usize>,
    data_ports: &mut Vec<Option<DataPort>>,
    err_ports: &mut Vec<Option<ErrPort>>,
) {
    if let Some(port) = data_ports.get_mut(data_index).and_then(|p| p.as_mut()) {
        port.state = DataPortState::Handshake;
    }
    if let Some(err_index) = err_index {
        if let Some(port) = err_ports.get_mut(err_index).and_then(|p| p.as_mut()) {
            port.handed_over = false;
        }
    }
}

/// Kills a session the host has abandoned (per heartbeat reconciliation) and
/// parks its data port in CleanupPending: the worker exit recycles the port to
/// Handshake and clears its buffers, so a reused connection never serves stale
/// bytes to the next command. Killing is a fast local SIGKILL, so this runs on
/// the event loop without blocking; the asynchronous worker teardown happens on
/// the worker thread.
fn cleanup_stale_session(
    session_id: u64,
    sessions: &mut HashMap<u64, Session>,
    data_ports: &mut Vec<Option<DataPort>>,
    err_ports: &mut Vec<Option<ErrPort>>,
) {
    let Some(session) = sessions.get(&session_id) else {
        return;
    };
    let pid = session.cmd_pid.load(Ordering::SeqCst);
    if pid > 0 {
        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
        log::warn!("[diag] reconcile: killed stale session {session_id} pid {pid}");
    }
    if let Some(port) = data_ports.get_mut(session.data_index).and_then(|p| p.as_mut()) {
        port.pending_hello = None;
        port.pending.clear();
        port.state = DataPortState::CleanupPending { old_session_id: session_id };
    }
    if let Some(err_index) = session.err_index {
        if let Some(err_port) = err_ports.get_mut(err_index).and_then(|p| p.as_mut()) {
            err_port.pending.clear();
            err_port.handed_over = false;
        }
    }
    log::warn!("[diag] reconcile: session {session_id} parked for cleanup");
}

/// Force-reclaims sessions whose ExecResultAck did not arrive before the ack
/// deadline, so a hung or crashed host never leaks ports.
fn reclaim_expired_acks(
    pending_ack: &mut HashMap<u64, PendingAck>,
    data_ports: &mut Vec<Option<DataPort>>,
    err_ports: &mut Vec<Option<ErrPort>>,
) {
    let now = std::time::Instant::now();
    let expired: Vec<u64> = pending_ack
        .iter()
        .filter(|(_, ack)| now >= ack.deadline)
        .map(|(session_id, _)| *session_id)
        .collect();
    for session_id in expired {
        if let Some(ack) = pending_ack.remove(&session_id) {
            reclaim_ports(ack.data_index, ack.err_index, data_ports, err_ports);
            log::warn!("[diag] reclaim_expired_acks: session_id={session_id} forced (ack timeout)");
        }
    }
}

/// A worker exited: read its result, reap it, report and reclaim its ports.
fn handle_worker_exit(
    worker_index: usize,
    workers: &mut Vec<Worker>,
    sessions: &mut HashMap<u64, Session>,
    pending_ack: &mut HashMap<u64, PendingAck>,
    data_ports: &mut Vec<Option<DataPort>>,
    err_ports: &mut Vec<Option<ErrPort>>,
    mgmt_write_pending: &mut Vec<u8>,
    path_map: &Arc<Mutex<PathMap>>,
) {
    let worker = workers.remove(worker_index);
    let cmd_pid = worker.cmd_pid.load(Ordering::SeqCst);
    log::info!("[diag] worker exit: cmd_pid={cmd_pid} kind={:?}",
        std::mem::discriminant(&worker.kind)
    );
    match worker.kind {
        WorkerKind::Command { session_id } => {
            let exit_code = read_command_result(worker.result_rx);
            // The command process was reaped by the worker thread (child.wait);
            // the worker is a thread now, so there is no process to reap here.
            close_fd(worker.result_rx);
            // Queue the ExecResult instead of writing synchronously (P1-3): a
            // full mgmt connection no longer parks the event loop, and if the
            // connection is currently down the frame stays buffered and is
            // replayed when it reconnects (P0-2), instead of being lost.
            let sent = queue_frame(
                mgmt_write_pending,
                &ServerMessage::ExecResult { session_id, exit_code },
            );
            if sent {
                log::info!("[diag] worker exit: ExecResult queued session_id={session_id} exit_code={exit_code:?}");
            } else {
                log::error!("[diag] worker exit: serialize ExecResult failed session_id={session_id}");
            }
            let Some(session) = sessions.remove(&session_id) else {
                log::warn!("[diag] worker exit: session_id={session_id} already gone");
                return;
            };
            // If the port is in CleanupPending, the host already reused it with
            // a fresh Hello while the old process was still running. Recycle the
            // port immediately (no ExecResultAck wait -- the host is not tracking
            // this session anymore), replay the held Hello, and clear the old
            // session's buffered bytes so the new handshake reads clean frames.
            let cleanup_pending = data_ports
                .get(session.data_index)
                .and_then(|p| p.as_ref())
                .is_some_and(|p| matches!(p.state, DataPortState::CleanupPending { .. }));
            if cleanup_pending {
                if let Some(port) = data_ports.get_mut(session.data_index).and_then(|p| p.as_mut()) {
                    if let Some(hello) = port.pending_hello.take() {
                        port.pending = hello;
                    } else {
                        port.pending.clear();
                    }
                    port.state = DataPortState::Handshake;
                }
                if let Some(err_index) = session.err_index {
                    if let Some(err_port) = err_ports.get_mut(err_index).and_then(|p| p.as_mut()) {
                        err_port.pending.clear();
                        err_port.handed_over = false;
                    }
                }
                log::warn!("[diag] worker exit: session {session_id} cleanup-pending, port recycled to handshake");
            } else if sent {
                // Two-phase reclaim: hold the ports until the host's
                // ExecResultAck (or the ack timeout) so a reused data
                // connection never carries stale bytes into the next
                // handshake. If the ExecResult itself could not be written the
                // host will never ack, so reclaim the ports immediately.
                pending_ack.insert(
                    session_id,
                    PendingAck {
                        data_index: session.data_index,
                        err_index: session.err_index,
                        deadline: std::time::Instant::now() + ACK_TIMEOUT,
                    },
                );
                log::info!("[diag] worker exit: session_id={session_id} awaiting ExecResultAck");
            } else {
                reclaim_ports(session.data_index, session.err_index, data_ports, err_ports);
                log::info!("[diag] worker exit: session_id={session_id} reclaimed immediately (ExecResult not sent)");
            }
        }
        WorkerKind::Mount {
            uri,
            guest_path,
            mount_tag,
        } => {
            let ok = read_mount_result(worker.result_rx);
            // Mount worker is a thread; nothing to reap.
            close_fd(worker.result_rx);
            if ok {
                path_map
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .add(RootMap {
                        host_root: uri.clone(),
                        guest_root: guest_path.clone(),
                    });
                if !queue_frame(mgmt_write_pending, &ServerMessage::MountOk { uri }) {
                    log::warn!("[diag] worker exit: serialize MountOk failed");
                }
                log::info!("[diag] worker exit: mounted {mount_tag} -> {guest_path}");
            } else {
                if !queue_frame(
                    mgmt_write_pending,
                    &ServerMessage::Error {
                        session_id: None,
                        message: format!("mount {mount_tag} failed"),
                    },
                ) {
                    log::warn!("[diag] worker exit: serialize mount Error failed");
                }
                log::error!("[diag] worker exit: mount {mount_tag} failed");
            }
        }
    }
}

/// Spawns a command worker thread. The thread builds and spawns the concrete
/// command process (glibc posix_spawn, no full-mm fork), forwards stdio and
/// reports the exit code through the result pipe. Returns
/// (cmd_pid, result_rx, start_w), or None if a pipe or the thread could not be
/// created. `cmd_pid` is written by the worker thread once the command is
/// spawned; the event loop never blocks on it.
fn spawn_command_worker(
    data_fd: i32,
    err_fd: Option<i32>,
    spec: qemu_cmd_agent_protocol::messages::ExecSpec,
    path_map: PathMap,
    session_id: u64,
) -> Option<(Arc<AtomicI32>, i32, i32, Arc<AtomicBool>)> {
    let mut result_pipe = [0i32; 2];
    let mut start_pipe = [0i32; 2];
    // SAFETY: pipe(2) for worker result and start signalling.
    if unsafe { libc::pipe(result_pipe.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(start_pipe.as_mut_ptr()) } != 0
    {
        log::error!("[diag] spawn_command_worker: pipe failed: {}", std::io::Error::last_os_error());
        return None;
    }
    // The worker thread runs in the same process, so the event loop and the
    // thread share ONE fd table: closing a fd number in one thread closes it
    // for the other. Give the worker its own copies (dup) of the pipe ends it
    // uses, then close the originals; the event loop keeps its own ends
    // (result read + start write) for polling and notify.
    let result_tx = unsafe { libc::dup(result_pipe[1]) };
    let start_rx = unsafe { libc::dup(start_pipe[0]) };
    if result_tx < 0 || start_rx < 0 {
        log::error!("[diag] spawn_command_worker: dup failed: {}",
            std::io::Error::last_os_error()
        );
        unsafe { libc::close(result_pipe[0]) };
        unsafe { libc::close(result_pipe[1]) };
        unsafe { libc::close(start_pipe[0]) };
        unsafe { libc::close(start_pipe[1]) };
        if result_tx >= 0 {
            unsafe { libc::close(result_tx) };
        }
        if start_rx >= 0 {
            unsafe { libc::close(start_rx) };
        }
        return None;
    }
    // The worker owns result_tx + start_rx (dup copies); the event loop owns
    // result_pipe[0] + start_pipe[1] (originals). Drop the originals of the
    // ends the worker's dup copies replaced.
    unsafe { libc::close(result_pipe[1]) };
    unsafe { libc::close(start_pipe[0]) };
    let cmd_pid = Arc::new(AtomicI32::new(-1));
    let cmd_pid_thread = cmd_pid.clone();
    let stdin_closed = Arc::new(AtomicBool::new(false));
    let stdin_closed_thread = stdin_closed.clone();
    let builder = std::thread::Builder::new().name(format!("cmd-worker-{session_id}"));
    let spawned = builder.spawn(move || {
        // Worker thread: use the dup'd result/start ends. It never closes the
        // event loop's fds (threads share one fd table).
        exec::worker_run_command(
            data_fd,
            err_fd,
            spec,
            path_map,
            start_rx,
            result_tx,
            cmd_pid_thread,
            stdin_closed_thread,
        );
    });
    match spawned {
        Ok(_) => {
            log::info!("[diag] spawn_command_worker: session_id={session_id} thread spawned");
            Some((cmd_pid, result_pipe[0], start_pipe[1], stdin_closed))
        }
        Err(err) => {
            log::error!("[diag] spawn_command_worker: thread spawn failed: {err}");
            unsafe { libc::close(result_pipe[0]) };
            unsafe { libc::close(result_pipe[1]) };
            unsafe { libc::close(start_pipe[0]) };
            unsafe { libc::close(start_pipe[1]) };
            unsafe { libc::close(result_tx) };
            unsafe { libc::close(start_rx) };
            None
        }
    }
}

/// Spawns a mount worker thread. Returns the result pipe read end, or -1 if
/// the pipe or the thread could not be created.
fn spawn_mount_worker(mount_tag: &str, guest_path: &str) -> i32 {
    let mut result_pipe = [0i32; 2];
    // SAFETY: pipe(2) for the mount result.
    if unsafe { libc::pipe(result_pipe.as_mut_ptr()) } != 0 {
        log::error!("[diag] spawn_mount_worker: pipe failed: {}", std::io::Error::last_os_error());
        return -1;
    }
    // Same fd-table reasoning as spawn_command_worker: dup the write end for
    // the worker thread so the event loop can close the original without
    // invalidating the thread's fd.
    let result_tx = unsafe { libc::dup(result_pipe[1]) };
    if result_tx < 0 {
        log::error!("[diag] spawn_mount_worker: dup failed: {}", std::io::Error::last_os_error());
        unsafe { libc::close(result_pipe[0]) };
        unsafe { libc::close(result_pipe[1]) };
        return -1;
    }
    unsafe { libc::close(result_pipe[1]) };
    let mount_tag = mount_tag.to_string();
    let guest_path = guest_path.to_string();
    let builder = std::thread::Builder::new().name("mount-worker".to_string());
    let spawned = builder.spawn(move || {
        exec::worker_run_mount(&mount_tag, &guest_path, result_tx);
    });
    match spawned {
        Ok(_) => {
            log::info!("[diag] spawn_mount_worker: thread spawned");
            result_pipe[0]
        }
        Err(err) => {
            log::error!("[diag] spawn_mount_worker: thread spawn failed: {err}");
            unsafe { libc::close(result_pipe[0]) };
            unsafe { libc::close(result_tx) };
            -1
        }
    }
}

/// Tells the command worker that SpawnOk was written and it may start
/// forwarding output.
fn notify_start(start_w: i32) {
    if start_w < 0 {
        return;
    }
    log::info!("[diag] notify_start: signalling worker (fd {start_w})");
    let byte = [0u8];
    // SAFETY: write a single byte to the non-blocking start pipe.
    let rc = unsafe { libc::write(start_w, byte.as_ptr() as *const libc::c_void, 1) };
    if rc != 1 {
        // The worker may already have exited (EPIPE); its result pipe still
        // surfaces the outcome, so a failed start notification is non-fatal.
        log::warn!(
            "[diag] notify_start: write to fd {start_w}: {}",
            std::io::Error::last_os_error()
        );
    }
    unsafe { libc::close(start_w) };
}

/// Reads the 4-byte exit code from a command worker's result pipe.
fn read_command_result(result_rx: i32) -> Option<i32> {
    let mut bytes = [0u8; 4];
    let n = unsafe { libc::read(result_rx, bytes.as_mut_ptr() as *mut libc::c_void, 4) };
    if n == 4 {
        let code = i32::from_le_bytes(bytes);
        log::info!("[diag] read_command_result: n={n} code={code}");
        Some(code)
    } else {
        log::warn!("[diag] read_command_result: short read n={n}");
        Some(UNKNOWN_EXIT)
    }
}

/// Reads the 1-byte mount result from a mount worker's result pipe.
fn read_mount_result(result_rx: i32) -> bool {
    let mut byte = [0u8; 1];
    let n = unsafe { libc::read(result_rx, byte.as_mut_ptr() as *mut libc::c_void, 1) };
    log::info!(
        "[diag] read_mount_result: byte={} ok={}",
        byte[0],
        n == 1 && byte[0] == 0
    );
    n == 1 && byte[0] == 0
}

fn close_fd(fd: i32) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

/// Reopens an err port device.
fn reopen_err_port(index: usize, err_ports: &mut Vec<Option<ErrPort>>) {
    if let Some(port) = err_ports.get_mut(index).and_then(|p| p.as_mut()) {
        port.fd = open_port(&port.dev).ok();
        port.handed_over = false;
        if port.fd.is_none() {
            log::warn!("[diag] reopen_err_port {index}: open failed");
        }
    }
}
