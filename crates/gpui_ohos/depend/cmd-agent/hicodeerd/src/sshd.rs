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

/// Pseudo-terminal parameters requested for a channel before its exec arrives.
#[derive(Clone, Debug)]
struct PtyRequest {
    cols: u32,
    rows: u32,
    term: String,
}

/// Per-connection handler state. russh `ChannelId` is a bare `u32` that
/// restarts from the same low value on every connection (each new exec opens
/// its own connection and lands on id 2), so the running-child map MUST be
/// private to one connection. A map shared across connections made concurrent
/// execs clobber each other's `ChildHandle`: a later command inserted its own
/// entry under the same channel id, dropping the running LSP's stdin pipe
/// mid-session (clangd then died with "Transport error: Input/output error"
/// right after replying to initialize).
pub struct ConnectionHandler {
    /// Per-connection sequence number (keeps pty masters apart).
    conn_id: u64,
    /// Identity this connection authenticated as, from the SSH user name; empty
    /// until it has. Every exec on this connection joins the process tree of
    /// the instance it names (see `peers`).
    client_id: String,
    /// Accepted client public keys (this run's dynamic client key).
    authorized: Arc<Vec<PublicKey>>,
    /// channel id -> running child stdin for client-stdin forwarding.
    children: Arc<Mutex<HashMap<ChannelId, exec::ChildHandle>>>,
    /// channel id -> requested pty parameters (cols, rows, term), set by
    /// pty_request and consumed by the exec that opens the interactive shell.
    ptys: Mutex<HashMap<ChannelId, PtyRequest>>,
}

impl ConnectionHandler {
    fn new(authorized: Arc<Vec<PublicKey>>) -> Self {
        Self {
            conn_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            client_id: String::new(),
            authorized,
            children: Arc::new(Mutex::new(HashMap::new())),
            ptys: Mutex::new(HashMap::new()),
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
            self.client_id = user.to_string();
            // Every connection an instance opens reports its identity, and the
            // periodic poll makes this its heartbeat.
            crate::peers::touch(user);
            Ok(Auth::Accept)
        } else {
            log::warn!("ssh: publickey auth rejected for user {user}");
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

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Interactive pty channels route input to the pty master; everything
        // else goes to the running exec child's stdin pipe.
        if !crate::pty::forward_input(self.conn_id, channel, data).await {
            exec::forward_stdin(&self.children, channel, data).await;
        }
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
        self.children.lock().await.remove(&channel);
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.ptys.lock().await.insert(
            channel,
            PtyRequest {
                cols: col_width,
                rows: row_height,
                term: term.to_string(),
            },
        );
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        crate::pty::resize(self.conn_id, channel, col_width, row_height).await;
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();

        // A pty was requested for this channel first: run an interactive shell
        // instead of the pipe-based exec (the terminal is talking to the guest).
        // The relay MUST run in its own task: this handler is the only place
        // where later `data`/`window_change` requests are delivered, so awaiting
        // the relay here would starve the shell of input and resizes.
        if let Some(pty) = self.ptys.lock().await.get(&channel).cloned() {
            let conn_id = self.conn_id;
            let client_id = self.client_id.clone();
            tokio::spawn(async move {
                crate::pty::run_pty_shell(
                    conn_id,
                    channel,
                    handle,
                    pty.cols,
                    pty.rows,
                    &command,
                    &pty.term,
                    &client_id,
                )
                .await;
            });
            return Ok(());
        }

        let children = self.children.clone();
        let client_id = self.client_id.clone();
        let _ = handle.channel_success(channel).await;
        exec::spawn_command(children, channel, handle, &command, &client_id).await;
        Ok(())
    }
}
