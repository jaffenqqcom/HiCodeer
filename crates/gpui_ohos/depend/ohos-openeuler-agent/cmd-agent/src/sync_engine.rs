//! Device-sandbox -> VM file mirror sync engine.
//!
//! A background thread watches the download directories under `data_dir()`
//! (LSP binaries, node runtime, extensions, AI plugins). Only "write complete"
//! events are acted on (close-after-write and rename-into-directory), so a
//! half-written file is never mirrored. Changes are pushed to the VM as
//! FileSync operations through the daemon; deletes are mirrored too.
//!
//! The engine runs as an independent thread inside the host process, spawned
//! by `daemon::spawn_daemon`, and needs no external handle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::client::Client;
use crate::daemon::{FileSyncOp, SharedControl};
use crate::error::{Error, Result, ResultContext};

/// Silence window before a pending batch is synced; coalesces the event storm
/// from bulk installs (npm node_modules etc.).
const SYNC_DEBOUNCE: Duration = Duration::from_millis(500);
/// Cap on a batch's wait, so a slow trickle of events cannot postpone a sync
/// indefinitely.
const SYNC_BATCH_CAP: Duration = Duration::from_secs(5);
/// Connect retry for the engine's business client: the daemon binds its unix
/// socket shortly after `spawn_daemon` returns.
const CLIENT_CONNECT_ATTEMPTS: usize = 20;
const CLIENT_CONNECT_DELAY: Duration = Duration::from_millis(100);

/// Starts the sync engine in a background thread. Watches `sync_roots`
/// (device-side download directories) and pushes changes to the VM through the
/// daemon's shared request channel, using its own business-side client. The
/// thread lives as long as the process does.
pub fn spawn_sync_engine(
    socket_path: String,
    shared: Arc<SharedControl>,
    sync_roots: Vec<PathBuf>,
) {
    let result = std::thread::Builder::new()
        .name("cmd-agent-sync".to_string())
        .spawn(move || {
            log::info!("sync engine starting, roots={sync_roots:?}");
            match sync_engine_main(&socket_path, shared, &sync_roots) {
                Ok(()) => log::info!("sync engine exited cleanly"),
                Err(err) => log::error!("sync engine failed: {err}"),
            }
        });
    if let Err(err) = result {
        log::error!("failed to spawn sync engine thread: {err}");
    }
}

