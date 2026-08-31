//! In-process QEMU runner for HarmonyOS NEXT.
//!
//! The HarmonyOS sandbox forbids spawning external processes, so the Linux
//! toolchain (git, LSP servers, ...) must run inside a QEMU guest instead.
//! This crate loads `libqemu-system-aarch64.so` with dlopen and boots the
//! guest on a dedicated thread, mirroring OHcode's qemu_runner.cpp approach
//! but driven from the Rust side so the OHOS entry (launch-zed) can start it.

pub mod executor;
pub mod qmp;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Name of the QEMU engine library shipped in the HAP native-libs directory.
const QEMU_LIB_NAME: &str = "libqemu-system-aarch64.so";
/// Symbol exported by the QEMU engine that takes over the current thread and
/// runs the machine until it exits. The rebuilt qemu-ohos library exports
/// `main`; the OHcode prebuilt library exported `qemu_system_entry`.
const QEMU_ENTRY_SYMBOL: &[u8] = b"main\0";

/// Machine type and CPU model matching OHcode's proven guest configuration.
/// cortex-a76 (ARMv8.2) instead of cortex-a57 (ARMv8.0): the OpenEuler guest's
/// git binary is compiled for a newer arch and hits SIGILL on ARMv8.0.
const MACHINE_TYPE: &str = "virt";
const CPU_MODEL: &str = "cortex-a76";
/// Number of guest CPU cores. Keep enough for LSP workers; idle cores sleep
/// via `idle=halt` + `-icount sleep=on`, so they cost nothing when idle.
const CPU_SMP: &str = "4";
/// Guest memory size; kept modest because the app shares the device RAM.
const MEM_SIZE: &str = "8G";

/// Kernel command line: serial console on ttyAMA0, boot straight into /sbin/init.
/// `idle=halt` forces the guest to idle with WFI so `-icount sleep=on` can put
/// the TCG threads to sleep (without it the guest polls idle and TCG burns a
/// full host core).
const KERNEL_CMDLINE: &str =
    "console=ttyAMA0,115200 rdinit=/sbin/init idle=halt ip=dhcp";

/// virtio-9p mount tag exposing the app sandbox (downloads, languages, ...).
/// Shared with the executor (`cmd-agent/src/executor.rs`), which sends the
/// MountFolder2QEMU request for the sandbox root.
pub(crate) const MOUNT_TAG_SANDBOX: &str = "sandbox";

/// virtio-9p mount tag for the read-only HAP resfile tree (prebundled tools:
/// clangd, python3, ssh, ... plus C/C++ headers). Shared to the guest as /tools
/// so binaries stay on the device bundle and are never copied into the sandbox.
pub(crate) const MOUNT_TAG_TOOLS: &str = "tools";

/// Management virtio-serial port name (ExecResult / Signal / mount / ack).
const MGMT_PORT_NAME: &str = "zcoder.mgmt";
/// Data port name prefix (`zcoder.cmd.<n>`, stdin/stdout per command).
const DATA_PORT_NAME_PREFIX: &str = "zcoder.cmd.";
/// stderr port name prefix (`zcoder.err.<n>`, stderr per command).
const ERR_PORT_NAME_PREFIX: &str = "zcoder.err.";
/// QMP control socket for runtime fsdev-add / device_add (work-directory
/// mounts), living under the port dir like the virtio-serial sockets.
pub(crate) const QMP_SOCKET: &str = "qmp.sock";
/// Max concurrently spawned commands; the port pool is 2*N+1 ports. A single
/// virtio-serial bus caps at 30 ports, so N is bounded by (30-1)/2 = 14.
/// `pub(crate)` so the executor (`cmd-agent/src/executor.rs`) uses the same
/// single definition instead of a copy.
pub(crate) const PORT_POOL_SIZE: usize = 14;
/// Number of pre-created pcie-root-ports for runtime hotplug of work-directory
/// virtio-9p devices. Each root port exposes a single hotplug slot, so one port
/// per concurrent mounted folder is needed. Shared with the executor, which
/// picks the root port by mount sequence modulo this count.
pub(crate) const WORKDIR_MOUNT_SLOTS: usize = 8;

