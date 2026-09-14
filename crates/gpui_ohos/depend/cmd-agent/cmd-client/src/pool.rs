//! SSH connection pool to the daemon's command listener.
//!
//! A single multi-threaded tokio runtime is shared by the pool thread and every
//! command's pump task. A pool thread keeps at least MIN_IDLE ready connections
//! (each an authenticated russh Handle); `allocate()` pops one with a bounded
//! wait. When the daemon restarts, its dynamic keys change and the bootstrap
//! swaps the config and clears the ready pool so in-flight commands fail
//! explicitly and re-establish against the new keys.

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
/// How long a command waits when the pool has never connected to the daemon (first
/// boot, the daemon not started yet). Bounded and short so a missing daemon never
/// blocks the host application's startup: allocate pokes the bootstrap to connect right now and
/// only waits this long before failing the command fast.
const FIRST_CONNECT_BUDGET: Duration = Duration::from_millis(1000);
/// How long a command waits when the pool was configured before (the daemon was
/// reachable) but is momentarily empty (e.g. the daemon restarted). Fails fast
/// rather than stalling the caller.
const RECONNECT_BUDGET: Duration = Duration::from_secs(3);
/// After a failed connect, subsequent commands fail immediately for this long
/// (no point re-waiting for a daemon that is down); the background bootstrap
/// keeps trying every BOOTSTRAP_INTERVAL and re-configures the pool the moment
/// the daemon is back.
const DOWN_COOLDOWN: Duration = Duration::from_secs(5);
/// Timeout for establishing one SSH connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval of the pool thread while topping up.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Retry delay inside allocate() when the pool is momentarily empty.
const ALLOCATE_RETRY: Duration = Duration::from_millis(20);

/// Rejects any host key that does not match the expected one. The expected key
/// is the daemon's command host key delivered in this run's `SshInfo`, so a
/// restarted daemon (new key) is rejected and triggers a re-bootstrap.
#[derive(Clone)]
pub struct VerifyHandler {
    pub(crate) expected: PublicKey,
}

impl Handler for VerifyHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_key: &PublicKey) -> Result<bool, Self::Error> {
        let accepted = server_key == &self.expected;
        if !accepted {
            log::warn!("pool: host key mismatch (hicodeerd restarted?), rejecting");
        }
        Ok(accepted)
    }
}

/// One authenticated SSH session.
pub type SshSession = Handle<VerifyHandler>;

/// Parses an OpenSSH public-key text into a `PublicKey`, keeping only the two
/// key tokens (`<algorithm> <base64>`) so a trailing comment or extra whitespace
/// (as `ssh-keygen` appends) never leaks into the parsed value.
pub(crate) fn host_public_key(text: &str) -> Result<PublicKey, String> {
    let canonical: String = text
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    PublicKey::from_openssh(&canonical).map_err(|err| format!("parse host public key: {err}"))
}

/// Connection parameters for the current daemon command SSH server.
#[derive(Clone)]
pub struct ConnConfig {
    pub host: String,
    pub port: u16,
    /// OpenSSH text of the expected command host public key.
    pub host_public_pem: String,
    /// OpenSSH text of the command client private key.
    pub private_key_pem: String,
    /// Identity presented as the SSH user name, naming this client instance to
    /// the daemon (see `protocol::CLIENT_ID_PREFIX`).
    pub client_id: String,
}

