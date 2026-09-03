// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Facilities for getting inodes’ paths from /proc/self/fd for migration.
 *
 * This module provides different objects that all share the same core for multiple purposes:
 * - Provide a preserialization migration info constructor for the find-paths migration mode
 * - Check migration info paths during migration and, if found incorrect, reconstruct them as we
 *   would for preserialization; this is used by --migration-confirm-paths, as well as an implicit
 *   double-check step after any path-based preserialization phase
 */

use super::InodeMigrationInfo;
use crate::fuse;
use crate::passthrough::inode_store::{InodeData, InodePathError, StrongInodeReference};
use crate::passthrough::stat::statx;
use crate::passthrough::util::{relative_path, FdPathError};
use crate::passthrough::PassthroughFs;
use crate::util::{other_io_error, ErrorContext};
use std::ffi::{CStr, CString};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/**
 * Provides all core functionality.
 *
 * This module provides functionality for three different cases; all of it is implemented on this
 * single internal struct that is incorporated into different public structs depending on the use.
 *
 * `Walker::run()` is the core method, which walks over the inode store and can check paths in
 * inode migration info structures, and construct them by looking into /proc/self/fd.  What exactly
 * is done depends on `mode`.
 */
struct Walker<'a> {
    /// Reference to the filesystem state to check
    fs: &'a PassthroughFs,
    /// Specifies which functionality we are supposed to provide
    #[allow(dead_code)] // will be used once we provide more than one mode
    mode: Mode,
    /// Optional: Cancel early
    cancel: Option<Arc<AtomicBool>>,
}

/**
 * Construct paths during preserialization.
 *
 * Give all inodes that don’t have a migration info set a path from /proc/self/fd.
 */
pub(in crate::passthrough::device_state) struct Constructor<'a> {
    /// `Walker` in `Mode::Constructor` mode.
    walker: Walker<'a>,
}

/**
 * `--migration-confirm-paths` implementation.
 *
 * Implements checking inodes’ paths right before serialization, as requested by the user through
 * the `--migration-confirm-paths` switch: Give all inodes that either don’t have a migration info
 * set, or where it is found to be incorrect, a path from /proc/self/fd.  Furthermore, given the
 * user has specifically requested this check run, return any error as a hard error, preventing
 * migration.
 */
pub(in crate::passthrough::device_state) struct ConfirmPaths<'a> {
    /// `Walker` in `Mode::ConfirmPaths` mode.
    walker: Walker<'a>,
}

/**
 * Double-check inodes’ paths after preserialization.
 *
 * Similar to `ConfirmPaths`, but is an implicit double-check run after the first preserialization
 * phase, and as a result, is more relaxed:
 * - On a fundamental unrecoverable error (e.g. failing to find the shared directory’s base path),
 *   printing a warning an skipping the whole run is OK
 * - We only need to find new paths for inodes that have a path in their migration info when we
 *   found that path to be incorrect.  No need to try to find paths for inodes that don’t have any
 *   migration info attached to them.
 */
pub(in crate::passthrough::device_state) struct ImplicitPathCheck<'a> {
    /// `Walker` in `Mode::ImplicitPathCheck` mode.
    walker: Walker<'a>,
}

/// Selects how a `Walker` should behave.
pub(in crate::passthrough::device_state) enum Mode {
    /// Collect inodes’s paths during preserialization.
    Constructor,

    /// Run the `--migration-confirm-paths` check.
    ConfirmPaths,

    /// Double-check inodes’ paths after preserialization.
    ImplicitPathCheck,
}

/**
 * Error type to enable `--migration-mode=find-paths` fall-back functionality.
 *
 * `--migration-mode=find-paths` first tries to get inodes’ paths from /proc/self/fd.  That
 * implementation is provided by this module.  If that fails, this error allows distinguishing
 * between:
 * - errors that may not happen when using another method of finding inodes’ paths (e.g.
 *   exhaustive iteration of everything inside of the shared directory), and
 * - errors that would probably happen regardless.
 *
 * That is, if encountering errors of the former type, we should fall back to the other method
 * (provided by [`super::find_paths`]).
 */
pub(in crate::passthrough) enum WrappedError {
    /// A different preserialization method might be able to find this path.
    Fallback(io::Error),

    /// Unrecoverable error, falling back probably won’t change anything.
    Unrecoverable(io::Error),
}

