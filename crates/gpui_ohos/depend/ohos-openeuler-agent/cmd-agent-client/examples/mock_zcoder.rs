//! mock_zcoder: a test harness that plays the business side (zcoder) against
//! the cmd-agent daemon. It runs the full verification sequence in one
//! process while keeping the management connection alive: git exec, service,
//! large output, stdin injection, multi-command parallelism, and signal-based
//! cleanup. Each step prints PASS/FAIL; the daemon exits when this process
//! ends (management connection close).
//!
//! Usage:
//!   mock_zcoder [--socket PATH] --repo DIR [--stdin-size BYTES]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cmd_agent_client::error::{Error, Result};
use cmd_agent_protocol::{ClientMessage, ExecSpec, PROTOCOL_VERSION, ServerMessage, Signal, frame};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::lock::Mutex;
use smol::net::unix::UnixStream;

const DEFAULT_SOCKET: &str = "/tmp/cmd-agent.sock";
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Connection handle: the management socket plus a shared exit-result table
/// populated by a background reader.
#[derive(Clone)]
struct Ctx {
    mgmt: Arc<Mutex<UnixStream>>,
    exits: Arc<Mutex<HashMap<u64, (Option<i32>, bool)>>>,
}

/// Opens the management connection and starts the reader task.
async fn connect_manage(socket: &str) -> Result<Ctx> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(Error::from)?;
    frame::write_message(&mut stream, &ClientMessage::Manage).await?;
    let exits: Arc<Mutex<HashMap<u64, (Option<i32>, bool)>>> = Arc::new(Mutex::new(HashMap::new()));
    let reader_exits = exits.clone();
    let reader = stream.clone();
    smol::spawn(async move {
        let mut reader = reader;
        loop {
            match frame::read_message::<_, ServerMessage>(&mut reader).await {
                Ok(ServerMessage::ExecResult {
                    session_id,
                    exit_code,
                    timed_out,
                }) => {
                    reader_exits.lock().await.insert(session_id, (exit_code, timed_out));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
    .detach();
    Ok(Ctx {
        mgmt: Arc::new(Mutex::new(stream)),
        exits,
    })
}

/// Waits for a session's exit result, polling the shared table.
async fn wait_exit(ctx: &Ctx, session_id: u64) -> Result<(Option<i32>, bool)> {
    let deadline = std::time::Instant::now() + EXIT_TIMEOUT;
    loop {
        if let Some(result) = ctx.exits.lock().await.get(&session_id).copied() {
            return Ok(result);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::message(format!(
                "session {session_id} exit result timed out"
            )));
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
}

/// Opens a data connection, handshakes, and spawns the child; returns the
/// raw-byte stream once SpawnOk arrives.
async fn open_data(socket: &str, session_id: u64, spec: ExecSpec) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)
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
    Ok(stream)
}

/// Runs one exec against the shared management connection: spawn, inject
/// stdin, read stdout until EOF, and await the exit code.
async fn exec(ctx: &Ctx, socket: &str, spec: ExecSpec) -> Result<(Vec<u8>, Option<i32>, bool)> {
    let session_id = next_session_id();
    let mut stream = open_data(socket, session_id, spec.clone()).await?;
    if !spec.stdin.is_empty() {
        stream.write_all(&spec.stdin).await?;
        stream.flush().await?;
    }
    let _ = stream.shutdown(smol::net::Shutdown::Write);

    let mut stdout = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => stdout.extend_from_slice(&buf[..n]),
            Err(err) => return Err(Error::from(err)),
        }
    }
    let (code, timed_out) = wait_exit(ctx, session_id).await?;
    Ok((stdout, code, timed_out))
}

/// Builds a git command in the exact shape zcoder emits (unified prefix).
fn git_spec(repo: &str, subcommand: Vec<&str>) -> ExecSpec {
    let mut args = vec![
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "--no-optional-locks".to_string(),
        "--no-pager".to_string(),
    ];
    args.extend(subcommand.iter().map(|s| s.to_string()));
    ExecSpec {
        source_program: "git".to_string(),
        binary: "git".to_string(),
        args,
        path_arg_indices: Vec::new(),
        cwd_path: Some(repo.to_string()),
        env: HashMap::new(),
        stdin: Vec::new(),
        timeout_ms: None,
    }
}

async fn step(name: &str, fut: impl std::future::Future<Output = Result<()>>) {
    match fut.await {
        Ok(()) => println!("[PASS] {name}"),
        Err(err) => println!("[FAIL] {name}: {err}"),
    }
}

async fn test_git(ctx: &Ctx, socket: &str, repo: &str) -> Result<()> {
    let spec = git_spec(repo, vec![
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--no-renames",
        "-z",
        "--",
    ]);
    let (stdout, code, timed_out) = exec(ctx, socket, spec).await?;
    if code != Some(0) {
        return Err(Error::message(format!(
            "git status exit_code={code:?} timed_out={timed_out}"
        )));
    }
    println!(
        "  git status: {} bytes, exit_code={code:?}",
        stdout.len()
    );
    if !stdout.is_empty() {
        println!("  {}", String::from_utf8_lossy(&stdout).trim_end());
    }
    Ok(())
}

async fn test_cat(ctx: &Ctx, socket: &str) -> Result<()> {
    let session_id = next_session_id();
    let mut stream = open_data(socket, session_id, ExecSpec::new("cat")).await?;
    stream.write_all(b"hello-service\n").await?;
    stream.flush().await?;
    let _ = stream.shutdown(smol::net::Shutdown::Write);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) => return Err(Error::from(err)),
        }
    }
    let (code, _) = wait_exit(ctx, session_id).await?;
    let echoed = String::from_utf8_lossy(&out);
    if code != Some(0) || !echoed.trim().eq("hello-service") {
        return Err(Error::message(format!(
            "cat echoed={echoed:?}, exit_code={code:?}"
        )));
    }
    println!("  cat service echoed back ok, exit_code={code:?}");
    Ok(())
}

