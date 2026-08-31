//! lsp_probe: drives a real language server (rust-analyzer) through the
//! cmd-agent daemon using the standard LSP over-stdio protocol. It performs
//! the full lifecycle: initialize -> initialized -> didOpen -> diagnostics ->
//! shutdown -> exit, and verifies the exit code arrives over the management
//! connection.
//!
//! Usage:
//!   lsp_probe [--socket PATH] [--binary PATH] [--root DIR]
//!
//! The `--binary` defaults to `rust-analyzer` on the PATH of the machine that
//! runs the cmd-agent server (the VM). Ensure it is installed there first.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cmd_agent::error::{Error, Result};
use cmd_agent_protocol::{ClientMessage, ExecSpec, PROTOCOL_VERSION, ServerMessage, frame};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::unix::UnixStream;

const DEFAULT_SOCKET: &str = "/tmp/cmd-agent.sock";
const LSP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Opens the management connection.
async fn connect_manage(socket: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(Error::from)?;
    frame::write_message(&mut stream, &ClientMessage::Manage).await?;
    Ok(stream)
}

/// Waits for a session's exit result on the management connection, matching
/// by session id (other messages are skipped).
async fn wait_exit(mgmt: &mut UnixStream, session_id: u64) -> Result<(Option<i32>, bool)> {
    let deadline = std::time::Instant::now() + LSP_RESPONSE_TIMEOUT;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(Error::message("exit result timed out"));
        }
        let message = frame::read_message::<_, ServerMessage>(mgmt).await?;
        if let ServerMessage::ExecResult {
            session_id: sid,
            exit_code,
            timed_out,
        } = message
        {
            if sid == session_id {
                return Ok((exit_code, timed_out));
            }
        }
    }
}

/// Sends one LSP message with Content-Length framing.
async fn lsp_send(stream: &mut UnixStream, body: &str) -> Result<()> {
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    stream.write_all(frame.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Reads one LSP message: header, Content-Length, body.
async fn lsp_recv(stream: &mut UnixStream) -> Result<serde_json::Value> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await?;
        header.push(byte[0]);
        if header.len() > 2048 {
            return Err(Error::message("LSP header too large"));
        }
    }
    let header_str = String::from_utf8_lossy(&header);
    let content_length = header_str
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .ok_or_else(|| Error::message("LSP message missing content-length"))?;
    let mut body = vec![0u8; content_length];
    stream.read_exact(&mut body).await?;
    let value = serde_json::from_slice(&body)
        .map_err(|err| Error::message(format!("LSP body not json: {err}")))?;
    log::info!(
        "LSP <- {}",
        String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>()
    );
    Ok(value)
}

