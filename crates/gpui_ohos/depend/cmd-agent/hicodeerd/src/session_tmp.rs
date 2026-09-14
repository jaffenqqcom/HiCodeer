//! Gives every command this daemon spawns a writable temporary directory.
//!
//! The daemon runs under its own account and cannot read the host application's
//! environment, so the directory arrives with the management bootstrap request:
//! the client appends the directory it works in, this side adopts it once, and
//! the value stays for the rest of the run. Children inherit it because they
//! inherit this process's environment.
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

use std::path::Path;
use std::sync::OnceLock;

/// Env var `std::env::temp_dir()` reads, and the one child toolchains honour.
const TMPDIR_ENV: &str = "TMPDIR";
/// Directory under the adopted root holding session scratch files.
const TMP_SUBDIR: &str = "tmp";

/// Set the first time a client reports its root. Later reports repeat the same
/// value on the bootstrap poll, so the work below runs exactly once.
static ADOPTED: OnceLock<()> = OnceLock::new();

/// Adopts the root a connected client reported: creates the scratch directory
/// under it and exports `TMPDIR` to every program this daemon spawns from here
/// on.
///
/// A root that cannot be prepared is ignored rather than applied -- exporting a
/// `TMPDIR` naming a directory that does not exist would break every child,
/// which is worse than leaving the inherited value alone.
pub(crate) fn adopt(root: &Path) {
    if ADOPTED.get().is_some() {
        return;
    }
    let dir = root.join(TMP_SUBDIR);
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::warn!("session tmp: create {}: {err}", dir.display());
        return;
    }
    std::env::set_var(TMPDIR_ENV, &dir);
    let _ = ADOPTED.set(());
}
