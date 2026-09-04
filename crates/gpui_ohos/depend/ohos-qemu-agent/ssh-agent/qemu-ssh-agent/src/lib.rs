//! In-process QEMU runner with an SSH connection-pool command executor.
//!
//! Loads `libqemu-system-aarch64.so` with dlopen and boots the guest on a
//! dedicated thread (mirroring qemu-cmd-agent), but replaces the virtio-serial
//! command channel with an SSH connection pool: the guest embeds a russh
//! server, and the host's `SshCommandExecutor` runs util::command over SSH.
//! File sharing stays on virtio-fs (see the virtio-fs design doc).

pub mod bootstrap;
pub mod command;
pub mod executor;
pub mod pool;
pub mod qmp;
pub mod virtiofs;

pub use executor::SshCommandExecutor;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Name of the QEMU engine library shipped in the HAP native-libs directory.
const QEMU_LIB_NAME: &str = "libqemu-system-aarch64.so";
/// Symbol exported by the QEMU engine that takes over the current thread and
/// runs the machine until it exits.
const QEMU_ENTRY_SYMBOL: &[u8] = b"main\0";

/// Machine type and CPU model matching the proven qemu-cmd-agent guest.
const MACHINE_TYPE: &str = "virt";
const CPU_MODEL: &str = "cortex-a76";
const CPU_SMP: &str = "4";
const MEM_SIZE: &str = "8G";
const KERNEL_CMDLINE: &str =
    "console=ttyAMA0,115200 rdinit=/sbin/init idle=halt ip=dhcp";

/// virtio-fs tags for the static sandbox/tools mounts (same as qemu-cmd-agent).
pub(crate) const MOUNT_TAG_SANDBOX: &str = "sandbox";
pub(crate) const MOUNT_TAG_TOOLS: &str = "tools";
/// virtio-fs backend unix socket names under the port dir.
pub(crate) const FS_SOCKET_TOOLS: &str = "fs_tools.sock";
pub(crate) const FS_SOCKET_SANDBOX: &str = "fs_sandbox.sock";
/// Prefix for virtio-fs backend sockets of dynamically mounted work dirs.
pub(crate) const FS_WORK_PREFIX: &str = "fs_work";
/// The single management virtio-serial port name (SSH bootstrap).
const MGMT_PORT_NAME: &str = "zcoder.ssh.mgmt";
/// QMP control socket for runtime device_add / hostfwd.
pub(crate) const QMP_SOCKET: &str = "qmp.sock";
/// Number of pre-created pcie-root-ports for runtime hotplug of work-directory
/// virtio-fs devices (each exposes one slot).
pub(crate) const WORKDIR_MOUNT_SLOTS: usize = 8;

/// Entry point signature of the loaded QEMU engine library.
type QemuSystemEntry = unsafe extern "C" fn(argc: c_int, argv: *const *const c_char) -> c_int;

/// Dlopen handle owned by the QEMU thread.
struct EngineHandle(*mut c_void);

// SAFETY: the handle is created in `start` and moved straight into the QEMU
// thread, which is the only place that dereferences it.
unsafe impl Send for EngineHandle {}

/// Whether the QEMU engine thread is currently running.
static QEMU_RUNNING: AtomicBool = AtomicBool::new(false);

fn qemu_running() -> bool {
    QEMU_RUNNING.load(Ordering::SeqCst)
}

/// Filesystem layout handed to the guest.
#[derive(Clone)]
pub struct QemuPaths {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// Directory holding the management serial socket, the QMP socket and the
    /// virtio-fs backend sockets.
    pub port_dir: PathBuf,
    /// Host directory shared to the guest as the "sandbox" mount.
    pub sandbox_mount: PathBuf,
    /// Read-only HAP resfile directory shared to the guest as "tools".
    pub tools_mount: PathBuf,
}

/// Starts the QEMU guest on a dedicated thread and leaves it running.
pub fn start(paths: QemuPaths) -> bool {
    if qemu_running() {
        log::info!("qemu_ssh_agent::start: already running");
        return true;
    }
    // Start the virtio-fs backends (sandbox/tools) before QEMU so their
    // listening sockets exist when the vhost-user-fs-pci chardev connects.
    virtiofs::start(&paths);

    // Load the engine, build the argv and run the machine on its own thread.
    let (engine, entry) = load_engine();
    let argv = build_argv(&paths);
    log::info!(
        "qemu_ssh_agent::start: launching guest with {} argv elements",
        argv.len()
    );
    QEMU_RUNNING.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("qemu-machine".to_string())
        .spawn(move || {
            run_machine(engine, entry, argv);
            QEMU_RUNNING.store(false, Ordering::SeqCst);
        })
        .map(|_| true)
        .unwrap_or_else(|err| {
            log::error!("qemu_ssh_agent::start: spawn machine thread: {err}");
            QEMU_RUNNING.store(false, Ordering::SeqCst);
            false
        })
}