impl<'a> Constructor<'a> {
    /// Prepare to collect paths for `fs`.
    pub fn new(fs: &'a PassthroughFs, cancel: Arc<AtomicBool>) -> Self {
        Constructor {
            walker: Walker::new(fs, Mode::Constructor, Some(cancel)),
        }
    }

    /**
     * Collect paths for all inodes in our inode store, during preserialization.
     *
     * Look through all inodes in our inode store, try to get their paths from /proc/self/fd,
     * constructing `InodeMigrationInfo` data for them.  May take some time, so is done during the
     * pre-serialization phase of migration.
     *
     * Cannot fail: Collecting inodes’ migration info is supposed to be a best-effort operation.
     * We can leave any and even all inodes’ migration info empty, then serialize them as invalid
     * inodes, and let the destination decide what to do based on its `--migration-on-error`
     * setting.
     *
     * However, it is possible that we find inodes whose paths we failed to get from /proc/self/fd,
     * but believe they probably do have a valid path inside the shared directory anyway (which the
     * kernel just failed to report); in this case, return `true` so the caller can decide to fall
     * back to the [`super::find_paths`] implementation.
     */
    pub fn execute(self) -> bool {
        match self.walker.run() {
            Ok(()) => false,

            Err(WrappedError::Fallback(err)) => {
                warn!("Failed to construct inode paths: {err}");
                true
            }

            // Unrecoverable error where not even falling back makes sense should be a rare
            // occurrence
            Err(WrappedError::Unrecoverable(err)) => {
                error!("Failed to construct inode paths: {err}; may be unable to migrate");
                false
            }
        }
    }
}

impl<'a> ConfirmPaths<'a> {
    /// Prepare to confirm paths collected for `fs`.
    pub fn new(fs: &'a PassthroughFs) -> Self {
        ConfirmPaths {
            walker: Walker::new(fs, Mode::ConfirmPaths, None),
        }
    }

    /**
     * Run the `--migration-confirm-paths` check.
     *
     * If necessary, try to fix the paths collected during the preserialization phase by looking
     * into /proc/self/fd.  Return errors.
     */
    pub fn confirm_paths(self) -> io::Result<()> {
        // There is no fallback in `ConfirmPaths` mode, treat all errors the same way
        self.walker.run().map_err(WrappedError::into_inner)
    }
}

impl<'a> ImplicitPathCheck<'a> {
    /// Prepare to double-check paths during preserialization.
    pub fn new(fs: &'a PassthroughFs, cancel: Arc<AtomicBool>) -> Self {
        ImplicitPathCheck {
            walker: Walker::new(fs, Mode::ImplicitPathCheck, Some(cancel)),
        }
    }

    /**
     * Double-check inodes’ paths after preserialization.
     *
     * Try to fix any paths that are wrong (by getting new paths from /proc/self/fd), but do not
     * return errors: This check is implicit, not requested by the user, so should be infallible,
     * not cancelling migration on error.
     */
    pub fn check_paths(self) {
        if let Err(err) = self.walker.run() {
            // There is no fallback in `ImplicitPathCheck` mode, treat all errors the same way
            let err = err.into_inner();
            warn!("Double-check of all inode paths collected for migration failed: {err}")
        }
    }
}

impl<'a> Walker<'a> {
    /**
     * Create a `Walker` over `fs` with the given `mode`.
     *
     * If `cancel` is given, the operation will be cancelled when it is found to be set.
     */
    fn new(fs: &'a PassthroughFs, mode: Mode, cancel: Option<Arc<AtomicBool>>) -> Self {
        Walker { fs, mode, cancel }
    }

