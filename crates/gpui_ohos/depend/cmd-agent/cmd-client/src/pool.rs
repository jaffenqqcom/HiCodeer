//! SSH connection pool to the daemon's command listener.
//!
//! A single multi-threaded tokio runtime is shared by the pool thread and every
//! command's pump task. The pool thread keeps at least MIN_IDLE ready
//! connections (each an authenticated russh Handle); `allocate()` pops one with
//! a bounded wait. When the daemon restarts, its dynamic keys change and the
//! bootstrap swaps the config and clears the ready pool so in-flight commands
//! fail explicitly and re-establish against the new keys.
//!
//! The pool thread is started by the bootstrap the first time a config arrives,
//! never before: until the management channel has answered once there is nothing
//! the pool could connect to, so a daemon that was never reached costs neither a
//! thread nor a connection attempt. It then stays up across daemon restarts, and
//! is disabled -- config and ready connections dropped -- whenever the
//! management channel is lost, since with the channel gone its keys are stale.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client::{self, Config, Handle, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

/// Minimum idle connections the pool thread maintains.
const MIN_IDLE: usize = 5;
/// Target maximum idle connections the pool thread fills to.
const MAX_IDLE: usize = 16;
/// How long a command waits when the pool was configured before (the daemon was
/// reachable) but is momentarily empty (e.g. the daemon restarted). Fails fast
/// rather than stalling the caller. A pool that was never configured is not
/// waited on at all -- the pool thread is not even running then.
const RECONNECT_BUDGET: Duration = Duration::from_secs(3);
/// Timeout for establishing one SSH connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval of the pool thread while topping up.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Retry delay inside allocate() when the pool is momentarily empty.
const ALLOCATE_RETRY: Duration = Duration::from_millis(20);
/// Interval the pool thread waits after a failed connect. The bootstrap drops
/// the config as soon as it notices the management channel is gone, which stops
/// these attempts altogether; until it does, this keeps a daemon that has gone
/// down from turning into a storm of connection attempts.
const CONNECT_BACKOFF: Duration = Duration::from_secs(1);

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
    /// Held behind an `Arc` because readers are hot: the pool thread asks for it
    /// once per poll and `allocate` once per wait step, and it carries two PEM
    /// keys, so handing out owned copies would be a lot of needless allocation.
    config: Mutex<Option<Arc<ConnConfig>>>,
    /// Whether the pool thread has been started, so starting it is idempotent.
    pool_started: AtomicBool,
}

impl Pool {
    /// Builds the pool and its tokio runtime. Starts neither a thread nor a
    /// connection: the pool thread is created when the first config arrives (see
    /// `update_config`), and until then there is nothing to connect to.
    pub fn new() -> std::io::Result<Arc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Arc::new(Self {
            runtime: Arc::new(runtime),
            ready: Mutex::new(VecDeque::new()),
            config: Mutex::new(None),
            pool_started: AtomicBool::new(false),
        }))
    }

    /// Exposes the shared runtime so executor pump tasks run on it.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Swaps the connection config and clears the ready pool (the daemon restarted
    /// with new dynamic keys), then makes sure the pool thread is running.
    pub fn update_config(self: &Arc<Self>, config: ConnConfig) {
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
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::new(config));
        self.start_pool_loop();
    }

    /// Disables the pool because the management channel is gone: the command
    /// listener's keys could only be stale, so keeping them would leave the pool
    /// reconnecting with credentials the daemon no longer accepts, and would keep
    /// newly issued commands waiting for a connection that cannot be made.
    /// Returns whether a config was actually dropped, so a caller retrying in a
    /// loop can report the transition once rather than once per attempt.
    pub fn clear_config(&self) -> bool {
        let had_config = self
            .config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .is_some();
        self.ready
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        had_config
    }

    /// Starts the pool thread, once. Called when a config first arrives, so a
    /// daemon that was never reached costs no thread and no connect attempt.
    fn start_pool_loop(self: &Arc<Self>) {
        if self.pool_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let worker = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("cmd-client-pool".to_string())
            .spawn(move || pool_loop(worker))
        {
            log::error!("cmd-client pool: start pool thread: {err}");
            self.pool_started.store(false, Ordering::Release);
        }
    }

    /// Current connection config, if the bootstrap has configured the pool yet.
    /// Cloning it is a refcount bump (see the `config` field).
    pub fn config(&self) -> Option<Arc<ConnConfig>> {
        self.config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Pops one ready connection. Never blocks the host application for long: it
    /// fails at once when there is no config -- the management channel was never
    /// established, so the pool thread is not running and there is nothing to
    /// wait for -- and after RECONNECT_BUDGET when the pool is configured but
    /// momentarily empty. Losing the management channel drops the config, which
    /// fails whoever is waiting here too.
    pub fn allocate(&self) -> std::io::Result<SshSession> {
        let deadline = Instant::now() + RECONNECT_BUDGET;
        loop {
            if let Some(session) = self
                .ready
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .pop_front()
            {
                // A connection can die while it sits in the pool: the daemon was
                // killed or restarted, or the loopback connection dropped. The
                // pool may not have noticed yet, and handing this one out would
                // only move the failure to whatever opens the first channel on
                // it. Drop it and keep looking instead -- the pool either has
                // another live connection or the wait below covers a refill.
                if session.is_closed() {
                    log::debug!("cmd-client pool: discarding a closed connection");
                    continue;
                }
                return Ok(session);
            }
            if self.config().is_none() {
                let err = "hicodeerd not connected yet (start hicodeerd on the command line terminal of system。请先在系统命令行终端运行hicodeerd程序)";
                log::warn!("cmd-client pool: allocate failed fast: no management channel");
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, err));
            }
            if Instant::now() >= deadline {
                let err = "hicodeerd connection unavailable";
                log::warn!("cmd-client pool: allocate failed fast: {err}");
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, err));
            }
            std::thread::sleep(ALLOCATE_RETRY);
        }
    }
}

