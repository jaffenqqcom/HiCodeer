// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Serialization functionality (i.e. what happens in `SerializableFileSystem::serialize()`): Take
 * information that we have collected during preserialization and turn it into actually
 * serializable structs ('serialized' module), which are then turned into a plain vector of bytes.
 */

use crate::fuse;
use crate::passthrough::device_state::preserialization::{
    self, HandleMigrationInfo, InodeMigrationInfo,
};
use crate::passthrough::device_state::serialized;
use crate::passthrough::inode_store::InodeData;
use crate::passthrough::mount_fd::MountFds;
use crate::passthrough::util::relative_path;
use crate::passthrough::{Handle, HandleData, MigrationMode, PassthroughFs};
use crate::util::{other_io_error, ResultErrorContext};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::ffi::CString;
use std::io;
use std::sync::atomic::Ordering;

/**
 * Helper structure to generate the mount FD map.
 *
 * The mount FD map maps the source’s mount IDs to paths in the shared directory, which the
 * destination instance can open as mount FDs to make use of file handles created on the source.
 */
struct MountPathsBuilder<'a> {
    /// Reference to [`PassthroughFs.mount_fds`](`PassthroughFs#structfield.mount_fds`)
    mount_fds: &'a MountFds,
    /// Path of the shared directory
    shared_dir_path: CString,
}

impl TryFrom<serialized::PassthroughFs> for Vec<u8> {
    type Error = io::Error;

    /// Root of serialization: Turn the final `serialized::PassthroughFs` struct into plain bytes
    fn try_from(state: serialized::PassthroughFs) -> io::Result<Self> {
        postcard::to_stdvec(&state).map_err(other_io_error)
    }
}

impl From<&PassthroughFs> for serialized::PassthroughFsV2 {
    /// Serialize `fs`, assuming it has been prepared for serialization (i.e. all inodes must have
    /// their migration info set)
    fn from(fs: &PassthroughFs) -> Self {
        let handles_map = fs.handles.read().unwrap();

        let inodes: Vec<serialized::Inode> = fs.inodes.iter().map(|inode| {
            inode
                .as_ref()
                .as_serialized(fs)
                .unwrap_or_else(|err| {
                    warn!(
                        "Failed to serialize inode {} (st_dev={}, mnt_id={}, st_ino={}): {err}; marking as invalid",
                        inode.inode, inode.ids.dev, inode.ids.mnt_id, inode.ids.ino
                    );
                    serialized::Inode {
                        id: inode.inode,
                        refcount: inode.refcount.load(Ordering::Relaxed),
                        location: serialized::InodeLocation::Invalid,
                        file_handle: None,
                    }
                })
        }).collect();

        let mount_paths = if fs.cfg.migration_mode == MigrationMode::FileHandles {
            match MountPathsBuilder::new(fs) {
                Ok(mpb) => mpb.build(inodes.iter()),
                Err(err) => {
                    warn!(
                        "Cannot collect mount points: {err}; will not be able to migrate any inodes"
                    );
                    HashMap::new()
                }
            }
        } else {
            // No need for mount paths outside of file-handles migration mode
            HashMap::new()
        };

        let handles = handles_map
            .iter()
            .map(|(handle, data)| (*handle, data.as_ref()).into())
            .collect();

        serialized::PassthroughFsV2 {
            v1: serialized::PassthroughFsV1 {
                inodes,
                next_inode: fs.next_inode.load(Ordering::Relaxed),

                handles,
                next_handle: fs.next_handle.load(Ordering::Relaxed),

                negotiated_opts: fs.into(),
            },

            mount_paths,
        }
    }
}

impl From<&PassthroughFs> for serialized::NegotiatedOpts {
    /// Serialize the options we have negotiated with the guest
    fn from(fs: &PassthroughFs) -> Self {
        serialized::NegotiatedOpts {
            writeback: fs.writeback.load(Ordering::Relaxed),
            announce_submounts: fs.announce_submounts.load(Ordering::Relaxed),
            posix_acl: fs.posix_acl.load(Ordering::Relaxed),
            sup_group_extension: fs.sup_group_extension.load(Ordering::Relaxed),
        }
    }
}

