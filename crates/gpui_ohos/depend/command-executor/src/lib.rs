//! Backend-agnostic remote command execution for OHOS.
//!
//! HarmonyOS forbids `exec` of external programs, so binaries such as git and
//! rust-analyzer run inside a VM (openeuler-agent or qemu-agent). This crate
//! defines the stable [`RemoteCommandExecutor`] trait and [`ExecSpec`] that both
//! backends implement (through a thin wrapper translating to their own wire
//! protocol). `util::command` consumes only this crate, so the two VM backends
//! are interchangeable and can be switched independently at compile time.
//!
//! This crate is a leaf: it depends on neither `util` nor either agent linker,
//! which is what lets `util` and the agent linkers share one executor interface
//! without forming a dependency cycle.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use smol::io::{AsyncRead, AsyncWrite};

/// How one of the child's standard descriptors is wired on the server side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FdMode {
    /// Connected to the data connection: stdin/stdout to the main connection,
    /// stderr to the dedicated stderr connection.
    #[default]
    Piped,
    /// Redirected to `/dev/null`.
    Null,
}

/// Signals that can be delivered to a running child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    SigInterrupt,
    SigTerm,
    SigKill,
}

/// One execution request: spawn a binary with argv and stream its stdio.
///
/// Backend-agnostic: each VM backend translates this into its own wire
/// `ExecSpec` when forwarding to the VM.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// Program that originated the command (e.g. "git", "rust-analyzer").
    pub source_program: String,
    pub binary: String,
    pub args: Vec<String>,
    pub cwd_path: Option<String>,
    pub env: HashMap<String, String>,
    pub stdin: Vec<u8>,
    pub stdin_mode: FdMode,
    pub stdout_mode: FdMode,
    pub stderr_mode: FdMode,
}

impl ExecSpec {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            source_program: String::new(),
            binary: binary.into(),
            args: Vec::new(),
            cwd_path: None,
            env: HashMap::new(),
            stdin: Vec::new(),
            stdin_mode: FdMode::Piped,
            stdout_mode: FdMode::Piped,
            stderr_mode: FdMode::Piped,
        }
    }
}

/// A spawned remote process: session id plus the three stdio streams.
pub struct RemoteChild {
    pub session_id: u64,
    pub stdin: Option<Box<dyn AsyncWrite + Unpin + Send>>,
    pub stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
}

/// Boxed async exit-status future returned by the executor.
pub type ExitFuture<'a> = Pin<Box<dyn Future<Output = io::Result<Option<i32>>> + Send + 'a>>;

/// Remote command execution behind a stable interface. The active VM backend
/// implements this (via a protocol-translating wrapper) and registers itself
/// through [`init_executor`].
pub trait RemoteCommandExecutor: Send + Sync {
    fn spawn(&self, spec: ExecSpec) -> io::Result<RemoteChild>;
    fn signal(&self, session_id: u64, signal: Signal) -> io::Result<()>;
    fn try_exit(&self, session_id: u64) -> Option<Option<i32>>;
    fn wait_exit_async(&self, session_id: u64) -> ExitFuture<'_>;
}

/// Mount/unmount of a zcoder-opened folder into the VM. QEMU-only; openeuler
/// uses folder sync instead, so this is only registered under the qemu backend.
pub trait FolderMounter: Send + Sync {
    fn mount_folder(&self, path: &str) -> io::Result<()>;
    fn unmount_folder(&self, path: &str) -> io::Result<()>;
}

static EXECUTOR: OnceLock<Arc<dyn RemoteCommandExecutor>> = OnceLock::new();
static MOUNTER: OnceLock<Arc<dyn FolderMounter>> = OnceLock::new();

/// Registers the process-wide remote command executor. Called once at startup
/// by the active VM backend (through launch-zed) after it is ready.
pub fn init_executor(executor: Arc<dyn RemoteCommandExecutor>) -> Result<(), ()> {
    log::info!("command_executor: registering remote command executor");
    EXECUTOR.set(executor).map_err(|_| ())
}

/// Returns the registered executor, or `None` if not yet initialized.
pub fn executor() -> Option<Arc<dyn RemoteCommandExecutor>> {
    EXECUTOR.get().cloned()
}

/// Registers the process-wide folder mounter. Called once at startup by the
/// qemu backend next to [`init_executor`].
pub fn init_mounter(mounter: Arc<dyn FolderMounter>) -> Result<(), ()> {
    log::info!("command_executor: registering folder mounter");
    MOUNTER.set(mounter).map_err(|_| ())
}

/// Returns the registered folder mounter, or `None` if not yet initialized.
pub fn mounter() -> Option<Arc<dyn FolderMounter>> {
    MOUNTER.get().cloned()
}
