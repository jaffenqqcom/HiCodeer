//! Deployment of the cmd-agent server to a remote host over SSH.
//!
//! The binary is transferred with SCP semantics (a streaming `cat > file` over
//! an exec channel) rather than the SFTP subprotocol, then started detached
//! with `setsid nohup`, and finally probed over TCP. Deployment is a low
//! frequency, blocking operation, so it runs on its own tokio runtime instead
//! of the smol-based protocol path.

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Config, Handle, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::ChannelMsg;

use crate::error::{Error, Result, ResultContext};

/// Probe retry budget: how many TCP attempts before declaring failure.
const PROBE_MAX_ATTEMPTS: u32 = 30;
/// Delay between TCP probe attempts in milliseconds.
const PROBE_INTERVAL_MS: u64 = 500;
/// Bytes uploaded per exec channel. Kept below the measured russh single
/// channel window limit (~8 MB) so a chunk never blocks waiting for a window
/// adjustment that russh does not process.
const UPLOAD_CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Timeout for establishing the SSH connection to the remote host.
const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// SSH connection and target layout for a deployment.
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Remote directory where the server binary is placed.
    pub remote_dir: String,
}

/// Accepts any server key. Suited to an internal bridge VM; production
/// deployments should pin the host key instead.
#[derive(Clone)]
struct AcceptAllHandler;

impl Handler for AcceptAllHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

type SshSession = Handle<AcceptAllHandler>;

/// Uploads the server binary to the remote host, starts it detached, and
/// probes the TCP port until the agent answers.
///
/// Blocking: call from a background executor, not the UI thread.
pub fn deploy(cfg: &SshConfig, server_binary: &[u8], listen_port: u16) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(Error::from)
        .with_context(|| "creating tokio runtime".to_string())?;
    rt.block_on(async move {
        let remote_path = format!("{}/cmd-agent-server", cfg.remote_dir);
        let mut session = connect(cfg).await?;

        // Run the deployment steps, then always disconnect explicitly. russh's
        // Handle drop does not send an SSH disconnect on its own, so without
        // this the sshd side keeps `[priv]`/`[net]` processes until the
        // connection times out, and repeated deploys pile them up.
        let outcome = async {
            create_remote_dir(&mut session, &cfg.remote_dir).await?;
            kill_old_server(&mut session).await?;
            upload_binary(&mut session, &remote_path, server_binary).await?;
            start_server(&mut session, &cfg.remote_dir, &remote_path, listen_port).await?;

            log::info!(
                "server uploaded and started, probing {}:{}",
                cfg.host,
                listen_port
            );
            probe(&cfg.host, listen_port).await
        }
        .await;

        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "", "English")
            .await;
        outcome
    })
}

/// Establishes an SSH connection and authenticates with a password.
async fn connect(cfg: &SshConfig) -> Result<SshSession> {
    log::info!("ssh connect to {}:{} as {}", cfg.host, cfg.port, cfg.user);
    let config = Arc::new(Config::default());
    let mut session = tokio::time::timeout(
        SSH_CONNECT_TIMEOUT,
        client::connect(config, (cfg.host.as_str(), cfg.port), AcceptAllHandler),
    )
    .await
    .map_err(|_| {
        Error::message(format!(
            "ssh connect to {}:{} timed out after {}s",
            cfg.host,
            cfg.port,
            SSH_CONNECT_TIMEOUT.as_secs()
        ))
    })?
    .map_err(Error::from)
    .with_context(|| format!("ssh connect to {}:{}", cfg.host, cfg.port))?;

    let authenticated = session
        .authenticate_password(&cfg.user, &cfg.password)
        .await
        .map_err(Error::from)
        .with_context(|| "ssh password authentication".to_string())?;
    if !authenticated.success() {
        return Err(Error::message(format!(
            "ssh authentication rejected for user {}",
            cfg.user
        )));
    }
    log::info!("ssh authenticated");
    Ok(session)
}

/// Creates the remote directory if it does not exist.
async fn create_remote_dir(session: &mut SshSession, dir: &str) -> Result<()> {
    let command = format!("mkdir -p {}", sh_quote(dir));
    run_simple(session, command, "mkdir").await
}

/// Kills any stale cmd-agent-server process by exact name, so the fresh
/// binary can bind the listen port. An absent process (pkill non-zero) is
/// not an error.
async fn kill_old_server(session: &mut SshSession) -> Result<()> {
    let command = "pkill -x cmd-agent-server; true".to_string();
    run_allow_fail(session, command, "kill old server").await
}

/// Runs a command whose non-zero exit status is acceptable, surfacing only
/// channel-level failures.
async fn run_allow_fail(
    session: &mut SshSession,
    command: String,
    what: &'static str,
) -> Result<()> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(Error::from)
        .with_context(|| format!("{what}: opening channel"))?;
    channel
        .exec(true, command)
        .await
        .map_err(Error::from)
        .with_context(|| format!("{what}: exec"))?;
    channel
        .eof()
        .await
        .map_err(Error::from)
        .with_context(|| format!("{what}: eof"))?;
    let _ = wait_exit(&mut channel).await;
    Ok(())
}

