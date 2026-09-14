//! Command execution for the daemon's SSH exec channels.
//!
//! Every exec request runs `sh -c <command>` as its own process-group leader
//! (`process_group(0)`, so pid == pgid). A global in-memory session table maps
//! each client-chosen session id to the group leader pid, keyed by the client
//! identity it belongs to; `peers` mirrors those groups so a client that goes
//! away takes its whole tree with it. Signaling a running group goes through
//! the reserved command `SIGNAL_PREFIX <session_id> <signal>`, intercepted here
//! rather than spawned. Stdout is bridged to the channel's Data stream and
//! stderr to ExtendedData; the exit status is reported over exit-status, and a
//! signal-killed child over exit-signal.
//!
//! The stdio bridge keeps BOTH directions open until the child actually exits:
//! a long-lived LSP server (clangd) reads its stdin and writes its stdout for
//! the whole session, so the SSH channel must not close either direction on a
//! transient condition. Only `child.wait()` completion ends the bridge.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::{Arc, LazyLock, Mutex};

use russh::server::Handle;
use russh::{ChannelId, CryptoVec};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::protocol;

/// Bytes read from the child's stdout/stderr per select iteration.
const IO_CHUNK_SIZE: usize = 8192;
/// Pause after a transient EOF so a truly-closed pipe does not busy-loop the
/// select, while still keeping the direction polled for an LSP that resumes.
const SLEEP_AFTER_EOF: std::time::Duration = std::time::Duration::from_millis(10);
/// Exit status used when a child dies without a status (not signaled, e.g. the
/// channel closed early) -- maps to util::command's None -> 128 convention.
const EXIT_UNKNOWN: u32 = 128;
/// Exit status reported when spawning the shell itself fails.
const EXIT_SPAWN_FAILED: u32 = 127;
/// Exit status reported when a signal targets an unknown session.
const EXIT_SIGNAL_UNKNOWN_SESSION: u32 = 1;
/// How many chars of the shell command to include in a log line.
const CMD_LOG_PREVIEW_CHARS: usize = 300;
/// (client identity, session id) -> process group leader pid of the running
/// command. The identity is part of the key because session ids are chosen by
/// the client and restart from 1 whenever it does: without it a restarted
/// client would address its own earlier entry instead of its new one.
static SESSIONS: LazyLock<Mutex<HashMap<(String, u64), i32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Records a newly spawned command's group leader pid under its session id, and
/// the group under the client instance that started it, so everything one
/// client started can be taken down together when that client goes away (see
/// `peers`).
fn register_session(client_id: &str, session_id: u64, pid: i32) {
    SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert((client_id.to_string(), session_id), pid);
    crate::peers::add_group(client_id, pid);
}

/// Drops a session entry once its command has fully exited.
fn unregister_session(client_id: &str, session_id: u64) {
    let removed = SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&(client_id.to_string(), session_id));
    if let Some(pid) = removed {
        crate::peers::drop_group(client_id, pid);
    }
}

/// Returns the recorded process-group leader pid for a session, if any.
fn session_pgid(client_id: &str, session_id: u64) -> Option<i32> {
    SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&(client_id.to_string(), session_id))
        .copied()
}

/// Per-command stdin handle, kept between handler callbacks so the client's
/// stdin Data packets can be forwarded to the running child.
pub struct ChildHandle {
    pub stdin: ChildStdin,
}

/// Forwards client stdin bytes into the child's stdin pipe.
pub async fn forward_stdin(
    children: &Arc<AsyncMutex<HashMap<ChannelId, ChildHandle>>>,
    channel: ChannelId,
    data: &[u8],
) {
    use tokio::io::AsyncWriteExt;
    let mut map = children.lock().await;
    if let Some(child) = map.get_mut(&channel) {
        if let Err(err) = child.stdin.write_all(data).await {
            log::error!("exec: write child stdin channel={channel} failed: {err}");
        }
    } else {
        log::warn!("exec: no live child for stdin channel={channel}");
    }
}

