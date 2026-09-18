//! Fixed-key management SSH listener (127.0.0.1:4023).
//!
//! cmd-client connects here with its fixed management key, authenticates with
//! the fixed client public key, and sends the reserved `BOOTSTRAP_COMMAND`
//! command to receive this run's `SshInfo` (the dynamic command host key +
//! client private key + command port). Each daemon restart regenerates the
//! dynamic keys, so the command listener the client is handed is always the
//! current one.
//!
//! The connection is long-lived rather than one poll per connection: the client
//! holds it open for as long as its process runs and re-sends the request over
//! it whenever it needs the keys. Its presence is therefore what says the
//! instance is still there, and its end is what says the instance is gone (see
//! `peers`) -- so nothing here retires a client on a timer.
//!
//! The client appends the directory it works in, which this side adopts for the
//! programs it spawns -- see `session_tmp`. That is the only
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
            client_id: None,
            conn_token: crate::peers::new_management_token(),
        }
    }
}

/// Per-connection management handler. Only the fixed management client public
/// key is accepted; the only supported exec is `BOOTSTRAP_COMMAND`.
pub struct ManagementHandler {
    authorized: Arc<Vec<PublicKey>>,
    ssh_info_json: Arc<String>,
    /// Identity this connection authenticated as, from the SSH user name; `None`
    /// until it has. The instance it names stays alive for exactly as long as
    /// connections holding its tokens are open (see `peers`).
    client_id: Option<String>,
    /// This connection's token in that record.
    conn_token: u64,
}

/// Releases this connection's claim on the instance when the connection ends.
/// The handler is owned by `russh_server::run_stream` and dropped when that
/// returns, which is the moment the connection is over -- whether the client
/// closed it or its process went away, the kernel closes its sockets either
/// way.
impl Drop for ManagementHandler {
    fn drop(&mut self) {
        if let Some(client_id) = self.client_id.as_deref() {
            crate::peers::management_closed(client_id, self.conn_token);
        }
    }
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
            // Holding this connection open for as long as the client runs is
            // what says the instance is still there; its end is what says the
            // instance is gone (see `peers`).
            crate::peers::management_opened(user, self.conn_token);
            self.client_id = Some(user.to_string());
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
                if let (Some(root), Some(client_id)) =
                    (data_root.as_deref(), self.client_id.as_deref())
                {
                    // Arrives on every poll; the adoption itself happens once
                    // for each client.
                    let root = Path::new(root);
                    crate::session_tmp::adopt(client_id, root);
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
