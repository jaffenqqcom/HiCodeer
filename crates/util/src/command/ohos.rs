//! OHOS remote command execution through the local cmd-agent daemon.
//!
//! HarmonyOS sandbox forbids `exec` of external programs, so binaries like git
//! and rust-analyzer run on the VM. This module mirrors the API of the other
//! platform `Command` wrappers, but `spawn` executes remotely: it hands an
//! `ExecSpec` to the cmd-agent daemon (via the business-side client) and the
//! returned `Child` carries raw byte streams wired to the remote process's
//! stdio. Callers stay unaware that the child lives on another machine.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::{Arc, OnceLock};

use cmd_agent_linker::RemoteCommandExecutor;
use cmd_agent_protocol::{ExecSpec, FdMode, RootMap, Signal};
use smol::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// How a child's standard descriptor is wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stdio {
    /// Connected to the data connection: the business side can read/write it.
    #[default]
    Piped,
    /// The child inherits from the parent descriptor.
    Inherit,
    /// Redirected to `/dev/null`.
    Null,
}

impl Stdio {
    pub fn piped() -> Self {
        Self::Piped
    }

    pub fn inherit() -> Self {
        Self::Inherit
    }

    pub fn null() -> Self {
        Self::Null
    }
}

/// Path-root mapping used to flag absolute-path arguments for VM-side mapping.
static ROOT_MAP: OnceLock<RootMap> = OnceLock::new();

/// Initializes the global cmd-agent client. The host calls this once after
/// spawning the daemon and before any command runs.
pub fn init(_socket_path: &str, root_map: Option<RootMap>) -> io::Result<()> {
    // The executor itself is registered by launch-zed once the daemon is up;
    // this hook verifies the registration and records the path-root mapping
    // used to flag absolute-path arguments for VM-side mapping.
    if cmd_agent_linker::executor().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "cmd-agent executor not registered",
        ));
    }
    if let Some(map) = root_map {
        let _ = ROOT_MAP.set(map);
    }
    log::info!("util::command::init: executor ready");
    Ok(())
}

fn executor() -> io::Result<Arc<dyn RemoteCommandExecutor>> {
    cmd_agent_linker::executor().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cmd-agent executor not initialized",
        )
    })
}

#[derive(Debug)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    envs: BTreeMap<OsString, Option<OsString>>,
    env_clear: bool,
    current_dir: Option<PathBuf>,
    stdin_cfg: Stdio,
    stdout_cfg: Stdio,
    stderr_cfg: Stdio,
    kill_on_drop: bool,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: BTreeMap::new(),
            env_clear: false,
            current_dir: None,
            stdin_cfg: Stdio::default(),
            stdout_cfg: Stdio::default(),
            stderr_cfg: Stdio::default(),
            kill_on_drop: false,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(|arg| arg.as_os_str())
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .insert(key.as_ref().to_owned(), Some(val.as_ref().to_owned()));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, val) in vars {
            self.envs
                .insert(key.as_ref().to_owned(), Some(val.as_ref().to_owned()));
        }
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_owned();
        if self.env_clear {
            self.envs.remove(&key);
        } else {
            self.envs.insert(key, None);
        }
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.envs.clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    pub fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.stdin_cfg = cfg;
        self
    }

    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.stdout_cfg = cfg;
        self
    }

    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.stderr_cfg = cfg;
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    pub fn get_program(&self) -> &OsStr {
        self.program.as_os_str()
    }

    /// Spawns the remote process. Blocking: the handshake with the daemon
    /// (plus the SpawnOk wait) completes here, so the returned `Child` is
    /// immediately usable.
    pub fn spawn(&mut self) -> io::Result<Child> {
        log::info!(
            "util::command::spawn: program={:?}, args={:?}, cwd={:?}",
            self.program,
            self.args,
            self.current_dir
        );
        let executor = executor()?;
        let child = executor.spawn(self.build_spec())?;
        log::info!("util::command::spawn: session_id={} started", child.session_id);
        Ok(Child {
            stdin: child.stdin,
            stdout: child.stdout,
            stderr: child.stderr,
            session_id: child.session_id,
            kill_on_drop: self.kill_on_drop,
            executor,
        })
    }

    pub async fn output(&mut self) -> io::Result<Output> {
        self.spawn()?.output().await
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn()?;
        child.status().await
    }

    fn build_spec(&self) -> ExecSpec {
        let program = self.program.to_string_lossy().into_owned();
        let mut spec = ExecSpec::new(program.clone());
        spec.source_program = program;
        spec.args = self
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        spec.cwd_path = self
            .current_dir
            .as_ref()
            .map(|dir| dir.to_string_lossy().into_owned());
        // Remote commands inherit the VM's default environment; only explicit
        // overrides travel in the spec. `env_remove` has no VM-side deletion
        // mechanism, so it is approximated by leaving the variable inherited.
        if !self.env_clear {
            for (key, maybe_val) in &self.envs {
                if let Some(val) = maybe_val {
                    spec.env.insert(
                        key.to_string_lossy().into_owned(),
                        val.to_string_lossy().into_owned(),
                    );
                }
            }
        }
        spec.stdin_mode = fd_mode(self.stdin_cfg);
        spec.stdout_mode = fd_mode(self.stdout_cfg);
        spec.stderr_mode = fd_mode(self.stderr_cfg);
        // Flag absolute-path arguments that live under the OHOS root so the
        // server maps them to the VM root.
        if let Some(root) = ROOT_MAP.get() {
            for (index, arg) in self.args.iter().enumerate() {
                let value = arg.to_string_lossy();
                if let Some(rest) = value.strip_prefix(&root.ohos_root) {
                    if rest.is_empty() || rest.starts_with('/') {
                        spec.path_arg_indices.push(index);
                    }
                }
            }
        }
        spec
    }
}