async fn test_bigout(ctx: &Ctx, socket: &str, count: usize) -> Result<()> {
    let mut spec = ExecSpec::new("seq");
    spec.args = vec![count.to_string()];
    let (stdout, code, timed_out) = exec(ctx, socket, spec).await?;
    if code != Some(0) || stdout.len() < count * 2 {
        return Err(Error::message(format!(
            "seq output {} bytes, expected ~{} bytes, exit_code={code:?}",
            stdout.len(),
            count * 2
        )));
    }
    println!(
        "  bigout (seq {count}): received {} bytes, exit_code={code:?}, timed_out={timed_out}",
        stdout.len()
    );
    Ok(())
}

async fn test_stdin(ctx: &Ctx, socket: &str, size: usize) -> Result<()> {
    let payload = vec![b'a'; size];
    let mut spec = ExecSpec::new("wc");
    spec.args = vec!["-c".to_string()];
    spec.stdin = payload;
    let (stdout, code, _) = exec(ctx, socket, spec).await?;
    let parsed = String::from_utf8_lossy(&stdout).trim().to_string();
    if code != Some(0) || parsed != size.to_string() {
        return Err(Error::message(format!(
            "wc -c expected {size}, got {parsed:?}, exit_code={code:?}"
        )));
    }
    println!("  stdin inject {size} bytes: wc -c confirmed, exit_code={code:?}");
    Ok(())
}

/// Runs a long-lived service and three git commands concurrently.
async fn test_parallel(ctx: &Ctx, socket: &str, repo: &str, lsp_binary: Option<&str>) -> Result<()> {
    let lsp = lsp_binary.unwrap_or("cat").to_string();
    let lsp_session = next_session_id();
    let mut lsp_stream = open_data(socket, lsp_session, ExecSpec::new(&lsp)).await?;
    println!("  service ({lsp}) session {lsp_session} running");

    let specs = vec![
        git_spec(repo, vec!["status", "--porcelain=v1", "--untracked-files=all", "--no-renames", "-z", "--"]),
        git_spec(repo, vec!["diff"]),
        git_spec(repo, vec!["log", "--format=%H%x00%P%x00%D", "--max-count=5"]),
    ];
    let mut handles = Vec::new();
    for spec in specs {
        let ctx = ctx.clone();
        let socket = socket.to_string();
        handles.push(smol::spawn(async move { exec(&ctx, &socket, spec).await }));
    }
    let mut git_results = Vec::new();
    for handle in handles {
        git_results.push(handle.await);
    }
    for (index, result) in git_results.iter().enumerate() {
        match result {
            Ok((stdout, code, timed_out)) => println!(
                "    git task {index}: {} bytes, exit_code={code:?}, timed_out={timed_out}",
                stdout.len()
            ),
            Err(err) => println!("    git task {index} FAILED: {err}"),
        }
    }
    if git_results.iter().any(|r| r.is_err())
        || git_results.iter().any(|r| matches!(r, Ok((_, Some(code), _)) if *code != 0))
    {
        return Err(Error::message("one or more git tasks failed"));
    }

    // Close the service and verify it ends cleanly.
    let _ = lsp_stream.shutdown(smol::net::Shutdown::Write);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match lsp_stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) => return Err(Error::from(err)),
        }
    }
    let (code, _) = wait_exit(ctx, lsp_session).await?;
    println!(
        "  service ({lsp}) closed, {} bytes, exit_code={code:?}",
        out.len()
    );
    Ok(())
}

