//! Management virtio-serial port bootstrap.
//!
//! Serves the `SshInfo` bootstrap frame to the host-side bootstrap. The
//! protocol is idempotent: every client `Hello` gets the current `SshInfo`
//! (host port to connect to, client private key, pid dir). The port is a
//! blocking character device; this module runs on a dedicated thread so the
//! async SSH server task is never blocked by port I/O.

use std::fs::File;
use std::io::{self, Read, Write};
use std::time::Duration;

use serde::Serialize;

/// Guest-side device path where S41virtioports symlinks the management port.
/// The symlink resolves to /dev/vport* once the port is created.
const VIRTIO_PORTS_DIR: &str = "/dev/virtio-ports";
/// Management port name (matches the QEMU chardev name in the host argv).
const MGMT_PORT_NAME: &str = "zcoder.ssh.mgmt";
/// Upper bound on a frame payload, guarding against unbounded reads.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// Delay between open attempts while the port device node is missing.
const OPEN_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Bootstrap payload handed to the host over the management port.
#[derive(Serialize)]
pub struct SshInfo {
    /// Random guest-side port the russh server listens on.
    pub port: u16,
    /// OpenSSH private key the host uses for publickey auth.
    pub client_private_key_pem: String,
    /// Writable guest dir where command pids are recorded for signaling.
    pub pid_dir: String,
    /// ssh-agentd pid (sshd_pid), for the host's restart bookkeeping.
    pub sshd_pid: u32,
}

/// Serves SshInfo on the management port forever. Blocking: run on a thread.
pub fn serve(info: SshInfo) {
    let dev = format!("{VIRTIO_PORTS_DIR}/{MGMT_PORT_NAME}");
    log::info!("serial: serving SshInfo on {dev}");
    loop {
        match File::options().read(true).write(true).open(&dev) {
            Ok(mut port) => {
                log::info!("serial: management port {dev} open");
                serve_loop(&mut port, &info);
                log::warn!("serial: management port {dev} closed, reopening");
            }
            Err(err) => log::debug!("serial: open {dev}: {err}"),
        }
        std::thread::sleep(OPEN_RETRY_DELAY);
    }
}

/// Reads client frames and replies with the current SshInfo for each one.
fn serve_loop(port: &mut File, info: &SshInfo) {
    loop {
        match read_frame(port) {
            Ok(bytes) => {
                log::info!(
                    "serial: got client frame: {}",
                    String::from_utf8_lossy(&bytes)
                );
                if let Err(err) = write_frame(port, info) {
                    log::error!("serial: write SshInfo: {err}");
                    return;
                }
                log::info!("serial: sent SshInfo port={} pid={}", info.port, info.sshd_pid);
            }
            Err(err) => {
                log::error!("serial: read frame: {err}");
                return;
            }
        }
    }
}

/// Writes a message as a length-prefixed JSON frame.
fn write_frame(port: &mut File, info: &SshInfo) -> io::Result<()> {
    let payload = serde_json::to_vec(info)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let len = payload.len() as u32;
    port.write_all(&len.to_le_bytes())?;
    port.write_all(&payload)?;
    port.flush()
}

/// Reads one length-prefixed JSON frame from the port.
fn read_frame(port: &mut File) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    port.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes exceeds {MAX_FRAME_SIZE}"),
        ));
    }
    let mut payload = vec![0u8; len];
    port.read_exact(&mut payload)?;
    Ok(payload)
}
