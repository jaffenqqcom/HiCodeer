// SPDX-License-Identifier: BSD-3-Clause

use crate::soft_idmap::{HostGid, HostUid, Id};
use bitflags::bitflags;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Error, Result};
use std::os::unix::io::{AsRawFd, BorrowedFd, RawFd};
use std::os::unix::prelude::FromRawFd;

// A helper function that check the return value of a C function call
// and wraps it in a `Result` type, returning the `errno` code as `Err`.
fn check_retval<T: From<i8> + PartialEq>(t: T) -> Result<T> {
    if t == T::from(-1_i8) {
        Err(Error::last_os_error())
    } else {
        Ok(t)
    }
}

/// Simple object to collect basic facts about the OS,
/// such as available syscalls.
pub struct OsFacts {
    pub has_openat2: bool,
}

#[allow(clippy::new_without_default)]
impl OsFacts {
    /// This object should only be constructed using new.
    #[must_use]
    pub fn new() -> Self {
        // On OHOS the sandbox seccomp policy rejects `openat2` (SYS_openat2)
        // with SIGSYS, which would kill the whole process. Skip the probe and
        // fall back to plain `openat`; the feature is only a performance
        // optimisation and `openat` is always available.
        #[cfg(target_env = "ohos")]
        let has_openat2 = false;

        #[cfg(not(target_env = "ohos"))]
        let has_openat2 = {
            // Checking for `openat2()` since it first appeared in Linux 5.6.
            // SAFETY: all-zero byte-pattern is a valid `libc::open_how`
            let how: libc::open_how = unsafe { std::mem::zeroed() };
            let cwd = CString::new(".").unwrap();
            // SAFETY: `cwd.as_ptr()` points to a valid NUL-terminated string,
            // and the `how` pointer is a valid pointer to an `open_how` struct.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    libc::AT_FDCWD,
                    cwd.as_ptr(),
                    std::ptr::addr_of!(how),
                    std::mem::size_of::<libc::open_how>(),
                )
            };

            let has = fd >= 0;
            if has {
                // SAFETY: `fd` is an open file descriptor
                unsafe {
                    libc::close(fd as libc::c_int);
                }
            }
            has
        };

        Self { has_openat2 }
    }
}

/// Safe wrapper for `mount(2)`
///
/// # Errors
///
/// Will return `Err(errno)` if `mount(2)` fails.
/// Each filesystem type may have its own special errors and its own special behavior,
/// see `mount(2)` and the linux source kernel for details.
///
/// # Panics
///
/// This function panics if the strings `source`, `target` or `fstype` contain an internal 0 byte.
pub fn mount(source: Option<&str>, target: &str, fstype: Option<&str>, flags: u64) -> Result<()> {
    let source = CString::new(source.unwrap_or("")).unwrap();
    let source = source.as_ptr();

    let target = CString::new(target).unwrap();
    let target = target.as_ptr();

    let fstype = CString::new(fstype.unwrap_or("")).unwrap();
    let fstype = fstype.as_ptr();

    // Safety: `source`, `target` or `fstype` are a valid C string pointers
    check_retval(unsafe { libc::mount(source, target, fstype, flags, std::ptr::null()) })?;
    Ok(())
}

/// Safe wrapper for `umount2(2)`
///
/// # Errors
///
/// Will return `Err(errno)` if `umount2(2)` fails.
/// Each filesystem type may have its own special errors and its own special behavior,
/// see `umount2(2)` and the linux source kernel for details.
///
/// # Panics
///
/// This function panics if the strings `target` contains an internal 0 byte.
pub fn umount2(target: &str, flags: i32) -> Result<()> {
    let target = CString::new(target).unwrap();
    let target = target.as_ptr();

    // Safety: `target` is a valid C string pointer
    check_retval(unsafe { libc::umount2(target, flags) })?;
    Ok(())
}

/// Safe wrapper for `fchdir(2)`
///
/// # Errors
///
/// Will return `Err(errno)` if `fchdir(2)` fails.
/// Each filesystem type may have its own special errors, see `fchdir(2)` for details.
pub fn fchdir(fd: RawFd) -> Result<()> {
    check_retval(unsafe { libc::fchdir(fd) })?;
    Ok(())
}

