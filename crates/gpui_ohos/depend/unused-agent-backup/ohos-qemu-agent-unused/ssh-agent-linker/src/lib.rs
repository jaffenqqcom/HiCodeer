//! Stable interface bridging the concrete QEMU SSH executor (`qemu-ssh-agent`'s
//! `SshCommandExecutor`) to the backend-agnostic `command_executor` crate.
//!
//! `util::command` depends only on `command_executor`, so the two VM backends
//! are interchangeable and can be switched independently at compile time. The
//! QEMU backend speaks SSH to the guest (exec via russh) and mounts workspaces
//! over virtio-fs, so this crate additionally exposes a `FolderMounter` facet.
//! launch-zed registers the running executor here at startup.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use smol::io::{AsyncRead, AsyncWrite};

/// A spawned remote process: session id plus the three stdio streams.
pub struct RemoteChild {
    pub session_id: u64,
    pub stdin: Option<Box<dyn AsyncWrite + Unpin + Send>>,
    pub stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
}

/// Boxed async exit-status future returned by the executor.
pub type ExitFuture<'a> = Pin<Box<dyn Future<Output = io::Result<Option<i32>>> + Send + 'a>>;

/// Remote command execution behind a stable interface. `SshCommandExecutor`
/// implements this; launch-zed wraps one before registering it with
/// `command_executor`.
pub trait RemoteCommandExecutor: Send + Sync {
    fn spawn(&self, spec: command_executor::ExecSpec) -> io::Result<RemoteChild>;
    fn signal(&self, session_id: u64, signal: command_executor::Signal) -> io::Result<()>;
    fn try_exit(&self, session_id: u64) -> Option<Option<i32>>;
    fn wait_exit_async(&self, session_id: u64) -> ExitFuture<'_>;
}

/// Mount/unmount of a zcoder-opened folder into the QEMU guest. Kept separate
/// from `RemoteCommandExecutor`: mounting is triggered by opening a workspace,
/// not by running a command. The concrete implementor (`SshCommandExecutor`)
/// hotplugs an in-process virtiofsd vhost-user-fs device via QMP and then
/// `mount -t virtiofs` over SSH; callers on the workspace side only see this
/// thin trait.
pub trait FolderMounter: Send + Sync {
    fn mount_folder(&self, path: &str) -> io::Result<()>;
    fn unmount_folder(&self, path: &str) -> io::Result<()>;
}

/// Adapts the backend-agnostic `command_executor::RemoteCommandExecutor`
/// interface to this crate's `RemoteCommandExecutor`. Registered (wrapped) at
/// startup.
struct QemuExecutorWrapper {
    inner: Arc<dyn RemoteCommandExecutor>,
}

impl command_executor::RemoteCommandExecutor for QemuExecutorWrapper {
    fn spawn(&self, spec: command_executor::ExecSpec) -> io::Result<command_executor::RemoteChild> {
        let child = self.inner.spawn(spec)?;
        Ok(command_executor::RemoteChild {
            session_id: child.session_id,
            stdin: child.stdin,
            stdout: child.stdout,
            stderr: child.stderr,
        })
    }

    fn signal(&self, session_id: u64, signal: command_executor::Signal) -> io::Result<()> {
        self.inner.signal(session_id, signal)
    }

    fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        self.inner.try_exit(session_id)
    }

    fn wait_exit_async(&self, session_id: u64) -> command_executor::ExitFuture<'_> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.wait_exit_async(session_id).await })
    }
}

/// Adapts the backend-agnostic `command_executor::FolderMounter` interface to
/// this crate's `FolderMounter`. Registered (wrapped) at startup.
struct QemuFolderMounterWrapper {
    inner: Arc<dyn FolderMounter>,
}

impl command_executor::FolderMounter for QemuFolderMounterWrapper {
    fn mount_folder(&self, path: &str) -> io::Result<()> {
        self.inner.mount_folder(path)
    }

    fn unmount_folder(&self, path: &str) -> io::Result<()> {
        self.inner.unmount_folder(path)
    }
}

/// Registers the process-wide remote command executor. Called once at startup
/// by launch-zed after the QEMU SSH executor is ready.
pub fn init_executor(executor: Arc<dyn RemoteCommandExecutor>) -> Result<(), ()> {
    log::info!("qemu_ssh_agent_linker::init_executor: registering remote command executor");
    command_executor::init_executor(Arc::new(QemuExecutorWrapper { inner: executor }))
        .map_err(|_| ())
}

/// Returns the registered executor, or None if not initialized yet.
pub fn executor() -> Option<Arc<dyn command_executor::RemoteCommandExecutor>> {
    command_executor::executor()
}

/// Registers the process-wide folder mounter. Called once at startup by
/// launch-zed next to `init_executor`.
pub fn init_mounter(mounter: Arc<dyn FolderMounter>) -> Result<(), ()> {
    log::info!("qemu_ssh_agent_linker::init_mounter: registering folder mounter");
    command_executor::init_mounter(Arc::new(QemuFolderMounterWrapper { inner: mounter }))
        .map_err(|_| ())
}

/// Returns the registered folder mounter, or None if not initialized yet.
pub fn mounter() -> Option<Arc<dyn command_executor::FolderMounter>> {
    command_executor::mounter()
}