/// Loads the QEMU engine library and resolves its entry point.
fn load_engine() -> (EngineHandle, QemuSystemEntry) {
    let lib_name = CString::new(QEMU_LIB_NAME).expect("static");
    // SAFETY: dlopen with a NUL-terminated path kept alive for the call.
    let handle = unsafe { libc::dlopen(lib_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        log::error!(
            "qemu_ssh_agent: dlopen {} failed: {}",
            QEMU_LIB_NAME,
            last_dl_error("unknown dlopen error")
        );
        panic!("qemu engine library failed to load");
    }
    // SAFETY: dlsym with a NUL-terminated symbol name; the returned pointer is
    // cast to the engine entry signature.
    let entry = unsafe { libc::dlsym(handle, QEMU_ENTRY_SYMBOL.as_ptr() as *const c_char) };
    if entry.is_null() {
        log::error!(
            "qemu_ssh_agent: dlsym main failed: {}",
            last_dl_error("unknown dlsym error")
        );
        panic!("qemu engine entry symbol missing");
    }
    // SAFETY: the entry pointer was exported by the engine as the `main` C
    // signature (argc/argv).
    let entry: QemuSystemEntry = unsafe { std::mem::transmute(entry) };
    (EngineHandle(handle), entry)
}

/// Returns a readable message for the last dlopen/dlsym error.
fn last_dl_error(fallback: &str) -> String {
    // SAFETY: dlerror returns a NUL-terminated string, or NULL when there is no
    // pending error.
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
    let kernel = CString::new(paths.kernel.to_string_lossy().as_bytes()).expect("path");
    let initrd = CString::new(paths.initrd.to_string_lossy().as_bytes()).expect("path");
    let port_dir = paths.port_dir.to_string_lossy().into_owned();
    let cmdline = CString::new(KERNEL_CMDLINE).expect("static cmdline");

    let mut argv = vec![
        CString::new("qemu-system-aarch64").expect("static"),
        CString::new("-nodefaults").expect("static"),
        CString::new("-no-user-config").expect("static"),
        CString::new("-M").expect("static"),
        CString::new(format!("{MACHINE_TYPE},memory-backend=mem")).expect("format"),
        CString::new("-cpu").expect("static"),
        CString::new(CPU_MODEL).expect("static"),
        CString::new("-smp").expect("static"),
        CString::new(CPU_SMP).expect("static"),
        CString::new("-accel").expect("static"),
        CString::new("tcg,thread=multi").expect("static"),
        CString::new("-object").expect("static"),
        CString::new(format!("memory-backend-memfd,id=mem,size={MEM_SIZE}")).expect("format"),
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
        // QMP control channel: runtime device_add for work-dir mounts and
        // hostfwd_add for the SSH bootstrap.
        CString::new("-qmp").expect("static"),
        CString::new(format!("unix:{port_dir}/{QMP_SOCKET},server=on,wait=off")).expect("format"),
        CString::new("-serial").expect("static"),
        // Debug builds forward the guest serial console to QEMU stdout (into
        // hilog); release builds use "null" so the guest never touches the
        // process stdin/stdout.
        #[cfg(feature = "qemu_debug_assertions")]
        CString::new("stdio").expect("static"),
        #[cfg(not(feature = "qemu_debug_assertions"))]
        CString::new("null").expect("static"),
        // Read-only tools mount over virtio-fs.
        CString::new("-chardev").expect("static"),
        CString::new(format!(
            "socket,path={port_dir}/{FS_SOCKET_TOOLS},id=fs_tools"
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new(format!(
            "vhost-user-fs-pci,id=fs_tools,chardev=fs_tools,tag={MOUNT_TAG_TOOLS},queue-size=1024"
        ))
        .expect("format"),
        // Writable sandbox mount over virtio-fs.
        CString::new("-chardev").expect("static"),
        CString::new(format!(
            "socket,path={port_dir}/{FS_SOCKET_SANDBOX},id=fs_sandbox"
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new(format!(
            "vhost-user-fs-pci,id=fs_sandbox,chardev=fs_sandbox,tag={MOUNT_TAG_SANDBOX},queue-size=1024"
        ))
        .expect("format"),
        // The single management virtio-serial port for SSH bootstrap.
        CString::new("-device").expect("static"),
        CString::new("virtio-serial-pci,id=virtio-serial0").expect("static"),
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
        // Guest user-mode network via slirp NAT (npm / LSP downloads). The SSH
        // hostfwd is added dynamically by the bootstrap once SshInfo arrives.
        CString::new("-netdev").expect("static"),
        CString::new("user,id=net0").expect("static"),
        CString::new("-device").expect("static"),
        CString::new("virtio-net-pci,id=net0dev,netdev=net0,romfile=").expect("static"),
    ];
    // PCIe root ports for runtime hotplug of work-directory virtio-fs devices.
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
    let argv_text: Vec<String> = argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    log::info!("qemu_ssh_agent: build_argv: {}", argv_text.join(" "));
    argv
}

/// Runs the QEMU engine until the guest shuts down; owns the engine handle and
/// the argv strings for the whole machine lifetime.
fn run_machine(handle: EngineHandle, entry: QemuSystemEntry, argv: Vec<CString>) {
    #[cfg(feature = "qemu_debug_assertions")]
    redirect_fds_to_log();
    let mut ptrs: Vec<*const c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    // SAFETY: argv is a NUL-terminated array of NUL-terminated strings that
    // stays alive for this call; the engine runs until the guest exits.
    let ret = unsafe { (entry)(argv.len() as c_int, ptrs.as_ptr()) };
    log::info!("qemu_ssh_agent: qemu main exited ret={ret}");
    // SAFETY: the handle was opened in load_engine and is no longer needed.
    unsafe { libc::dlclose(handle.0) };
}

/// Forwards process stdout/stderr (guest serial console in debug builds) into
/// hilog so engine errors and guest boot logs are visible on the device.
#[cfg(feature = "qemu_debug_assertions")]
fn redirect_fds_to_log() {
    let mut out_fds = [0 as c_int; 2];
    let mut err_fds = [0 as c_int; 2];
    // SAFETY: pipe/dup2/close are plain syscalls; each pair is owned by this
    // thread (write end) and the drain thread (read end).
    if unsafe { libc::pipe(out_fds.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(err_fds.as_mut_ptr()) } != 0
    {
        log::error!(
            "qemu_ssh_agent::redirect_fds_to_log: pipe failed: {}",
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
    log::info!("qemu_ssh_agent::redirect_fds_to_log: stdout/stderr captured via pipes");
}

/// Drains the stdout and stderr pipes with one poll() loop and re-emits whole
/// lines into hilog: guest console at info level, engine errors at error level.
/// Bytes are buffered per pipe until a newline, so a line split across reads is
/// still printed as one complete sentence instead of one fragment per read.
#[cfg(feature = "qemu_debug_assertions")]
fn drain_fds(out_fd: c_int, err_fd: c_int) {
    let mut pollfds = [
        libc::pollfd {
            fd: out_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: err_fd,
            events: libc::POLLIN,
            revents: 0,
        },
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
            let line_buf = if is_out {
                &mut out_lines
            } else {
                &mut err_lines
            };
            // SAFETY: read on the pipe fd; 0 or -1 means the write end closed.
            let n = unsafe { libc::read(p.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n <= 0 {
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
    log::debug!("qemu_ssh_agent::drain_fds: both pipes closed");
}

/// Emits every newline-terminated line from `buf`, dropping the consumed prefix
/// and keeping any partial tail for the next read.
#[cfg(feature = "qemu_debug_assertions")]
fn emit_complete_lines(buf: &mut Vec<u8>, is_out: bool) {
    let mut consumed = 0;
    for (i, byte) in buf.iter().enumerate() {
        if *byte == b'\n' || *byte == b'\r' {
            emit_line(&buf[consumed..i], is_out);
            consumed = i + 1;
        }
    }
    if consumed > 0 {
        buf.drain(..consumed);
    }
}

/// Emits the whole remaining buffer as one final line (no trailing newline).
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
        log::info!("[diag] [qemu-console] {line}");
    } else {
        log::error!("[diag] [qemu-stderr] {line}");
    }
}