/// Safe wrapper for `fchmod(2)`
///
/// # Errors
///
/// Will return `Err(errno)` if `fchmod(2)` fails.
/// Each filesystem type may have its own special errors, see `fchmod(2)` for details.
pub fn fchmod(fd: RawFd, mode: libc::mode_t) -> Result<()> {
    check_retval(unsafe { libc::fchmod(fd, mode) })?;
    Ok(())
}

/// Safe wrapper for `fchmodat(2)`
///
/// # Errors
///
/// Will return `Err(errno)` if `fchmodat(2)` fails.
/// Each filesystem type may have its own special errors, see `fchmodat(2)` for details.
pub fn fchmodat(dirfd: RawFd, pathname: String, mode: libc::mode_t, flags: i32) -> Result<()> {
    let pathname =
        CString::new(pathname).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let pathname = pathname.as_ptr();

    check_retval(unsafe { libc::fchmodat(dirfd, pathname, mode, flags) })?;
    Ok(())
}

/// Safe wrapper for `umask(2)`
pub fn umask(mask: u32) -> u32 {
    // SAFETY: this call doesn't modify any memory and there is no need
    // to check the return value because this system call always succeeds.
    unsafe { libc::umask(mask) }
}

/// An RAII implementation of a scoped file mode creation mask (umask), it set the
/// new umask. When this structure is dropped (falls out of scope), it set the previous
/// value of the mask.
pub struct ScopedUmask {
    umask: libc::mode_t,
}

impl ScopedUmask {
    pub fn new(new_umask: u32) -> Self {
        Self {
            umask: umask(new_umask),
        }
    }
}

impl Drop for ScopedUmask {
    fn drop(&mut self) {
        umask(self.umask);
    }
}

/// Safe wrapper around `openat(2)`.
///
/// # Errors
///
/// Will return `Err(errno)` if `openat(2)` fails,
/// see `openat(2)` for details.
pub fn openat(dir: &impl AsRawFd, pathname: &CStr, flags: i32, mode: Option<u32>) -> Result<RawFd> {
    let mode = u64::from(mode.unwrap_or(0));

    // SAFETY: `pathname` points to a valid NUL-terminated string.
    // However, the caller must ensure that `dir` can provide a valid file descriptor.
    check_retval(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            pathname.as_ptr(),
            flags as libc::c_int,
            mode,
        )
    })
}

/// Safe wrapper around `open_tree(2)`.
///
/// # Errors
///
/// Will return `Err(errno)` if `open_tree(2)` fails,
/// see `open_tree(2)` for details.
pub fn open_tree(dir: Option<&dyn AsRawFd>, pathname: &CStr, flags: u32) -> Result<RawFd> {
    let fd = dir.map(AsRawFd::as_raw_fd).unwrap_or(libc::AT_FDCWD);

    // SAFETY: `libc::syscall` is called with the correct arity and types.
    // `pathname` points to a valid NUL-terminated string.
    // `fd` is guaranteed to be a valid file descriptor or AT_FDCWD.
    check_retval(
        unsafe { libc::syscall(libc::SYS_open_tree, fd, pathname.as_ptr(), flags) } as RawFd,
    )
}

// libc does not define this on musl.
// https://github.com/rust-lang/libc/pull/3534#discussion_r2573036200
pub const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x00000004;

/// Safe wrapper around `move_mount(2)`.
///
/// # Errors
///
/// Will return `Err(errno)` if `move_mount(2)` fails,
/// see `move_mount(2)` for details.
pub fn move_mount(
    from_dir: Option<&dyn AsRawFd>,
    from_path: &CStr,
    to_dir: Option<&dyn AsRawFd>,
    to_path: &CStr,
    flags: u32,
) -> Result<()> {
    let from_fd = from_dir.map(AsRawFd::as_raw_fd).unwrap_or(libc::AT_FDCWD);
    let to_fd = to_dir.map(AsRawFd::as_raw_fd).unwrap_or(libc::AT_FDCWD);

    // SAFETY: `libc::syscall` is called with the correct arity and types.
    // `from_path` and `to_path` point to valid NUL-terminated strings.
    // `from_fd` and `to_fd` are guaranteed to be valid file descirptors or AT_FDCWD.
    check_retval(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            from_fd,
            from_path.as_ptr(),
            to_fd,
            to_path.as_ptr(),
            flags,
        )
    })?;

    Ok(())
}

