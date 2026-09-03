// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Deserialization functionality (i.e. what happens in
 * `SerializableFileSystem::deserialize_and_apply()`): Take a plain vector of bytes, deserialize
 * it into our serializable structs ('serialized' module), and then apply the information from
 * there to a `PassthroughFs`, restoring the state from the migration source.
 */

use crate::fuse;
use crate::passthrough::device_state::preserialization::HandleMigrationInfo;
use crate::passthrough::device_state::serialized;
use crate::passthrough::file_handle::SerializableFileHandle;
use crate::passthrough::inode_store::{InodeData, InodeIds, StrongInodeReference};
use crate::passthrough::mount_fd::MountFd;
use crate::passthrough::stat::statx;
use crate::passthrough::util::{openat, printable_fd};
use crate::passthrough::{
    FileOrHandle, FileType, HandleData, HandleDataFile, InodeFileHandlesMode, MigrationOnError,
    NegotiationMode, PassthroughFs,
};
use crate::util::{other_io_error, ErrorContext, ResultErrorContext};
use std::collections::{BTreeMap, HashMap};
use std::convert::{TryFrom, TryInto};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

impl TryFrom<Vec<u8>> for serialized::PassthroughFs {
    type Error = io::Error;

    /// Root of deserialization: Turn plain bytes into a structured `serialized::PassthroughFs`
    fn try_from(serialized: Vec<u8>) -> io::Result<Self> {
        postcard::from_bytes(&serialized).map_err(other_io_error)
    }
}

impl serialized::PassthroughFsV1 {
    /**
     * Apply the state represented in `self: PassthroughFsV1` to `fs`.
     *
     * Restore the inode store, open handles, etc.
     */
    pub(super) fn apply(self, fs: &PassthroughFs) -> io::Result<()> {
        self.apply_with_mount_paths(fs, HashMap::new())
    }

    /**
     * Actual `apply()` implementation.
     *
     * Underlying implementation for both `PassthroughFsV1::apply()` and
     * `PassthroughFsV2::apply()`.  Migrating file handles requires a map of source mount IDs to
     * paths inside the shared directory, which is not present in `PassthroughFsV1`.  This function
     * takes this argument (`mount_paths`) explicitly, allowing `PassthroughFsV1::apply()` to pass
     * an empty map (not allowing migration of file handles), and `PassthroughFsV2::apply()` to
     * pass the map it got.
     */
    fn apply_with_mount_paths(
        mut self,
        fs: &PassthroughFs,
        mount_paths: HashMap<u64, String>,
    ) -> io::Result<()> {
        // Apply options as negotiated with the guest on the source
        self.negotiated_opts.apply(fs)?;

        fs.inodes.clear();

        let mount_fds: HashMap<u64, Arc<MountFd>> = if self.inodes.is_empty()
            || mount_paths.is_empty()
        {
            // No nodes or mount paths given?  We will not need this map, just create an empty one
            HashMap::new()
        } else {
            // Deserialize the root inode first; every path in `mount_paths` is relative to it, so
            // we must have it open to deserialize the mount FD map
            let Some((root_index, _)) = self
                .inodes
                .iter()
                .enumerate()
                .find(|(_, inode)| inode.id == fuse::ROOT_ID)
            else {
                return Err(other_io_error("Received no root node from the source"));
            };
            self.inodes
                .swap_remove(root_index)
                .deserialize_root_node(fs)?;

            let root_node = fs.inodes.get(fuse::ROOT_ID).unwrap();
            let root_node_file = root_node
                .get_file()
                .err_context(|| "Cannot open shared directory")?;

            mount_paths.into_iter().filter_map(|(mount_id, mount_path)| {
                match MountFd::new(fs.mount_fds.as_ref(), &root_node_file, &mount_path) {
                    Ok(mount_fd) => Some((mount_id, mount_fd)),
                    Err(err) => {
                        warn!(
                            "Failed to open path {mount_path} to open file handles for mount ID {mount_id}: {err}; \
                            will not be able to open inodes represented by file handles on that mount"
                        );
                        None
                    }
                }
            }).collect()
        };

        // Some inodes may depend on other inodes being deserialized before them, so trying to
        // deserialize them without their dependency being fulfilled will return `false` below,
        // asking to be deferred.  Therefore, it may take multiple iterations until we have
        // successfully deserialized all inodes.
        // (However serialized inodes are represented, it must be ensured that no loops occur in
        // such dependencies.)
        while !self.inodes.is_empty() {
            let mut i = 0;
            let mut processed_any = false;
            while i < self.inodes.len() {
                if self.inodes[i].deserialize_with_fs(fs, &mount_fds)? {
                    // All good
                    self.inodes.swap_remove(i);
                    processed_any = true;
                } else {
                    // Process this inode later (e.g. needs to resolve a reference to a parent node
                    // that has not yet been deserialized)
                    i += 1;
                }
            }

            if !processed_any {
                return Err(other_io_error(
                    "Unresolved references between serialized inodes",
                ));
            }
        }

        fs.next_inode.store(self.next_inode, Ordering::Relaxed);

        // Reconstruct handles (i.e., open those files)
        *fs.handles.write().unwrap() = BTreeMap::new();
        for handle in self.handles {
            handle.deserialize_with_fs(fs)?;
        }

        fs.next_handle.store(self.next_handle, Ordering::Relaxed);

        Ok(())
    }
}

