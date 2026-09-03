// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{InodeLocation, InodeMigrationInfo};
use crate::filesystem::DirectoryIterator;
use crate::fuse;
use crate::passthrough::file_handle::{FileHandle, SerializableFileHandle};
use crate::passthrough::inode_store::{InodeData, InodeIds, StrongInodeReference};
use crate::passthrough::stat::statx;
use crate::passthrough::{FileOrHandle, PassthroughFs};
use crate::read_dir::ReadDir;
use crate::util::{other_io_error, ResultErrorContext};
use std::convert::{TryFrom, TryInto};
use std::ffi::{CStr, CString};
use std::fmt::{self, Display};
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The result of 'find-paths' pre-serialization: A filename relative to some parent inode.
pub(in crate::passthrough) struct InodePath {
    pub parent: StrongInodeReference,
    pub filename: String,
}

/// Stores state for constructing serializable data for inodes using the `InodeMigrationInfo::Path`
/// variant, in order to prepare for migration.
pub(in crate::passthrough::device_state) struct Constructor<'a> {
    /// Reference to the filesystem for which to reconstruct inodes' paths.
    fs: &'a PassthroughFs,
    /// Set to true when we are supposed to cancel
    cancel: Arc<AtomicBool>,
}

impl InodePath {
    /// Create the migration info for an inode that is collected during the `prepare_serialization`
    /// phase
    pub fn new_with_cstr(parent_ref: StrongInodeReference, filename: &CStr) -> io::Result<Self> {
        let utf8_name = filename.to_str().map_err(|err| {
            other_io_error(format!(
                "Cannot convert filename into UTF-8: {filename:?}: {err}"
            ))
        })?;

        Ok(InodePath {
            parent: parent_ref,
            filename: utf8_name.to_string(),
        })
    }

    pub(super) fn for_each_strong_reference<F: FnMut(StrongInodeReference)>(self, mut f: F) {
        f(self.parent);
    }

    /// Checker whether the associated inode (`inode_data`) is present under this path, returning
    /// an error if (and only if) it is not.
    pub(super) fn check_presence(
        &self,
        inode_data: &InodeData,
        full_info: &InodeMigrationInfo,
    ) -> io::Result<()> {
        let filename = CString::new(self.filename.clone())?;
        let parent_fd = self.parent.get().get_file()?;
        let st = statx(&parent_fd, Some(&filename))?;

        if st.st.st_dev != inode_data.ids.dev {
            return Err(other_io_error(format!(
                "Device ID differs: Expected {}, found {}",
                inode_data.ids.dev, st.st.st_dev
            )));
        }

        // Try to take a file handle from the migration info; if none is there, try to generate it
        // (but ignore errors, falling back to checking the inode ID).  We do really want to check
        // the file handle if possible, though, to detect inode ID reuse.
        let (fh, fh_ref) = if let Some(fh_ref) = full_info.file_handle.as_ref() {
            (None, Some(fh_ref))
        } else if let Ok(fh) = SerializableFileHandle::try_from(&inode_data.file_or_handle) {
            (Some(fh), None)
        } else {
            (None, None)
        };
        if let Some(fh) = fh_ref.or(fh.as_ref()) {
            // If we got a file handle for `inode_data`, failing to get it for `filename` probably
            // means it is a different inode.  Be cautious and return an error then.
            let actual_fh = FileHandle::from_name_at_fail_hard(&parent_fd, &filename)
                .err_context(|| "Failed to generate file handle")?;
            // Ignore mount ID: A file handle can be in two different mount IDs, but as long as it
            // is on the same device, it is still the same mount ID; and we have already checked
            // the device ID.
            fh.require_equal_without_mount_id(&actual_fh.into())
                .map_err(other_io_error)
        } else {
            // Cannot generate file handle?  Fall back to just the inode ID.
            if st.st.st_ino != inode_data.ids.ino {
                return Err(other_io_error(format!(
                    "Inode ID differs: Expected {}, found {}",
                    inode_data.ids.ino, st.st.st_ino
                )));
            }
            Ok(())
        }
    }
}

impl From<InodePath> for InodeLocation {
    fn from(path: InodePath) -> Self {
        InodeLocation::Path(path)
    }
}

impl Display for InodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parent = self.parent.get();
        let parent_mig_info_locked = parent.migration_info.lock().unwrap();
        if let Some(parent_mig_info) = parent_mig_info_locked.as_ref() {
            write!(f, "{}/{}", parent_mig_info.location, self.filename)
        } else {
            write!(f, "[inode {}]/{}", parent.inode, self.filename)
        }
    }
}

/// The `Constructor` is an `InodeMigrationInfoConstructor` that creates `InodeMigrationInfo` of
/// the `InodeMigrationInfo::Path` variant: It recurses through the filesystem (i.e. the shared
/// directory), matching up all inodes it finds with our inode store, and thus finds the parent
/// directory node and filename for every such inode.
impl<'a> Constructor<'a> {
    pub fn new(fs: &'a PassthroughFs, cancel: Arc<AtomicBool>) -> Self {
        Constructor { fs, cancel }
    }

    /**
     * Collect paths for all inodes in our inode store, during preserialization.
     *
     * Recurse from the root directory (the shared directory), constructing `InodeMigrationInfo`
     * data for every inode in the inode store.  This may take a long time, which is why it is done
     * in the preserialization phase.
     *
     * Cannot fail: Collecting inodes’ migration info is supposed to be a best-effort operation.
     * We can leave any and even all inodes’ migration info empty, then serialize them as invalid
     * inodes, and let the destination decide what to do based on its --migration-on-error setting.
     */
    pub fn execute(self) {
        // Only need to do something if we have a root node to recurse from; otherwise the
        // filesystem is not mounted and we do not need to do anything.
        if let Ok(root) = self.fs.inodes.get_strong(fuse::ROOT_ID) {
            self.recurse_from(root);
        }
    }

