//! Key generation for the guest SSH server.
//!
//! Two keypairs are created at startup with russh-keys (ed25519):
//! - the server **host key**, served to every connecting client, and
//! - the one-time **client keypair**, whose public half is accepted by the
//!   server's auth handler and whose private half is sent to the zcoder host
//!   over the management virtio-serial port in SshInfo.

use rand_core::OsRng;
use russh::keys::ssh_key::{LineEnding, PrivateKey, PublicKey};

/// Host key plus the one-time client keypair.
pub struct Keys {
    /// Server host key presented to clients.
    pub host_key: PrivateKey,
    /// Client private key, sent to the zcoder host in SshInfo.
    pub client_private: PrivateKey,
    /// Client public key, accepted by the server's publickey auth.
    pub client_public: PublicKey,
}

/// Generates a fresh ed25519 host key and one-time client keypair.
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

/// Serializes the client private key as an OpenSSH private-key blob, the form
/// the zcoder host hands to russh's client for publickey authentication.
pub fn client_private_pem(key: &PrivateKey) -> Result<String, String> {
    key.to_openssh(LineEnding::LF)
        .map(|key| key.to_string())
        .map_err(|err| format!("serialize client private key: {err}"))
}