impl serialized::PassthroughFsV2 {
    /**
     * Apply the state represented in `self: PassthroughFsV2` to `fs`.
     *
     * Restore the inode store, open handles, etc.
     */
    pub(super) fn apply(self, fs: &PassthroughFs) -> io::Result<()> {
        self.v1.apply_with_mount_paths(fs, self.mount_paths)
    }
}

impl serialized::NegotiatedOpts {
    /// Apply the options negotiated with the guest on the source side to `fs`'s configuration
    fn apply(self, fs: &PassthroughFs) -> io::Result<()> {
        if !fs.cfg.writeback && self.writeback {
            return Err(other_io_error(
                "Migration source wants writeback enabled, but it is disabled on the destination",
            ));
        }
        // Note the case of `fs.cfg.writeback && !self.writeback`, i.e. the user asked for it to be
        // enabled, but the migration source had it disabled: From a technical perspective, just
        // disabling it here is fine, because that is what happens (and what we want to happen)
        // when the guest does not support the flag (in which case there will already have been a
        // warning on INIT).  However, it is imaginable that the guest supports the flag, but it
        // was user-disabled on the source (and is user-enabled now): We can't distinguish this
        // case from the no-guest-support one, and disabling the flag is still the right thing to
        // do, because we would need to re-negotiate through INIT first before we can enable it.
        // Given that it would be strange for the user to use different configurations for source
        // and destination, do not print a warning either.
        fs.writeback.store(self.writeback, Ordering::Relaxed);

        if !fs.cfg.announce_submounts && self.announce_submounts {
            return Err(other_io_error(
                "Migration source wants announce-submounts enabled, but it is disabled on the \
                 destination",
            ));
        }
        // The comment from writeback applies here, too
        fs.announce_submounts
            .store(self.announce_submounts, Ordering::Relaxed);

        if fs.cfg.posix_acl == NegotiationMode::Never && self.posix_acl {
            return Err(other_io_error(
                "Migration source wants posix ACLs enabled, but it is disabled on the destination",
            ));
        }
        // The comment from writeback applies here, too
        fs.posix_acl.store(self.posix_acl, Ordering::Relaxed);

        fs.sup_group_extension
            .store(self.sup_group_extension, Ordering::Relaxed);

        Ok(())
    }
}

