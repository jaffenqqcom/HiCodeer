//! Text-level protocol shared between zcoderd and cmd-client.
//!
//! cmd-client keeps a mirrored copy in `cmd-client/src/protocol.rs`; keep every
//! constant here byte-for-byte identical to that file (there is intentionally no
//! crate dependency between the two sides, per DESIGN.md section 2).

use serde::{Deserialize, Serialize};

/// Port of the dynamic-key command SSH listener (command port).
pub const COMMAND_PORT: u16 = 4022;
/// Port of the fixed-key management SSH listener (bootstrap).
pub const MANAGEMENT_PORT: u16 = 4023;
/// Loopback address both listeners bind to.
pub const LOOPBACK_ADDR: &str = "127.0.0.1";
/// Reserved management command: a connected management client sends this to
/// receive the current `SshInfo` (dynamic command keys) for this zcoderd run.
pub const BOOTSTRAP_COMMAND: &str = "zcoderd-bootstrap";
/// First line of every command-channel exec payload, associating the exec with
/// a client-chosen session id: `__zcoderd_sid__ <session_id>`.
pub const SESSION_ID_PREFIX: &str = "__zcoderd_sid__";
/// Reserved command-channel command for signaling a running session group:
/// `__zcoderd_signal__ <session_id> <signal>`.
pub const SIGNAL_PREFIX: &str = "__zcoderd_signal__";

/// Bootstrap payload returned by the management listener.
///
/// The command host key and the client private key are freshly generated for
/// this zcoderd run; the client uses them to authenticate against and verify
/// the command listener (host key verification, never AcceptAll).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshInfo {
    pub command_port: u16,
    /// OpenSSH-formatted public half of this run's command host key.
    pub command_host_key_pem: String,
    /// OpenSSH-formatted private half of this run's command client key.
    pub client_private_key_pem: String,
}

/// Splits a command-channel exec payload into its session id and the real shell
/// command. The payload is `__zcoderd_sid__ <sid>\n<command>`; if the first line
/// does not carry the prefix, the whole payload is returned with sid 0 (no
/// session is registered for it).
pub fn split_sid_payload(command: &str) -> (u64, String) {
    match command.split_once('\n') {
        Some((head, body)) => {
            if let Some(id_text) = head.strip_prefix(SESSION_ID_PREFIX) {
                let sid = id_text.trim().parse::<u64>().unwrap_or(0);
                if sid != 0 {
                    return (sid, body.to_string());
                }
            }
            (0, command.to_string())
        }
        None => (0, command.to_string()),
    }
}

/// Parses a reserved signal command `__zcoderd_signal__ <session_id> <signal>`.
pub fn parse_signal_command(command: &str) -> Option<(u64, i32)> {
    let mut parts = command.split_whitespace();
    if parts.next() != Some(SIGNAL_PREFIX) {
        return None;
    }
    let sid = parts.next()?.parse::<u64>().ok()?;
    let signal = parts.next()?.parse::<i32>().ok()?;
    Some((sid, signal))
}
