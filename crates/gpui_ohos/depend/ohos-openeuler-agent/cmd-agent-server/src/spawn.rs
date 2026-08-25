//! Direct child spawning: the spawned process takes over the connection
//! socket as its stdio, so data flows straight between the client and the
//! child without passing through the server process.
//!
//! The data connection is handshaken as frames, then handed to the child via
//! `dup2`; the server keeps only the child handle for wait/kill and reports
//! the exit code over the management connection.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use cmd_agent_protocol::{ExecSpec, FdMode, RootMap};

use crate::error::{Error, Result, ResultContext};

/// Poll interval when waiting for a child to exit.
const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How long to keep reaping after a kill before giving up on the child.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Device-side application-sandbox file root; the VM mirrors it under the
/// cmd-agent data directory, so `<this>/...` becomes `$VM_AGENT_ROOT/...`.
const DEVICE_APP_FILES_ROOT: &str = "/data/storage/el2/base/haps/entry/files";
/// VM-side root that mirrors the device sandbox files (LSP installs, npm
/// cache, formatter packages all live under `<this>/zed/...`).
const VM_AGENT_ROOT: &str = "/home/user/cmd-agent";
/// Device-side IDE workspace root; the VM shares it at the workspace mount.
const DEVICE_IDE_ROOT: &str = "/storage/Users/currentUser";
const VM_SHARED_ROOT: &str = "/mnt/linux_share";

/// Maps an OHOS-side path to the VM-side path using the fixed rules below.
/// Every argument may carry a path, so callers apply this to each one.
///
/// Rule A: `<DEVICE_APP_FILES_ROOT>/...` -> `<VM_AGENT_ROOT>/...`
/// Rule B: `<DEVICE_IDE_ROOT>/...` -> `<VM_SHARED_ROOT>/...`
///
/// The prefix match is component-bounded: a path like `<root>X/...` whose next
/// segment merely shares a prefix with the root is left untouched.
pub fn map_path(path: &str) -> String {
    // Some args embed a path after a flag (e.g. `--cache=<path>`); map the
    // value part so the device path is rewritten to the VM's. Only flag-like
    // args are split, so plain paths and script contents pass through whole.
    if path.starts_with('-') {
        if let Some((flag, value)) = path.split_once('=') {
            if !value.is_empty() && value.starts_with('/') {
                return format!("{flag}={}", map_path_value(value));
            }
        }
    }
    map_path_value(path)
}

/// Maps a bare path argument under the fixed rules.
fn map_path_value(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(DEVICE_APP_FILES_ROOT) {
        if rest.is_empty() || rest.starts_with('/') {
            return format!("{VM_AGENT_ROOT}{rest}");
        }
    }
    if let Some(rest) = path.strip_prefix(DEVICE_IDE_ROOT) {
        if rest.is_empty() || rest.starts_with('/') {
            return format!("{}{}", VM_SHARED_ROOT, rest);
        }
    }
    path.to_string()
}