/// If `command` is a bare `which` invocation, returns the program names it was
/// asked to locate (flags and the `which` keyword itself stripped). Returns
/// `None` for any command that is not a plain `which` call -- a shell pipeline,
/// compound, redirection, or anything else whose exit status would not reflect
/// `which` alone -- so a missing-program message is never fabricated for it.
fn parse_which_programs(command: &str) -> Option<Vec<String>> {
    let trimmed = command.trim();
    if trimmed.contains('|')
        || trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains('>')
        || trimmed.contains('<')
        || trimmed.contains('`')
        || trimmed.contains('$')
        || trimmed.contains('(')
        || trimmed.contains(')')
    {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    if parts.next() != Some("which") {
        return None;
    }
    let programs: Vec<String> = parts
        .filter(|tok| !tok.starts_with('-'))
        .map(|tok| tok.to_string())
        .collect();
    Some(programs)
}

/// Basename of a path-like token (`/a/b/npm` -> `npm`, `npm` -> `npm`).
fn token_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// `true` when the word names the npm program itself: either the `npm`
/// wrapper (any path ending in `/npm`, or the bare word), or npm's real
/// entrypoint `npm-cli.js` (how the managed Node runtime invokes it).
fn is_npm_token(word: &str) -> bool {
    matches!(token_basename(word), "npm" | "npm-cli.js")
}

/// What [`rewrite_npm_para`] produces: the command to run, and the directory an
/// install deposits its packages into. Everything that is not an install
/// carries neither, and nothing is signed afterwards.
pub struct NpmRewrite {
    pub command: String,
    pub install_root: Option<String>,
}

impl NpmRewrite {
    /// The command unchanged, with nothing to sign afterwards.
    fn passthrough(command: &str) -> Self {
        Self {
            command: command.to_string(),
            install_root: None,
        }
    }
}

/// The directory an install deposits into: the `--prefix` value when the client
/// passed one, otherwise the directory the command `cd`s into before running the
/// program (`mkdir -p <dir> && cd <dir> && ...`, the client's prelude). `None`
/// when the command names neither, which leaves that install unsigned.
///
/// `leading` is the index the trailing command starts at, so only the prelude is
/// searched for `cd`, while `--prefix` is recognised anywhere on the line (npm
/// accepts it after the subcommand too).
fn install_root(words: &[Word], leading: usize) -> Option<String> {
    let mut prefix = None;
    for (index, word) in words.iter().enumerate() {
        if word.bare == "--prefix" {
            if let Some(value) = words.get(index + 1) {
                prefix = Some(value.bare.clone());
            }
        } else if let Some(value) = word.bare.strip_prefix("--prefix=") {
            prefix = Some(value.to_string());
        }
    }
    if prefix.is_some() {
        return prefix;
    }
    let mut cwd = None;
    for pair in words[..leading].windows(2) {
        if pair[0].bare == "cd" {
            cwd = Some(pair[1].bare.clone());
        }
    }
    cwd
}

/// Recognizes an npm install invocation and reports the directory it deposits
/// into; the command itself is passed through untouched.
///
/// Recognition runs on unquoted values, because commands arrive in the shape
/// the client's quoter produces: every argument single-quoted, preceded by
/// `exec` and possibly `VAR=value` assignments, and with global options such as
/// `--prefix <dir>` sitting between the program and the subcommand. Two launch
/// forms are handled:
/// - `npm install ...` / `<any/path>/npm install ...` (system npm wrapper), and
/// - `<any/path>/node <any/path>/npm-cli.js install ...` (managed Node runtime).
/// The install subcommand is matched by its npm-recognized spellings
/// (`install`/`i`/`add`/`isntall`). Anything else (pipelines, unrelated
/// commands) passes through unchanged, byte for byte.
///
/// Nothing about the command line is touched: the client's flags are the ones
/// npm sees. That was not always true of the lifecycle scripts -- they were
/// once deferred to keep a `postinstall` from running a native binary it had
/// just unpacked, while it was still unsigned. The preload (see `shim`) signs
/// such a binary as a child process is spawned, so the scripts are left to run
/// when npm runs them, which is the only time some packages do their real work:
/// one whose own dependencies are installed by its root `postinstall` never
/// finishes otherwise.
///
/// The client's bin entries need nothing either -- this filesystem refuses
/// links, and npm builds those entries as links, but the preload stands a real
/// file in for each one the filesystem refuses.
pub fn rewrite_npm_para(command: &str) -> NpmRewrite {
    let words = split_words(command);
    // Require at least: <npm program> <install subcommand>.
    if words.len() < 2 {
        return NpmRewrite::passthrough(command);
    }

    // Only the trailing command of a `&&`/`;`/`|` chain can hold the program:
    // the client's `mkdir -p <cwd> && cd <cwd>` prelude always leads.
    let leading = words
        .iter()
        .rposition(|w| is_separator(&w.bare))
        .map_or(0, |i| i + 1);

    // Skip leading `VAR=value` assignments and the `exec` builtin, both of
    // which the client emits ahead of the program.
    let mut idx = leading;
    while idx < words.len() && is_env_assignment(&words[idx].bare) {
        idx += 1;
    }
    while idx < words.len() && words[idx].bare == "exec" {
        idx += 1;
    }
    if idx >= words.len() {
        return NpmRewrite::passthrough(command);
    }

    // Locate the first operand for either launch form.
    let operand_idx = if is_npm_token(&words[idx].bare) {
        idx + 1
    } else if token_basename(&words[idx].bare) == "node"
        && idx + 1 < words.len()
        && is_npm_token(&words[idx + 1].bare)
    {
        idx + 2
    } else {
        return NpmRewrite::passthrough(command);
    };

    // Step over global options placed between the program and the subcommand.
    let mut sub_idx = operand_idx;
    while sub_idx < words.len() && words[sub_idx].bare.starts_with('-') {
        let option = words[sub_idx].bare.clone();
        sub_idx += 1;
        if !option.contains('=') && takes_value(&option) {
            sub_idx += 1;
        }
    }
    if sub_idx >= words.len()
        || !matches!(
            words[sub_idx].bare.as_str(),
            "install" | "i" | "add" | "isntall"
        )
    {
        return NpmRewrite::passthrough(command);
    }
    NpmRewrite {
        command: command.to_string(),
        install_root: install_root(&words, leading),
    }
}

/// One shell word of a command string: its value with quotes and escapes
/// stripped, which is what the recognizer matches against. The original
/// spelling is followed while splitting only because a word's extent is not
/// known until its closing quote is seen.
struct Word {
    bare: String,
}

/// Splits a command string into shell words, respecting single quotes.
///
/// The client's quoter wraps every argument in single quotes and escapes an
/// embedded quote as `'\''`, with the result that a word whose value contains
/// spaces -- npm hyphen-range specs do -- must not be torn apart. A plain
/// whitespace split cannot tell the two apart, so quoting is tracked here.
fn split_words(command: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut raw = String::new();
    let mut bare = String::new();
    let mut quoted = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                raw.push(c);
            }
            '\\' => {
                // `\'` inside a quoted run is a literal quote, not a delimiter.
                raw.push(c);
                if let Some(next) = chars.next() {
                    raw.push(next);
                    bare.push(next);
                }
            }
            _ if c.is_whitespace() && !quoted => {
                if !raw.is_empty() {
                    words.push(Word {
                        bare: std::mem::take(&mut bare),
                    });
                }
            }
            _ => {
                raw.push(c);
                bare.push(c);
            }
        }
    }
    if !raw.is_empty() {
        words.push(Word { bare });
    }
    words
}

