//! Text-level protocol shared between zcoderd and cmd-client.
//!
//! Mirrors `zcoderd/src/protocol.rs`; keep every constant here byte-for-byte
//! identical to that file (there is intentionally no crate dependency between
//! the two sides, per DESIGN.md section 2).

use serde::Deserialize;

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
/// this zcoderd run; this client uses them to authenticate against and verify
/// the command listener (host key verification, never AcceptAll).
#[derive(Debug, Clone, Deserialize)]
pub struct SshInfo {
    pub command_port: u16,
    /// OpenSSH-formatted public half of this run's command host key.
    pub command_host_key_pem: String,
    /// OpenSSH-formatted private half of this run's command client key.
    pub client_private_key_pem: String,
}

/// Builds the exec payload for one spawn: a session-id first line followed by
/// the real shell command (zcoderd strips the first line and runs the rest).
pub fn sid_payload(session_id: u64, shell_command: &str) -> String {
    format!("{SESSION_ID_PREFIX} {session_id}\n{shell_command}")
}

/// Builds the reserved signal command for a running session.
pub fn signal_command(session_id: u64, signal: i32) -> String {
    format!("{SIGNAL_PREFIX} {session_id} {signal}")
}
