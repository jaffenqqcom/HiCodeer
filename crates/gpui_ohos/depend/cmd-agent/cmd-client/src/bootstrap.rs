//! SSH bootstrap over the fixed-key management listener (127.0.0.1:4023).
//!
//! Connects to the daemon's management port with the fixed management keys,
//! authenticates, sends the reserved `BOOTSTRAP_COMMAND` command, and reads the
//! returned `SshInfo` (this run's dynamic command host key + client private key
//! + command port). When the daemon restarts its dynamic keys change, so this loop
//! re-fetches periodically and hands the pool a new config only when something
//! changed. Runs on a dedicated thread (never on a host-application/GPUI calling thread).
//!
//! The connection is held open for as long as this loop runs, and each round
//! sends the request over it instead of over a fresh connection. Its presence is
//! what tells the daemon this instance is still there: the user name it
//! authenticates with names this instance, so while the connection is up the
//! daemon leaves this instance's process trees alone, and when it ends -- which,
//! on a loopback connection nothing else ever closes, means this process is
//! gone -- the daemon takes them down (see the daemon's `peers`).
//!
//! Nothing here may close that connection while this process lives, and it
//! carries no keepalive: a keepalive left unanswered while the process is frozen
//! by the system would drop the very connection whose presence says the instance
//! is alive. Its rekey bounds are long for the same reason -- see
//! `pool::REKEY_TIME_LIMIT`.

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

use crate::endpoint::CommandEndpoint;
use crate::pool::{ConnConfig, Pool, SshSession, VerifyHandler};
use crate::protocol::{bootstrap_command, SshInfo};

/// Delay between bootstrap re-fetches before a config has ever been obtained:
/// short, so a daemon started after this process is picked up quickly and the
/// pool thread starts as soon as it can.
const BOOTSTRAP_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Delay between bootstrap re-fetches once a config exists. Each round is a real
/// request over the held connection -- a fresh channel running
/// `BOOTSTRAP_COMMAND` -- so this is not an SSH keepalive and does not protect
/// the connection. What it bounds is how long the pool can go on using command
/// keys the daemon has already replaced by restarting, which is why it sits well
/// below what a mere refresh would need.
const BOOTSTRAP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// Timeout for one management connection / bootstrap round trip.
const MGMT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on the SshInfo payload, far larger than any serialized keys JSON.
const MAX_SSH_INFO_BYTES: usize = 64 * 1024;

