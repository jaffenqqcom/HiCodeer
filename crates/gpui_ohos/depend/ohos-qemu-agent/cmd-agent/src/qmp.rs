//! Minimal QMP client for runtime 9p exports.
//!
//! Work-directory fsdevs cannot be created at QEMU boot (their paths are only
//! known when the user opens a folder), so the mount path creates them on
//! demand through QEMU's QMP interface: `fsdev-add`, then `device_add` a
//! virtio-9p-pci device bound to the new fsdev. The QMP socket is a server=on
//! chardev that serves one client at a time; these helpers connect, issue the
//! commands and disconnect, which suits the low-frequency open-folder path.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};

/// Security model for work-directory exports. `passthrough` keeps the host
/// files untouched: mapped-file would scatter per-file metadata files through
/// the user's folder (unacceptable inside a git repository), and the sandbox
/// root keeps using mapped-file from the QEMU command line. The guest then
/// sees the host file ownership; git runs as guest root, so cmd-agentd
/// provisions `safe.directory` so the resulting ownership check is silenced.
const SECURITY_MODEL: &str = "passthrough";

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
    log::info!("[diag] qmp::connect_qmp: greeting banner: {banner}");
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
    log::info!("[diag] qmp::connect_qmp: connected to {socket}");
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
    log::info!("[diag] qmp::command: sending {execute}");
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
        log::info!("[diag] qmp::command: {execute} ok");
        return Ok(());
    }
}

/// Creates a work-directory fsdev and hotplugs a virtio-9p device against it
/// in one QMP session.
pub fn create_workdir_fsdev(
    socket: &str,
    fsdev_id: &str,
    path: &str,
    device_id: &str,
    mount_tag: &str,
    bus: &str,
) -> std::io::Result<()> {
    log::info!("[diag] qmp::create_workdir_fsdev: fsdev={fsdev_id} path={path} device={device_id} tag={mount_tag}"
    );
    let (mut stream, mut reader) = connect_qmp(socket)?;
    command(
        &mut stream,
        &mut reader,
        "fsdev-add",
        json!({
            "id": fsdev_id,
            "path": path,
            "security-model": SECURITY_MODEL,
        }),
    )?;
    command(
        &mut stream,
        &mut reader,
        "device_add",
        json!({
            "driver": "virtio-9p-pci",
            "id": device_id,
            "fsdev": fsdev_id,
            "mount_tag": mount_tag,
            // Attach to a pre-created root port (rp<N>): pcie.0 has no hotplug
            // handler, but each root port's secondary bus does, so runtime
            // hotplug of a virtio-9p device works. Each root port has one slot,
            // so the caller picks a distinct rp per mounted folder.
            "bus": bus,
        }),
    )?;
    let _ = stream.shutdown(Shutdown::Both);
    log::info!("[diag] qmp::create_workdir_fsdev: done for {fsdev_id}");
    Ok(())
}

/// Sends `quit` to shut the QEMU machine down cleanly. Used by the restart
/// path when the guest agent is judged lost.
pub fn quit(socket: &str) -> std::io::Result<()> {
    let (mut stream, mut reader) = connect_qmp(socket)?;
    command(&mut stream, &mut reader, "quit", json!({}))?;
    let _ = stream.shutdown(Shutdown::Both);
    log::info!("[diag] qmp::quit: quit sent");
    Ok(())
}
