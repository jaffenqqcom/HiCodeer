//! zcoderd: on-device command server for zcoder.
//!
//! Runs as an independent executable (a public HNP shipped in the zcoder HAP,
//! launched on the OHOS device via hdc / the system). It listens on two
//! loopback SSH ports:
//! - 4022 (command): dynamic keys freshly generated on every start, serves
//!   command exec over a russh server, and
//! - 4023 (management): fixed build-time keys, serving `zcoderd-bootstrap` so a
//!   cmd-client can fetch this run's dynamic command keys.
//!
//! Replaces the previous external-VM command backends (openeuler-agent /
//! qemu-agent): zcoderd runs on the same device as zcoder and can spawn
//! arbitrary OHOS command-line programs, removing the VM dependency.

mod exec;
mod keygen;
mod logger;
mod management;
mod protocol;
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
const CONF_DIR_ENV: &str = "ZCODERD_CONF_DIR";
/// Name of the fixed management host private key file (ssh-keygen output).
const MGMT_HOST_KEY_FILE: &str = "mgmt_host_key";
/// Name of the file holding the authorized management client public key.
const MGMT_AUTHORIZED_KEYS_FILE: &str = "authorized_keys";

/// Shared `.zcoder` data root and LSP download dir under the user directory.
/// zcoderd (public HNP) creates them because it is the only process permitted
/// to create directories under /storage/Users/currentUser; the sandboxed zcoder
/// app is not. zcoderd also chmods them world-writable so the app can write.
#[cfg(target_env = "ohos")]
const USER_ZCODER_ROOT: &str = "/storage/Users/currentUser/.zcoder";
#[cfg(target_env = "ohos")]
const USER_ZCODER_LANGUAGES: &str = "/storage/Users/currentUser/.zcoder/languages";

fn main() {
    // Silent by default; pass --log to enable stdout + hilog (OHOS) logging.
    let logging = std::env::args().any(|arg| arg == "--log");
    logger::init(logging);
    if let Err(err) = run() {
        // Startup failure must surface even in silent mode.
        eprintln!("zcoderd fatal: {err}");
        std::process::exit(1);
    }
}

/// Resolves the directory holding the fixed management keys: `ZCODERD_CONF_DIR`
/// when set, otherwise `<hnp-package-root>/conf` (a `conf` directory next to
/// the `bin` that contains this executable). Resolving `/proc/self/exe` follows
/// any exec symlink to the real package layout.
fn conf_dir() -> std::io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONF_DIR_ENV) {
        let path = PathBuf::from(dir);
        log::info!("conf dir from {CONF_DIR_ENV}: {}", path.display());
        return Ok(path);
    }
    let exe = std::fs::read_link("/proc/self/exe")
        .unwrap_or_else(|_| std::env::current_exe().expect("current_exe"));
    let bin_dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no parent dir"))?;
    // The HNP layout is <pkg>/bin/zcoderd + <pkg>/conf/... .
    let pkg_root = bin_dir
        .parent()
        .ok_or_else(|| std::io::Error::other("bin dir has no parent dir"))?;
    Ok(pkg_root.join("conf"))
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
    log::info!("management keys loaded from {}", conf.display());
    Ok((host_key, authorized))
}

/// Creates the shared `.zcoder` data tree on the device. On non-OHOS builds
/// (local bring-up) this is a no-op. Runs entirely inside zcoderd (never called
/// by zcoder, so it can never block the app). Invoked at startup and lazily
/// before each command exec; a success flag makes the second and later calls a
/// near-zero check, and a failure leaves the flag clear so a later exec retries
/// once /storage becomes reachable.
pub(crate) fn ensure_user_zcoder_dirs() {
    #[cfg(target_env = "ohos")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.load(Ordering::Relaxed) {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let mut all_ok = true;
        for dir in [USER_ZCODER_ROOT, USER_ZCODER_LANGUAGES] {
            if let Err(err) = std::fs::create_dir_all(dir) {
                log::error!("zcoderd: create shared dir {dir} failed: {err}");
                all_ok = false;
                continue;
            }
            // Widen permissions (best-effort): the sandboxed app writes into
            // these dirs but only zcoderd can create/chmod them.
            let perms = std::fs::Permissions::from_mode(0o777);
            if let Err(err) = std::fs::set_permissions(dir, perms) {
                log::error!("zcoderd: chmod shared dir {dir} failed: {err}");
                all_ok = false;
            }
        }
        if all_ok {
            DONE.store(true, Ordering::Relaxed);
            log::warn!("zcoderd: shared .zcoder dirs ready");
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let conf = conf_dir()?;
    let (mgmt_host_key, mgmt_authorized) = read_mgmt_keys(&conf)?;

    // Dynamic keys for this run's command listener (never persisted).
    let dynamic = keygen::generate()?;
    let ssh_info = protocol::SshInfo {
        command_port: COMMAND_PORT,
        command_host_key_pem: keygen::public_openssh(dynamic.host_key.public_key())?,
        client_private_key_pem: keygen::private_openssh(&dynamic.client_private)?,
    };
    // Never log any key material (host key, client key, PEMs) in any mode.

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let command_addr = format!("{LOOPBACK_ADDR}:{COMMAND_PORT}");
        let mgmt_addr = format!("{LOOPBACK_ADDR}:{MANAGEMENT_PORT}");
        let command_listener = TcpListener::bind(&command_addr).await?;
        let mgmt_listener = TcpListener::bind(&mgmt_addr).await?;
        log::info!("zcoderd: command sshd listening on {command_addr}");
        log::info!("zcoderd: management sshd listening on {mgmt_addr}");

        // Command listener config: this run's dynamic keys.
        let command_authorized = vec![dynamic.client_public];
        let command_server = SshServer::new(command_authorized);
        let command_config = Arc::new(russh_server::Config {
            keys: vec![dynamic.host_key],
            // cmd-client keeps a pool of long-lived SSH connections; never let
            // the server reap an idle pooled connection.
            inactivity_timeout: None,
            ..Default::default()
        });

        // Management listener config: the fixed build-time keys.
        let mgmt_server = ManagementServer::new(vec![mgmt_authorized], &ssh_info);
        let mgmt_config = Arc::new(russh_server::Config {
            keys: vec![mgmt_host_key],
            inactivity_timeout: None,
            ..Default::default()
        });

        // zcoderd owns the shared data root: create it before any client can
        // request a download into it.
        ensure_user_zcoder_dirs();

        tokio::try_join!(
            accept_command(command_listener, command_config, command_server),
            accept_management(mgmt_listener, mgmt_config, mgmt_server),
        )?;
        Ok(())
    })
}

/// Accepts command-listener connections and serves each with a fresh handler.
async fn accept_command(
    listener: TcpListener,
    config: Arc<russh_server::Config>,
    server: SshServer,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        log::info!("command: accepted connection from {peer}");
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
        let (stream, peer) = listener.accept().await?;
        // Normal management handshakes happen every ~10s (bootstrap polling);
        // only problems are logged at warn in the handler, so keep this quiet.
        log::debug!("management: accepted connection from {peer}");
        let handler = server.new_connection();
        let config = config.clone();
        tokio::spawn(async move {
            let _ = russh_server::run_stream(config, stream, handler).await;
        });
    }
}