/// `true` for the shell operators that chain one command to the next.
fn is_separator(word: &str) -> bool {
    matches!(word, "&&" | "||" | ";" | "|")
}

/// npm global options that consume the following word as their value.
///
/// Only options known to be spelled without `=` belong here: listing a
/// valueless one would step over the subcommand and hide the rewrite.
fn takes_value(option: &str) -> bool {
    matches!(
        option,
        "--prefix" | "--userconfig" | "--globalconfig" | "--proxy" | "--registry" | "--cache"
    )
}

/// Matches `NAME=value` (npm command lines never quote env names).
fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Spawns `sh -c <command>` in a fresh process group and bridges its stdio to
/// the channel until the child exits. A reserved signal command is intercepted
/// instead of spawned. The bridging loop and exit reporting run to completion
/// in a background task so the exec_request callback returns immediately.
///
/// `client_id` is the identity the connection authenticated as; the command's
/// process group is recorded under it as well, so the whole tree one client
/// started can be taken down together (see `peers`).
pub async fn spawn_command(
    children: Arc<AsyncMutex<HashMap<ChannelId, ChildHandle>>>,
    channel: ChannelId,
    handle: Handle,
    command: &str,
    client_id: &str,
) {
    // Reserved signal command: signal the recorded process group, never spawn.
    if let Some((session_id, signal)) = protocol::parse_signal_command(command) {
        signal_session(channel, &handle, client_id, session_id, signal).await;
        return;
    }

    let (session_id, shell_command) = protocol::split_sid_payload(command);
    // npm install is recognized once here for EVERY caller (see
    // rewrite_npm_para), so an install can name the directory it deposits
    // into, which the signing sweep scans once it exits (see `sign_elf`). The
    // command itself goes out exactly as the client spelled it.
    let rewrite = rewrite_npm_para(&shell_command);
    let shell_command = rewrite.command;
    let install_root = rewrite.install_root;
    // Detect a plain `which` call so we can surface the missing program name(s)
    // on stdout when nothing is found (see the bridging task below).
    let which_programs = parse_which_programs(&shell_command);
    // Preview is captured before shell_command is moved into the Command below,
    // and is read by the start line here and by the exit reports below.
    let cmd_preview: String = shell_command.chars().take(CMD_LOG_PREVIEW_CHARS).collect();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(shell_command);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Make the child its own process-group leader (pid == pgid) so signaling
    // `-pgid` (see SESSIONS) reaches the whole command group.
    cmd.process_group(0);
    // Bin entries need nothing here: npm builds them itself, and the preload
    // (see `shim`) substitutes a real file wherever this filesystem refuses
    // the link.
    crate::shim::apply(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::error!("exec: spawn sh -c failed channel={channel}: {err}");
            let _ = handle.channel_failure(channel).await;
            let _ = handle.exit_status_request(channel, EXIT_SPAWN_FAILED).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
            return;
        }
    };
    let pid = child.id().unwrap_or(0);
    // A command that stays alive -- the language servers and agents this daemon
    // mostly runs -- would otherwise leave no trace of having started. Its
    // `cmd=` text is the same one the exit line carries, so the two pair up.
    log::info!("exec: started pid={pid} cmd={cmd_preview}");

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Some(stdin) = child.stdin.take() {
        children.lock().await.insert(channel, ChildHandle { stdin });
    } else {
        log::error!("exec: no stdin pipe for channel={channel} pid={pid}");
    }

    if session_id != 0 {
        register_session(client_id, session_id, pid as i32);
    }

    let client_id = client_id.to_string();
    tokio::spawn(async move {
        // Bridge both directions until the child exits; never close a
        // direction while the channel is alive (LSP servers stay resident).
        let exit = bridge_until_exit(&mut child, &mut stdout, &mut stderr, &handle, channel).await;
        // An install leaves native binaries unsigned -- npm unpacks tarballs
        // verbatim -- so they are signed here: after the install is done, before
        // its status is reported. Scripts that ran during the install signed
        // what they needed as they went (see `shim`), so a replay is neither
        // needed nor wanted. Blocking work, off the runtime thread, and it
        // never fails the command it follows.
        if let (Some(root), Some(status)) = (install_root, exit) {
            if status.code() == Some(0) {
                let _ = tokio::task::spawn_blocking(move || {
                    crate::sign_elf::sign_tree(&root);
                })
                .await;
            }
        }
        // For a `which` call that found nothing, print an explicit miss line
        // to stdout so callers (e.g. the command panel probing PATH) can see
        // exactly what is absent. `which` prints nothing to stdout on a miss,
        // so this line is the only stdout content in that case.
        if let (Some(programs), Some(status)) = (&which_programs, exit) {
            if status.code() != Some(0) && !programs.is_empty() {
                for prog in programs {
                    let line = format!("which: not found: {prog}\n");
                    let _ = handle.data(channel, CryptoVec::from(line.as_bytes())).await;
                }
            }
        }
        report_exit(exit, &cmd_preview, &handle, channel).await;
        let _ = handle.eof(channel).await;
        let _ = handle.close(channel).await;
        children.lock().await.remove(&channel);
        if session_id != 0 {
            unregister_session(&client_id, session_id);
        }
    });
}

