//! Automatic installation of missing programs on the VM.
//!
//! When a `which <program>` spawn exits non-zero (the program is not
//! installed), the caller is about to use that program. This module queues a
//! background install with the VM's package manager (dnf on OpenEuler) so a
//! later `which` finds it. The install never blocks the caller's `which`.
//!
//! Install requests funnel through a single background worker thread that
//! processes the queue serially: dnf holds an exclusive lock on its package
//! database, so concurrent installs would just block each other and stack up
//! threads. Each program is installed at most once; a failed install is
//! retried on a later `which` miss, up to `MAX_INSTALL_ATTEMPTS`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use log::{error, info, warn};

/// Maximum install attempts per program before automatic install is given up,
/// so an unavailable package (bad name, missing repo) does not spawn a dnf
/// run on every `which` miss.
const MAX_INSTALL_ATTEMPTS: u32 = 3;

/// Programs whose dnf package name differs from the program name. Names not
/// listed here default to the same name (most tools ship under their own
/// name). Mappings are kept minimal; unknown programs are tried by name and
/// failures are logged.
fn package_name(program: &str) -> &str {
    match program {
        "node" => "nodejs",
        _ => program,
    }
}

/// Programs whose install is queued or already succeeded. A program is kept
/// here after success (no reinstall) and removed on failure (so a later
/// `which` miss can retry, bounded by the failure counter).
static REQUESTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Per-program install failure counts, the bound on automatic retries.
static FAILURES: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

/// Pending install requests fed to the single installer worker thread.
static QUEUE: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);

/// Queues a background dnf install for `program` (at most one pending install
/// per program). Returns immediately; the caller's `which` still reports the
/// program missing for this query. The install finishes asynchronously, so a
/// subsequent `which` finds the program once dnf has installed it.
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

/// Runs the dnf install for one program to completion. Blocking by design; the
/// caller runs this on the dedicated installer worker.
fn run_install(program: &str) {
    let pkg = package_name(program);
    // OpenEuler's package manager is dnf. The server may run as a non-root
    // user (deployed over SSH), so use passwordless sudo when not root; if the
    // user lacks sudo rights the install fails and is logged.
    let mut install = if unsafe { libc::getuid() } == 0 {
        Command::new("dnf")
    } else {
        let mut sudo = Command::new("sudo");
        sudo.arg("-n");
        sudo.arg("dnf");
        sudo
    };
    install.args(["install", "-y", pkg]);
    info!("install: installing {program} (pkg={pkg})");
    match install.output() {
        Ok(output) if output.status.success() => {
            info!("install: {program} installed successfully");
            clear_failures(program);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("install: {program} install failed: {}", stderr.trim());
            record_failure(program);
        }
        Err(err) => {
            error!("install: {program} install could not start: {err}");
            record_failure(program);
        }
    }
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
