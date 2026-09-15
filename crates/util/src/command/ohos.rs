//! OHOS command execution - hybrid local/remote.
//!
//! HarmonyOS's sandbox forbids `exec` of arbitrary external programs, so most
//! binaries (LSP servers, node, chmod, ...) run through the on-device daemon
//! command server: `spawn` hands an [`ExecSpec`](cmd_client::ExecSpec) to the
//! registered `cmd-client` executor and the returned `Child` carries raw byte
//! streams wired to the remote process's stdio. A remote child starts from the
//! daemon's own environment: none of the caller's variables travel, because the
//! two run under different uids and a caller's value can point inside its
//! private sandbox. Only a local child receives the caller's overrides.
//!
//! Tools found on-device in the private HNP install dir (`/data/app/bin`) are
//! the exception: they are forked on the device itself via `smol::process`, so
//! they no longer depend on the daemon. The install dir is snapshotted once at
//! startup ([`init_local_tools`]); `spawn` routes a command local when its
//! basename is in that set and forwards everything else to the daemon. Because
//! `/data/app/bin` never changes while the process lives, no per-spawn
//! directory read is needed. If the set is empty (no HNP shipped), every
//! command simply runs through the daemon.
//!
//! This module depends only on the self-contained `cmd-client` crate, which
//! carries its own executor contract (`ExecSpec` / `RemoteCommandExecutor`).
//! It never references a concrete agent or the retired `command-executor` crate.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::{Arc, OnceLock};

use cmd_client::{ExecSpec, FdMode, RemoteCommandExecutor, Signal};
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

/// Tools discovered on-device (private HNP install dir), snapshotted once at
/// startup by [`init_local_tools`] and reused for every spawn afterwards.
/// `/data/app/bin` is static for the process lifetime, so the set is built
/// exactly once - no per-command directory read.
static LOCAL_TOOL_NAMES: OnceLock<Vec<String>> = OnceLock::new();

/// Install dir that HarmonyOS mounts private HNP binaries into. Only this
/// (private) location is consulted - public HNP is intentionally NOT a fallback,
/// the tool set is private-only.
const LOCAL_TOOL_BIN_DIRS: &[&str] = &["/data/app/bin"];

/// Scan the private HNP install dir and record every present executable name,
/// so a later [`Command::spawn`] can route it locally without re-reading the
/// directory. Call once at startup (idempotent): the snapshot is taken on the
/// first call and cached. If the dir is missing or unreadable the set stays
/// empty, which means every command is forwarded to the VM - nothing on-device,
/// nothing forced local.
pub fn init_local_tools() {
    LOCAL_TOOL_NAMES.get_or_init(|| {
        let mut names: Vec<String> = Vec::new();
        for dir in LOCAL_TOOL_BIN_DIRS {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if is_executable_file(&path) {
                    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                        if !names.iter().any(|existing| existing == name) {
                            names.push(name.to_string());
                        }
                    }
                }
            }
        }
        names
    });
}

/// Executable mode bits (owner/group/other execute) required on a candidate
/// file before it is treated as runnable.
const EXECUTABLE_MODE_BITS: u32 = 0o111;

/// Names snapshotted as on-device tools, for startup diagnostics.
pub fn local_tool_programs() -> &'static [String] {
    LOCAL_TOOL_NAMES
        .get()
        .map(|names| names.as_slice())
        .unwrap_or(&[])
}

/// True when `program`'s basename was snapshotted as an on-device tool, so a
/// bare name ("git") and an explicit path to the same binary both route local.
fn local_exec_matches(program: &OsStr) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            LOCAL_TOOL_NAMES
                .get()
                .map(|names| names.iter().any(|existing| existing == name))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Resolve `program` (bare name or explicit path) to an executable absolute
/// path on this device. Errors carry a user-readable hint that the matching
/// HNP must be shipped in the HAP.
fn resolve_local_program(program: &OsStr) -> io::Result<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        if is_executable_file(path) {
            return Ok(path.to_path_buf());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "local {} not found at explicit path {}",
                program.to_string_lossy(),
                path.display()
            ),
        ));
    }
    // The process PATH already points at the private HNP bin dir; try it first,
    // then fall back to the well-known private install dir.
    if let Ok(found) = which::which(program) {
        return Ok(found);
    }
    for dir in LOCAL_TOOL_BIN_DIRS {
        let candidate = Path::new(dir).join(program);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{}: requested local execution but no binary found in $PATH or \
             private HNP dir {}. Ship the matching .hnp in module.json5 hnpPackages and \
             rebuild the HAP.",
            program.to_string_lossy(),
            LOCAL_TOOL_BIN_DIRS[0]
        ),
    ))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| {
            metadata.is_file()
                && (metadata.permissions().mode() & EXECUTABLE_MODE_BITS) != 0
        })
        .unwrap_or(false)
}

