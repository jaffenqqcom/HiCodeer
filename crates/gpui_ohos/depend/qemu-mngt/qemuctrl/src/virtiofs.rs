//! In-process virtio-fs backend daemons (replacing virtio-9p shares).
//!
//! Each shared host directory is served by its own virtiofsd backend running on
//! a dedicated thread; QEMU's vhost-user-fs-pci frontend connects to the
//! backend's listening unix socket and the guest mounts the matching tag at the
//! same absolute path. The OHOS sandbox forbids spawning external processes, so
//! the daemon is driven through the `virtiofsd` crate as a library (no CLI, no
//! seccomp). OHOS-only (depends on the vhost-user stack).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use log::*;
use vhost::vhost_user::Listener;
use vhost_user_backend::VhostUserDaemon;
use virtiofsd::filesystem::{FileSystem, SerializableFileSystem};
use virtiofsd::passthrough::{CachePolicy, Config, PassthroughFs};
use virtiofsd::vhost_user::VhostUserFsBackendBuilder;
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

use crate::{
    FS_SOCKET_DATA, FS_SOCKET_SANDBOX, FS_WORK_PREFIX, MOUNT_TAG_DATA, MOUNT_TAG_SANDBOX, QemuPaths,
};

/// Error type for the backend threads: the vhost-user / virtiofsd error enums
/// only implement `Display` (not `std::error::Error`), so they are boxed as
/// strings here.
type Result<T> = std::result::Result<T, String>;

/// Starts the virtio-fs backends for the static mounts (host `files/`, tag
/// "sandbox", plus the customer data root, tag "customer_data", when it lies
/// outside the sandbox) before QEMU, so each listening socket exists when its
/// vhost-user-fs-pci chardev connects.
pub fn start(paths: &QemuPaths) {
    let port_dir = paths.port_dir.clone();
    let sandbox_sock = port_dir.join(FS_SOCKET_SANDBOX);
    spawn_backend(MOUNT_TAG_SANDBOX.to_string(), sandbox_sock, paths.sandbox_mount.clone());
    if let Some(data_root) = paths.data_mount.clone() {
        let data_sock = port_dir.join(FS_SOCKET_DATA);
        spawn_backend(MOUNT_TAG_DATA.to_string(), data_sock, data_root);
    }
}

/// Starts one virtiofsd backend for a dynamically mounted work directory.
/// Returns the socket path for the QMP chardev-add, or an error when the
/// backend thread could not be spawned (so the caller never points QEMU at a
/// socket nobody will ever serve).
pub fn spawn_workdir(port_dir: &Path, sequence: u64, shared_dir: PathBuf) -> std::io::Result<PathBuf> {
    let socket = port_dir.join(format!("{FS_WORK_PREFIX}{sequence}.sock"));
    let tag = workdir_tag(sequence);
    if !spawn_backend(tag, socket.clone(), shared_dir) {
        return Err(std::io::Error::other(format!(
            "failed to spawn virtiofsd backend thread for work{sequence}"
        )));
    }
    Ok(socket)
}

/// virtio-fs mount tag for a dynamically mounted work directory.
pub fn workdir_tag(sequence: u64) -> String {
    format!("work{sequence}")
}

/// Spawns one writable virtiofsd backend thread for a single shared directory.
/// Returns `false` when the thread could not be spawned (already logged).
fn spawn_backend(tag: String, socket_path: PathBuf, shared_dir: PathBuf) -> bool {
    let spawned = thread::Builder::new()
        .name(format!("virtiofsd-{tag}"))
        .spawn(move || {
            if let Err(err) = run_backend(&tag, &socket_path, &shared_dir) {
                error!("virtiofs-{tag}: backend failed: {err}");
            }
        });
    match spawned {
        Ok(_) => true,
        Err(err) => {
            // The tag string moved into the (failed) closure; the thread name
            // in the builder already carries the same identifier.
            error!("virtiofs: spawn backend thread: {err}");
            false
        }
    }
}

/// Runs one writable virtiofsd backend until the vhost-user client (QEMU)
/// disconnects.
fn run_backend(tag: &str, socket_path: &Path, shared_dir: &Path) -> Result<()> {
    let listener = Listener::new(socket_path.to_string_lossy().into_owned(), true)
        .map_err(|e| e.to_string())?;
    let fs_cfg = Config {
        root_dir: shared_dir.to_string_lossy().into_owned(),
        cache_policy: CachePolicy::Auto,
        ..Default::default()
    };
    let fs = PassthroughFs::new(fs_cfg).map_err(|e| e.to_string())?;
    serve(tag, socket_path, listener, fs)
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
    daemon.start(&mut listener).map_err(|e| e.to_string())?;
    daemon.wait().map_err(|e| e.to_string())?;
    Ok(())
}