/// Streams the binary into the remote file with SCP semantics.
///
/// The file is written in chunks, one exec channel per chunk (`cat >` for the
/// first, `cat >>` to append). russh does not process SSH window-adjust
/// messages, so a single channel stalls once its send window is exhausted;
/// chunking below that limit keeps each channel short-lived.
async fn upload_binary(session: &mut SshSession, remote_path: &str, data: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let chunk_count = data.chunks(UPLOAD_CHUNK_SIZE).count();
    log::info!(
        "uploading {} bytes to {remote_path} in {chunk_count} chunks",
        data.len()
    );

    for (index, chunk) in data.chunks(UPLOAD_CHUNK_SIZE).enumerate() {
        let operator = if index == 0 { ">" } else { ">>" };
        let mut channel = session
            .channel_open_session()
            .await
            .map_err(Error::from)
            .with_context(|| "opening upload channel".to_string())?;
        let command = format!("cat {operator} {}", sh_quote(remote_path));
        channel
            .exec(true, command)
            .await
            .map_err(Error::from)
            .with_context(|| "upload exec".to_string())?;

        let mut writer = channel.make_writer();
        writer
            .write_all(chunk)
            .await
            .map_err(Error::from)
            .with_context(|| "upload data".to_string())?;
        // AsyncWrite::shutdown sends EOF; `cat` then observes EOF and exits.
        writer
            .shutdown()
            .await
            .map_err(Error::from)
            .with_context(|| "upload shutdown".to_string())?;
        drop(writer);

        let status = wait_exit(&mut channel)
            .await
            .with_context(|| "upload exit status".to_string())?;
        if status != 0 {
            return Err(Error::message(format!(
                "upload chunk {} failed, remote exit status {status}",
                index + 1
            )));
        }
        log::info!("upload chunk {}/{} complete", index + 1, chunk_count);
    }

    log::info!("upload complete");
    Ok(())
}

/// Starts the server detached so it survives the SSH session.
async fn start_server(
    session: &mut SshSession,
    remote_dir: &str,
    remote_path: &str,
    listen_port: u16,
) -> Result<()> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(Error::from)
        .with_context(|| "opening start channel".to_string())?;
    let command = format!(
        "chmod +x {path} && setsid nohup {path} --listen 0.0.0.0:{port} > {dir}/server.log 2>&1 &",
        path = sh_quote(remote_path),
        port = listen_port,
        dir = sh_quote(remote_dir)
    );
    // The `&` backgrounded command returns immediately; a non-zero status is
    // still worth surfacing for diagnosis.
    channel
        .exec(true, command)
        .await
        .map_err(Error::from)
        .with_context(|| "start exec".to_string())?;
    let status = wait_exit(&mut channel)
        .await
        .with_context(|| "start exit status".to_string())?;
    if status != 0 {
        return Err(Error::message(format!(
            "start command failed, remote exit status {status}"
        )));
    }
    log::info!("start command accepted");
    Ok(())
}

/// Runs a short command that needs no stdin and waits for its exit status.
async fn run_simple(session: &mut SshSession, command: String, what: &'static str) -> Result<()> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(Error::from)
        .with_context(|| format!("{what}: opening channel"))?;
    channel
        .exec(true, command)
        .await
        .map_err(Error::from)
        .with_context(|| format!("{what}: exec"))?;
    channel
        .eof()
        .await
        .map_err(Error::from)
        .with_context(|| format!("{what}: eof"))?;
    let status = wait_exit(&mut channel)
        .await
        .with_context(|| format!("{what}: exit status"))?;
    if status != 0 {
        return Err(Error::message(format!(
            "{what} failed, remote exit status {status}"
        )));
    }
    Ok(())
}

/// Waits for the channel to report an exit status.
async fn wait_exit(
    channel: &mut russh::Channel<russh::client::Msg>,
) -> Result<u32> {
    loop {
        let Some(message) = channel.wait().await else {
            break;
        };
        match message {
            ChannelMsg::ExitStatus { exit_status } => return Ok(exit_status),
            other => log::debug!(
                "wait_exit: ignoring channel message {}",
                message_kind(&other)
            ),
        }
    }
    Err(Error::message("channel closed without exit status"))
}

/// Short name for a channel message, for debug logging.
fn message_kind(message: &ChannelMsg) -> &'static str {
    match message {
        ChannelMsg::Data { .. } => "data",
        ChannelMsg::ExtendedData { .. } => "extended_data",
        ChannelMsg::Eof => "eof",
        ChannelMsg::Close => "close",
        ChannelMsg::WindowAdjusted { .. } => "window_adjusted",
        ChannelMsg::ExitStatus { .. } => "exit_status",
        ChannelMsg::ExitSignal { .. } => "exit_signal",
        _ => "other",
    }
}

/// Retries a TCP connect until the agent answers.
async fn probe(host: &str, port: u16) -> Result<()> {
    let addr = format!("{host}:{port}");
    for attempt in 0..PROBE_MAX_ATTEMPTS {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            log::info!("cmd-agent server reachable at {addr}");
            return Ok(());
        }
        log::debug!("probe attempt {} failed, retrying", attempt + 1);
        tokio::time::sleep(Duration::from_millis(PROBE_INTERVAL_MS)).await;
    }
    Err(Error::message(format!(
        "cmd-agent server at {addr} not reachable after {PROBE_MAX_ATTEMPTS} attempts"
    )))
}

/// Quotes a path for the remote POSIX shell.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