/// Outcome of resolving one local tool, exposed for startup diagnostics.
pub struct LocalToolStatus {
    pub program: String,
    pub resolved: Option<PathBuf>,
    pub executable: bool,
    pub error: Option<String>,
}

/// Non-fatal diagnostic mirror of [`resolve_local_program`]: never errors, so a
/// startup probe can log a missing tool without taking the process down.
pub fn local_tool_status(program: &str) -> LocalToolStatus {
    match resolve_local_program(OsStr::new(program)) {
        Ok(resolved) => {
            let executable = is_executable_file(&resolved);
            LocalToolStatus {
                program: program.to_string(),
                resolved: Some(resolved),
                executable,
                error: None,
            }
        }
        Err(error) => LocalToolStatus {
            program: program.to_string(),
            resolved: None,
            executable: false,
            error: Some(error.to_string()),
        },
    }
}

/// Initializes the global remote command executor. launch-zed calls this once
/// after it has registered the `cmd-client` executor.
pub fn init(_socket_path: &str) -> io::Result<()> {
    if executor().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "hicodeerd executor not registered",
        ));
    }
    Ok(())
}

fn executor() -> io::Result<Arc<dyn RemoteCommandExecutor>> {
    cmd_client::executor().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "hicodeerd executor not initialized",
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

    /// Records an override for the child. Honoured only when the child runs on
    /// this device; a remote child starts from the daemon's environment instead
    /// (see `build_spec`).
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

    /// Rebuild a routable command from an already-built `std::process::Command`,
    /// copying program, arguments, working directory and the per-variable
    /// environment overrides. `env_clear` is deliberately not recovered: std
    /// exposes no getter for it, so a cleared environment is indistinguishable
    /// from an untouched one, and guessing wrong would silently leak the
    /// caller's environment into the child.
    pub fn from_std(std_command: std::process::Command) -> Self {
        let mut command = Self::new(std_command.get_program());
        command.args(std_command.get_args());
        if let Some(dir) = std_command.get_current_dir() {
            command.current_dir(dir);
        }
        for (key, value) in std_command.get_envs() {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        command
    }

    /// Spawns the command. Tools snapshotted as on-device (git/ssh/curl)
    /// fork on the device itself; everything else is spawned through the VM
    /// executor. Blocking: the remote handshake (plus the SpawnOk wait)
    /// completes here, so the returned `Child` is immediately usable.
    pub fn spawn(&mut self) -> io::Result<Child> {
        if local_exec_matches(&self.program) {
            // A tool that exists on-device is forced local; a missing binary is
            // an error, never a silent fallback to the VM.
            return spawn_local(self);
        }
        let executor = executor()?;
        let child = executor.spawn(self.build_spec())?;
        Ok(Child {
            stdin: child.stdin,
            stdout: child.stdout,
            stderr: child.stderr,
            kind: ChildKind::Remote {
                session_id: child.session_id,
                executor,
            },
            kill_on_drop: self.kill_on_drop,
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
        // A remote command carries no environment: it starts from whatever the
        // daemon itself was launched with. The two run under different uids, so
        // the caller's own environment can name roots that only resolve inside
        // its sandbox, and forwarding those would hand the child a path it
        // cannot open. Explicit overrides are honoured only where the child runs
        // on this device -- see `spawn_local`.
        spec.stdin_mode = fd_mode(self.stdin_cfg);
        spec.stdout_mode = fd_mode(self.stdout_cfg);
        spec.stderr_mode = fd_mode(self.stderr_cfg);
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

/// Apply the env entries a local HNP tool needs to find its own runtime:
/// its bin dir prepended on PATH (so git can spawn ssh / git-upload-pack from
/// the same package) plus git's libexec/templates. The tool's bundled .so
/// files deliberately need no LD_LIBRARY_PATH here: every shipped binary
/// carries the official DT_RUNPATH ($ORIGIN/../lib on executables, $ORIGIN on
/// the libraries), and a stray LD_LIBRARY_PATH would only shadow that RUNPATH
/// (LD_LIBRARY_PATH is searched first) - so none is injected, letting the
/// official $ORIGIN resolution drive library loading. Each entry is applied
/// only when the corresponding directory actually exists, so a build without
/// the HNP never depends on hard-coded absolute paths.
fn apply_local_tool_env(command: &mut smol::process::Command, resolved: &Path) {
    let Ok(real) = std::fs::canonicalize(resolved) else {
        return;
    };
    let Some(bin_dir) = real.parent() else {
        return;
    };
    let Some(root) = bin_dir.parent() else {
        return;
    };
    let tool_name = real.file_name().and_then(|name| name.to_str()).unwrap_or("");

    // PATH: make the tool's own bin dir resolvable to child processes.
    let want_bin = bin_dir.to_string_lossy().into_owned();
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let full_path = if current_path.is_empty() {
        want_bin.clone()
    } else {
        format!("{}:{}", want_bin, current_path.to_string_lossy())
    };
    if full_path != want_bin {
        command.env("PATH", full_path);
    }

    // git needs libexec/git-core (git-remote-http/https) and the init templates.
    if tool_name == "git" {
        let git_core = root.join("libexec").join("git-core");
        if git_core.is_dir() {
            command.env("GIT_EXEC_PATH", &git_core);
        }
        let templates = root.join("share").join("git-core").join("templates");
        if templates.is_dir() {
            command.env("GIT_TEMPLATE_DIR", templates);
        }
    }
}

fn local_stdio(stdio: Stdio) -> smol::process::Stdio {
    match stdio {
        Stdio::Piped => smol::process::Stdio::piped(),
        // A local child genuinely shares the caller's descriptors, so Inherit
        // keeps its real meaning here (the remote path maps it to null instead).
        Stdio::Inherit => smol::process::Stdio::inherit(),
        Stdio::Null => smol::process::Stdio::null(),
    }
}

/// Fork and exec a local HNP tool on this device. Never touches the remote
/// executor; a resolution/spawn failure surfaces as an error. Uses plain
/// `smol::process` (no `pre_exec`), so the OHOS musl signal-reset / close_fds
/// workarounds required by warp's PTY fork path do not apply here.
fn spawn_local(command: &Command) -> io::Result<Child> {
    let exe = resolve_local_program(&command.program)?;
    let mut child_command = smol::process::Command::new(&exe);
    child_command.args(&command.args);
    child_command.stdin(local_stdio(command.stdin_cfg));
    child_command.stdout(local_stdio(command.stdout_cfg));
    child_command.stderr(local_stdio(command.stderr_cfg));
    if let Some(dir) = &command.current_dir {
        child_command.current_dir(dir);
    }
    if command.env_clear {
        child_command.env_clear();
    }
    for (key, value) in &command.envs {
        match value {
            Some(value) => {
                child_command.env(key, value);
            }
            None => {
                child_command.env_remove(key);
            }
        }
    }
    apply_local_tool_env(&mut child_command, &exe);
    child_command.kill_on_drop(command.kill_on_drop);
    let mut child = child_command.spawn().map_err(|error| {
        log::error!(
            "util::command::spawn_local: spawn {} failed: {error} (HNP type 'private' may deny \
             main-process exec on this device; flip to 'public' and reinstall if EACCES)",
            exe.display()
        );
        io::Error::new(error.kind(), format!("spawn {} failed: {error}", exe.display()))
    })?;
    Ok(Child {
        stdin: child
            .stdin
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncWrite + Unpin + Send + Sync>),
        stdout: child
            .stdout
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncRead + Unpin + Send + Sync>),
        stderr: child
            .stderr
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncRead + Unpin + Send + Sync>),
        kind: ChildKind::Local { child },
        kill_on_drop: command.kill_on_drop,
    })
}

/// Where the child actually runs: on the VM through the remote executor, or
/// forked locally on this device (a private HNP tool).
enum ChildKind {
    Remote {
        session_id: u64,
        executor: Arc<dyn RemoteCommandExecutor>,
    },
    Local {
        child: smol::process::Child,
    },
}

// The streams are `Sync` as well as `Send` because `util::process::Child`
// aliases this type on OHOS and its callers require `Send + Sync` (e.g.
// `context_server::Transport`); the concrete streams behind them (smol
// `Async<..>`) already satisfy both.
pub struct Child {
    pub stdin: Option<Box<dyn AsyncWrite + Unpin + Send + Sync>>,
    pub stdout: Option<Box<dyn AsyncRead + Unpin + Send + Sync>>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send + Sync>>,
    kill_on_drop: bool,
    kind: ChildKind,
}

// Trait objects (Box<dyn AsyncWrite/AsyncRead>) do not implement Debug, so a
// hand-written impl is required instead of a derived one. Only scalar fields
// are printed; the underlying streams are intentionally omitted.
impl std::fmt::Debug for Child {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut builder = f.debug_struct("Child");
        builder.field("kill_on_drop", &self.kill_on_drop);
        match &self.kind {
            ChildKind::Remote { session_id, .. } => {
                builder.field("session_id", session_id);
            }
            ChildKind::Local { .. } => {
                builder.field("kind", &"local");
            }
        }
        builder.finish()
    }
}

impl Child {
    pub fn id(&self) -> u32 {
        match &self.kind {
            ChildKind::Remote { session_id, .. } => *session_id as u32,
            ChildKind::Local { child } => child.id(),
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        match &mut self.kind {
            ChildKind::Remote { session_id, executor } => {
                executor.signal(*session_id, Signal::SigKill)
            }
            ChildKind::Local { child } => child.kill(),
        }
    }

    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.kind {
            ChildKind::Remote { session_id, executor } => {
                Ok(executor.try_exit(*session_id).map(status_from_code))
            }
            ChildKind::Local { child } => child.try_status(),
        }
    }

    pub fn status(
        &mut self,
    ) -> impl std::future::Future<Output = io::Result<ExitStatus>> + Send + 'static {
        // Boxed dyn future: the two arms have unrelated concrete future types,
        // and the opaque return type requires a single concrete type, so both
        // are coerced to Pin<Box<dyn Future + Send>>.
        let future: std::pin::Pin<
            Box<dyn std::future::Future<Output = io::Result<ExitStatus>> + Send>,
        > = match &mut self.kind {
            ChildKind::Remote { session_id, executor } => {
                // Clone the owned pieces so the returned future does not borrow
                // `self`, mirroring the desktop (smol) contract (impl Future +
                // Send + 'static): callers can spawn the status future elsewhere.
                let executor = executor.clone();
                let session_id = *session_id;
                Box::pin(async move {
                    let exit_code = executor.wait_exit_async(session_id).await?;
                    Ok(status_from_code(exit_code))
                })
            }
            ChildKind::Local { child } => {
                // async-process's status() future is 'static (internally cloned
                // Arc), so it can be boxed without borrowing `child` afterwards.
                let status = child.status();
                Box::pin(async move { status.await })
            }
        };
        future
    }

    pub async fn output(mut self) -> io::Result<Output> {
        // Close stdin before waiting: `output` consumes the child and can never
        // feed it, so a command that reads stdin must see EOF or it blocks
        // forever. On the remote path the daemon only forwards that EOF once
        // this side drops its stdin end, so keeping it open hangs the child and
        // this future with it.
        self.stdin.take();

        let status = self.status();

        let stdout = self.stdout.take();
        let stdout_future = async move {
            let mut data = Vec::new();
            if let Some(mut stdout) = stdout {
                stdout.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let stderr = self.stderr.take();
        let stderr_future = async move {
            let mut data = Vec::new();
            if let Some(mut stderr) = stderr {
                stderr.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        // Read both streams concurrently: reading them one after the other
        // deadlocks, because the relay blocks on a full stderr pipe while the
        // caller is still waiting for stdout to reach EOF.
        let (stdout_buf, stderr_buf) =
            futures_lite::future::try_zip(stdout_future, stderr_future).await?;
        let status = status.await?;

        Ok(Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        })
    }

    /// Spawn a process from an already-built `std::process::Command`, routing it
    /// exactly like [`Command::spawn`]: an on-device HNP tool forks locally,
    /// everything else is forwarded to the daemon. This is the entry point for
    /// callers that hold a plain std command they did not build through
    /// [`Command`] (the shared `util::process` call sites), so the sandbox
    /// workaround stays in one place instead of being repeated per caller.
    pub fn spawn(
        std_command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> io::Result<Child> {
        let mut command = Command::from_std(std_command);
        command.stdin(stdin).stdout(stdout).stderr(stderr);
        command.spawn()
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            match &mut self.kind {
                ChildKind::Remote { session_id, executor } => {
                    // Best-effort: killing an already-exited process group is a
                    // no-op on the server.
                    let _ = executor.signal(*session_id, Signal::SigKill);
                }
                ChildKind::Local { child } => {
                    let _ = child.kill();
                }
            }
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

/// An interactive shell session on the command backend's pty.
pub use cmd_client::RemotePty as RemoteShell;
// Re-exported so callers that host a remote shell (crates/terminal) can forward
// window resizes from a thread that does not own the session.
pub use cmd_client::ResizeHandle;

/// Opens an interactive shell on the registered command backend - the OHOS
/// the daemon or the QEMU guest daemon, whichever the launch layer selected.
/// The shell starts in `cwd` when that directory exists on the backend.
///
/// Returns `NotFound` when no backend is registered and `Unsupported` when the
/// backend cannot serve a pty, so the terminal can fall back to a local shell
/// instead of failing to open.
pub async fn open_remote_shell(
    cols: u32,
    rows: u32,
    cwd: Option<&str>,
) -> io::Result<RemoteShell> {
    log::info!("util::command::open_remote_shell: cols={cols} rows={rows} cwd={cwd:?}");
    let executor = cmd_client::executor().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no command backend registered")
    })?;
    match executor.open_shell_pty(cols, rows, cwd).await {
        Ok(shell) => {
            log::info!("util::command::open_remote_shell: interactive shell opened");
            Ok(shell)
        }
        Err(err) => {
            log::warn!("util::command::open_remote_shell: backend has no shell: {err}");
            Err(err)
        }
    }
}
