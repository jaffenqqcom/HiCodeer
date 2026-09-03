//! SSH bootstrap over the management virtio-serial port.
//!
//! The guest's ssh-agentd serves an `SshInfo` frame (russh server port, client
//! private key, pid dir) in reply to any client frame. This module connects the
//! management serial socket, sends a `Hello`, reads `SshInfo`, adds the QEMU
//! hostfwd (host 127.0.0.1:<fixed port> -> guest <port>), and hands the pool its
//! connection config. It keeps listening so a russh-server restart (new port)
//! triggers a reconfiguration. Runs on a dedicated thread.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::pool::{ConnConfig, Pool};

/// Management serial socket file (QEMU chardev server) under the port dir.
const MGMT_SOCKET: &str = "mgmt.sock";
/// Delay between connection attempts while the socket is absent.
const MGMT_RETRY: Duration = Duration::from_secs(1);
/// First host-side hostfwd port; bumped when a port is already taken.
const HOSTFWD_START_PORT: u16 = 2222;
/// The slirp user netdev the hostfwd rules attach to.
const NETDEV_ID: &str = "net0";
/// Hostfwd binds the host side to loopback only.
const HOST_ADDR: &str = "127.0.0.1";
/// Upper bound on a management frame payload.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Bootstrap payload served by the guest over the management port.
#[derive(Deserialize)]
struct SshInfo {
    port: u16,
    client_private_key_pem: String,
    pid_dir: String,
    #[allow(dead_code)]
    sshd_pid: u32,
}

/// Runs the bootstrap loop forever on the calling thread.
pub fn start(
    port_dir: &Path,
    pool: Arc<Pool>,
    pid_dir: Arc<Mutex<Option<String>>>,
) {
    let mgmt_socket = port_dir.join(MGMT_SOCKET);
    let qmp_socket = port_dir.join(crate::QMP_SOCKET);
    log::info!("ssh bootstrap: watching {}", mgmt_socket.display());
    let mut host_port = HOSTFWD_START_PORT;
    loop {
        match UnixStream::connect(&mgmt_socket) {
            Ok(stream) => {
                log::info!("ssh bootstrap: connected to management socket");
                if let Err(err) = session(
                    stream,
                    &qmp_socket,
                    &pool,
                    &pid_dir,
                    &mut host_port,
                ) {
                    log::warn!("ssh bootstrap: management session ended: {err}");
                }
            }
            Err(err) => log::debug!("ssh bootstrap: connect mgmt: {err}"),
        }
        std::thread::sleep(MGMT_RETRY);
    }
}

/// One management session: Hello, then keep reconfiguring on every SshInfo.
fn session(
    mut stream: UnixStream,
    qmp_socket: &Path,
    pool: &Arc<Pool>,
    pid_dir: &Arc<Mutex<Option<String>>>,
    host_port: &mut u16,
) -> std::io::Result<()> {
    // Hello frame: little-endian length prefix, matching the guest's read_frame
    // (the guest replies with SshInfo to any complete frame).
    let hello: &[u8] = b"{\"hello\":1}";
    stream.write_all(&(hello.len() as u32).to_le_bytes())?;
    stream.write_all(hello)?;
    stream.flush()?;
    loop {
        let info = read_frame(&mut stream)?;
        log::info!(
            "ssh bootstrap: SshInfo port={} pid_dir={}",
            info.port,
            info.pid_dir
        );
        // hostfwd: fixed host port -> current guest port. If the add fails
        // (port busy), bump the host port and retry once.
        let qmp = qmp_socket.to_string_lossy().into_owned();
        if let Err(err) = crate::qmp::hostfwd_add(&qmp, NETDEV_ID, *host_port, info.port) {
            log::warn!("ssh bootstrap: hostfwd_add {}:{} failed: {err}", *host_port, info.port);
            *host_port += 1;
            crate::qmp::hostfwd_add(&qmp, NETDEV_ID, *host_port, info.port)
                .inspect_err(|err| log::error!("ssh bootstrap: hostfwd_add retry failed: {err}"))?;
        }
        pool.update_config(ConnConfig {
            host: HOST_ADDR.to_string(),
            port: *host_port,
            private_key_pem: info.client_private_key_pem.clone(),
        });
        *pid_dir.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(info.pid_dir);
        log::info!("ssh bootstrap: pool configured to {}:{}", HOST_ADDR, *host_port);
    }
}

/// Reads one length-prefixed JSON frame and deserializes it.
fn read_frame(stream: &mut UnixStream) -> std::io::Result<SshInfo> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bootstrap frame too large: {len} bytes"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}
