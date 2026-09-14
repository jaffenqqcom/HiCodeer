//! Guest (QEMU) shell sessions hosted by a Zed terminal.
//!
//! The OHOS sandbox only execs `/bin/sh`, so when the QEMU command backend is
//! up a new terminal runs its shell inside the guest instead. Alacritty's pty
//! type can only be built by constructors that spawn a child, so the terminal
//! keeps a *local* pty pair: alacritty owns the master and drives it exactly as
//! before (keystrokes, resize, event loop), while two bridge threads relay the
//! slave to the backend's pty session (see `util::command::open_remote_shell`).
//! The local child is a `/bin/sh` parked on a FIFO so it never reads the slave
//! and keeps the pty alive for as long as the terminal does.

use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use alacritty_terminal::tty;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use util::command::{RemoteShell, ResizeHandle, open_remote_shell};

use crate::TerminalBounds;
use crate::alacritty::AlacrittyPty;

/// Terminal size for the local pty before the first window resize reaches the
/// bridge; the first layout pass corrects it and the bridge forwards it.
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
/// Window-size poll period of the input thread. The local pty size is only
/// observable via TIOCGWINSZ, so guest resizes are forwarded on this cadence.
const RESIZE_POLL_MS: libc::c_int = 300;
/// Bytes moved per relay iteration in either direction.
const RELAY_CHUNK: usize = 8192;
/// Shell that parks the local pty's child (the only program OHOS may exec).
const HOLD_SHELL: &str = "/bin/sh";
/// Directory under the app sandbox files directory holding the FIFOs that park
/// local pty children.
const HOLD_DIR: &str = ".pty-hold";
/// Environment variable carrying the app sandbox's `files/` directory, exported
/// by `ensure_shell_env` on the main thread. Read from here because
/// `openharmony_ability::global_app()` is a thread_local and this probe runs on
/// a background thread.
const SANDBOX_FILES_DIR_ENV: &str = "SANDBOX_FILES_DIR";
/// Lowest fd a dup may land on: standard streams must stay untouched.
const MIN_DUP_FD: libc::c_int = 3;
/// Thread names, so a stack dump shows the bridge by role.
const INPUT_THREAD: &str = "ohos-pty-in";
const OUTPUT_THREAD: &str = "ohos-pty-out";
/// Distinguishes the hold FIFOs of concurrent terminals in one process.
static HOLD_SEQ: AtomicU64 = AtomicU64::new(0);

/// A probed guest shell plus the local plumbing that hosts it.
pub(crate) struct GuestShell {
    remote: RemoteShell,
    hold: HoldFifo,
}

impl GuestShell {
    /// Shell program and arguments for the local pty child (see `HoldFifo`).
    pub(crate) fn local_shell_argv(&self) -> (String, Vec<String>) {
        (
            HOLD_SHELL.to_string(),
            vec!["-c".to_string(), self.hold.hold_command()],
        )
    }

    /// Starts the two relay threads; they own the local slave from here on and
    /// release the parked child when either side of the session ends.
    fn start_bridge(mut self, slave: OwnedFd) -> std::io::Result<()> {
        let resize = self.remote.resize_handle();
        let stdin = self.remote.stdin.take();
        let stdout = self.remote.stdout.take();
        let hold = self.hold;
        // Dropping the session keeps the backend relay task alive through the
        // resize handle; when the handle drops, the task shuts down too.
        drop(self.remote);
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            log::error!("ohos_shell::start_bridge: guest shell missing stdio pipes");
            return Err(std::io::Error::other("guest shell missing stdio"));
        };
        let output_slave = dup_fd(&slave)?;
        let output_done = Arc::new(AtomicBool::new(false));
        let input_done = output_done.clone();
        std::thread::Builder::new()
            .name(OUTPUT_THREAD.to_string())
            .spawn(move || relay_output(output_slave, stdout, output_done))
            .map_err(std::io::Error::other)?;
        std::thread::Builder::new()
            .name(INPUT_THREAD.to_string())
            .spawn(move || relay_input(&slave, stdin, resize, input_done, hold))
            .map_err(std::io::Error::other)?;
        log::info!("ohos_shell::start_bridge: guest shell bridge started");
        Ok(())
    }
}

/// Probes the registered command backend for an interactive shell. `None` (with
/// a log line) means the caller keeps the plain local `/bin/sh` terminal:
/// either no backend is registered or it cannot serve a pty.
pub(crate) async fn probe(cwd: Option<&str>) -> Option<GuestShell> {
    let remote = match open_remote_shell(INITIAL_COLS as u32, INITIAL_ROWS as u32, cwd).await {
        Ok(remote) => remote,
        Err(err) => {
            log::info!("ohos_shell::probe: no guest shell ({err}); using the local /bin/sh");
            return None;
        }
    };
    match HoldFifo::create() {
        Ok(hold) => {
            log::info!("ohos_shell::probe: guest shell ready cwd={cwd:?}");
            Some(GuestShell { remote, hold })
        }
        Err(err) => {
            log::error!("ohos_shell::probe: hold fifo: {err}; using the local /bin/sh");
            None
        }
    }
}

