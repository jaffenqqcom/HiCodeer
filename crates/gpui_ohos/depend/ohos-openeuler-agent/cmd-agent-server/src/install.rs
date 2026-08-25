//! Automatic installation of missing programs on the VM.
//!
//! When a `which <program>` spawn exits non-zero (the program is not
//! installed), the caller is about to use that program. This module queues a
//! background install — the system package manager (dnf) first, with a global
//! npm install as a fallback when dnf has no such package — so a later
//! `which` finds it. The install never blocks the caller's `which`.
//!
//! Languages and their language servers are too many to enumerate, so the
//! strategy is general rather than keyed to a fixed list: any program is
//! tried with dnf by name, and only a definitive dnf "No match for argument"
//! triggers the npm fallback (a transient repo/network error must not install
//! an unrelated same-named npm package).
//!
//! Install requests funnel through a single background worker thread that
//! processes the queue serially: dnf holds an exclusive lock on its package
//! database, so concurrent installs would just block each other and stack up
//! threads. Each program is installed at most once; a failed install is
//! retried on a later `which` miss, up to `MAX_INSTALL_ATTEMPTS`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use log::{error, info, warn};

/// Maximum install attempts per program before automatic install is given up,
/// so an unavailable package (bad name, missing repo) does not spawn a dnf
/// run on every `which` miss.
const MAX_INSTALL_ATTEMPTS: u32 = 3;

/// dnf package name for a program; most tools ship under their own name, only
/// a few are renamed (nodejs is packaged as nodejs, npm ships with it).
fn dnf_package<'a>(program: &'a str) -> &'a str {
    match program {
        "node" | "npm" => "nodejs",
        _ => program,
    }
}

/// npm package name for a program. Defaults to the program name itself; only
/// the cases where the npm package ships under a different name than the
/// executable are listed.
fn npm_package<'a>(program: &'a str) -> &'a str {
    match program {
        // vscode-langservers-extracted provides css/json language servers.
        "vscode-css-language-server" | "vscode-json-language-server" => {
            "vscode-langservers-extracted"
        }
        "vtsls" => "@vtsls/language-server",
        "tailwindcss-language-server" => "@tailwindcss/language-server",
        "pyright-langserver" => "pyright",
        _ => program,
    }
}

/// True when dnf reports the package is absent from every enabled repo (its
/// `No match for argument` error, on stdout; `Unable to find a match` on
/// stderr). This is the only condition under which the npm fallback is safe.
fn dnf_reported_no_match(output: &std::process::Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout.contains("No match for argument")
        || stderr.contains("No match for argument")
        || stderr.contains("Unable to find a match")
}

/// Programs whose install is queued or already succeeded. A program is kept
/// here after success (no reinstall) and removed on failure (so a later
/// `which` miss can retry, bounded by the failure counter).
static REQUESTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Per-program install failure counts, the bound on automatic retries.
static FAILURES: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

/// Pending install requests fed to the single installer worker thread.
static QUEUE: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);

