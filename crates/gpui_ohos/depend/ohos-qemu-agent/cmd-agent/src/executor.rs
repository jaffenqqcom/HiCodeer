//! Host-side `RemoteCommandExecutor` for the QEMU cmd-agent channel.
//!
//! zcoder's `util::command` calls this through `cmd-agent-linker`. Each spawn
//! allocates a data/err port pair from the pool QEMU created, handshakes over
//! the corresponding unix sockets, then hands the (async-wrapped) sockets back
//! as the child's stdio. Exit results arrive on the dedicated management
//! socket, which a background thread funnels into a shared table so
//! `try_exit` / `wait_exit_async` never block the caller's thread.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::task::{Context, Poll, Waker};

use async_io::Async;
use qemu_cmd_agent_linker::RemoteChild;
use qemu_cmd_agent_protocol::frame;
use qemu_cmd_agent_protocol::messages::{
    self, ClientMessage, ExecSpec, FdMode, ServerMessage, Signal,
};
use smol::io::{AsyncRead, AsyncWrite};

use crate::qmp;
#[cfg(target_env = "ohos")]
use crate::virtiofs;
use crate::{MOUNT_TAG_SANDBOX, PORT_POOL_SIZE, QMP_SOCKET, WORKDIR_MOUNT_SLOTS};

/// Management socket file name under the port dir.
const MGMT_SOCKET: &str = "mgmt.sock";
/// Data socket file name prefix (`cmd.<n>.sock`).
const DATA_SOCKET_PREFIX: &str = "cmd.";
/// stderr socket file name prefix (`err.<n>.sock`).
const ERR_SOCKET_PREFIX: &str = "err.";
/// Guest-side mount point of the sandbox root.
const GUEST_SANDBOX_PATH: &str = "/sandbox";
/// Prefix for dynamically hotplugged vhost-user-fs device ids.
const DEVICE_PREFIX: &str = "virtiofs";
/// Prefix for the per-work-dir chardev that connects QEMU to the virtiofsd
/// backend socket (created via QMP chardev-add).
const CHARDEV_PREFIX: &str = "vfwork";
/// Prefix for work-directory mount tags (the guest mounts `mount -t virtiofs
/// <tag>`).
const MOUNT_TAG_PREFIX: &str = "ztag";
/// Guest mount points of work directories mirror the device path exactly (the
/// same path is mounted, so the guest sees identical paths). Keeping them
/// identical means LSP index caches (e.g. clangd) stay valid across restarts:
/// a numbered mount point would change every boot and invalidate cached paths.
/// Bound on the spawn handshake read, so a silent cmd-agentd never blocks the
/// caller's thread forever (LSP `which` runs on the UI thread). The guest event
/// loop can lag processing a Hello by ~1s (it may be busy with a mount worker
/// first), so this stays generous enough to cover that lag.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Bound on the MountOk wait inside run_mount. mount_folder is called
/// synchronously from workspace open, so a mount that never completes must
/// fail (bounded) rather than block the caller forever.
const MOUNT_OK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Read bound used for the sandbox mount handshake at startup. The guest's
/// first reply after cold boot can exceed the 2s management read timeout
/// (cmd-agentd is still initializing), which would leave the sandbox MountOk
/// unread on the socket and trip the next work-directory mount's ack check.
/// Long enough to cover a slow first boot, short enough to bound startup.
const SANDBOX_MOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Heartbeat interval on the management connection.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// Faster heartbeat used while a session reconciliation is in progress, so a
/// stale session is confirmed and cleaned up quickly.
const HEARTBEAT_INTERVAL_RECONCILING: std::time::Duration = std::time::Duration::from_secs(5);
/// Consecutive reconciliations where a session is missing on the peer before
/// it is cleaned up. Two rounds absorb the normal transient mismatch (host
/// removes a session on ExecResult, guest only on ExecResultAck).
const RECONCILE_THRESHOLD: u32 = 2;
/// poll() timeout: bounds reconnect latency while the guest is coming up and
/// lets the management loop re-check deferred port releases every second.
/// Heartbeats are still gated by HEARTBEAT_INTERVAL, not by this timeout.
const HEARTBEAT_POLL_MILLIS: i32 = 1000;
/// Heartbeats without any inbound activity before the guest agent is judged
/// lost and QEMU is restarted.
const HEARTBEAT_MISS_LIMIT: u32 = 3;
/// [diag] How long a background-thread spawn waits for the guest agent to
/// become ready (management handshake done) before giving up. UI-thread spawns
/// fail immediately instead, so the interface never blocks on a booting guest.
const READY_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// [diag] How long a spawn waits after readiness for deferred work-directory
/// mounts to finish, so git/LSP find the mapped cwd present in the guest.
const DEFERRED_MOUNT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Consecutive Pending polls tolerated after the exit signal before the
/// EOF-on-exit stream reports EOF. A single tolerated Pending was not enough:
/// trailing stdout/stderr split across several virtio-serial deliveries can be
/// followed by another Pending and then one more batch, and reporting EOF on
/// the first Pending dropped that final batch (P2-6). Three covers a few
/// lagging deliveries while still terminating a truly output-less exit.
const EXIT_EOF_GRACE_POLLS: u8 = 3;
/// How long a port may sit in `pending_release` before it is force-released
/// even though its stdio readers were never dropped. A caller that holds a
/// stream and never reads to EOF would otherwise pin the port forever; after
/// the deadline the connection is drained (the guest already stopped writing
/// once ExecResult was sent) and the port is freed with a warning (P1-6).
const PENDING_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Upper bound on a single mgmt frame payload (mirrors the protocol crate's
/// bound), guarding the persistent buffer against a corrupt length prefix.
const MAX_FRAME_SIZE: usize = 256 * 1024 * 1024;

/// Requests the management thread to forward over its single long-lived mgmt
/// connection. Signals, mounts and unmounts all reuse that one connection:
/// QEMU's socket chardev accepts exactly one client, so a second connection
/// would sit in the accept backlog unread. A mount request carries a reply
/// channel so the caller waits for MountOk (or an Error).
enum MgmtCommand {
    Signal {
        session_id: u64,
        signal: Signal,
    },
    /// Flush a StdinEof frame over the management connection once the client
    /// dropped a session's stdin writer. The data connection cannot signal EOF
    /// (half-closing the shared stdio socket would poison the reusable port),
    /// so stdin closure is reported out-of-band.
    StdinEof {
        session_id: u64,
    },
    Mount {
        uri: String,
        mount_tag: String,
        guest_path: String,
        reply: mpsc::SyncSender<std::io::Result<()>>,
    },
    Unmount {
        uri: String,
    },
}

/// Per-session exit state, shared between the management thread and waiters.
struct SessionState {
    exit_code: Option<Option<i32>>,
    waiters: Vec<smol::channel::Sender<Option<i32>>>,
    /// Index of the data/err port pair this session holds, so the management
    /// thread can release it back into the pool when ExecResult arrives.
    port_index: usize,
    /// Set when ExecResult arrives; the EOF-on-exit stdio streams observe this
    /// to stop reading, drain their connection and report EOF.
    exited: Arc<AtomicBool>,
    /// Wakes readers parked on an output-less command when it exits.
    wake: Arc<WakeSignal>,
    /// Consecutive heartbeat reconciliations where this session was missing on
    /// the guest side. Reaching RECONCILE_THRESHOLD means the guest has torn it
    /// down, so this side releases its port (long-lived processes such as LSPs
    /// are never reported missing while they keep running).
    inconsistent_rounds: u32,
}

/// One data/err port pair with its persistent host connections. The
/// connections are created once and reused across commands: in this QEMU
/// guest a virtio-serial port only delivers data on its first host connection
/// (a disconnect poisons it), so reconnecting per command loses the handshake.
/// A command borrows a `try_clone` of the connection; the slot keeps the
/// original so it survives the command and is reused by the next one.
struct PortSlot {
    busy: bool,
    data: Option<UnixStream>,
    err: Option<UnixStream>,
    /// Set by the stdout EOF-on-exit stream once the data connection has been
    /// fully drained. The port is released only once BOTH the data and the err
    /// connection are clean, so a reused connection never carries stale bytes
    /// into the next handshake.
    data_drained: Arc<AtomicBool>,
    /// Set by the stderr EOF-on-exit stream once the err connection has been
    /// fully drained.
    err_drained: Arc<AtomicBool>,
    /// Weak ref to the stdout reader holder, detecting when it was dropped
    /// without being read to EOF.
    data_holder: Weak<()>,
    /// Weak ref to the stderr reader holder.
    err_holder: Weak<()>,
}

struct State {
    sessions: Mutex<HashMap<u64, SessionState>>,
    ports: Mutex<Vec<PortSlot>>,
    pending_release: Mutex<Vec<(usize, std::time::Instant)>>,
    /// [diag] Set once the guest agent's management handshake succeeds, cleared
    /// on connection loss. Guards UI-thread spawns against blocking on a guest
    /// that is still booting; background-thread spawns wait on it instead.
    ready: AtomicBool,
    /// True once the sandbox root mount completed after the last (re)connect.
    /// Deferred work-directory mounts wait on `ready && sandbox_mounted` so
    /// they never race the sandbox mount for the mgmt socket's MountOk and
    /// produce an ack mismatch.
    sandbox_mounted: AtomicBool,
}

/// The stdin writer handed to callers for a session. It forwards writes to the
/// data connection (stdin bytes travel over the same socket as stdout), and on
/// drop asks the management thread to flush a `StdinEof` frame over the
/// management connection. The guest closes the child's stdin pipe on that
/// frame, letting a resident `cat-file --batch` see EOF and exit; without it
/// the process waits on stdin forever because the data socket is never
/// half-closed (that would poison the reusable port).
struct StdinSignalWriter {
    inner: smol::Async<std::os::unix::net::UnixStream>,
    session_id: u64,
    cmd_tx: mpsc::Sender<MgmtCommand>,
    /// eventfd that wakes the management thread so it drains `cmd_tx` and
    /// flushes the StdinEof frame. A send alone is not enough: the thread only
    /// reads the queue after this eventfd fires.
    cmd_wake: i32,
}

impl smol::io::AsyncWrite for StdinSignalWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // StdinSignalWriter is Unpin, so `Pin::get_mut` yields a plain `&mut`
        // and the inner Async (also Unpin) can be polled directly.
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

impl Drop for StdinSignalWriter {
    fn drop(&mut self) {
        let session_id = self.session_id;
        log::info!("[diag] StdinSignalWriter::drop: session_id={session_id} signalling stdin eof");
        // The management command queue is unbounded (std::sync::mpsc::channel),
        // so send() never blocks on capacity; it only fails if the management
        // thread has gone away, in which case the guest is gone too.
        if let Err(err) = self
            .cmd_tx
            .send(MgmtCommand::StdinEof { session_id })
        {
            log::warn!("[diag] StdinSignalWriter::drop: queue StdinEof: {err}");
        } else {
            // Wake the management thread so it drains the queue promptly; the
            // queue is only read after this eventfd fires.
            wake_eventfd(self.cmd_wake);
        }
    }
}

/// A command-exit wake-up signal. When a reader is parked waiting for data
/// that never arrives (a command that produces no output), the connection's
/// epoll never fires and the reader would hang forever, keeping its Async
/// registration on the shared connection and stealing the next command's
/// handshake frames. The management thread calls `fire()` when ExecResult
/// arrives, waking the parked reader so it drains and releases the connection.
struct WakeSignal {
    fired: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl WakeSignal {
    fn new() -> Self {
        Self {
            fired: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    fn register(&self, waker: &Waker) {
        let mut guard = self.waker.lock().unwrap_or_else(|e| e.into_inner());
        if self.fired.load(Ordering::SeqCst) {
            waker.wake_by_ref();
        } else {
            *guard = Some(waker.clone());
        }
    }

    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        if let Some(w) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            w.wake();
        }
    }
}

/// AsyncRead wrapper that turns the long-lived connection into an EOF-on-exit
/// stream. The virtio-serial port fd never closes (the guest keeps it open for
/// reuse), so a plain read_to_end would hang forever; this wrapper stops
/// reading once the command's ExecResult arrives (`exited`), drains whatever is
/// still buffered (the guest flushed everything before ExecResult) and then
/// reports EOF. It also flags the port as `drained` so the management thread
/// releases it only after the connection is truly clean. A `WakeSignal` makes
/// sure a reader parked on an output-less command is woken when the command
/// exits, so it releases the connection promptly.
struct ExitEofStream {
    inner: Async<UnixStream>,
    exited: Arc<AtomicBool>,
    drained: Arc<AtomicBool>,
    /// Wakes a reader parked waiting for data that never arrives.
    wake: Arc<WakeSignal>,
    /// Keeps this side's (stdout or stderr) reader holder alive while a reader
    /// exists; the management thread uses the Weak to detect when readers are
    /// gone.
    _holder: Arc<()>,
    eof_sent: bool,
    /// After the exit signal, this many consecutive Pending polls are
    /// tolerated before EOF so virtio can drain trailing bytes that were
    /// written just before the worker exited, possibly in several batches.
    /// Data arriving at any point resets the counter (P2-6).
    exited_pending_count: u8,
    /// [diag] Label of the command this stream forwards stdout for, used to
    /// report how many stdout bytes the guest actually produced. `None` skips
    /// the report (only git commands are tagged).
    diag_label: Option<String>,
    /// [diag] Running count of stdout bytes read from the guest.
    diag_total: usize,
}

impl ExitEofStream {
    fn poll_exited(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.eof_sent {
            return Poll::Ready(Ok(0));
        }
        // Command exited: drain whatever is still buffered (the guest
        // forwarded all output before sending ExecResult), then EOF. The
        // Async fd is non-blocking, so a Pending poll means the connection is
        // fully drained. One Pending round is tolerated before EOF so virtio
        // can deliver trailing bytes written just before the worker exited.
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(n)) if n > 0 => {
                // Data arrived: reset the pending counter so the next chunk
                // also gets its grace polls before EOF.
                self.exited_pending_count = 0;
                Poll::Ready(Ok(n))
            }
            Poll::Pending => {
                if self.exited_pending_count < EXIT_EOF_GRACE_POLLS {
                    self.exited_pending_count += 1;
                    // Keep the wake registered so a reader parked on an
                    // output-less command is still released on the next round.
                    self.wake.register(cx.waker());
                    Poll::Pending
                } else {
                    self.eof_sent = true;
                    self.drained.store(true, Ordering::SeqCst);
                    self.report_diag_total();
                    Poll::Ready(Ok(0))
                }
            }
            _ => {
                self.eof_sent = true;
                self.drained.store(true, Ordering::SeqCst);
                self.report_diag_total();
                Poll::Ready(Ok(0))
            }
        }
    }

    /// [diag] Reports how many stdout bytes the tagged command produced.
    fn report_diag_total(&self) {
        if let Some(label) = &self.diag_label {
            log::info!("[diag] stdout '{label}': total {} bytes", self.diag_total);
        }
    }
}

