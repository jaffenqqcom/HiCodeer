//! Remote command execution over the SSH connection pool.
//!
//! `SshCommandExecutor` implements the stable `RemoteCommandExecutor` and
//! `FolderMounter` traits. Each spawn allocates a pooled SSH connection, opens
//! a session channel, runs the translated shell command, and bridges stdio
//! through socketpairs: the caller sees smol `Async<UnixStream>` ends, while a
//! tokio pump task (on the pool runtime) relays channel Data/ExtendedData into
//! the socketpairs and reports the exit status.

use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qemu_ssh_agent_linker::{ExitFuture, RemoteChild};
use command_executor::{ExecSpec, Signal};
use russh::ChannelMsg;
use smol::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::command;
use crate::pool::{Pool, SshSession};
use crate::qmp;
use crate::virtiofs;

/// Cap on a synchronous mount/`signal` wait before failing.
const QMP_OR_SSH_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll/sleep granularity for the bounded pool allocate wait (not used here).
const IO_CHUNK_SIZE: usize = 8192;
/// Tag prefix for dynamically mounted work directories (kept in sync with the
/// QEMU argv's static sandbox/tools tags).
const MOUNT_TAG_PREFIX: &str = "ztag";
/// Wait after QMP device_add before sending the guest mount command. The guest
/// kernel discovers a hotplugged vhost-user-fs device asynchronously; this
/// grace lets the probe usually complete so the first mount attempt succeeds,
/// instead of failing into the in-guest 1s retry sleep.
const MOUNT_PROBE_GRACE: Duration = Duration::from_millis(500);
/// Device id prefix for dynamically hotplugged vhost-user-fs devices.
const DEVICE_PREFIX: &str = "virtiofs";
/// Chardev id prefix for dynamically added vhost-user-fs chardevs.
const CHARDEV_PREFIX: &str = "vfwork";
/// Number of pre-created pcie-root-ports for runtime hotplug (each exposes one
/// slot; ports are picked by mount sequence modulo this count).
const WORKDIR_MOUNT_SLOTS: usize = 8;

/// Per-session exit state, shared between the tokio pump task and waiters on
/// the smol executor.
pub struct SessionState {
    exit: Mutex<Option<Option<i32>>>,
    notify: tokio::sync::Notify,
}

impl SessionState {
    fn new() -> Self {
        Self {
            exit: Mutex::new(None),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Records the exit result once (later calls are ignored) and wakes
    /// waiters. `None` means the command died without a status (signal or
    /// connection loss), mapping to util::command's None -> 128.
    fn set_exit(&self, exit: Option<i32>) {
        let mut guard = self.exit.lock().unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            *guard = Some(exit);
        }
        self.notify.notify_waiters();
    }
}

/// Remote command executor and folder mounter over the SSH pool.
pub struct SshCommandExecutor {
    pool: Arc<Pool>,
    sessions: Mutex<HashMap<u64, Arc<SessionState>>>,
    next_session: AtomicU64,
    /// Guest pid dir for command pid files; set by the bootstrap once SshInfo
    /// arrives (commands record `$$` there, signal reads it).
    pid_dir: Arc<Mutex<Option<String>>>,
    qmp_socket: PathBuf,
    mounted: Mutex<HashSet<String>>,
    mount_counter: AtomicU64,
}

impl SshCommandExecutor {
    /// Creates the pool, starts the QEMU-side bootstrap (management serial port
    /// handshake -> hostfwd -> pool config) and returns the executor.
    pub fn new(port_dir: std::path::PathBuf, _sandbox_root: Option<String>) -> std::io::Result<Self> {
        log::info!("SshCommandExecutor::new: port_dir={}", port_dir.display());
        let pool = Pool::new()?;
        let pid_dir = Arc::new(Mutex::new(None));
        let qmp_socket = port_dir.join("qmp.sock");
        let executor = Self {
            pool: pool.clone(),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            pid_dir: pid_dir.clone(),
            qmp_socket,
            mounted: Mutex::new(HashSet::new()),
            mount_counter: AtomicU64::new(1),
        };
        // Bootstrap thread: connect the management serial port, learn SshInfo,
        // add the hostfwd, and hand the pool its connection config.
        let bootstrap_pool = pool.clone();
        let bootstrap_pid = pid_dir.clone();
        std::thread::Builder::new()
            .name("ssh-bootstrap".to_string())
            .spawn(move || {
                crate::bootstrap::start(&port_dir, bootstrap_pool, bootstrap_pid);
            })
            .map_err(std::io::Error::other)?;
        Ok(executor)
    }