impl serialized::Inode {
    /// Deserialize this inode into `fs`'s inode store.  Return `Ok(true)` on success, `Err(_)` on
    /// error, and `Ok(false)` when there is a dependency to another inode that has not yet been
    /// deserialized, so deserialization should be re-attempted later.
    fn deserialize_with_fs(
        &self,
        fs: &PassthroughFs,
        mount_fds: &HashMap<u64, Arc<MountFd>>,
    ) -> io::Result<bool> {
        match &self.location {
            serialized::InodeLocation::RootNode => {
                if self.id != fuse::ROOT_ID {
                    return Err(other_io_error(format!(
                        "Node with non-root ID ({}) given as root node",
                        self.id
                    )));
                }
                self.deserialize_root_node(fs)?;
                Ok(true)
            }

            serialized::InodeLocation::Path { parent, filename } => {
                if self.id == fuse::ROOT_ID {
                    return Err(other_io_error(
                        "Refusing to use path given for root node".to_string(),
                    ));
                }

                let parent_ref = match fs.inodes.get(*parent) {
                    None => {
                        // `parent` not found yet, defer deserialization until it is present
                        return Ok(false);
                    }

                    Some(parent_data) => {
                        // Safe because the migration source guarantees that this reference is
                        // included in the parent node's refcount.  Once we have deserialized this
                        // inode, we must drop that reference, and moving it into
                        // `deserialize_path()` will achieve that.
                        unsafe { StrongInodeReference::new_no_increment(parent_data, &fs.inodes) }
                    }
                };

                let inode_data = self
                    .deserialize_path(fs, parent_ref, filename)
                    .or_else(|err| self.deserialize_invalid_inode(fs, err))?;

                let inode_data = match self.check_file_handle(&inode_data) {
                    Ok(()) => inode_data,
                    Err(err) => self.deserialize_invalid_inode(fs, err)?,
                };

                fs.inodes.new_inode(inode_data)?;
                Ok(true)
            }

            serialized::InodeLocation::Invalid => {
                let err = io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Migration source has lost inode {}", self.id),
                );
                let inode_data = self.deserialize_invalid_inode(fs, err)?;
                fs.inodes.new_inode(inode_data)?;
                Ok(true)
            }

            serialized::InodeLocation::FullPath { filename } => {
                if self.id == fuse::ROOT_ID {
                    return Err(other_io_error(
                        "Refusing to use path given for root node".to_string(),
                    ));
                }

                let Ok(shared_dir) = fs.inodes.get_strong(fuse::ROOT_ID) else {
                    // No root node?  Defer until we have it.
                    return Ok(false);
                };

                let inode_data = self
                    .deserialize_path(fs, shared_dir, filename)
                    .or_else(|err| self.deserialize_invalid_inode(fs, err))?;

                fs.inodes.new_inode(inode_data)?;
                Ok(true)
            }

            serialized::InodeLocation::FileHandle { handle } => {
                if self.id == fuse::ROOT_ID {
                    return Err(other_io_error(
                        "Refusing to use file handle given for root node".to_string(),
                    ));
                }

                let inode_data = self
                    .deserialize_file_handle(fs, mount_fds, handle)
                    .or_else(|err| self.deserialize_invalid_inode(fs, err))?;

                fs.inodes.new_inode(inode_data)?;
                Ok(true)
            }
        }
    }

    /**
     * “Deserialize” the root node.
     *
     * We will not get any information about it from the source because its location is always
     * defined on the command line, so all we do is open that location and apply the refcount the
     * source had for it.
     *
     * `self.id` must be the FUSE root inode ID.
     */
    fn deserialize_root_node(&self, fs: &PassthroughFs) -> io::Result<()> {
        assert!(self.id == fuse::ROOT_ID);
        if !matches!(&self.location, serialized::InodeLocation::RootNode) {
            return Err(other_io_error(
                "Root node has not been serialized as root node",
            ));
        }

        // We open the root node ourselves (from the configuration the user gave us)...
        fs.open_root_node()?;
        // ...and only take the refcount from the source, ignoring filename and parent information.
        // Note that we must not call `fs.open_root_node()` before we have the correct refcount, or
        // deserializing child nodes (which drops one reference each) would quickly reduce the
        // refcount below 0.
        let root_data = fs.inodes.get(fuse::ROOT_ID).unwrap();
        root_data.refcount.store(self.refcount, Ordering::Relaxed);

        // For the root node, a non-matching file handle is always a hard error.  We cannot
        // deserialize the root node as an invalid node.
        self.check_file_handle(&root_data)?;

        Ok(())
    }

    /// Helper function for `deserialize_with_fs()`: Try to locate an inode based on its parent
    /// directory and its filename.
    /// Takes ownership of the `parent` strong reference and drops it.
    /// On success, returns `InodeData` to add to `fs.inodes`.
    fn deserialize_path(
        &self,
        fs: &PassthroughFs,
        parent: StrongInodeReference,
        filename: &str,
    ) -> io::Result<InodeData> {
        let parent_fd = parent.get().get_file()?;
        let fd = openat(
            &parent_fd,
            filename,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
        .map_err(|err| {
            let pfd = printable_fd(&parent_fd, Some(&fs.proc_self_fd));
            io::Error::new(
                err.kind(),
                format!(
                    "Opening {pfd}{}{filename}: {err}",
                    if pfd.ends_with('/') { "" } else { "/" },
                ),
            )
        })?;

        let st = statx(&fd, None)?;
        let handle = fs.get_file_handle_opt(&fd, &st)?;

        let file_or_handle = if let Some(h) = handle.as_ref() {
            FileOrHandle::Handle(fs.make_file_handle_openable(h)?)
        } else {
            FileOrHandle::File(fs.guest_fds.allocate(fd)?)
        };

        Ok(InodeData {
            inode: self.id,
            file_or_handle,
            refcount: AtomicU64::new(self.refcount),
            ids: InodeIds {
                ino: st.st.st_ino,
                dev: st.st.st_dev,
                mnt_id: st.mnt_id,
            },
            mode: st.st.st_mode,
            migration_info: Mutex::new(None),
        })
    }

    /// Helper function for `deserialize_with_fs()`: Handle invalid inodes, i.e. ones that cannot
    /// be located.
    /// Depending on the configuration, they either cause a hard error, or should be added as
    /// explicitly invalid inodes to `fs.inodes` (in which case their `InodeData` is returned).
    fn deserialize_invalid_inode(
        &self,
        fs: &PassthroughFs,
        err: io::Error,
    ) -> io::Result<InodeData> {
        match fs.cfg.migration_on_error {
            MigrationOnError::Abort => Err(err.context(format!("Inode {}", self.id))),
            MigrationOnError::GuestError => {
                warn!("Invalid inode {} indexed: {err}", self.id);
                Ok(InodeData {
                    inode: self.id,
                    file_or_handle: FileOrHandle::Invalid(Arc::new(err)),
                    refcount: AtomicU64::new(self.refcount),
                    ids: Default::default(),
                    mode: Default::default(),
                    migration_info: Default::default(),
                })
            }
        }
    }

    /// If the source sent us a reference file handle, check it against `inode_data`'s file handle
    fn check_file_handle(&self, inode_data: &InodeData) -> io::Result<()> {
        let Some(ref_fh) = &self.file_handle else {
            return Ok(());
        };

        let is_fh: SerializableFileHandle = (&inode_data.file_or_handle).try_into()?;
        // Disregard the mount ID, this may be a different host, so the mount ID may differ
        is_fh.require_equal_without_mount_id(ref_fh).map_err(|err| {
            other_io_error(format!(
                "Inode {} is not the same inode as in the migration source: {err}",
                self.id
            ))
        })
    }

    /**
     * Helper function for `deserialize_with_fs()`: Handle file handles.
     *
     * Get a mount FD for the given file handle, turning it into an
     * [`OpenableFileHandle`](crate::passthrough::file_handle::OpenableFileHandle).  Then get the
     * [`InodeIds`] we need to complete the [`InodeData`] object, and return that.
     */
    fn deserialize_file_handle(
        &self,
        fs: &PassthroughFs,
        mount_fds: &HashMap<u64, Arc<MountFd>>,
        handle: &SerializableFileHandle,
    ) -> io::Result<InodeData> {
        let source_mount_id = handle.mount_id();
        let mfd = mount_fds
            .get(&source_mount_id)
            .ok_or_else(|| other_io_error(format!("Unknown mount ID {source_mount_id}")))?;
        let ofh = handle.to_openable(Arc::clone(mfd))?;

        let fd = ofh
            .open(libc::O_PATH)
            .err_context(|| "Opening file handle")?;
        let st = statx(&fd, None).err_context(|| "stat")?;

        let file_or_handle = match fs.cfg.inode_file_handles {
            InodeFileHandlesMode::Never => FileOrHandle::File(fs.guest_fds.allocate(fd)?),
            InodeFileHandlesMode::Mandatory | InodeFileHandlesMode::Prefer => {
                FileOrHandle::Handle(ofh)
            }
        };

        Ok(InodeData {
            inode: self.id,
            file_or_handle,
            refcount: AtomicU64::new(self.refcount),
            ids: InodeIds {
                ino: st.st.st_ino,
                dev: st.st.st_dev,
                mnt_id: st.mnt_id,
            },
            mode: st.st.st_mode,
            migration_info: Mutex::new(None),
        })
    }
}

