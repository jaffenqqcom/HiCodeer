//! Translates an `ExecSpec` into a POSIX sh command string for the guest.
//!
//! The SSH session env is empty, so PATH / LD_LIBRARY_PATH / HOME and the
//! spec's own env are injected as an exec prefix. The cwd is created before
//! cd-ing. A pid prefix records the process-group-leader pid ($$ under `exec`)
//! into a pid file so the host can signal the whole group with
//! `kill -KILL -$(cat <pid_dir>/<session>.pid)`.

use command_executor::{ExecSpec, FdMode};

/// Guest PATH: /tools/bin (prebundled tools), the sandbox languages tree
/// (per-language LSP servers downloaded into the app sandbox), then the
/// standard search path. Mirrors the guest /etc/profile written by ssh-agentd.
const GUEST_PATH: &str =
    "/tools/bin:/sandbox/haps/entry/files/zcoder/languages:/usr/local/bin:/usr/bin:/bin";
/// Guest LD_LIBRARY_PATH: shared libraries shipped in the read-only /tools tree.
const GUEST_LD_LIBRARY_PATH: &str = "/tools/lib64";
/// Persistent home dir inside the mounted sandbox.
const GUEST_HOME: &str = "/sandbox/home";
/// Environment key set for git to suppress "dubious ownership" (virtio-fs
/// passthrough reports a foreign uid/gid for host-owned repos).
const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
const GIT_CONFIG_KEY: &str = "GIT_CONFIG_KEY_0";
const GIT_CONFIG_VALUE: &str = "GIT_CONFIG_VALUE_0";
/// Device app-sandbox root (el2/base), served to the guest at `/sandbox` via
/// the el2/base virtio-fs mount.
const DEVICE_SANDBOX_ROOT: &str = "/data/storage/el2/base";
/// Guest mount point of the device sandbox root.
const GUEST_SANDBOX_ROOT: &str = "/sandbox";

/// Rewrites a device-side absolute path into the guest namespace. Paths under
/// the device sandbox root (/data/storage/el2/base/...) appear at
/// /sandbox/... in the guest (the el2/base virtio-fs mount). Worktree paths
/// under /storage/Users/currentUser/... are mounted at the same path in the
/// guest and are left unchanged, as are /tools/... and bare program names.
fn map_guest_path(path: &str) -> String {
    match path.strip_prefix(DEVICE_SANDBOX_ROOT) {
        Some(rest) => format!("{GUEST_SANDBOX_ROOT}{rest}"),
        None => path.to_string(),
    }
}

/// Rewrites one argv element: a bare device path, or a `--flag=<path>`
/// argument whose value is a device path (mirrors the cmd-agent inline-`=`
/// path-mapping lesson).
fn map_guest_arg(arg: &str) -> String {
    if let Some((key, value)) = arg.split_once('=') {
        if value.starts_with(DEVICE_SANDBOX_ROOT) {
            return format!("{key}={}", map_guest_path(value));
        }
    }
    map_guest_path(arg)
}

/// Builds the shell command string for one exec request.
pub fn build_command(spec: &ExecSpec, pid_dir: &str, session_id: u64) -> String {
    // pid prefix: echo $$ before exec records the pid that `exec` preserves
    // (the process-group leader), so host-side `kill -KILL -<pid>` works.
    // busybox ash runs the builtin `echo` in the current shell process, so a
    // bare `echo $$ > file` redirect steals fd 1 and the exec'd command's
    // stdout (e.g. clangd's LSP replies) would land in the pid file, its
    // stdout pipe EOFs, and the host then shuts stdin down (Transport error).
    // A `exec 8>&1 / exec 1>&8` save/restore does not help: busybox ash's
    // fd-copy semantics inside an `&&` chain still leave fd 1 on the pid file.
    // A pipeline forces BOTH ends (echo, cat) into child processes, so the
    // parent shell's fd 1 is never redirected in the first place.
    let pid_file = format!("{}/{}.pid", pid_dir, session_id);
    let mut parts = vec![format!("echo $$ | cat > {}", sh_quote(&pid_file))];

    // Working directory: created then entered. Missing cwd must not fail the
    // whole command (a stale path would otherwise make git/LSP unusable).
    if let Some(cwd) = &spec.cwd_path {
        if !cwd.is_empty() {
            let guest_cwd = map_guest_path(cwd.as_str());
            parts.push(format!("mkdir -p {}", sh_quote(&guest_cwd)));
            parts.push(format!("cd {}", sh_quote(&guest_cwd)));
        }
    }

    // Environment: SSH sessions start empty, so inject the guest layout and
    // the caller's env as an exec prefix.
    let mut envs: Vec<String> = Vec::new();
    envs.push(format!("PATH={}", sh_quote(GUEST_PATH)));
    envs.push(format!("LD_LIBRARY_PATH={}", sh_quote(GUEST_LD_LIBRARY_PATH)));
    if spec.source_program == "git" {
        envs.push(format!("{GIT_CONFIG_COUNT}=1"));
        envs.push(format!("{GIT_CONFIG_KEY}={}", sh_quote("safe.directory")));
        envs.push(format!("{GIT_CONFIG_VALUE}={}", sh_quote("*")));
    }
    for (key, value) in &spec.env {
        // Skip HOME and PATH from the caller's env: they are forced below to
        // the guest layout (mirrors the rcS environment the qemu-cmd-agent
        // inherits). A stale PATH here would make `which clangd` (and future
        // zcoder-installed tools) unresolvable.
        if key == "HOME" || key == "PATH" {
            continue;
        }
        envs.push(format!("{}={}", key, sh_quote(value)));
    }
    // HOME and PATH are forced last so they override any caller-supplied value.
    envs.push(format!("PATH={}", sh_quote(GUEST_PATH)));
    envs.push(format!("HOME={}", sh_quote(GUEST_HOME)));

    let mut command = vec![sh_quote(&map_guest_path(&spec.binary))];
    command.extend(
        spec.args
            .iter()
            .map(|arg| sh_quote(&map_guest_arg(arg))),
    );
    let mut line = format!("{} exec {}", envs.join(" "), command.join(" "));

    // Fd redirection: /dev/null for Null modes, otherwise the SSH channel.
    match spec.stdin_mode {
        FdMode::Null => line.push_str(" </dev/null"),
        FdMode::Piped => {}
    }
    match spec.stdout_mode {
        FdMode::Null => line.push_str(" >/dev/null"),
        FdMode::Piped => {}
    }
    match spec.stderr_mode {
        FdMode::Null => line.push_str(" 2>/dev/null"),
        FdMode::Piped => {}
    }
    parts.push(line);

    parts.join(" && ")
}

/// Quotes a value for a POSIX shell: single quotes with `'\''` escaping.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