impl AsyncRead for ExitEofStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.eof_sent {
            return Poll::Ready(Ok(0));
        }
        if self.exited.load(Ordering::SeqCst) {
            return self.get_mut().poll_exited(cx, buf);
        }
        let me = self.get_mut();
        match Pin::new(&mut me.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(n)) if n > 0 => {
                me.diag_total += n;
                Poll::Ready(Ok(n))
            }
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                // Parked on a connection that may never produce data (a
                // command with no output) and never EOF (long-lived). Register
                // a wake so the exit signal releases us; otherwise this stale
                // reader stays registered on the connection and steals the
                // next command's handshake frames.
                me.wake.register(cx.waker());
                // Re-check: the exit may have fired while we registered.
                if me.exited.load(Ordering::SeqCst) {
                    me.poll_exited(cx, buf)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// The command executor registered as the `RemoteCommandExecutor`.
pub struct QemuCommandExecutor {
    port_dir: PathBuf,
    state: Arc<State>,
    next_session: AtomicU64,
    /// Requests queued here and flushed by the management thread over its
    /// long-lived management connection. A fresh connection to the mgmt socket
    /// would never be served: QEMU's socket chardev accepts exactly one client,
    /// so a second connect would sit in the accept backlog unread.
    cmd_tx: mpsc::Sender<MgmtCommand>,
    /// eventfd woken after every `cmd_tx` send, so the management thread can
    /// block in poll instead of busy-polling the queue on a timeout.
    cmd_wake: i32,
    /// QMP socket for runtime fsdev-add / device_add of work directories.
    qmp_socket: PathBuf,
    /// Folders already mounted, so re-opening a folder is a no-op. `Arc` so a
    /// background mount thread can update it without borrowing the executor.
    mounted: Arc<Mutex<HashSet<String>>>,
    /// Mount sequence shared by fsdev id / device id / mount tag / hotplug bus.
    mount_counter: Arc<AtomicU64>,
    /// Whether the executor accepts new spawns; cleared while a lost guest is
    /// being restarted, restored when the management connection is back.
    available: Arc<AtomicBool>,
    /// [diag] Work directories whose mount was deferred to a background thread
    /// because the guest agent was still booting. Spawn waits for this to empty
    /// after readiness, so git/LSP find the mapped cwd present in the guest.
    deferred_mounts: Arc<Mutex<Vec<String>>>,
}

impl QemuCommandExecutor {
    /// Connects to the port pool and starts the management thread that
    /// collects `ExecResult`s. `port_dir` is QEMU's virtio-serial socket dir.
    /// When `sandbox_root` is set, the management thread mounts it through
    /// `MountFolder2QEMU` once the guest agent is up, so the guest's path
    /// mapping table gets the fixed host-root -> /sandbox entry.
    pub fn new(port_dir: PathBuf, sandbox_root: Option<String>) -> std::io::Result<Self> {
        log::info!("[diag] QemuCommandExecutor::new: port_dir={} sandbox_root={:?}",
            port_dir.display(),
            sandbox_root
        );
        let state = Arc::new(State {
            sessions: Mutex::new(HashMap::new()),
            ports: Mutex::new(
                (0..PORT_POOL_SIZE)
                    .map(|_| PortSlot {
                        busy: false,
                        data: None,
                        err: None,
                        data_drained: Arc::new(AtomicBool::new(false)),
                        err_drained: Arc::new(AtomicBool::new(false)),
                        data_holder: Weak::new(),
                        err_holder: Weak::new(),
                    })
                    .collect(),
            ),
            pending_release: Mutex::new(Vec::new()),
            ready: AtomicBool::new(false),
            sandbox_mounted: AtomicBool::new(false),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel();
        // eventfd woken on every queue send, letting the management thread
        // block in poll (socket + eventfd) instead of poll-with-timeout to
        // flush the queue.
        // SAFETY: eventfd(2) with CLOEXEC|NONBLOCK; non-zero on success.
        let cmd_wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if cmd_wake < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let available = Arc::new(AtomicBool::new(true));
        let mounted = Arc::new(Mutex::new(HashSet::new()));
        let mount_counter = Arc::new(AtomicU64::new(1));
        let mgmt_dir = port_dir.clone();
        let mgmt_state = state.clone();
        let mgmt_available = available.clone();
        let mgmt_mounted = mounted.clone();
        let mgmt_mount_counter = mount_counter.clone();
        std::thread::Builder::new()
            .name("qemu-cmd-mgmt".to_string())
            .spawn(move || {
                mgmt_loop(
                    mgmt_dir,
                    mgmt_state,
                    sandbox_root,
                    cmd_rx,
                    cmd_wake,
                    mgmt_available,
                    mgmt_mounted,
                    mgmt_mount_counter,
                )
            })
            .map_err(std::io::Error::other)?;
        let qmp_socket = port_dir.join(QMP_SOCKET);
        Ok(Self {
            port_dir,
            state,
            next_session: AtomicU64::new(1),
            cmd_tx,
            cmd_wake,
            qmp_socket,
            mounted,
            mount_counter,
            available,
            deferred_mounts: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Reserves a free data/err port pair, returning its pool index. The
    /// persistent connections are created lazily in `acquire_connections`.
    fn alloc_ports(&self) -> std::io::Result<usize> {
        let mut ports = self.state.ports.lock().unwrap_or_else(|e| e.into_inner());
        for index in 0..PORT_POOL_SIZE {
            if !ports[index].busy {
                ports[index].busy = true;
                log::info!("[diag] alloc_ports: reserved port {index}");
                return Ok(index);
            }
        }
        log::error!("[diag] alloc_ports: no free cmd-agent port in pool");
        Err(std::io::Error::other("no free cmd-agent port in pool"))
    }

    /// Frees a port pair. When `reset` is true the persistent connections are
    /// dropped too (a failed handshake may have poisoned them); otherwise they
    /// are kept so the next command reuses the connection.
    fn release_ports(&self, index: usize, reset: bool) {
        let mut ports = self.state.ports.lock().unwrap_or_else(|e| e.into_inner());
        let slot = ports.get_mut(index).expect("port index in range");
        slot.busy = false;
        log::info!(
            "[diag] release_ports: index={index} data={} err={}",
            slot.data.is_some(),
            slot.err.is_some()
        );
        if reset {
            slot.data = None;
            slot.err = None;
            slot.data_drained = Arc::new(AtomicBool::new(false));
            slot.err_drained = Arc::new(AtomicBool::new(false));
            slot.data_holder = Weak::new();
            slot.err_holder = Weak::new();
        }
    }

    /// Returns clones of the persistent data/err connections for `index`,
    /// creating them on first use. The originals stay in the slot so the
    /// connection outlives the command and is reused by the next one.
    fn acquire_connections(
        &self,
        index: usize,
        want_err: bool,
    ) -> std::io::Result<(UnixStream, Option<UnixStream>)> {
        // Decide what needs connecting under the lock, then connect OUTSIDE it:
        // UnixStream::connect has no timeout, and a blocking connect while
        // holding the global ports lock would freeze every other spawn,
        // release_ports and the management thread (P1-4).
        let (need_data, need_err) = {
            let ports = self.state.ports.lock().unwrap_or_else(|e| e.into_inner());
            let slot = &ports[index];
            (slot.data.is_none(), want_err && slot.err.is_none())
        };
        let data_sock = self.port_dir.join(format!("{DATA_SOCKET_PREFIX}{index}.sock"));
        let new_data = if need_data {
            log::info!("[diag] acquire_connections: port {index} first data connect {data_sock:?}");
            Some(UnixStream::connect(&data_sock)?)
        } else {
            log::info!("[diag] acquire_connections: port {index} data socket reused");
            None
        };
        let err_sock = self.port_dir.join(format!("{ERR_SOCKET_PREFIX}{index}.sock"));
        let new_err = if need_err {
            log::info!("[diag] acquire_connections: port {index} first err connect {err_sock:?}");
            Some(UnixStream::connect(&err_sock)?)
        } else {
            None
        };
        // Write the fresh connections back under the lock. A port index is
        // never shared by two spawns concurrently (alloc_ports marks it busy),
        // so no other thread races us into the slot between the connect and
        // this write-back.
        {
            let mut ports = self.state.ports.lock().unwrap_or_else(|e| e.into_inner());
            let slot = &mut ports[index];
            if slot.data.is_none() {
                slot.data = new_data;
            }
            if want_err && slot.err.is_none() {
                slot.err = new_err;
            }
        }
        let ports = self.state.ports.lock().unwrap_or_else(|e| e.into_inner());
        let slot = &ports[index];
        let data = slot
            .data
            .as_ref()
            .expect("data connection set")
            .try_clone()?;
        let err = slot.err.as_ref().map(|s| s.try_clone()).transpose()?;
        Ok((data, err))
    }

    fn session_id(&self) -> u64 {
        self.next_session.fetch_add(1, Ordering::SeqCst)
    }

    /// [diag] Blocks the calling (background) thread until the guest agent's
    /// management handshake succeeds, up to `READY_WAIT_TIMEOUT`. Never called
    /// on the UI thread (spawn fails fast there instead).
    fn wait_ready(&self) {
        let deadline = std::time::Instant::now() + READY_WAIT_TIMEOUT;
        while !self.state.ready.load(Ordering::SeqCst) {
            if std::time::Instant::now() >= deadline {
                log::warn!("[diag] wait_ready: timed out after {READY_WAIT_TIMEOUT:?}");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        log::info!("[diag] wait_ready: guest agent ready");
    }

    /// [diag] After the guest agent is ready, waits for any deferred
    /// work-directory mounts to finish (bounded by `DEFERRED_MOUNT_WAIT`), so
    /// git/LSP commands see the mapped cwd present in the guest. Called only on
    /// the background path, never on the UI thread.
    fn wait_deferred_mounts(&self) {
        let deadline = std::time::Instant::now() + DEFERRED_MOUNT_WAIT;
        while !self
            .deferred_mounts
            .lock()
            .expect("deferred poisoned")
            .is_empty()
        {
            if std::time::Instant::now() >= deadline {
                log::warn!("[diag] wait_deferred_mounts: timed out, proceeding");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        log::info!("[diag] wait_deferred_mounts: no deferred mounts pending");
    }

    /// Wakes the management thread so it flushes queued commands immediately
    /// instead of waiting for the next socket event. Failing to wake is fine:
    /// the eventfd stays non-zero and the next poll round drains the queue.
    fn wake_cmd(&self) {
        wake_eventfd(self.cmd_wake);
    }

    /// Mounts a zcoder-opened folder into the guest: QMP fsdev-add + device_add
    /// create a fresh export for the real path, then MountFolder2QEMU makes
    /// cmd-agentd mount it and register the path mapping. Idempotent: re-mounting
    /// an already-mounted folder is a no-op.
    pub fn mount_folder(&self, path: &str) -> std::io::Result<()> {
        log::info!("[diag] QemuCommandExecutor::mount_folder: path={path} (sync)");
        if self
            .mounted
            .lock()
            .expect("mounted poisoned")
            .contains(path)
        {
            log::info!("[diag] QemuCommandExecutor::mount_folder: {path} already mounted");
            return Ok(());
        }
        // Run the full mount (QMP fsdev-add + device_add + MountFolder2QEMU +
        // MountOk wait) synchronously so the caller (workspace mount_opened_dirs,
        // awaited inside Workspace::new_local) returns only after the guest has
        // actually mounted the folder. Git/LSP then run against a folder that
        // exists in the guest. When the guest agent is still booting, the Mount
        // request is queued to the management thread and this blocks until it is
        // flushed (bounded by the management handshake retry).
        let path = path.to_string();
        let cmd_tx = self.cmd_tx.clone();
        let cmd_wake = self.cmd_wake;
        let qmp_socket = self.qmp_socket.to_string_lossy().into_owned();
        let mounted = self.mounted.clone();
        let mount_counter = self.mount_counter.clone();
        // [diag] If the guest agent is still booting, defer the mount to a
        // background thread: the workspace open path (UI thread) must not block
        // up to MOUNT_OK_TIMEOUT. Startup proceeds immediately; the thread waits
        // for the guest, then mounts, so git commands (which wait for readiness
        // plus the deferred-mounts drain) find the mapped cwd present in guest.
        if !self.state.ready.load(Ordering::SeqCst) {
            let state = self.state.clone();
            let deferred = self.deferred_mounts.clone();
            log::warn!(
                "[diag] mount_folder: guest agent not ready, deferring mount of {path} to background"
            );
            deferred.lock().unwrap_or_else(|e| e.into_inner()).push(path.clone());
            std::thread::Builder::new()
                .name("mount-deferred".to_string())
                .spawn(move || {
                    // Wait unconditionally for the guest agent to come up
                    // (`state.ready` flips once the mgmt handshake succeeds)
                    // AND the sandbox mount to finish. Guest boot time varies
                    // widely under TCG, so there is no fixed timeout here: when
                    // cmd-agent and cmd-agentd are connected and the sandbox
                    // MountOk has been consumed, run the mount the caller
                    // queued while the guest was still booting, instead of
                    // giving up and dropping it (which left the directory
                    // permanently unmounted when boot outlived the old 30s
                    // bound). Waiting for the sandbox also keeps this workdir
                    // mount from racing the sandbox mount for the mgmt
                    // socket's MountOk.
                    while !state.ready.load(Ordering::SeqCst)
                        || !state.sandbox_mounted.load(Ordering::SeqCst)
                    {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    if let Err(err) =
                        run_mount(&cmd_tx, cmd_wake, &qmp_socket, &mounted, &mount_counter, &path)
                    {
                        log::error!("[diag] mount_folder: deferred mount {path}: {err}");
                    }
                    deferred.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| p != &path);
                    log::info!("[diag] mount_folder: deferred mount {path} done");
                })
                .ok();
            return Ok(());
        }
        run_mount(&cmd_tx, cmd_wake, &qmp_socket, &mounted, &mount_counter, &path)
            .inspect_err(|err| log::error!("[diag] QemuCommandExecutor::mount_folder: {path}: {err}"))
    }

    /// Revokes the mapping for a mounted folder. Per DESIGN the guest mount
    /// point is deliberately left in place (an LSP may still be scanning it);
    /// only the mapping is removed. Fire-and-forget: the protocol has no ack
    /// for UnmountFolder2QEMU.
    pub fn unmount_folder(&self, path: &str) -> std::io::Result<()> {
        log::info!("[diag] QemuCommandExecutor::unmount_folder: path={path}");
        self.cmd_tx
            .send(MgmtCommand::Unmount {
                uri: path.to_string(),
            })
            .map_err(|_| std::io::Error::other("management thread gone"))?;
        self.wake_cmd();
        self.mounted
            .lock()
            .expect("mounted poisoned")
            .remove(path);
        log::info!("[diag] QemuCommandExecutor::unmount_folder: {path} mapping revoked");
        Ok(())
    }
}

/// Exposes the executor as the `FolderMounter` registered at startup, so the
/// workspace can mount an opened folder through the stable linker interface.
impl qemu_cmd_agent_linker::FolderMounter for QemuCommandExecutor {
    fn mount_folder(&self, path: &str) -> std::io::Result<()> {
        QemuCommandExecutor::mount_folder(self, path)
    }

    fn unmount_folder(&self, path: &str) -> std::io::Result<()> {
        QemuCommandExecutor::unmount_folder(self, path)
    }
}

impl qemu_cmd_agent_linker::RemoteCommandExecutor for QemuCommandExecutor {
    fn spawn(&self, spec: ExecSpec) -> std::io::Result<RemoteChild> {
        if !self.available.load(Ordering::SeqCst) {
            log::warn!("[diag] spawn: rejected, cmd-agent unavailable (QEMU restarting)");
            return Err(std::io::Error::other(
                "cmd-agent unavailable (QEMU restarting)",
            ));
        }
        // [diag] Differentiate UI-thread calls from background calls: a UI
        // (main) thread call must fail fast while the guest agent is still
        // booting (it blocks the interface), while a background call may wait
        // for the agent to become ready and then forward the command.
        if !self.state.ready.load(Ordering::SeqCst) {
            // The main (UI) thread is identified by tid == pid: the kernel
            // guarantees the process's first thread has its own tid equal to
            // the process pid, which is independent of the thread name (the
            // OHOS main thread is not named "main"). A UI-thread call must
            // fail fast while the guest agent is booting; background threads
            // may wait for readiness below.
            let is_main_thread = unsafe { libc::gettid() == libc::getpid() };
            if is_main_thread {
                log::warn!("[diag] spawn: UI thread (tid=pid), guest agent not ready, failing fast");
                return Err(std::io::Error::other("cmd-agent not ready yet"));
            }
            log::info!(
                "[diag] spawn: background call (tid={}), waiting for guest agent to become ready",
                unsafe { libc::gettid() }
            );
            self.wait_ready();
            if !self.state.ready.load(Ordering::SeqCst) {
                log::error!("[diag] spawn: guest agent not ready after wait, giving up");
                return Err(std::io::Error::other("cmd-agent not ready after wait"));
            }
        }
        // Wait for deferred work-directory mounts only when one is actually
        // pending, so the guest has the mapped cwd before git/LSP run. Checked
        // on every spawn (a mount queued just before the agent came up may
        // still be in flight), but the common no-pending case must not lock the
        // list and log per spawn -- that would spam "no deferred mounts
        // pending" for every concurrent git command at startup.
        let has_deferred_mounts = {
            let deferred = self.deferred_mounts.lock().unwrap_or_else(|e| e.into_inner());
            !deferred.is_empty()
        };
        if has_deferred_mounts {
            self.wait_deferred_mounts();
        }
        log::info!("[diag] QemuCommandExecutor::spawn: program={} args={:?}",
            spec.source_program,
            spec.args
        );
        let session_id = self.session_id();
        // Copy the stream modes up front: `spec` is moved into the handshake
        // closure below, but the modes are still needed when building the
        // RemoteChild streams.
        let stdout_mode = spec.stdout_mode;
        let stderr_mode = spec.stderr_mode;
        // [diag] Tag git commands so the stdout stream can report how many
        // bytes the guest produced (status/diff returning nothing would leave
        // the git panel without any changes).
        let stdout_diag_label = if spec.source_program == "git" {
            let brief: Vec<&str> = spec
                .args
                .iter()
                .filter(|arg| !arg.starts_with('-'))
                .map(|arg| arg.as_str())
                .take(3)
                .collect();
            Some(if brief.is_empty() { "git".to_string() } else { brief.join(" ") })
        } else {
            None
        };
        // stderr_mode is moved into the handshake closure (SpawnStderr check);
        // keep a Copy for the holder setup and the RemoteChild construction.
        let stderr_mode_child = stderr_mode;
        let index = self
            .alloc_ports()
            .inspect_err(|err| log::error!("[diag] QemuCommandExecutor::spawn: alloc_ports: {err}"))?;
        log::info!("[diag] spawn: session_id={session_id} port={index}");

        // Persistent connections: created once, reused across commands. In this
        // guest a virtio-serial port only serves its first host connection, so
        // reconnecting per command would lose the handshake. The handshake below
        // runs on a clone; the slot keeps the original for the next command.
        let (data_stream, err_connect) = self
            .acquire_connections(index, spec.stderr_mode != FdMode::Null)
            .inspect_err(|err| {
                self.release_ports(index, true);
                log::error!("[diag] QemuCommandExecutor::spawn: acquire connections port {index}: {err}"
                );
            })?;
        log::info!(
            "[diag] spawn: session_id={session_id} connected data socket fd={} (reused)",
            data_stream.as_raw_fd()
        );

        // Handshake inside a closure so every failure path releases the port
        // pair (and drops a poisoned connection). The stderr SpawnStderr goes
        // first (protocol guarantees it precedes Spawn) unless the caller asked
        // for /dev/null stderr, in which case RemoteChild::stderr is None.
        let handshake = (|| -> std::io::Result<(UnixStream, Option<UnixStream>)> {
            // Async::new on the previous command's stdio wrappers leaves the
            // shared slot connection non-blocking (O_NONBLOCK lives on the
            // file description, which try_clone shares). A reused connection
            // would then read WouldBlock (EAGAIN) immediately during the
            // handshake instead of waiting the timeout; the handshake fails,
            // the connection is dropped, and the virtio-serial port is
            // poisoned for every later command. Restore blocking mode first so
            // the Hello/Spawn reads actually block (bounded by SO_RCVTIMEO).
            data_stream.set_nonblocking(false)?;
            // Data connection: Hello -> HelloOk -> SpawnStderr -> Spawn -> SpawnOk.
            // The SpawnStderr frame rides the data connection: in this QEMU
            // guest the err port is guest->host only (stderr output), so a
            // host->guest frame written to it is never delivered to cmd-agentd.
            let mut data_stream = data_stream;
            data_stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
            frame::write_message(
                &mut data_stream,
                &ClientMessage::Hello {
                    version: messages::PROTOCOL_VERSION,
                },
            )
            .inspect_err(|err| log::error!("[diag] QemuCommandExecutor::spawn: write Hello: {err}"))?;
            match frame::read_message::<_, ServerMessage>(&mut data_stream)
                .inspect_err(|err| log::error!("[diag] QemuCommandExecutor::spawn: read HelloOk: {err}"))?
            {
                ServerMessage::HelloOk { .. } => {
                    log::info!("[diag] spawn: session_id={session_id} got HelloOk");
                }
                other => {
                    log::error!("[diag] spawn: session_id={session_id} unexpected hello reply: {other:?}");
                    return Err(std::io::Error::other(format!(
                        "expected HelloOk, got {other:?}"
                    )));
                }
            }
            // SpawnStderr precedes Spawn (protocol order) and now rides the
            // data connection; see the handshake comment above.
            if stderr_mode != FdMode::Null {
                frame::write_message(&mut data_stream, &ClientMessage::SpawnStderr { session_id })
                    .inspect_err(|err| {
                        log::error!("[diag] QemuCommandExecutor::spawn: write SpawnStderr: {err}")
                    })?;
            }
            log::info!(
                "[diag] spawn: session_id={session_id} sending Spawn program={} binary={} args={:?} cwd={:?} env_keys={:?} stdin_bytes={}",
                spec.source_program,
                spec.binary,
                spec.args,
                spec.cwd_path,
                spec.env.keys().collect::<Vec<_>>(),
                spec.stdin.len()
            );
            frame::write_message(&mut data_stream, &ClientMessage::Spawn { session_id, spec })
                .inspect_err(|err| log::error!("[diag] QemuCommandExecutor::spawn: write Spawn: {err}"))?;
            let spawnok_read_start = std::time::Instant::now();
            match frame::read_message::<_, ServerMessage>(&mut data_stream)
                .inspect_err(|err| log::error!("[diag] QemuCommandExecutor::spawn: read SpawnOk: {err} after {:?}", spawnok_read_start.elapsed()))?
            {
                ServerMessage::SpawnOk { .. } => {
                    log::info!("[diag] spawn: session_id={session_id} got SpawnOk after {:?}", spawnok_read_start.elapsed());
                }
                ServerMessage::Error { message, .. } => {
                    log::error!("[diag] spawn: session_id={session_id} rejected: {message}");
                    return Err(std::io::Error::other(format!("spawn rejected: {message}")));
                }
                other => {
                    log::error!("[diag] spawn: session_id={session_id} unexpected spawn reply: {other:?}");
                    return Err(std::io::Error::other(format!("expected SpawnOk, got {other:?}")));
                }
            }
            // Handshake done; clear the timeout so the stdio streams are unbounded.
            let _ = data_stream.set_read_timeout(None);
            if let Some(err_stream) = err_connect.as_ref() {
                let _ = err_stream.set_read_timeout(None);
            }
            Ok((data_stream, err_connect))
        })();
        let (data_stream, err_connect) = match handshake {
            Ok(connected) => connected,
            Err(err) => {
                self.release_ports(index, true);
                log::error!("[diag] QemuCommandExecutor::spawn: handshake failed, releasing port {index}: {err}"
                );
                return Err(err);
            }
        };

        // Register the session (management thread fills exit_code and frees the
        // port pair on ExecResult, only once the data AND err connections have
        // both been drained).
        let holder_data = Arc::new(());
        let holder_err = Arc::new(());
        let exited = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(WakeSignal::new());
        let (data_drained, err_drained) = {
            let mut ports = self.state.ports.lock().unwrap_or_else(|e| e.into_inner());
            let slot = &mut ports[index];
            // A Null stream creates no reader, so it has no holder and the
            // port side is immediately clean; a Piped stream registers its
            // reader holder so the port is only released once that reader is
            // dropped (epoll unregistered).
            if stdout_mode == FdMode::Null {
                slot.data_holder = Weak::new();
            } else {
                slot.data_holder = Arc::downgrade(&holder_data);
            }
            if stderr_mode_child == FdMode::Null {
                slot.err_holder = Weak::new();
            } else {
                slot.err_holder = Arc::downgrade(&holder_err);
            }
            (slot.data_drained.clone(), slot.err_drained.clone())
        };
        self.state
            .sessions
            .lock()
            .expect("sessions poisoned")
            .insert(
                session_id,
                SessionState {
                    exit_code: None,
                    waiters: Vec::new(),
                    port_index: index,
                    exited: exited.clone(),
                    wake: wake.clone(),
                    inconsistent_rounds: 0,
                },
            );

        // Wrap the sockets in async handles for the RemoteChild streams.
        // `std::os::unix::net::UnixStream::try_clone` duplicates the fd so the
        // single data socket can back both stdin (write) and stdout (read). The
        // read streams are EOF-on-exit: the connection never closes, so they
        // substitute the command's exit signal for EOF.
        let data_reader = data_stream.try_clone()?;
        let data_async = Async::new(data_stream)?;
        let data_clone = Async::new(data_reader)?;
        let stdout = ExitEofStream {
            inner: data_clone,
            exited: exited.clone(),
            drained: data_drained.clone(),
            wake: wake.clone(),
            _holder: holder_data,
            eof_sent: false,
            exited_pending_count: 0,
            diag_label: stdout_diag_label,
            diag_total: 0,
        };
        let stderr = err_connect
            .map(|stream| -> std::io::Result<ExitEofStream> {
                Ok(ExitEofStream {
                    inner: Async::new(stream)?,
                    exited: exited.clone(),
                    drained: err_drained.clone(),
                    wake: wake.clone(),
                    _holder: holder_err,
                    eof_sent: false,
                    exited_pending_count: 0,
                    diag_label: None,
                    diag_total: 0,
                })
            })
            .transpose()?;
        log::info!("[diag] QemuCommandExecutor::spawn: session_id={session_id} port={index}");

        let stdout_opt = if stdout_mode == FdMode::Null {
            None
        } else {
            Some(Box::new(stdout) as Box<dyn AsyncRead + Unpin + Send>)
        };
        let stderr_opt = if stderr_mode_child == FdMode::Null {
            None
        } else {
            stderr.map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>)
        };
        Ok(RemoteChild {
            session_id,
            stdin: Some(Box::new(StdinSignalWriter {
                inner: data_async,
                session_id,
                cmd_tx: self.cmd_tx.clone(),
                cmd_wake: self.cmd_wake,
            }) as Box<dyn AsyncWrite + Unpin + Send>),
            stdout: stdout_opt,
            stderr: stderr_opt,
        })
    }

    fn signal(
        &self,
        session_id: u64,
        signal: Signal,
    ) -> std::io::Result<()> {
        log::info!("[diag] QemuCommandExecutor::signal: session_id={session_id} signal={signal:?}");
        // The management thread owns the only live mgmt socket connection (QEMU
        // accepts exactly one client per socket chardev); a fresh connection
        // here would sit in the accept backlog unread and block forever on the
        // HelloOk read. Queue the signal and let that thread flush it.
        self.cmd_tx
            .send(MgmtCommand::Signal { session_id, signal })
            .map_err(|_| std::io::Error::other("management thread gone"))?;
        self.wake_cmd();
        log::info!("[diag] QemuCommandExecutor::signal: session_id={session_id} queued");
        Ok(())
    }

    fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        let code = self
            .state
            .sessions
            .lock()
            .expect("sessions poisoned")
            .get(&session_id)
            .and_then(|s| s.exit_code);
        log::debug!("[diag] QemuCommandExecutor::try_exit: session_id={session_id} exit={code:?}");
        code
    }

    fn wait_exit_async(
        &self,
        session_id: u64,
    ) -> qemu_cmd_agent_linker::ExitFuture<'_> {
        log::info!("[diag] QemuCommandExecutor::wait_exit_async: session_id={session_id} waiting");
        let state = self.state.clone();
        Box::pin(async move {
            let rx = {
                let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = sessions.get_mut(&session_id) {
                    if let Some(code) = s.exit_code {
                        log::info!("[diag] QemuCommandExecutor::wait_exit_async: session_id={session_id} already exited code={code:?}");
                        // Nobody is left to await this session; drop its entry so
                        // completed commands do not accumulate in the map.
                        if s.waiters.is_empty() {
                            sessions.remove(&session_id);
                        }
                        return Ok(code);
                    }
                    let (tx, rx) = smol::channel::unbounded();
                    s.waiters.push(tx);
                    rx
                } else {
                    log::warn!("[diag] QemuCommandExecutor::wait_exit_async: session_id={session_id} not registered"
                    );
                    return Ok(None);
                }
            };
            match rx.recv().await {
                Ok(code) => {
                    log::info!("[diag] QemuCommandExecutor::wait_exit_async: session_id={session_id} code={code:?}");
                    Ok(code)
                }
                Err(err) => {
                    log::warn!("[diag] QemuCommandExecutor::wait_exit_async: session_id={session_id} waiter dropped: {err}");
                    Ok(None)
                }
            }
        })
    }
}

/// Dedicated thread: keeps the management connection alive, reads `ExecResult`
/// into the shared table, notifies waiters, frees the session's port pair and
/// acknowledges with `ExecResultAck` so the guest agent reclaims its ports. It
/// also flushes queued `Signal`s over the same connection (QEMU accepts exactly
/// one client per socket chardev, so signals cannot use a second connection).
fn mgmt_loop(
    port_dir: PathBuf,
    state: Arc<State>,
    sandbox_root: Option<String>,
    cmd_rx: mpsc::Receiver<MgmtCommand>,
    cmd_wake: i32,
    available: Arc<AtomicBool>,
    mounted: Arc<Mutex<HashSet<String>>>,
    mount_counter: Arc<AtomicU64>,
) {
    // Mount the fixed sandbox root; cleared once mounted, re-armed after a
    // QEMU restart so the fresh guest gets its /sandbox export again.
    let mut pending_sandbox_mount = sandbox_root.clone();
    // In-flight work-directory mounts awaiting their MountOk replies, matched
    // by uri. Multiple mounts can run concurrently (one per deferred folder),
    // so this is a list; the reply table would otherwise have one slot and a
    // second mount would overwrite the first's sender, failing it with a
    // Disconnected error.
    let mut pending_mount_reply: Vec<(String, mpsc::SyncSender<std::io::Result<()>>)> = Vec::new();
    // True once a connection succeeded at least once: a later drop then means
    // the guest agent was lost and QEMU must be restarted, whereas startup
    // connects simply retry until the guest comes up.
    let mut connected_once = false;
    // Set while a lost guest is being restarted; the next successful connect
    // resets executor state and re-mounts the sandbox root.
    let mut restarting = false;
    loop {
        let mgmt_sock = port_dir.join(MGMT_SOCKET);
        let stream = match UnixStream::connect(&mgmt_sock) {
            Ok(s) => s,
            Err(err) => {
                if connected_once && !restarting {
                    // Auto-restart disabled for diagnosis (user request): mark
                    // the guest unavailable and keep reconnecting, but never
                    // reboot QEMU so the wedge can be inspected.
                    log::error!("[diag] mgmt_loop: connection lost; auto-restart DISABLED (diagnosis)");
                    available.store(false, Ordering::SeqCst);
                    restarting = true;
                    connected_once = false;
                    state.ready.store(false, Ordering::SeqCst);
                } else {
                    log::warn!("[diag] mgmt_loop: connect {mgmt_sock:?}: {err}");
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
        };
        log::info!("[diag] mgmt_loop: connected to {mgmt_sock:?}");
        let mut stream = stream;
        log::info!("[diag] mgmt_loop: fd={}", stream.as_raw_fd());
        // Bound the management handshake: the mgmt socket file exists as soon
        // as QEMU creates its chardev, before cmd-agentd boots. Without a
        // timeout the Hello/HelloOk exchange blocks forever on a not-yet-ready
        // guest and queued Mounts are never flushed. With a timeout the loop
        // retries and handshakes successfully once cmd-agentd is up.
        stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .ok();
        // Bound writes too: if the guest stops reading the mgmt connection, an
        // unbounded write (heartbeat / ack) would block this thread and,
        // together with the guest's own blocked write, deadlock both sides
        // (each waits for the other to drain the mgmt socket).
        stream
            .set_write_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        if let Err(err) = handshake_mgmt(&mut stream) {
            if connected_once && !restarting {
                // Auto-restart disabled for diagnosis (see connection-lost case).
                log::error!("[diag] mgmt_loop: handshake lost; auto-restart DISABLED (diagnosis)");
                available.store(false, Ordering::SeqCst);
                restarting = true;
                connected_once = false;
                state.ready.store(false, Ordering::SeqCst);
            } else {
                log::warn!("[diag] mgmt_loop: handshake: {err}");
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        connected_once = true;
        state.ready.store(true, Ordering::SeqCst);
        // After a restart: reset executor state and re-arm the sandbox mount.
        if restarting {
            state.sessions.lock().unwrap_or_else(|e| e.into_inner()).clear();
            for slot in state.ports.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
                slot.busy = false;
                slot.data = None;
                slot.err = None;
                slot.data_drained = Arc::new(AtomicBool::new(false));
                slot.err_drained = Arc::new(AtomicBool::new(false));
                slot.data_holder = Weak::new();
                slot.err_holder = Weak::new();
            }
            state.pending_release.lock().unwrap_or_else(|e| e.into_inner()).clear();
            mounted.lock().unwrap_or_else(|e| e.into_inner()).clear();
            mount_counter.store(1, Ordering::SeqCst);
            state.sandbox_mounted.store(false, Ordering::SeqCst);
            pending_sandbox_mount = sandbox_root.clone();
            log::info!("[diag] mgmt_loop: executor reset after QEMU restart");
        }
        if let Some(root) = pending_sandbox_mount.clone() {
            // The guest's first reply after cold boot can exceed the 2s mgmt
            // read timeout (cmd-agentd initializing), leaving the MountOk
            // unread and tripping the next work-directory mount's ack. Bump the
            // read bound for the sandbox handshake so its MountOk is always
            // consumed here, then restore the normal bound for the main loop.
            stream.set_read_timeout(Some(SANDBOX_MOUNT_TIMEOUT)).ok();
            match request_mount(&mut stream, &root, &state) {
                Ok(()) => {
                    log::info!("[diag] mgmt_loop: sandbox root mounted: {root}");
                    pending_sandbox_mount = None;
                    state.sandbox_mounted.store(true, Ordering::SeqCst);
                }
                Err(err) => log::warn!("[diag] mgmt_loop: sandbox mount: {err}"),
            }
            stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok();
        }
        // The guest agent is up; accept spawns again.
        available.store(true, Ordering::SeqCst);
        restarting = false;

        let mut last_beat = std::time::Instant::now();
        let mut missed_heartbeats: u32 = 0;
        // Shortened while a session reconciliation is pending, so stale
        // sessions are confirmed and cleaned up quickly.
        let mut heartbeat_interval = HEARTBEAT_INTERVAL;
        // Persistent buffer for inbound mgmt frames (P1-5): bytes accumulate
        // across poll rounds and complete frames are sliced off by
        // parse_mgmt_message, so a split frame never blocks the loop.
        let mut mgmt_pending: Vec<u8> = Vec::new();
        loop {
            // Block in poll over the mgmt socket and the eventfd; a queued
            // command wakes the eventfd so signals/mounts are never delayed
            // behind a silent socket.
            let mut pfds = [
                libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cmd_wake,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll over the mgmt fd and the eventfd; both stay valid
            // while this connection lives.
            let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 2, HEARTBEAT_POLL_MILLIS) };
            if rc < 0 {
                log::warn!("[diag] mgmt_loop: poll: {}",
                    std::io::Error::last_os_error()
                );
                break;
            }
            // Release ports whose stdio has drained since ExecResult (or whose
            // reader is gone), so the pool never leaks even when a caller
            // never reads the output streams.
            release_drained_ports(&state);
            // A queued signal / mount / unmount woke us: drain the queue.
            if pfds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                // SAFETY: read clears the non-blocking eventfd; its value is
                // non-zero right after POLLIN.
                let mut wake_value: u64 = 0;
                unsafe {
                    libc::read(
                        cmd_wake,
                        &mut wake_value as *mut u64 as *mut libc::c_void,
                        8,
                    )
                };
                while let Ok(command) = cmd_rx.try_recv() {
                    match command {
                        MgmtCommand::Signal { session_id, signal } => {
                            log::info!("[diag] mgmt_loop: flushing signal session_id={session_id} signal={signal:?}"
                            );
                            if let Err(err) = frame::write_message(
                                &mut stream,
                                &ClientMessage::Signal { session_id, signal },
                            ) {
                                log::warn!("[diag] mgmt_loop: signal write: {err}");
                                // The eventfd was read clear above; re-arm it so
                                // the next round re-enters this flush loop and
                                // drains the remaining queued commands instead
                                // of stranding them until a later wake.
                                wake_eventfd(cmd_wake);
                                break;
                            }
                        }
                        MgmtCommand::StdinEof { session_id } => {
                            log::info!("[diag] mgmt_loop: flushing stdin eof session_id={session_id}");
                            if let Err(err) = frame::write_message(
                                &mut stream,
                                &ClientMessage::StdinEof { session_id },
                            ) {
                                log::warn!("[diag] mgmt_loop: stdin eof write: {err}");
                                wake_eventfd(cmd_wake);
                                break;
                            }
                        }
                        MgmtCommand::Mount {
                            uri,
                            mount_tag,
                            guest_path,
                            reply,
                        } => {
                            log::info!("[diag] mgmt_loop: flushing mount {uri} tag={mount_tag} guest={guest_path}"
                            );
                            if let Err(err) = frame::write_message(
                                &mut stream,
                                &ClientMessage::MountFolder2QEMU {
                                    uri: uri.clone(),
                                    mount_tag: mount_tag.clone(),
                                    guest_path: guest_path.clone(),
                                },
                            ) {
                                log::warn!("[diag] mgmt_loop: mount write: {err}");
                                let _ = reply.send(Err(std::io::Error::other(format!(
                                    "mount write: {err}"
                                ))));
                                wake_eventfd(cmd_wake);
                                break;
                            }
                            pending_mount_reply.push((uri, reply));
                        }
                        MgmtCommand::Unmount { uri } => {
                            log::info!("[diag] mgmt_loop: flushing unmount {uri}");
                            if let Err(err) = frame::write_message(
                                &mut stream,
                                &ClientMessage::UnmountFolder2QEMU { uri },
                            ) {
                                log::warn!("[diag] mgmt_loop: unmount write: {err}");
                                wake_eventfd(cmd_wake);
                                break;
                            }
                        }
                    }
                }
            }
            // Inbound messages on the mgmt socket. Drain available bytes into a
            // persistent buffer, then handle every complete frame: a half-frame
            // never blocks the loop (remaining bytes arrive in a later round),
            // and a frame split across virtio-serial deliveries keeps its bytes
            // across rounds (P1-5).
            if pfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                if !drain_stream_into(&mut stream, &mut mgmt_pending) {
                    log::debug!("[diag] mgmt_loop: mgmt socket EOF, reconnecting");
                    break;
                }
                while let Some(message) = parse_mgmt_message(&mut mgmt_pending) {
                    match message {
                        ServerMessage::ExecResult {
                            session_id,
                            exit_code,
                            ..
                        } => {
                            missed_heartbeats = 0;
                            log::info!("[diag] mgmt_loop: ExecResult session_id={session_id} exit_code={exit_code:?}"
                            );
                            handle_exec_result(&state, &mut stream, session_id, exit_code);
                        }
                        ServerMessage::MountOk { uri } => {
                            missed_heartbeats = 0;
                            log::info!("[diag] mgmt_loop: MountOk {uri}");
                            // Match by uri and route to that mount's reply
                            // sender; the others stay pending. Multiple
                            // concurrent mounts no longer trip an ack mismatch.
                            if let Some(pos) = pending_mount_reply
                                .iter()
                                .position(|(pending_uri, _)| *pending_uri == uri)
                            {
                                let (_, reply) = pending_mount_reply.remove(pos);
                                let _ = reply.send(Ok(()));
                            } else {
                                log::warn!("[diag] mgmt_loop: MountOk {uri} with no pending reply");
                            }
                        }
                        ServerMessage::HeartbeatOk { sessions: guest_sessions } => {
                            missed_heartbeats = 0;
                            let reconciling = reconcile_sessions(&state, &guest_sessions);
                            heartbeat_interval = if reconciling {
                                HEARTBEAT_INTERVAL_RECONCILING
                            } else {
                                HEARTBEAT_INTERVAL
                            };
                            log::debug!(
                                "[diag] mgmt_loop: heartbeat ok, reconciling={reconciling}"
                            );
                        }
                        ServerMessage::Error { message, .. } => {
                            missed_heartbeats = 0;
                            log::warn!("[diag] mgmt_loop: server error: {message}");
                            for (uri, reply) in pending_mount_reply.drain(..) {
                                let _ = reply.send(Err(std::io::Error::other(format!(
                                    "mount {uri} rejected: {message}"
                                ))));
                            }
                        }
                        other => {
                            // Any inbound mgmt frame is liveness proof: a
                            // responsive guest is alive even if we do not
                            // recognize the message.
                            missed_heartbeats = 0;
                            log::warn!("[diag] mgmt_loop: unexpected message {other:?}");
                        }
                    }
                }
                // A hangup with data already drained means the peer closed
                // after flushing; drop the connection to reconnect cleanly
                // instead of spinning on a POLLHUP that never clears.
                if pfds[0].revents & libc::POLLHUP != 0 {
                    log::debug!("[diag] mgmt_loop: mgmt socket hangup, reconnecting");
                    break;
                }
            }
            // Heartbeat tick: send one every interval; too many without any
            // inbound activity means the guest agent is lost. The frame carries
            // this side's active sessions so the guest can reconcile too.
            if last_beat.elapsed() >= heartbeat_interval {
                let sessions: Vec<u64> = {
                    let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    sessions
                        .iter()
                        .filter(|(_, s)| s.exit_code.is_none())
                        .map(|(id, _)| *id)
                        .collect()
                };
                if let Err(err) = frame::write_message(
                    &mut stream,
                    &ClientMessage::Heartbeat { sessions },
                ) {
                    log::warn!("[diag] mgmt_loop: heartbeat write: {err}");
                    break;
                }
                missed_heartbeats += 1;
                last_beat = std::time::Instant::now();
                if missed_heartbeats > HEARTBEAT_MISS_LIMIT {
                    // Auto-restart disabled for diagnosis: the guest stays up
                    // so the wedge can be inspected; the executor is marked
                    // unavailable until the management loop reconnects.
                    log::error!("[diag] mgmt_loop: cmd-agentd unresponsive; auto-restart DISABLED (diagnosis)");
                    available.store(false, Ordering::SeqCst);
                    restarting = true;
                    break;
                }
            }
        }
        // Host connection dropped; the guest agent keeps the port open, so
        // reconnect. In-flight mounts can never get their MountOk now.
        for (uri, reply) in pending_mount_reply.drain(..) {
            let _ = reply.send(Err(std::io::Error::other(format!(
                "management connection dropped during mount {uri}"
            ))));
        }
        log::info!("[diag] mgmt_loop: connection dropped, reconnecting");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Records one `ExecResult`: updates the exit state, wakes the waiters, frees
/// the session's data/err port pair back into the pool and acknowledges so the
/// guest agent reclaims its ports. Shared by the mgmt read loop and the
/// mount-wait loop so results racing in during a mount are not lost.
fn handle_exec_result(
    state: &State,
    stream: &mut UnixStream,
    session_id: u64,
    exit_code: Option<i32>,
) {
    let port_index = {
        let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match sessions.get_mut(&session_id) {
            Some(s) => {
                s.exit_code = Some(exit_code);
                // Signal the EOF-on-exit stdio streams: they drain the
                // connection and report EOF so the caller's read_to_end ends.
                s.exited.store(true, Ordering::SeqCst);
                // Wake readers parked on an output-less command, so they
                // release the connection promptly instead of staying
                // registered and stealing the next command's frames.
                s.wake.fire();
                log::info!(
                    "[diag] handle_exec_result: session_id={session_id} exit={exit_code:?} waiters={}",
                    s.waiters.len()
                );
                for waiter in s.waiters.drain(..) {
                    if waiter.try_send(exit_code).is_err() {
                        log::debug!("[diag] handle_exec_result: waiter for session_id={session_id} already dropped"
                        );
                    }
                }
                s.port_index
            }
            None => {
                log::warn!("[diag] handle_exec_result: ExecResult for unknown session_id={session_id}");
                // Nothing to free or ack; the guest reclaims on its 2s timeout.
                return;
            }
        }
    };
    // The release condition is ExecResult PLUS stdio consumption on BOTH the
    // data and err connections: the port is freed only once each connection
    // has drained (or its reader is gone), so a reused connection never
    // carries stale stdout/stderr bytes into the next handshake. Do NOT wait
    // here (P1-1): this runs on the single management thread, and a caller
    // that does not read its streams would park it for the wait window per
    // command, blocking heartbeats and every other queued command. Defer the
    // release instead; the management loop re-checks pending ports each round.
    let clean = {
        let ports = state.ports.lock().unwrap_or_else(|e| e.into_inner());
        port_clean(&ports[port_index])
    };
    if clean {
        state.ports.lock().unwrap_or_else(|e| e.into_inner())[port_index].busy = false;
        log::info!("[diag] handle_exec_result: session_id={session_id} freed port {port_index} (stdio clean)");
    } else {
        state
            .pending_release
            .lock()
            .expect("pending poisoned")
            .push((
                port_index,
                std::time::Instant::now() + PENDING_RELEASE_TIMEOUT,
            ));
        log::info!("[diag] handle_exec_result: session_id={session_id} port {port_index} deferred to mgmt_loop");
    }
    // Ack so cmd-agentd reclaims the data/err ports.
    if let Err(err) = frame::write_message(
        stream,
        &ClientMessage::ExecResultAck { session_id },
    ) {
        log::warn!("[diag] handle_exec_result: ack session_id={session_id}: {err}");
    } else {
        log::info!("[diag] handle_exec_result: session_id={session_id} ack sent");
    }
}

/// Whether a port's data AND err connections are clean. A connection is clean
/// only when its reader has been released (the EOF-on-exit stream was dropped,
/// which unregisters the Async handle from the connection's epoll). "No data
/// right now" (a WouldBlock/Pending poll) is deliberately NOT treated as
/// clean: a parked reader may still steal the next command's frames, and
/// buffered bytes may still be in flight. Releasing the port early while a
/// reader is still registered is exactly what poisoned ports in the past.
fn port_clean(slot: &PortSlot) -> bool {
    let data_ok = slot.data_holder.upgrade().is_none();
    let err_ok = slot.err_holder.upgrade().is_none();
    data_ok && err_ok
}

/// Reconciles this side's active sessions against the peer's list carried in
/// the last Heartbeat/HeartbeatOk. A session this side thinks is alive but the
/// peer no longer tracks has been torn down there; after `RECONCILE_THRESHOLD`
/// consecutive mismatches this side releases its port. Long-lived processes
/// (LSPs, git batch readers) are reported by the guest while they run, so they
/// never accumulate mismatches. Returns true while any mismatch is pending so
/// the caller can shorten the heartbeat and confirm quickly.
fn reconcile_sessions(state: &Arc<State>, peer_sessions: &[u64]) -> bool {
    let peer_set: HashSet<u64> = peer_sessions.iter().copied().collect();
    let mut pending_cleanup = Vec::new();
    let mut any_mismatch = false;
    {
        let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (session_id, s) in sessions.iter_mut() {
            if s.exit_code.is_some() {
                continue;
            }
            if peer_set.contains(session_id) {
                s.inconsistent_rounds = 0;
            } else {
                s.inconsistent_rounds += 1;
                any_mismatch = true;
                if s.inconsistent_rounds >= RECONCILE_THRESHOLD {
                    pending_cleanup.push(*session_id);
                }
            }
        }
    }
    for session_id in pending_cleanup {
        spawn_session_cleanup(state, session_id);
    }
    any_mismatch
}

/// Releases a stale session's port on a background thread so the management
/// loop is never blocked by draining connections or waking waiters. The port
/// connection is drained (clearing residual bytes from the abandoned process)
/// before the port is freed, so a reused connection never carries stale data
/// into the next handshake.
fn spawn_session_cleanup(state: &Arc<State>, session_id: u64) {
    let state = state.clone();
    std::thread::spawn(move || {
        log::warn!("[diag] reconcile: cleaning up stale session {session_id}");
        let port_index = {
            let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.get(&session_id).map(|s| s.port_index)
        };
        if let Some(port_index) = port_index {
            {
                let mut ports = state.ports.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(slot) = ports.get_mut(port_index) {
                    drain_conn(slot.data.as_mut());
                    drain_conn(slot.err.as_mut());
                    slot.busy = false;
                    slot.data_holder = Weak::new();
                    slot.err_holder = Weak::new();
                    slot.data_drained = Arc::new(AtomicBool::new(false));
                    slot.err_drained = Arc::new(AtomicBool::new(false));
                }
            }
        }
        if let Some(s) = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id)
        {
            s.exited.store(true, Ordering::SeqCst);
            s.wake.fire();
            for waiter in s.waiters {
                let _ = waiter.try_send(None);
            }
        }
    });
}

/// Drains whatever bytes are currently buffered on the management connection
/// into `pending`, so a frame split across virtio-serial deliveries keeps its
/// bytes across rounds. A tiny read timeout makes each read return the bytes
/// that are ready without blocking on a half-frame; returning false means the
/// peer closed (EOF). The read timeout is cleared afterwards, which is exactly
/// the P1-5 fix: the management connection no longer carries a long
/// SO_RCVTIMEO into the main loop, so a partial frame cannot park the loop.
fn drain_stream_into(stream: &mut UnixStream, pending: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 8192];
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1)));
    let result = loop {
        match stream.read(&mut buf) {
            Ok(0) => break false,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(_) => break true,
        }
    };
    let _ = stream.set_read_timeout(None);
    result
}

/// Slices one complete management frame off the persistent buffer. Returns
/// None when fewer than one frame's bytes are buffered; an oversized or
/// unparseable frame is dropped so the buffer cannot wedge on garbage.
fn parse_mgmt_message(pending: &mut Vec<u8>) -> Option<ServerMessage> {
    if pending.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
    if len > MAX_FRAME_SIZE {
        log::warn!("[diag] mgmt_loop: oversize frame len={len}, clearing buffer");
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
            log::warn!("[diag] mgmt_loop: parse failed: {err}, dropping frame");
            pending.drain(..4 + len);
            None
        }
    }
}

fn release_drained_ports(state: &State) {
    let pending: Vec<(usize, std::time::Instant)> = state
        .pending_release
        .lock()
        .expect("pending poisoned")
        .drain(..)
        .collect();
    if pending.is_empty() {
        return;
    }
    for (idx, deadline) in pending {
        let (clean, data_drained, err_drained) = {
            let ports = state.ports.lock().unwrap_or_else(|e| e.into_inner());
            let slot = &ports[idx];
            (
                port_clean(slot),
                slot.data_drained.load(Ordering::SeqCst),
                slot.err_drained.load(Ordering::SeqCst),
            )
        };
        if clean {
            if !data_drained {
                drain_port_conn(state, idx, true);
            }
            if !err_drained {
                drain_port_conn(state, idx, false);
            }
            let mut ports = state.ports.lock().unwrap_or_else(|e| e.into_inner());
            ports[idx].busy = false;
            ports[idx].data_holder = Weak::new();
            ports[idx].err_holder = Weak::new();
            log::info!("[diag] release_drained_ports: freed port {idx} (drained)");
        } else if std::time::Instant::now() >= deadline {
            // The caller never drained the streams (or never dropped them).
            // Force-release: the guest already stopped writing after
            // ExecResult, so draining here clears the socket buffer and the
            // port can be reused instead of leaking (P1-6).
            log::warn!("[diag] release_drained_ports: port {idx} force-released (stdio never drained)");
            drain_port_conn(state, idx, true);
            drain_port_conn(state, idx, false);
            let mut ports = state.ports.lock().unwrap_or_else(|e| e.into_inner());
            ports[idx].busy = false;
            ports[idx].data_holder = Weak::new();
            ports[idx].err_holder = Weak::new();
        } else {
            state
                .pending_release
                .lock()
                .expect("pending poisoned")
                .push((idx, deadline));
        }
    }
}

/// Best-effort drain of one of a port's connections. Called only when that
/// connection's reader is known to be gone, so it cannot steal data from an
/// active reader. The guest stops writing once the worker exits (before
/// ExecResult), so draining to a read timeout clears the socket buffer.
fn drain_port_conn(state: &State, index: usize, data: bool) {
    let mut ports = state.ports.lock().unwrap_or_else(|e| e.into_inner());
    let slot = ports.get_mut(index).expect("port index in range");
    if data {
        drain_conn(slot.data.as_mut());
        slot.data_drained = Arc::new(AtomicBool::new(false));
    } else {
        drain_conn(slot.err.as_mut());
        slot.err_drained = Arc::new(AtomicBool::new(false));
    }
}

fn drain_conn(conn: Option<&mut UnixStream>) {
    if let Some(conn) = conn {
        let _ = conn.set_read_timeout(Some(std::time::Duration::from_millis(1)));
        let mut sink = [0u8; 8192];
        loop {
            match conn.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = conn.set_read_timeout(None);
    }
}

/// Writes the eventfd to wake the management thread's poll. Non-blocking and
/// best-effort: a full eventfd simply stays non-zero.
fn wake_eventfd(fd: i32) {
    if fd < 0 {
        return;
    }
    let value: u64 = 1;
    // SAFETY: fd is a valid non-blocking eventfd; an 8-byte write.
    unsafe { libc::write(fd, &value as *const u64 as *const libc::c_void, 8) };
}

/// Runs one full mount on a background thread: QMP fsdev-add + device_add,
/// then MountFolder2QEMU over the management connection, waiting for MountOk.
/// Idempotent against the shared `mounted` set.
fn run_mount(
    cmd_tx: &mpsc::Sender<MgmtCommand>,
    cmd_wake: i32,
    qmp_socket: &str,
    mounted: &Mutex<HashSet<String>>,
    mount_counter: &AtomicU64,
    path: &str,
) -> std::io::Result<()> {
    if mounted.lock().unwrap_or_else(|e| e.into_inner()).contains(path) {
        return Ok(());
    }
    let sequence = mount_counter.fetch_add(1, Ordering::SeqCst);
    let device_id = format!("{DEVICE_PREFIX}{sequence}");
    let chardev_id = format!("{CHARDEV_PREFIX}{sequence}");
    let mount_tag = format!("{MOUNT_TAG_PREFIX}{sequence}");
    // Mount work directories at the same path they have on the device, so the
    // guest sees identical paths. The guest side create_dir_all()s the mount
    // point before mounting.
    let guest_path = path.to_string();
    // Each pcie-root-port exposes one hotplug slot; pick a distinct root port
    // per mount so multiple work directories can be mounted concurrently.
    let bus = format!("rp{}", sequence % WORKDIR_MOUNT_SLOTS as u64);
    log::info!(
        "[diag] run_mount: path={path} sequence={sequence} device={device_id} chardev={chardev_id} tag={mount_tag} guest={guest_path} bus={bus}"
    );
    // Start the in-process virtiofsd backend for this work dir (listens on
    // fs_work{sequence}.sock), then hotplug a vhost-user-fs device bound to it.
    let port_dir = Path::new(qmp_socket).parent().unwrap_or(Path::new(""));
    let backend_socket = {
        #[cfg(target_env = "ohos")]
        {
            virtiofs::spawn_workdir(port_dir, sequence, PathBuf::from(path), mount_tag.clone())
        }
        #[cfg(not(target_env = "ohos"))]
        {
            PathBuf::new()
        }
    };
    qmp::create_workdir_vhost_fs(
        qmp_socket,
        &chardev_id,
        &backend_socket.to_string_lossy(),
        &device_id,
        &mount_tag,
        &bus,
    )
    .inspect_err(|err| log::error!("[diag] run_mount: QMP export for {path}: {err}"))?;
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    cmd_tx
        .send(MgmtCommand::Mount {
            uri: path.to_string(),
            mount_tag,
            guest_path: guest_path.clone(),
            reply: reply_tx,
        })
        .map_err(|_| std::io::Error::other("management thread gone"))?;
    wake_eventfd(cmd_wake);
    // Bound the MountOk wait so a mount that never completes (e.g. the guest
    // agent never flushes the queue) fails rather than blocking the caller
    // forever. mount_folder is called synchronously from workspace open.
    match reply_rx.recv_timeout(MOUNT_OK_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            log::error!("[diag] run_mount: mount {path}: {err}");
            return Err(err);
        }
        Err(err) => {
            log::error!("[diag] run_mount: MountOk wait failed for {path}: {err}");
            return Err(std::io::Error::other(format!("mount reply: {err}")));
        }
    }
    mounted
        .lock()
        .expect("mounted poisoned")
        .insert(path.to_string());
    log::info!("[diag] run_mount: {path} mounted at {guest_path}");
    Ok(())
}

/// Management handshake: Hello -> HelloOk -> Manage.
fn handshake_mgmt(stream: &mut UnixStream) -> std::io::Result<()> {
    log::info!("[diag] handshake_mgmt: sending Hello");
    frame::write_message(
        stream,
        &ClientMessage::Hello {
            version: messages::PROTOCOL_VERSION,
        },
    )?;
    match frame::read_message::<_, ServerMessage>(stream)? {
        ServerMessage::HelloOk { .. } => {
            log::info!("[diag] handshake_mgmt: got HelloOk");
        }
        other => {
            log::error!("[diag] handshake_mgmt: unexpected reply: {other:?}");
            return Err(std::io::Error::other(format!(
                "expected HelloOk, got {other:?}"
            )));
        }
    }
    frame::write_message(stream, &ClientMessage::Manage)?;
    log::info!("[diag] handshake_mgmt: Manage sent, mgmt handshake complete");
    Ok(())
}

/// Sends MountFolder2QEMU for the fixed sandbox root and waits for MountOk,
/// so the guest registers host-root -> /sandbox before any command runs.
/// `ExecResult`s that race in during the mount are handled (not dropped), so a
/// command finishing in this window is not lost.
fn request_mount(
    stream: &mut UnixStream,
    sandbox_root: &str,
    state: &State,
) -> std::io::Result<()> {
    log::info!("[diag] request_mount: sending MountFolder2QEMU for {sandbox_root}");
    frame::write_message(
        stream,
        &ClientMessage::MountFolder2QEMU {
            uri: sandbox_root.to_string(),
            mount_tag: MOUNT_TAG_SANDBOX.to_string(),
            guest_path: GUEST_SANDBOX_PATH.to_string(),
        },
    )
    .inspect_err(|err| log::error!("[diag] request_mount: write MountFolder2QEMU: {err}"))?;
    loop {
        match frame::read_message::<_, ServerMessage>(stream)
            .inspect_err(|err| log::error!("[diag] request_mount: read response: {err}"))?
        {
            ServerMessage::MountOk { uri } => {
                log::info!("[diag] request_mount: mounted {uri}");
                return Ok(());
            }
            ServerMessage::Error { message, .. } => {
                log::error!("[diag] request_mount: mount rejected: {message}");
                return Err(std::io::Error::other(message));
            }
            ServerMessage::ExecResult {
                session_id,
                exit_code,
                ..
            } => {
                log::info!("[diag] request_mount: ExecResult during mount session_id={session_id} exit_code={exit_code:?}"
                );
                handle_exec_result(state, stream, session_id, exit_code);
            }
            other => {
                log::debug!("[diag] request_mount: ignoring response {other:?}");
            }
        }
    }
}

