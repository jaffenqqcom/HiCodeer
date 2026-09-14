//! Text-level protocol shared between the daemon and cmd-client.
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
/// receive the current `SshInfo` (dynamic command keys) for this daemon run.
/// The client may append the directory it works in: `BOOTSTRAP_COMMAND <path>`.
pub const BOOTSTRAP_COMMAND: &str = "hicodeerd-bootstrap";
/// First line of every command-channel exec payload, associating the exec with
/// a client-chosen session id: `SESSION_ID_PREFIX <session_id>`.
pub const SESSION_ID_PREFIX: &str = "__hicodeerd_sid__";
/// Reserved command-channel command for signaling a running session group:
/// `SIGNAL_PREFIX <session_id> <signal>`.
pub const SIGNAL_PREFIX: &str = "__hicodeerd_signal__";
/// Prefix of the identity a client presents as its SSH user name: every
/// connection one instance opens carries `CLIENT_ID_PREFIX<random>`.
///
/// The daemon groups the process trees it spawns under the identity their
/// connection arrived with, so an instance that stops heartbeating -- or one
/// that replaces it -- can have everything it started taken down together (see
/// `peers`). A user name without this prefix (a bare account name, as older
/// clients send) identifies no instance and is left untracked.
pub const CLIENT_ID_PREFIX: &str = "hk-";

/// Bootstrap payload returned by the management listener.
///
/// The command host key and the client private key are freshly generated for
/// this daemon run; the client uses them to authenticate against and verify
/// the command listener (host key verification, never AcceptAll).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshInfo {
    pub command_port: u16,
    /// OpenSSH-formatted public half of this run's command host key.
    pub command_host_key_pem: String,
    /// OpenSSH-formatted private half of this run's command client key.
    pub client_private_key_pem: String,
}

/// Parses a management bootstrap request, returning the directory the client
/// wants this daemon to adopt when it named one.
///
/// The outer `None` means "not a bootstrap request at all", which lets the
/// caller tell an unknown command apart from a bootstrap that simply carries no
/// directory. A bare `BOOTSTRAP_COMMAND` yields `Some(None)`; anything appended
/// after whitespace is taken verbatim, so a directory containing spaces survives
/// the round trip.
pub fn parse_bootstrap_command(command: &str) -> Option<Option<String>> {
    let rest = command.strip_prefix(BOOTSTRAP_COMMAND)?;
    if rest.is_empty() {
        return Some(None);
    }
    if !rest.starts_with(char::is_whitespace) {
        // A different command that merely shares the prefix.
        return None;
    }
    match rest.trim() {
        "" => Some(None),
        data_root => Some(Some(data_root.to_string())),
    }
}

/// Splits a command-channel exec payload into its session id and the real shell
/// command. The payload is `SESSION_ID_PREFIX <sid>\n<command>`; if the first line
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

/// Parses a reserved signal command `SIGNAL_PREFIX <session_id> <signal>`.
pub fn parse_signal_command(command: &str) -> Option<(u64, i32)> {
    let mut parts = command.split_whitespace();
    if parts.next() != Some(SIGNAL_PREFIX) {
        return None;
    }
    let sid = parts.next()?.parse::<u64>().ok()?;
    let signal = parts.next()?.parse::<i32>().ok()?;
    Some((sid, signal))
}
