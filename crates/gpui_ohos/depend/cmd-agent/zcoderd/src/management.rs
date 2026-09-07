//! Fixed-key management SSH listener (127.0.0.1:4023).
//!
//! cmd-client connects here with its fixed management key, authenticates with
//! the fixed client public key, and sends the reserved `zcoderd-bootstrap`
//! command to receive this run's `SshInfo` (the dynamic command host key +
//! client private key + command port). Each zcoderd restart regenerates the
//! dynamic keys, so the command listener the client is handed is always the
//! current one.

use std::sync::Arc;

use russh::keys::ssh_key::PublicKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId};

use crate::protocol::{BOOTSTRAP_COMMAND, SshInfo};

/// Factory for per-connection management handlers (stateless apart from the
/// fixed authorized key and this run's serialized `SshInfo`).
pub struct ManagementServer {
    authorized: Arc<Vec<PublicKey>>,
    ssh_info_json: Arc<String>,
}

impl ManagementServer {
    pub fn new(authorized: Vec<PublicKey>, ssh_info: &SshInfo) -> Self {
        let ssh_info_json = serde_json::to_string(ssh_info)
            .map(Arc::new)
            .unwrap_or_else(|err| {
                log::error!("management: serialize SshInfo: {err}");
                Arc::new(String::new())
            });
        Self {
            authorized: Arc::new(authorized),
            ssh_info_json,
        }
    }

    pub fn new_connection(&self) -> ManagementHandler {
        ManagementHandler {
            authorized: self.authorized.clone(),
            ssh_info_json: self.ssh_info_json.clone(),
        }
    }
}

/// Per-connection management handler. Only the fixed management client public
/// key is accepted; the only supported exec is `zcoderd-bootstrap`.
pub struct ManagementHandler {
    authorized: Arc<Vec<PublicKey>>,
    ssh_info_json: Arc<String>,
}

impl server::Handler for ManagementHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let accepted = self.authorized.iter().any(|k| k == key);
        if accepted {
            // Normal handshakes recur every ~10s (bootstrap polling); stay quiet.
            log::debug!("mgmt: publickey auth accepted for user {user}");
            Ok(Auth::Accept)
        } else {
            log::warn!("mgmt: publickey auth rejected for user {user}");
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        log::debug!("mgmt: channel {} open session", channel.id());
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        if command.trim() == BOOTSTRAP_COMMAND {
            log::debug!("mgmt: bootstrap requested on channel={channel}");
            let _ = handle.channel_success(channel).await;
            if !self.ssh_info_json.is_empty() {
                let _ = handle
                    .data(channel, russh::CryptoVec::from(self.ssh_info_json.as_bytes()))
                    .await;
            }
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        } else {
            log::warn!("mgmt: unsupported command on channel={channel}: {command}");
            let _ = handle.channel_failure(channel).await;
        }
        Ok(())
    }
}