    /// Resolves the guest pid dir once known (bootstrap sets it).
    fn pid_dir_value(&self) -> Option<String> {
        self.pid_dir.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
    }

    /// Synchronizes the guest's `CLOCK_REALTIME` to the host (OHOS device) wall
    /// clock, over the existing SSH command channel.
    ///
    /// The QEMU guest is a BusyBox initramfs (no python, no hwclock-driven RTC
    /// init), so its system clock starts at epoch (1970) and drifts under TCG.
    /// This pulls the host time into the guest without any new protocol: it runs
    /// BusyBox's `date` applet (`date -s @<epoch>`) through the same
    /// `run_ssh_command` path used for git/LSP. The command runs as root in the
    /// guest (ssh-agentd), so `date -s` has CAP_SYS_TIME.
    ///
    /// Precision: second only. BusyBox exposes no sub-second `clock_settime`
    /// wrapper, so the guest clock can only be pinned to whole seconds. Call
    /// `sync_system_time_once` after the executor is registered to pin it once at
    /// boot; the single pin bounds the TCG drift between boots.
    /// `@<epoch>` is a TZ-independent absolute timestamp accepted by BusyBox
    /// >= 1.20; if the BusyBox build predates that, use
    /// `date -u -s "<YYYY-MM-DD HH:MM:SS>"` (the TZ-independent UTC form) instead.
    pub async fn sync_system_time(&self) -> std::io::Result<()> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // BusyBox `date -s @<epoch>` sets CLOCK_REALTIME to the absolute epoch
        // seconds. Root in the guest satisfies CAP_SYS_TIME. Sub-second precision
        // is unavailable without a clock_settime wrapper, which BusyBox lacks.
        let command = format!("date -s @{secs}");
        let conn = self.pool.allocate()?;
        run_ssh_command(conn, &command).await
    }

    /// Pins the guest wall clock to the host exactly once, after the guest has
    /// booted. `sync_system_time` blocks on `pool.allocate()` (which waits for the
    /// guest SSH server) the first time, so this is safe to call right after the
    /// executor is registered, before the guest is up. No periodic re-sync: a
    /// single pin at boot bounds the TCG drift between boots, which is all Zed
    /// needs for TLS/cache/make timestamps.
    pub fn sync_system_time_once(self: Arc<Self>) {
        let handle = self.pool.runtime().handle().clone();
        handle.spawn(async move {
            if let Err(err) = self.sync_system_time().await {
                log::warn!("ssh executor: initial guest time sync failed: {err}");
            }
        });
    }
}

