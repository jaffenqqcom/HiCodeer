//! Length-prefixed JSON frame transport (synchronous).
//!
//! A frame is `[4-byte little-endian length][JSON payload]`. The length prefix
//! lets a reader slice a complete JSON message off the byte stream without
//! parsing partial data. The guest-side agent reads and writes virtio-serial
//! ports as plain blocking character devices, so this module is synchronous.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{self, Read, Write};

/// Upper bound on a single frame payload, guarding against unbounded reads.
const MAX_FRAME_SIZE: usize = 256 * 1024 * 1024;

/// Writes a message as a length-prefixed JSON frame.
pub fn write_message<W, M>(writer: &mut W, message: &M) -> io::Result<()>
where
    W: Write,
    M: Serialize,
{
    let payload = serde_json::to_vec(message)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    // [diag] Trace every outbound frame (type + brief payload) so the whole
    // host<->guest exchange is auditable in one place.
    {
        let text = String::from_utf8_lossy(&payload);
        let brief = if text.len() > 900 {
            format!("{}...<{} bytes>", &text[..900], payload.len())
        } else {
            text.into_owned()
        };
        log::info!("[diag] frame::write: {brief}");
    }
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Reads a complete frame from the stream and deserializes it into `M`.
pub fn read_message<R, M>(reader: &mut R) -> io::Result<M>
where
    R: Read,
    M: DeserializeOwned,
{
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes exceeds {MAX_FRAME_SIZE}"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    // [diag] Trace every inbound frame.
    {
        let text = String::from_utf8_lossy(&payload);
        let brief = if text.len() > 900 {
            format!("{}...<{} bytes>", &text[..900], payload.len())
        } else {
            text.into_owned()
        };
        log::info!("[diag] frame::read: {brief}");
    }
    serde_json::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