    /// Recurse from the given directory inode
    fn recurse_from(&self, root_ref: StrongInodeReference) {
        let mut dir_buf = vec![0u8; 1024];

        // We don't actually use recursion (to not exhaust the stack), but keep a list of
        // directories we still need to visit, and pop from it until it is empty and we're done
        let mut remaining_dirs = vec![root_ref];
        while let Some(inode_ref) = remaining_dirs.pop() {
            let dirfd = match inode_ref.get().open_file(
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                &self.fs.proc_self_fd,
            ) {
                Ok(fd) => fd,
                Err(err) => {
                    let dir_id = inode_ref.get().identify(&self.fs.proc_self_fd);
                    warn!("Failed to recurse into {dir_id}: {err}");
                    continue;
                }
            };

            // Read all directory entries, check them for matches in our inode store, and add any
            // directory to `remaining_dirs`
            loop {
                // Safe because we use nothing but this function on the FD
                let read_dir_result = unsafe { ReadDir::new_no_seek(&dirfd, dir_buf.as_mut()) };
                let mut entries = match read_dir_result {
                    Ok(entries) => entries,
                    Err(err) => {
                        let dir_id = inode_ref.get().identify(&self.fs.proc_self_fd);
                        warn!("Failed to read directory entries of {dir_id}: {err}");
                        break;
                    }
                };
                if entries.remaining() == 0 {
                    break;
                }

                while let Some(entry) = entries.next() {
                    if self.cancel.load(Ordering::Relaxed) {
                        return;
                    }

                    match self.discover(&inode_ref, &dirfd, entry.name) {
                        Ok(Some(entry_inode)) => {
                            // Add directories to visit to the list
                            remaining_dirs.push(entry_inode);
                        }
                        Ok(None) => (),
                        Err(err) => {
                            let dir_id = inode_ref.get().identify(&self.fs.proc_self_fd);
                            let name = entry.name.to_string_lossy();
                            warn!("Failed to discover entry {name} of {dir_id}: {err}");
                        }
                    }
                }
            }
        }
    }

    /// Check the given directory entry (parent + name) for matches in our inode store.  If we find
    /// any corresponding `InodeData` there, its `.migration_info` is set accordingly.
    /// For all directories (and directories only), return a strong reference to an inode in our
    /// store that can be used to recurse further.
    fn discover<F: AsRawFd>(
        &self,
        parent_reference: &StrongInodeReference,
        parent_fd: &F,
        name: &CStr,
    ) -> io::Result<Option<StrongInodeReference>> {
        let utf8_name = name.to_str().map_err(|err| {
            other_io_error(format!(
                "Cannot convert filename into UTF-8: {name:?}: {err}",
            ))
        })?;

        // Ignore these
        if utf8_name == "." || utf8_name == ".." {
            return Ok(None);
        }

        let path_fd = {
            let fd = self
                .fs
                .open_relative_to(parent_fd, name, libc::O_PATH, None)?;
            unsafe { File::from_raw_fd(fd) }
        };
        let stat = statx(&path_fd, None)?;
        let handle = self.fs.get_file_handle_opt(&path_fd, &stat)?;

        let ids = InodeIds {
            ino: stat.st.st_ino,
            dev: stat.st.st_dev,
            mnt_id: stat.mnt_id,
        };

        let is_directory = stat.st.st_mode & libc::S_IFMT == libc::S_IFDIR;

        if let Ok(inode_ref) = self.fs.inodes.claim_inode(handle.as_ref(), &ids) {
            let mig_info = InodeMigrationInfo::new_internal(
                &self.fs.cfg,
                InodePath {
                    parent: StrongInodeReference::clone(parent_reference),
                    filename: utf8_name.to_string(),
                },
                || {
                    Ok(match &handle {
                        Some(h) => h.into(),
                        None => FileHandle::from_fd_fail_hard(&path_fd)?.into(),
                    })
                },
            )?;

            *inode_ref.get().migration_info.lock().unwrap() = Some(mig_info);

            return Ok(is_directory.then_some(inode_ref));
        }

        // We did not find a matching entry in our inode store.  In case of non-directories, we are
        // done.
        if !is_directory {
            return Ok(None);
        }

        // However, in case of directories, we must create an entry, so we can return it.
        // (Our inode store may still have matching entries recursively downwards from this
        // directory.  Because every node is serialized referencing its parent, this directory
        // inode may end up being recursively referenced this way, we don't know yet.
        // In case there is no such entry, the refcount will eventually return to 0 before
        // `Self::execute()` returns, dropping it from the inode store again, so it will not
        // actually end up being serialized.)

        let file_or_handle = if let Some(h) = handle.as_ref() {
            FileOrHandle::Handle(self.fs.make_file_handle_openable(h)?)
        } else {
            FileOrHandle::File(self.fs.guest_fds.allocate(path_fd)?)
        };

        let mig_info = InodeMigrationInfo::new_internal(
            &self.fs.cfg,
            InodePath {
                parent: StrongInodeReference::clone(parent_reference),
                filename: utf8_name.to_string(),
            },
            || (&file_or_handle).try_into(),
        )?;

        let new_inode = InodeData {
            inode: self.fs.next_inode.fetch_add(1, Ordering::Relaxed),
            file_or_handle,
            refcount: AtomicU64::new(1),
            ids,
            mode: stat.st.st_mode,
            migration_info: Mutex::new(Some(mig_info)),
        };

        Ok(Some(self.fs.inodes.get_or_insert(new_inode)?))
    }
}
