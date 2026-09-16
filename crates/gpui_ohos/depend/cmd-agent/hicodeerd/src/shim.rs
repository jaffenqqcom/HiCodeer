//! Preloads that repair host defects in the programs this daemon spawns.
//!
//! Two files ship inside the daemon's own package, in `<pkg>/shim/` beside
//! `bin/` and `conf/`. Because they travel in the package, the system installs
//! them and replaces them on every reinstall, and nothing at runtime writes,
//! checks or heals a copy of its own.
//!
//! [`install`] resolves that directory from the package root -- see
//! `crate::package_root` -- and exports the two variables that make every later
//! child carry the preloads. Both consumers read their variable from the
//! environment at exec time, so the preloads reach command exec, PTY shells and
//! any spawn site added later without a per-call hook.
//!
//! - `NODE_OPTIONS=--require <pkg>/shim/shim.js`. The script's own header
//!   details what it repairs; in short, an OHOS app runs under a uid no account
//!   database entry covers, so `os.userInfo()` throws `ERR_SYSTEM_ERROR`,
//!   `process.platform` is reported as `openharmony`, which tooling that
//!   switches on it treats as fatal, and the install tree's filesystem refuses
//!   symlinks, which stops `npm install` when it builds `node_modules/.bin`.
//! - `LD_PRELOAD=<pkg>/shim/musllib-shim.so`. The platform's libc caps pthread
//!   keys at `PTHREAD_KEYS_MAX` (128) per process, and the
//!   `aarch64-unknown-linux-ohos` target does not enable native thread-local
//!   storage, so Rust's std implements `thread_local!` on top of those keys --
//!   one key per variable in the whole process. Any program that loads enough
//!   Rust code at once (a language server, a build) therefore runs out of keys
//!   and aborts with "out of TLS keys". The preload interposes the pthread TLS
//!   entry points and serves a virtual key space from a single real key, which
//!   removes the ceiling for every process it is loaded into.
//!
//! A preload that is absent is reported and left out of the environment rather
//! than named in it: both consumers treat an unusable entry as fatal to their
//! own startup -- the loader refuses an `LD_PRELOAD` it cannot open, and Node
//! refuses a `--require` naming a missing file -- which is worse than the
//! defects this works around.
//!
//! Only the OHOS build ships the payload: both defects are properties of the
//! OHOS runtime, and the guest runs a full Linux userland that exhibits
//! neither. The guest build therefore finds an empty directory and carries on.
//! No preload is required for the daemon to serve: nothing here returns an
//! error, and every unusable preload is reported and skipped the same way a
//! missing one is.

use std::path::Path;

/// Directory under the package root holding the preloads.
const SHIM_SUBDIR: &str = "shim";
/// Node preload, loaded through `--require`.
const NODE_SHIM_FILE: &str = "shim.js";
/// Shared-object preload, loaded through `LD_PRELOAD`.
const TLS_SHIM_FILE: &str = "musllib-shim.so";
/// Env var Node reads its default command-line flags from.
const NODE_OPTIONS_ENV: &str = "NODE_OPTIONS";
/// Node flag that loads a script before the program Node was asked to run.
const NODE_OPTIONS_REQUIRE: &str = "--require";
/// Env var the dynamic loader reads its preload list from.
const LD_PRELOAD_ENV: &str = "LD_PRELOAD";
/// Env var carrying the username the Node preload reports.
const SHIM_USER_ENV: &str = "HICODEERD_OSUSER";
/// Username the Node preload reports when `SHIM_USER_ENV` is unset.
const SHIM_USER: &str = "hicodeer";

/// Exports the environment that makes every program spawned from here on carry
/// both preloads. A preload that cannot be used is reported and skipped; the
/// daemon keeps serving either way.
pub fn install() {
    let directory = match crate::package_root() {
        Ok(root) => root.join(SHIM_SUBDIR),
        Err(err) => {
            log::warn!("shim: cannot resolve the package root: {err}");
            return;
        }
    };

    let node_shim = directory.join(NODE_SHIM_FILE);
    match expressible(&node_shim) {
        Some(path) => {
            append_env(NODE_OPTIONS_ENV, &format!("{NODE_OPTIONS_REQUIRE} {path}"));
            // The preload reports this instead of the account database entry
            // this process does not have.
            std::env::set_var(SHIM_USER_ENV, SHIM_USER);
        }
        None => {
            log::warn!(
                "shim: no Node preload at {}; Node children keep the platform defects",
                node_shim.display()
            );
        }
    }

    let tls_shim = directory.join(TLS_SHIM_FILE);
    match expressible(&tls_shim) {
        Some(path) => {
            append_env(LD_PRELOAD_ENV, &path);
        }
        None => {
            log::warn!(
                "shim: no TLS preload at {}; children keep the pthread key ceiling",
                tls_shim.display()
            );
        }
    }
}

/// Returns the path in a form the consumer's variable can carry, or `None` when
/// the file is not there or the variable cannot express the path.
///
/// Both consumers split their variable on whitespace and offer no quoting, so a
/// path containing whitespace cannot be written down at all; a path that is not
/// valid UTF-8 cannot either. Absence is left to the caller to report, so that
/// each preload is described in its own terms.
fn expressible(path: &Path) -> Option<String> {
    let text = match path.to_str() {
        Some(text) => text,
        None => {
            log::warn!("shim: {path:?} is not valid UTF-8, skipping it");
            return None;
        }
    };
    if text.contains(char::is_whitespace) {
        log::warn!("shim: {text} contains whitespace, skipping it");
        return None;
    }
    if !path.is_file() {
        return None;
    }
    Some(text.to_string())
}

/// Appends `addition` to the whitespace-separated `variable` unless the value
/// already carries it: a preload inherited from the parent keeps working, and
/// naming the same entry twice would only lengthen the value.
fn append_env(variable: &str, addition: &str) {
    let mut value = std::env::var(variable).unwrap_or_default();
    if value.contains(addition) {
        return;
    }
    if !value.is_empty() {
        value.push(' ');
    }
    value.push_str(addition);
    std::env::set_var(variable, value);
}