/// Intercepts `SIGNAL_PREFIX <session_id> <signal>`: kills the recorded
/// process group and replies with the command exit status.
async fn signal_session(
    channel: ChannelId,
    handle: &Handle,
    client_id: &str,
    session_id: u64,
    signal: i32,
) {
    let pgid = session_pgid(client_id, session_id);
    match pgid {
        Some(pgid) => {
            // SAFETY: kill on a negative pid targets the process group; the
            // group leader exists and belongs to this server until unregistered.
            let result = unsafe { libc::kill(-pgid, signal) };
            if result == 0 {
                let _ = handle.exit_status_request(channel, 0).await;
            } else {
                log::warn!(
                    "exec: kill session={session_id} pgid={pgid} sig={signal}: {}",
                    std::io::Error::last_os_error()
                );
                let _ = handle.exit_status_request(channel, EXIT_SIGNAL_UNKNOWN_SESSION).await;
            }
        }
        None => {
            log::warn!("exec: signal for unknown session={session_id}");
            let _ = handle.exit_status_request(channel, EXIT_SIGNAL_UNKNOWN_SESSION).await;
        }
    }
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// Reads child stdout/stderr into channel Data/ExtendedData until the child
/// exits. A long-lived LSP writes stdout continuously; we keep reading it and
/// never end on an EOF alone -- only `child.wait()` completes the bridge.
async fn bridge_until_exit(
    child: &mut Child,
    stdout: &mut Option<ChildStdout>,
    stderr: &mut Option<ChildStderr>,
    handle: &Handle,
    channel: ChannelId,
) -> Option<ExitStatus> {
    let mut out_buf = [0u8; IO_CHUNK_SIZE];
    let mut err_buf = [0u8; IO_CHUNK_SIZE];
    loop {
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(st) => {
                        // Drain any remaining stdio after exit before reporting.
                        drain_remaining(stdout, stderr, handle, channel).await;
                        return Some(st);
                    }
                    Err(_) => return None,
                }
            }
            // stdout/stderr stay polled for the whole session: a long-lived LSP
            // may temporarily return EOF then resume, so a transient 0 is NOT a
            // reason to drop the direction. Only child.wait() ends the bridge.
            result = read_chunk(stdout.as_mut(), &mut out_buf), if stdout.is_some() => {
                match result {
                    Ok(0) => {
                        let _ = handle.eof(channel).await;
                        tokio::time::sleep(SLEEP_AFTER_EOF).await;
                    }
                    Ok(n) => {
                        if let Err(err) = handle.data(channel, CryptoVec::from(&out_buf[..n])).await {
                            log::warn!("exec: send stdout channel={channel}: {err:?}");
                        }
                    }
                    Err(err) => {
                        log::warn!("exec: read stdout channel={channel}: {err}");
                    }
                }
            }
            result = read_chunk(stderr.as_mut(), &mut err_buf), if stderr.is_some() => {
                match result {
                    Ok(0) => {
                        let _ = handle.eof(channel).await;
                        tokio::time::sleep(SLEEP_AFTER_EOF).await;
                    }
                    Ok(n) => {
                        if let Err(err) = handle.extended_data(channel, 1, CryptoVec::from(&err_buf[..n])).await {
                            log::warn!("exec: send stderr channel={channel}: {err:?}");
                        }
                    }
                    Err(err) => {
                        log::warn!("exec: read stderr channel={channel}: {err}");
                    }
                }
            }
        }
    }
}