fn sync_engine_main(
    socket_path: &str,
    shared: Arc<SharedControl>,
    sync_roots: &[PathBuf],
) -> Result<()> {
    // Connect a business-side client for this engine, like the host process
    // does; the daemon's file_sync_loop executes the transfers.
    let mut client = None;
    for attempt in 0..CLIENT_CONNECT_ATTEMPTS {
        match Client::connect(socket_path, None, shared.clone()) {
            Ok(connected) => {
                client = Some(connected);
                break;
            }
            Err(err) => {
                log::debug!("sync engine client connect attempt {attempt} failed: {err}");
                std::thread::sleep(CLIENT_CONNECT_DELAY);
            }
        }
    }
    let client = client.ok_or_else(|| Error::message("sync engine client connect failed"))?;

    // Build the notify watcher and watch every directory under each root
    // (notify does not recurse by itself). New subdirectories are added to the
    // watch when a Create(Dir) event arrives.
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())
        .with_context(|| "creating notify watcher".to_string())?;
    // Watch every root that already exists. Roots that do not exist yet (a
    // fresh sandbox before any download) are recorded and retried in the
    // event loop: once they appear they are watched and scanned once, so a
    // download that finished in the gap is still mirrored.
    let mut missing_roots: Vec<PathBuf> = Vec::new();
    for root in sync_roots {
        if root.exists() {
            watch_recursive(&mut watcher, root)?;
        } else {
            log::info!(
                "sync engine: root {} does not exist yet, will watch on creation",
                root.display()
            );
            missing_roots.push(root.clone());
        }
    }

    // Event pump with debounce: collect write-complete events, then fire one
    // batch once the stream quiets down (or hits the cap).
    let mut pending: HashMap<PathBuf, FileSyncOp> = HashMap::new();
    let mut last_event = Instant::now();
    let mut batch_start = Instant::now();
    let mut sync_id: u64 = 0;
    loop {
        if !pending.is_empty() {
            let idle = last_event.elapsed() >= SYNC_DEBOUNCE;
            let capped = batch_start.elapsed() >= SYNC_BATCH_CAP;
            if idle || capped {
                sync_id += 1;
                let ops: Vec<FileSyncOp> = pending.drain().map(|(_, op)| op).collect();
                log::info!(
                    "sync engine: pushing {} ops (sync_id={sync_id})",
                    ops.len()
                );
                for op in &ops {
                    // [diag] list every op so the pushed set can be cross-checked
                    // against the github_download finalize log.
                    log::info!("[diag] sync op(sync_id={sync_id}): {op:?}");
                }
                if let Err(err) = client.file_sync(sync_id, ops) {
                    log::error!("sync engine: file_sync failed: {err}");
                }
                batch_start = Instant::now();
            }
        }

        match rx.recv_timeout(SYNC_DEBOUNCE) {
            Ok(Ok(event)) => {
                last_event = Instant::now();
                handle_event(&mut watcher, &mut pending, event);
            }
            Ok(Err(err)) => {
                log::warn!("sync engine: watcher error: {err}");
            }
            Err(RecvTimeoutError::Timeout) => {
                // Wake up to re-check the debounce condition above, and retry
                // watching sync roots that were missing at startup until they
                // appear.
                if !missing_roots.is_empty() {
                    retry_missing_roots(&mut watcher, &mut missing_roots, &mut pending);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                log::warn!("sync engine: watcher channel closed, exiting");
                break;
            }
        }
    }
    Ok(())
}

/// True when `path` lives under a temporary directory the sync engine must not
/// mirror (extension install staging areas and `.tmp-*` download dirs). These
/// files are unstable while being written or moved; syncing them would only
/// produce races and half-written mirrors.
fn is_temp_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == "staging" || name.starts_with(".tmp-") || name.starts_with(".tmp")
    })
}

/// Recursively watches every directory under `root` (notify is non-recursive).
fn watch_recursive(watcher: &mut RecommendedWatcher, root: &Path) -> Result<()> {
    if !root.exists() {
        log::info!(
            "sync engine: root {} does not exist yet, skipping",
            root.display()
        );
        return Ok(());
    }
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching {}", dir.display()))?;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && !is_temp_path(&path) {
                    dirs.push(path);
                }
            }
        }
    }
    Ok(())
}

