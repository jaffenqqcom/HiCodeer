//! Stable interface for remote command execution, decoupling `util` from the
//! concrete cmd-agent-client. The zcoder workspace depends on this crate
//! (via `util`) instead of cmd-agent-client directly, so changes to the
//! client's internals (daemon, deploy, ssh) do not force a rebuild of every
//! crate that depends on `util`. The implementor (cmd-agent-client) is
//! registered at startup by launch-zed.
//!
//! The interface mirrors the subset of `cmd_agent_client::client::Client`
//! that `util::command` actually uses: spawn, signal, try_exit, wait_exit.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use cmd_agent_protocol::{ExecSpec, Signal};
use smol::io::{AsyncRead, AsyncWrite};

/// A spawned remote process: session id plus the three stdio streams. The
/// streams mirror `cmd_agent_client::client::Session`, so the implementor can
/// map them directly.
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

static EXECUTOR: OnceLock<Arc<dyn RemoteCommandExecutor>> = OnceLock::new();

/// Registers the process-wide remote command executor. Called once at startup
/// by the host (launch-zed) after the cmd-agent daemon is ready.
pub fn init_executor(executor: Arc<dyn RemoteCommandExecutor>) -> Result<(), ()> {
    log::info!("cmd_agent_linker::init_executor: registering remote command executor");
    EXECUTOR.set(executor).map_err(|_| ())
}

/// Returns the registered executor, or None if not initialized yet.
pub fn executor() -> Option<Arc<dyn RemoteCommandExecutor>> {
    EXECUTOR.get().cloned()
}
