//! Text-level protocol shared between the daemon and cmd-client.
//!
//! Mirrors the `protocol.rs` in the daemon crate; keep every constant here byte-for-byte
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
/// receive the current `SshInfo` (dynamic command keys) for this daemon run.
/// The client may append the directory it works in: `BOOTSTRAP_COMMAND <path>`.
pub const BOOTSTRAP_COMMAND: &str = "hicodeerd-bootstrap";
/// First line of every command-channel exec payload, associating the exec with
/// a client-chosen session id: `SESSION_ID_PREFIX <session_id>`.
pub const SESSION_ID_PREFIX: &str = "__hicodeerd_sid__";
/// Reserved command-channel command for signaling a running session group:
/// `SIGNAL_PREFIX <session_id> <signal>`.
pub const SIGNAL_PREFIX: &str = "__hicodeerd_signal__";
/// Prefix of the identity this client presents as its SSH user name (see the
/// daemon-side copy of this file). It names one client instance for as long as
/// that instance lives, which is what lets the daemon take down everything the
/// instance started once it goes away.
pub const CLIENT_ID_PREFIX: &str = "hk-";

/// Builds the identity this client presents as its SSH user name.
///
/// Random rather than derived from anything about the host: two instances must
/// never collide, including one that replaces a predecessor still winding down.
/// A host with no readable random device falls back to the clock and process
/// id, which is enough to tell two instances apart.
pub fn new_client_id() -> String {
    use std::io::Read as _;

    let mut bytes = [0u8; 16];
    let seeded = std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .is_ok();
    if !seeded {
        bytes = fallback_bytes();
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(CLIENT_ID_PREFIX.len() + bytes.len() * 2);
    id.push_str(CLIENT_ID_PREFIX);
    for byte in bytes {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    id
}

/// Entropy substitute for a host without a readable random device.
fn fallback_bytes() -> [u8; 16] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    (nanos ^ ((std::process::id() as u128) << 64)).to_le_bytes()
}

/// Bootstrap payload returned by the management listener.
///
/// The command host key and the client private key are freshly generated for
/// this daemon run; this client uses them to authenticate against and verify
/// the command listener (host key verification, never AcceptAll).
#[derive(Debug, Clone, Deserialize)]
pub struct SshInfo {
    pub command_port: u16,
    /// OpenSSH-formatted public half of this run's command host key.
    pub command_host_key_pem: String,
    /// OpenSSH-formatted private half of this run's command client key.
    pub client_private_key_pem: String,
}

/// Builds the management bootstrap request, appending the directory this side
/// works in so the daemon can adopt it for the programs it spawns.
///
/// A client that has not resolved a directory yet sends the bare command; the
/// daemon treats that exactly as it always has, so a plain request keeps
/// working against an older build.
pub fn bootstrap_command(data_root: Option<&str>) -> String {
    match data_root {
        Some(root) if !root.is_empty() => format!("{BOOTSTRAP_COMMAND} {root}"),
        _ => BOOTSTRAP_COMMAND.to_string(),
    }
}

/// Builds the exec payload for one spawn: a session-id first line followed by
/// the real shell command (the daemon strips the first line and runs the rest).
pub fn sid_payload(session_id: u64, shell_command: &str) -> String {
    format!("{SESSION_ID_PREFIX} {session_id}\n{shell_command}")
}

/// Builds the reserved signal command for a running session.
pub fn signal_command(session_id: u64, signal: i32) -> String {
    format!("{SIGNAL_PREFIX} {session_id} {signal}")
}