/// Maps a write-complete notify event to pending FileSync operations. Events
/// that signal work in progress (data writes, metadata changes) are ignored so
/// a half-written file is never mirrored.
fn handle_event(
    watcher: &mut RecommendedWatcher,
    pending: &mut HashMap<PathBuf, FileSyncOp>,
    event: Event,
) {
    match event.kind {
        EventKind::Create(CreateKind::Folder) => {
            for path in &event.paths {
                if path.is_dir() && !is_temp_path(path) {
                    if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
                        log::warn!("sync engine: watch {} failed: {err}", path.display());
                    }
                }
            }
        }
        EventKind::Create(CreateKind::File) => {
            // The file may still be being written; the matching Close(Write)
            // carries the complete content, so wait for it.
        }
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
            for path in &event.paths {
                if is_temp_path(path) {
                    continue;
                }
                if path.is_dir() {
                    collect_dir_ops(path, pending);
                    if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
                        log::warn!("sync engine: watch {} failed: {err}", path.display());
                    }
                } else if path.is_file() {
                    pending.insert(
                        path.clone(),
                        FileSyncOp::WriteContent {
                            device_path: path.to_string_lossy().into_owned(),
                        },
                    );
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for path in &event.paths {
                if is_temp_path(path) {
                    continue;
                }
                if path.is_dir() {
                    // A directory renamed into place atomically (the "download
                    // to a temp dir, then rename" pattern): every write inside
                    // it happened in the temp dir the watcher never saw, so
                    // scan the whole tree now and watch the root for later
                    // changes (new files, subdirectory creates).
                    collect_dir_ops(path, pending);
                    if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
                        log::warn!("sync engine: watch {} failed: {err}", path.display());
                    }
                } else if path.is_file() {
                    pending.insert(
                        path.clone(),
                        FileSyncOp::WriteContent {
                            device_path: path.to_string_lossy().into_owned(),
                        },
                    );
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // A rename within/into a watched dir: the surviving path is the
            // complete file or directory, the vanished one is the temporary
            // source. A surviving directory needs a full tree scan, exactly
            // like a RenameMode::To directory.
            for path in &event.paths {
                if is_temp_path(path) {
                    continue;
                }
                let op = if path.is_dir() {
                    collect_dir_ops(path, pending);
                    if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
                        log::warn!("sync engine: watch {} failed: {err}", path.display());
                    }
                    continue;
                } else if path.exists() {
                    FileSyncOp::WriteContent {
                        device_path: path.to_string_lossy().into_owned(),
                    }
                } else {
                    FileSyncOp::Delete {
                        device_path: path.to_string_lossy().into_owned(),
                    }
                };
                pending.insert(path.clone(), op);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            for path in &event.paths {
                if is_temp_path(path) {
                    continue;
                }
                pending.insert(
                    path.clone(),
                    FileSyncOp::Delete {
                        device_path: path.to_string_lossy().into_owned(),
                    },
                );
            }
        }
        EventKind::Remove(_) => {
            for path in &event.paths {
                if is_temp_path(path) {
                    continue;
                }
                pending.insert(
                    path.clone(),
                    FileSyncOp::Delete {
                        device_path: path.to_string_lossy().into_owned(),
                    },
                );
            }
        }
        _ => {
            log::debug!("sync engine: ignored event {:?}", event.kind);
        }
    }
}

/// Retries watching sync roots that were missing at startup (a fresh sandbox
/// before any download). Once a root appears, its tree is watched and scanned
/// once, because a download that finished during the gap is already on disk
/// and will not emit further events.
fn retry_missing_roots(
    watcher: &mut RecommendedWatcher,
    missing_roots: &mut Vec<PathBuf>,
    pending: &mut HashMap<PathBuf, FileSyncOp>,
) {
    let mut still_missing = Vec::new();
    for root in missing_roots.iter() {
        if root.exists() {
            log::info!(
                "sync engine: root {} appeared, watching and scanning",
                root.display()
            );
            if let Err(err) = watch_recursive(watcher, root) {
                log::warn!("sync engine: watch {} failed: {err}", root.display());
            }
            collect_dir_ops(root, pending);
        } else {
            still_missing.push(root.clone());
        }
    }
    *missing_roots = still_missing;
}

/// Recursively collects sync operations for a directory tree that was renamed
/// or written into place in one atomic move (the "download to a temp dir, then
/// rename" pattern). Every file inside it was created in the temp dir, which
/// the watcher never saw, so its contents must be scanned now; subsequent
/// changes are caught by the watcher the caller registers on the root.
fn collect_dir_ops(root: &Path, pending: &mut HashMap<PathBuf, FileSyncOp>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if is_temp_path(&dir) {
            continue;
        }
        pending.insert(
            dir.clone(),
            FileSyncOp::CreateDir {
                device_path: dir.to_string_lossy().into_owned(),
            },
        );
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_temp_path(&path) {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else {
                    pending.insert(
                        path.clone(),
                        FileSyncOp::WriteContent {
                            device_path: path.to_string_lossy().into_owned(),
                        },
                    );
                }
            }
        }
    }
}