/// Spawns the child described by `spec`, wiring its three standard descriptors
/// according to `spec.stdin_mode/stdout_mode/stderr_mode`.
///
/// Piped descriptors are `dup2`'d onto the connection sockets (`main_fd` for
/// stdin/stdout, `stderr_fd` for stderr); Null descriptors point at
/// `/dev/null`. The child leads a new process group so the whole tree can be
/// killed on timeout, disconnect, or explicit signal. The fds must stay valid
/// for the whole spawn; the caller holds the owning streams.
pub fn spawn_direct(
    main_fd: RawFd,
    stderr_fd: Option<RawFd>,
    spec: &ExecSpec,
    _root_map: Option<&RootMap>,
    session_id: u64,
) -> Result<std::process::Child> {
    log::info!(
        "spawn: session={session_id}, program={}, binary={}, args={:?}, stdin={:?}, stdout={:?}, stderr={:?}",
        spec.source_program,
        spec.binary,
        spec.args,
        spec.stdin_mode,
        spec.stdout_mode,
        spec.stderr_mode
    );
    // Full execution spec for offline analysis of zcodor's commands: every
    // field travels in ExecSpec, so the client's intent is fully recoverable
    // from the log when debugging how a command should be adapted.
    log::info!(
        "spawn[spec]: session={session_id}, source_program={}, binary={}, args={:?}, cwd={:?}, env={:?}, path_arg_indices={:?}, stdin_mode={:?}, stdout_mode={:?}, stderr_mode={:?}, timeout_ms={:?}",
        spec.source_program,
        spec.binary,
        spec.args,
        spec.cwd_path,
        spec.env,
        spec.path_arg_indices,
        spec.stdin_mode,
        spec.stdout_mode,
        spec.stderr_mode,
        spec.timeout_ms
    );

    // Every argument may carry a path, so map each one with the fixed rules.
    let mut args = spec.args.clone();
    for arg in &mut args {
        *arg = map_path(arg);
    }
    let cwd = spec.cwd_path.as_ref().map(|path| map_path(path));

    // Resolve the program to execute on the VM with a fallback chain:
    //   1. A device path mapped through map_path that exists on the VM wins
    //      (e.g. Zed-managed node or LSPs mirrored under $HOME), preserving
    //      the exact version zcodor downloaded.
    //   2. A VM absolute path that exists (e.g. one returned by `which`) is
    //      used as-is, so the exact location on the VM is honored.
    //   3. Otherwise the program is looked up by name in the VM's PATH.
    let mapped_binary = map_path(&spec.binary);
    let binary_for_exec = if Path::new(&mapped_binary).exists() {
        mapped_binary
    } else if Path::new(&spec.binary).exists() {
        spec.binary.clone()
    } else {
        Path::new(&spec.binary)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| spec.binary.clone())
    };
    // [diag] diagnostic for spawn failures: confirm what execvp is actually
    // handed and which PATH it searches.
    log::info!(
        "spawn_direct[diag]: session={session_id}, spec.binary={}, binary_for_exec={binary_for_exec:?}, PATH={}",
        spec.binary,
        std::env::var("PATH").unwrap_or_default()
    );
    // [diag] verify the connection socket fds are still valid before spawn;
    // an EBADF here means the data/stderr connection closed before the child
    // could take over.
    let fd_valid = |fd: i32| unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 };
    log::info!(
        "spawn_direct[diag]: session={session_id}, main_fd={main_fd} valid={}, stderr_fd={stderr_fd:?} valid={:?}",
        fd_valid(main_fd),
        stderr_fd.map(fd_valid)
    );
    // [diag] show the VM-side cwd and args after root mapping, so path bugs
    // (e.g. git receiving a device path) are visible in the log.
    log::info!(
        "spawn_direct[diag]: session={session_id}, mapped_cwd={cwd:?}, mapped_args={args:?}",
    );
    let mut command = Command::new(&binary_for_exec);
    command
        .args(&args)
        .envs(&spec.env)
        .process_group(0);
    if let Some(cwd) = &cwd {
        // The device client creates its working directory before issuing a
        // command, but the VM-side mirror path may not exist yet (e.g. an npm
        // install dir under $HOME). Create it so spawn succeeds; a failure
        // here is non-fatal because the child may create the directory itself
        // (e.g. npm --prefix).
        if !Path::new(cwd).exists() {
            log::info!("spawn: creating mapped cwd {cwd:?}");
            if let Err(err) = std::fs::create_dir_all(cwd) {
                log::warn!("spawn: failed to create mapped cwd {cwd:?}: {err}");
            }
        }
        command.current_dir(cwd);
    }

    // The child takes over the connection sockets as its stdio. The sockets
    // were non-blocking for the async server; the child expects blocking
    // stdio, so O_NONBLOCK is cleared on the dup'd descriptors. dup2 shares
    // the open file description, which also turns the server's copy blocking,
    // but the server drops its socket copies immediately after spawn.
    let stdin_piped = spec.stdin_mode == FdMode::Piped;
    let stdout_piped = spec.stdout_mode == FdMode::Piped;
    let stderr_piped = spec.stderr_mode == FdMode::Piped;
    let main_fd: i32 = main_fd;
    let stderr_fd: Option<i32> = stderr_fd;
    unsafe {
        command.pre_exec(move || {
            if stdin_piped {
                if libc::dup2(main_fd, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if dup_devnull(0, libc::O_RDONLY).is_err() {
                return Err(std::io::Error::last_os_error());
            }
            if stdout_piped {
                if libc::dup2(main_fd, 1) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if dup_devnull(1, libc::O_WRONLY).is_err() {
                return Err(std::io::Error::last_os_error());
            }
            if stderr_piped {
                match stderr_fd {
                    Some(fd) => {
                        if libc::dup2(fd, 2) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    // Piped stderr was requested but no stderr connection was
                    // opened (e.g. a legacy client); fall back to /dev/null
                    // rather than failing the spawn.
                    None => {
                        if dup_devnull(2, libc::O_WRONLY).is_err() {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
            } else if dup_devnull(2, libc::O_WRONLY).is_err() {
                return Err(std::io::Error::last_os_error());
            }
            for fd in [0, 1, 2] {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                }
            }
            Ok(())
        });
    }

    command
        .spawn()
        .with_context(|| format!("failed to spawn {}", spec.binary))
}

/// Redirects `target_fd` to `/dev/null` with the given open flags. Only valid
/// inside `pre_exec`; the temporary descriptor is closed right after the dup.
unsafe fn dup_devnull(target_fd: i32, flags: i32) -> std::io::Result<()> {
    let null = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, flags | libc::O_CLOEXEC);
    if null < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if libc::dup2(null, target_fd) < 0 {
        return Err(std::io::Error::last_os_error());
    }
    libc::close(null);
    Ok(())
}

/// Waits for the child to exit, enforcing an optional timeout, without
/// holding the child mutex between polls so a signal handler can interleave
/// and kill the process group while we wait.
///
/// Uses a pidfd so the wait is event-driven: `poll` wakes the moment the
/// child exits instead of spinning on `try_wait` every poll interval. On
/// kernels without pidfd support it falls back to the polling wait.
///
/// On timeout the whole process group is killed and reaped explicitly, so no
/// zombie or orphaned grandchild is left behind.
pub async fn wait_child_exit_shared(
    child: &Arc<smol::lock::Mutex<std::process::Child>>,
    timeout_ms: Option<u64>,
) -> Result<(Option<i32>, bool)> {
    let pid = child.lock().await.id() as i32;
    // SAFETY: pidfd_open(2) creates a new fd referring to the child. It does
    // not touch any existing resource, and ownership moves into the Async
    // wrapper which closes the fd on drop. Uses the raw syscall so it does not
    // depend on a recent libc.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if pidfd < 0 {
        log::warn!(
            "pidfd_open failed (errno={}), falling back to polling wait",
            std::io::Error::last_os_error()
        );
        return wait_child_exit_polling(child, timeout_ms).await;
    }
    // SAFETY: the raw fd was just created and is uniquely owned here.
    let pidfd = smol::Async::new(unsafe { OwnedFd::from_raw_fd(pidfd) })
        .with_context(|| "wrapping pidfd in Async".to_string())?;

    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
    loop {
        let wait = match deadline {
            Some(deadline) => deadline.saturating_duration_since(std::time::Instant::now()),
            None => Duration::from_secs(3600),
        };
        // Wait for the pidfd to become readable (the child exited) without
        // ever blocking the smol executor thread: a blocking `libc::poll`
        // here would stall every other connection on the executor.
        let ready = smol::future::or(
            async { pidfd.readable().await.is_ok() },
            async {
                smol::Timer::after(wait).await;
                false
            },
        )
        .await;
        if ready {
            // Child exited; reap its status.
            let status = { child.lock().await.try_wait()? };
            if let Some(status) = status {
                return Ok((status.code(), false));
            }
            // readable can fire just before the zombie is reapable; loop so
            // the next readable (immediately ready) lets try_wait succeed.
        } else {
            log::warn!(
                "child timed out after {}ms, killing process group",
                timeout_ms.unwrap_or_default()
            );
            let mut guard = child.lock().await;
            kill_process_group(&mut guard).await?;
            return Ok((None, true));
        }
    }
}

/// Polling fallback for kernels without pidfd support.
async fn wait_child_exit_polling(
    child: &Arc<smol::lock::Mutex<std::process::Child>>,
    timeout_ms: Option<u64>,
) -> Result<(Option<i32>, bool)> {
    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
    loop {
        let status = { child.lock().await.try_wait()? };
        if let Some(status) = status {
            return Ok((status.code(), false));
        }
        if let Some(deadline) = deadline {
            if std::time::Instant::now() >= deadline {
                log::warn!(
                    "child timed out after {}ms, killing process group",
                    timeout_ms.unwrap_or_default()
                );
                let mut guard = child.lock().await;
                kill_process_group(&mut guard).await?;
                return Ok((None, true));
            }
        }
        smol::Timer::after(CHILD_STATUS_POLL_INTERVAL).await;
    }
}

/// Kills the child's whole process group and reaps it.
pub async fn kill_process_group(child: &mut std::process::Child) -> Result<()> {
    let pid = child.id() as i32;
    if pid > 1 {
        // SAFETY: `-pid` targets the process group whose leader is the child.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let deadline = std::time::Instant::now() + REAP_TIMEOUT;
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() >= deadline {
            break;
        }
        smol::Timer::after(Duration::from_millis(10)).await;
    }
    Ok(())
}
