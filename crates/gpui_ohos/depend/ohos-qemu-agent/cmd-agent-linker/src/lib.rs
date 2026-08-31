//! Stable interface for remote command execution, decoupling `util` from the
//! concrete cmd-agent. The zcoder workspace depends on this crate (via `util`)
//! instead of cmd-agent directly, so changes to the client's internals do not
//! force a rebuild of every crate that depends on `util`. The implementor
//! (cmd-agent) is registered at startup by launch-zed.
//!
//! Uses the QEMU protocol types, so this crate and everything it pulls in has
//! no dependency on the OpenEuler cmd-agent tree.
//!
//! The interface mirrors the subset of the cmd-agent client that
//! `util::command` actually uses: spawn, signal, try_exit, wait_exit.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use qemu_cmd_agent_protocol::messages::{ExecSpec, Signal};
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

/// Remote command execution behind a stable interface. See the module doc for
/// why this exists.
pub trait RemoteCommandExecutor: Send + Sync {
    fn spawn(&self, spec: ExecSpec) -> io::Result<RemoteChild>;
    fn signal(&self, session_id: u64, signal: Signal) -> io::Result<()>;
    fn try_exit(&self, session_id: u64) -> Option<Option<i32>>;
    fn wait_exit_async(&self, session_id: u64) -> ExitFuture<'_>;
}

/// Mount/unmount of a zcoder-opened folder into the QEMU guest. Kept separate
/// from `RemoteCommandExecutor`: mounting is triggered by opening a workspace,
/// not by running a command. The concrete implementor (cmd-agent) performs the
/// QMP fsdev-add / device_add plus the MountFolder2QEMU handshake; callers on
/// the workspace side only see this thin trait.
pub trait FolderMounter: Send + Sync {
    fn mount_folder(&self, path: &str) -> io::Result<()>;
    fn unmount_folder(&self, path: &str) -> io::Result<()>;
}

static EXECUTOR: OnceLock<Arc<dyn RemoteCommandExecutor>> = OnceLock::new();

/// Registers the process-wide remote command executor. Called once at startup
/// by the host (launch-zed) after the cmd-agent executor is ready.
pub fn init_executor(executor: Arc<dyn RemoteCommandExecutor>) -> Result<(), ()> {
    log::info!("qemu_cmd_agent_linker::init_executor: registering remote command executor");
    EXECUTOR.set(executor).map_err(|_| ())
}

/// Returns the registered executor, or None if not initialized yet.
pub fn executor() -> Option<Arc<dyn RemoteCommandExecutor>> {
    EXECUTOR.get().cloned()
}

static MOUNTER: OnceLock<Arc<dyn FolderMounter>> = OnceLock::new();

/// Registers the process-wide folder mounter. Called once at startup by the
/// host (launch-zed) next to `init_executor`.
pub fn init_mounter(mounter: Arc<dyn FolderMounter>) -> Result<(), ()> {
    log::info!("qemu_cmd_agent_linker::init_mounter: registering folder mounter");
    MOUNTER.set(mounter).map_err(|_| ())
}

/// Returns the registered folder mounter, or None if not initialized yet.
pub fn mounter() -> Option<Arc<dyn FolderMounter>> {
    MOUNTER.get().cloned()
}
