//! On-device command server for the host application.
//!
//! Runs as an independent executable (a public HNP shipped in the host HAP,
//! launched on the OHOS device via hdc / the system). It listens on two
//! loopback SSH ports:
//! - 4022 (command): dynamic keys freshly generated on every start, serves
//!   command exec over a russh server, and
//! - 4023 (management): fixed build-time keys, serving `BOOTSTRAP_COMMAND` so a
//!   cmd-client can fetch this run's dynamic command keys.
//!
//! Replaces the previous external-VM command backends (openeuler-agent /
//! qemu-agent): the daemon runs on the same device as the host application and can spawn
//! arbitrary OHOS command-line programs, removing the VM dependency.

mod exec;
mod keygen;
mod logger;
mod management;
mod peers;
mod shim;
mod protocol;
mod pty;
mod session_tmp;
mod sign_elf;
mod sshd;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::ssh_key::{PrivateKey, PublicKey};
use russh::server as russh_server;
use tokio::net::TcpListener;

use crate::management::ManagementServer;
use crate::protocol::{COMMAND_PORT, LOOPBACK_ADDR, MANAGEMENT_PORT};
use crate::sshd::{ConnectionHandler, SshServer};

/// Env var overriding the fixed management-key directory (for local bring-up
/// and for hdc-launched runs where the keys live outside the HNP conf dir).
const CONF_DIR_ENV: &str = "HICODEERD_CONF_DIR";
/// Env var overriding the address both SSH listeners bind to. The daemon build
/// that runs inside the QEMU guest must bind `0.0.0.0` so the host-side slirp
/// hostfwd rules can reach it; the OHOS build keeps the loopback default.
const BIND_ADDR_ENV: &str = "HICODEERD_BIND_ADDR";
/// Env var (guest mode): when set to "1", periodically reclaims guest dcache.
/// The embedded virtiofsd backend holds one O_PATH fd per guest-looked-up inode
/// until the guest sends FUSE_FORGET; a guest with ample RAM rarely evicts its
/// dcache, so fds would grow unbounded and exhaust the process. Writing
/// `drop_caches=2` forces dentry/inode eviction -> FUSE_FORGET -> fd release.
const DROP_CACHES_ENV: &str = "HICODEERD_DROP_CACHES";
/// Sysfs knob to write and the value (2 = drop unused dentries/inodes only).
const DROP_CACHES_PATH: &str = "/proc/sys/vm/drop_caches";
const DROP_CACHES_VALUE: &str = "2";
/// Reclaim cadence (see the 2026-09-03 virtiofsd fd-exhaustion record).
const DROP_CACHES_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// Bytes of traffic before an SSH key re-exchange is requested. The protocol
/// forbids raising this past the ceiling russh itself enforces (see
/// `russh::Limits::new`), and neither listener carries anywhere near it.
const REKEY_BYTE_LIMIT: usize = 1 << 30;
/// Time before an SSH key re-exchange is requested. An app frozen by the system
/// (`nap-background`) or a suspended device stops answering altogether, so
/// anything short of this would rekey into silence and drop the connection --
/// which the management listener reads as the client being gone, and answers by
/// taking down everything that client started (see `peers`). A year is
/// effectively "never" for a connection that only ever carries a few kilobytes.
const REKEY_TIME_LIMIT: std::time::Duration =
    std::time::Duration::from_secs(365 * 24 * 60 * 60);
/// Name of the fixed management host private key file (ssh-keygen output).
const MGMT_HOST_KEY_FILE: &str = "mgmt_host_key";
/// Name of the file holding the authorized management client public key.
const MGMT_AUTHORIZED_KEYS_FILE: &str = "authorized_keys";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help") {
        print_help();
        return;
    }
    // Silent by default; pass --log to enable stdout + hilog (OHOS) logging.
    let logging = args.iter().any(|arg| arg == "--log");
    logger::init(logging);
    if let Err(err) = run() {
        // Startup failure must surface even in silent mode.
        eprintln!("hicodeerd fatal: {err}");
        std::process::exit(1);
    }
}

