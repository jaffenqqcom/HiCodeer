//! SSH connection pool for the zcoder host.
//!
//! A single multi-threaded tokio runtime is shared by the pool thread and every
//! command's pump task. A pool thread keeps at least MIN_IDLE ready connections
//! (each an authenticated russh Handle); `allocate()` pops one with a bounded
//! wait. When the guest restarts its SSH server the config is swapped, the
//! ready pool is cleared, and in-flight commands fail explicitly.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client::{self, Config, Handle, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

/// Minimum idle connections the pool thread maintains.
const MIN_IDLE: usize = 5;
/// Target maximum idle connections the pool thread fills to.
const MAX_IDLE: usize = 16;
/// How long allocate() waits for a connection before failing. Generous: the
/// guest SSH server may not be up when zcoder's startup restore runs (git/LSP
/// commands fire immediately), so allocate must wait for the bootstrap to
/// configure the pool rather than failing a startup command outright.
const ALLOCATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout for establishing one SSH connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval of the pool thread while topping up.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Retry delay inside allocate() when the pool is momentarily empty.
const ALLOCATE_RETRY: Duration = Duration::from_millis(20);

/// Accepts any host key. Safe here because the connection is bound to host
/// 127.0.0.1 only and the client key is delivered once over the management
/// port; mirrors the OpenEuler cmd-agent's AcceptAllHandler.
#[derive(Clone)]
pub struct AcceptAllHandler;

impl Handler for AcceptAllHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _server_key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// One authenticated SSH session.
pub type SshSession = Handle<AcceptAllHandler>;

/// Connection parameters for the current guest SSH server.
#[derive(Clone)]
pub struct ConnConfig {
    pub host: String,
    pub port: u16,
    pub private_key_pem: String,
}

/// Shared SSH connection pool.
pub struct Pool {
    runtime: Arc<tokio::runtime::Runtime>,
    ready: Mutex<VecDeque<SshSession>>,
    config: Mutex<Option<ConnConfig>>,
}

impl Pool {
    /// Starts the pool thread with a fresh multi-threaded tokio runtime.
    pub fn new() -> std::io::Result<Arc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(std::io::Error::other)?;
        let pool = Arc::new(Self {
            runtime: Arc::new(runtime),
            ready: Mutex::new(VecDeque::new()),
            config: Mutex::new(None),
        });
        let worker = pool.clone();
        std::thread::Builder::new()
            .name("ssh-pool".to_string())
            .spawn(move || pool_loop(worker))
            .map_err(std::io::Error::other)?;
        log::info!("ssh pool: started");
        Ok(pool)
    }

    /// Exposes the shared runtime so executor pump tasks run on it.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Swaps the connection config and clears the ready pool (guest SSH server
    /// restarted with a new port/key).
    pub fn update_config(&self, config: ConnConfig) {
        log::info!(
            "ssh pool: updating config to {}:{} and clearing ready pool",
            config.host,
            config.port
        );
        self.ready
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        *self
            .config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(config);
    }

    /// Pops one ready connection, waiting up to ALLOCATE_TIMEOUT when empty.
    pub fn allocate(&self) -> std::io::Result<SshSession> {
        let deadline = Instant::now() + ALLOCATE_TIMEOUT;
        loop {
            if let Some(session) = self
                .ready
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .pop_front()
            {
                return Ok(session);
            }
            if Instant::now() >= deadline {
                log::warn!("ssh pool: allocate timed out after {ALLOCATE_TIMEOUT:?}");
                return Err(std::io::Error::other("ssh pool empty"));
            }
            std::thread::sleep(ALLOCATE_RETRY);
        }
    }
}

/// The pool thread: keeps the ready pool topped up to MAX_IDLE.
fn pool_loop(pool: Arc<Pool>) {
    log::info!("ssh pool: thread running");
    loop {
        let config = pool
            .config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(config) = config {
            let idle = pool
                .ready
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .len();
            if idle < MIN_IDLE {
                let missing = MAX_IDLE - idle;
                log::debug!("ssh pool: idle={idle} topping up {missing}");
                for _ in 0..missing {
                    match pool.runtime.block_on(connect(&config)) {
                        Ok(session) => pool
                            .ready
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push_back(session),
                        Err(err) => {
                            log::warn!("ssh pool: connect failed: {err}");
                            break;
                        }
                    }
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Keepalive interval for pooled connections: the guest server is configured
/// with no inactivity timeout, so this only guards against intermediate drops.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Establishes and authenticates one SSH connection with the client key.
async fn connect(config: &ConnConfig) -> Result<SshSession, String> {
    let mut client_cfg = Config::default();
    client_cfg.keepalive_interval = Some(KEEPALIVE_INTERVAL);
    let client_config = Arc::new(client_cfg);
    let mut session = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(
            client_config,
            (config.host.as_str(), config.port),
            AcceptAllHandler,
        ),
    )
    .await
    .map_err(|_| format!("connect to {}:{} timed out", config.host, config.port))?
    .map_err(|err| format!("connect to {}:{}: {err}", config.host, config.port))?;

    let key = PrivateKey::from_openssh(&config.private_key_pem)
        .map_err(|err| format!("parse client private key: {err}"))?;
    let auth = session
        .authenticate_publickey("root", PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|err| format!("publickey auth: {err}"))?;
    if !auth.success() {
        return Err("publickey auth rejected".to_string());
    }
    log::info!("ssh pool: connected to {}:{}", config.host, config.port);
    Ok(session)
}