/// Reads one chunk from a child pipe, returning the byte count (0 on EOF).
async fn read_chunk<R: AsyncReadExt + Unpin>(
    reader: Option<&mut R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(r) => r.read(buf).await,
        None => Ok(0),
    }
}

/// Drains any bytes still buffered in the child pipes after it exits, so no
/// output is lost before the exit status is reported.
async fn drain_remaining(
    stdout: &mut Option<ChildStdout>,
    stderr: &mut Option<ChildStderr>,
    handle: &Handle,
    channel: ChannelId,
) {
    use tokio::io::AsyncReadExt as _;
    let mut buf = [0u8; IO_CHUNK_SIZE];
    if let Some(out) = stdout.as_mut() {
        loop {
            match out.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Err(err) = handle.data(channel, CryptoVec::from(&buf[..n])).await {
                        log::warn!("exec: drain stdout channel={channel}: {err:?}");
                        break;
                    }
                }
            }
        }
    }
    if let Some(err) = stderr.as_mut() {
        loop {
            match err.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Err(e2) = handle
                        .extended_data(channel, 1, CryptoVec::from(&buf[..n]))
                        .await
                    {
                        log::warn!("exec: drain stderr channel={channel}: {e2:?}");
                        break;
                    }
                }
            }
        }
    }
}

