//! Stable interface for remote command execution, decoupling `util` (and the
//! zcoder workspace) from the concrete openeuler cmd-agent. The implementor
//! (`cmd-agent`'s `Client`) is registered at startup by launch-zed, which
//! converts the backend-agnostic `command_executor::ExecSpec` into the
//! openeuler wire `ExecSpec`.
//!
//! The zcoder workspace depends on this crate (via `util`) instead of
//! cmd-agent directly, so changes to the client's internals do not force a
//! rebuild of every crate that depends on `util`.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use cmd_agent_protocol::{ExecSpec as WireExecSpec, FdMode as WireFdMode, Signal as WireSignal};
use smol::io::{AsyncRead, AsyncWrite};

/// A spawned remote process: session id plus the three stdio streams. The
/// streams mirror `cmd_agent::client::Session`, so the implementor can
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
/// why this exists. Implemented by `cmd-agent`'s `Client`.
pub trait RemoteCommandExecutor: Send + Sync {
    fn spawn(&self, spec: WireExecSpec) -> io::Result<RemoteChild>;
    fn signal(&self, session_id: u64, signal: WireSignal) -> io::Result<()>;
    fn try_exit(&self, session_id: u64) -> Option<Option<i32>>;
    fn wait_exit_async(&self, session_id: u64) -> ExitFuture<'_>;
}

/// Adapts the backend-agnostic `command_executor::RemoteCommandExecutor`
/// interface to this crate's wire protocol. Registered (wrapped) at startup.
struct OpenEulerExecutorWrapper {
    inner: Arc<dyn RemoteCommandExecutor>,
}

fn to_wire_fd(mode: command_executor::FdMode) -> WireFdMode {
    match mode {
        command_executor::FdMode::Piped => WireFdMode::Piped,
        command_executor::FdMode::Null => WireFdMode::Null,
    }
}

fn to_wire_signal(signal: command_executor::Signal) -> WireSignal {
    match signal {
        command_executor::Signal::SigInterrupt => WireSignal::SigInterrupt,
        command_executor::Signal::SigTerm => WireSignal::SigTerm,
        command_executor::Signal::SigKill => WireSignal::SigKill,
    }
}

impl command_executor::RemoteCommandExecutor for OpenEulerExecutorWrapper {
    fn spawn(&self, spec: command_executor::ExecSpec) -> io::Result<command_executor::RemoteChild> {
        let mut wire = WireExecSpec::new(spec.binary.clone());
        wire.source_program = spec.source_program;
        wire.args = spec.args;
        wire.cwd_path = spec.cwd_path;
        wire.env = spec.env;
        wire.stdin = spec.stdin;
        wire.stdin_mode = to_wire_fd(spec.stdin_mode);
        wire.stdout_mode = to_wire_fd(spec.stdout_mode);
        wire.stderr_mode = to_wire_fd(spec.stderr_mode);
        let child = self.inner.spawn(wire)?;
        Ok(command_executor::RemoteChild {
            session_id: child.session_id,
            stdin: child.stdin,
            stdout: child.stdout,
            stderr: child.stderr,
        })
    }

    fn signal(&self, session_id: u64, signal: command_executor::Signal) -> io::Result<()> {
        self.inner.signal(session_id, to_wire_signal(signal))
    }

    fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        self.inner.try_exit(session_id)
    }

    fn wait_exit_async(&self, session_id: u64) -> command_executor::ExitFuture<'_> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.wait_exit_async(session_id).await })
    }
}

/// Registers the process-wide remote command executor. Called once at startup
/// by launch-zed after the openeuler cmd-agent client is ready.
pub fn init_executor(executor: Arc<dyn RemoteCommandExecutor>) -> Result<(), ()> {
    log::info!("cmd_agent_linker::init_executor: registering remote command executor");
    command_executor::init_executor(Arc::new(OpenEulerExecutorWrapper { inner: executor }))
        .map_err(|_| ())
}

/// Returns the registered executor, or None if not initialized yet.
pub fn executor() -> Option<Arc<dyn command_executor::RemoteCommandExecutor>> {
    command_executor::executor()
}