/// Prints what this binary accepts. Runs before any listener is bound and
/// before a logger is installed, so `--help` reads the same whatever else is
/// on the command line.
fn print_help() {
    println!("hicodeerd - on-device command server for the host application");
    println!();
    println!("Usage: hicodeerd [--log] [--help]");
    println!();
    println!("Listens on the loopback ports {COMMAND_PORT} (command) and {MANAGEMENT_PORT} (management). The");
    println!("management listener hands a fresh client the keys this run's command");
    println!("listener accepts.");
    println!();
    println!("Options:");
    println!("  --log   Enable logging. Without it the daemon is silent: no logger is");
    println!("          installed and every record is dropped. With it, records go to");
    println!("          hilog on the device and to stdout on a plain host. Only");
    println!("          failures are recorded either way.");
    println!("  --help  Print this help and exit.");
}

/// Root of the package this executable was installed into: the directory that
/// carries `bin/` and the rest of the payload, one level above the executable.
/// Reading `/proc/self/exe` follows any exec symlink to the real installed
/// layout, which is what `conf` and the preloads are addressed relative to.
///
/// On OHOS that is the version directory of this daemon's HNP
/// (`<pkg>.org/<pkg>_<version>/`), which the system installs and replaces as a
/// whole; inside the guest it is the `qemu` directory the host stages.
pub(crate) fn package_root() -> std::io::Result<PathBuf> {
    // `/proc/self/exe` is the reliable source: it follows the exec symlink to
    // the real installed layout. Some hosts do not mount procfs, so fall back to
    // asking the runtime rather than assuming either one works.
    let exe = match std::fs::read_link("/proc/self/exe") {
        Ok(exe) => exe,
        Err(err) => {
            log::warn!("daemon: read /proc/self/exe failed ({err}); asking the runtime instead");
            std::env::current_exe()?
        }
    };
    let bin_dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no parent dir"))?;
    bin_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| std::io::Error::other("bin dir has no parent dir"))
}

/// Resolves the directory holding the fixed management keys: `CONF_DIR_ENV`
/// when set, otherwise `<package root>/conf` (a `conf` directory next to the
/// `bin` that contains this executable).
fn conf_dir() -> std::io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONF_DIR_ENV) {
        let path = PathBuf::from(dir);
        return Ok(path);
    }
    // The HNP layout is <pkg>/bin/<daemon binary> + <pkg>/conf/... .
    Ok(package_root()?.join("conf"))
}