    /**
     * Run the `Walker` over all inodes in our store.
     *
     * Iterate through the store, check the paths we found (depending on the `mode`), and update
     * inodes’ migration info with paths from /proc/self/fd (depending on the `mode`).
     *
     * In case of error, differentiate between:
     * - `Fallback(err)`: We failed to construct inode migration info for some number of inodes.
     *   However, we expect a different, more exhaustive method to find inodes’ paths (e.g. DFS
     *   through the shared directory) can succeed.  In case of `Mode::Constructor`, the caller
     *   must fall back to such a different preserialization module (i.e. [`super::find_paths`]).
     *   In other modes, this should be treated the same as `Unrecoverable`.
     * - `Unrecoverable(err)`: Hard error, falling back to a different method is not advised.
     */
    fn run(self) -> Result<(), WrappedError> {
        let Some(root_node) = self.fs.inodes.get(fuse::ROOT_ID) else {
            // No root?  That’s fine if and only if we don’t have any inodes at all.
            return if self.fs.inodes.is_empty() {
                Ok(())
            } else {
                // Should never happen, consider this error unrecoverable
                Err(WrappedError::Unrecoverable(other_io_error(
                    "Root node not found",
                )))
            };
        };

        // It’s possible we fail to get the root node’s path, but we can’t continue then.  Advise
        // to fall back on error.
        let shared_dir_path = root_node.get_path(&self.fs.proc_self_fd).map_err(|err| {
            WrappedError::Fallback(
                io::Error::from(err).context("Failed to get shared directory's path"),
            )
        })?;

        for inode_data in self.fs.inodes.iter() {
            if self
                .cancel
                .as_ref()
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(false)
            {
                break;
            }

            if !self.should_update_inode(&inode_data) {
                continue;
            }

            let set_path_result =
                set_path_migration_info_from_proc_self_fd(&inode_data, self.fs, &shared_dir_path);
            match self.mode {
                // For preserialization, finding inodes is not worth a notification.  For errors,
                // we distinguish between errors for which we advise our caller to fall back to a
                // different preserialization methods, and once where we do not.  The latter we
                // just log, the former we return to the caller immediately.  They must then fall
                // back to an exhaustive method, so aborting early is OK.
                Mode::Constructor => match set_path_result {
                    Ok(()) => (),
                    Err(WrappedError::Fallback(err)) => return Err(WrappedError::Fallback(err)),
                    Err(WrappedError::Unrecoverable(err)) => {
                        error!("Inode {}: {}", inode_data.inode, err)
                    }
                },

                // In check modes, we note inodes we found, and log all kinds of errors
                // indiscriminately.
                Mode::ConfirmPaths | Mode::ImplicitPathCheck => {
                    if let Err(err) = set_path_result {
                        error!("Inode {}: {}", inode_data.inode, err.into_inner());
                    } else if let Some(new_info) =
                        inode_data.migration_info.lock().unwrap().as_ref()
                    {
                        info!("Found inode {}: {}", inode_data.inode, new_info.location);
                    }
                }
            }
        }

        Ok(())
    }

