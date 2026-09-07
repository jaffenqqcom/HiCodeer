//! In-process virtio-fs backend daemons (replacing the virtio-9p shares).
//!
//! Each static mount (/sandbox, /tools) is served by its own virtiofsd backend
//! running on a dedicated thread. QEMU's vhost-user-fs-pci frontend connects to
//! the backend's listening unix socket; the guest then mounts the matching tag.
//! The OHOS sandbox forbids spawning external processes, so the daemon is
//! driven through the `virtiofsd` crate as a library (no CLI, no seccomp).
//! This module is identical to qemu-cmd-agent's virtiofs; it is kept separate
//! so the old crate stays untouched.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use log::*;
use vhost::vhost_user::Listener;
use vhost_user_backend::VhostUserDaemon;
use virtiofsd::filesystem::{FileSystem, SerializableFileSystem};
use virtiofsd::passthrough::read_only::PassthroughFsRo;
use virtiofsd::passthrough::{CachePolicy, Config, PassthroughFs};
use virtiofsd::vhost_user::VhostUserFsBackendBuilder;
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

use crate::{FS_SOCKET_SANDBOX, FS_SOCKET_TOOLS, FS_WORK_PREFIX, QemuPaths};

/// Error type for the backend threads: the vhost-user / virtiofsd error enums
/// only implement `Display` (not `std::error::Error`), so they are boxed as
/// strings here.
type Result<T> = std::result::Result<T, String>;

/// Starts the virtio-fs backends for the static sandbox/tools mounts.
pub fn start(paths: &QemuPaths) {
    let port_dir = paths.port_dir.clone();
    let sandbox_sock = port_dir.join(FS_SOCKET_SANDBOX);
    let tools_sock = port_dir.join(FS_SOCKET_TOOLS);
    spawn_backend("sandbox".to_string(), sandbox_sock, paths.sandbox_mount.clone(), false);
    spawn_backend("tools".to_string(), tools_sock, paths.tools_mount.clone(), true);
}

/// Starts one virtiofsd backend for a dynamically mounted work directory.
/// Returns the socket path for the QMP chardev-add.
pub fn spawn_workdir(port_dir: &Path, sequence: u64, shared_dir: PathBuf, tag: String) -> PathBuf {
    let socket = port_dir.join(format!("{FS_WORK_PREFIX}{sequence}.sock"));
    info!(
        "virtiofs-{tag}: spawning workdir backend for {} on {}",
        shared_dir.display(),
        socket.display()
    );
    spawn_backend(tag, socket.clone(), shared_dir, false);
    socket
}

/// Spawns one virtiofsd backend thread for a single shared directory.
fn spawn_backend(tag: String, socket_path: PathBuf, shared_dir: PathBuf, readonly: bool) {
    info!(
        "virtiofs-{tag}: starting backend for {} on {}",
        shared_dir.display(),
        socket_path.display()
    );
    thread::Builder::new()
        .name(format!("virtiofsd-{tag}"))
        .spawn(move || {
            if let Err(err) = run_backend(&tag, &socket_path, &shared_dir, readonly) {
                error!("virtiofs-{tag}: backend failed: {err}");
            }
        })
        .expect("spawn virtiofsd thread");
}

/// Runs one virtiofsd backend until the vhost-user client (QEMU) disconnects.
fn run_backend(tag: &str, socket_path: &Path, shared_dir: &Path, readonly: bool) -> Result<()> {
    let listener = Listener::new(socket_path.to_string_lossy().into_owned(), true)
        .map_err(|e| e.to_string())?;
    let fs_cfg = Config {
        root_dir: shared_dir.to_string_lossy().into_owned(),
        cache_policy: CachePolicy::Auto,
        ..Default::default()
    };
    if readonly {
        let fs = PassthroughFsRo::new(fs_cfg).map_err(|e| e.to_string())?;
        serve(tag, socket_path, listener, fs)
    } else {
        let fs = PassthroughFs::new(fs_cfg).map_err(|e| e.to_string())?;
        serve(tag, socket_path, listener, fs)
    }
}

/// Generic backend loop over any virtiofsd filesystem implementation.
fn serve<F>(tag: &str, socket_path: &Path, mut listener: Listener, fs: F) -> Result<()>
where
    F: FileSystem + SerializableFileSystem + Send + Sync + 'static,
{
    let backend = Arc::new(
        VhostUserFsBackendBuilder::default()
            .set_tag(Some(tag.to_string()))
            .build(fs)
            .map_err(|e| e.to_string())?,
    );
    let mut daemon = VhostUserDaemon::new(
        format!("virtiofsd-{tag}"),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| e.to_string())?;
    info!(
        "virtiofs-{tag}: waiting for vhost-user connection on {}",
        socket_path.display()
    );
    daemon.start(&mut listener).map_err(|e| e.to_string())?;
    info!("virtiofs-{tag}: client connected, serving requests");
    daemon.wait().map_err(|e| e.to_string())?;
    info!("virtiofs-{tag}: client disconnected");
    Ok(())
}
