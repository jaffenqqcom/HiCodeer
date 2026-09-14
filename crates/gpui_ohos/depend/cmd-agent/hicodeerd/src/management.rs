//! Fixed-key management SSH listener (127.0.0.1:4023).
//!
//! cmd-client connects here with its fixed management key, authenticates with
//! the fixed client public key, and sends the reserved `BOOTSTRAP_COMMAND`
//! command to receive this run's `SshInfo` (the dynamic command host key +
//! client private key + command port). Each daemon restart regenerates the
//! dynamic keys, so the command listener the client is handed is always the
//! current one.
//!
//! The client appends the directory it works in, which this side adopts for the
//! programs it spawns -- see `session_tmp` and `shim`. That is the only
//! path by which the daemon learns the directory: it runs under its own account
//! and cannot read the host application's environment, so the request has to
//! carry it.

use std::path::Path;
use std::sync::Arc;

use russh::keys::ssh_key::PublicKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId};

use crate::protocol::{parse_bootstrap_command, SshInfo};

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
/// key is accepted; the only supported exec is `BOOTSTRAP_COMMAND`.
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
            // The poll behind this connection is the client's heartbeat: it
            // arrives every few seconds for as long as the client is alive, and
            // its absence is what says the client is gone (see `peers`).
            crate::peers::touch(user);
            Ok(Auth::Accept)
        } else {
            log::warn!("mgmt: publickey auth rejected for user {user}");
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data);
        let handle = session.handle();
        match parse_bootstrap_command(command.trim()) {
            Some(data_root) => {
                if let Some(root) = data_root {
                    // Arrives on every poll; the adoption itself happens once.
                    let root = Path::new(&root);
                    crate::session_tmp::adopt(root);
                    crate::shim::adopt(root);
                    // The root is also where this run's log file goes.
                    crate::logger::attach_file(root);
                }
                let _ = handle.channel_success(channel).await;
                if !self.ssh_info_json.is_empty() {
                    let _ = handle
                        .data(channel, russh::CryptoVec::from(self.ssh_info_json.as_bytes()))
                        .await;
                }
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            }
            None => {
                log::warn!("mgmt: unsupported command on channel={channel}: {command}");
                let _ = handle.channel_failure(channel).await;
            }
        }
        Ok(())
    }
}
