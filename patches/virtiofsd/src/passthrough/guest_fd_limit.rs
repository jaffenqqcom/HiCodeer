// Copyright 2024 Red Hat, Inc. All rights reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

//! Allow limiting the number of file descriptors we allocate for the guest.
//!
//! Any process only has a limited number of file descriptor slots available for use, and besides
//! allocating FDs for the guest, virtiofsd also needs to be able to create file descriptors for
//! internal use.  By limiting the number we will allocate for the guest, we can ensure there are
//! always free slots open for such internal use.

use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Basically just a semaphore, but specifically for limiting guest FD use.
///
/// Wraps plain `File`s to create `GuestFile`s that count against the guest FD limit until dropped.
pub(crate) struct GuestFdSemaphore {
    /// Initial (overall) limit.
    initial: u64,

    /// How many allocations are still available before exhausting the limit.
    available: AtomicU64,

    /// Whether an error about no remaining FD slots has been logged.
    ///
    /// Further errors will then be suppressed.
    error_logged: AtomicBool,
}

/// Returned by `GuestFdSemaphore::allocate()`, will release the slot when dropped.
pub struct GuestFile {
    /// Contained FD.
    file: File,

    /// Semaphore reference.
    sem: Arc<GuestFdSemaphore>,
}

impl GuestFdSemaphore {
    /// Create a new instance with the given `limit`.
    pub fn new(limit: u64) -> Self {
        GuestFdSemaphore {
            initial: limit,
            available: limit.into(),
            error_logged: false.into(),
        }
    }

    /// Put the given file into a free slot.
    ///
    /// The slot is released by dropping the returned `GuestFile`.
    pub fn allocate(self: &Arc<Self>, file: File) -> io::Result<GuestFile> {
        self.available.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |previous| previous.checked_sub(1),
        ).map_err(|_| {
            if !self.error_logged.fetch_or(true, Ordering::Relaxed) {
                error!(
                    "No more file descriptors available to the guest (0 available out of {} initially), \
                    consider increasing the --rlimit-nofile value",
                    self.initial,
                );
            }

            // Since this error is likely returned to the guest (and not logged), prefer an
            // error with a reasonable errno number over a useful error message.
            io::Error::from_raw_os_error(libc::ENFILE)
        })?;

        Ok(GuestFile {
            file,
            sem: Arc::clone(self),
        })
    }

    /// Release one slot.
    ///
    /// Do not use directly, just drop [`GuestFile`].
    fn release(&self) {
        let increased_to = self
            .available
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .unwrap_or_else(|| panic!("FD semaphore overflow"));
        debug_assert!(increased_to <= self.initial);
    }
}

impl GuestFile {
    /// Get the inner file.
    pub fn get_file(&self) -> &File {
        &self.file
    }
}

impl AsRawFd for GuestFile {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Drop for GuestFile {
    fn drop(&mut self) {
        self.sem.release();
    }
}
