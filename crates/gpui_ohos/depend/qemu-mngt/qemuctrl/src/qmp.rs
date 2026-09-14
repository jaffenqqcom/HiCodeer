//! Minimal QMP client for runtime vhost-user-fs exports.
//!
//! Work-directory shares cannot be created at QEMU boot (their host paths are
//! only known later, when a worktree is first used), so this helper drives
//! QEMU's QMP interface on demand. The QMP socket is a server=on chardev that
//! serves one client at a time; each helper opens a short-lived session.
//!
//! Hostfwd is deliberately **not** managed here: it is baked into the netdev
//! argv at boot, so the two helper functions for dynamic hostfwd were removed.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};

/// Cap on waiting for the QEMU greeting after connecting.
const GREETING_TIMEOUT: Duration = Duration::from_secs(10);

/// Opens a QMP session: connect, drain the greeting banner, negotiate
/// capabilities. Returns the stream and a buffered reader over a cloned fd so
/// reads and writes never contend on one BufReader.
fn connect_qmp(socket: &str) -> std::io::Result<(UnixStream, BufReader<UnixStream>)> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(GREETING_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut banner = String::new();
    let read = reader.read_line(&mut banner)?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "QMP connection closed before greeting",
        ));
    }
    let banner: Value = serde_json::from_str(banner.trim()).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("QMP greeting not JSON: {err}"),
        )
    })?;
    if banner.get("QMP").is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected QMP greeting: {banner}"),
        ));
    }
    command(&mut stream, &mut reader, "qmp_capabilities", json!({}))?;
    Ok((stream, reader))
}

/// Sends one QMP command and waits for its return, skipping async events that
/// arrive unrequested.
fn command(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    execute: &str,
    arguments: Value,
) -> std::io::Result<()> {
    let message = json!({ "execute": execute, "arguments": arguments });
    let line = format!(
        "{}\n",
        serde_json::to_string(&message)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?
    );
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    loop {
        let mut response = String::new();
        let read = reader.read_line(&mut response)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("QMP connection closed during {execute}"),
            ));
        }
        let value: Value = serde_json::from_str(response.trim()).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("QMP response not JSON: {err}"),
            )
        })?;
        if value.get("event").is_some() {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(std::io::Error::other(format!(
                "QMP {execute} failed: {error}"
            )));
        }
        return Ok(());
    }
}

/// Hotplugs a vhost-user-fs device for a work directory in one QMP session:
/// chardev-add a client socket to the virtiofsd backend's listening socket,
/// then device_add vhost-user-fs-pci bound to it.
pub fn create_workdir_vhost_fs(
    socket: &str,
    chardev_id: &str,
    backend_socket: &str,
    device_id: &str,
    mount_tag: &str,
    bus: &str,
) -> std::io::Result<()> {
    let (mut stream, mut reader) = connect_qmp(socket)?;
    command(
        &mut stream,
        &mut reader,
        "chardev-add",
        json!({
            "id": chardev_id,
            "backend": {
                "type": "socket",
                "data": {
                    "addr": {"type": "unix", "data": {"path": backend_socket}},
                    "server": false,
                }
            }
        }),
    )?;
    command(
        &mut stream,
        &mut reader,
        "device_add",
        json!({
            "driver": "vhost-user-fs-pci",
            "id": device_id,
            "chardev": chardev_id,
            "tag": mount_tag,
            // Attach to a pre-created root port (rp<N>): pcie.0 has no hotplug
            // handler, but each root port's secondary bus does.
            "bus": bus,
        }),
    )?;
    if let Err(err) = stream.shutdown(Shutdown::Both) {
        // Best-effort close; the process side drops the fd right after.
        log::debug!("qmp: shutdown session: {err}");
    }
    Ok(())
}
