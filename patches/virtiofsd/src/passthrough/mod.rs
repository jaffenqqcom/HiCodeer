// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod credentials;
pub mod device_state;
pub mod file_handle;
mod guest_fd_limit;
pub mod inode_store;
pub mod mount_fd;
pub mod read_only;
pub mod stat;
pub mod util;
pub mod xattrmap;

use crate::filesystem::{
    Context, Entry, Extensions, FileSystem, FsOptions, GetxattrReply, ListxattrReply, OpenOptions,
    SecContext, SetattrValid, SetxattrFlags, ZeroCopyReader, ZeroCopyWriter,
};
use crate::passthrough::credentials::{drop_effective_cap, UnixCredentials, UnixCredentialsGuard};
use crate::passthrough::device_state::preserialization::{
    self, HandleMigrationInfo, InodeMigrationInfo,
};
use crate::passthrough::inode_store::{
    Inode, InodeData, InodeFile, InodeIds, InodeStore, StrongInodeReference,
};
use crate::passthrough::util::{
    ebadf, is_safe_inode, openat, openat_verbose, reopen_fd_through_proc,
};
use crate::read_dir::ReadDir;
use crate::soft_idmap::{self, GuestGid, GuestUid, HostGid, HostUid, Id, IdMap};
use crate::util::{other_io_error, ResultErrorContext};
use crate::{fuse, oslib};
use file_handle::{FileHandle, FileOrHandle, OpenableFileHandle};
use guest_fd_limit::{GuestFdSemaphore, GuestFile};
use mount_fd::{MPRError, MountFds};
pub(crate) use stat::{statx, StatExt};
use std::borrow::Cow;
use std::collections::{btree_map, BTreeMap};
use std::convert::TryInto;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::io::ErrorKind;
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use xattrmap::{AppliedRule, XattrMap};

const EMPTY_CSTR: &[u8] = b"\0";
const POSIX_ACL_ACCESS_XATTR: &[u8] = b"system.posix_acl_access";

type Handle = u64;

fn should_clear_sgid_for_setxattr(
    posix_acl: bool,
    extra_flags: SetxattrFlags,
    name: &CStr,
) -> bool {
    posix_acl
        && extra_flags.contains(SetxattrFlags::SETXATTR_ACL_KILL_SGID)
        && name.to_bytes() == POSIX_ACL_ACCESS_XATTR
}

enum HandleDataFile {
    File(RwLock<GuestFile>),
    // `io::Error` does not implement `Clone`, so without wrapping it in `Arc`, returning the error
    // anywhere would be impossible without consuming it
    Invalid(Arc<io::Error>),
}

#[derive(Copy, Clone, Debug)]
struct FileType(u32);

impl FileType {
    fn from_mode(mode: u32) -> Self {
        FileType(mode & libc::S_IFMT)
    }

    fn is_regular_file(&self) -> bool {
        self.0 == libc::S_IFREG
    }
}

struct HandleData {
    inode: Inode,
    file: HandleDataFile,
    file_type: FileType,

    // On migration, must be set when we serialize our internal state to send it to the
    // destination.  As long as `HandleMigrationInfo::new()` is cheap, we may as well
    // keep it always set.
    migration_info: HandleMigrationInfo,
}

struct ScopedWorkingDirectory {
    back_to: RawFd,
}

impl ScopedWorkingDirectory {
    fn new(new_wd: RawFd, old_wd: RawFd) -> ScopedWorkingDirectory {
        oslib::fchdir(new_wd).expect("the working directory should be changed");
        ScopedWorkingDirectory { back_to: old_wd }
    }
}

impl Drop for ScopedWorkingDirectory {
    fn drop(&mut self) {
        oslib::fchdir(self.back_to).expect("the working directory should be changed");
    }
}

/// The caching policy that the file system should report to the FUSE client. By default the FUSE
/// protocol uses close-to-open consistency. This means that any cached contents of the file are
/// invalidated the next time that file is opened.
#[derive(Default, Debug, Clone)]
pub enum CachePolicy {
    /// The client should never cache file data and all I/O should be directly forwarded to the
    /// server. This policy must be selected when file contents may change without the knowledge of
    /// the FUSE client (i.e., the file system does not have exclusive access to the directory).
    Never,

    /// This is almost same as Never, but it allows page cache of directories, dentries and attr
    /// cache in guest. In other words, it acts like cache=never for normal files, and like
    /// cache=always for directories, besides, metadata like dentries and attrs are kept as well.
    /// This policy can be used if:
    /// 1. the client wants to use Never policy but it's performance in I/O is not good enough
    /// 2. the file system has exclusive access to the directory
    /// 3. cache directory content and other fs metadata can make a difference on performance.
    Metadata,

    /// The client is free to choose when and how to cache file data. This is the default policy and
    /// uses close-to-open consistency as described in the enum documentation.
    #[default]
    Auto,

    /// The client should always cache file data. This means that the FUSE client will not
    /// invalidate any cached data that was returned by the file system the last time the file was
    /// opened. This policy should only be selected when the file system has exclusive access to the
    /// directory.
    Always,
}

impl FromStr for CachePolicy {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match &s.to_lowercase()[..] {
            "never" => Ok(CachePolicy::Never),
            "metadata" => Ok(CachePolicy::Metadata),
            "auto" => Ok(CachePolicy::Auto),
            "always" => Ok(CachePolicy::Always),
            _ => Err("invalid cache policy"),
        }
    }
}

/// Whether and how to enable an optional feature during capability negotiation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NegotiationMode {
    /// Do not enable the feature
    Never,
    /// Enable if the guest supports it, warn otherwise
    Auto,
    /// Require the feature, error if unsupported
    Always,
}

impl std::fmt::Display for NegotiationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NegotiationMode::Never => write!(f, "never"),
            NegotiationMode::Auto => write!(f, "auto"),
            NegotiationMode::Always => write!(f, "always"),
        }
    }
}

impl FromStr for NegotiationMode {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "never" => Ok(NegotiationMode::Never),
            "auto" => Ok(NegotiationMode::Auto),
            "always" => Ok(NegotiationMode::Always),
            _ => Err("invalid mode, expected: never, auto, always"),
        }
    }
}

/// When to use file handles to reference inodes instead of `O_PATH` file descriptors.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum InodeFileHandlesMode {
    /// Never use file handles, always use `O_PATH` file descriptors.
    #[default]
    Never,

    /// Attempt to generate file handles, but fall back to `O_PATH` file descriptors where the
    /// underlying filesystem does not support file handles.
    Prefer,

    /// Always use file handles, never fall back to `O_PATH` file descriptors.
    Mandatory,
}

/// What to do when an error occurs during migration (checked on the migration destination)
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum MigrationOnError {
    /// Whenever any failure occurs, return a hard error to the vhost-user front-end (e.g.  QEMU),
    /// aborting migration.
    #[default]
    Abort,

    /// Let migration finish, but the guest will be unable to access any of the files that were
    /// failed to be found/opened, receiving only errors.
    GuestError,
}

impl FromStr for MigrationOnError {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "abort" => Ok(MigrationOnError::Abort),
            "guest-error" => Ok(MigrationOnError::GuestError),

            _ => Err("invalid migration-on-error value"),
        }
    }
}

/// How to migrate our internal state to the destination instance
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum MigrationMode {
    /**
     * Obtain paths for all inodes indexed and opened by the guest, and transfer those paths to
     * the destination.
     *
     * To get those paths, we try to read the symbolic links in /proc/self/fd first; if that does
     * not work, we will fall back to iterating through the shared directory (exhaustive search),
     * enumerating all paths within.
     */
    #[default]
    FindPaths,

    /// Transfer inodes by their file handles.
    FileHandles,
}

impl FromStr for MigrationMode {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "find-paths" => Ok(MigrationMode::FindPaths),
            "file-handles" => Ok(MigrationMode::FileHandles),

            _ => Err("invalid migration-mode value"),
        }
    }
}

/// Options that configure the behavior of the file system.
#[derive(Debug)]
pub struct Config {
    /// How long the FUSE client should consider directory entries to be valid. If the contents of a
    /// directory can only be modified by the FUSE client (i.e., the file system has exclusive
    /// access), then this should be a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub entry_timeout: Duration,

    /// How long the FUSE client should consider file and directory attributes to be valid. If the
    /// attributes of a file or directory can only be modified by the FUSE client (i.e., the file
    /// system has exclusive access), then this should be set to a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub attr_timeout: Duration,

    /// The caching policy the file system should use. See the documentation of `CachePolicy` for
    /// more details.
    pub cache_policy: CachePolicy,

    /// Whether the file system should enabled writeback caching. This can improve performance as it
    /// allows the FUSE client to cache and coalesce multiple writes before sending them to the file
    /// system. However, enabling this option can increase the risk of data corruption if the file
    /// contents can change without the knowledge of the FUSE client (i.e., the server does **NOT**
    /// have exclusive access). Additionally, the file system should have read access to all files
    /// in the directory it is serving as the FUSE client may send read requests even for files
    /// opened with `O_WRONLY`.
    ///
    /// Therefore callers should only enable this option when they can guarantee that: 1) the file
    /// system has exclusive access to the directory and 2) the file system has read permissions for
    /// all files in that directory.
    ///
    /// The default value for this option is `false`.
    pub writeback: bool,

    /// The path of the root directory.
    ///
    /// The default is `/`.
    pub root_dir: String,

    /// A prefix to strip from the mount points listed in /proc/self/mountinfo.
    ///
    /// The default is `None`.
    pub mountinfo_prefix: Option<String>,

    /// Whether the file system should support Extended Attributes (xattr). Enabling this feature may
    /// have a significant impact on performance, especially on write parallelism. This is the result
    /// of FUSE attempting to remove the special file privileges after each write request.
    ///
    /// The default value for this options is `false`.
    pub xattr: bool,

    /// An optional translation layer for host<->guest Extended Attribute (xattr) names.
    pub xattrmap: Option<XattrMap>,

    /// The xattr name that "security.capability" is remapped to, if the client remapped it at all.
    /// If the client's xattrmap did not remap "security.capability", this will be `None`.
    pub xattr_security_capability: Option<CString>,

    /// Optional `File` object for /proc/self/fd. Callers can open a `File` and pass it here, so
    /// there's no need to open it in PassthroughFs::new(). This is specially useful for
    /// sandboxing.
    ///
    /// The default is `None`.
    pub proc_sfd_rawfd: Option<File>,

    /// Optional `File` object for /proc/self/mountinfo.  Callers can open a `File` and pass it
    /// here, so there is no need to open it in PassthroughFs::new().  This is especially useful
    /// for sandboxing.
    ///
    /// The default is `None`.
    pub proc_mountinfo_rawfd: Option<File>,

