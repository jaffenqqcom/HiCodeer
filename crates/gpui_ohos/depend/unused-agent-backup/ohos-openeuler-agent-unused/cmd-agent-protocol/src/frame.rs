//! Length-prefixed frame transport.
//!
//! A frame is `[4-byte little-endian length][JSON payload]`. The length
//! prefix lets a reader slice a complete JSON message off the byte stream
//! without parsing partial data.

use serde::de::DeserializeOwned;
use serde::Serialize;
use smol::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result, ResultContext};

/// Upper bound on a single frame payload, guarding against unbounded reads.
const MAX_FRAME_SIZE: usize = 256 * 1024 * 1024;

/// Writes a message as a length-prefixed JSON frame.
pub async fn write_message<S, M>(stream: &mut S, message: &M) -> Result<()>
where
    S: AsyncWrite + Unpin,
    M: Serialize,
{
    let payload = serde_json::to_vec(message).with_context(|| "serializing message".to_string())?;
    let len = payload.len() as u32;
    stream
        .write_all(&len.to_le_bytes())
        .await
        .with_context(|| "writing frame length".to_string())?;
    stream
        .write_all(&payload)
        .await
        .with_context(|| "writing frame payload".to_string())?;
    stream.flush().await.with_context(|| "flushing frame".to_string())?;
    log::debug!("wrote frame: {} bytes", len);
    Ok(())
}

/// Reads a complete frame from the stream and deserializes it into `M`.
pub async fn read_message<S, M>(stream: &mut S) -> Result<M>
where
    S: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    let mut len_bytes = [0u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .await
        .with_context(|| "reading frame length".to_string())?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(Error::Protocol(format!(
            "frame too large: {len} bytes exceeds {MAX_FRAME_SIZE}"
        )));
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .with_context(|| "reading frame payload".to_string())?;
    let message = serde_json::from_slice(&payload).with_context(|| "deserializing message".to_string())?;
    log::debug!("read frame: {} bytes", len);
    Ok(message)
}