/// The pool thread: keeps the ready pool topped up to MAX_IDLE.
///
/// Only ever started once a config exists (see `Pool::start_pool_loop`), and made
/// idle again by `clear_config` as soon as the management channel is lost, so it
/// connects only to a daemon the bootstrap has just heard from.
fn pool_loop(pool: Arc<Pool>) {
    let mut reported_failure = false;
    loop {
        // No config means there is nothing to connect to -- the management
        // channel is gone or has not answered yet. That is not a recovery, so
        // reported_failure is left alone; and it is not a failure either, so it
        // does not back off, which would delay a daemon that comes up right
        // after this point by the whole backoff.
        let Some(config) = pool.config() else {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        };
        match top_up(&pool, &config) {
            Ok(()) => {
                if reported_failure {
                    reported_failure = false;
                    log::info!("cmd-client pool: connections restored");
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                // Once per outage: a retry loop that logs every attempt is what
                // made this a log storm before.
                if !reported_failure {
                    reported_failure = true;
                    log::warn!("cmd-client pool: connect failed: {err}");
                }
                std::thread::sleep(CONNECT_BACKOFF);
            }
        }
    }
}

/// Fills the ready pool up to MAX_IDLE, reporting the first failed connect.
fn top_up(pool: &Arc<Pool>, config: &ConnConfig) -> Result<(), String> {
    let idle = pool
        .ready
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .len();
    if idle >= MIN_IDLE {
        return Ok(());
    }
    for _ in idle..MAX_IDLE {
        let session = pool.runtime.block_on(connect(config))?;
        pool.ready
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(session);
    }
    Ok(())
}

/// Keepalive interval for pooled connections: the server is configured
/// with no inactivity timeout, so this only guards against intermediate drops.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Bytes of traffic before an SSH key re-exchange is requested. The protocol
/// forbids raising this past the ceiling russh itself enforces (see
/// `russh::Limits::new`), and a pooled connection carries far less than it.
pub(crate) const REKEY_BYTE_LIMIT: usize = 1 << 30;
/// Time before an SSH key re-exchange is requested, for this pool and for the
/// management connection alike (see `bootstrap`). Effectively "never": the
/// daemon holds both kinds of connection open indefinitely, and a client frozen
/// by the system cannot answer a rekey, so a short interval would drop exactly
/// the connection whose presence says the instance is still alive.
pub(crate) const REKEY_TIME_LIMIT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Establishes and authenticates one SSH connection with the command client key,
/// verifying the command host key against the expected public key.
async fn connect(config: &ConnConfig) -> Result<SshSession, String> {
    let expected_host = host_public_key(&config.host_public_pem)?;
    let mut client_cfg = Config::default();
    client_cfg.keepalive_interval = Some(KEEPALIVE_INTERVAL);
    client_cfg.limits = russh::Limits::new(REKEY_BYTE_LIMIT, REKEY_BYTE_LIMIT, REKEY_TIME_LIMIT);
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
    // Bounded like the connect above: `client::connect` only covers the TCP
    // connection and key exchange, so a daemon that accepts the connection and
    // then stops answering would otherwise park this call for good.
    let auth = tokio::time::timeout(
        CONNECT_TIMEOUT,
        session.authenticate_publickey(
            config.client_id.as_str(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        ),
    )
    .await
    .map_err(|_| format!("publickey auth to {}:{} timed out", config.host, config.port))?
    .map_err(|err| format!("publickey auth: {err}"))?;
    if !auth.success() {
        return Err("publickey auth rejected".to_string());
    }
    Ok(session)
}