/// Entry point signature of the loaded QEMU engine library.
type QemuSystemEntry = unsafe extern "C" fn(argc: c_int, argv: *const *const c_char) -> c_int;

/// Dlopen handle owned by the QEMU thread. The raw pointer is never shared;
/// it is only dereferenced on the machine thread (dlclose on shutdown), so
/// marking it Send is sound.
struct EngineHandle(*mut c_void);

// SAFETY: the handle is created in `start` and moved straight into the QEMU
// thread, which is the only place that dereferences it.
unsafe impl Send for EngineHandle {}

/// Whether the QEMU engine thread is currently running. Set before the machine
/// thread spawns and cleared by its closure after `run_machine` returns, so a
/// thread that ended cleanly flips the flag itself. QEMU runs on a dedicated
/// thread (not a forked child): a clean quit returns from `main` and the thread
/// unwinds normally. A fatal path inside the engine that calls `exit()`/`abort()`
/// terminates the whole app process -- there is no process boundary any more.
static QEMU_RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether the QEMU engine thread is still alive.
fn qemu_running() -> bool {
    QEMU_RUNNING.load(Ordering::SeqCst)
}

/// Filesystem layout handed to the guest: kernel/initrd locations plus the
/// shared folders the guest mounts over virtio-9p.
#[derive(Clone)]
pub struct QemuPaths {
    /// Kernel image (packaged in the HAP resfile, read-only).
    pub kernel: PathBuf,
    /// Root filesystem initramfs (packaged in the HAP resfile, read-only).
    pub initrd: PathBuf,
    /// Directory holding the virtio-serial port pool unix sockets
    /// (mgmt.sock, cmd.<n>.sock, err.<n>.sock).
    pub port_dir: PathBuf,
    /// Host directory shared to the guest as the "sandbox" mount.
    pub sandbox_mount: PathBuf,
    /// Read-only HAP resfile directory (el1/bundle module resources), shared
    /// to the guest as the "tools" mount so prebundled binaries (clangd,
    /// python3, ssh) are reachable without copying them into the sandbox.
    pub tools_mount: PathBuf,
}

/// Starts the QEMU guest on a dedicated thread and leaves it running. Returns
/// false when the engine library cannot be loaded or the thread cannot spawn,
/// or true when the machine thread launched (it keeps running in the
/// background; guest boot is asynchronous). The QEMU thread is never
/// restarted: it is started once here and lives for the whole app session.
pub fn start(paths: QemuPaths) -> bool {
    if qemu_running() {
        log::info!("[diag] ohos_qemu::start: already running, returning true");
        return true;
    }

    let argv = build_argv(&paths);

    log::info!("[diag] ohos_qemu::start: kernel={} initrd={} port_dir={} sandbox={} argc={}",
        paths.kernel.display(),
        paths.initrd.display(),
        paths.port_dir.display(),
        paths.sandbox_mount.display(),
        argv.len()
    );

    // Run the guest on a dedicated thread (not a forked child). The engine is
    // dlopen'd and `main` takes over that thread until the guest shuts down;
    // a clean quit returns from `main` and the thread unwinds normally. The
    // JoinHandle is dropped on purpose: dropping it detaches the thread, so it
    // keeps running for the lifetime of the app. The virtio-serial sockets and
    // the 9p mounts are shared with the app through the same fd table, so the
    // executor behaves exactly as before. Trade-off: the engine now lives
    // inside the app process, so any `exit()`/`abort()` on a fatal error path
    // terminates the whole app (a forked child would only kill itself).
    QEMU_RUNNING.store(true, Ordering::SeqCst);
    match std::thread::Builder::new()
        .name("qemu-machine".to_string())
        .spawn(move || {
            // Load the engine and run QEMU main. If the engine cannot be
            // loaded, log and unwind -- never fall through into app code.
            if let Some((handle, entry)) = load_engine() {
                run_machine(EngineHandle(handle), entry, argv);
            } else {
                log::error!("[diag] ohos_qemu::start: engine load failed on machine thread");
            }
            QEMU_RUNNING.store(false, Ordering::SeqCst);
        }) {
        Ok(_handle) => {
            log::info!("[diag] ohos_qemu::start: guest thread spawned");
            true
        }
        Err(err) => {
            QEMU_RUNNING.store(false, Ordering::SeqCst);
            log::error!("[diag] ohos_qemu::start: thread spawn failed: {err}");
            false
        }
    }
}

