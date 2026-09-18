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

/// Delay between bootstrap re-fetches while the daemon is up (config unchanged).
const BOOTSTRAP_INTERVAL: Duration = Duration::from_secs(10);
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
        // Held open for the life of this loop (see the module header). Only a
        // failure drops it, and the round after that opens a new one.
        let mut session: Option<SshSession> = None;
        // Fetch immediately at startup, then wait for either the periodic tick
        // or an on-demand poke (a command arrived while the pool had no ready
        // connection) before fetching again. This keeps the background retry at
        // BOOTSTRAP_INTERVAL while letting a command trigger an instant
        // reconnect, without ever blocking a host-application calling thread.
        let mut fetch_now = true;
        loop {
            if !fetch_now {
                tokio::select! {
                    _ = pool.poke.notified() => {}
                    _ = tokio::time::sleep(BOOTSTRAP_INTERVAL) => {}
                }
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
                Err(err) => {
                    log::warn!("cmd-client bootstrap: fetch failed: {err}");
                    // Whatever went wrong, this connection cannot be trusted
                    // any more: drop it so the next round starts a new one.
                    session = None;
                }
            }
        }
    });
}

/// One bootstrap round trip over the held management connection, opening that
/// connection first when there is not one yet.
async fn fetch_ssh_info(
    session: &mut Option<SshSession>,
    endpoint: &CommandEndpoint,
    mgmt_client_priv_pem: &str,
    mgmt_host_pub_pem: &str,
    client_id: &str,
) -> Result<SshInfo, String> {
    if session.is_none() {
        *session = Some(
            connect_management(endpoint, mgmt_client_priv_pem, mgmt_host_pub_pem, client_id)
                .await?,
        );
    }
    let Some(connection) = session.as_ref() else {
        return Err("management connection missing".to_string());
    };

    let mut channel = connection
        .channel_open_session()
        .await
        .map_err(|err| format!("open management channel: {err}"))?;
    // Tell the daemon which directory this side works in, so the programs it
    // spawns land their files in the same place this side already uses. Read
    // per round trip rather than once: the variable is set on the host
    // application's start-up path, and re-reading costs nothing while a request
    // that raced ahead of it simply arrives on the next tick.
    let data_root = std::env::var("HOME").ok();
    let request = bootstrap_command(data_root.as_deref());
    channel
        .exec(true, request.as_str())
        .await
        .map_err(|err| format!("exec bootstrap: {err}"))?;

    let mut payload = Vec::new();
    loop {
        match tokio::time::timeout(MGMT_TIMEOUT, channel.wait()).await {
            Ok(Some(russh::ChannelMsg::Data { data })) => {
                payload.extend_from_slice(&data);
                if payload.len() > MAX_SSH_INFO_BYTES {
                    return Err("SshInfo too large".to_string());
                }
            }
            Ok(Some(russh::ChannelMsg::Close)) | Ok(None) => break,
            Ok(Some(_)) => continue,
            Err(_) => return Err("management channel timed out".to_string()),
        }
    }
    serde_json::from_slice(&payload).map_err(|err| format!("parse SshInfo: {err}"))
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
    let auth = connection
        .authenticate_publickey(client_id, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|err| format!("management publickey auth: {err}"))?;
    if !auth.success() {
        return Err("management publickey auth rejected".to_string());
    }
    log::info!("cmd-client bootstrap: management connection established");
    Ok(connection)
}
