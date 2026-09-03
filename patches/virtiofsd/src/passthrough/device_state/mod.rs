// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Module for migrating our internal FS state (i.e. serializing and deserializing it), with the
 * following submodules:
 * - serialized: Serialized data structures
 * - preserialization: Structures and functionality for preparing for migration (serialization),
 *   i.e. define and construct the precursors to the eventually serialized information that are
 *   stored alongside the associated inodes and handles they describe
 * - serialization: Functionality for serializing
 * - deserialization: Functionality for deserializing
 */

mod deserialization;
pub(super) mod preserialization;
mod serialization;
mod serialized;

use crate::filesystem::SerializableFileSystem;
use crate::passthrough::{MigrationMode, PassthroughFs};
use preserialization::proc_paths::{self, ConfirmPaths, ImplicitPathCheck};
use preserialization::{file_handles, find_paths};
use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Adds serialization (migration) capabilities to `PassthroughFs`
impl SerializableFileSystem for PassthroughFs {
    fn prepare_serialization(&self, cancel: Arc<AtomicBool>) {
        self.inodes.clear_migration_info();

        // Set this so the filesystem code knows that every node is supposed to have up-to-date
        // migration information.  For example, nodes that are created after they would have been
        // visited by the reconstructor below will not get migration info, unless the general
        // filesystem code makes an effort to set it (when the node is created).
        self.track_migration_info.store(true, Ordering::Relaxed);

        match self.cfg.migration_mode {
            MigrationMode::FindPaths => {
                // Create the reconstructor (which reconstructs parent+filename information for
                // each node in our inode store), and run it.  Try the proc_paths module first, if
                // that advises us to fall back, try find_paths second.
                if proc_paths::Constructor::new(self, Arc::clone(&cancel)).execute() {
                    warn!("Falling back to iterating through the shared directory to reconstruct paths for migration");
                    find_paths::Constructor::new(self, Arc::clone(&cancel)).execute();
                }
            }

            MigrationMode::FileHandles => {
                // Get file handles for each node in our inode store
                file_handles::Constructor::new(self, Arc::clone(&cancel)).execute();
            }
        }

        // Check reconstructed paths once.  This is to rule out TOCTTOU problems, specifically the
        // following:
        // 1. Our preserialization constructor above finds a path for some inode
        // 2. That inode is concurrently unlinked by the guest, so its inode migration info is
        //    invalidated
        // 3. The preserialization constructor then constructs an inode migration info with the
        //    path it found (that is now wrong), adding it to the inode
        // To fix this problem, preserialization must re-check each path after putting it into the
        // `InodeData.migration_info` field.  Do that by running the proc_paths checker.
        let checker = ImplicitPathCheck::new(self, cancel);
        checker.check_paths();
    }

    fn serialize(&self, mut state_pipe: File) -> io::Result<()> {
        self.track_migration_info.store(false, Ordering::Relaxed);

        if self.cfg.migration_confirm_paths {
            let checker = ConfirmPaths::new(self);
            if let Err(err) = checker.confirm_paths() {
                self.inodes.clear_migration_info();
                return Err(err);
            }
        }

        let state = serialized::PassthroughFs::V2(self.into());
        self.inodes.clear_migration_info();
        let serialized: Vec<u8> = state.try_into()?;
        state_pipe.write_all(&serialized)?;
        Ok(())
    }

    fn deserialize_and_apply(&self, mut state_pipe: File) -> io::Result<()> {
        let mut serialized: Vec<u8> = Vec::new();
        state_pipe.read_to_end(&mut serialized)?;
        match serialized::PassthroughFs::try_from(serialized)? {
            serialized::PassthroughFs::V1(state) => state.apply(self)?,
            serialized::PassthroughFs::V2(state) => state.apply(self)?,
        };
        Ok(())
    }
}