/// An utility function that uses `openat2(2)` to restrict the how the provided pathname
/// is resolved. It uses the following flags:
/// - `RESOLVE_IN_ROOT`: Treat the directory referred to by dirfd as the root directory while
///   resolving pathname. This has the effect as though virtiofsd had used chroot(2) to modify its
///   root directory to dirfd.
/// - `RESOLVE_NO_MAGICLINKS`: Disallow all magic-link (i.e., proc(2) link-like files) resolution
///   during path resolution.
///
/// Additionally, the flags `O_NOFOLLOW` and `O_CLOEXEC` are added.
///
/// # Error
///
/// Will return `Err(errno)` if `openat2(2)` fails, see the man page for details.
///
/// # Safety
///
/// The caller must ensure that dirfd is a valid file descriptor.
pub fn do_open_relative_to(
    dir: &impl AsRawFd,
    pathname: &CStr,
    flags: i32,
    mode: Option<u32>,
) -> Result<RawFd> {
    // `openat2(2)` returns an error if `how.mode` contains bits other than those in range 07777,
    // let's ignore the extra bits to be compatible with `openat(2)`.
    let mode = u64::from(mode.unwrap_or(0)) & 0o7777;

    // SAFETY: all-zero byte-pattern represents a valid `libc::open_how`
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.resolve = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;
    how.flags = flags as u64;
    how.mode = mode;

    // SAFETY: `pathname` points to a valid NUL-terminated string, and the `how` pointer is a valid
    // pointer to an `open_how` struct. However, the caller must ensure that `dir` can provide a
    // valid file descriptor (this can be changed to BorrowedFd).
    check_retval(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            pathname.as_ptr(),
            std::ptr::addr_of!(how),
            std::mem::size_of::<libc::open_how>(),
        )
    } as RawFd)
}

mod filehandle {
    use crate::passthrough::file_handle::SerializableFileHandle;
    use crate::util::other_io_error;
    use std::convert::{TryFrom, TryInto};
    use std::io;

    const MAX_HANDLE_SZ: usize = 128;

    #[derive(Clone, PartialOrd, Ord, PartialEq, Eq)]
    #[repr(C)]
    pub struct CFileHandle {
        handle_bytes: libc::c_uint,
        handle_type: libc::c_int,
        f_handle: [u8; MAX_HANDLE_SZ],
    }

    impl Default for CFileHandle {
        fn default() -> Self {
            CFileHandle {
                handle_bytes: MAX_HANDLE_SZ as libc::c_uint,
                handle_type: 0,
                f_handle: [0; MAX_HANDLE_SZ],
            }
        }
    }

    impl CFileHandle {
        pub fn as_bytes(&self) -> &[u8] {
            &self.f_handle[..(self.handle_bytes as usize)]
        }

        pub fn handle_type(&self) -> libc::c_int {
            self.handle_type
        }
    }

    impl TryFrom<&SerializableFileHandle> for CFileHandle {
        type Error = io::Error;

        fn try_from(sfh: &SerializableFileHandle) -> io::Result<Self> {
            let sfh_bytes = sfh.as_bytes();
            if sfh_bytes.len() > MAX_HANDLE_SZ {
                return Err(other_io_error("File handle too long"));
            }
            let mut f_handle = [0u8; MAX_HANDLE_SZ];
            f_handle[..sfh_bytes.len()].copy_from_slice(sfh_bytes);

            Ok(CFileHandle {
                handle_bytes: sfh_bytes.len().try_into().map_err(|err| {
                    other_io_error(format!(
                        "Handle size ({} bytes) too big: {err}",
                        sfh_bytes.len(),
                    ))
                })?,
                #[allow(clippy::useless_conversion)]
                handle_type: sfh.handle_type().try_into().map_err(|err| {
                    other_io_error(format!(
                        "Handle type (0x{:x}) too large: {err}",
                        sfh.handle_type(),
                    ))
                })?,
                f_handle,
            })
        }
    }

    extern "C" {
        pub fn name_to_handle_at(
            dirfd: libc::c_int,
            pathname: *const libc::c_char,
            file_handle: *mut CFileHandle,
            mount_id: *mut libc::c_int,
            flags: libc::c_int,
        ) -> libc::c_int;

        // Technically `file_handle` should be a `mut` pointer, but `open_by_handle_at()` is specified
        // not to change it, so we can declare it `const`.
        pub fn open_by_handle_at(
            mount_fd: libc::c_int,
            file_handle: *const CFileHandle,
            flags: libc::c_int,
        ) -> libc::c_int;
    }
}
pub use filehandle::CFileHandle;

