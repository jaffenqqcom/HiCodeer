//! Backend-agnostic remote command execution contract, self-contained.
//!
//! This crate intentionally carries its own copy of the executor contract
//! (`ExecSpec` / `RemoteChild` / `FdMode` / `Signal` / `RemoteCommandExecutor`)
//! instead of depending on the `command-executor` crate (DESIGN.md section 2:
//! cmd-client is self-contained and depends on no other gpui_ohos crate). The
//! type names and shapes mirror `command-executor` so `util::command::ohos`
//! only has to swap its import source.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;

use smol::io::{AsyncRead, AsyncWrite};

use crate::pty::RemotePty;

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

impl Signal {
    /// POSIX signal number used on the wire (`SIGNAL_PREFIX <sid> <code>`).
    pub fn code(self) -> i32 {
        match self {
            Signal::SigInterrupt => 2,
            Signal::SigTerm => 15,
            Signal::SigKill => 9,
        }
    }
}

/// One execution request: spawn a binary with argv and stream its stdio.
///
/// Paths are passed through verbatim: the daemon runs on the same device as the host application
/// and sees the same filesystem, so no path mapping is applied.
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
///
/// The streams are `Sync` in addition to `Send`: the caller that owns them
/// (`util::command::Child`, aliased by `util::process::Child` on OHOS) is stored
/// in `Send + Sync` containers, and the concrete streams are smol
/// `Async<UnixStream>`, which already satisfy both.
pub struct RemoteChild {
    pub session_id: u64,
    pub stdin: Option<Box<dyn AsyncWrite + Unpin + Send + Sync>>,
    pub stdout: Option<Box<dyn AsyncRead + Unpin + Send + Sync>>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send + Sync>>,
}

/// Boxed async exit-status future returned by the executor.
pub type ExitFuture<'a> = Pin<Box<dyn Future<Output = io::Result<Option<i32>>> + Send + 'a>>;

/// Boxed future resolving to an interactive shell session on the backend pty.
pub type ShellPtyFuture<'a> = Pin<Box<dyn Future<Output = io::Result<RemotePty>> + Send + 'a>>;

/// Remote command execution behind a stable interface. `cmd-client` registers
/// itself through [`super::init_executor`] at startup.
pub trait RemoteCommandExecutor: Send + Sync {
    fn spawn(&self, spec: ExecSpec) -> io::Result<RemoteChild>;
    fn signal(&self, session_id: u64, signal: Signal) -> io::Result<()>;
    fn try_exit(&self, session_id: u64) -> Option<Option<i32>>;
    fn wait_exit_async(&self, session_id: u64) -> ExitFuture<'_>;

    /// Opens an interactive shell on the backend pty, running `program` with
    /// `args` and starting in `cwd` when that directory exists there. The
    /// program and arguments are the caller's: the backend runs exactly what it
    /// is given and substitutes nothing. Backends without interactive-shell
    /// support return `ErrorKind::Unsupported`, so the caller can fall back to a
    /// local shell instead of failing the terminal.
    fn open_shell_pty<'a>(
        &self,
        _cols: u32,
        _rows: u32,
        _cwd: Option<&'a str>,
        _program: &'a str,
        _args: &'a [String],
    ) -> ShellPtyFuture<'a> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "backend does not support interactive shells",
            ))
        })
    }
}
