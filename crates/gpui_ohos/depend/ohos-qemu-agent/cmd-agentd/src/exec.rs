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
use std::time::Instant;

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
/// Interval between child-state samples while a command is running. Wall time
/// alone cannot distinguish "the command is computing slowly" from "the command
/// is blocked", so the worker samples the child's scheduler state and consumed
/// CPU time instead of inferring it.
const CHILD_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Total wall-clock budget for the filesystem micro-benchmark. The benchmark
/// runs detached from the mount path, so exceeding the budget only truncates
/// the numbers, never the mount.
const FS_BENCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
/// Persistent guest HOME (the `/sandbox` 9p export mirrors the device app
/// sandbox under `/data/storage/el2/base`). Pointing HOME here keeps LSP index
/// caches (clangd, rust-analyzer, ...) and other `~`-based tool state on the
/// device's persistent storage instead of the initramfs root, which is wiped on
/// every QEMU restart. The directory is created once by cmd-agentd at startup
/// (see main), then injected into every spawned command.
pub(crate) const GUEST_PERSISTENT_HOME: &str = "/sandbox/home";

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
        let cwd_check_start = std::time::Instant::now();
        if !std::path::Path::new(&cwd).exists() {
            log::info!("[diag] exec::build_command: creating cwd {cwd}");
            if let Err(err) = std::fs::create_dir_all(&cwd) {
                log::warn!("[diag] exec::build_command: create cwd {cwd}: {err}");
            }
        }
        log::info!("[diag] exec::build_command: cwd={cwd} exists checked in {:?}", cwd_check_start.elapsed());
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
    // Every guest command gets a persistent HOME under the sandbox mount so
    // LSP index caches and other ~-based state survive QEMU restarts (the
    // initramfs root does not). The directory is created once at agent start.
    cmd.env("HOME", GUEST_PERSISTENT_HOME);
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

/// State of a running child, read from /proc.
struct ChildSample {
    /// Scheduler state character from /proc/<pid>/stat ('R', 'S', 'D', 'Z'...).
    state: char,
    /// CPU milliseconds consumed by the child *and its reaped descendants*
    /// (utime+stime+cutime+cstime). A command that forks helpers therefore does
    /// not look idle just because the parent is waiting.
    cpu_millis: u64,
    threads: u64,
    /// Kernel wait channel of the main thread: the single most direct answer to
    /// "what is this process blocked on".
    wchan: String,
    /// Per-thread `tid:state:wchan` for up to six threads, so a multi-threaded
    /// command (git's preload index, for example) shows what its workers do.
    thread_details: Vec<String>,
}

