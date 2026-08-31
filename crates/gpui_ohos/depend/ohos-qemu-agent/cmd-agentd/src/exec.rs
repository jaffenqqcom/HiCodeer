//! Command assembly and the worker-thread entry points.
//!
//! `build_command` rewrites paths through the mapping table and builds a
//! `std::process::Command`. The `worker_run_*` functions run in a worker
//! thread spawned by the agent's event-loop thread: they share the agent's
//! virtio-serial port fds, do the blocking work (exec, stdio forwarding,
//! mount) and report back through a result pipe. Each command is spawned with
//! `Command::spawn` (glibc posix_spawn, no full-mm fork) as its own process
//! group leader, so signals can target the command independently.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use qemu_cmd_agent_protocol::messages::{ExecSpec, FdMode};

use qemu_cmd_agentd::path_map::PathMap;

/// Bound on how much child stdout/stderr may buffer in guest memory before the
/// worker stops reading from the child (pipe backpressure) so the host not
/// draining the port cannot grow the buffers without bound (P2-5).
const MAX_PENDING_OUTPUT_BYTES: usize = 64 * 1024;
/// Bound on waiting for the child to exit after the forwarding loop ended; a
/// daemonized child must not park the worker thread forever (P2-4).
const WAIT_CHILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long the mount worker retries the 9p mount after a device-not-ready
/// failure. The QMP device_add that exports a folder completes before the guest
/// kernel enumerates the new virtio-9p device, so mounting immediately can fail
/// with "device not found" (exit 255); this window covers the enumeration delay.
const MOUNT_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
/// Delay between mount retry attempts.
const MOUNT_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// Builds a `Command` from an `ExecSpec`, rewriting `binary`, every argument
/// and the working directory through the path mapping table. Arguments that
/// match no root are passed through unchanged.
pub fn build_command(spec: &ExecSpec, path_map: &PathMap) -> Command {
    let binary = path_map.map_path(&spec.binary);
    log::info!("[diag] exec::build_command: program={} binary={} args={:?}",
        spec.source_program,
        binary,
        spec.args
    );
    let mut cmd = Command::new(&binary);
    for arg in &spec.args {
        cmd.arg(path_map.map_path(arg));
    }
    if let Some(cwd) = &spec.cwd_path {
        let cwd = path_map.map_path(cwd);
        // Mirror the OpenEuler cmd-agentd fix: a mapped cwd may not exist yet
        // on the guest (npm --prefix creates it lazily, but spawn needs it).
        if !std::path::Path::new(&cwd).exists() {
            log::info!("[diag] exec::build_command: creating cwd {cwd}");
            if let Err(err) = std::fs::create_dir_all(&cwd) {
                log::warn!("[diag] exec::build_command: create cwd {cwd}: {err}");
            }
        }
        log::info!("[diag] exec::build_command: cwd={cwd}");
        cmd.current_dir(cwd);
    }
    // Dump every env var with its full value: env values are NOT path-mapped,
    // so a guest-path leak (e.g. GIT_INDEX_FILE pointing at a device path) is
    // visible here in one line.
    let env_text: Vec<String> = spec
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    log::info!("[diag] exec::build_command: env: {}", env_text.join(" | "));
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    // The guest mounts work directories with 9p passthrough, so the mounted
    // files keep the host-side owner UID while git runs here as root. git's
    // dubious-ownership guard (CVE-2022-24765) then rejects every work-dir
    // repository. Inject safe.directory through git's environment-config
    // mechanism (GIT_CONFIG_COUNT) so those mounts are accepted without
    // changing the command line or the guest image.
    if spec.source_program == "git" {
        cmd.env("GIT_CONFIG_COUNT", "1");
        cmd.env("GIT_CONFIG_KEY_0", "safe.directory");
        cmd.env("GIT_CONFIG_VALUE_0", "*");
        log::info!("[diag] exec::build_command: git: injected GIT_CONFIG safe.directory=*");
    }
    cmd.stdin(if spec.stdin_mode == FdMode::Null {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    cmd.stdout(if spec.stdout_mode == FdMode::Null {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    cmd.stderr(if spec.stderr_mode == FdMode::Null {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    // The command is made its own process group leader (process_group(0), set
    // before exec), so a signal to the group (kill(-cmd_pid)) reaches the
    // command and its children -- a killed shell leaves no orphaned
    // grandchildren. The worker thread is not a process group leader, so the
    // group is anchored on the command itself.
    cmd.process_group(0);
    log::info!(
        "[diag] exec::build_command: final binary={binary} args={:?} cwd={:?}",
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        cmd.get_current_dir().map(|p| p.display().to_string())
    );
    cmd
}

/// Worker-process command runner. Runs in the forked worker of a data port:
/// builds the command, spawns it, forwards stdio over the inherited data/err
/// port fds and reports the exit code through the result pipe. Never returns.
pub fn worker_run_command(
    data_fd: i32,
    err_fd: Option<i32>,
    spec: ExecSpec,
    path_map: PathMap,
    start_rx: i32,
    result_tx: i32,
    cmd_pid: Arc<AtomicI32>,
    stdin_closed: Arc<AtomicBool>,
) {
    let mut cmd = build_command(&spec, &path_map);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::error!("[diag] worker_run_command: spawn: {err}");
            report_exit(result_tx, None);
            return;
        }
    };
    let child_pid = child.id() as i32;
    cmd_pid.store(child_pid, Ordering::SeqCst);
    log::info!("[diag] worker_run_command: pid={child_pid}");
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    // Wait for the parent to finish writing SpawnOk before emitting output, so
    // the frame is never interleaved with command stdout.
    let mut start_byte = [0u8; 1];
    unsafe { libc::read(start_rx, start_byte.as_mut_ptr() as *mut libc::c_void, 1) };
    unsafe { libc::close(start_rx) };
    log::info!("[diag] worker_run_command: start signal received, forwarding begins");

    if !spec.stdin.is_empty() {
        if let Some(s) = stdin.as_mut() {
            if let Err(err) = s.write_all(&spec.stdin) {
                log::warn!("[diag] worker_run_command: write inline stdin: {err}");
            } else {
                log::info!(
                    "[diag] worker_run_command: wrote {} inline stdin bytes",
                    spec.stdin.len()
                );
            }
        }
    }

    let port_fd = data_fd;
    let stdout_fd = stdout.as_ref().map(|s| s.as_raw_fd());
    let stdin_fd = stdin.as_ref().map(|s| s.as_raw_fd());
    let stderr_fd = stderr.as_ref().map(|s| s.as_raw_fd());
    if let Some(fd) = stdin_fd {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
    }
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(5);
    pollfds.push(libc::pollfd {
        fd: port_fd,
        events: libc::POLLIN,
        revents: 0,
    });
    // Keep the poll-set indices for stdout/stderr so their fds can be detached
    // once EOF is seen; otherwise an EOF'd pipe polls as POLLIN|POLLHUP forever
    // and busy-spins this worker until the child exits (P0-1).
    let stdout_poll_index = if let Some(fd) = stdout_fd {
        pollfds.push(libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        Some(pollfds.len() - 1)
    } else {
        None
    };
    let stderr_poll_index = if let Some(fd) = stderr_fd {
        pollfds.push(libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        Some(pollfds.len() - 1)
    } else {
        None
    };
    let stdin_poll_index = if let Some(fd) = stdin_fd {
        pollfds.push(libc::pollfd {
            fd,
            events: 0,
            revents: 0,
        });
        Some(pollfds.len() - 1)
    } else {
        None
    };
    let err_poll_index = if let Some(fd) = err_fd {
        pollfds.push(libc::pollfd {
            fd,
            events: 0,
            revents: 0,
        });
        Some(pollfds.len() - 1)
    } else {
        None
    };

    let mut buf = [0u8; 8192];
    let mut pending_stdin: Vec<u8> = Vec::new();
    // stdout/stderr from the child are buffered here and drained to the host
    // port on POLLOUT. The data/err ports are non-blocking now, so a full QEMU
    // chardev buffer (the host not draining guest stdout) must park this worker
    // with backpressure instead of dropping bytes on WouldBlock.
    let mut pending_stdout: Vec<u8> = Vec::new();
    let mut pending_stderr: Vec<u8> = Vec::new();
    let mut stdout_done = stdout_fd.is_none();
    let mut stderr_done = stderr_fd.is_none();
    let mut child_exited = false;
    let mut exit_code: Option<i32> = None;
    while !stdout_done
        || !stderr_done
        || !child_exited
        || !pending_stdout.is_empty()
        || !pending_stderr.is_empty()
    {
        if !child_exited {
            if let Ok(Some(status)) = child.try_wait() {
                child_exited = true;
                exit_code = status.code();
                if exit_code.is_none() {
                    use std::os::unix::process::ExitStatusExt;
                    log::warn!(
                        "[diag] worker_run_command: child killed in loop, signal={:?}",
                        status.signal()
                    );
                }
            }
        }
        if stdout_done
            && stderr_done
            && child_exited
            && pending_stdout.is_empty()
            && pending_stderr.is_empty()
        {
            break;
        }
        // Dynamic POLLOUT: drain stdout to the port and stderr to the err port
        // only while buffered output remains, so a full chardev buffer parks
        // this worker (backpressure) instead of blocking on the fd.
        pollfds[0].events =
            libc::POLLIN | if pending_stdout.is_empty() { 0 } else { libc::POLLOUT };
        if let Some(idx) = stdin_poll_index {
            pollfds[idx].events = if pending_stdin.is_empty() {
                0
            } else {
                libc::POLLOUT
            };
        }
        if let Some(idx) = err_poll_index {
            pollfds[idx].events = if pending_stderr.is_empty() {
                0
            } else {
                libc::POLLOUT
            };
        }
        // Backpressure (P2-5): stop reading child stdout/stderr once the
        // buffered bytes hit the bound -- the child's pipe fills and the child
        // blocks -- and resume once the host drains the port below the bound.
        // This caps guest memory for a producer that outruns the host.
        if let Some(idx) = stdout_poll_index {
            pollfds[idx].events = if pending_stdout.len() >= MAX_PENDING_OUTPUT_BYTES {
                0
            } else {
                libc::POLLIN
            };
        }
        if let Some(idx) = stderr_poll_index {
            pollfds[idx].events = if pending_stderr.len() >= MAX_PENDING_OUTPUT_BYTES {
                0
            } else {
                libc::POLLIN
            };
        }
        // SAFETY: poll over stable fds held for the whole command.
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 100) };
        if rc < 0 {
            log::warn!("[diag] worker_run_command: poll: {}", std::io::Error::last_os_error());
            break;
        }
        if rc == 0 {
            continue;
        }
        let mut disable_stdin = false;
        // The host signals stdin closure over the management connection (the
        // data socket is shared with stdout and cannot be half-closed). Drop
        // the child's stdin pipe so the process observes EOF and can exit.
        if stdin_closed.load(Ordering::SeqCst) {
            if stdin.is_some() {
                log::info!("[diag] worker_run_command: stdin closed signal, closing child stdin");
                stdin = None;
                disable_stdin = true;
            }
        }
        for p in pollfds.iter_mut() {
            if p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLOUT) == 0 {
                continue;
            }
            if p.fd == port_fd {
                if p.revents & libc::POLLIN != 0 {
                    // Data from zcoder is the child's stdin.
                    match read_fd(port_fd, &mut buf) {
                        Ok(0) => {
                            stdin = None;
                            disable_stdin = true;
                        }
                        Ok(n) => pending_stdin.extend_from_slice(&buf[..n]),
                        // A non-blocking port may report POLLIN with no bytes
                        // yet; that is not an EOF, so keep the stdin open.
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(err) => {
                            log::warn!("[diag] worker_run_command: stdin read: {err}");
                            stdin = None;
                            disable_stdin = true;
                        }
                    }
                }
                if p.revents & libc::POLLOUT != 0 && !pending_stdout.is_empty() {
                    match write_fd(port_fd, &pending_stdout) {
                        Ok(written) => {
                            log::debug!(
                                "[diag] worker_run_command: wrote {written} stdout bytes to port"
                            );
                            pending_stdout.drain(..written);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(err) => {
                            log::warn!("[diag] worker_run_command: stdout to port: {err}");
                            stdout_done = true;
                        }
                    }
                }
            } else if stdout_fd == Some(p.fd) {
                if let Some(out) = stdout.as_mut() {
                    match out.read(&mut buf) {
                        Ok(0) => stdout_done = true,
                        Err(_) => stdout_done = true,
                        Ok(n) => pending_stdout.extend_from_slice(&buf[..n]),
                    }
                }
            } else if stderr_fd == Some(p.fd) {
                if let Some(serr) = stderr.as_mut() {
                    match serr.read(&mut buf) {
                        // Mirror stdout: mark EOF so the pollfd is detached
                        // below and the loop can terminate once stderr is also
                        // drained, instead of spinning on the EOF'd pipe.
                        Ok(0) => stderr_done = true,
                        Err(_) => stderr_done = true,
                        Ok(n) => pending_stderr.extend_from_slice(&buf[..n]),
                    }
                }
            } else if err_fd == Some(p.fd) && p.revents & libc::POLLOUT != 0 {
                // Drain buffered stderr to the err port once it becomes
                // writable (events set above when pending_stderr is non-empty).
                if !pending_stderr.is_empty() {
                    match write_fd(err_fd.expect("matched Some"), &pending_stderr) {
                        Ok(written) => {
                            log::debug!(
                                "[diag] worker_run_command: wrote {written} stderr bytes to err port"
                            );
                            pending_stderr.drain(..written);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(err) => {
                            log::warn!("[diag] worker_run_command: stderr to err port: {err}");
                            break;
                        }
                    }
                }
            } else if stdin_fd == Some(p.fd) && p.revents & libc::POLLOUT != 0 {
                let write_failed = if let Some(s) = stdin.as_mut() {
                    if !pending_stdin.is_empty() {
                        match s.write(&pending_stdin) {
                            Ok(written) => {
                                log::debug!(
                                    "[diag] worker_run_command: wrote {written} stdin bytes to child"
                                );
                                pending_stdin.drain(..written);
                                false
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
                            Err(err) => {
                                log::warn!("[diag] worker_run_command: stdin write: {err}");
                                pending_stdin.clear();
                                true
                            }
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if write_failed {
                    stdin = None;
                    disable_stdin = true;
                }
            }
        }
        if disable_stdin {
            if let Some(idx) = stdin_poll_index {
                pollfds[idx].fd = -1;
            }
        }
        // Detach EOF'd output pipes: an EOF'd pipe keeps polling POLLIN|POLLHUP
        // forever, which would make poll return immediately and busy-spin this
        // worker until the child exits. With them detached, poll blocks on the
        // remaining fds and the 100ms timeout drives try_wait() instead.
        if stdout_done {
            if let Some(idx) = stdout_poll_index {
                pollfds[idx].fd = -1;
            }
        }
        if stderr_done {
            if let Some(idx) = stderr_poll_index {
                pollfds[idx].fd = -1;
            }
        }
    }

    // Reap if the loop left before the child exited (stdout EOF without exit,
    // or an error broke the loop). Bounded (P2-4): a daemonized child that
    // never exits must not park the worker thread forever, so give up after
    // WAIT_CHILD_TIMEOUT and report what we have.
    if !child_exited {
        let deadline = std::time::Instant::now() + WAIT_CHILD_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                exit_code = status.code();
                if exit_code.is_none() {
                    use std::os::unix::process::ExitStatusExt;
                    log::warn!(
                        "[diag] worker_run_command: child killed on wait, signal={:?}",
                        status.signal()
                    );
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                log::warn!(
                    "[diag] worker_run_command: child did not exit within {WAIT_CHILD_TIMEOUT:?}, giving up"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    log::info!("[diag] worker_run_command: exit_code={exit_code:?}");
    // report_exit closes the result pipe write end, so the event loop's poll
    // on the read end sees EOF and reclaims the port. The thread then returns
    // (no process to _exit); the command process was reaped by child.wait().
    report_exit(result_tx, exit_code);
}

/// Worker-process mount runner. Runs in the forked worker of a mount request:
/// creates the mount point and mounts the 9p tag; reports success/failure as a
/// one-byte result. Never returns.
pub fn worker_run_mount(mount_tag: &str, guest_path: &str, result_tx: i32) {
    if let Err(err) = std::fs::create_dir_all(guest_path) {
        log::error!("[diag] worker_run_mount: create {guest_path}: {err}");
        report_mount(result_tx, false);
        return;
    }
    if is_mounted(guest_path) {
        log::info!("[diag] worker_run_mount: {guest_path} already mounted");
        report_mount(result_tx, true);
        return;
    }
    // The QMP device_add that exports the folder completes before the guest
    // kernel enumerates the new virtio-9p device, so mounting immediately can
    // fail with a "device not found" (exit 255). Retry within a short window so
    // the device is ready before giving up. stderr is captured for diagnosis.
    let deadline = std::time::Instant::now() + MOUNT_RETRY_WINDOW;
    let mut attempts = 0;
    let ok = loop {
        attempts += 1;
        log::info!(
            "[diag] worker_run_mount: mount -t 9p -o trans=virtio,version=9p2000.L {mount_tag} {guest_path} (attempt {attempts})"
        );
        let output = std::process::Command::new("mount")
            .args([
                "-t",
                "9p",
                "-o",
                "trans=virtio,version=9p2000.L",
                mount_tag,
                guest_path,
            ])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                log::info!(
                    "[diag] worker_run_mount: mount result raw=Ok(Some({}))",
                    output.status.code().unwrap_or(-1)
                );
                break true;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "[diag] worker_run_mount: mount attempt {attempts} failed, stderr={}",
                    stderr.trim()
                );
            }
            Err(err) => {
                log::warn!("[diag] worker_run_mount: mount spawn failed: {err}");
            }
        }
        if std::time::Instant::now() >= deadline {
            log::error!(
                "[diag] worker_run_mount: mount {mount_tag} failed after {attempts} attempts"
            );
            break false;
        }
        std::thread::sleep(MOUNT_RETRY_INTERVAL);
    };
    if ok {
        log::info!("[diag] worker_run_mount: {mount_tag} mounted at {guest_path}");
    }
    // report_mount closes the result pipe write end, so the event loop sees
    // EOF and reclaims the mount. The thread then returns.
    report_mount(result_tx, ok);
}

/// Whether `path` is a mount point listed in /proc/mounts.
pub fn is_mounted(path: &str) -> bool {
    log::info!("[diag] is_mounted: checking {path}");
    if let Ok(content) = std::fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            if let Some(mount_point) = line.split_whitespace().nth(1) {
                if mount_point == path {
                    log::info!("[diag] is_mounted: {path} IS mounted");
                    return true;
                }
            }
        }
    }
    log::info!("[diag] is_mounted: {path} NOT mounted");
    false
}

fn read_fd(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_fd(fd: i32, buf: &[u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Writes the 4-byte exit code (or -1 for a signal/unknown exit) to the result
/// pipe, then closes it.
fn report_exit(result_tx: i32, exit_code: Option<i32>) {
    let code = exit_code.unwrap_or(-1);
    log::info!("[diag] report_exit: code={code} (option={exit_code:?})");
    let bytes = code.to_le_bytes();
    unsafe { libc::write(result_tx, bytes.as_ptr() as *const libc::c_void, 4) };
    unsafe { libc::close(result_tx) };
}

/// Writes a one-byte mount result (0 ok, 1 failed) to the result pipe.
fn report_mount(result_tx: i32, ok: bool) {
    log::info!("[diag] report_mount: ok={ok}");
    let byte = [if ok { 0u8 } else { 1u8 }];
    unsafe { libc::write(result_tx, byte.as_ptr() as *const libc::c_void, 1) };
    unsafe { libc::close(result_tx) };
}