    /// Whether the file system should announce submounts to the guest.  Not doing so means that
    /// the FUSE client may see st_ino collisions: This stat field is passed through, so if the
    /// shared directory encompasses multiple mounts, some inodes (in different file systems) may
    /// have the same st_ino value.  If the FUSE client does not know these inodes are in different
    /// file systems, then it will be oblivious to this collision.
    /// By announcing submount points, the FUSE client can create virtual submounts with distinct
    /// st_dev values where necessary, so that the combination of st_dev and st_ino will stay
    /// unique.
    /// On the other hand, it may be undesirable to let the client know the shared directory's
    /// submount structure.  The user needs to decide which drawback weighs heavier for them, which
    /// is why this is a configurable option.
    ///
    /// The default is `false`.
    pub announce_submounts: bool,

    /// Whether to use file handles to reference inodes.  We need to be able to open file
    /// descriptors for arbitrary inodes, and by default that is done by storing an `O_PATH` FD in
    /// `InodeData`.  Not least because there is a maximum number of FDs a process can have open
    /// users may find it preferable to store a file handle instead, which we can use to open an FD
    /// when necessary.
    /// So this switch allows to choose between the alternatives: When set to `Never`, `InodeData`
    /// will store `O_PATH` FDs.  Otherwise, we will attempt to generate and store a file handle
    /// instead.  With `Prefer`, errors that are inherent to file handles (like no support from the
    /// underlying filesystem) lead to falling back to `O_PATH` FDs, and only generic errors (like
    /// `ENOENT` or `ENOMEM`) are passed to the guest.  `Mandatory` enforces the use of file
    /// handles, returning all errors to the guest.
    ///
    /// The default is `Never`.
    pub inode_file_handles: InodeFileHandlesMode,

    /// Whether the file system should support READDIRPLUS (READDIR+LOOKUP) operations.
    ///
    /// The default is `false`.
    pub readdirplus: bool,

    /// Whether the file system should honor the O_DIRECT flag. If this option is disabled (which
    /// is the default value), that flag will be filtered out at `open_inode`.
    ///
    /// The default is `false`.
    pub allow_direct_io: bool,

    /// If `killpriv_v2` is true then it indicates that the file system is expected to clear the
    /// setuid and setgid bits.
    pub killpriv_v2: bool,

    /// Enable support for posix ACLs
    ///
    /// The default is `Never`.
    pub posix_acl: NegotiationMode,

    /// If `security_label` is enabled during negotiation, then server will indicate to client
    /// to send any security context associated with file during file
    /// creation and set that security context on newly created file.
    /// This security context is expected to be security.selinux.
    ///
    /// The default is `Never`.
    pub security_label: NegotiationMode,

    /// If `clean_noatime` is true automatically clean up O_NOATIME flag to prevent potential
    /// permission errors.
    pub clean_noatime: bool,

    /// If `allow_mmap` is true, then server will allow shared mmap'ing of files opened/created
    /// with DIRECT_IO.
    ///
    /// The default is `false`.
    pub allow_mmap: bool,

    /// Defines what happens when restoring our internal state on the destination fails.
    ///
    /// The default is `Abort`.
    pub migration_on_error: MigrationOnError,

    /// Whether to store a file handle for each inode in the migration stream, alongside the
    /// information on how to find the inode.  The destination must generate the file handle for
    /// the inode it has opened and verify they match.
    ///
    /// The default is `false`.
    pub migration_verify_handles: bool,

    /// Whether to confirm (for path-based migration) at serialization (during switch-over) whether
    /// the paths still match the inodes they are supposed to represent, and if they do not, try to
    /// correct the path via the respective symlink in /proc/self/fd.
    ///
    /// The default is `false`.
    pub migration_confirm_paths: bool,

    /// Defines how to migrate our internal state to the destination instance.
    ///
    /// The default is `FindPaths`.
    pub migration_mode: MigrationMode,

    /**
     * UID map parameters given on the command line.
     *
     * Is `take()`n when `PassthroughFs` is created, i.e. `None` during runtime.
     */
    pub uid_map: Option<Vec<soft_idmap::cmdline::IdMap>>,

    /**
     * GID map parameters given on the command line.
     *
     * Is `take()`n when `PassthroughFs` is created, i.e. `None` during runtime.
     */
    pub gid_map: Option<Vec<soft_idmap::cmdline::IdMap>>,

    /// Number of file descriptors we can allocate for guest use.  Limiting this ensures there is
    /// always some room for file descriptors used and needed by virtiofsd internally.
    ///
    /// The default is `u64::MAX`.
    pub guest_fd_limit: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            entry_timeout: Duration::from_secs(5),
            attr_timeout: Duration::from_secs(5),
            cache_policy: Default::default(),
            writeback: false,
            root_dir: String::from("/"),
            mountinfo_prefix: None,
            xattr: false,
            xattrmap: None,
            xattr_security_capability: None,
            proc_sfd_rawfd: None,
            proc_mountinfo_rawfd: None,
            announce_submounts: false,
            inode_file_handles: Default::default(),
            readdirplus: true,
            allow_direct_io: false,
            killpriv_v2: false,
            posix_acl: NegotiationMode::Never,
            security_label: NegotiationMode::Never,
            clean_noatime: true,
            allow_mmap: false,
            migration_on_error: MigrationOnError::Abort,
            migration_verify_handles: false,
            migration_confirm_paths: false,
            migration_mode: MigrationMode::FindPaths,
            uid_map: None,
            gid_map: None,
            guest_fd_limit: u64::MAX,
        }
    }
}

/// A file system that simply "passes through" all requests it receives to the underlying file
/// system. To keep the implementation simple it servers the contents of its root directory. Users
/// that wish to serve only a specific directory should set up the environment so that that
/// directory ends up as the root of the file system process. One way to accomplish this is via a
/// combination of mount namespaces and the pivot_root system call.
pub struct PassthroughFs {
    // File descriptors for various points in the file system tree. These fds are always opened with
    // the `O_PATH` option so they cannot be used for reading or writing any data. See the
    // documentation of the `O_PATH` flag in `open(2)` for more details on what one can and cannot
    // do with an fd opened with this flag.
    inodes: InodeStore,
    next_inode: AtomicU64,

    // File descriptors for open files and directories. Unlike the fds in `inodes`, these _can_ be
    // used for reading and writing data.
    handles: RwLock<BTreeMap<Handle, Arc<HandleData>>>,
    next_handle: AtomicU64,

    // Represents a limit for the number of file descriptors we allow allocating for the guest.
    // Having such a limit that is below the actual maximum number of file descriptors virtiofsd is
    // allowed to use ensures that virtiofsd can always create file descriptors for internal use.
    guest_fds: Arc<GuestFdSemaphore>,

    // Maps mount IDs to an open FD on the respective ID for the purpose of open_by_handle_at().
    // This is set when inode_file_handles is not never, since in the 'never' case,
    // open_by_handle_at() is not called.
    mount_fds: Option<MountFds>,

    // File descriptor pointing to the `/proc/self/fd` directory. This is used to convert an fd from
    // `inodes` into one that can go into `handles`. This is accomplished by reading the
    // `/proc/self/fd/{}` symlink. We keep an open fd here in case the file system tree that we are
    // meant to be serving doesn't have access to `/proc/self/fd`.
    proc_self_fd: File,

    // File descriptor pointing to the original working directory.
    orig_wd_fd: File,

    // Whether writeback caching is enabled for this directory. This will only be true when
    // `cfg.writeback` is true and `init` was called with `FsOptions::WRITEBACK_CACHE`.
    writeback: AtomicBool,

    // Whether to announce submounts (i.e., whether the guest supports them and whether they are
    // enabled in the configuration)
    announce_submounts: AtomicBool,

    // Whether posix ACLs is enabled.
    posix_acl: AtomicBool,

    // Basic facts about the OS
    os_facts: oslib::OsFacts,

    // Whether the guest kernel supports the supplementary group extension.
    sup_group_extension: AtomicBool,

    // Whether we are preparing for migration and need to track changes to inodes like renames.  We
    // should then also make sure newly created inodes immediately have their migration info set.
    track_migration_info: AtomicBool,

    cfg: Config,

    /// Map to translate between host and guest UIDs.
    uid_map: IdMap<GuestUid, HostUid>,

    /// Map to translate between host and guest GIDs.
    gid_map: IdMap<GuestGid, HostGid>,
}

impl PassthroughFs {
    pub fn new(mut cfg: Config) -> io::Result<PassthroughFs> {
        let proc_self_fd = if let Some(fd) = cfg.proc_sfd_rawfd.take() {
            fd
        } else {
            openat_verbose(
                &libc::AT_FDCWD,
                "/proc/self/fd",
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )?
        };

        let orig_wd_fd = openat_verbose(
            &libc::AT_FDCWD,
            ".",
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )?;

        let mount_fds = if cfg.inode_file_handles == InodeFileHandlesMode::Never
            && cfg.migration_mode != MigrationMode::FileHandles
        {
            None
        } else {
            let mountinfo_fd = if let Some(fd) = cfg.proc_mountinfo_rawfd.take() {
                fd
            } else {
                openat_verbose(
                    &libc::AT_FDCWD,
                    "/proc/self/mountinfo",
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )?
            };
            Some(MountFds::new(mountinfo_fd, cfg.mountinfo_prefix.clone()))
        };

        let uid_map = if let Some(map) = cfg.uid_map.take() {
            map.try_into().err_context(|| "UID map")?
        } else {
            IdMap::empty()
        };

        let gid_map = if let Some(map) = cfg.gid_map.take() {
            map.try_into().err_context(|| "GID map")?
        } else {
            IdMap::empty()
        };

        let mut fs = PassthroughFs {
            inodes: Default::default(),
            next_inode: AtomicU64::new(fuse::ROOT_ID + 1),
            handles: RwLock::new(BTreeMap::new()),
            next_handle: AtomicU64::new(0),
            guest_fds: Arc::new(GuestFdSemaphore::new(cfg.guest_fd_limit)),
            mount_fds,
            proc_self_fd,
            orig_wd_fd,
            writeback: AtomicBool::new(false),
            announce_submounts: AtomicBool::new(false),
            posix_acl: AtomicBool::new(false),
            sup_group_extension: AtomicBool::new(false),
            os_facts: oslib::OsFacts::new(),
            track_migration_info: AtomicBool::new(false),
            cfg,
            uid_map,
            gid_map,
        };

        // Check to see if the client remapped "security.capability", if so,
        // stash its mapping since the daemon will have to enforce semantics
        // that the host kernel otherwise would if the xattrname was not mapped.
        let sec_xattr = unsafe { CStr::from_bytes_with_nul_unchecked(b"security.capability\0") };
        fs.cfg.xattr_security_capability = fs
            .map_client_xattrname(sec_xattr)
            .ok()
            .filter(|n| !sec_xattr.eq(n))
            .map(CString::from);

        fs.check_working_file_handles()?;

        // We need to clear the umask here because we want the client to be
        // able to set all the bits in the mode.
        oslib::umask(0o000);

        Ok(fs)
    }