pub fn name_to_handle_at(
    dirfd: &impl AsRawFd,
    pathname: &CStr,
    file_handle: &mut CFileHandle,
    mount_id: &mut libc::c_int,
    flags: libc::c_int,
) -> Result<()> {
    // SAFETY: `dirfd` is a valid file descriptor, `file_handle`
    // is a valid reference to `CFileHandle`, and `mount_id` is
    // valid reference to an `int`
    check_retval(unsafe {
        filehandle::name_to_handle_at(
            dirfd.as_raw_fd(),
            pathname.as_ptr(),
            file_handle,
            mount_id,
            flags,
        )
    })?;
    Ok(())
}

pub fn open_by_handle_at(
    mount_fd: &impl AsRawFd,
    file_handle: &CFileHandle,
    flags: libc::c_int,
) -> Result<File> {
    // SAFETY: `mount_fd` is a valid file descriptor and `file_handle`
    // is a valid reference to `CFileHandle`
    let fd = check_retval(unsafe {
        filehandle::open_by_handle_at(mount_fd.as_raw_fd(), file_handle, flags)
    })?;

    // SAFETY: `open_by_handle_at()` guarantees `fd` is a valid file descriptor
    Ok(unsafe { File::from_raw_fd(fd) })
}

bitflags! {
    /// A bitwise OR of zero or more flags passed in as a parameter to the
    /// write vectored function `writev_at()`.
    pub struct WritevFlags: i32 {
        /// High priority writes. Allows block-based filesystems to use polling of the device, which
        /// provides lower latency, but may use additional resources. (Currently, this feature is
        /// usable only on a file descriptor opened using the `O_DIRECT` flag.)
        const RWF_HIPRI = libc::RWF_HIPRI;

        /// Provide a per-write equivalent of the `O_DSYNC` `open(2)` flag. Its effect applies
        /// only to the data range written by the system call.
        const RWF_DSYNC = libc::RWF_DSYNC;

        /// Provide a per-write equivalent of the `O_SYNC` `open(2)` flag. Its effect applies only
        /// to the data range written by the system call.
        const RWF_SYNC = libc::RWF_SYNC;

        /// Provide a per-write equivalent of the `O_APPEND` `open(2)` flag. Its effect applies only
        /// to the data range written by the system call. The offset argument does not affect the
        /// write operation; the data is always appended to the end of the file.
        /// However, if the offset argument is -1, the current file offset is updated.
        const RWF_APPEND = libc::RWF_APPEND;

        /// Do not honor the `O_APPEND` `open(2)` flag (since Linux 6.9).
        const RWF_NOAPPEND = libc::RWF_NOAPPEND;

        /// Requires that writes to regular files in block-based filesystems be issued with
        /// torn-write protection. Torn-write protection means that for a power or any other
        /// hardware failure, all or none of the data from the write will be stored, but never a
        /// mix of old and new data (since Linux 6.11).
        const RWF_ATOMIC = libc::RWF_ATOMIC;

        /// Uncached buffered write (since Linux 6.14).
        const RWF_DONTCACHE = libc::RWF_DONTCACHE;
    }
}

bitflags! {
    /// A bitwise OR of zero or more flags passed in as a parameter to the
    /// read vectored function `readv_at()`.
    pub struct ReadvFlags: i32 {
        /// High priority read. Allows block-based filesystems to use polling of the device, which
        /// provides lower latency, but may use additional resources. (Currently, this feature is
        /// usable only on a file descriptor opened using the O_DIRECT flag.)
        const RWF_HIPRI = libc::RWF_HIPRI;

        /// Do not wait for data which is not immediately available. If this flag is specified,
        /// the `readv_at()` will return instantly if it would have to read data from the backing
        /// storage or wait for a lock. If some data was successfully read, it will return the
        /// number of bytes read. If no bytes were read, it will return -1 and set errno to
        /// `EAGAIN`.
        const RWF_NOWAIT = libc::RWF_NOWAIT;

        /// Uncached buffered read, any data read will be removed from the page cache upon
        /// completion (since Linux 6.14).
        const RWF_DONTCACHE = libc::RWF_DONTCACHE;
    }
}

