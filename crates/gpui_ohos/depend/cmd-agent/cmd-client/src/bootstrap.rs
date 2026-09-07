//! SSH bootstrap over the fixed-key management listener (127.0.0.1:4023).
//!
//! Connects to zcoderd's management port with the fixed management keys,
//! authenticates, sends the reserved `zcoderd-bootstrap` command, and reads the
//! returned `SshInfo` (this run's dynamic command host key + client private key
//! + command port). When zcoderd restarts its dynamic keys change, so this loop
//! re-fetches periodically and hands the pool a new config only when something
//! changed. Runs on a dedicated thread (never on a zcoder/GPUI calling thread).

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

use crate::pool::{ConnConfig, Pool, VerifyHandler};
use crate::protocol::{BOOTSTRAP_COMMAND, LOOPBACK_ADDR, MANAGEMENT_PORT, SshInfo};

/// Delay between bootstrap re-fetches while zcoderd is up (config unchanged).
const BOOTSTRAP_INTERVAL: Duration = Duration::from_secs(10);
/// Timeout for one management connection / bootstrap round trip.
const MGMT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on the SshInfo payload, far larger than any serialized keys JSON.
const MAX_SSH_INFO_BYTES: usize = 64 * 1024;

/// Runs the bootstrap loop forever on the calling thread.
pub fn start(pool: Arc<Pool>, mgmt_client_priv_pem: String, mgmt_host_pub_pem: String) {
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
        // reconnect, without ever blocking a zcoder calling thread.
        let mut fetch_now = true;
        loop {
            if !fetch_now {
                tokio::select! {
                    _ = pool.poke.notified() => {}
                    _ = tokio::time::sleep(BOOTSTRAP_INTERVAL) => {}
                }
            }
            fetch_now = false;
            match fetch_ssh_info(&mgmt_client_priv_pem, &mgmt_host_pub_pem).await {
                Ok(info) => {
                    let changed = match pool.config() {
                        Some(current) => {
                            current.port != info.command_port
                                || current.host_public_pem != info.command_host_key_pem
                                || current.private_key_pem != info.client_private_key_pem
                        }
                        None => true,
                    };
                    if changed {
                        log::info!(
                            "cmd-client bootstrap: new SshInfo port={} (keys changed), reconfiguring pool",
                            info.command_port
                        );
                        pool.update_config(ConnConfig {
                            host: LOOPBACK_ADDR.to_string(),
                            port: info.command_port,
                            host_public_pem: info.command_host_key_pem,
                            private_key_pem: info.client_private_key_pem,
                        });
                    }
                }
                Err(err) => {
                    log::debug!("cmd-client bootstrap: fetch failed: {err}");
                }
            }
        }
    });
}

/// One bootstrap round trip: connect the management port, authenticate with the
/// fixed client key, exec `zcoderd-bootstrap`, and deserialize the returned
/// `SshInfo`.
async fn fetch_ssh_info(
    mgmt_client_priv_pem: &str,
    mgmt_host_pub_pem: &str,
) -> Result<SshInfo, String> {
    let expected_host =
        crate::pool::host_public_key(mgmt_host_pub_pem).map_err(|err| format!("parse management host key: {err}"))?;
    let client_config = Arc::new(client::Config::default());
    let addr = (LOOPBACK_ADDR, MANAGEMENT_PORT);
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
        .authenticate_publickey("root", PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|err| format!("management publickey auth: {err}"))?;
    if !auth.success() {
        return Err("management publickey auth rejected".to_string());
    }

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|err| format!("open management channel: {err}"))?;
    channel
        .exec(true, BOOTSTRAP_COMMAND)
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