/// Reports the child's exit status or signal to the client, and writes a line
/// saying how it ended -- a warning when it did not succeed, information when
/// it did. Every line carries the command text, so the start of a command and
/// the line that followed it can be read as a pair.
async fn report_exit(
    exit: Option<ExitStatus>,
    cmd: &str,
    handle: &Handle,
    channel: ChannelId,
) {
    match exit {
        Some(status) => {
            if let Some(code) = status.code() {
                if code != 0 {
                    log::warn!("exec: cmd={cmd} exit={code} failed");
                } else {
                    log::info!("exec: cmd={cmd} exit=0 done");
                }
                let _ = handle.exit_status_request(channel, code as u32).await;
            } else if let Some(signal) = status.signal() {
                let sig = signal_name(signal);
                log::warn!("exec: cmd={cmd} killed by signal={sig:?} failed");
                let _ = handle
                    .exit_signal_request(channel, sig, false, String::new(), String::new())
                    .await;
            } else {
                log::warn!("exec: cmd={cmd} exited without status failed");
                let _ = handle.exit_status_request(channel, EXIT_UNKNOWN).await;
            }
        }
        None => {
            log::error!("exec: cmd={cmd} wait failed");
            let _ = handle.exit_status_request(channel, EXIT_UNKNOWN).await;
        }
    }
}

/// Maps a raw signal number to a russh `Sig` (falling back to a generic term
/// when the number is unknown to russh).
fn signal_name(signal: i32) -> russh::Sig {
    match signal {
        1 => russh::Sig::HUP,
        2 => russh::Sig::INT,
        9 => russh::Sig::KILL,
        15 => russh::Sig::TERM,
        _ => russh::Sig::TERM,
    }
}

#[cfg(test)]
mod tests {
    use super::{rewrite_npm_para, split_words};

    /// The command text the caller would run, which is the input unchanged.
    fn rewritten(command: &str) -> String {
        rewrite_npm_para(command).command
    }

    /// The directory an install is recognized as depositing into, if any.
    fn signed_root(command: &str) -> Option<String> {
        rewrite_npm_para(command).install_root
    }

