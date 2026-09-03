//! Guest-side SSH agent for the zcoder QEMU guest.
//!
//! Generates an ed25519 host key and a one-time client keypair, embeds a
//! russh SSH server on a random guest port, and serves the bootstrap `SshInfo`
//! (port + client private key) to the host over the management virtio-serial
//! port. The zcoder host connects through slirp hostfwd and executes commands
//! over the SSH connection pool.

mod exec;
mod keygen;
mod serial;
mod server;

use std::sync::Arc;

use russh::server as russh_server;
use tokio::net::TcpListener;

use crate::server::SshServer;

/// Writable guest dir holding per-command pid files (used by host-side signal
/// via `kill -KILL -$(cat <pid_dir>/<session>.pid)`).
const PID_DIR: &str = "/sandbox/ssh";

/// How often the guest drops its dentry/inode caches.
///
/// The guest pins every virtio-fs inode it has looked up in its dcache, and
/// only releases them (sending FUSE_FORGET, which frees the matching O_PATH fd
/// in the host's virtiofsd) when the kernel reclaims those dentries. With ample
/// guest RAM a traversal of a huge tree (e.g. `git status` over an
/// un-ignored cargo `build/`) leaves those dentries resident forever, so
/// virtiofsd fds grow monotonically until the zcoder process hits its
/// RLIMIT_NOFILE and crashes. Periodically forcing a dentry/inode reclaim
/// bounds that growth.
const DROP_CACHES_INTERVAL_SECS: u64 = 15;
/// Value 2 reclaims dentries and inodes only (not the page cache), which is
/// exactly the slab that pins virtio-fs inodes; see `Documentation/admin-guide/
/// sysctl/vm.rst` in the kernel tree.
const DROP_CACHES_VALUE: &str = "2";
/// Sysctl written by the periodic reclaim task.
const DROP_CACHES_PATH: &str = "/proc/sys/vm/drop_caches";

/// Simple logger writing to stderr, which the debug QEMU build forwards to
/// hilog via the guest serial console.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        eprintln!("ssh-agentd {}: {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

fn main() {
    let _ = log::set_logger(&StderrLogger);
    log::set_max_level(log::LevelFilter::Info);
    if let Err(err) = run() {
        log::error!("ssh-agentd fatal: {err}");
        std::process::exit(1);
    }
}

/// Rewrites /etc/profile so login shells (which zcoder's capturing-env uses
/// via `/bin/sh -l -c env`) export the correct HOME and PATH from the root:
/// HOME on the persistent sandbox mount (initramfs /root is wiped on QEMU
/// restart), PATH covering /tools and the sandbox languages tree.
///
/// HOME is only exported to /sandbox/home when /sandbox is actually mounted
/// (the HAP sandbox, el2/base): otherwise the directory would be a throwaway
/// tmpfs path on the initramfs and pointless as a persistent HOME.
fn ensure_profile() -> std::io::Result<()> {
    const PROFILE_PATH: &str = "/etc/profile";
    let content = "\
export PATH=/tools/bin:/sandbox/haps/entry/files/zcoder/languages:/usr/local/bin:/usr/bin:/bin
export LD_LIBRARY_PATH=/tools/lib64
export PYTHONHOME=/tools
export CPATH=/tools/include
export HOME=/sandbox/home
export SHELL=/bin/sh
";
    std::fs::write(PROFILE_PATH, content)?;
    log::info!("ssh-agentd: rewrote {PROFILE_PATH} with correct HOME/PATH");
    Ok(())
}

/// True when the HAP sandbox (el2/base) is mounted at /sandbox, i.e. the
/// S40sandbox virtio-fs mount succeeded. A HOME created there is persistent
/// across QEMU restarts; on the initramfs it would be wiped.
fn sandbox_mounted() -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|mounts| mounts.lines().any(|line| line.contains(" /sandbox ")))
        .unwrap_or(false)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    ensure_profile().map_err(Box::<dyn std::error::Error>::from)?;
    let keys = keygen::generate().map_err(Box::<dyn std::error::Error>::from)?;
    let client_pem = keygen::client_private_pem(&keys.client_private)
        .map_err(Box::<dyn std::error::Error>::from)?;
    std::fs::create_dir_all(PID_DIR)?;
    if sandbox_mounted() {
        std::fs::create_dir_all("/sandbox/home")?;
        log::info!("ssh-agentd: created /sandbox/home on the mounted sandbox");
    } else {
        log::warn!("ssh-agentd: /sandbox not mounted; HOME stays /root");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
        let port = listener.local_addr()?.port();
        log::info!("ssh-agentd: russh server listening on 0.0.0.0:{port}");

        // Periodically reclaim guest dentry/inode caches so virtio-fs inodes
        // get FORGET-ted and the host virtiofsd frees their O_PATH fds (see
        // DROP_CACHES_INTERVAL_SECS). Runs detached from the accept loop.
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(DROP_CACHES_INTERVAL_SECS);
            loop {
                tokio::time::sleep(interval).await;
                match std::fs::write(DROP_CACHES_PATH, DROP_CACHES_VALUE) {
                    Ok(()) => {
                        log::debug!("ssh-agentd: reclaimed guest dentry/inode caches");
                    }
                    Err(err) => {
                        log::warn!("ssh-agentd: drop_caches write failed: {err}");
                    }
                }
            }
        });

        // Management serial thread serves SshInfo (blocking I/O) on its own
        // thread so it never stalls the async server task.
        let info = serial::SshInfo {
            port,
            client_private_key_pem: client_pem,
            pid_dir: PID_DIR.to_string(),
            sshd_pid: std::process::id(),
        };
        std::thread::Builder::new()
            .name("ssh-agentd-serial".to_string())
            .spawn(move || serial::serve(info))?;

        let authorized = vec![keys.client_public];
        let server = SshServer::new(authorized);
        let config = Arc::new(russh_server::Config {
            keys: vec![keys.host_key],
            // The zcoder host keeps a pool of long-lived SSH connections; never
            // let the server reap an idle pooled connection (the 600s default
            // would drop them and the host's next command would fail).
            inactivity_timeout: None,
            ..Default::default()
        });

        loop {
            let (stream, peer) = listener.accept().await?;
            log::info!("ssh-agentd: accepted connection from {peer}");
            let handler = server.new_connection();
            let config = config.clone();
            tokio::spawn(async move {
                let _ = russh_server::run_stream(config, stream, handler).await;
            });
        }
    })
}