/// Waits for a response matching `id`.
async fn lsp_wait_response(
    stream: &mut UnixStream,
    id: u64,
    deadline: std::time::Instant,
) -> Result<serde_json::Value> {
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(Error::message(format!("LSP response {id} timed out")));
        }
        let message = smol::future::or(
            async { lsp_recv(stream).await },
            async {
                smol::Timer::after(Duration::from_secs(1)).await;
                Err(Error::message("poll"))
            },
        )
        .await;
        match message {
            Ok(value) if value["id"] == serde_json::json!(id) => return Ok(value),
            Ok(_) => continue,
            Err(err) if err.to_string() == "poll" => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Reads LSP messages until a non-empty `publishDiagnostics` arrives or the
/// deadline passes. Empty diagnostics are treated as the initial publish and
/// skipped, since rust-analyzer first reports an empty set and only later
/// publishes real diagnostics after analysis.
async fn lsp_collect_diagnostics(
    stream: &mut UnixStream,
    deadline: std::time::Instant,
) -> Result<bool> {
    let mut saw_any = false;
    loop {
        if std::time::Instant::now() >= deadline {
            return Ok(saw_any);
        }
        let message = smol::future::or(
            async { lsp_recv(stream).await },
            async {
                smol::Timer::after(Duration::from_secs(1)).await;
                Err(Error::message("poll"))
            },
        )
        .await;
        match message {
            Ok(value) => {
                if value["method"] == serde_json::json!("textDocument/publishDiagnostics") {
                    let diagnostics = &value["params"]["diagnostics"];
                    let count = diagnostics.as_array().map(|d| d.len()).unwrap_or(0);
                    println!("received publishDiagnostics with {count} diagnostics");
                    if count > 0 {
                        println!("  sample: {}", diagnostics.to_string());
                        return Ok(true);
                    }
                    saw_any = true;
                }
            }
            Err(err) if err.to_string() == "poll" => continue,
            Err(err) => return Err(err),
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket = DEFAULT_SOCKET.to_string();
    let mut binary = "rust-analyzer".to_string();
    let mut root = "/tmp".to_string();
    let mut probe_file = "probe.rs".to_string();
    let mut daemon_binary: Option<String> = None;
    let mut vm_addr = "127.0.0.1:4040".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--binary" => {
                binary = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--root" => {
                root = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--probe-file" => {
                probe_file = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--daemon-binary" => {
                daemon_binary = args.get(i + 1).cloned();
                i += 2;
            }
            "--vm-addr" => {
                vm_addr = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let session_id = next_session_id();
    let mut spec = ExecSpec::new(&binary);
    spec.cwd_path = Some(root.clone());
    let root_uri = format!("file://{root}");

    // When a daemon binary is given, spawn it as our own child so its parent
    // is this process (mirroring zcoder spawning the daemon).
    let daemon_child = if let Some(daemon_binary) = daemon_binary {
        let child = std::process::Command::new(&daemon_binary)
            .arg("--unix-socket")
            .arg(&socket)
            .arg("--vm-addr")
            .arg(&vm_addr)
            .stderr(std::process::Stdio::from(
                std::fs::File::create("/tmp/lsp-daemon.log")
                    .map_err(|err| Error::message(format!("creating daemon log: {err}")))?,
            ))
            .spawn()
            .map_err(|err| Error::message(format!("spawning daemon: {err}")))?;
        wait_for_socket(&socket, Duration::from_secs(10))?;
        Some(child)
    } else {
        None
    };

    let result = smol::block_on(async move {
        // Management connection for the exit result.
        let mut mgmt = connect_manage(&socket).await?;

        // Data connection: spawn rust-analyzer.
        let mut stream = UnixStream::connect(&socket)
            .await
            .map_err(Error::from)?;
        frame::write_message(
            &mut stream,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                root_map: None,
            },
        )
        .await?;
        loop {
            match frame::read_message::<_, ServerMessage>(&mut stream).await? {
                ServerMessage::HelloOk { .. } => break,
                ServerMessage::Error { message, .. } => {
                    return Err(Error::message(format!("hello rejected: {message}")))
                }
                _ => continue,
            }
        }
        frame::write_message(&mut stream, &ClientMessage::Spawn { session_id, spec }).await?;
        loop {
            match frame::read_message::<_, ServerMessage>(&mut stream).await? {
                ServerMessage::SpawnOk { session_id: sid } if sid == session_id => break,
                ServerMessage::Error { message, .. } => {
                    return Err(Error::message(format!("spawn rejected: {message}")))
                }
                _ => continue,
            }
        }
        println!("rust-analyzer spawned, session {session_id}");

        // LSP initialize.
        let deadline = std::time::Instant::now() + LSP_RESPONSE_TIMEOUT;
        lsp_send(
            &mut stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": null,
                    "rootUri": root_uri,
                    "capabilities": {},
                }
            })
            .to_string(),
        )
        .await?;
        let init_response = lsp_wait_response(&mut stream, 1, deadline).await?;
        let server_info = init_response["result"]["serverInfo"].clone();
        println!(
            "initialize ok, server_info={}",
            if server_info.is_null() {
                "(none)".to_string()
            } else {
                server_info.to_string()
            }
        );

        // initialized notification.
        lsp_send(
            &mut stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {},
            })
            .to_string(),
        )
        .await?;

        // didOpen + collect diagnostics.
        let source = "fn main() {\n    let x: i32 = \"wrong type\";\n    println!(\"{}\", x);\n}\n";
        lsp_send(
            &mut stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": format!("{root_uri}/{probe_file}"),
                        "languageId": "rust",
                        "version": 1,
                        "text": source,
                    }
                },
            })
            .to_string(),
        )
        .await?;
        println!("didOpen sent, waiting for diagnostics...");
        let diag_deadline = std::time::Instant::now() + Duration::from_secs(30);
        let saw_diagnostics = lsp_collect_diagnostics(&mut stream, diag_deadline).await?;
        if !saw_diagnostics {
            println!("warning: no publishDiagnostics received within timeout");
        }

        // shutdown request.
        lsp_send(
            &mut stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "shutdown",
                "params": null,
            })
            .to_string(),
        )
        .await?;
        let _ = lsp_wait_response(&mut stream, 2, std::time::Instant::now() + LSP_RESPONSE_TIMEOUT)
            .await?;
        println!("shutdown ok");

        // exit notification, then the child exits and the connection EOFs.
        lsp_send(
            &mut stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "exit",
                "params": null,
            })
            .to_string(),
        )
        .await?;
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => println!("leftover output: {n} bytes"),
                Err(err) => return Err(Error::from(err)),
            }
        }
        println!("connection EOF (server exited)");

        // Exit code over the management connection.
        let (exit_code, timed_out) = wait_exit(&mut mgmt, session_id).await?;
        println!("lsp exit_code={exit_code:?}, timed_out={timed_out}");
        // Close the management write direction so the daemon sees EOF and
        // shuts down.
        let _ = mgmt.shutdown(smol::net::Shutdown::Write);
        Ok(())
    });
    if let Some(mut child) = daemon_child {
        // Wait briefly for the daemon to exit on its own, then reap it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait()? {
                Some(_) => break,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
    result
}

/// Polls a unix socket until it accepts connections or the timeout passes.
fn wait_for_socket(socket: &str, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(Error::message(format!(
        "daemon socket {socket} not ready within {}s",
        timeout.as_secs()
    )))
}
