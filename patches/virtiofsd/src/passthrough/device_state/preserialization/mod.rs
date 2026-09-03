// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::passthrough::file_handle::{FileOrHandle, SerializableFileHandle};
use crate::passthrough::inode_store::{InodeData, StrongInodeReference};
use crate::passthrough::{self, MigrationMode};
use crate::util::ResultErrorContext;
use std::convert::TryInto;
use std::ffi::CStr;
use std::fmt::{self, Display};
use std::io;

pub mod file_handles;
pub mod find_paths;
pub mod proc_paths;

/// Precursor to `serialized::Inode` that is constructed while serialization is being prepared, and
/// will then be transformed into the latter at the time of serialization.  To be stored in the
/// inode store, alongside each inode (i.e. in its `InodeData`).  Constructing this is costly, so
/// should only be done when necessary, i.e. when actually preparing for migration.
pub(in crate::passthrough) struct InodeMigrationInfo {
    /// Location of the inode (how the destination can find it)
    pub location: InodeLocation,

    /// The inode's file handle.  The destination is not supposed to open this handle, but instead
    /// compare it against the one from the inode it has opened based on `location`.
    pub file_handle: Option<SerializableFileHandle>,
}

pub(in crate::passthrough) enum InodeLocation {
    /// The root node: No information is stored, the destination is supposed to find this on its
    /// own (as configured by the user)
    RootNode,

    /// Inode is represented by its parent directory and its filename therein, allowing the
    /// destination to `openat(2)` it
    Path(find_paths::InodePath),

    /// Inode is represented by its file handle
    FileHandle(file_handles::FileHandle),
}

/// Precursor to `SerializableHandleRepresentation` that is constructed while serialization is
/// being prepared, and will then be transformed into the latter at the time of serialization.
/// To be stored in the `handles` map, alongside each handle (i.e. in its `HandleData`).
/// Constructing this is cheap, so can be done whenever any handle is created.
pub(in crate::passthrough) enum HandleMigrationInfo {
    /// Handle can be opened by opening its associated inode with the given `open(2)` flags
    OpenInode { flags: i32 },
}

impl InodeMigrationInfo {
    /// General function for public use that creates the correct `InodeLocation` variant based on
    /// the `migration_mode` setting
    pub fn new(
        fs_cfg: &passthrough::Config,
        parent_ref: StrongInodeReference,
        filename: &CStr,
        file_or_handle: &FileOrHandle,
    ) -> io::Result<Self> {
        let location: InodeLocation = match fs_cfg.migration_mode {
            MigrationMode::FindPaths => {
                find_paths::InodePath::new_with_cstr(parent_ref, filename)?.into()
            }

            MigrationMode::FileHandles => {
                let handle = file_or_handle.try_into().err_context(|| {
                    format!(
                        "(inode {})/{filename:?}: Failed to generate file handle",
                        parent_ref.get().inode,
                    )
                })?;
                file_handles::FileHandle::new(handle).into()
            }
        };
        Self::new_internal(fs_cfg, location, || file_or_handle.try_into())
    }

    /// Internal `new` function that takes the actually constituting elements of the struct
    fn new_internal<L: Into<InodeLocation>, F: FnOnce() -> io::Result<SerializableFileHandle>>(
        fs_cfg: &passthrough::Config,
        inode_location: L,
        file_handle_fn: F,
    ) -> io::Result<Self> {
        let file_handle: Option<SerializableFileHandle> = if fs_cfg.migration_verify_handles {
            Some(file_handle_fn()?)
        } else {
            None
        };

        Ok(InodeMigrationInfo {
            location: inode_location.into(),
            file_handle,
        })
    }

    /// Use this for the root node.  That node is special in that the destination gets no
    /// information on how to find it, because that is configured by the user.
    pub(in crate::passthrough) fn new_root(
        fs_cfg: &passthrough::Config,
        file_or_handle: &FileOrHandle,
    ) -> io::Result<Self> {
        Self::new_internal(fs_cfg, InodeLocation::RootNode, || {
            file_or_handle.try_into()
        })
    }

    /// Call the given function for each `StrongInodeReference` contained in this
    /// `InodeMigrationInfo`
    pub fn for_each_strong_reference<F: FnMut(StrongInodeReference)>(self, f: F) {
        match self.location {
            InodeLocation::RootNode => (),
            InodeLocation::Path(p) => p.for_each_strong_reference(f),
            InodeLocation::FileHandle(fh) => fh.for_each_strong_reference(f),
        }
    }

    /**
     * Return `true` if this migration info contains a path.
     *
     * If so, when the associated inode’s path is modified or invalidated (e.g. renamed, moved,
     * unlinked), its migration info must then be updated or invalidated accordingly.
     */
    pub fn has_path(&self) -> bool {
        // Use `match` instead of `matches!()` so we don’t forget any potential future variants
        match &self.location {
            InodeLocation::RootNode => false,
            InodeLocation::Path(_) => true,
            InodeLocation::FileHandle(_) => false,
        }
    }

    /**
     * Assuming this migration info contains a path, check whether the associated inode (given
     * through `inode_data`) is indeed present under that path, returning an error if (and only if)
     * it is not.
     *
     * Always return `Ok(())` if this migration info’s location is not defined by a path.
     */
    pub fn check_path_presence(&self, inode_data: &InodeData) -> io::Result<()> {
        match &self.location {
            InodeLocation::RootNode => Ok(()),
            InodeLocation::Path(p) => p.check_presence(inode_data, self),
            InodeLocation::FileHandle(_) => Ok(()),
        }
    }
}

impl Display for InodeLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InodeLocation::RootNode => write!(f, "[shared directory root]"),
            InodeLocation::Path(p) => write!(f, "{p}"),
            InodeLocation::FileHandle(fh) => write!(f, "{fh}"),
        }
    }
}

impl HandleMigrationInfo {
    /// Create the migration info for a handle that will be required when serializing
    pub fn new(flags: i32) -> Self {
        HandleMigrationInfo::OpenInode {
            // Remove flags that make sense when the file is first opened by the guest, but which
            // we should not set when continuing to use the file after migration because they would
            // e.g. modify the file
            flags: flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC),
        }
    }
}