/// Opens the pty the terminal drives. With a guest shell this is a local pair
/// whose slave the bridge relays to the guest; without one the plain alacritty
/// pty (a real local `/bin/sh` child) is used.
pub(crate) fn open_pty(
    guest: Option<GuestShell>,
    options: &tty::Options,
    window_id: u64,
) -> std::io::Result<AlacrittyPty> {
    if let Some(guest) = guest {
        match open_guest_pty(guest, options, window_id) {
            Ok(pty) => return Ok(pty),
            Err(err) => {
                log::error!(
                    "ohos_shell::open_pty: guest pty failed ({err}); using the local /bin/sh"
                );
            }
        }
    }
    crate::alacritty::open_pty(options, TerminalBounds::default(), window_id)
}

/// Builds the local pty pair, hands it to alacritty, and starts the bridge.
fn open_guest_pty(
    guest: GuestShell,
    options: &tty::Options,
    window_id: u64,
) -> std::io::Result<AlacrittyPty> {
    let (master, slave, bridge_slave) = open_local_pty()?;
    let pty = tty::from_fd(options, window_id, master, slave)?;
    guest.start_bridge(bridge_slave)?;
    log::info!("ohos_shell::open_guest_pty: terminal hosted in the guest shell");
    Ok(pty)
}

/// Keeps the local pty's child process parked.
///
/// alacritty's event loop stops reading the moment the pty child exits, so the
/// child must outlive the terminal; and it must never read the pty slave,
/// because the bridge owns that stream. The child therefore runs `/bin/sh`
/// reading a FIFO we hold open and never write: it blocks forever and touches
/// nothing. Releasing the FIFO (on bridge shutdown) lets it exit, which is what
/// retires the terminal.
struct HoldFifo {
    path: PathBuf,
    /// Write end kept open; closing it would give the child EOF immediately.
    _writer: File,
}

impl HoldFifo {
    fn create() -> std::io::Result<Self> {
        let dir = hold_dir()?;
        let seq = HOLD_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("hold-{}-{seq}", std::process::id()));
        // A FIFO left over from a previous run makes mkfifo fail with EEXIST.
        if let Err(err) = std::fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("ohos_shell: remove stale fifo {path:?}: {err}");
        }
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|err| std::io::Error::other(format!("fifo path: {err}")))?;
        // SAFETY: mkfifo on a path inside the app sandbox.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            path,
            _writer: writer,
        })
    }

    /// Shell command that parks the child on this FIFO: stdin is the FIFO (the
    /// `read` blocks until data arrives, which never happens), and stdout /
    /// stderr are dropped so the child cannot write into the terminal.
    fn hold_command(&self) -> String {
        format!(
            "exec 0<{} 1>/dev/null 2>/dev/null; read x",
            sh_quote(&self.path.to_string_lossy())
        )
    }
}

impl Drop for HoldFifo {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            log::debug!("ohos_shell: remove fifo {:?}: {err}", self.path);
        }
    }
}

/// Directory for the hold FIFOs.
///
/// They must live on the on-device filesystem (`/data`, hmfs). The app sandbox
/// files directory is the one place guaranteed to be writable for the app uid
/// and to support FIFOs; HOME cannot be used, because once the user has picked a
/// home directory it points at a virtiofs share, and virtiofs rejects mkfifo
/// with EPERM.
fn hold_dir() -> std::io::Result<PathBuf> {
    let root = std::env::var_os(SANDBOX_FILES_DIR_ENV)
        .ok_or_else(|| std::io::Error::other(format!("{SANDBOX_FILES_DIR_ENV} is not set")))?;
    let dir = PathBuf::from(root).join(HOLD_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Single-quotes `value` for `/bin/sh`.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Allocates a local pty pair: `(master, slave, bridge_slave)`. alacritty takes
/// master + slave (`tty::from_fd` closes its slave copy in the child) and the
/// bridge keeps its own dup of the slave, because it outlives that call.
fn open_local_pty() -> std::io::Result<(OwnedFd, OwnedFd, OwnedFd)> {
    // SAFETY: plain libc pty allocation; the returned fd is owned here.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: grantpt/unlockpt on a freshly opened master.
    if unsafe { libc::grantpt(master) } != 0 || unsafe { libc::unlockpt(master) } != 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: close the master we opened.
        unsafe { libc::close(master) };
        return Err(err);
    }
    let name = unsafe { libc::ptsname(master) };
    if name.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(master) };
        return Err(err);
    }
    // SAFETY: ptsname returns a NUL-terminated static string.
    let slave_path = unsafe { std::ffi::CStr::from_ptr(name) }.to_owned();
    if let Err(err) = set_window_size(master, INITIAL_COLS, INITIAL_ROWS) {
        unsafe { libc::close(master) };
        return Err(err);
    }
    if let Err(err) = set_raw_mode(master) {
        unsafe { libc::close(master) };
        return Err(err);
    }
    // SAFETY: open the slave by path (required for a usable pty).
    let slave = unsafe { libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(master) };
        return Err(err);
    }
    // SAFETY: ownership of the raw fds transfers to the OwnedFd values.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let bridge_slave = dup_fd(&slave)?;
    Ok((master, slave, bridge_slave))
}