impl InodeData {
    /// Serialize an inode, which requires that its `migration_info` is set
    fn as_serialized(&self, fs: &PassthroughFs) -> io::Result<serialized::Inode> {
        let id = self.inode;
        let refcount = self.refcount.load(Ordering::Relaxed);

        // Note that we do not special-case invalid inodes here (`self.file_or_handle ==
        // FileOrHandle::Invalid(_)`), i.e. inodes that this instance failed to find on a prior
        // incoming migration.  We do not expect them to have migration info (we could not open
        // them, so we should not know where to find them), but if we do, there must be a reason
        // for it, so we might as well forward it to our destination.

        let migration_info_locked = self.migration_info.lock().unwrap();
        let migration_info = migration_info_locked
            .as_ref()
            .ok_or_else(|| other_io_error("Failed to reconstruct inode location"))?;

        // The root node (and only the root node) must have its special kind of placeholder info
        assert_eq!(
            (id == fuse::ROOT_ID),
            matches!(
                migration_info.location,
                preserialization::InodeLocation::RootNode
            )
        );

        // Serialize the information that tells the destination how to find this inode
        let location = migration_info.as_serialized()?;

        let file_handle = if fs.cfg.migration_verify_handles {
            // We could construct the file handle now, but we don't want to do I/O here.  It should
            // have been prepared in the preserialization phase.  If it is not, that's an internal
            // programming error.
            let handle = migration_info
                .file_handle
                .as_ref()
                .ok_or_else(|| other_io_error("No prepared file handle found"))?;
            Some(handle.clone())
        } else {
            None
        };

        Ok(serialized::Inode {
            id,
            refcount,
            location,
            file_handle,
        })
    }
}

impl InodeMigrationInfo {
    /// Helper for serializing inodes: Turn their prepared `migration_info` into a
    /// `serialized::InodeLocation`
    fn as_serialized(&self) -> io::Result<serialized::InodeLocation> {
        Ok(match &self.location {
            preserialization::InodeLocation::RootNode => serialized::InodeLocation::RootNode,

            preserialization::InodeLocation::Path(preserialization::find_paths::InodePath {
                parent,
                filename,
            }) => {
                // Safe: We serialize everything before we will drop the serialized state (the
                // inode store), so the strong refcount in there will outlive this weak reference
                // (which means that the ID we get will remain valid until everything is
                // serialized, i.e. that parent node will be part of the serialized state)
                let parent = unsafe { parent.get_raw() };
                let filename = filename.clone();

                serialized::InodeLocation::Path { parent, filename }
            }

            preserialization::InodeLocation::FileHandle(
                preserialization::file_handles::FileHandle { handle },
            ) => serialized::InodeLocation::FileHandle {
                handle: handle.clone(),
            },
        })
    }
}

impl From<(Handle, &HandleData)> for serialized::Handle {
    /// Serialize a handle
    fn from(handle: (Handle, &HandleData)) -> Self {
        // Note that we will happily process invalid handles here (`handle.1.file ==
        // HandleDataFile::Invalid(_)`), i.e. handles that this instance failed to open on a prior
        // incoming migration.  A handle is identified by the inode to which it belongs, and
        // instructions on how to open that inode (e.g. `open()` flags).  If this instance failed
        // to open the inode in this way (on in-migration), that does not prevent us from
        // forwarding the same information to the next destination (on out-migration), and thus
        // allow it to re-try.

        let source = (&handle.1.migration_info).into();
        serialized::Handle {
            id: handle.0,
            inode: handle.1.inode,
            source,
        }
    }
}

impl From<&HandleMigrationInfo> for serialized::HandleSource {
    /// Helper for serializing handles: Turn their prepared `migration_info` into a
    /// `serialized::HandleSource`
    fn from(repr: &HandleMigrationInfo) -> Self {
        match repr {
            HandleMigrationInfo::OpenInode { flags } => {
                serialized::HandleSource::OpenInode { flags: *flags }
            }
        }
    }
}