    /**
     * Check the given inode’s migration info.
     *
     * - Return `true` iff the info should be updated from /proc/self/fd.
     * - Return `false` iff the info seems fine, and should be left as-is.
     */
    fn should_update_inode(&self, inode_data: &InodeData) -> bool {
        let mut migration_info_locked = inode_data.migration_info.lock().unwrap();
        match (&self.mode, migration_info_locked.as_ref()) {
            // Do not touch inodes:
            // - Without migration info in the implicit/lax check mode
            // - When we are supposed to collect migration info, not check/update it, i.e. during
            //   preserialization, and the inode already has migration info
            (Mode::ImplicitPathCheck, None) | (Mode::Constructor, Some(_)) => false,

            // In both the explicit check mode and the preserialization constructor, give migration
            // info to those inodes that don’t already have it
            (Mode::ConfirmPaths, None) | (Mode::Constructor, None) => true,

            // In both check modes, when there is pre-existing migration info, we have to check its
            // path; update those we find to be incorrect
            (Mode::ConfirmPaths, Some(migration_info))
            | (Mode::ImplicitPathCheck, Some(migration_info)) => {
                if let Err(err) = migration_info.check_path_presence(inode_data) {
                    // Migration info is wrong, clear it unconditionally, regardless of whether we
                    // can find a better one
                    let migration_info = migration_info_locked.take().unwrap();
                    warn!(
                        "Lost inode {} (former location: {}): {}; looking it up through /proc/self/fd",
                        inode_data.inode, migration_info.location, err
                    );
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// Return an inode’s link count, if available.
fn link_count(inode_data: &InodeData) -> Option<libc::nlink_t> {
    inode_data
        .get_file()
        .ok()
        .and_then(|f| statx(&f, None).ok())
        .map(|stat| stat.st.st_nlink)
}

/**
 * Update inode migration info from /proc/self/fd.
 *
 * Fetch the given inode’s path from /proc/self/fd, split that path into components relative to
 * the shared directory root, and for all inodes along that path, if they don’t have a migration
 * info set, set it accordingly.
 *
 * Note that this is decidedly not a method of `Walker` so that we can easily reuse it in other
 * places; specifically, to re-establish a path for inodes that have been potentially invalidated.
 */
pub(in crate::passthrough) fn set_path_migration_info_from_proc_self_fd(
    inode_data: &InodeData,
    fs: &PassthroughFs,
    shared_dir_path: &CStr,
) -> Result<(), WrappedError> {
    let abs_path_result = inode_data.get_path(&fs.proc_self_fd);
    let Ok(abs_path) = abs_path_result else {
        let err = abs_path_result.unwrap_err();
        // In case of `Mode::Constructor`, depending on the exact kind of error, figure out whether
        // it makes sense to fall back to a different method of finding inodes’ paths
        let fall_back = match &err {
            // If the kernel reports this inode to be deleted even though it has a link somewhere,
            // fall back and try to find that link’s path
            InodePathError::FdPathError(FdPathError::Deleted(_)) => {
                link_count(inode_data).map(|n| n > 0).unwrap_or(false)
            }

            // If the kernel reports this inode under a path outside of the shared directory but it
            // has multiple links, one of those might be inside of the shared directory, so fall
            // back and try to find it
            InodePathError::OutsideRoot => link_count(inode_data).map(|n| n > 1).unwrap_or(false),

            // Very general problem, should not happen, so consider this unrecoverable
            InodePathError::NoFd(_) => false,

            // Consider all other internal errors from getting the path from /proc/self/fd to be
            // problems pertaining specifically to this method of obtaining paths, i.e. mark them
            // as `Fallback` errors
            InodePathError::FdPathError(_) => true,
        };
        let err = io::Error::from(err).context("Failed to get path from /proc/self/fd");
        return if fall_back {
            Err(WrappedError::Fallback(err))
        } else {
            Err(WrappedError::Unrecoverable(err))
        };
    };

    let rel_path = relative_path(&abs_path, shared_dir_path)
        .map_err(|err| {
            // Same as `OutsideRoot` above
            if link_count(inode_data).map(|n| n > 1).unwrap_or(false) {
                WrappedError::Fallback(err)
            } else {
                WrappedError::Unrecoverable(err)
            }
        })?
        .to_str()
        .map_err(|err| {
            // Non UTF-8 path names are unrecoverable
            WrappedError::Unrecoverable(other_io_error(format!(
                "Path {abs_path:?} is not a UTF-8 string: {err}"
            )))
        })?
        .to_string();

    let path = Path::new(&rel_path);

    // Getting the root node should always succeed; if it doesn’t, everything is broken anyway and
    // falling back will not fix it.
    let mut parent = fs
        .inodes
        .get_strong(fuse::ROOT_ID)
        .map_err(WrappedError::Unrecoverable)?;

    for element in path {
        // Both `unwrap()`s must succeed: We know the path is UTF-8, and we know it does not
        // contain internal NULs (because it used to be a CString before)
        let element_cstr = CString::new(element.to_str().unwrap()).unwrap();
        // This look-up automatically sets the inode migration data on this inode.
        // If we fail the look-up (i.e. fail to traverse the path), other migration methods are
        // unlikely to succeed either, so consider errors here unrecoverable.
        let entry = fs
            .do_lookup(parent.get().inode, &element_cstr)
            .map_err(WrappedError::Unrecoverable)?;

        // `entry.inode` is effectively a strong reference, so this must succeed
        let entry_data = fs.inodes.get(entry.inode).unwrap();
        // Safe: Turns `entry.inode` back into a typed strong reference
        let entry_inode = unsafe { StrongInodeReference::new_no_increment(entry_data, &fs.inodes) };

        {
            let entry_data = entry_inode.get();
            let mut mig_info = entry_data.migration_info.lock().unwrap();
            if mig_info.is_none() {
                // If we fail to set the migration info while traversing the path, other
                // preserialization methods will likely encounter the same problem.  Unrecoverable
                // error.
                *mig_info = Some(
                    InodeMigrationInfo::new(
                        &fs.cfg,
                        parent,
                        &element_cstr,
                        &entry_data.file_or_handle,
                    )
                    .map_err(WrappedError::Unrecoverable)?,
                );
            }
        }

        parent = entry_inode;
    }

    if parent.get().inode != inode_data.inode {
        // For some reason, we failed to end up on the inode where we wanted to end up.  Maybe
        // another preserialization method would have more luck?  Advise to fall back.
        return Err(WrappedError::Fallback(other_io_error(format!(
            "Inode not found under path reported by /proc/self/fd ({rel_path:?})"
        ))));
    }

    Ok(())
}

impl WrappedError {
    /// Return the contained `io::Error`, discarding the fall-back advice.
    pub fn into_inner(self) -> io::Error {
        match self {
            WrappedError::Fallback(err) => err,
            WrappedError::Unrecoverable(err) => err,
        }
    }
}
