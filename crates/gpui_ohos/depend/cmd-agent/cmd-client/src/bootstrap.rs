//! SSH bootstrap over the fixed-key management listener (127.0.0.1:4023).
//!
//! Connects to the daemon's management port with the fixed management keys,
//! authenticates, sends the reserved `BOOTSTRAP_COMMAND` command, and reads the
//! returned `SshInfo` (this run's dynamic command host key + client private key
//! + command port). When the daemon restarts its dynamic keys change, so this loop
//! re-fetches periodically and hands the pool a new config only when something
//! changed. Runs on a dedicated thread (never on a host-application/GPUI calling thread).
//!
//! The re-fetch doubles as this client's heartbeat: the user name it
//! authenticates with names this instance, so every poll tells the daemon the
//! instance is still there, and the daemon takes its absence -- not this loop's
//! own knowledge -- as the end of the instance (see the daemon's `peers`).

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

use crate::endpoint::CommandEndpoint;
use crate::pool::{ConnConfig, Pool, VerifyHandler};
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
                }
            }
        }
    });
}

/// One bootstrap round trip: connect the endpoint's management port,
/// authenticate with the fixed client key, exec `BOOTSTRAP_COMMAND`, and
/// deserialize the returned `SshInfo`.
async fn fetch_ssh_info(
    endpoint: &CommandEndpoint,
    mgmt_client_priv_pem: &str,
    mgmt_host_pub_pem: &str,
    client_id: &str,
) -> Result<SshInfo, String> {
    let expected_host =
        crate::pool::host_public_key(mgmt_host_pub_pem).map_err(|err| format!("parse management host key: {err}"))?;
    let client_config = Arc::new(client::Config::default());
    let addr = (endpoint.mgmt_host.as_str(), endpoint.mgmt_port);
    let mut session = tokio::time::timeout(
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
    let auth = session
        .authenticate_publickey(client_id, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|err| format!("management publickey auth: {err}"))?;
    if !auth.success() {
        return Err("management publickey auth rejected".to_string());
    }

    let mut channel = session
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