    fn switch_to_proc_self_fd(&self) -> ScopedWorkingDirectory {
        ScopedWorkingDirectory::new(self.proc_self_fd.as_raw_fd(), self.orig_wd_fd.as_raw_fd())
    }

    pub fn keep_fds(&self) -> Vec<RawFd> {
        vec![self.proc_self_fd.as_raw_fd()]
    }

    fn open_relative_to(
        &self,
        dir: &impl AsRawFd,
        pathname: &CStr,
        flags: i32,
        mode: Option<u32>,
    ) -> io::Result<RawFd> {
        let flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | flags;

        if self.os_facts.has_openat2 {
            oslib::do_open_relative_to(dir, pathname, flags, mode)
        } else {
            oslib::openat(dir, pathname, flags, mode)
        }
    }

    fn find_handle(&self, handle: Handle, inode: Inode) -> io::Result<Arc<HandleData>> {
        self.handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)
    }

    fn open_inode(&self, data: &InodeData, mut flags: i32) -> io::Result<File> {
        // When writeback caching is enabled, the kernel may send read requests even if the
        // userspace program opened the file write-only. So we need to ensure that we have opened
        // the file for reading as well as writing.
        let writeback = self.writeback.load(Ordering::Relaxed);
        if writeback && flags & libc::O_ACCMODE == libc::O_WRONLY {
            flags &= !libc::O_ACCMODE;
            flags |= libc::O_RDWR;
        }

        // When writeback caching is enabled the kernel is responsible for handling `O_APPEND`.
        // However, this breaks atomicity as the file may have changed on disk, invalidating the
        // cached copy of the data in the kernel and the offset that the kernel thinks is the end of
        // the file. Just allow this for now as it is the user's responsibility to enable writeback
        // caching only for directories that are not shared. It also means that we need to clear the
        // `O_APPEND` flag.
        if writeback && flags & libc::O_APPEND != 0 {
            flags &= !libc::O_APPEND;
        }

        if !self.cfg.allow_direct_io && flags & libc::O_DIRECT != 0 {
            flags &= !libc::O_DIRECT;
        }

        data.open_file(flags | libc::O_CLOEXEC, &self.proc_self_fd)?
            .into_file()
    }

    /// Generate a file handle for `fd` using `FileHandle::from_fd()`.  `st` is `fd`'s stat
    /// information (we may need the mount ID for errors/warnings).
    ///
    /// These are the possible return values:
    /// - `Ok(Some(_))`: Success, caller should use this file handle.
    /// - `Ok(None)`: No error, but no file handle is available.  The caller should fall back to
    ///   using an `O_PATH` FD.
    /// - `Err(_)`: An error occurred, the caller should return this to the guest.
    ///
    /// This function takes the chosen `self.cfg.inode_file_handles` mode into account:
    /// - `Never`: Always return `Ok(None)`.
    /// - `Prefer`: Return `Ok(None)` when file handles are not supported by this filesystem.
    ///   Otherwise, return either `Ok(Some(_))` or `Err(_)`, depending on whether a file handle
    ///   could be generated or not.
    /// - `Mandatory`: Never return `Ok(None)`.  When the filesystem does not support file handles,
    ///   return an `Err(_)`.
    ///
    /// When the filesystem does not support file handles, this is logged (as a warning in
    /// `Prefer` mode, and as an error in `Mandatory` mode) one time per filesystem.
    fn get_file_handle_opt(
        &self,
        fd: &impl AsRawFd,
        st: &StatExt,
    ) -> io::Result<Option<FileHandle>> {
        let handle = match self.cfg.inode_file_handles {
            InodeFileHandlesMode::Never => {
                // Let's make this quick, so we can skip this case below
                return Ok(None);
            }

            InodeFileHandlesMode::Prefer | InodeFileHandlesMode::Mandatory => {
                FileHandle::from_fd(fd)?
            }
        };

        if handle.is_none() {
            // No error, but no handle (because of EOPNOTSUPP/EOVERFLOW)?  Log it.
            let io_err = io::Error::from_raw_os_error(libc::EOPNOTSUPP);

            let desc = match self.cfg.inode_file_handles {
                InodeFileHandlesMode::Never => unreachable!(),
                InodeFileHandlesMode::Prefer => {
                    "Filesystem does not support file handles, falling back to O_PATH FDs"
                }
                InodeFileHandlesMode::Mandatory => "Filesystem does not support file handles",
            };

            // Use the MPRError object, because (with a mount ID obtained through statx())
            // `self.mount_fds.error_for()` will attempt to add a prefix to the error description
            // that describes the offending filesystem by mount point and mount ID, and will also
            // suppress the message if we have already logged any error concerning file handles for
            // the respective filesystem (so we only log errors/warnings once).
            let err: MPRError = if st.mnt_id > 0 {
                // Valid mount ID
                // self.mount_fds won't be None if we enter here.
                self.mount_fds
                    .as_ref()
                    .unwrap()
                    .error_for(st.mnt_id, io_err)
            } else {
                // No valid mount ID, return error object not bound to a filesystem
                io_err.into()
            }
            .set_desc(desc.to_string());

            // In `Prefer` mode, warn; in `Mandatory` mode, log and return an error.
            // (Suppress logging if the error is silenced, which means that we have already logged
            // a warning/error for this filesystem.)
            match self.cfg.inode_file_handles {
                InodeFileHandlesMode::Never => unreachable!(),
                InodeFileHandlesMode::Prefer => {
                    if !err.silent() {
                        warn!("{err}");
                    }
                }
                InodeFileHandlesMode::Mandatory => {
                    if !err.silent() {
                        error!("{err}");
                    }
                    return Err(err.into_inner());
                }
            }
        }

        Ok(handle)
    }

    fn make_file_handle_openable(&self, fh: &FileHandle) -> io::Result<OpenableFileHandle> {
        // self.mount_fds won't be None if we enter here.
        fh.to_openable(self.mount_fds.as_ref().unwrap(), |fd, flags| {
            reopen_fd_through_proc(&fd, flags, &self.proc_self_fd)
        })
        .map_err(|e| {
            if !e.silent() {
                error!("{e}");
            }
            e.into_inner()
        })
    }

    fn check_working_file_handles(&mut self) -> io::Result<()> {
        if self.cfg.inode_file_handles == InodeFileHandlesMode::Never {
            // No need to check anything
            return Ok(());
        }

        // Try to open the root directory, turn it into a file handle, then try to open that file
        // handle to see whether file handles do indeed work
        // (Note that we pass through all I/O errors to the caller, because `PassthroughFs::init()`
        // will do these calls (`openat()`, `stat()`, etc.) anyway, so if they do not work now,
        // they probably are not going to work later either.  Better to report errors early then.)
        let root_dir = openat_verbose(
            &libc::AT_FDCWD,
            self.cfg.root_dir.as_str(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?;

        let st = statx(&root_dir, None)?;
        if let Some(h) = self.get_file_handle_opt(&root_dir, &st)? {
            // Got an openable file handle, try opening it
            match self.make_file_handle_openable(&h)?.open(libc::O_PATH) {
                Ok(_) => (),
                Err(e) => match self.cfg.inode_file_handles {
                    InodeFileHandlesMode::Never => unreachable!(),
                    InodeFileHandlesMode::Prefer => {
                        warn!("Failed to open file handle for the root node: {e}");
                        warn!("File handles do not appear safe to use, disabling file handles altogether");
                        self.cfg.inode_file_handles = InodeFileHandlesMode::Never;
                    }
                    InodeFileHandlesMode::Mandatory => {
                        error!("Failed to open file handle for the root node: {e}");
                        error!("Refusing to use (mandatory) file handles, as they do not appear safe to use");
                        return Err(e);
                    }
                },
            }
        } else {
            // Did not get an openable file handle (nor an error), so we cannot be in `mandatory`
            // mode.  We also cannot be in `never` mode, because that is sorted out at the very
            // beginning of this function.  Still, use `match` so the compiler could warn us if we
            // were to forget some (future?) variant.
            match self.cfg.inode_file_handles {
                InodeFileHandlesMode::Never => unreachable!(),
                InodeFileHandlesMode::Prefer => {
                    warn!("Failed to generate a file handle for the root node, disabling file handles altogether");
                    self.cfg.inode_file_handles = InodeFileHandlesMode::Never;
                }
                InodeFileHandlesMode::Mandatory => unreachable!(),
            }
        }

        Ok(())
    }

    /// Try to look up an inode by its `name` relative to the given `parent` inode.  If the inode
    /// is registered in our inode store (`self.inodes`), return a strong reference to it.
    /// Otherwise, return `None` instead.
    /// Along with the inode (or `None`), return information gathered along the way: An `O_PATH`
    /// file to the inode, stat information, and optionally a file handle if virtiofsd has been
    /// configured to use file handles.
    /// Return an error if the parent node cannot be opened, the given inode cannot be found on the
    /// filesystem, or generating the stat information or file handle fails.
    fn try_lookup_implementation(
        &self,
        parent_data: &InodeData,
        name: &CStr,
    ) -> io::Result<(
        Option<StrongInodeReference>,
        File,
        StatExt,
        Option<FileHandle>,
    )> {
        let p_file = parent_data.get_file()?;

        let path_fd = {
            let fd = self.open_relative_to(&p_file, name, libc::O_PATH, None)?;
            // Safe because we just opened this fd.
            unsafe { File::from_raw_fd(fd) }
        };

        let st = statx(&path_fd, None)?;

        // Note that this will always be `None` if `cfg.inode_file_handles` is `Never`, but we only
        // really need the handle when we do not have an `O_PATH` fd open for every inode.  So if
        // `cfg.inode_file_handles` is `Never`, we do not need it anyway.
        let handle = self.get_file_handle_opt(&path_fd, &st)?;

        let ids = InodeIds {
            ino: st.st.st_ino,
            dev: st.st.st_dev,
            mnt_id: st.mnt_id,
        };

        Ok((
            self.inodes.claim_inode(handle.as_ref(), &ids).ok(),
            path_fd,
            st,
            handle,
        ))
    }

    /// Try to look up an inode by its `name` relative to the parent inode given by its
    /// `parent_data`.  If the inode is registered in our inode store (`self.inodes`), return a
    /// strong reference to it.  Otherwise, return `None`.
    /// Return an error if the parent node cannot be opened, the given inode cannot be found on the
    /// filesystem, or generating the Stat information or file handle fails.
    fn try_lookup(
        &self,
        parent_data: &InodeData,
        name: &CStr,
    ) -> io::Result<Option<StrongInodeReference>> {
        self.try_lookup_implementation(parent_data, name)
            .map(|result| result.0)
    }

    fn do_lookup(&self, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let p = self.inodes.get(parent).ok_or_else(ebadf)?;
        let (existing_inode, path_fd, st, handle) = self.try_lookup_implementation(&p, name)?;

        let mut attr_flags: u32 = 0;

        if st.st.st_mode & libc::S_IFMT == libc::S_IFDIR
            && self.announce_submounts.load(Ordering::Relaxed)
            && (st.st.st_dev != p.ids.dev || st.mnt_id != p.ids.mnt_id)
        {
            attr_flags |= fuse::ATTR_SUBMOUNT;
        }

        let inode = if let Some(inode) = existing_inode {
            inode
        } else {
            let file_or_handle = if let Some(h) = handle.as_ref() {
                FileOrHandle::Handle(self.make_file_handle_openable(h)?)
            } else {
                FileOrHandle::File(self.guest_fds.allocate(path_fd)?)
            };

            let mig_info = if self.track_migration_info.load(Ordering::Relaxed) {
                let parent_strong_ref = StrongInodeReference::new_with_data(p, &self.inodes)?;
                Some(InodeMigrationInfo::new(
                    &self.cfg,
                    parent_strong_ref,
                    name,
                    &file_or_handle,
                )?)
            } else {
                None
            };

            let inode_data = InodeData {
                inode: self.next_inode.fetch_add(1, Ordering::Relaxed),
                file_or_handle,
                refcount: AtomicU64::new(1),
                ids: InodeIds {
                    ino: st.st.st_ino,
                    dev: st.st.st_dev,
                    mnt_id: st.mnt_id,
                },
                mode: st.st.st_mode,
                migration_info: Mutex::new(mig_info),
            };
            self.inodes.get_or_insert(inode_data)?
        };

        let attr = fuse::Attr::try_with_flags(
            st.st,
            attr_flags,
            |uid| self.map_host_uid(uid),
            |gid| self.map_host_gid(gid),
        )?;
        Ok(Entry {
            // By leaking, we transfer ownership of this refcount to the guest.  That is safe,
            // because the guest is expected to explicitly release its reference and decrement the
            // refcount via `FORGET` later.
            inode: unsafe { inode.leak() },
            generation: 0,
            attr,
            attr_timeout: self.cfg.attr_timeout,
            entry_timeout: self.cfg.entry_timeout,
        })
    }

    fn do_open(
        &self,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        // We need to clean the `O_APPEND` flag in case the file is mem mapped or if the flag
        // is later modified in the guest using `fcntl(F_SETFL)`. We do a per-write `O_APPEND`
        // check setting `RWF_APPEND` for non-mmapped writes, if necessary.
        let mut flags = flags & !(libc::O_APPEND as u32);

        // Clean O_NOATIME (unless specified otherwise with --preserve-noatime) to prevent
        // potential permission errors when running in unprivileged mode.
        if self.cfg.clean_noatime {
            flags &= !(libc::O_NOATIME as u32)
        }

        let inode_data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let file_type = FileType::from_mode(inode_data.mode);
        let file = {
            let _killpriv_guard = if self.cfg.killpriv_v2 && kill_priv {
                drop_effective_cap("FSETID")?
            } else {
                None
            };
            self.open_inode(&inode_data, flags as i32)?
        };

        if flags & (libc::O_TRUNC as u32) != 0 {
            self.clear_file_capabilities(file.as_raw_fd(), file_type, false)?;
        }

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode,
            file: self.guest_fds.allocate(file)?.into(),
            file_type,
            migration_info: HandleMigrationInfo::new(flags as i32),
        };

        self.handles.write().unwrap().insert(handle, Arc::new(data));

        let mut opts = OpenOptions::empty();
        match self.cfg.cache_policy {
            // We only set the direct I/O option on files.
            CachePolicy::Never => opts.set(
                OpenOptions::DIRECT_IO,
                flags & (libc::O_DIRECTORY as u32) == 0,
            ),
            CachePolicy::Metadata => {
                if flags & (libc::O_DIRECTORY as u32) == 0 {
                    opts |= OpenOptions::DIRECT_IO;
                } else {
                    opts |= OpenOptions::CACHE_DIR | OpenOptions::KEEP_CACHE;
                }
            }
            CachePolicy::Always => {
                opts |= OpenOptions::KEEP_CACHE;
                if flags & (libc::O_DIRECTORY as u32) != 0 {
                    opts |= OpenOptions::CACHE_DIR;
                }
            }
            _ => {}
        };

        Ok((Some(handle), opts))
    }

    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        let mut handles = self.handles.write().unwrap();

        if let btree_map::Entry::Occupied(e) = handles.entry(handle) {
            if e.get().inode == inode {
                // We don't need to close the file here because that will happen automatically when
                // the last `Arc` is dropped.
                e.remove();
                return Ok(());
            }
        }

        Err(ebadf())
    }

    fn do_getattr(&self, inode: Inode) -> io::Result<(fuse::Attr, Duration)> {
        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let inode_file = data.get_file()?;
        let st = statx(&inode_file, None)?.st;
        let attr = fuse::Attr::try_from_stat64(
            st,
            |uid| self.map_host_uid(uid),
            |gid| self.map_host_gid(gid),
        )?;

        Ok((attr, self.cfg.attr_timeout))
    }

    fn do_unlink(&self, parent: Inode, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        let data = self.inodes.get(parent).ok_or_else(ebadf)?;
        let parent_file = data.get_file()?;

        let invalidated_inode = self.before_invalidating_path(&data, name);
        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::unlinkat(parent_file.as_raw_fd(), name.as_ptr(), flags) };
        if let Some(invalidated_inode) = invalidated_inode {
            self.after_invalidating_path(invalidated_inode, "Unlinked");
        }
        if res == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn block_xattr(&self, name: &[u8]) -> bool {
        // Currently we only filter out posix acl xattrs.
        // If acls are enabled, there is nothing to  filter.
        if self.posix_acl.load(Ordering::Relaxed) {
            return false;
        }

        let acl_access = "system.posix_acl_access".as_bytes();
        let acl_default = "system.posix_acl_default".as_bytes();
        acl_access.starts_with(name) || acl_default.starts_with(name)
    }

    fn map_client_xattrname<'a>(&self, name: &'a CStr) -> std::io::Result<Cow<'a, CStr>> {
        if self.block_xattr(name.to_bytes()) {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
        }

        match &self.cfg.xattrmap {
            Some(map) => match map.map_client_xattr(name).expect("unterminated mapping") {
                AppliedRule::Deny => Err(io::Error::from_raw_os_error(libc::EPERM)),
                AppliedRule::Unsupported => Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
                AppliedRule::Pass(new_name) => Ok(new_name),
            },
            None => Ok(Cow::Borrowed(name)),
        }
    }

    fn map_server_xattrlist(&self, xattr_names: Vec<u8>) -> Vec<u8> {
        let all_xattrs = match &self.cfg.xattrmap {
            Some(map) => map
                .map_server_xattrlist(xattr_names)
                .expect("unterminated mapping"),
            None => xattr_names,
        };

        // filter out the blocked xattrs
        let mut filtered = Vec::with_capacity(all_xattrs.len());
        let all_xattrs = all_xattrs.split(|b| *b == 0).filter(|bs| !bs.is_empty());

        for xattr in all_xattrs {
            if !self.block_xattr(xattr) {
                filtered.extend_from_slice(xattr);
                filtered.push(0);
            }
        }

        filtered.shrink_to_fit();

        filtered
    }

    /// Clears file capabilities
    /// Only clears capabilities on regular files, matching kernel semantics.
    ///
    /// * `fd` - A file descriptor
    /// * `file_type` - The type of the file
    /// * `o_path` - Must be `true` if the file referred to by `fd` was opened with the `O_PATH` flag
    ///
    /// If it is not clear whether `fd` was opened with `O_PATH` it is safe to set `o_path`
    /// to `true`.
    fn clear_file_capabilities(
        &self,
        fd: RawFd,
        file_type: FileType,
        o_path: bool,
    ) -> io::Result<()> {
        match self.cfg.xattr_security_capability.as_ref() {
            // Unmapped, let the kernel take care of this.
            None => Ok(()),
            // Otherwise we have to uphold the same semantics the kernel
            // would; which is to drop the "security.capability" xattr
            // on write
            Some(xattrname) => {
                // The kernel's `chown_common()` (i.e., `chown()`/`chgrp()`) and nfsd
                // clear caps on all non-directory inodes, but file caps are only
                // consumed by `execve()`, which is restricted to regular files.
                if !file_type.is_regular_file() {
                    return Ok(());
                }

                let res = if o_path {
                    let proc_file_name = CString::new(format!("{fd}"))
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let _working_dir_guard = self.switch_to_proc_self_fd();
                    unsafe { libc::removexattr(proc_file_name.as_ptr(), xattrname.as_ptr()) }
                } else {
                    unsafe { libc::fremovexattr(fd, xattrname.as_ptr()) }
                };

                if res == 0 {
                    Ok(())
                } else {
                    let eno = io::Error::last_os_error();
                    match eno.raw_os_error().unwrap() {
                        libc::ENODATA | libc::ENOTSUP => Ok(()),
                        _ => Err(eno),
                    }
                }
            }
        }
    }

    /// Clears S_ISGID from file mode
    ///
    /// * `file` - file reference (must implement AsRawFd)
    /// * `o_path` - Must be `true` if the file referred to by `fd` was opened with the `O_PATH` flag
    ///
    /// If it is not clear whether `fd` was opened with `O_PATH` it is safe to set `o_path`
    /// to `true`.
    fn clear_sgid(&self, file: &impl AsRawFd, o_path: bool) -> io::Result<()> {
        let fd = file.as_raw_fd();
        let st = statx(file, None)?.st;

        if o_path {
            oslib::fchmodat(
                self.proc_self_fd.as_raw_fd(),
                format!("{fd}"),
                st.st_mode & 0o7777 & !libc::S_ISGID,
                0,
            )
        } else {
            oslib::fchmod(fd, st.st_mode & 0o7777 & !libc::S_ISGID)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_create(
        &self,
        ctx: &Context,
        parent_file: &InodeFile,
        name: &CStr,
        mode: u32,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<RawFd> {
        let fd = {
            let _credentials_guard = self.unix_credentials_guard(ctx, &extensions)?;
            let _umask_guard = self
                .posix_acl
                .load(Ordering::Relaxed)
                .then(|| oslib::ScopedUmask::new(umask));

            // Add libc:O_EXCL to ensure we're not accidentally opening a file the guest wouldn't
            // be allowed to access otherwise.
            self.open_relative_to(
                parent_file,
                name,
                flags as i32 | libc::O_CREAT | libc::O_EXCL,
                mode.into(),
            )?
        };

        // Set security context
        if let Some(secctx) = extensions.secctx {
            // Remap security xattr name.
            let xattr_name = match self.map_client_xattrname(&secctx.name) {
                Ok(xattr_name) => xattr_name,
                Err(e) => {
                    unsafe {
                        libc::unlinkat(parent_file.as_raw_fd(), name.as_ptr(), 0);
                    }
                    return Err(e);
                }
            };

            let ret = unsafe {
                libc::fsetxattr(
                    fd,
                    xattr_name.as_ptr(),
                    secctx.secctx.as_ptr() as *const libc::c_void,
                    secctx.secctx.len(),
                    0,
                )
            };

            if ret != 0 {
                unsafe {
                    libc::unlinkat(parent_file.as_raw_fd(), name.as_ptr(), 0);
                }
                return Err(io::Error::last_os_error());
            }
        }
        Ok(fd)
    }

    fn do_mknod_mkdir_symlink_secctx(
        &self,
        parent_file: &InodeFile,
        name: &CStr,
        secctx: &SecContext,
    ) -> io::Result<()> {
        // Remap security xattr name.
        let xattr_name = self.map_client_xattrname(&secctx.name)?;

        // Set security context on newly created node. It could be
        // device node as well, so it is not safe to open the node
        // and call fsetxattr(). Instead, use the fchdir(proc_fd)
        // and call setxattr(o_path_fd). We use this trick while
        // setting xattr as well.

        // Open O_PATH fd for dir/symlink/special node just created.
        let path_fd = self.open_relative_to(parent_file, name, libc::O_PATH, None)?;

        let procname = CString::new(format!("{path_fd}"))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));

        let procname = match procname {
            Ok(name) => name,
            Err(error) => {
                return Err(error);
            }
        };

        let _working_dir_guard = self.switch_to_proc_self_fd();

        let res = unsafe {
            libc::setxattr(
                procname.as_ptr(),
                xattr_name.as_ptr(),
                secctx.secctx.as_ptr() as *const libc::c_void,
                secctx.secctx.len(),
                0,
            )
        };

        let res_err = io::Error::last_os_error();

        if res == 0 {
            Ok(())
        } else {
            Err(res_err)
        }
    }

    pub fn open_root_node(&self) -> io::Result<()> {
        // We use `O_PATH` because we just want this for traversing the directory tree
        // and not for actually reading the contents. We don't use `open_relative_to()`
        // here because we are not opening a guest-provided pathname. Also, `self.cfg.root_dir`
        // is an absolute pathname, thus not relative to CWD, so we will not be able to open it
        // if "/" didn't change (e.g., chroot or pivot_root)
        let path_fd = openat(
            &libc::AT_FDCWD,
            self.cfg.root_dir.as_str(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?;

        let st = statx(&path_fd, None)?;
        let handle = self.get_file_handle_opt(&path_fd, &st)?;

        let file_or_handle = if let Some(h) = handle.as_ref() {
            FileOrHandle::Handle(self.make_file_handle_openable(h)?)
        } else {
            FileOrHandle::File(self.guest_fds.allocate(path_fd)?)
        };

        // Always keep the root node's migration info set (`InodeStore::clear_migration_info()`
        // will not clear it either); this way, whenever the filesystem is mounted (and this
        // function is called), we will have it set and can migrate it.
        // (Other nodes' migration info is set in `do_lookup()` when they are discovered during
        // migration.)
        let migration_info = match InodeMigrationInfo::new_root(&self.cfg, &file_or_handle) {
            Ok(mig_info) => Some(mig_info),
            Err(err) => {
                warn!(
                    "Failed to construct migration information for the root node: {err}; \
                    may not be able to migrate"
                );
                None
            }
        };

        // Not sure why the root inode gets a refcount of 2 but that's what libfuse does.
        let inode = InodeData {
            inode: fuse::ROOT_ID,
            file_or_handle,
            refcount: AtomicU64::new(2),
            ids: InodeIds {
                ino: st.st.st_ino,
                dev: st.st.st_dev,
                mnt_id: st.mnt_id,
            },
            mode: st.st.st_mode,
            migration_info: Mutex::new(migration_info),
        };
        self.inodes.new_inode(inode)?;
        Ok(())
    }

    /// After renaming an inode while preparing for migration, update its migration info if
    /// necessary.  For example, when representing inodes through their filename and parent
    /// directory node, these must be updated to match the new name and location.
    /// `parent` and `filename` are the inode's new location.
    fn update_inode_migration_info(
        &self,
        parent_data: Arc<InodeData>,
        filename: &CStr,
    ) -> io::Result<()> {
        if !self.track_migration_info.load(Ordering::Relaxed) {
            // Not preparing for migration?  Nothing to do.
            return Ok(());
        }

        // We only need to update the node's migration info if we have it in our store
        if let Some(inode) = self.try_lookup(&parent_data, filename)? {
            let inode_data = inode.get();
            let parent_strong_ref = StrongInodeReference::new_with_data(parent_data, &self.inodes)?;
            let mut info_locked = inode_data.migration_info.lock().unwrap();
            // Unconditionally clear any potentially existing path, because it will be outdated
            if let Some(info) = info_locked.take() {
                if !info.has_path() {
                    // We have some migration info, but it is not path-based?  Keep it then.
                    *info_locked = Some(info);
                    return Ok(());
                }
            }
            *info_locked = Some(InodeMigrationInfo::new(
                &self.cfg,
                parent_strong_ref,
                filename,
                &inode_data.file_or_handle,
            )?);
        }

        Ok(())
    }

    /**
     * Prepare for a path to be invalidated (e.g. by overwriting or unlinking), which can be
     * relevant to migration.
     *
     * If there is an inode in our inode store on the given path, return it.  If so, the caller
     * must call `after_invalidating_path()` after the invalidating operation is done.
     *
     * Background: When an inode's path is invalidated (while preparing for migration), e.g.
     * because it is unlinked or overwritten by a different inode, its migration info must too be
     * invalidated so we do not transmit a wrong path to the destination.  While we must look for
     * the inode before the invalidating operation (lest it is gone), we must invalidate its
     * migration info only *after* that operation, or we’d have a TOCTTOU problem.  Consider this
     * order of execution:
     * 1. We invalidate the path in the migration info (before executing the operation)
     * 2. Preserialization process runs in the background; before the operation is done, it finds
     *    the still-existing (old) inode, re-setting the migration info's path to the one we just
     *    cleared
     * 3. Operation executes, making that path point to a different inode
     *
     * Therefore, the invalidation process is split into two parts, `before_invalidating_path()`
     * and `after_invalidating_path()`.
     */
    fn before_invalidating_path(
        &self,
        parent: &InodeData,
        filename: &CStr,
    ) -> Option<StrongInodeReference> {
        // Note that we have to do this unconditionally, regardless of the value of
        // `track_migration_info` -- same TOCTTOU problem as described in the comment above applies
        // (preserialization might start before the operation is done)
        self.try_lookup(parent, filename).ok().flatten()
    }

    /**
     * Counterpart to `before_invalidating_path()`; must be called after the path-invalidating
     * operation.
     *
     * If the inode’s migration info contains any path data, it is invalidated, and we
     * try to refresh it from /proc/self/fd: It’s possible that the operation failed (and so the
     * original path is still intact), or that the inode has another hard link left that the
     * kernel knows about (unlikely, but worth a try).
     *
     * `inode` is the reference returned by `before_invalidating_path()`, `old_parent` and
     * `old_filename` are used solely to generate a human-readable warning, as is `operation`,
     * which is a capitalized simple past verb form describing the operation (e.g. "Overwrote").
     */
    fn after_invalidating_path(&self, inode: StrongInodeReference, operation: &str) {
        let inode_data = inode.get();

        let old_location = {
            let mut migration_info_locked = inode_data.migration_info.lock().unwrap();
            let Some(migration_info) = migration_info_locked.take() else {
                // No migration info?  Nothing to do then.
                return;
            };
            if !migration_info.has_path() {
                // No path in the migration info?  Nothing to invalidate then.
                *migration_info_locked = Some(migration_info);
                return;
            }
            migration_info.location
        };

        match self.after_invalidating_path_refresh(inode_data) {
            Ok(()) => {
                if let Some(migration_info) = inode_data.migration_info.lock().unwrap().as_ref() {
                    let new_location = &migration_info.location;
                    info!("{operation} {old_location}, but found under {new_location}");
                } else {
                    warn!(
                        "{operation} {old_location}, seem to have found new path, but lost it \
                        again; may be unable to migrate inode"
                    );
                }
            }
            Err(err) => warn!(
                "{operation} {old_location}, failed to get new path: {err}; will be unable to \
                migrate inode"
            ),
        }
    }

    /**
     * Try to find a new path to the given inode.
     *
     * Used internally by `after_invalidating_path()`.  Updates all migration info objects along
     * the path.
     */
    fn after_invalidating_path_refresh(&self, inode_data: &InodeData) -> io::Result<()> {
        let shared_dir_path = self
            .inodes
            .get(fuse::ROOT_ID)
            .ok_or_else(|| other_io_error("Shared directory root node not found"))?
            .get_path(&self.proc_self_fd)
            .map_err(io::Error::from)
            .err_context(|| "Failed to get shared directory root path")?;

        preserialization::proc_paths::set_path_migration_info_from_proc_self_fd(
            inode_data,
            self,
            &shared_dir_path,
        )
        .map_err(preserialization::proc_paths::WrappedError::into_inner)
    }

    /**
     * Temporarily changes the effective UID and GID.
     *
     * Changes the effective UID and GID to the one required by `ctx`, potentially including a
     * supplementary GID given in `extensions`.  Return to the previous state once the returned
     * guard is dropped.
     */
    fn unix_credentials_guard(
        &self,
        ctx: &Context,
        extensions: &Extensions,
    ) -> io::Result<Option<UnixCredentialsGuard>> {
        let host_uid = self.map_guest_uid(ctx.uid)?;
        let host_gid = self.map_guest_gid(ctx.gid)?;
        let supp_gids = extensions
            .sup_gids
            .iter()
            .map(|gid| self.map_guest_gid(*gid))
            .collect::<io::Result<_>>()?;

        UnixCredentials::new(host_uid, host_gid)
            .supplementary_gid(self.sup_group_extension.load(Ordering::Relaxed), supp_gids)
            .set()
    }

    /// Translate `guest_uid` to a host UID using [`self.uid_map`](`Self#structfield.uid_map`).
    fn map_guest_uid(&self, guest_uid: GuestUid) -> io::Result<HostUid> {
        self.uid_map.map_guest(guest_uid).map_err(Into::into)
    }

    /// Translate `guest_gid` to a host GID using [`self.gid_map`](`Self#structfield.gid_map`).
    fn map_guest_gid(&self, guest_gid: GuestGid) -> io::Result<HostGid> {
        self.gid_map.map_guest(guest_gid).map_err(Into::into)
    }

    /// Translate `host_uid` to a guest UID using [`self.uid_map`](`Self#structfield.uid_map`).
    fn map_host_uid(&self, host_uid: HostUid) -> io::Result<GuestUid> {
        self.uid_map.map_host(host_uid).map_err(Into::into)
    }

    /// Translate `host_gid` to a guest GID using [`self.gid_map`](`Self#structfield.gid_map`).
    fn map_host_gid(&self, host_gid: HostGid) -> io::Result<GuestGid> {
        self.gid_map.map_host(host_gid).map_err(Into::into)
    }
}

impl FileSystem for PassthroughFs {
    type Inode = Inode;
    type Handle = Handle;
    type DirIter = ReadDir<Vec<u8>>;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // Force-wipe prior state in case someone "forgot" to send a DESTROY
        self.destroy();

        self.open_root_node()?;

        // Note: On migration, all options negotiated here with the guest must be sent to the
        // destination in the `device_state::serialized::NegotiatedOpts` structure.  So when adding
        // a new option here, don't forget to add it there, too, and handle it both in
        // `<serialized::NegotiatedOpts as From<&PassthroughFs>>::from()` and
        // `serialized::NegotiatedOpts::apply()`.

        let mut opts = if self.cfg.readdirplus {
            FsOptions::DO_READDIRPLUS | FsOptions::READDIRPLUS_AUTO
        } else {
            FsOptions::empty()
        };
        if self.cfg.writeback && capable.contains(FsOptions::WRITEBACK_CACHE) {
            opts |= FsOptions::WRITEBACK_CACHE;
            self.writeback.store(true, Ordering::Relaxed);
        }
        if self.cfg.announce_submounts {
            if capable.contains(FsOptions::SUBMOUNTS) {
                self.announce_submounts.store(true, Ordering::Relaxed);
            } else {
                eprintln!("Warning: Cannot announce submounts, client does not support it");
            }
        }
        if self.cfg.killpriv_v2 {
            if capable.contains(FsOptions::HANDLE_KILLPRIV_V2) {
                opts |= FsOptions::HANDLE_KILLPRIV_V2;
            } else {
                warn!("Cannot enable KILLPRIV_V2, client does not support it");
            }
        }
        if self.cfg.posix_acl != NegotiationMode::Never {
            let acl_required_flags =
                FsOptions::POSIX_ACL | FsOptions::DONT_MASK | FsOptions::SETXATTR_EXT;
            if capable.contains(acl_required_flags) {
                opts |= acl_required_flags;
                self.posix_acl.store(true, Ordering::Relaxed);
                debug!("init: enabling posix acl");
            } else if self.cfg.posix_acl == NegotiationMode::Always {
                error!("Cannot enable posix ACLs, client does not support it");
                return Err(io::Error::from_raw_os_error(libc::EPROTO));
            } else {
                warn!("posix ACLs requested but client does not support it, continuing without");
            }
        }

        if self.cfg.security_label != NegotiationMode::Never {
            if capable.contains(FsOptions::SECURITY_CTX) {
                opts |= FsOptions::SECURITY_CTX;
            } else if self.cfg.security_label == NegotiationMode::Always {
                error!("Cannot enable security label. kernel does not support FUSE_SECURITY_CTX capability");
                return Err(io::Error::from_raw_os_error(libc::EPROTO));
            } else {
                warn!(
                    "security label requested but kernel does not support it, continuing without"
                );
            }
        }

        if self.cfg.allow_mmap {
            opts |= FsOptions::DIRECT_IO_ALLOW_MMAP;
        }

        if capable.contains(FsOptions::CREATE_SUPP_GROUP) {
            self.sup_group_extension.store(true, Ordering::Relaxed);
        }

        Ok(opts)
    }

    fn destroy(&self) {
        self.handles.write().unwrap().clear();
        self.inodes.clear();
        self.writeback.store(false, Ordering::Relaxed);
        self.announce_submounts.store(false, Ordering::Relaxed);
        self.posix_acl.store(false, Ordering::Relaxed);
        self.sup_group_extension.store(false, Ordering::Relaxed);
    }

    fn statfs(&self, _ctx: Context, inode: Inode) -> io::Result<libc::statvfs64> {
        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let inode_file = data.get_file()?;
        let mut out = MaybeUninit::<libc::statvfs64>::zeroed();

        // Safe because this will only modify `out` and we check the return value.
        let res = unsafe { libc::fstatvfs64(inode_file.as_raw_fd(), out.as_mut_ptr()) };
        if res == 0 {
            // Safe because the kernel guarantees that `out` has been initialized.
            Ok(unsafe { out.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn lookup(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        self.do_lookup(parent, name)
    }

    fn forget(&self, _ctx: Context, inode: Inode, count: u64) {
        self.inodes.forget_one(inode, count)
    }

    fn batch_forget(&self, _ctx: Context, requests: Vec<(Inode, u64)>) {
        self.inodes.forget_many(requests)
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.do_open(inode, false, flags | (libc::O_DIRECTORY as u32))
    }

    fn releasedir(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let data = self.inodes.get(parent).ok_or_else(ebadf)?;
        let parent_file = data.get_file()?;

        let invalidated_inode = self.before_invalidating_path(&data, name);
        let res = {
            let _credentials_guard = self.unix_credentials_guard(&ctx, &extensions)?;
            let _umask_guard = self
                .posix_acl
                .load(Ordering::Relaxed)
                .then(|| oslib::ScopedUmask::new(umask));

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe { libc::mkdirat(parent_file.as_raw_fd(), name.as_ptr(), mode) }
        };
        if let Some(invalidated_inode) = invalidated_inode {
            self.after_invalidating_path(invalidated_inode, "Overwrote (via mkdir)");
        }
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Set security context on dir.
        if let Some(secctx) = extensions.secctx {
            if let Err(e) = self.do_mknod_mkdir_symlink_secctx(&parent_file, name, &secctx) {
                unsafe {
                    libc::unlinkat(parent_file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                };
                return Err(e);
            }
        }

        self.do_lookup(parent, name)
    }

    fn rmdir(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.do_unlink(parent, name, libc::AT_REMOVEDIR)
    }

    fn readdir(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
    ) -> io::Result<Self::DirIter> {
        if size == 0 {
            return Ok(ReadDir::default());
        }
        let data = self.find_handle(handle, inode)?;

        let buf = vec![0; size as usize];

        // Since we are going to work with the kernel offset, we have to acquire the file
        // lock for both the `lseek64` and `getdents64` syscalls to ensure that no other
        // thread changes the kernel offset while we are using it.
        #[allow(clippy::readonly_write_lock)]
        let dir = data.file.get()?.write().unwrap();

        ReadDir::new(&*dir, offset, buf)
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.do_open(inode, kill_priv, flags)
    }

    fn release(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn create(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        kill_priv: bool,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        let data = self.inodes.get(parent).ok_or_else(ebadf)?;
        let parent_file = data.get_file()?;

        // We need to clean the `O_APPEND` flag in case the file is mem mapped or if the flag
        // is later modified in the guest using `fcntl(F_SETFL)`. We do a per-write `O_APPEND`
        // check setting `RWF_APPEND` for non-mmapped writes, if necessary.
        let create_flags = flags & !(libc::O_APPEND as u32);
        let fd = self.do_create(
            &ctx,
            &parent_file,
            name,
            mode,
            create_flags,
            umask,
            extensions,
        );

        let (entry, handle) = match fd {
            Err(last_error) => {
                // Ignore the error if the file exists and O_EXCL is not present in `flags`
                match last_error.kind() {
                    io::ErrorKind::AlreadyExists => {
                        if (flags as i32 & libc::O_EXCL) != 0 {
                            return Err(last_error);
                        }
                    }
                    _ => return Err(last_error),
                }

                let entry = self.do_lookup(parent, name)?;
                let (handle, _) = self.do_open(entry.inode, kill_priv, flags)?;
                let handle = handle.ok_or_else(ebadf)?;

                (entry, handle)
            }
            Ok(fd) => {
                // Safe because we just opened this fd.
                let file = unsafe { File::from_raw_fd(fd) };

                let entry = self.do_lookup(parent, name)?;

                let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
                let data = HandleData {
                    inode: entry.inode,
                    file: self.guest_fds.allocate(file)?.into(),
                    file_type: FileType::from_mode(mode),
                    migration_info: HandleMigrationInfo::new(flags as i32),
                };

                self.handles.write().unwrap().insert(handle, Arc::new(data));

                (entry, handle)
            }
        };

        let mut opts = OpenOptions::empty();
        match self.cfg.cache_policy {
            CachePolicy::Never => opts |= OpenOptions::DIRECT_IO,
            CachePolicy::Metadata => opts |= OpenOptions::DIRECT_IO,
            CachePolicy::Always => opts |= OpenOptions::KEEP_CACHE,
            _ => {}
        };

        Ok((entry, Some(handle), opts))
    }

    fn unlink(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.do_unlink(parent, name, 0)
    }

    fn read<W: ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let data = self.find_handle(handle, inode)?;

        // This is safe because read_from_file_at uses preadv64, so the underlying file descriptor
        // offset is not affected by this operation.
        let f = data.file.get()?.read().unwrap();
        w.read_from_file_at(f.get_file(), size as usize, offset, None)
    }

    fn write<R: ZeroCopyReader>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mut r: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<usize> {
        let data = self.find_handle(handle, inode)?;

        // This is safe because write_to_file_at uses `pwritev2(2)`, so the underlying file
        // descriptor offset is not affected by this operation.
        let f = data.file.get()?.read().unwrap();

        {
            let _killpriv_guard = if self.cfg.killpriv_v2 && kill_priv {
                // We need to drop FSETID during a write so that the kernel will remove setuid
                // or setgid bits from the file if it was written to by someone other than the
                // owner.
                drop_effective_cap("FSETID")?
            } else {
                None
            };

            self.clear_file_capabilities(f.as_raw_fd(), data.file_type, false)?;

            // We don't set the `RWF_APPEND` (i.e., equivalent to `O_APPEND`) flag, if it's a
            // delayed write (i.e., using writeback mode or a mem mapped file) even if the file
            // was open in append mode, since the guest kernel sends the correct offset.
            // For non-delayed writes, we set the append mode, if necessary, to correctly handle
            // writes on a file shared among VMs. This case can only be handled correctly if the
            // write on the underlying file is performed in append mode.
            let is_append = flags & libc::O_APPEND as u32 != 0;
            let flags = (!delayed_write && is_append).then_some(oslib::WritevFlags::RWF_APPEND);
            r.write_to_file_at(f.get_file(), size as usize, offset, flags)
        }
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(fuse::Attr, Duration)> {
        self.do_getattr(inode)
    }

    fn setattr(
        &self,
        _ctx: Context,
        inode: Inode,
        attr: fuse::SetattrIn,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(fuse::Attr, Duration)> {
        let inode_data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let file_type = FileType::from_mode(inode_data.mode);

        // In this case, we need to open a new O_RDWR FD
        let rdwr_inode_file = handle.is_none() && valid.intersects(SetattrValid::SIZE);
        let inode_file = if rdwr_inode_file {
            inode_data.open_file(libc::O_NONBLOCK | libc::O_RDWR, &self.proc_self_fd)?
        } else {
            inode_data.get_file()?
        };

        // `HandleData` is never read, but we need to keep a reference so its file is not dropped
        #[allow(dead_code)]
        enum Data {
            Handle(Arc<HandleData>, RawFd),
            ProcPath(CString),
        }

        // If we have a handle then use it otherwise get a new fd from the inode.
        let data = if let Some(handle) = handle {
            let hd = self.find_handle(handle, inode)?;

            let fd = hd.file.get()?.write().unwrap().as_raw_fd();
            Data::Handle(hd, fd)
        } else {
            let pathname = CString::new(format!("{}", inode_file.as_raw_fd()))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Data::ProcPath(pathname)
        };

        if valid.contains(SetattrValid::MODE) {
            // Safe because this doesn't modify any memory and we check the return value.
            let res = unsafe {
                match data {
                    Data::Handle(_, fd) => libc::fchmod(fd, attr.mode),
                    Data::ProcPath(ref p) => {
                        libc::fchmodat(self.proc_self_fd.as_raw_fd(), p.as_ptr(), attr.mode, 0)
                    }
                }
            };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            let uid = if valid.contains(SetattrValid::UID) {
                self.map_guest_uid(attr.uid)?.into_inner()
            } else {
                // Cannot use -1 here because these are unsigned values.
                u32::MAX
            };
            let gid = if valid.contains(SetattrValid::GID) {
                self.map_guest_gid(attr.gid)?.into_inner()
            } else {
                // Cannot use -1 here because these are unsigned values.
                u32::MAX
            };

            self.clear_file_capabilities(inode_file.as_raw_fd(), file_type, true)?;

            // Safe because this is a constant value and a valid C string.
            let empty = unsafe { CStr::from_bytes_with_nul_unchecked(EMPTY_CSTR) };

            // Safe because this doesn't modify any memory and we check the return value.
            let res = unsafe {
                libc::fchownat(
                    inode_file.as_raw_fd(),
                    empty.as_ptr(),
                    uid,
                    gid,
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if valid.contains(SetattrValid::SIZE) {
            let fd = match data {
                Data::Handle(_, fd) => fd,
                _ => {
                    // Should have opened an O_RDWR inode_file above
                    assert!(rdwr_inode_file);
                    inode_file.as_raw_fd()
                }
            };

            let _killpriv_guard =
                if self.cfg.killpriv_v2 && valid.contains(SetattrValid::KILL_SUIDGID) {
                    drop_effective_cap("FSETID")?
                } else {
                    None
                };

            // Safe because this doesn't modify any memory and we check the return value.
            let res = self
                .clear_file_capabilities(fd, file_type, false)
                .map(|_| unsafe { libc::ftruncate(fd, attr.size as i64) })?;
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if valid.intersects(SetattrValid::ATIME | SetattrValid::MTIME) {
            let mut tvs = [
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
            ];

            if valid.contains(SetattrValid::ATIME_NOW) {
                tvs[0].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::ATIME) {
                tvs[0].tv_sec = attr.atime as i64;
                tvs[0].tv_nsec = attr.atimensec.into();
            }

            if valid.contains(SetattrValid::MTIME_NOW) {
                tvs[1].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::MTIME) {
                tvs[1].tv_sec = attr.mtime as i64;
                tvs[1].tv_nsec = attr.mtimensec.into();
            }

            // Safe because this doesn't modify any memory and we check the return value.
            let res = match data {
                Data::Handle(_, fd) => unsafe { libc::futimens(fd, tvs.as_ptr()) },
                Data::ProcPath(ref p) => unsafe {
                    libc::utimensat(self.proc_self_fd.as_raw_fd(), p.as_ptr(), tvs.as_ptr(), 0)
                },
            };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        self.do_getattr(inode)
    }

    fn rename(
        &self,
        _ctx: Context,
        olddir: Inode,
        oldname: &CStr,
        newdir: Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        let old_inode = self.inodes.get(olddir).ok_or_else(ebadf)?;
        let new_inode = self.inodes.get(newdir).ok_or_else(ebadf)?;

        let old_file = old_inode.get_file()?;
        let new_file = new_inode.get_file()?;

        let invalidated_inode = self.before_invalidating_path(&new_inode, newname);
        // Safe because this doesn't modify any memory and we check the return value.
        // TODO: Switch to libc::renameat2 once https://github.com/rust-lang/libc/pull/1508 lands
        // and we have glibc 2.28.
        let res = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                old_file.as_raw_fd(),
                oldname.as_ptr(),
                new_file.as_raw_fd(),
                newname.as_ptr(),
                flags,
            )
        };
        if let Some(invalidated_inode) = invalidated_inode {
            self.after_invalidating_path(invalidated_inode, "Overwrote (via rename)");
        }
        if res != 0 {
            return Err(io::Error::last_os_error());
        }

        if let Err(err) = self.update_inode_migration_info(new_inode, newname) {
            warn!(
                "Failed to update renamed file's ({oldname:?} -> {newname:?}) migration info, \
                the migration destination may be unable to find it: {err}",
            );
        }

        Ok(())
    }

    fn mknod(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let data = self.inodes.get(parent).ok_or_else(ebadf)?;
        let parent_file = data.get_file()?;

        let invalidated_inode = self.before_invalidating_path(&data, name);
        let res = {
            let _credentials_guard = self.unix_credentials_guard(&ctx, &extensions)?;
            let _umask_guard = self
                .posix_acl
                .load(Ordering::Relaxed)
                .then(|| oslib::ScopedUmask::new(umask));

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe {
                libc::mknodat(
                    parent_file.as_raw_fd(),
                    name.as_ptr(),
                    mode as libc::mode_t,
                    u64::from(rdev),
                )
            }
        };
        if let Some(invalidated_inode) = invalidated_inode {
            self.after_invalidating_path(invalidated_inode, "Overwrote (via mknod)");
        }

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Set security context on node.
        if let Some(secctx) = extensions.secctx {
            if let Err(e) = self.do_mknod_mkdir_symlink_secctx(&parent_file, name, &secctx) {
                unsafe {
                    libc::unlinkat(parent_file.as_raw_fd(), name.as_ptr(), 0);
                };
                return Err(e);
            }
        }
        self.do_lookup(parent, name)
    }

    fn link(
        &self,
        _ctx: Context,
        inode: Inode,
        newparent: Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let new_inode = self.inodes.get(newparent).ok_or_else(ebadf)?;

        let inode_file = data.get_file()?;
        let newparent_file = new_inode.get_file()?;

        let procname = CString::new(format!("{}", inode_file.as_raw_fd()))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let invalidated_inode = self.before_invalidating_path(&new_inode, newname);
        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe {
            libc::linkat(
                self.proc_self_fd.as_raw_fd(),
                procname.as_ptr(),
                newparent_file.as_raw_fd(),
                newname.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if let Some(invalidated_inode) = invalidated_inode {
            self.after_invalidating_path(invalidated_inode, "Overwrote (via link)");
        }
        if res == 0 {
            self.do_lookup(newparent, newname)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let data = self.inodes.get(parent).ok_or_else(ebadf)?;
        let parent_file = data.get_file()?;

        let invalidated_inode = self.before_invalidating_path(&data, name);
        let res = {
            let _credentials_guard = self.unix_credentials_guard(&ctx, &extensions)?;

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe { libc::symlinkat(linkname.as_ptr(), parent_file.as_raw_fd(), name.as_ptr()) }
        };
        if let Some(invalidated_inode) = invalidated_inode {
            self.after_invalidating_path(invalidated_inode, "Overwrote (via symlink)");
        }

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Set security context on symlink.
        if let Some(secctx) = extensions.secctx {
            if let Err(e) = self.do_mknod_mkdir_symlink_secctx(&parent_file, name, &secctx) {
                unsafe {
                    libc::unlinkat(parent_file.as_raw_fd(), name.as_ptr(), 0);
                };
                return Err(e);
            }
        }

        self.do_lookup(parent, name)
    }

    fn readlink(&self, _ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let inode_file = data.get_file()?;

        let mut buf = vec![0; libc::PATH_MAX as usize];

        // Safe because this is a constant value and a valid C string.
        let empty = unsafe { CStr::from_bytes_with_nul_unchecked(EMPTY_CSTR) };

        // Safe because this will only modify the contents of `buf` and we check the return value.
        let res = unsafe {
            libc::readlinkat(
                inode_file.as_raw_fd(),
                empty.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        buf.resize(res as usize, 0);
        Ok(buf)
    }

    fn flush(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        let data = self.find_handle(handle, inode)?;

        // Since this method is called whenever an fd is closed in the client, we can emulate that
        // behavior by doing the same thing (dup-ing the fd and then immediately closing it). Safe
        // because this doesn't modify any memory and we check the return values.
        unsafe {
            let newfd = libc::dup(data.file.get()?.write().unwrap().as_raw_fd());
            if newfd < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::close(newfd) < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    fn fsync(&self, _ctx: Context, inode: Inode, datasync: bool, handle: Handle) -> io::Result<()> {
        let data = self.find_handle(handle, inode)?;

        let fd = data.file.get()?.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe {
            if datasync {
                libc::fdatasync(fd)
            } else {
                libc::fsync(fd)
            }
        };

        if res == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.fsync(ctx, inode, datasync, handle)
    }

    fn access(&self, ctx: Context, inode: Inode, mask: u32) -> io::Result<()> {
        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let inode_file = data.get_file()?;
        let st = statx(&inode_file, None)?.st;
        let mode = mask as i32 & (libc::R_OK | libc::W_OK | libc::X_OK);

        if mode == libc::F_OK {
            // The file exists since we were able to call `stat(2)` on it.
            return Ok(());
        }

        // We use ctx.uid/ctx.gid for these checks, but when idmapped mounts
        // support is enabled on the guest side, it means that "default_permissions"
        // flag is set on virtiofs mount and FUSE_ACCESS request should never be
        // sent to the userspace. Please, refer to the kernel commit
        // ("fs/fuse: warn if fuse_access is called when idmapped mounts are allowed").
        // In case when idmapped mounts are not enabled we are good to rely on ctx.uid/ctx.gid values.

        let st_uid = self.map_host_uid(HostUid::from(st.st_uid))?;
        let st_gid = self.map_host_gid(HostGid::from(st.st_gid))?;

        if (mode & libc::R_OK) != 0
            && !ctx.uid.is_root()
            && (st_uid != ctx.uid || st.st_mode & 0o400 == 0)
            && (st_gid != ctx.gid || st.st_mode & 0o040 == 0)
            && st.st_mode & 0o004 == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }

        if (mode & libc::W_OK) != 0
            && !ctx.uid.is_root()
            && (st_uid != ctx.uid || st.st_mode & 0o200 == 0)
            && (st_gid != ctx.gid || st.st_mode & 0o020 == 0)
            && st.st_mode & 0o002 == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }

        // root can only execute something if it is executable by one of the owner, the group, or
        // everyone.
        if (mode & libc::X_OK) != 0
            && (!ctx.uid.is_root() || st.st_mode & 0o111 == 0)
            && (st_uid != ctx.uid || st.st_mode & 0o100 == 0)
            && (st_gid != ctx.gid || st.st_mode & 0o010 == 0)
            && st.st_mode & 0o001 == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }

        Ok(())
    }

    fn setxattr(
        &self,
        _ctx: Context,
        inode: Inode,
        name: &CStr,
        value: &[u8],
        flags: u32,
        extra_flags: SetxattrFlags,
    ) -> io::Result<()> {
        if !self.cfg.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let name = self.map_client_xattrname(name)?;

        // If we are setting posix access acl and if SGID needs to be
        // cleared. Let's do it explicitly by calling a chmod() syscall.
        let must_clear_sgid = should_clear_sgid_for_setxattr(
            self.posix_acl.load(Ordering::Relaxed),
            extra_flags,
            name.as_ref(),
        );

        let file_type = FileType::from_mode(data.mode);
        let res = if is_safe_inode(data.mode) {
            // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
            // need to get a new fd.
            let file = self.open_inode(&data, libc::O_RDONLY | libc::O_NONBLOCK)?;

            self.clear_file_capabilities(file.as_raw_fd(), file_type, false)?;

            if must_clear_sgid {
                self.clear_sgid(&file, false)?;
            }

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    flags as libc::c_int,
                )
            }
        } else {
            let file = data.get_file()?;

            // No-op for non-regular files (is_safe_inode is true only for regular files and
            // directories). Kept for consistency.
            self.clear_file_capabilities(file.as_raw_fd(), file_type, true)?;

            if must_clear_sgid {
                self.clear_sgid(&file, true)?;
            }

            let procname = CString::new(format!("{}", file.as_raw_fd()))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let _working_dir_guard = self.switch_to_proc_self_fd();

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe {
                libc::setxattr(
                    procname.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    flags as libc::c_int,
                )
            }
        };
        if res == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn getxattr(
        &self,
        _ctx: Context,
        inode: Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        if !self.cfg.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        let mut buf = vec![0; size as usize];

        let name = self.map_client_xattrname(name).map_err(|e| {
            if e.kind() == ErrorKind::PermissionDenied {
                io::Error::from_raw_os_error(libc::ENODATA)
            } else {
                e
            }
        })?;

        let data = self.inodes.get(inode).ok_or_else(ebadf)?;

        let res = if is_safe_inode(data.mode) {
            // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
            // need to get a new fd.
            let file = self.open_inode(&data, libc::O_RDONLY | libc::O_NONBLOCK)?;

            // Safe because this will only modify the contents of `buf`.
            unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size as libc::size_t,
                )
            }
        } else {
            let file = data.get_file()?;

            let procname = CString::new(format!("{}", file.as_raw_fd()))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let _working_dir_guard = self.switch_to_proc_self_fd();

            // Safe because this will only modify the contents of `buf`.
            unsafe {
                libc::getxattr(
                    procname.as_ptr(),
                    name.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size as libc::size_t,
                )
            }
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        if size == 0 {
            Ok(GetxattrReply::Count(res as u32))
        } else {
            buf.resize(res as usize, 0);
            Ok(GetxattrReply::Value(buf))
        }
    }

    fn listxattr(&self, _ctx: Context, inode: Inode, size: u32) -> io::Result<ListxattrReply> {
        if !self.cfg.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let mut buf = vec![0; size as usize];

        let res = if is_safe_inode(data.mode) {
            // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
            // need to get a new fd.
            let file = self.open_inode(&data, libc::O_RDONLY | libc::O_NONBLOCK)?;

            // Safe because this will only modify the contents of `buf`.
            unsafe {
                libc::flistxattr(
                    file.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    size as libc::size_t,
                )
            }
        } else {
            let file = data.get_file()?;

            let procname = CString::new(format!("{}", file.as_raw_fd()))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let _working_dir_guard = self.switch_to_proc_self_fd();

            // Safe because this will only modify the contents of `buf`.
            unsafe {
                libc::listxattr(
                    procname.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    size as libc::size_t,
                )
            }
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        if size == 0 {
            Ok(ListxattrReply::Count(res as u32))
        } else {
            buf.resize(res as usize, 0);
            let buf = self.map_server_xattrlist(buf);
            Ok(ListxattrReply::Names(buf))
        }
    }

    fn removexattr(&self, _ctx: Context, inode: Inode, name: &CStr) -> io::Result<()> {
        if !self.cfg.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let name = self.map_client_xattrname(name)?;

        let res = if is_safe_inode(data.mode) {
            // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
            // need to get a new fd.
            let file = self.open_inode(&data, libc::O_RDONLY | libc::O_NONBLOCK)?;

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) }
        } else {
            let file = data.get_file()?;

            let procname = CString::new(format!("{}", file.as_raw_fd()))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let _working_dir_guard = self.switch_to_proc_self_fd();

            // Safe because this doesn't modify any memory and we check the return value.
            unsafe { libc::removexattr(procname.as_ptr(), name.as_ptr()) }
        };

        if res == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn fallocate(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        let data = self.find_handle(handle, inode)?;

        let fd = data.file.get()?.write().unwrap().as_raw_fd();
        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe {
            libc::fallocate64(
                fd,
                mode as libc::c_int,
                offset as libc::off64_t,
                length as libc::off64_t,
            )
        };
        if res == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn lseek(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        let data = self.find_handle(handle, inode)?;

        let fd = data.file.get()?.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::lseek(fd, offset as libc::off64_t, whence as libc::c_int) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res as u64)
        }
    }

    fn copyfilerange(
        &self,
        _ctx: Context,
        inode_in: Inode,
        handle_in: Handle,
        offset_in: u64,
        inode_out: Inode,
        handle_out: Handle,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> io::Result<usize> {
        let data_in = self.find_handle(handle_in, inode_in)?;

        // Take just a read lock as we're not going to alter the file descriptor offset.
        let fd_in = data_in.file.get()?.read().unwrap().as_raw_fd();

        let data_out = self.find_handle(handle_out, inode_out)?;

        // Take just a read lock as we're not going to alter the file descriptor offset.
        let fd_out = data_out.file.get()?.read().unwrap().as_raw_fd();

        // Safe because this will only modify `offset_in` and `offset_out` and we check
        // the return value.
        let res = unsafe {
            libc::syscall(
                libc::SYS_copy_file_range,
                fd_in,
                &mut (offset_in as i64) as &mut _ as *mut _,
                fd_out,
                &mut (offset_out as i64) as &mut _ as *mut _,
                len,
                flags,
            )
        };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res as usize)
        }
    }

    fn syncfs(&self, _ctx: Context, inode: Inode) -> io::Result<()> {
        // TODO: Branch here depending on whether virtiofsd announces submounts or not.

        let data = self.inodes.get(inode).ok_or_else(ebadf)?;
        let file = self.open_inode(&data, libc::O_RDONLY | libc::O_NOFOLLOW)?;
        let raw_fd = file.as_raw_fd();
        debug!("syncfs: inode={inode}, mount_fd={raw_fd}");
        let ret = unsafe { libc::syncfs(raw_fd) };
        if ret != 0 {
            // Thread-safe, because errno is stored in thread-local storage.
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl HandleDataFile {
    fn get(&self) -> io::Result<&'_ RwLock<GuestFile>> {
        match self {
            HandleDataFile::File(file) => Ok(file),
            HandleDataFile::Invalid(err) => Err(io::Error::new(
                err.kind(),
                format!("Handle is invalid because of an error during the preceding migration, which was: {err}"),
            )),
        }
    }
}

impl From<GuestFile> for HandleDataFile {
    fn from(file: GuestFile) -> Self {
        HandleDataFile::File(RwLock::new(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setxattr_non_utf8_name_does_not_clear_sgid() {
        let name = CStr::from_bytes_with_nul(b"user.\xff\0").unwrap();

        assert!(!should_clear_sgid_for_setxattr(
            true,
            SetxattrFlags::SETXATTR_ACL_KILL_SGID,
            name,
        ));
    }

    #[test]
    fn setxattr_posix_acl_access_clears_sgid() {
        let name = CStr::from_bytes_with_nul(b"system.posix_acl_access\0").unwrap();

        assert!(should_clear_sgid_for_setxattr(
            true,
            SetxattrFlags::SETXATTR_ACL_KILL_SGID,
            name,
        ));
    }
}