/// Loads the QEMU engine library and resolves the `main` entry symbol.
/// Returns the raw handle and the resolved entry point on success.
fn load_engine() -> Option<(*mut c_void, QemuSystemEntry)> {
    let lib_name = CString::new(QEMU_LIB_NAME).ok()?;
    // SAFETY: dlopen/dlsym with NUL-terminated C strings; the handle stays
    // open for the lifetime of the guest thread (moved into it).
    let handle = unsafe { libc::dlopen(lib_name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let err = last_dl_error("unknown dlopen error");
        log::error!("[diag] ohos_qemu::load_engine: dlopen {QEMU_LIB_NAME} failed: {err}");
        return None;
    }

    let symbol = unsafe { libc::dlsym(handle, QEMU_ENTRY_SYMBOL.as_ptr() as *const c_char) };
    if symbol.is_null() {
        let err = last_dl_error("unknown dlsym error");
        log::error!("[diag] ohos_qemu::load_engine: dlsym main failed: {err}");
        return None;
    }

    let entry: QemuSystemEntry = unsafe { std::mem::transmute(symbol) };
    log::info!("[diag] ohos_qemu::load_engine: main resolved at {symbol:p}");
    Some((handle, entry))
}

/// Redirects QEMU stderr (engine errors) and stdout (guest serial console) into
/// two pipes drained by a single poll() thread, re-emitting chunks into hilog.
/// Debug-only: release builds skip the pipe thread and output falls to null.
#[cfg(feature = "qemu_debug_assertions")]
fn redirect_fds_to_log() {
    let mut out_fds = [0 as c_int; 2];
    let mut err_fds = [0 as c_int; 2];
    // SAFETY: pipe/dup2/close are plain syscalls; each pair is owned by this
    // thread (write end) and the drain thread (read end).
    if unsafe { libc::pipe(out_fds.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(err_fds.as_mut_ptr()) } != 0
    {
        log::error!("[diag] ohos_qemu::redirect_fds_to_log: pipe failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    unsafe {
        libc::dup2(out_fds[1], libc::STDOUT_FILENO);
        libc::close(out_fds[1]);
        libc::dup2(err_fds[1], libc::STDERR_FILENO);
        libc::close(err_fds[1]);
    }
    std::thread::Builder::new()
        .name("qemu-output-drain".to_string())
        .spawn(move || drain_fds(out_fds[0], err_fds[0]))
        .ok();
    log::info!("[diag] ohos_qemu::redirect_fds_to_log: stdout/stderr captured via pipes");
}

/// Drains the stdout and stderr pipes with one poll() loop and re-emits whole
/// lines into hilog: guest console at info level, engine errors at error level.
/// Bytes are buffered per pipe until a newline, so a line split across reads is
/// still printed as one complete sentence instead of one fragment per read.
#[cfg(feature = "qemu_debug_assertions")]
fn drain_fds(out_fd: c_int, err_fd: c_int) {
    let mut pollfds = [
        libc::pollfd { fd: out_fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: err_fd, events: libc::POLLIN, revents: 0 },
    ];
    let mut out_lines = Vec::new();
    let mut err_lines = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: poll blocks until either pipe has data or is closed; the two
        // fds stay valid until this thread exits.
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, -1) };
        if rc <= 0 {
            break;
        }
        for p in pollfds.iter_mut() {
            if p.fd < 0 || p.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                continue;
            }
            let is_out = p.fd == out_fd;
            let line_buf = if is_out { &mut out_lines } else { &mut err_lines };
            // SAFETY: read on the pipe fd; 0 or -1 means the write end closed.
            let n = unsafe { libc::read(p.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n <= 0 {
                // Flush whatever is left (no trailing newline) before closing.
                emit_remaining_lines(line_buf, is_out);
                // SAFETY: matching the pipe read fds opened above.
                unsafe { libc::close(p.fd) };
                p.fd = -1;
                continue;
            }
            line_buf.extend_from_slice(&buf[..n as usize]);
            emit_complete_lines(line_buf, is_out);
        }
        if pollfds.iter().all(|p| p.fd < 0) {
            break;
        }
    }
    log::debug!("[diag] ohos_qemu::drain_fds: both pipes closed");
}

/// Emits every newline-terminated line from `buf`, dropping the consumed
/// prefix and keeping any partial tail for the next read.
#[cfg(feature = "qemu_debug_assertions")]
fn emit_complete_lines(buf: &mut Vec<u8>, is_out: bool) {
    let mut consumed = 0;
    for (i, byte) in buf.iter().enumerate() {
        // Serial consoles commonly end lines with \r, \n or \r\n; split on
        // either so a complete sentence is emitted as one piece.
        if *byte == b'\n' || *byte == b'\r' {
            emit_line(&buf[consumed..i], is_out);
            consumed = i + 1;
        }
    }
    if consumed > 0 {
        buf.drain(..consumed);
    }
}

/// Emits the whole remaining buffer as one final line (no trailing newline),
/// used when the pipe write end closes.
#[cfg(feature = "qemu_debug_assertions")]
fn emit_remaining_lines(buf: &mut Vec<u8>, is_out: bool) {
    if !buf.is_empty() {
        emit_line(buf, is_out);
        buf.clear();
    }
}

/// Logs one complete line with the `[qemu-console]` / `[qemu-stderr]` prefix.
#[cfg(feature = "qemu_debug_assertions")]
fn emit_line(bytes: &[u8], is_out: bool) {
    let line = String::from_utf8_lossy(bytes);
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if is_out {
        // Full guest console output is kept on purpose: kernel boot, 9p mount
        // and network bring-up diagnostics are easier to trace this way. If it
        // floods hilog, filter to [cmd-agentd] lines only.
        log::info!("[diag] [qemu-console] {line}");
    } else {
        log::error!("[diag] [qemu-stderr] {line}");
    }
}

/// Returns a readable message for the last dlopen/dlsym error, or a fallback
/// when the error string is unavailable.
fn last_dl_error(fallback: &str) -> String {
    // SAFETY: dlerror returns a NUL-terminated string, or NULL when there is
    // no pending error; read it only when non-NULL.
    let ptr = unsafe { libc::dlerror() };
    if ptr.is_null() {
        fallback.to_string()
    } else {
        // SAFETY: ptr is non-NULL and points to a NUL-terminated string.
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }
}

/// Builds the QEMU argv vector. `-append` is a single C-string that embeds
/// spaces, so each QEMU option pair is one vector element.
fn build_argv(paths: &QemuPaths) -> Vec<CString> {
    let sandbox = CString::new(paths.sandbox_mount.to_string_lossy().as_bytes()).expect("path");
    let tools = CString::new(paths.tools_mount.to_string_lossy().as_bytes()).expect("path");
    let kernel = CString::new(paths.kernel.to_string_lossy().as_bytes()).expect("path");
    let initrd = CString::new(paths.initrd.to_string_lossy().as_bytes()).expect("path");
    let port_dir = paths.port_dir.to_string_lossy().into_owned();
    let cmdline = CString::new(KERNEL_CMDLINE).expect("static cmdline");

    let mut argv = vec![
        CString::new("qemu-system-aarch64").expect("static"),
        CString::new("-nodefaults").expect("static"),
        CString::new("-no-user-config").expect("static"),
        CString::new("-M").expect("static"),
        CString::new(MACHINE_TYPE).expect("static"),
        CString::new("-cpu").expect("static"),
        CString::new(CPU_MODEL).expect("static"),
        CString::new("-smp").expect("static"),
        CString::new(CPU_SMP).expect("static"),
        // MTTCG: run the vCPU threads in parallel so a multi-core guest is
        // actually simulated concurrently. -icount is incompatible with MTTCG
        // and is dropped; idle vCPUs sleep on the guest's WFI (the kernel
        // cmdline uses idle=halt). Startup (init + initrd unpack) is
        // single-threaded, so keeping the vCPU count modest (4) bounds the
        // MTTCG synchronization overhead while still parallelizing runtime
        // multi-threaded work (LSP, compilation).
        CString::new("-accel").expect("static"),
        CString::new("tcg,thread=multi").expect("static"),
        CString::new("-m").expect("static"),
        CString::new(MEM_SIZE).expect("static"),
        CString::new("-kernel").expect("static"),
        kernel,
        CString::new("-initrd").expect("static"),
        initrd,
        CString::new("-append").expect("static"),
        cmdline,
        CString::new("-display").expect("static"),
        CString::new("none").expect("static"),
        CString::new("-monitor").expect("static"),
        CString::new("none").expect("static"),
        // QMP control channel: runtime fsdev-add / device_add for work-directory
        // mounts. server=on mirrors the virtio-serial sockets; the mount path
        // opens one short-lived session per open-folder operation.
        CString::new("-qmp").expect("static"),
        CString::new(format!("unix:{port_dir}/{QMP_SOCKET},server=on,wait=off")).expect("format"),
        CString::new("-serial").expect("static"),
        // Guest console: with qemu_debug_assertions it goes to QEMU stdout,
        // which run_machine forwards into hilog; without it use "null" so a
        // chatty guest never reads the process stdin or writes the process
        // stdout, which could otherwise block the whole app.
        #[cfg(feature = "qemu_debug_assertions")]
        CString::new("stdio").expect("static"),
        #[cfg(not(feature = "qemu_debug_assertions"))]
        CString::new("null").expect("static"),
        // Fixed read-only tools mount: the HAP resfile (el1/bundle) holding
        // prebundled tools (clangd, python3, ssh) and C/C++ headers. Read-only,
        // so security_model=none is safe and no mapped-file metadata is written;
        // the guest mounts it first at /tools and PATH points there.
        CString::new("-fsdev").expect("static"),
        CString::new(format!(
            "local,security_model=none,id=fsdev_tools,path={tools_path},readonly=on",
            tools_path = tools.to_string_lossy()
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new(format!(
            "virtio-9p-pci,id=fs_tools,fsdev=fsdev_tools,mount_tag={MOUNT_TAG_TOOLS}"
        ))
        .expect("format"),
        // Fixed sandbox mount: the app sandbox, where zcoder keeps downloaded
        // programs and their configs. Open-folder directories are mounted
        // dynamically by the mount manager crate (QMP device_add), not here.
        CString::new("-fsdev").expect("static"),
        CString::new(format!(
            "local,security_model=mapped-file,id=fsdev0,path={sandbox_path}",
            sandbox_path = sandbox.to_string_lossy()
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new(format!(
            "virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag={MOUNT_TAG_SANDBOX}"
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new("virtio-serial-pci,id=virtio-serial0").expect("static"),
        // Management port: ExecResult / Signal / mount / ack channel.
        CString::new("-chardev").expect("static"),
        CString::new(format!(
            "socket,path={port_dir}/mgmt.sock,server=on,wait=off,id=mgmt0"
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new(format!(
            "virtserialport,chardev=mgmt0,bus=virtio-serial0.0,name={MGMT_PORT_NAME}"
        ))
        .expect("format"),
        // Guest user-mode network via slirp NAT: the guest reaches the outside
        // world through this process (npm / LSP downloads) without its own IP
        // setup. Default slirp subnet 10.0.2.0/24, gateway 10.0.2.2, DNS
        // 10.0.2.3; the guest brings eth0 up on boot.
        CString::new("-netdev").expect("static"),
        CString::new("user,id=net0").expect("static"),
        CString::new("-device").expect("static"),
        // romfile= disables the PXE boot ROM: the statically linked engine has
        // no efi-virtio.rom, and a headless guest never PXE-boots.
        CString::new("virtio-net-pci,id=net0dev,netdev=net0,romfile=").expect("static"),
    ];
    // PCIe root ports for runtime hotplug: pcie.0 itself does not support
    // device_add, so root ports (whose secondary buses own a hotplug handler)
    // are created up front. Each root port exposes one hotplug slot; runtime
    // virtio-9p devices attach to rp<N> via QMP device_add bus=rp<N> (picked
    // by mount sequence). The guest pciehp driver discovers them and the mount
    // worker mounts the tag.
    for rp_index in 0..WORKDIR_MOUNT_SLOTS {
        argv.push(CString::new("-device").expect("static"));
        argv.push(
            CString::new(format!(
                "pcie-root-port,id=rp{rp_index},chassis={},bus=pcie.0",
                rp_index + 1
            ))
            .expect("format"),
        );
    }
    // Per-command data (stdin/stdout) and stderr ports.
    for index in 0..PORT_POOL_SIZE {
        argv.push(CString::new("-chardev").expect("static"));
        argv.push(CString::new(format!(
            "socket,path={port_dir}/cmd.{index}.sock,server=on,wait=off,id=cmd{index}"
        ))
        .expect("format"));
        argv.push(CString::new("-device").expect("static"));
        argv.push(CString::new(format!(
            "virtserialport,chardev=cmd{index},bus=virtio-serial0.0,name={DATA_PORT_NAME_PREFIX}{index}"
        ))
        .expect("format"));
        argv.push(CString::new("-chardev").expect("static"));
        argv.push(CString::new(format!(
            "socket,path={port_dir}/err.{index}.sock,server=on,wait=off,id=err{index}"
        ))
        .expect("format"));
        argv.push(CString::new("-device").expect("static"));
        argv.push(CString::new(format!(
            "virtserialport,chardev=err{index},bus=virtio-serial0.0,name={ERR_PORT_NAME_PREFIX}{index}"
        ))
        .expect("format"));
    }
    log::debug!("[diag] ohos_qemu::build_argv: sandbox={} port_dir={} ports={}",
        sandbox.to_string_lossy(),
        port_dir,
        PORT_POOL_SIZE
    );
    // [diag] Dump the full QEMU argv so machine/network/mount/port config is
    // verifiable at a glance.
    let argv_text: Vec<String> = argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    log::info!("[diag] build_argv: {}", argv_text.join(" "));
    argv
}

/// Runs the QEMU engine until the guest shuts down; owns the engine handle and
/// the argv strings for the whole machine lifetime.
fn run_machine(handle: EngineHandle, entry: QemuSystemEntry, argv: Vec<CString>) {
    // Debug builds forward QEMU stderr (engine errors) and stdout (guest serial
    // console via `-serial stdio`) into hilog for diagnosis; release builds
    // build argv with `-serial null`, so the guest console never touches the
    // process stdin/stdout and cannot stall the app.
    #[cfg(feature = "qemu_debug_assertions")]
    redirect_fds_to_log();
    let mut ptrs: Vec<*const c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    log::info!("[diag] ohos_qemu::run_machine: entering qemu main argc={}", argv.len());
    // SAFETY: argv is a NUL-terminated array of NUL-terminated strings that
    // stays alive for this call; the engine runs until the guest exits.
    let ret = unsafe { (entry)(argv.len() as c_int, ptrs.as_ptr()) };
    if ret != 0 {
        log::error!("[diag] ohos_qemu::run_machine: qemu main exited with ret={ret}");
    } else {
        log::info!("[diag] ohos_qemu::run_machine: qemu main exited ret={ret}");
    }
    // Keep the library mapped until the machine has fully shut down.
    // SAFETY: the handle was opened in load_engine and is no longer needed.
    unsafe { libc::dlclose(handle.0) };
    // This function runs on the QEMU thread. Return instead of `_exit(ret)`:
    // `_exit` on a thread would terminate the whole app process. The thread
    // closure clears QEMU_RUNNING after this returns.
}