impl qemu_ssh_agent_linker::RemoteCommandExecutor for SshCommandExecutor {
    fn spawn(&self, spec: ExecSpec) -> std::io::Result<RemoteChild> {
        let session_id = self.next_session.fetch_add(1, Ordering::SeqCst);
        let pid_dir = self
            .pid_dir_value()
            .unwrap_or_else(|| "/sandbox/ssh".to_string());
        let command = command::build_command(&spec, &pid_dir, session_id);
        log::info!(
            "ssh executor: spawn session={session_id} binary={} cmd={command}",
            spec.binary
        );
        let conn = self.pool.allocate()?;

        // Socketpairs: caller side is smol Async<UnixStream>, pump side is a
        // tokio UnixStream on the pool runtime.
        let (stdout_reader, stdout_pump) = UnixStream::pair()?;
        let (stderr_reader, stderr_pump) = UnixStream::pair()?;
        let (stdin_pump, stdin_writer) = UnixStream::pair()?;

        let stdout: Box<dyn AsyncRead + Unpin + Send> =
            Box::new(smol::Async::new(stdout_reader)?);
        let stderr: Box<dyn AsyncRead + Unpin + Send> =
            Box::new(smol::Async::new(stderr_reader)?);
        let stdin: Box<dyn AsyncWrite + Unpin + Send> =
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

    fn signal(&self, session_id: u64, _signal: Signal) -> std::io::Result<()> {
        // Single mechanism: signal the recorded process group. util::command
        // only ever sends SigKill.
        let Some(pid_dir) = self.pid_dir_value() else {
            return Ok(());
        };
        let command = format!(
            "kill -KILL -$(cat {}/{}.pid)",
            command::sh_quote(&pid_dir),
            session_id
        );
        let conn = self.pool.allocate()?;
        let runtime = self.pool.runtime();
        runtime.spawn(async move {
            if let Err(err) = run_ssh_command(conn, &command).await {
                log::warn!("ssh executor: signal session={session_id}: {err}");
            } else {
                log::info!("ssh executor: signaled session={session_id}");
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
            loop {
                if let Some(exit) = *state.exit.lock().unwrap_or_else(|poison| poison.into_inner())
                {
                    return Ok(exit);
                }
                state.notify.notified().await;
            }
        })
    }
}

/// Mount/unmount of a zcoder-opened folder into the guest. Mounts over
/// virtio-fs: hotplug a vhost-user-fs device via QMP, then `mount -t virtiofs`
/// through SSH so the guest sees the same path.
impl qemu_ssh_agent_linker::FolderMounter for SshCommandExecutor {
    fn mount_folder(&self, path: &str) -> std::io::Result<()> {
        if self
            .mounted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(path)
        {
            return Ok(());
        }
        let sequence = self.mount_counter.fetch_add(1, Ordering::SeqCst);
        let device_id = format!("{DEVICE_PREFIX}{sequence}");
        let chardev_id = format!("{CHARDEV_PREFIX}{sequence}");
        let mount_tag = format!("{MOUNT_TAG_PREFIX}{sequence}");
        let bus = format!("rp{}", sequence % WORKDIR_MOUNT_SLOTS as u64);
        log::info!(
            "ssh executor: mount_folder path={path} sequence={sequence} tag={mount_tag} bus={bus}"
        );

        // 1. In-process virtiofsd backend for this work dir.
        let port_dir = self.qmp_socket.parent().unwrap_or(Path::new("")).to_path_buf();
        let backend_socket =
            virtiofs::spawn_workdir(&port_dir, sequence, path.into(), mount_tag.clone());

        // 2. QMP hotplug: chardev-add + device_add vhost-user-fs-pci.
        qmp::create_workdir_vhost_fs(
            &self.qmp_socket.to_string_lossy(),
            &chardev_id,
            &backend_socket.to_string_lossy(),
            &device_id,
            &mount_tag,
            &bus,
        )
        .inspect_err(|err| log::error!("ssh executor: QMP export for {path}: {err}"))?;

        // 3. SSH `mkdir -p <path> && mount -t virtiofs <tag> <path>`. The guest
        // kernel discovers a hotplugged vhost-user-fs device asynchronously, so
        // the mount is retried in-guest until the tag appears (bounded window,
        // mirroring the qemu-cmd-agent mount retry). A short grace after
        // device_add lets the probe usually finish before the first attempt;
        // the in-guest retry then sleeps 0.3s (falling back to 1s if the busybox
        // build lacks fractional sleep support) instead of a full second.
        std::thread::sleep(MOUNT_PROBE_GRACE);
        let command = format!(
            "mkdir -p {} && for i in 1 2 3 4 5 6 7 8 9 10; do mount -t virtiofs {} {} && exit 0; sleep 0.3 2>/dev/null || sleep 1; done; exit 1",
            command::sh_quote(path),
            mount_tag,
            command::sh_quote(path)
        );
        let conn = self.pool.allocate()?;
        let runtime = self.pool.runtime();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let result = run_ssh_command(conn, &command).await;
            let _ = reply_tx.send(result);
        });
        match reply_rx.recv_timeout(QMP_OR_SSH_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(err) => {
                log::error!("ssh executor: mount {path} timed out: {err}");
                return Err(std::io::Error::other(format!("mount reply: {err}")));
            }
        }
        self.mounted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(path.to_string());
        log::info!("ssh executor: {path} mounted at {path}");
        Ok(())
    }