/// Spawns a long-running child, signals it over the management connection,
/// and verifies it is reaped.
async fn test_drop(ctx: &Ctx, socket: &str, binary: &str) -> Result<()> {
    let session_id = next_session_id();
    let mut spec = ExecSpec::new(binary);
    spec.args = vec!["60".to_string()];
    let mut stream = open_data(socket, session_id, spec).await?;
    frame::write_message(
        &mut *ctx.mgmt.lock().await,
        &ClientMessage::Signal {
            session_id,
            signal: Signal::SigKill,
        },
    )
    .await?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) => return Err(Error::from(err)),
        }
    }
    let (code, _) = wait_exit(ctx, session_id).await?;
    println!(
        "  dropped child (binary={binary}), exit_code={code:?}, output={} bytes",
        out.len()
    );
    // A signal-killed child reports a non-numeric exit code.
    if code.is_some() {
        return Err(Error::message(format!(
            "expected signal termination (None exit code), got {code:?}"
        )));
    }
    Ok(())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket = DEFAULT_SOCKET.to_string();
    let mut repo: Option<String> = None;
    let mut stdin_size = 512 * 1024usize;
    let mut lsp_binary: Option<String> = None;
    let mut daemon_binary: Option<String> = None;
    let mut vm_addr = "127.0.0.1:4040".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--repo" => {
                repo = args.get(i + 1).cloned();
                i += 2;
            }
            "--stdin-size" => {
                stdin_size = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(stdin_size);
                i += 2;
            }
            "--lsp-binary" => {
                lsp_binary = args.get(i + 1).cloned();
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
    let repo = repo.ok_or_else(|| Error::message("--repo is required"))?;

    // When a daemon binary is given, spawn it as our own child so its parent
    // is this process (mirroring zcoder spawning the daemon). The daemon exits
    // when we do (parent death or management-connection close).
    let daemon_child = if let Some(daemon_binary) = daemon_binary {
        let child = std::process::Command::new(&daemon_binary)
            .arg("--unix-socket")
            .arg(&socket)
            .arg("--vm-addr")
            .arg(&vm_addr)
            .stderr(std::process::Stdio::from(
                std::fs::File::create("/tmp/mock-daemon.log")
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
        let ctx = connect_manage(&socket).await?;
        println!("management connection established");

        step("git status exec", test_git(&ctx, &socket, &repo)).await;
        step("cat service", test_cat(&ctx, &socket)).await;
        step(
            "bigout (seq 1000000)",
            test_bigout(&ctx, &socket, 1_000_000),
        )
        .await;
        step(
            &format!("stdin inject {stdin_size} bytes"),
            test_stdin(&ctx, &socket, stdin_size),
        )
        .await;
        step(
            "parallel (service + 3 git)",
            test_parallel(&ctx, &socket, &repo, lsp_binary.as_deref()),
        )
        .await;
        step("signal kill cleanup", test_drop(&ctx, &socket, "sleep")).await;

        println!("ALL STEPS DONE");
        // Close the management write direction so the daemon sees EOF and
        // shuts down (the background reader task keeps a socket clone alive,
        // so the connection would otherwise stay half-open).
        let _ = ctx.mgmt.lock().await.shutdown(smol::net::Shutdown::Write);
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
