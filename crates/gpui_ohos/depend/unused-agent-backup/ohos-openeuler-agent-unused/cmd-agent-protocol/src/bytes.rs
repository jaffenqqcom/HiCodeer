//! Base64 encoding for binary fields in JSON frames.
//!
//! JSON cannot carry raw bytes, so binary payloads (stdin/stdout/stderr)
//! are base64-encoded on the wire. This module provides the serde helper.

use serde::{Deserialize, Deserializer, Serializer};

pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes))
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s)
        .map_err(serde::de::Error::custom)
}
