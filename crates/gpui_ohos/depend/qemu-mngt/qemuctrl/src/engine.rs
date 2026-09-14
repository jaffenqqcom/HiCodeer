//! QEMU engine loading and the dedicated machine thread.
//!
//! Loads `libqemu-system-aarch64.so` with dlopen, resolves its `main` symbol
//! and runs the machine on its own thread (`qemu-machine`) until the guest
//! shuts down. No SSH / command-execution code lives here: guest commands run
//! through cmd-client against a daemon inside the guest.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{QEMU_ENTRY_SYMBOL, QEMU_LIB_NAME};

/// Entry point signature of the loaded QEMU engine library.
type QemuSystemEntry = unsafe extern "C" fn(argc: c_int, argv: *const *const c_char) -> c_int;

/// Dlopen handle owned by the QEMU thread.
struct EngineHandle(*mut c_void);

// SAFETY: the handle is created in `spawn` and moved straight into the QEMU
// thread, which is the only place that dereferences it.
unsafe impl Send for EngineHandle {}

/// Whether the QEMU engine thread is currently running.
static QEMU_RUNNING: AtomicBool = AtomicBool::new(false);

fn qemu_running() -> bool {
    QEMU_RUNNING.load(Ordering::SeqCst)
}

/// Whether the QEMU engine thread is currently running.
pub fn is_running() -> bool {
    qemu_running()
}

/// Hook invoked on the engine thread right after the machine exits (guest
/// poweroff or crash). Returning `Some(argv_sequence)` boots again on this
/// same thread — the engine thread is its own supervisor, so no polling is
/// needed anywhere. Returning `None` ends the thread.
pub type RestartHook = Box<dyn FnMut() -> Option<Vec<Vec<CString>>> + Send>;

/// Spawns the QEMU machine thread, running each argv vector in turn on the
/// same thread (normally one stage; see `qemu_manager::build_argv`). After the
/// last stage exits, `restart` decides whether to boot again (guest relaunch)
/// or end the thread. Idempotent: returns `true` when already running. The
/// running flag is cleared only after the final machine exits.
pub fn spawn_with_restart(argv_sequence: Vec<Vec<CString>>, mut restart: RestartHook) -> bool {
    if qemu_running() {
        return true;
    }
    if argv_sequence.is_empty() {
        log::warn!("qemu engine::spawn_with_restart: empty argv sequence");
        return false;
    }
    QEMU_RUNNING.store(true, Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("qemu-machine".to_string())
        .spawn(move || {
            let mut stages = argv_sequence;
            loop {
                for argv in stages {
                    match load_engine() {
                        Ok((engine, entry)) => run_machine(engine, entry, argv),
                        Err(err) => log::error!("qemu engine: load engine failed: {err}"),
                    }
                }
                match restart() {
                    Some(next) => {
                        log::info!("qemu engine: relaunching machine per restart hook");
                        stages = next;
                    }
                    None => break,
                }
            }
            QEMU_RUNNING.store(false, Ordering::SeqCst);
            log::warn!("qemu engine: machine thread exited (all stages done)");
        })
        .map(|_| true)
        .unwrap_or_else(|err| {
            log::error!("qemu engine::spawn_with_restart: spawn machine thread: {err}");
            QEMU_RUNNING.store(false, Ordering::SeqCst);
            false
        });
    spawned
}

/// Spawns the QEMU machine thread for a single argv vector and no relaunch
/// (thread ends when the guest powers off). See [`spawn_with_restart`].
pub fn spawn(argv: Vec<CString>) -> bool {
    spawn_with_restart(vec![argv], Box::new(|| None))
}

/// Loads the QEMU engine library and resolves its entry point.
fn load_engine() -> Result<(EngineHandle, QemuSystemEntry), String> {
    let lib_name = CString::new(QEMU_LIB_NAME).map_err(|_| "static lib name".to_string())?;
    // SAFETY: dlopen with a NUL-terminated path kept alive for the call.
    let handle =
        unsafe { libc::dlopen(lib_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(last_dl_error("unknown dlopen error"));
    }
    // SAFETY: dlsym with a NUL-terminated symbol name; the returned pointer is
    // cast to the engine entry signature.
    let entry =
        unsafe { libc::dlsym(handle, QEMU_ENTRY_SYMBOL.as_ptr() as *const c_char) };
    if entry.is_null() {
        let err = last_dl_error("unknown dlsym error");
        // Deliberately leak the handle: even on this failure path the library's
        // ELF constructors have run and may have registered pthread TLS
        // destructors pointing into the lib; dlclose would unmap them and a
        // later thread exit would fault in __pthread_tsd_run_dtors.
        return Err(err);
    }
    // SAFETY: the entry pointer was exported by the engine as the `main` C
    // signature (argc/argv).
    let entry: QemuSystemEntry = unsafe { std::mem::transmute(entry) };
    Ok((EngineHandle(handle), entry))
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
    log::info!("qemu engine: qemu main exited ret={ret}");
    // Deliberately leak the engine handle (never dlclose): QEMU/glib register
    // pthread TLS destructors pointing into the library, and this thread's
    // pending pthread_exit runs them after we return. Unmapping the library
    // here made those destructors jump into unmapped memory and SIGSEGV the
    // whole process. The engine is a process singleton; keeping it mapped also
    // makes a supervisor-driven relaunch cheaper.
    std::mem::forget(handle);
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
            "qemu engine::redirect_fds_to_log: pipe failed: {}",
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
