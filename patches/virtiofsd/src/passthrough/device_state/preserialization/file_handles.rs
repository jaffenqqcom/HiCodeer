// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Preserialization implementation to represent all inodes as their file handles.

use super::{InodeLocation, InodeMigrationInfo};
use crate::passthrough::file_handle::{self, FileOrHandle, SerializableFileHandle};
use crate::passthrough::inode_store::{InodeData, StrongInodeReference};
use crate::passthrough::PassthroughFs;
use crate::util::ResultErrorContext;
use std::fmt::{self, Display};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The result of *file-handles* pre-serialization: A file handle.
pub(in crate::passthrough) struct FileHandle {
    /**
     * The file handle.
     *
     * Its mount ID is only valid on the migration source (i.e. here, because the source is where
     * preserialization occurs).
     */
    pub handle: SerializableFileHandle,
}

/**
 * Construct file handles during preserialization.
 *
 * Generate a file handle for all inodes that don’t have a migration info set yet.
 */
pub(in crate::passthrough::device_state) struct Constructor<'a> {
    /// Reference to the filesystem for which to reconstruct inodes’ paths.
    fs: &'a PassthroughFs,
    /// Set to true when we are supposed to cancel.
    cancel: Arc<AtomicBool>,
}

impl<'a> Constructor<'a> {
    /// Prepare to collect file handles for `fs`’s inodes.
    pub fn new(fs: &'a PassthroughFs, cancel: Arc<AtomicBool>) -> Self {
        Constructor { fs, cancel }
    }

    /**
     * Collect file handles for all inodes in our inode store, during preserialization.
     *
     * Recurse from the root directory (the shared directory), constructing `InodeMigrationInfo`
     * data for every inode in the inode store.  This may take a long time, which is why it is done
     * in the preserialization phase.
     *
     * Cannot fail: Collecting inodes’ migration info is supposed to be a best-effort operation.
     * We can leave any and even all inodes’ migration info empty, then serialize them as invalid
     * inodes, and let the destination decide what to do based on its `--migration-on-error`
     * setting.
     */
    pub fn execute(self) {
        for inode_data in self.fs.inodes.iter() {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }

            // Migration info is automatically cleared before `execute()`, so if we find migration
            // info here, it must be up-to-date, and we don't need to fill it in.
            if inode_data.migration_info.lock().unwrap().is_some() {
                continue;
            }

            if let Err(err) = self.set_migration_info(&inode_data) {
                error!(
                    "Inode {} ({}): {err}",
                    inode_data.inode,
                    inode_data.identify(&self.fs.proc_self_fd),
                );
            }
        }
    }
}

impl FileHandle {
    /// Trivial constructor.
    pub fn new(handle: SerializableFileHandle) -> Self {
        FileHandle { handle }
    }

    /**
     * Call `f` for each [`StrongInodeReference`] we have in `self`.
     *
     * File handles never contain references to other inodes, so this is a no-op.
     */
    pub(super) fn for_each_strong_reference<F: FnMut(StrongInodeReference)>(self, _f: F) {}
}

impl From<FileHandle> for InodeLocation {
    fn from(fh: FileHandle) -> Self {
        InodeLocation::FileHandle(fh)
    }
}

impl Display for FileHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[file handle: {}]", self.handle)
    }
}

impl Constructor<'_> {
    /**
     * Set `inode_data`’s migration info to its file handle.
     *
     * Try to generate a file handle for `inode_data` (or use the one we already have, if any), and
     * construct and set its migration info based on it.
     */
    fn set_migration_info(&self, inode_data: &InodeData) -> io::Result<()> {
        let handle: SerializableFileHandle = match &inode_data.file_or_handle {
            FileOrHandle::File(file) => file_handle::FileHandle::from_fd_fail_hard(file)
                .err_context(|| "Failed to generate file handle")?
                .into(),
            FileOrHandle::Handle(handle) => handle.inner().into(),
            FileOrHandle::Invalid(err) => return Err(io::Error::new(
                err.kind(),
                format!("Inode is invalid because of an error during the preceding migration, which was: {err}"),
            )),
        };

        let mig_info = InodeMigrationInfo::new_internal(
            &self.fs.cfg,
            FileHandle::new(handle.clone()),
            || Ok(handle),
        )?;

        *inode_data.migration_info.lock().unwrap() = Some(mig_info);
        Ok(())
    }
}