impl<'a> MountPathsBuilder<'a> {
    /**
     * Create a new `MountPathsBuilder` for `fs`.
     *
     * `fs` is needed to:
     * - get the shared directory’s (root node’s) path,
     * - get a reference to [`fs.mount_fds`](PassthroughFs#structfield.mount_fds), which is
     *   basically the map we want to serialize (except it maps to FDs, and we want to map to
     *   paths).
     */
    fn new(fs: &'a PassthroughFs) -> io::Result<Self> {
        // No reason to use `MountPathsBuilder` in any other migration mode
        assert!(fs.cfg.migration_mode == MigrationMode::FileHandles);

        // With the migration mode is set to "file-handles", `PassthroughFs::new()` is expected to
        // create `mount_fds`, so it should be present
        let Some(mount_fds) = fs.mount_fds.as_ref() else {
            return Err(other_io_error("No mount FD map found"));
        };

        let Some(root_node) = fs.inodes.get(fuse::ROOT_ID) else {
            if fs.inodes.is_empty() {
                // No inodes at all, the FS is probably not mounted.  There will not be any
                // serialized inodes, so `build()` will have nothing to do, and we can just keep
                // `shared_dir_path` empty.
                return Ok(MountPathsBuilder {
                    mount_fds,
                    shared_dir_path: CString::new("").unwrap(),
                });
            } else {
                return Err(other_io_error(
                    "Root node (shared directory) not in inode store",
                ));
            }
        };

        let shared_dir_path = root_node
            .get_path(&fs.proc_self_fd)
            .map_err(io::Error::from)
            .err_context(|| "Failed to get shared directory path")?;

        Ok(MountPathsBuilder {
            mount_fds,
            shared_dir_path: shared_dir_path.to_owned(),
        })
    }

    /**
     * Internal use: Get `mnt_id`’s path in the shared directory.
     *
     * Return the path of an inode relative to the shared directory that is on the mount `mnt_id`.
     */
    fn get_mount_path(&mut self, mnt_id: u64) -> io::Result<String> {
        let path = self
            .mount_fds
            .get_mount_root(mnt_id)
            .map_err(other_io_error)?;
        // Clone `path` so we can still use it in the error message
        let c_path = CString::new(path.clone())
            .map_err(|_| other_io_error(format!("Cannot convert path ({path}) to C string")))?;
        let c_relative_path = match relative_path(&c_path, &self.shared_dir_path) {
            Ok(rp) => rp,
            // Error means the path is outside of the shared directory.  Return the shared
            // directory itself, then.
            Err(_) => return Ok(".".to_string()),
        };
        let relative_path = c_relative_path.to_str().map_err(|_| {
            other_io_error(format!(
                "Path {c_relative_path:?} cannot be converted to UTF-8"
            ))
        })?;

        if relative_path.is_empty() {
            Ok(".".to_string())
        } else {
            Ok(relative_path.to_string())
        }
    }

    /**
     * Given an iterator over all serialized inodes, construct the map.
     *
     * Iterate over all serialized inodes, and create a mount path map that has an entry for every
     * mount ID referenced by any file handle in any of the serialized inodes.
     */
    fn build<'b, I: Iterator<Item = &'b serialized::Inode>>(
        mut self,
        iter: I,
    ) -> HashMap<u64, String> {
        let mount_ids: HashSet<u64> = iter
            .filter_map(|si| match &si.location {
                serialized::InodeLocation::FileHandle { handle } => Some(handle.mount_id()),
                _ => None,
            })
            .collect();

        let mut map = HashMap::new();
        for mount_id in mount_ids {
            match self.get_mount_path(mount_id) {
                Ok(path) => {
                    map.insert(mount_id, path);
                }
                Err(err) => warn!(
                    "Failed to get mount ID {mount_id}'s root: {err}; \
                    will not be able to migrate inodes on this filesystem"
                ),
            }
        }

        map
    }
}