/// Runs the bootstrap loop forever on the calling thread.
pub fn start(
    pool: Arc<Pool>,
    endpoint: CommandEndpoint,
    mgmt_client_priv_pem: String,
    mgmt_host_pub_pem: String,
    client_id: String,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            log::error!("cmd-client bootstrap: build runtime: {err}");
            return;
        }
    };
    rt.block_on(async move {
        // Held open for the life of this loop (see the module header), and never
        // let go before a replacement is up: a failed round stands a new
        // connection up first and only then releases this one, so the daemon is
        // never left without a connection vouching for this instance.
        let mut session: Option<SshSession> = None;
        // Whether the current outage has already been reported. Without this the
        // retry loop would log a line per attempt, forever.
        let mut reported_failure = false;
        // Fetch immediately at startup, then wait one interval before fetching
        // again: short while the daemon has never answered, so one started after
        // this process is picked up quickly, and long once it has, which still
        // bounds how long the pool can go on using keys a restart has already
        // replaced. Never blocks a host-application thread.
        let mut fetch_now = true;
        loop {
            if !fetch_now {
                let interval = if pool.config().is_some() {
                    BOOTSTRAP_HEARTBEAT_INTERVAL
                } else {
                    BOOTSTRAP_PROBE_INTERVAL
                };
                tokio::time::sleep(interval).await;
            }
            fetch_now = false;
            match fetch_ssh_info(
                &mut session,
                &endpoint,
                &mgmt_client_priv_pem,
                &mgmt_host_pub_pem,
                &client_id,
            )
            .await
            {
                Ok(info) => {
                    reported_failure = false;
                    // The command port is fixed by the endpoint (host-side
                    // hostfwd rule in QEMU mode); only key changes matter.
                    let changed = match pool.config() {
                        Some(current) => {
                            current.host_public_pem != info.command_host_key_pem
                                || current.private_key_pem != info.client_private_key_pem
                        }
                        None => true,
                    };
                    if changed {
                        log::info!(
                            "cmd-client bootstrap: new SshInfo keys changed, reconfiguring pool to {}:{}",
                            endpoint.command_host,
                            endpoint.command_port
                        );
                        pool.update_config(ConnConfig {
                            host: endpoint.command_host.clone(),
                            port: endpoint.command_port,
                            host_public_pem: info.command_host_key_pem,
                            private_key_pem: info.client_private_key_pem,
                            client_id: client_id.clone(),
                        });
                    }
                }
                Err(FetchError::Connection(err)) => {
                    // Disable the pool: its config and its ready connections
                    // both come from this channel, so with the channel gone they
                    // can only be stale.
                    let pool_disabled = pool.clear_config();
                    if pool_disabled {
                        reported_failure = true;
                        log::warn!("cmd-client bootstrap: management channel lost, pool disabled: {err}");
                    } else if reported_failure {
                        log::debug!("cmd-client bootstrap: fetch failed: {err}");
                    } else {
                        reported_failure = true;
                        log::warn!("cmd-client bootstrap: fetch failed: {err}");
                    }
                    // Replace the connection that round could not use, but stand
                    // the new one up first. The daemon reads a management
                    // connection ending as this instance being gone and takes its
                    // process trees down (see `hicodeerd::peers`), so closing
                    // this one before another is open would retire the trees of a
                    // client that is in fact still running. It counts the
                    // connections themselves, so the overlap below is what keeps
                    // the instance alive across the swap. Only worth doing when
                    // there is a connection to replace: without one the daemon
                    // holds nothing of ours to retire, and the round at the top of
                    // the loop establishes a connection on its own.
                    if session.is_some() {
                        match connect_management(
                            &endpoint,
                            &mgmt_client_priv_pem,
                            &mgmt_host_pub_pem,
                            &client_id,
                        )
                        .await
                        {
                            Ok(fresh) => {
                                // The old connection is dropped by this
                                // assignment, and only now that the reply above
                                // has already put the new one on the daemon's
                                // record. The connection established itself to the
                                // log on the way here.
                                session = Some(fresh);
                            }
                            Err(reconnect_err) => {
                                // Keep the old connection rather than dropping
                                // it: closing it here would be the very
                                // zero-connection window this exists to avoid,
                                // and if it is genuinely dead the round above will
                                // report that again on the next pass, which also
                                // retries this.
                                log::debug!("cmd-client bootstrap: management reconnect failed: {reconnect_err}");
                            }
                        }
                    }
                }
                Err(FetchError::Protocol(err)) => {
                    // The channel itself is fine, so the keys it last handed
                    // over stand and the pool keeps working: only this round's
                    // reply was unusable. Tearing the pool down over it would
                    // drop healthy connections the daemon never invalidated.
                    if reported_failure {
                        log::debug!("cmd-client bootstrap: unusable management reply: {err}");
                    } else {
                        reported_failure = true;
                        log::warn!("cmd-client bootstrap: unusable management reply, pool kept: {err}");
                    }
                }
            }
        }
    });
}

/// Why one bootstrap round failed.
///
/// The distinction matters because the pool is torn down on failure: only a
/// `Connection` failure says the management channel is unusable. A `Protocol`
/// failure leaves the channel intact, so the keys it last handed over still
/// stand and the pool must be left alone.
enum FetchError {
    /// The management connection could not be established, or has gone bad.
    Connection(String),
    /// The connection held, but the daemon's reply could not be used.
    Protocol(String),
}