    fn unmount_folder(&self, path: &str) -> std::io::Result<()> {
        // Fire-and-forget: drop from the mounted set, no umount.
        self.mounted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(path);
        Ok(())
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
            log::error!("ssh pump: channel_open_session: {err}");
            state.set_exit(None);
            return;
        }
    };
    if let Err(err) = channel.exec(true, command.as_bytes()).await {
        log::error!("ssh pump: exec: {err}");
        state.set_exit(None);
        return;
    }
    // tokio requires non-blocking sockets: set each socketpair end before
    // wrapping, otherwise from_std panics ("Registering a blocking socket").
    let mut stdout_pump = stdout_pump;
    if let Err(err) = stdout_pump.set_nonblocking(true) {
        log::error!("ssh pump: set stdout nonblocking: {err}");
        state.set_exit(None);
        return;
    }
    let mut stdout_w = match tokio::net::UnixStream::from_std(stdout_pump) {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("ssh pump: wrap stdout: {err}");
            state.set_exit(None);
            return;
        }
    };
    let mut stderr_pump = stderr_pump;
    if let Err(err) = stderr_pump.set_nonblocking(true) {
        log::error!("ssh pump: set stderr nonblocking: {err}");
        state.set_exit(None);
        return;
    }
    let mut stderr_w = match tokio::net::UnixStream::from_std(stderr_pump) {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("ssh pump: wrap stderr: {err}");
            state.set_exit(None);
            return;
        }
    };
    let mut stdin_pump = stdin_pump;
    if let Err(err) = stdin_pump.set_nonblocking(true) {
        log::error!("ssh pump: set stdin nonblocking: {err}");
        state.set_exit(None);
        return;
    }
    let mut stdin_r = match tokio::net::UnixStream::from_std(stdin_pump) {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("ssh pump: wrap stdin: {err}");
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
                        log::info!("[diag] ssh pump: got {} bytes stdout", data.len());
                        if let Err(err) = stdout_w.write_all(&data).await {
                            log::warn!("[diag] ssh pump: write stdout failed: {err}");
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        log::info!("[diag] ssh pump: got {} bytes stderr", data.len());
                        if let Err(err) = stderr_w.write_all(&data).await {
                            log::warn!("[diag] ssh pump: write stderr failed: {err}");
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        log::info!("[diag] ssh pump: session exit status {exit_status}");
                        state.set_exit(Some(exit_status as i32));
                    }
                    Some(ChannelMsg::ExitSignal { .. }) => {
                        log::info!("[diag] ssh pump: session exited by signal");
                        state.set_exit(None);
                    }
                    Some(ChannelMsg::Eof) => {
                        // The remote side has no more stdout/stderr data, but
                        // the exit-status message for a finished command is sent
                        // AFTER this EOF: the guest EOFs on a closed child
                        // stdout pipe, then reports the exit status once the
                        // child is reaped. Breaking here made every quick
                        // command (which, git) resolve with exit_code=None and
                        // fail output.status(). Keep looping until the
                        // ExitStatus and Close arrive.
                        log::info!("[diag] ssh pump: got Eof, awaiting exit status");
                    }
                    Some(ChannelMsg::Close) => {
                        log::info!("[diag] ssh pump: got Close, breaking");
                        break;
                    }
                    None => {
                        log::info!("[diag] ssh pump: channel None, breaking");
                        break;
                    }
                    Some(other) => {
                        log::info!("[diag] ssh pump: unhandled msg, continuing");
                        let _ = other;
                    }
                }
            }
            read = stdin_r.read(&mut buf), if stdin_open => {
                match read {
                    Ok(0) => {
                        // Send the channel EOF per the SSH standard: the caller
                        // (util) closed its stdin, so the guest must learn that
                        // and enter its normal error handling instead of a
                        // long-lived LSP blocking forever waiting for input.
                        log::info!("[diag] ssh pump: stdin EOF, sending channel eof");
                        stdin_open = false;
                        let _ = stdin_writer.shutdown().await;
                    }
                    Ok(n) => {
                        log::info!("[diag] ssh pump: forwarding {n} bytes stdin");
                        if let Err(err) = stdin_writer.write_all(&buf[..n]).await {
                            log::warn!("[diag] ssh pump: write stdin: {err}");
                            stdin_open = false;
                        }
                    }
                    Err(err) => {
                        log::warn!("[diag] ssh pump: read stdin: {err}");
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
    log::debug!("ssh pump: pump finished");
}