/// Safe wrapper for `pwritev2(2)`
///
/// This system call is similar `pwritev(2)`, but add a new argument,
/// flags, which modifies the behavior on a per-call basis.
/// Unlike `pwritev(2)`, if the offset argument is -1, then the current file offset
/// is used and updated.
///
/// # Errors
///
/// Will return `Err(errno)` if `pwritev2(2)` fails, see `pwritev2(2)` for details.
///
/// # Safety
///
/// The caller must ensure that each iovec element is valid (i.e., it has a valid `iov_base`
/// pointer and `iov_len`).
pub unsafe fn writev_at(
    fd: BorrowedFd,
    iovecs: &[libc::iovec],
    offset: i64,
    flags: Option<WritevFlags>,
) -> Result<usize> {
    let flags = flags.unwrap_or(WritevFlags::empty());
    // SAFETY: `fd` is a valid filed descriptor, `iov` is a valid pointer
    // to the iovec slice `ìovecs` of `iovcnt` elements. However, the caller
    // must ensure that each iovec element has a valid `iov_base` pointer and `iov_len`.
    let bytes_written = check_retval(unsafe {
        libc::pwritev2(
            fd.as_raw_fd(),
            iovecs.as_ptr(),
            iovecs.len() as libc::c_int,
            offset,
            flags.bits(),
        )
    })?;
    Ok(bytes_written as usize)
}

/// Safe wrapper for `preadv2(2)`
///
/// This system call is similar `preadv(2)`, but add a new argument,
/// flags, which modifies the behavior on a per-call basis.
/// Unlike `preadv(2)`, if the offset argument is -1, then the current file offset
/// is used and updated.
///
/// # Errors
///
/// Will return `Err(errno)` if `preadv2(2)` fails, see `preadv2(2)` for details.
///
/// # Safety
///
/// The caller must ensure that each iovec element is valid (i.e., it has a valid `iov_base`
/// pointer and `iov_len`).
pub unsafe fn readv_at(
    fd: BorrowedFd,
    iovecs: &[libc::iovec],
    offset: i64,
    flags: Option<ReadvFlags>,
) -> Result<usize> {
    let flags = flags.unwrap_or(ReadvFlags::empty());
    // SAFETY: `fd` is a valid filed descriptor, `iov` is a valid pointer
    // to the iovec slice `ìovecs` of `iovcnt` elements. However, the caller
    // must ensure that each iovec element has a valid `iov_base` pointer and `iov_len`.
    let bytes_read = check_retval(unsafe {
        libc::preadv2(
            fd.as_raw_fd(),
            iovecs.as_ptr(),
            iovecs.len() as libc::c_int,
            offset,
            flags.bits(),
        )
    })?;
    Ok(bytes_read as usize)
}

pub struct PipeReader(File);

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

pub struct PipeWriter(File);

impl io::Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    let mut fds: [RawFd; 2] = [-1, -1];
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok((
            PipeReader(unsafe { File::from_raw_fd(fds[0]) }),
            PipeWriter(unsafe { File::from_raw_fd(fds[1]) }),
        ))
    }
}

// We want credential changes to be per-thread because otherwise
// we might interfere with operations being carried out on other
// threads with different uids/gids. However, posix requires that
// all threads in a process share the same credentials. To do this
// libc uses signals to ensure that when one thread changes its
// credentials the other threads do the same thing.
//
// So instead we invoke the syscall directly in order to get around
// this limitation. Another option is to use the setfsuid and
// setfsgid systems calls. However since those calls have no way to
// return an error, it's preferable to do this instead.
/// Set effective user ID
pub fn seteffuid(uid: HostUid) -> io::Result<()> {
    check_retval(unsafe { libc::syscall(libc::SYS_setresuid, -1, uid.into_inner(), -1) })?;
    Ok(())
}

/// Set effective group ID
pub fn seteffgid(gid: HostGid) -> io::Result<()> {
    check_retval(unsafe { libc::syscall(libc::SYS_setresgid, -1, gid.into_inner(), -1) })?;
    Ok(())
}

/// Set supplementary group
pub fn setsupgroup(gids: &[HostGid]) -> io::Result<()> {
    check_retval(unsafe {
        libc::syscall(
            libc::SYS_setgroups,
            gids.len(),
            // Safe because `HostGid` is exactly `libc::gid_t` with `repr(transparent)`
            gids.as_ptr() as *const libc::gid_t,
        )
    })?;
    Ok(())
}

/// Drop all supplementary groups
pub fn dropsupgroups() -> io::Result<()> {
    check_retval(unsafe {
        libc::syscall(libc::SYS_setgroups, 0, std::ptr::null::<libc::gid_t>())
    })?;
    Ok(())
}