impl serialized::Handle {
    /// Deserialize this handle into `fs`'s handle map.
    fn deserialize_with_fs(&self, fs: &PassthroughFs) -> io::Result<()> {
        let inode = fs
            .inodes
            .get(self.inode)
            .ok_or_else(|| other_io_error(format!("Inode {} not found", self.inode)))?;

        let (file, migration_info) = match self.source {
            serialized::HandleSource::OpenInode { flags } => {
                let handle_data_file = match inode
                    .open_file(flags, &fs.proc_self_fd)
                    .and_then(|f| f.into_file())
                {
                    Ok(f) => fs.guest_fds.allocate(f).map(Into::into),
                    Err(err) => {
                        let error_msg = if let Ok(path) = inode.get_path(&fs.proc_self_fd) {
                            let p = path.as_c_str().to_string_lossy();
                            format!(
                                "Opening inode {} ({p}) as handle {}: {err}",
                                self.inode, self.id
                            )
                        } else {
                            format!("Opening inode {} as handle {}: {err}", self.inode, self.id)
                        };
                        Err(io::Error::new(err.kind(), error_msg))
                    }
                };

                let handle_data_file = match handle_data_file {
                    Ok(hdf) => hdf,
                    Err(err) => match fs.cfg.migration_on_error {
                        MigrationOnError::Abort => return Err(err),
                        MigrationOnError::GuestError => {
                            warn!("Invalid handle {} is open in guest: {err}", self.id);
                            HandleDataFile::Invalid(Arc::new(err))
                        }
                    },
                };

                let migration_info = HandleMigrationInfo::OpenInode { flags };
                (handle_data_file, migration_info)
            }
        };

        let handle_data = HandleData {
            inode: self.inode,
            file,
            file_type: FileType::from_mode(inode.mode),
            migration_info,
        };
        fs.handles
            .write()
            .unwrap()
            .insert(self.id, Arc::new(handle_data));
        Ok(())
    }
}
