//! Embedded russh SSH server for the dynamic-key command listener.
//!
//! Accepts SSH connections from cmd-client on 127.0.0.1:4022, authenticates
//! with the dynamic client public key (password auth disabled), and runs exec
//! commands. Channel stdin is forwarded to the running child; stdout/stderr and
//! the exit status are bridged back by `exec`. Ported from the qemu-ssh-agentd
//! server with the guest-specific carrier removed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use russh::keys::ssh_key::PublicKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::sync::Mutex;

use crate::exec;

/// Monotonic id assigned to every accepted connection, so log lines from
/// concurrent connections (which share channel numbers) are distinguishable.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Per-connection handler state. russh `ChannelId` is a bare `u32` that
/// restarts from the same low value on every connection (each new exec opens
/// its own connection and lands on id 2), so the running-child map MUST be
/// private to one connection. A map shared across connections made concurrent
/// execs clobber each other's `ChildHandle`: a later command inserted its own
/// entry under the same channel id, dropping the running LSP's stdin pipe
/// mid-session (clangd then died with "Transport error: Input/output error"
/// right after replying to initialize).
pub struct ConnectionHandler {
    /// Per-connection sequence number (for correlating log lines).
    conn_id: u64,
    /// Accepted client public keys (this run's dynamic client key).
    authorized: Arc<Vec<PublicKey>>,
    /// channel id -> running child stdin for client-stdin forwarding.
    children: Arc<Mutex<HashMap<ChannelId, exec::ChildHandle>>>,
}

impl ConnectionHandler {
    fn new(authorized: Arc<Vec<PublicKey>>) -> Self {
        Self {
            conn_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            authorized,
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Factory for per-connection handlers. russh 0.55 `run_stream` takes a
/// concrete `Handler` (not a Server template), so the accept loop creates one
/// fresh `ConnectionHandler` per accepted connection, each owning a private
/// children map (see `ConnectionHandler`).
pub struct SshServer {
    authorized: Arc<Vec<PublicKey>>,
}

impl SshServer {
    pub fn new(authorized: Vec<PublicKey>) -> Self {
        Self {
            authorized: Arc::new(authorized),
        }
    }

    pub fn new_connection(&self) -> ConnectionHandler {
        ConnectionHandler::new(self.authorized.clone())
    }
}

impl server::Handler for ConnectionHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let accepted = self.authorized.iter().any(|k| k == key);
        if accepted {
            log::info!("ssh: publickey auth accepted for user {user}");
            Ok(Auth::Accept)
        } else {
            log::warn!("ssh: publickey auth rejected for user {user}");
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        log::info!(
            "ssh: conn {} channel {} open session",
            self.conn_id,
            channel.id()
        );
        Ok(true)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        log::info!(
            "ssh: conn {} data {} bytes on channel {channel}",
            self.conn_id,
            data.len()
        );
        exec::forward_stdin(&self.children, channel, data).await;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Client finished sending stdin. Drop the child stdin write end so a
        // line/batch-oriented child (e.g. `git cat-file --batch-check`) sees
        // EOF and exits. Without propagating EOF, such a child blocks forever
        // on stdin, the exec channel never closes, and a caller awaiting full
        // output hangs indefinitely.
        log::info!(
            "ssh: conn {} channel {channel} client eof; closing child stdin",
            self.conn_id
        );
        self.children.lock().await.remove(&channel);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        log::info!(
            "ssh: conn {} exec_request channel={channel} cmd={command}",
            self.conn_id
        );
        let handle = session.handle();
        let children = self.children.clone();
        let _ = handle.channel_success(channel).await;
        exec::spawn_command(children, channel, handle, &command).await;
        Ok(())
    }
}