/// Sets the pty window size with TIOCSWINSZ.
fn set_window_size(fd: RawFd, cols: u16, rows: u16) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on a valid pty fd.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Puts the local pty into raw mode so the bridge is a transparent byte pipe:
/// canonical mode would hold input until a newline and echo it back to
/// alacritty, and OPOST would double the CRs the guest terminal already emits.
fn set_raw_mode(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: termios calls on a valid pty fd.
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: tcgetattr succeeded, so the struct is initialized.
    let mut termios = unsafe { termios.assume_init() };
    termios.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL
        | libc::IXON);
    termios.c_oflag &= !libc::OPOST;
    termios.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    termios.c_cflag &= !(libc::CSIZE | libc::PARENB);
    termios.c_cflag |= libc::CS8;
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Duplicates `fd` with CLOEXEC set.
fn dup_fd(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC on a valid owned fd.
    let duped = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, MIN_DUP_FD) };
    if duped < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: duped is a fresh fd owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

/// Relays local keystrokes to the guest shell and forwards window resizes.
fn relay_input(
    slave: &OwnedFd,
    mut stdin: Box<dyn AsyncWrite + Unpin + Send>,
    resize: ResizeHandle,
    output_done: Arc<AtomicBool>,
    hold: HoldFifo,
) {
    let fd = slave.as_raw_fd();
    let mut buf = [0u8; RELAY_CHUNK];
    let mut size = (INITIAL_COLS, INITIAL_ROWS);
    loop {
        if output_done.load(Ordering::Relaxed) {
            log::info!("ohos_shell::relay_input: guest shell closed");
            break;
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one valid fd with a timeout.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, RESIZE_POLL_MS) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("ohos_shell::relay_input: poll slave: {err}");
            break;
        }
        if ready == 0 {
            forward_window_size(fd, &resize, &mut size);
            continue;
        }
        if poll_fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        match read_fd(fd, &mut buf) {
            Ok(0) => {
                log::info!("ohos_shell::relay_input: local pty closed");
                break;
            }
            Ok(n) => {
                if let Err(err) = futures_lite::future::block_on(stdin.write_all(&buf[..n])) {
                    log::warn!("ohos_shell::relay_input: send input: {err}");
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                log::warn!("ohos_shell::relay_input: read slave: {err}");
                break;
            }
        }
    }
    // Releasing the FIFO lets the parked child exit, which is what retires the
    // terminal's alacritty event loop.
    drop(hold);
    log::info!("ohos_shell::relay_input: bridge stopped");
}

/// Relays guest shell output into the local pty slave until the guest side
/// closes, then flags the input thread so both threads retire together.
fn relay_output(
    slave: OwnedFd,
    mut stdout: Box<dyn AsyncRead + Unpin + Send>,
    output_done: Arc<AtomicBool>,
) {
    let fd = slave.as_raw_fd();
    let mut buf = [0u8; RELAY_CHUNK];
    loop {
        match futures_lite::future::block_on(stdout.read(&mut buf)) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(err) = write_all_fd(fd, &buf[..n]) {
                    log::warn!("ohos_shell::relay_output: write slave: {err}");
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                log::warn!("ohos_shell::relay_output: read guest output: {err}");
                break;
            }
        }
    }
    output_done.store(true, Ordering::Relaxed);
    log::info!("ohos_shell::relay_output: bridge stopped");
}

/// Forwards the local pty size to the guest whenever it changes.
fn forward_window_size(fd: RawFd, resize: &ResizeHandle, last: &mut (u16, u16)) {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ on a valid pty fd.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return;
    }
    let size = (ws.ws_col, ws.ws_row);
    if size == *last || size.0 == 0 || size.1 == 0 {
        return;
    }
    *last = size;
    log::debug!(
        "ohos_shell: forward resize {}x{} to the guest",
        size.0,
        size.1
    );
    resize.resize(size.0 as u32, size.1 as u32);
}

/// Reads into `buf`; errno is surfaced to the caller (no retry here).
fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: read into a buffer we own.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Writes the whole buffer to `fd`, handling short writes and EINTR.
fn write_all_fd(fd: RawFd, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        // SAFETY: write from a slice we own.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        data = &data[n as usize..];
    }
    Ok(())
}