    /// The shape the client actually emits: a `mkdir`/`cd` prelude, env
    /// assignments, the `exec` builtin, single-quoted arguments, and a
    /// hyphen-range package spec whose value contains spaces.
    #[test]
    fn passes_client_shape_through() {
        let input = concat!(
            "mkdir -p '/w' && cd '/w' && PATH='/usr/bin' exec '/usr/bin/npm'",
            " '--prefix' '/d/codebuddy-code' 'install'",
            " '@tencent-ai/codebuddy-code@0.0.0 - 1.2.3' '--save-exact'",
            " '--cache=/d/node/cache' </dev/null",
        );
        assert_eq!(rewritten(input), input);
        // `--prefix` wins over the prelude's `cd`.
        assert_eq!(signed_root(input).as_deref(), Some("/d/codebuddy-code"));
    }

    /// Managed Node runtime form: `node <npm-cli.js>`, with config options
    /// trailing the operands.
    #[test]
    fn passes_managed_shape_through() {
        let input = concat!(
            "exec '/d/node' '/d/node/lib/node_modules/npm/bin/npm-cli.js'",
            " '--prefix' '/d/agent' 'install' 'pkg'",
            " '--userconfig' '/d/blank_user_npmrc'",
        );
        assert_eq!(rewritten(input), input);
        assert_eq!(signed_root(input).as_deref(), Some("/d/agent"));
    }

    /// Without `--prefix` the root comes from the client's `cd` prelude, which
    /// sits before the trailing command.
    #[test]
    fn fallback_to_cwd() {
        let input = "mkdir -p '/d/agent' && cd '/d/agent' && exec 'npm' 'install' 'pkg'";
        assert_eq!(signed_root(input).as_deref(), Some("/d/agent"));
    }

    /// An install that names no directory reports no root, so nothing is signed.
    #[test]
    fn rootless_no_sign() {
        assert_eq!(signed_root("FOO=1 npm install pkg"), None);
    }

    /// A value that happens to read `install` belongs to its option, not to the
    /// subcommand slot -- so the install is still recognized, and the option's
    /// value is not read as the directory it deposits into.
    #[test]
    fn skips_option_values() {
        let input = "exec '/usr/bin/npm' '--prefix' '/d/install' 'install' 'pkg'";
        assert_eq!(rewritten(input), input);
        assert_eq!(signed_root(input).as_deref(), Some("/d/install"));
    }

    /// The client's flags reach npm exactly as written -- no flag of ours is
    /// added to the line, whatever the client did or did not spell.
    #[test]
    fn keeps_client_flags() {
        for input in [
            concat!(
                "exec '/usr/bin/npm' '--prefix' '/d' 'install'",
                " '--ignore-scripts' 'pkg'",
            ),
            concat!(
                "exec '/usr/bin/npm' '--prefix' '/d' 'install'",
                " '--no-bin-links' 'pkg'",
            ),
        ] {
            assert_eq!(rewritten(input), input);
            assert_eq!(signed_root(input).as_deref(), Some("/d"));
        }
    }

    /// Non-install npm calls and unrelated programs pass through untouched, with
    /// nothing to sign.
    #[test]
    fn leaves_other_commands() {
        for input in [
            "exec '/usr/bin/npm' 'config' 'list' '--json'",
            "exec '/usr/bin/npm' 'view' 'cowsay' 'version'",
            "exec 'git' 'status'",
            "mkdir -p '/w' && cd '/w'",
        ] {
            assert_eq!(rewritten(input), input);
            assert_eq!(signed_root(input), None);
        }
    }

    /// Bare spelling still works, so recognition does not depend on the quoter
    /// being the source of the command string.
    #[test]
    fn recognizes_bare_form() {
        assert_eq!(rewritten("FOO=1 npm install pkg"), "FOO=1 npm install pkg");
        assert_eq!(signed_root("FOO=1 npm install pkg"), None);
    }

    /// A quoted word keeps its internal spaces instead of splitting in two.
    #[test]
    fn keeps_spaced_spec() {
        let words = split_words("'/a' '@pkg@0.0.0 - 1.2.3'");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].bare, "/a");
        assert_eq!(words[1].bare, "@pkg@0.0.0 - 1.2.3");
    }

    /// An escaped quote inside a quoted word stays content, not a delimiter.
    #[test]
    fn keeps_escaped_quote() {
        let words = split_words("'a'\\''b'");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].bare, "a'b");
    }

}