fn fd_mode(stdio: Stdio) -> FdMode {
    match stdio {
        Stdio::Piped => FdMode::Piped,
        // Inherit has no meaning for a remote child (the VM has no caller
        // terminal), so it maps to /dev/null like Null.
        Stdio::Inherit | Stdio::Null => FdMode::Null,
    }
}

pub struct Child {
    pub stdin: Option<Box<dyn AsyncWrite + Unpin + Send>>,
    pub stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
    session_id: u64,
    kill_on_drop: bool,
    executor: Arc<dyn RemoteCommandExecutor>,
}

// Trait objects (Box<dyn AsyncWrite/AsyncRead>) do not implement Debug, so a
// hand-written impl is required instead of a derived one. Only the scalar
// fields are printed; the underlying streams are intentionally omitted.
impl std::fmt::Debug for Child {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Child")
            .field("session_id", &self.session_id)
            .field("kill_on_drop", &self.kill_on_drop)
            .finish()
    }
}

impl Child {
    pub fn id(&self) -> u32 {
        self.session_id as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.executor.signal(self.session_id, Signal::SigKill)
    }

    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(self.executor.try_exit(self.session_id).map(status_from_code))
    }

    pub fn status(
        &mut self,
    ) -> impl std::future::Future<Output = io::Result<ExitStatus>> + Send + 'static {
        // Clone the owned pieces so the returned future does not borrow `self`.
        // This mirrors the darwin implementation's contract (impl Future +
        // Send + 'static): callers can move `process` into a spawned task while
        // this status future runs in another one.
        let executor = self.executor.clone();
        let session_id = self.session_id;
        async move {
            log::info!("util::command::Child::status: session_id={session_id} waiting for exit");
            let exit_code = executor.wait_exit_async(session_id).await?;
            log::info!("util::command::Child::status: session_id={session_id} exit_code={exit_code:?}");
            Ok(status_from_code(exit_code))
        }
    }

    pub async fn output(mut self) -> io::Result<Output> {
        log::info!("util::command::Child::output: session_id={} reading output", self.session_id);
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        if let Some(mut stdout) = self.stdout.take() {
            stdout.read_to_end(&mut stdout_buf).await?;
        }
        // [diag] distinguish a stuck stdout read from a stuck wait_exit.
        log::info!(
            "[diag] Child::output: session_id={} stdout read done, stdout_bytes={}",
            self.session_id,
            stdout_buf.len()
        );
        if let Some(mut stderr) = self.stderr.take() {
            stderr.read_to_end(&mut stderr_buf).await?;
        }
        let exit_code = self.executor.wait_exit_async(self.session_id).await?;
        log::info!(
            "util::command::Child::output: session_id={} exit_code={exit_code:?}, stdout_bytes={}, stderr_bytes={}",
            self.session_id,
            stdout_buf.len(),
            stderr_buf.len()
        );
        Ok(Output {
            status: status_from_code(exit_code),
            stdout: stdout_buf,
            stderr: stderr_buf,
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            // Best-effort: killing an already-exited process group is a no-op
            // on the server.
            let _ = self.executor.signal(self.session_id, Signal::SigKill);
        }
    }
}

fn status_from_code(exit_code: Option<i32>) -> ExitStatus {
    match exit_code {
        Some(code) => ExitStatus::from_raw(code << 8),
        // Terminated by a signal; approximate with a non-successful status.
        None => ExitStatus::from_raw(128 << 8),
    }
}
