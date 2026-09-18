//! Gives every command this daemon spawns a writable temporary directory.
//!
//! The daemon runs under its own account and cannot read the host application's
//! environment, so the directory arrives with the management bootstrap request:
//! the client appends the directory it works in, this side records it for that
//! client, and the programs that client spawns are handed it as `TMPDIR`.
//!
//! The device offers no usable global scratch area: `/tmp` is a read-only image
//! mount, and the sandbox cache the platform exports through `TMPDIR` resolves
//! to a different directory (or to nothing at all) once the process leaves the
//! account it was exported for. Children that cannot create scratch files there
//! end up parking them in the user's home directory instead, which both litters
//! a directory the user browses and leaves the files behind whenever a child is
//! killed before it can clean up.
//!
//! `TMPDIR` alone moves them: the standard library's `temp_dir()` consults that
//! variable and nothing else, so pointing it at `<root>/tmp` fixes the problem
//! at the source. The adopted root is a real directory the host application and
//! the guest VM both see at the same absolute path, it outlives
//! reinstallations, and it is the one location this daemon is guaranteed to be
//! able to write.
//!
//! Each client instance keeps its own root, because several instances may be
//! served at once and every spawn is handed the value of the instance that
//! asked for it. The value is set on the child being started rather than in
//! this process's own environment: a variable set here would be inherited by
//! every child alike, with no way to tell afterwards which client a given
//! child's value came from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Env var `std::env::temp_dir()` reads, and the one child toolchains honour.
/// Spawn sites name it when handing a child the value recorded here.
pub(crate) const TMPDIR_VAR: &str = "TMPDIR";
/// Directory under the adopted root holding session scratch files.
const TMP_SUBDIR: &str = "tmp";

/// Scratch directory per client instance, keyed by the identity its connections
/// authenticated as.
static CLIENT_TMPDIRS: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Borrows the record, ignoring a poisoned lock (it is plain data that stays
/// consistent, so a panic elsewhere must not disable it).
fn tmpdirs() -> std::sync::MutexGuard<'static, HashMap<String, PathBuf>> {
    CLIENT_TMPDIRS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Adopts the root a connected client reported: creates the scratch directory
/// under it and records it as that client's `TMPDIR` value. Later reports from
/// the same client name the same value, so the work runs once per client.
///
/// A root that cannot be prepared is ignored rather than recorded -- pointing a
/// child at a directory that does not exist would break it, which is worse than
/// leaving the child with whatever it would otherwise inherit.
pub(crate) fn adopt(client_id: &str, root: &Path) {
    if tmpdirs().contains_key(client_id) {
        return;
    }
    let dir = root.join(TMP_SUBDIR);
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::warn!("session tmp: create {}: {err}", dir.display());
        return;
    }
    tmpdirs().insert(client_id.to_string(), dir);
}

/// The `TMPDIR` value recorded for `client_id`, for a spawn site to hand to the
/// child it is about to start. `None` while that client has reported no root.
pub(crate) fn tmpdir(client_id: &str) -> Option<PathBuf> {
    tmpdirs().get(client_id).cloned()
}