/// Queues a background install for `program` (at most one pending install per
/// program). Returns immediately; the caller's `which` still reports the
/// program missing for this query. The install finishes asynchronously, so a
/// subsequent `which` finds the program once it has been installed.
pub fn ensure_program_installed(program: &str) {
    // A queried program may carry a path (e.g. /usr/bin/foo); dnf only knows
    // package names, so keep the basename.
    let program = Path::new(program.trim())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .trim()
        .to_string();
    if program.is_empty() {
        return;
    }
    // Give up on a program that has failed too many times.
    {
        let failures = FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(map) = failures.as_ref() {
            if map.get(&program).copied().unwrap_or(0) >= MAX_INSTALL_ATTEMPTS {
                info!(
                    "install: {program} failed {MAX_INSTALL_ATTEMPTS} times, no longer auto-installing"
                );
                return;
            }
        }
    }
    // Deduplicate: one pending/succeeded install per program.
    {
        let mut requested = REQUESTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let set = requested.get_or_insert_with(HashSet::new);
        if !set.insert(program.clone()) {
            info!("install: {program} already queued or installed, skipping duplicate");
            return;
        }
    }
    // Lazily start the single installer worker that processes requests
    // serially: dnf's package-database lock makes concurrent installs block
    // on each other and stack up threads.
    let sender = {
        let mut queue = QUEUE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match queue.as_ref() {
            Some(tx) => tx.clone(),
            None => {
                let (tx, rx) = std::sync::mpsc::channel::<String>();
                let name = "pkg-installer".to_string();
                match std::thread::Builder::new()
                    .name(name.clone())
                    .spawn(move || install_worker(rx))
                {
                    Ok(_) => {
                        *queue = Some(tx.clone());
                        info!("install: started installer worker {name}");
                        tx
                    }
                    Err(err) => {
                        error!("install: failed to start installer worker {name}: {err}");
                        return;
                    }
                }
            }
        }
    };
    if let Err(err) = sender.send(program.clone()) {
        error!("install: failed to enqueue {program}: {err}");
    }
}

/// Runs installs from the queue to completion. Lives as long as the server.
fn install_worker(rx: std::sync::mpsc::Receiver<String>) {
    while let Ok(program) = rx.recv() {
        run_install(&program);
    }
    info!("install: installer worker stopped");
}

/// Runs the install for one program to completion: dnf first, npm fallback on
/// a definitive dnf "No match". Blocking by design; the caller runs this on
/// the dedicated installer worker.
fn run_install(program: &str) {
    let dnf_output = run_install_command(program, dnf_package(program), "dnf", &["install", "-y"]);
    let Ok(dnf_output) = dnf_output else {
        record_failure(program);
        return;
    };
    if dnf_output.status.success() {
        info!("install: {program} installed successfully (dnf)");
        clear_failures(program);
        return;
    }
    if !dnf_reported_no_match(&dnf_output) {
        record_failure(program);
        return;
    }
    info!("install: {program} not in dnf repos, falling back to npm");
    let npm_output = run_install_command(program, npm_package(program), "npm", &["install", "-g"]);
    let Ok(npm_output) = npm_output else {
        record_failure(program);
        return;
    };
    if npm_output.status.success() {
        info!("install: {program} installed successfully (npm)");
        clear_failures(program);
    } else {
        record_failure(program);
    }
}

/// Runs `[sudo -n] <tool> <args...> <pkg>` and returns its output. The server
/// may run as a non-root user (deployed over SSH), so use passwordless sudo
/// when not root; if the user lacks sudo rights the install fails and is
/// logged. npm global installs land under the system node's prefix, so the
/// installed binary shows up on the same PATH `which` uses.
fn run_install_command(
    program: &str,
    pkg: &str,
    tool: &str,
    args: &[&str],
) -> io::Result<std::process::Output> {
    let mut install = if unsafe { libc::getuid() } == 0 {
        Command::new(tool)
    } else {
        let mut sudo = Command::new("sudo");
        sudo.arg("-n");
        sudo.arg(tool);
        sudo
    };
    install.args(args);
    install.arg(pkg);
    info!("install: installing {program} via {tool} (pkg={pkg})");
    let output = match install.output() {
        Ok(output) => output,
        Err(err) => {
            error!("install: {program} install via {tool} could not start: {err}");
            return Err(err);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("install: {program} install via {tool} failed: {}", stderr.trim());
    }
    Ok(output)
}

/// Marks an install failure: allow a retry (remove from the requested set) and
/// bump the failure counter that bounds automatic retries.
fn record_failure(program: &str) {
    if let Some(set) = REQUESTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        set.remove(program);
    }
    let mut failures = FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = failures.get_or_insert_with(HashMap::new);
    let count = map.entry(program.to_string()).or_insert(0);
    *count += 1;
}

/// Clears the failure counter after a successful install.
fn clear_failures(program: &str) {
    if let Some(map) = FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        map.remove(program);
    }
}