/// Shared SSH connection pool.
pub struct Pool {
    runtime: Arc<tokio::runtime::Runtime>,
    ready: Mutex<VecDeque<SshSession>>,
    config: Mutex<Option<ConnConfig>>,
    /// Wakes the bootstrap loop immediately so an incoming command can trigger a
    /// reconnect instead of waiting for the next 10s tick.
    pub(crate) poke: tokio::sync::Notify,
    /// When the last connect failure happened; gates the fail-fast cooldown.
    down_since: Mutex<Option<Instant>>,
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
            poke: tokio::sync::Notify::new(),
            down_since: Mutex::new(None),
        });
        let worker = pool.clone();
        std::thread::Builder::new()
            .name("cmd-client-pool".to_string())
            .spawn(move || pool_loop(worker))
            .map_err(std::io::Error::other)?;
        Ok(pool)
    }

    /// Exposes the shared runtime so executor pump tasks run on it.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Swaps the connection config and clears the ready pool (the daemon restarted
    /// with new dynamic keys).
    pub fn update_config(&self, config: ConnConfig) {
        log::info!(
            "cmd-client pool: updating config to {}:{} and clearing ready pool",
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

    /// Current connection config, if the bootstrap has configured the pool yet.
    pub fn config(&self) -> Option<ConnConfig> {
        self.config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Pops one ready connection. Never blocks the host application for long: if the daemon has
    /// not been reached yet (config is None) or a recent connect failed, fail
    /// the command fast instead of stalling the caller. The bootstrap loop
    /// keeps reconnecting in the background (every BOOTSTRAP_INTERVAL, or
    /// immediately when poked here), so a command issued right after the daemon
    /// comes up triggers an on-demand reconnect.
    pub fn allocate(&self) -> std::io::Result<SshSession> {
        let configured = self.config().is_some();
        let now = Instant::now();
        let cooling_down = !configured
            && self
                .down_since
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .map(|t| now.duration_since(t) < DOWN_COOLDOWN)
                .unwrap_or(false);
        let budget = if cooling_down {
            Duration::ZERO
        } else if configured {
            RECONNECT_BUDGET
        } else {
            FIRST_CONNECT_BUDGET
        };
        let deadline = now + budget;
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
                if !self.config().is_some() {
                    *self
                        .down_since
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(Instant::now());
                }
                let err = if configured {
                    "hicodeerd connection unavailable"
                } else {
                    "hicodeerd not connected yet (start hicodeerd on the device)"
                };
                log::warn!("cmd-client pool: allocate failed fast: {err}");
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, err));
            }
            // Ask the bootstrap to reconnect right now (on-demand), then poll.
            self.poke.notify_one();
            std::thread::sleep(ALLOCATE_RETRY);
        }
    }
}

/// The pool thread: keeps the ready pool topped up to MAX_IDLE.
fn pool_loop(pool: Arc<Pool>) {
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
                for _ in 0..missing {
                    match pool.runtime.block_on(connect(&config)) {
                        Ok(session) => pool
                            .ready
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push_back(session),
                        Err(err) => {
                            log::warn!("cmd-client pool: connect failed: {err}");
                            break;
                        }
                    }
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Keepalive interval for pooled connections: the server is configured
/// with no inactivity timeout, so this only guards against intermediate drops.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Establishes and authenticates one SSH connection with the command client key,
/// verifying the command host key against the expected public key.
async fn connect(config: &ConnConfig) -> Result<SshSession, String> {
    let expected_host = host_public_key(&config.host_public_pem)?;
    let mut client_cfg = Config::default();
    client_cfg.keepalive_interval = Some(KEEPALIVE_INTERVAL);
    let client_config = Arc::new(client_cfg);
    let mut session = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(
            client_config,
            (config.host.as_str(), config.port),
            VerifyHandler {
                expected: expected_host,
            },
        ),
    )
    .await
    .map_err(|_| format!("connect to {}:{} timed out", config.host, config.port))?
    .map_err(|err| format!("connect to {}:{}: {err}", config.host, config.port))?;

    let key = PrivateKey::from_openssh(&config.private_key_pem)
        .map_err(|err| format!("parse client private key: {err}"))?;
    let auth = session
        .authenticate_publickey(
            config.client_id.as_str(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .map_err(|err| format!("publickey auth: {err}"))?;
    if !auth.success() {
        return Err("publickey auth rejected".to_string());
    }
    Ok(session)
}
