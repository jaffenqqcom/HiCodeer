//! Key generation and serialization for the zcoderd SSH server.
//!
//! Two keypairs are involved at startup:
//! - a freshly generated **dynamic** host key and client keypair for the
//!   command listener (both regenerated on every zcoderd run, never persisted),
//!   and
//! - a **fixed** management host key plus the authorized management client
//!   public key, generated once at build time and shipped in this crate's conf
//!   directory (the management side; the client keeps the matching halves).

use rand_core::OsRng;
use russh::keys::ssh_key::{LineEnding, PrivateKey, PublicKey};

/// Host key plus the (dynamic) client keypair.
pub struct Keys {
    /// Server host key presented to connecting clients.
    pub host_key: PrivateKey,
    /// Client private key, sent to cmd-client in the bootstrap `SshInfo`.
    pub client_private: PrivateKey,
    /// Client public key, accepted by the command listener's publickey auth.
    pub client_public: PublicKey,
}

/// Generates a fresh ed25519 host key and client keypair.
pub fn generate() -> Result<Keys, String> {
    let host_key = PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519)
        .map_err(|err| format!("generate host key: {err}"))?;
    let client_private = PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519)
        .map_err(|err| format!("generate client key: {err}"))?;
    let client_public = client_private.public_key().clone();
    log::info!("keygen: generated ed25519 host key and client keypair");
    Ok(Keys {
        host_key,
        client_private,
        client_public,
    })
}

/// Serializes a private key as an OpenSSH private-key blob (what russh clients
/// hand to `authenticate_publickey` / what russh servers load as a host key).
pub fn private_openssh(key: &PrivateKey) -> Result<String, String> {
    key.to_openssh(LineEnding::LF)
        .map(|key| key.to_string())
        .map_err(|err| format!("serialize private key: {err}"))
}

/// Serializes a public key as its single-line OpenSSH form, the text cmd-client
/// parses with `PublicKey::from_openssh` for host key verification.
pub fn public_openssh(key: &PublicKey) -> Result<String, String> {
    key.to_openssh()
        .map_err(|err| format!("serialize public key: {err}"))
}