/// Loads the fixed management host private key and the authorized management
/// client public key from the conf dir.
fn read_mgmt_keys(conf: &Path) -> Result<(PrivateKey, PublicKey), String> {
    let host_pem = std::fs::read_to_string(conf.join(MGMT_HOST_KEY_FILE))
        .map_err(|err| format!("read {}: {err}", MGMT_HOST_KEY_FILE))?;
    let host_key = PrivateKey::from_openssh(&host_pem)
        .map_err(|err| format!("parse {}: {err}", MGMT_HOST_KEY_FILE))?;
    let authorized_text = std::fs::read_to_string(conf.join(MGMT_AUTHORIZED_KEYS_FILE))
        .map_err(|err| format!("read {}: {err}", MGMT_AUTHORIZED_KEYS_FILE))?;
    let pub_line = authorized_text
        .lines()
        .find(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .ok_or_else(|| format!("{} is empty", MGMT_AUTHORIZED_KEYS_FILE))?;
    // Keep only the two key tokens so a trailing comment or extra whitespace
    // (as `ssh-keygen` appends) never leaks into the parsed value.
    let canonical: String = pub_line
        .trim()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let authorized = PublicKey::from_openssh(&canonical)
        .map_err(|err| format!("parse {}: {err}", MGMT_AUTHORIZED_KEYS_FILE))?;
    Ok((host_key, authorized))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let conf = conf_dir()?;
    // Exports the preloads every program spawned from here on has to inherit.
    // Resolved from the package root, so it needs nothing from a client. A
    // package that ships no preloads leaves the environment untouched.
    shim::install();
    let (mgmt_host_key, mgmt_authorized) = read_mgmt_keys(&conf)?;

    // Dynamic keys for this run's command listener (never persisted).
    let dynamic = keygen::generate()?;
    let ssh_info = protocol::SshInfo {
        command_port: COMMAND_PORT,
        command_host_key_pem: keygen::public_openssh(dynamic.host_key.public_key())?,
        client_private_key_pem: keygen::private_openssh(&dynamic.client_private)?,
    };
    // Never log any key material (host key, client key, PEMs) in any mode.

    let bind_host =
        std::env::var(BIND_ADDR_ENV).unwrap_or_else(|_| LOOPBACK_ADDR.to_string());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let command_addr = format!("{bind_host}:{COMMAND_PORT}");
        let mgmt_addr = format!("{bind_host}:{MANAGEMENT_PORT}");
        let command_listener = TcpListener::bind(&command_addr).await?;
        let mgmt_listener = TcpListener::bind(&mgmt_addr).await?;

        // Command listener config: this run's dynamic keys.
        let command_authorized = vec![dynamic.client_public];
        let command_server = SshServer::new(command_authorized);
        let command_config = Arc::new(russh_server::Config {
            keys: vec![dynamic.host_key],
            // cmd-client keeps a pool of long-lived SSH connections; never let
            // the server reap an idle pooled connection.
            inactivity_timeout: None,
            limits: russh::Limits::new(REKEY_BYTE_LIMIT, REKEY_BYTE_LIMIT, REKEY_TIME_LIMIT),
            ..Default::default()
        });

        // Management listener config: the fixed build-time keys.
        let mgmt_server = ManagementServer::new(vec![mgmt_authorized], &ssh_info);
        let mgmt_config = Arc::new(russh_server::Config {
            keys: vec![mgmt_host_key],
            inactivity_timeout: None,
            // This connection's presence is what says the client is alive
            // (see `peers`), so it must never be asked to rekey while its
            // process may be frozen and unable to answer.
            limits: russh::Limits::new(REKEY_BYTE_LIMIT, REKEY_BYTE_LIMIT, REKEY_TIME_LIMIT),
            ..Default::default()
        });

        maybe_spawn_drop_caches();
        let mut shutdown = ShutdownSignals::new()?;
        tokio::select! {
            result = async {
                tokio::try_join!(
                    accept_command(command_listener, command_config, command_server),
                    accept_management(mgmt_listener, mgmt_config, mgmt_server),
                )
            } => {
                result?;
            }
            name = shutdown.recv() => {
                log::warn!("daemon: {name} received; shutting down");
            }
        }
        // Both ways out converge here: nothing a client started may outlive the
        // daemon that owns it.
        peers::retire_all("daemon exiting");
        Ok(())
    })
}

/// In guest mode (`DROP_CACHES_ENV=1`) periodically reclaims the guest
/// dcache so the in-process virtiofsd backend releases its O_PATH fds
/// (see `DROP_CACHES_*` constants).
fn maybe_spawn_drop_caches() {
    if std::env::var(DROP_CACHES_ENV).as_deref() != Ok("1") {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DROP_CACHES_INTERVAL);
        interval.tick().await; // immediate first tick
        loop {
            interval.tick().await;
            if let Err(err) = std::fs::write(DROP_CACHES_PATH, DROP_CACHES_VALUE) {
                log::warn!("hicodeerd: drop_caches write failed: {err}");
            }
        }
    });
}

/// Accepts command-listener connections and serves each with a fresh handler.
async fn accept_command(
    listener: TcpListener,
    config: Arc<russh_server::Config>,
    server: SshServer,
) -> std::io::Result<()> {
    loop {
        let (stream, _peer) = listener.accept().await?;
        let handler: ConnectionHandler = server.new_connection();
        let config = config.clone();
        tokio::spawn(async move {
            let _ = russh_server::run_stream(config, stream, handler).await;
        });
    }
}

/// Accepts management-listener connections and serves each with a fresh handler.
async fn accept_management(
    listener: TcpListener,
    config: Arc<russh_server::Config>,
    server: ManagementServer,
) -> std::io::Result<()> {
    loop {
        let (stream, _peer) = listener.accept().await?;
        let handler = server.new_connection();
        let config = config.clone();
        tokio::spawn(async move {
            let _ = russh_server::run_stream(config, stream, handler).await;
        });
    }
}

/// The signals that ask the daemon to stop. Registered once, before the accept
/// loops start, so one arriving during start-up is not missed.
struct ShutdownSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Resolves to the name of the first of them to arrive. SIGHUP is included
    /// because the daemon is normally started from an interactive shell, which
    /// sends it when that terminal goes away.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.hangup.recv() => "SIGHUP",
        }
    }
}