fn clock_ticks_per_sec() -> u64 {
    // SAFETY: sysconf(_SC_CLK_TCK) is infallible for this argument.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

/// Reads one child's /proc snapshot. Returns `None` once the child is gone.
fn sample_child(pid: i32) -> Option<ChildSample> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` is parenthesized and may itself contain spaces, so index the
    // remaining fields from the last ')'.
    let tail = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let cutime: u64 = fields.get(13)?.parse().ok()?;
    let cstime: u64 = fields.get(14)?.parse().ok()?;
    let threads: u64 = fields.get(17)?.parse().ok()?;
    let cpu_millis = (utime + stime + cutime + cstime) * 1000 / clock_ticks_per_sec();
    let wchan = read_wchan(&format!("/proc/{pid}/wchan"));
    let mut thread_details = Vec::new();
    if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for entry in entries.flatten().take(6) {
            let tid = entry.file_name().to_string_lossy().into_owned();
            let state = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit_once(')').map(|(_, tail)| tail.to_string()))
                .and_then(|tail| {
                    tail.split_whitespace()
                        .next()
                        .and_then(|field| field.chars().next())
                })
                .unwrap_or('?');
            thread_details.push(format!(
                "{tid}:{state}:{}",
                read_wchan(&format!("/proc/{pid}/task/{tid}/wchan"))
            ));
        }
    }
    Some(ChildSample {
        state,
        cpu_millis,
        threads,
        wchan,
        thread_details,
    })
}

/// The kernel wait channel, or "-" when the kernel does not expose one.
fn read_wchan(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() || value == "0" {
                "-".to_string()
            } else {
                value.to_string()
            }
        }
        Err(_) => "-".to_string(),
    }
}

/// Guest-wide load and available memory, so a stalled command can be blamed on
/// CPU contention or memory pressure instead of guessed at.
fn guest_load_line() -> String {
    let load = std::fs::read_to_string("/proc/loadavg")
        .map(|line| {
            line.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_else(|_| "?".to_string());
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("MemAvailable:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .map(|kb| format!("{kb}kB"))
        })
        .unwrap_or_else(|| "?".to_string());
    format!("load={load} memavail={mem}")
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
    session_id: u64,
) {
    let mut cmd = build_command(&spec, &path_map);
    let spawn_start = std::time::Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::error!("[diag] worker_run_command: spawn: {err}");
            report_exit(result_tx, None);
            return;
        }
    };
    log::info!("[diag] worker_run_command: cmd.spawn took {:?}", spawn_start.elapsed());
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
    // [diag] Sampling state: how long the command has been running, how much
    // CPU the child has burned since the previous sample, and how many bytes
    // crossed each hop (child pipe -> port). Together these separate the three
    // ways a command can look hung: computing, blocked on the share, or blocked
    // because the host is not draining the port.
    let started_at = Instant::now();
    let mut last_sample_at: Option<Instant> = None;
    let mut last_cpu_millis: u64 = 0;
    let mut stdout_read_bytes: u64 = 0;
    let mut stderr_read_bytes: u64 = 0;
    let mut stdout_written_bytes: u64 = 0;
    let mut stderr_written_bytes: u64 = 0;
    // stdout/stderr from the child are buffered here and drained to the host
    // port on POLLOUT. The data/err ports are non-blocking now, so a full QEMU
    // chardev buffer (the host not draining guest stdout) must park this worker
    // with backpressure instead of dropping bytes on WouldBlock.
    let mut pending_stdout: Vec<u8> = Vec::new();
    let mut pending_stderr: Vec<u8> = Vec::new();
    let mut stdout_done = stdout_fd.is_none();
    let mut stderr_done = stderr_fd.is_none();
    let mut child_exited = false;
    let mut exited_at: Option<Instant> = None;
    let mut last_alive_log: Option<Instant> = None;
    let mut exit_code: Option<i32> = None;
    while !stdout_done
        || !stderr_done
        || !child_exited
        || !pending_stdout.is_empty()
        || !pending_stderr.is_empty()
    {
        // [diag] Periodic child sample. This is the only evidence that can tell
        // a slow command from a blocked one: a climbing cpu_millis with state R
        // means the guest is genuinely executing, a flat cpu_millis with state
        // D/S plus a wchan means the child is parked on something else.
        if last_sample_at.map_or(true, |at| at.elapsed() >= CHILD_SAMPLE_INTERVAL) {
            last_sample_at = Some(Instant::now());
            match sample_child(child_pid) {
                Some(sample) => {
                    let delta = sample.cpu_millis.saturating_sub(last_cpu_millis);
                    last_cpu_millis = sample.cpu_millis;
                    log::warn!(
                        "[diag] worker session_id={session_id} alive {}s pid={child_pid} state={} cpu={}ms(+{}ms) threads={} wchan={} | pipe_out={}B pipe_err={}B port_out={}B port_err={}B pend_out={} pend_err={} | exited={} out_done={} err_done={} | {} | threads=[{}]",
                        started_at.elapsed().as_secs(),
                        sample.state,
                        sample.cpu_millis,
                        delta,
                        sample.threads,
                        sample.wchan,
                        stdout_read_bytes,
                        stderr_read_bytes,
                        stdout_written_bytes,
                        stderr_written_bytes,
                        pending_stdout.len(),
                        pending_stderr.len(),
                        child_exited,
                        stdout_done,
                        stderr_done,
                        guest_load_line(),
                        sample.thread_details.join(" ")
                    );
                }
                None => {
                    log::warn!(
                        "[diag] worker session_id={session_id} alive {}s pid={child_pid} /proc sample unavailable exited={} out_done={} err_done={}",
                        started_at.elapsed().as_secs(),
                        child_exited,
                        stdout_done,
                        stderr_done
                    );
                }
            }
        }
        if !child_exited {
            if let Ok(Some(status)) = child.try_wait() {
                child_exited = true;
                exit_code = status.code();
                exited_at = Some(Instant::now());
                log::warn!(
                    "[diag] worker session_id={session_id} child exited exit_code={exit_code:?}, worker still looping: pending_stdout={} pending_stderr={} stdout_done={} stderr_done={}",
                    pending_stdout.len(), pending_stderr.len(), stdout_done, stderr_done
                );
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
        // [diag] child 已退出但 worker 仍在循环（未回传）-> 计时，定位"执行完返回有问题"
        if child_exited {
            if let Some(t0) = exited_at {
                let elapsed = t0.elapsed();
                if elapsed.as_secs() >= 1
                    && (last_alive_log.is_none()
                        || last_alive_log.unwrap().elapsed().as_secs() >= 5)
                {
                    log::warn!(
                        "[diag] worker session_id={session_id} still NOT exited {}s after child exit: pending_stdout={} pending_stderr={} stdout_done={} stderr_done={}",
                        elapsed.as_secs(), pending_stdout.len(), pending_stderr.len(), stdout_done, stderr_done
                    );
                    last_alive_log = Some(Instant::now());
                }
            }
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
                            stdout_written_bytes += written as u64;
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
                        Ok(n) => {
                            stdout_read_bytes += n as u64;
                            pending_stdout.extend_from_slice(&buf[..n]);
                        }
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
                        Ok(n) => {
                            stderr_read_bytes += n as u64;
                            pending_stderr.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            } else if err_fd == Some(p.fd) && p.revents & libc::POLLOUT != 0 {
                // Drain buffered stderr to the err port once it becomes
                // writable (events set above when pending_stderr is non-empty).
                if !pending_stderr.is_empty() {
                    match write_fd(err_fd.expect("matched Some"), &pending_stderr) {
                        Ok(written) => {
                            stderr_written_bytes += written as u64;
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
            "[diag] worker_run_mount: mount -t virtiofs {mount_tag} {guest_path} (attempt {attempts})"
        );
        let output = std::process::Command::new("mount")
            .args(["-t", "virtiofs", mount_tag, guest_path])
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
        // [diag] Measure the share in a detached thread. The numbers separate a
        // slow guest CPU (TCG) from a slow share (9p), which is otherwise
        // impossible to tell apart from a hanging command. Detached so MountOk
        // is not delayed: a benchmark line is a log, not the mount result.
        let bench_path = guest_path.to_string();
        match std::thread::Builder::new()
            .name("fs-bench".to_string())
            .spawn(move || run_fs_benchmark(&bench_path))
        {
            Ok(handle) => {
                // Detach: dropping the JoinHandle detaches the thread, which
                // keeps running until it returns. The mount worker must not
                // wait on the benchmark.
                drop(handle);
            }
            Err(err) => log::warn!("[diag] worker_run_mount: fs-bench spawn failed: {err}"),
        }
    }
    // report_mount closes the result pipe write end, so the event loop sees
    // EOF and reclaims the mount. The thread then returns.
    report_mount(result_tx, ok);
}

/// Filesystem micro-benchmark comparing the freshly mounted 9p share against
/// the guest's tmpfs. The same workload runs on both, so the delta is the
/// share's own cost: a tmpfs number close to the 9p number means the guest CPU
/// (TCG) is the bottleneck, a large gap means the share is. Bounded by
/// `FS_BENCH_BUDGET` so it can never stall anything; a truncated run is still a
/// useful data point. The result is a single `[diag][bench] result` log line.
fn run_fs_benchmark(guest_path: &str) {
    let deadline = Instant::now() + FS_BENCH_BUDGET;
    let mount_line = std::fs::read_to_string("/proc/mounts")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.split_whitespace().nth(1) == Some(guest_path))
                .map(|line| line.to_string())
        })
        .unwrap_or_else(|| "not-found".to_string());
    log::warn!("[diag][bench] start mount-line: {mount_line}");
    let tmpfs_dir = std::path::PathBuf::from("/tmp/zcoder-fsbench");
    let share_dir = std::path::Path::new(guest_path).join(".zcoder-fsbench");
    for dir in [&tmpfs_dir, &share_dir] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            log::warn!("[diag][bench] mkdir {dir:?}: {err}");
        }
    }
    let tmpfs = bench_one(&tmpfs_dir, deadline);
    let share = bench_one(&share_dir, deadline);
    log::warn!(
        "[diag][bench] result(ms) | stat_hot tmpfs={} share={} | scan500 tmpfs={} share={} | write1M tmpfs={} share={} | read1M tmpfs={} share={} | share_wrote={}",
        tmpfs.stat_hot_ms,
        share.stat_hot_ms,
        tmpfs.scan_ms,
        share.scan_ms,
        tmpfs.write_ms,
        share.write_ms,
        tmpfs.read_ms,
        share.read_ms,
        share.wrote,
    );
    // Remove scratch dirs so the benchmark leaks nothing into the work tree
    // (the 9p branch writes onto the real device sandbox).
    let _ = std::fs::remove_dir_all(&tmpfs_dir);
    let _ = std::fs::remove_dir_all(&share_dir);
}

/// One directory's worth of the benchmark. Each stage is skipped once `deadline`
/// passes, yielding a partial but still informative `BenchOne`.
struct BenchOne {
    stat_hot_ms: u64,
    scan_ms: u64,
    write_ms: u64,
    read_ms: u64,
    wrote: bool,
}

fn bench_one(dir: &std::path::Path, deadline: Instant) -> BenchOne {
    let mut result = BenchOne {
        stat_hot_ms: 0,
        scan_ms: 0,
        write_ms: 0,
        read_ms: 0,
        wrote: false,
    };
    let probe = dir.join("probe.bin");
    // 1) write 1 MiB in 8 KiB chunks -- the default 9p msize is 8 KiB, so this
    //    directly exposes how many round trips a modest write costs.
    if Instant::now() < deadline {
        let chunk = vec![0u8; 8 * 1024];
        if let Ok(mut file) = std::fs::File::create(&probe) {
            let start = Instant::now();
            let mut ok_write = true;
            for _ in 0..128 {
                if std::io::Write::write_all(&mut file, &chunk).is_err() {
                    ok_write = false;
                    break;
                }
            }
            let _ = file.sync_all();
            result.write_ms = start.elapsed().as_millis() as u64;
            result.wrote = ok_write;
        }
    }
    // 2) read 1 MiB in 8 KiB chunks.
    if Instant::now() < deadline {
        let mut buf = vec![0u8; 8 * 1024];
        if let Ok(mut file) = std::fs::File::open(&probe) {
            let start = Instant::now();
            while std::io::Read::read(&mut file, &mut buf).map_or(false, |n| n > 0) {}
            result.read_ms = start.elapsed().as_millis() as u64;
        }
    }
    // 3) stat_hot: stat the same existing file 500 times -- a pure round trip
    //    with a warm cache, so the number is the per-call 9p/host latency.
    if Instant::now() < deadline {
        let start = Instant::now();
        for _ in 0..500 {
            let _ = std::fs::metadata(&probe);
        }
        result.stat_hot_ms = start.elapsed().as_millis() as u64;
    }
    // 4) scan: create+stat 500 distinct files then remove them -- a cold-ish
    //    directory scan, the closest stand-in for what `git status -uall` does.
    if Instant::now() < deadline {
        let start = Instant::now();
        for i in 0..500u32 {
            let p = dir.join(format!("scan-{i}"));
            if std::fs::write(&p, b"x").is_ok() {
                let _ = std::fs::metadata(&p);
            }
        }
        for i in 0..500u32 {
            let _ = std::fs::remove_file(dir.join(format!("scan-{i}")));
        }
        result.scan_ms = start.elapsed().as_millis() as u64;
    }
    let _ = std::fs::remove_file(&probe);
    result
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
