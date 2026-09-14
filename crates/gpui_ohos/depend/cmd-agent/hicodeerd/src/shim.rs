//! Preload that keeps spawned Node.js processes working on hosts that depart
//! from a mainstream Linux userland.
//!
//! Some Node tooling assumes a familiar platform and fails outright here: an
//! OHOS app or service runs under a uid no account database entry covers, so
//! `os.userInfo()` throws `ERR_SYSTEM_ERROR`, and `process.platform` is reported
//! as `openharmony`, which tools that switch on it treat as fatal. Both are
//! repaired by one preload script.
//!
//! That script is `shim.js`, and it is the only preload -- `include_str!` pulls
//! it into this executable, so the file ships with the binary rather than beside
//! it. Add new host fixes inside that script instead of adding a second shim:
//! the script's own header explains why. Do not reintroduce a per-bug file here
//! or in `shim/`.
//!
//! The script is re-materialised under the user data root, which the host
//! application and the guest VM both see at the same absolute path -- that
//! makes it reachable whichever system the daemon runs on. Children receive it
//! through `NODE_OPTIONS` unconditionally: the preload calls the real
//! implementation first and only substitutes when that call fails, so on hosts
//! where nothing is wrong it is a no-op.
//!
//! The data root is not guessed here. It arrives with the management bootstrap
//! request -- the same one that carries the session temporary directory --
//! because the daemon runs under its own account and cannot read the host
//! application's environment.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Preload script, compiled in so no side file has to travel with the binary.
const SHIM_SOURCE: &str = include_str!("../shim/shim.js");
/// File name of the preload.
const SHIM_FILE: &str = "shim.js";
/// Where the preload lives under the data root.
const SHIM_SUBDIR: &str = "node/shim";
/// Env var Node reads its default command-line flags from.
const NODE_OPTIONS_ENV: &str = "NODE_OPTIONS";
/// Env var carrying the username the preload reports.
const SHIM_USER_ENV: &str = "HICODEERD_OSUSER";
/// Username the preload reports when `SHIM_USER_ENV` is unset.
const SHIM_USER: &str = "hicodeer";

/// Where the preload ended up, plus the `NODE_OPTIONS` value that loads it.
struct Resolved {
    path: PathBuf,
    node_options: String,
}

/// Set the first time a client reports its root. Later reports repeat the same
/// value on the bootstrap poll, so the work below runs exactly once.
static RESOLVED: OnceLock<Resolved> = OnceLock::new();

/// Materialises the preload under the root a connected client reported and
/// caches the `NODE_OPTIONS` value children must inherit.
///
/// A root that cannot hold the preload is left unrecorded rather than recorded
/// as a failure: exporting a `NODE_OPTIONS` naming a file that is not there
/// would stop every Node process from starting at all, which is worse than the
/// bugs this works around. The next poll retries.
pub(crate) fn adopt(root: &Path) {
    if RESOLVED.get().is_some() {
        return;
    }
    let path = root.join(SHIM_SUBDIR).join(SHIM_FILE);
    let Some(node_options) = place_and_build(&path) else {
        log::warn!("shim: cannot place {SHIM_FILE} under {}", root.display());
        return;
    };
    if RESOLVED.set(Resolved { path, node_options }).is_err() {
        log::warn!("shim: adopted more than once, keeping the first value");
    }
}

/// Adds the preload to a child's environment; no-op only when no usable
/// location was found. Silent on the happy path -- [`place_and_build`] warns
/// for the cases worth knowing about.
pub fn apply(cmd: &mut tokio::process::Command) {
    if let Some(resolved) = RESOLVED.get() {
        // The data root doubles as the Node scratch area, and the managed
        // runtime wipes that area when it re-fetches Node. Re-checking here
        // keeps `NODE_OPTIONS` from naming a file that just disappeared, which
        // would stop the child from starting at all.
        ensure_placed(&resolved.path);
        cmd.env(NODE_OPTIONS_ENV, &resolved.node_options);
        cmd.env(SHIM_USER_ENV, SHIM_USER);
    }
}

/// Writes the compiled-in preload to `path` and returns the `NODE_OPTIONS`
/// value that loads it, or `None` when that path cannot be used.
fn place_and_build(path: &Path) -> Option<String> {
    // Never poison NODE_OPTIONS with a path Node cannot load: an unusable
    // `--require` stops every Node process in the session from starting at all,
    // which is worse than the bugs this works around.
    let shim = match path.to_str() {
        Some(shim) => shim,
        None => {
            log::warn!("shim: {path:?} is not valid UTF-8, skipping it");
            return None;
        }
    };
    // Node splits NODE_OPTIONS on whitespace and offers no quoting, so a path
    // containing whitespace cannot be expressed at all.
    if shim.contains(char::is_whitespace) {
        log::warn!("shim: {shim} contains whitespace, skipping it");
        return None;
    }
    if !write_if_stale(path) {
        return None;
    }

    let mut value = std::env::var(NODE_OPTIONS_ENV).unwrap_or_default();
    if !value.contains(shim) {
        if !value.is_empty() {
            value.push(' ');
        }
        value.push_str("--require ");
        value.push_str(shim);
    }
    Some(value)
}

/// Writes the preload unless the file already holds exactly that content.
/// Returns whether the file is in place afterwards.
fn write_if_stale(path: &Path) -> bool {
    let fresh = std::fs::read(path).is_ok_and(|current| current == SHIM_SOURCE.as_bytes());
    if fresh {
        return true;
    }
    if let Some(dir) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(dir) {
            log::warn!("shim: create {}: {err}", dir.display());
            return false;
        }
    }
    match std::fs::write(path, SHIM_SOURCE) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("shim: write {}: {err}", path.display());
            false
        }
    }
}

/// In-flight repair, cheap enough to run per child: a single `stat` decides
/// whether the preload is still there, and only a miss costs a rewrite.
fn ensure_placed(path: &Path) {
    let present = std::fs::metadata(path).is_ok_and(|meta| meta.len() == SHIM_SOURCE.len() as u64);
    if !present {
        write_if_stale(path);
    }
}