/// One bootstrap round trip over the held management connection, opening that
/// connection first when there is not one yet.
async fn fetch_ssh_info(
    session: &mut Option<SshSession>,
    endpoint: &CommandEndpoint,
    mgmt_client_priv_pem: &str,
    mgmt_host_pub_pem: &str,
    client_id: &str,
) -> Result<SshInfo, FetchError> {
    if session.is_none() {
        *session = Some(
            connect_management(endpoint, mgmt_client_priv_pem, mgmt_host_pub_pem, client_id)
                .await
                .map_err(FetchError::Connection)?,
        );
    }
    let Some(connection) = session.as_ref() else {
        return Err(FetchError::Connection(
            "management connection missing".to_string(),
        ));
    };

    // Bounded like every other step here: a daemon that accepts the connection
    // but never answers would otherwise hang this loop for good, and take with
    // it any chance of recovering once the daemon does come back.
    let mut channel = tokio::time::timeout(MGMT_TIMEOUT, connection.channel_open_session())
        .await
        .map_err(|_| FetchError::Connection("open management channel timed out".to_string()))?
        .map_err(|err| FetchError::Connection(format!("open management channel: {err}")))?;
    // Tell the daemon which directory this side works in, so the programs it
    // spawns land their files in the same place this side already uses. Read
    // per round trip rather than once: the variable is set on the host
    // application's start-up path, and re-reading costs nothing while a request
    // that raced ahead of it simply arrives on the next tick.
    let data_root = std::env::var("HOME").ok();
    let request = bootstrap_command(data_root.as_deref());
    tokio::time::timeout(MGMT_TIMEOUT, channel.exec(true, request.as_str()))
        .await
        .map_err(|_| FetchError::Connection("exec bootstrap timed out".to_string()))?
        .map_err(|err| FetchError::Connection(format!("exec bootstrap: {err}")))?;

    let mut payload = Vec::new();
    loop {
        match tokio::time::timeout(MGMT_TIMEOUT, channel.wait()).await {
            Ok(Some(russh::ChannelMsg::Data { data })) => {
                payload.extend_from_slice(&data);
                if payload.len() > MAX_SSH_INFO_BYTES {
                    return Err(FetchError::Protocol("SshInfo too large".to_string()));
                }
            }
            Ok(Some(russh::ChannelMsg::Close)) | Ok(None) => break,
            Ok(Some(_)) => continue,
            Err(_) => {
                return Err(FetchError::Connection(
                    "management channel timed out".to_string(),
                ))
            }
        }
    }
    serde_json::from_slice(&payload)
        .map_err(|err| FetchError::Protocol(format!("parse SshInfo: {err}")))
}

/// Opens and authenticates one management connection.
///
/// The client config is left with its default keepalive (none) and given rekey
/// bounds measured in years: this connection has to survive a client that the
/// system has frozen, so nothing may be sent that expects an answer while that
/// is possible (see the module header).
async fn connect_management(
    endpoint: &CommandEndpoint,
    mgmt_client_priv_pem: &str,
    mgmt_host_pub_pem: &str,
    client_id: &str,
) -> Result<SshSession, String> {
    let expected_host = crate::pool::host_public_key(mgmt_host_pub_pem)
        .map_err(|err| format!("parse management host key: {err}"))?;
    let mut client_cfg = client::Config::default();
    client_cfg.limits = russh::Limits::new(
        crate::pool::REKEY_BYTE_LIMIT,
        crate::pool::REKEY_BYTE_LIMIT,
        crate::pool::REKEY_TIME_LIMIT,
    );
    let client_config = Arc::new(client_cfg);
    let addr = (endpoint.mgmt_host.as_str(), endpoint.mgmt_port);
    let mut connection = tokio::time::timeout(
        MGMT_TIMEOUT,
        client::connect(
            client_config,
            addr,
            VerifyHandler {
                expected: expected_host,
            },
        ),
    )
    .await
    .map_err(|_| format!("connect {addr:?} timed out"))?
    .map_err(|err| format!("connect {addr:?}: {err}"))?;

    let key = PrivateKey::from_openssh(mgmt_client_priv_pem)
        .map_err(|err| format!("parse management client key: {err}"))?;
    // Bounded like the connect above: a daemon that accepts the TCP connection
    // and then stops answering would otherwise park this call for good, and with
    // it the only loop that can notice the daemon coming back.
    let auth = tokio::time::timeout(
        MGMT_TIMEOUT,
        connection.authenticate_publickey(
            client_id,
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        ),
    )
    .await
    .map_err(|_| "management publickey auth timed out".to_string())?
    .map_err(|err| format!("management publickey auth: {err}"))?;
    if !auth.success() {
        return Err("management publickey auth rejected".to_string());
    }
    log::info!("cmd-client bootstrap: management connection established");
    Ok(connection)
}
