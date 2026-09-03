// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

use log::*;
use passthrough::xattrmap::XattrMap;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::ffi::CString;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::{cmp, env, process};
use virtiofsd::idmap::{GidMap, UidMap};

use clap::{CommandFactory, Parser};

use vhost::vhost_user::Error::Disconnected;
use vhost::vhost_user::Listener;
use vhost_user_backend::Error::HandleRequest;
use vhost_user_backend::VhostUserDaemon;
use virtiofsd::filesystem::{FileSystem, SerializableFileSystem};
use virtiofsd::passthrough::read_only::PassthroughFsRo;
use virtiofsd::passthrough::{
    self, CachePolicy, InodeFileHandlesMode, MigrationMode, MigrationOnError, NegotiationMode,
    PassthroughFs,
};
use virtiofsd::sandbox::{Sandbox, SandboxMode};
use virtiofsd::seccomp::{enable_seccomp, SeccompAction};
use virtiofsd::util::write_pid_file;
use virtiofsd::vhost_user::{Error, VhostUserFsBackendBuilder, MAX_TAG_LEN};
use virtiofsd::{limits, oslib, soft_idmap};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

/// Maximum number of memory areas (slots) supported by the vhost-user-backend crate.
///
/// The constant has the same name there, but is not exported.
const MAX_MEM_SLOTS: u64 = 509;

/// How many file descriptors to reserve for internal use.
///
/// The exact value has been chosen mostly arbitrarily, but we know we need one FD per shared
/// memory area, which is where the `MAX_MEM_SLOTS` comes from.
///
/// Given how allocating FD towards the guest quota works, we also need to add one FD per thread in
/// our pool: FDs are created first, and only then accounted for.  All threads must be able to
/// simultaneously create an FD and then have it be accounted for, so we need to make room for as
/// many additional FDs as there are threads in the pool.  The thread pool size is set at runtime,
/// though, so cannot be taken into account here, but instead where this constant is used.
const INTERNAL_FD_RESERVE: u64 = MAX_MEM_SLOTS + 100;

type Result<T> = std::result::Result<T, Error>;

fn parse_seccomp(src: &str) -> std::result::Result<SeccompAction, &'static str> {
    Ok(match src {
        "none" => SeccompAction::Allow, // i.e. no seccomp
        "kill" => SeccompAction::Kill,
        "log" => SeccompAction::Log,
        "trap" => SeccompAction::Trap,
        _ => return Err("Matching variant not found"),
    })
}

/// On the command line, we want to allow aliases for `InodeFileHandlesMode` values.  This enum has
/// all values allowed on the command line, and with `From`/`Into`, it can be translated into the
/// internally used `InodeFileHandlesMode` enum.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum InodeFileHandlesCommandLineMode {
    /// `InodeFileHandlesMode::Never`
    Never,
    /// Alias for `InodeFileHandlesMode::Prefer`
    Fallback,
    /// `InodeFileHandlesMode::Prefer`
    Prefer,
    /// `InodeFileHandlesMode::Mandatory`
    Mandatory,
}

impl From<InodeFileHandlesCommandLineMode> for InodeFileHandlesMode {
    fn from(clm: InodeFileHandlesCommandLineMode) -> Self {
        match clm {
            InodeFileHandlesCommandLineMode::Never => InodeFileHandlesMode::Never,
            InodeFileHandlesCommandLineMode::Fallback => InodeFileHandlesMode::Prefer,
            InodeFileHandlesCommandLineMode::Prefer => InodeFileHandlesMode::Prefer,
            InodeFileHandlesCommandLineMode::Mandatory => InodeFileHandlesMode::Mandatory,
        }
    }
}

impl FromStr for InodeFileHandlesCommandLineMode {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "never" => Ok(InodeFileHandlesCommandLineMode::Never),
            "fallback" => Ok(InodeFileHandlesCommandLineMode::Fallback),
            "prefer" => Ok(InodeFileHandlesCommandLineMode::Prefer),
            "mandatory" => Ok(InodeFileHandlesCommandLineMode::Mandatory),

            _ => Err("invalid inode file handles mode"),
        }
    }
}

fn parse_tag(tag: &str) -> Result<String> {
    if !tag.is_empty() && tag.len() <= MAX_TAG_LEN {
        Ok(tag.into())
    } else {
        Err(Error::InvalidTag)
    }
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "virtiofsd",
    about = "Launch a virtiofsd backend.",
    version,
    args_override_self = true,
    arg_required_else_help = true,
    override_usage = "virtiofsd --shared-dir <SHARED_DIR> --socket-path <SOCKET_PATH> [OPTIONS]"
)]
struct Opt {
    /// Shared directory path
    #[arg(long, required_unless_present_any = &["compat_options", "print_capabilities"])]
    shared_dir: Option<String>,

    /// The tag that the virtio device advertises
    ///
    /// Setting this option will enable advertising of
    /// VHOST_USER_PROTOCOL_F_CONFIG. However, the vhost-user frontend of your
    /// hypervisor may not negotiate this feature and (or) ignore this value.
    /// Notably, QEMU currently (as of 8.1) ignores the CONFIG feature. QEMU
    /// versions from 7.1 to 8.0 will crash while attempting to log a warning
    /// about not supporting the feature.
    #[arg(long, value_parser = parse_tag)]
    tag: Option<String>,

    /// vhost-user socket path [deprecated]
    #[arg(long, required_unless_present_any = &["fd", "socket_path", "print_capabilities"])]
    socket: Option<String>,

    /// vhost-user socket path
    #[arg(long = "socket-path", required_unless_present_any = &["fd", "socket", "print_capabilities"])]
    socket_path: Option<String>,

    /// Name of group for the vhost-user socket
    #[arg(long = "socket-group", conflicts_with_all = &["fd", "print_capabilities"])]
    socket_group: Option<String>,

    /// File descriptor for the listening (not yet connected) socket
    #[arg(long, required_unless_present_any = &["socket", "socket_path", "print_capabilities"], conflicts_with_all = &["socket_path", "socket"])]
    fd: Option<RawFd>,

    /// Maximum thread pool size. A value of "0" disables the pool
    #[arg(long, default_value = "0")]
    thread_pool_size: usize,

    /// Enable support for extended attributes
    #[arg(long)]
    xattr: bool,

    /// Enable support for posix ACLs (implies --xattr)
    #[arg(long, require_equals = true, default_value = "never", default_missing_value = "always", num_args = 0..=1, value_name = "never|auto|always")]
    posix_acl: NegotiationMode,

    /// Add custom rules for translating extended attributes between host and guest
    /// (e.g. :map::user.virtiofs.:)
    #[arg(long, value_parser = |s: &_| XattrMap::try_from(s))]
    xattrmap: Option<XattrMap>,

    /// Sandbox mechanism to isolate the daemon process (namespace, chroot, none)
    #[arg(long, default_value = "namespace")]
    sandbox: SandboxMode,

    /// Prevent the guest from making modifications to the filesystem.
    #[arg(long)]
    readonly: bool,

    /// Action to take when seccomp finds a not allowed syscall (none, kill, log, trap)
    #[arg(long, value_parser = parse_seccomp, default_value = "kill")]
    seccomp: SeccompAction,

    /// Tell the guest which directories are mount points [default]
    #[arg(long)]
    announce_submounts: bool,

    /// Do not tell the guest which directories are mount points
    #[arg(long, overrides_with("announce_submounts"))]
    no_announce_submounts: bool,

    /// When to use file handles to reference inodes instead of O_PATH file descriptors (never,
    /// prefer, mandatory)
    ///
    /// - never: Never use file handles, always use O_PATH file descriptors.
    ///
    /// - prefer: Attempt to generate file handles, but fall back to O_PATH file descriptors where
    ///   the underlying filesystem does not support file handles.  Useful when there are various
    ///   different filesystems under the shared directory and some of them do not support file
    ///   handles.  ("fallback" is a deprecated alias for "prefer".)
    ///
    /// - mandatory: Always use file handles, never fall back to O_PATH file descriptors.
    ///
    /// Using file handles reduces the number of file descriptors virtiofsd keeps open, which is
    /// not only helpful with resources, but may also be important in cases where virtiofsd should
    /// only have file descriptors open for files that are open in the guest, e.g. to get around
    /// bad interactions with NFS's silly renaming.
    #[arg(long, require_equals = true, default_value = "prefer")]
    inode_file_handles: InodeFileHandlesCommandLineMode,

    /// The caching policy the file system should use (auto, always, never, metadata)
    #[arg(long, default_value = "auto")]
    cache: CachePolicy,

    /// When used with --cache={metadata, never} will allow shared files to be mmap'd.
    /// Regardless of the selected cache policy, this option should only be enabled
    /// when the file system has exclusive access to the directory.
    #[arg(long)]
    allow_mmap: bool,

    /// Disable support for READDIRPLUS operations
    #[arg(long)]
    no_readdirplus: bool,

    /// Enable writeback cache
    #[arg(long)]
    writeback: bool,

    /// Honor the O_DIRECT flag passed down by guest applications
    #[arg(long)]
    allow_direct_io: bool,

    /// Print vhost-user.json backend program capabilities and exit
    #[arg(long = "print-capabilities")]
    print_capabilities: bool,

    /// Modify the list of capabilities, e.g., --modcaps=+sys_admin:-chown
    #[arg(long)]
    modcaps: Option<String>,

    /// Log level (error, warn, info, debug, trace, off)
    #[arg(long = "log-level", default_value = "info")]
    log_level: LevelFilter,

    /// Log to syslog [default: stderr]
    #[arg(long)]
    syslog: bool,

    /// Set maximum number of file descriptors (0 leaves rlimit unchanged)
    /// [default: min(1000000, '/proc/sys/fs/nr_open')]
    #[arg(long = "rlimit-nofile")]
    rlimit_nofile: Option<u64>,

    /// Options in a format compatible with the legacy implementation [deprecated]
    #[arg(short = 'o')]
    compat_options: Option<Vec<String>>,

    /// Set log level to "debug" [deprecated]
    #[arg(short = 'd')]
    compat_debug: bool,

    /// Disable KILLPRIV V2 support [default]
    #[arg(long)]
    _no_killpriv_v2: bool,

    /// Enable KILLPRIV V2 support
    #[arg(long, overrides_with("_no_killpriv_v2"))]
    killpriv_v2: bool,

    /// Compatibility option that has no effect [deprecated]
    #[arg(short = 'f')]
    compat_foreground: bool,

    /// Enable security label support (implies --xattr).
    /// If enabled, expects SELinux xattr on file creation
    /// from client and stores it in the newly created file.
    #[arg(long = "security-label", require_equals = true, default_value = "never", default_missing_value = "always", num_args = 0..=1, value_name = "never|auto|always")]
    security_label: NegotiationMode,

    /// Map a range of UIDs from the host into the namespace, given as
    /// :namespace_uid:host_uid:count:
    ///
    /// As opposed to '--translate-uid', this mapping is not done by virtiofsd, but by the
    /// user namespace into which virtiofsd is placed via '--sandbox=namespace'.
    ///
    /// For example, :0:100000:65536: will map the 65536 host UIDs [100000, 165535]
    /// into the namespace as [0, 65535].
    ///
    /// Provide this argument multiple times to map multiple UID ranges.
    #[arg(long)]
    uid_map: Vec<UidMap>,

    /// Map a range of GIDs from the host into the namespace, given as
    /// :namespace_gid:host_gid:count:
    ///
    /// As opposed to '--translate-gid', this mapping is not done by virtiofsd, but by the
    /// user namespace into which virtiofsd is placed via '--sandbox=namespace'.
    ///
    /// For example, :0:100000:65536: will map the 65536 host GIDs [100000, 165535]
    /// into the namespace as [0, 65535].
    ///
    /// Provide this argument multiple times to map multiple GID ranges.
    #[arg(long)]
    gid_map: Vec<GidMap>,

    /// Describe how to translate UIDs between guest and host, given as
    /// '<type>:<source base>:<target base>:<count>'.
    ///
    /// As opposed to '--uid-map', this mapping is done internally by virtiofsd, and does not
    /// require using a user namespace.
    ///
    /// 'type' describes how to do the mapping, and in which direction:
    ///
    /// - 'guest': 1:1 map a range of guest UIDs to host UIDs
    ///
    /// - 'host': 1:1 map a range of host UIDs to guest UIDs
    ///
    /// - 'squash-guest': n:1 map a range of guest UIDs all to a single host UID
    ///
    /// - 'squash-host': n:1 map a range of host UIDs all to a single guest UID
    ///
    /// - 'forbid-guest': Forbid guest UIDs in the given range: Return an error to the guest
    ///   whenever it tries to create a file with such a UID or make a file have such a UID
    ///
    /// - 'map': bidirectionally 1:1 map between a range of guest UIDs and host UIDs; the
    ///   order is: 'map:<guest base>:<host base>:<count>'
    ///
    /// Provide this argument multiple times to map multiple UID ranges.
    ///
    /// Cannot be used together with --posix-acl=always|auto; translating UIDs (or GIDs) in
    /// virtiofsd would break posix ACLs.
    #[arg(long)]
    translate_uid: Vec<soft_idmap::cmdline::IdMap>,

    /// Same as '--translate-uid', but for GIDs.
    #[arg(long)]
    translate_gid: Vec<soft_idmap::cmdline::IdMap>,

    /// Preserve O_NOATIME behavior, otherwise automatically clean up O_NOATIME flag to prevent
    /// potential permission errors when running in unprivileged mode (e.g., when accessing files
    /// without having ownership/capability to use O_NOATIME).
    #[arg(long = "preserve-noatime")]
    preserve_noatime: bool,

    /// Defines how to perform migration, i.e. how to represent the internal state to the
    /// destination, and how to obtain that representation.
    ///
    /// - find-paths: Obtain paths for all inodes indexed and opened by the guest, and transfer
    ///   those paths to the destination.  To get those paths, we try to read the symbolic links in
    ///   /proc/self/fd first; if that does not work, we will fall back to iterating through the
    ///   shared directory (exhaustive search), enumerating all paths within.
    ///
    /// - file-handles: Pass file handles.  For this to work, source and destination instance must
    ///   operate on exactly the same shared directory on the same filesystem (which may be a
    ///   network filesystem, mounted on different hosts).  The destination instance must have the
    ///   capability to open file handles, i.e. CAP_DAC_READ_SEARCH -- generally, this requires
    ///   running virtiofsd as root and use `--modcaps=+dac_read_search`.
    ///
    /// This parameter is ignored on the destination side.
    #[arg(long = "migration-mode", default_value = "find-paths")]
    migration_mode: MigrationMode,

    /// Controls how to respond to errors during migration.
    ///
    /// If any inode turns out not to be migrateable (either the source cannot serialize it, or the
    /// destination cannot opened the serialized representation), the destination can react in
    /// different ways:
    ///
    /// - abort: Whenever any error occurs, return a hard error to the vhost-user front-end (e.g.
    ///   QEMU), aborting migration.
    ///
    /// - guest-error: Let migration finish, but the guest will be unable to access any of the
    ///   affected inodes, receiving only errors.
    ///
    /// This parameter is ignored on the source side.
    #[arg(long = "migration-on-error", default_value = "abort")]
    migration_on_error: MigrationOnError,

    /// Only for find-paths migration mode: Ensure that the migration destination opens the very
    /// same inodes as the source (only works if source and destination use the same shared
    /// directory on the same filesystem).
    ///
    /// This option makes the source attach the respective file handle to each inode transferred
    /// during migration.  Once the destination has (re-)opened the inode, it will generate the
    /// file handle on its end, and compare, ensuring that it has opened the very same inode.
    ///
    /// (File handles are per-filesystem unique identifiers for inodes that, besides the inode ID,
    /// also include a generation ID to protect against inode ID reuse.)
    ///
    /// Using this option protects against external parties renaming or replacing inodes
    /// while migration is ongoing, which, without this option, can lead to data loss or
    /// corruption, so it should always be used when other processes besides virtiofsd have write
    /// access to the shared directory.  However, again, it only works if both source and
    /// destination use the same shared directory.
    ///
    /// This parameter is ignored on the destination side.
    #[arg(long = "migration-verify-handles")]
    migration_verify_handles: bool,

    /// Only for find-paths migration mode: Double-check the identity of inodes right before
    /// switching over to the destination, potentially making migration more resilient when third
    /// parties have write access to the shared directory.
    ///
    /// When representing migrated inodes using their paths relative to the shared directory,
    /// double-check during switch-over to the destination that each path still matches the
    /// respective inode, and on mismatch, try to correct it via the respective symlink in
    /// /proc/self/fd.
    ///
    /// Because this option requires accessing each inode indexed or opened by the guest, it can
    /// prolong the switch-over phase of migration (when both source and destination are paused)
    /// for an indeterminate amount of time.
    ///
    /// This parameter is ignored on the destination side.
    #[arg(long = "migration-confirm-paths")]
    migration_confirm_paths: bool,
}

fn parse_compat(opt: Opt) -> Opt {
    use clap::error::ErrorKind;
    fn value_error(arg: &str, value: &str) -> ! {
        <Opt as CommandFactory>::command()
            .error(
                ErrorKind::InvalidValue,
                format!("Invalid compat value '{value}' for '-o {arg}'"),
            )
            .exit()
    }
    fn argument_error(arg: &str) -> ! {
        <Opt as CommandFactory>::command()
            .error(
                ErrorKind::UnknownArgument,
                format!("Invalid compat argument '-o {arg}'"),
            )
            .exit()
    }

    fn parse_tuple(opt: &mut Opt, tuple: &str) {
        match tuple.split('=').collect::<Vec<&str>>()[..] {
            ["xattrmap", value] => {
                opt.xattrmap = Some(
                    XattrMap::try_from(value).unwrap_or_else(|_| value_error("xattrmap", value)),
                )
            }
            ["cache", value] => match value {
                "auto" => opt.cache = CachePolicy::Auto,
                "always" => opt.cache = CachePolicy::Always,
                "none" => opt.cache = CachePolicy::Never,
                "metadata" => opt.cache = CachePolicy::Metadata,
                _ => value_error("cache", value),
            },
            ["loglevel", value] => match value {
                "debug" => opt.log_level = LevelFilter::Debug,
                "info" => opt.log_level = LevelFilter::Info,
                "warn" => opt.log_level = LevelFilter::Warn,
                "err" => opt.log_level = LevelFilter::Error,
                _ => value_error("loglevel", value),
            },
            ["sandbox", value] => match value {
                "namespace" => opt.sandbox = SandboxMode::Namespace,
                "chroot" => opt.sandbox = SandboxMode::Chroot,
                _ => value_error("sandbox", value),
            },
            ["source", value] => opt.shared_dir = Some(value.to_string()),
            ["modcaps", value] => opt.modcaps = Some(value.to_string()),
            _ => argument_error(tuple),
        }
    }

    fn parse_single(opt: &mut Opt, option: &str) {
        match option {
            "xattr" => opt.xattr = true,
            "no_xattr" => opt.xattr = false,
            "readdirplus" => opt.no_readdirplus = false,
            "no_readdirplus" => opt.no_readdirplus = true,
            "writeback" => opt.writeback = true,
            "no_writeback" => opt.writeback = false,
            "allow_direct_io" => opt.allow_direct_io = true,
            "no_allow_direct_io" => opt.allow_direct_io = false,
            "announce_submounts" => opt.announce_submounts = true,
            "killpriv_v2" => opt.killpriv_v2 = true,
            "no_killpriv_v2" => opt.killpriv_v2 = false,
            "posix_acl" => opt.posix_acl = NegotiationMode::Always,
            "no_posix_acl" => opt.posix_acl = NegotiationMode::Never,
            "security_label" => opt.security_label = NegotiationMode::Always,
            "no_security_label" => opt.security_label = NegotiationMode::Never,
            "no_posix_lock" | "no_flock" => (),
            _ => argument_error(option),
        }
    }

    let mut clean_opt = opt.clone();

    if let Some(compat_options) = opt.compat_options.as_ref() {
        for line in compat_options {
            for option in line.split(',') {
                if option.contains('=') {
                    parse_tuple(&mut clean_opt, option);
                } else {
                    parse_single(&mut clean_opt, option);
                }
            }
        }
    }

    clean_opt
}

fn print_capabilities() {
    println!("{{");
    println!("  \"type\": \"fs\",");
    println!("  \"features\": [");
    println!("    \"migrate-precopy\",");
    println!("    \"posix-acl-negotiation-mode\",");
    println!("    \"security-label-negotiation-mode\",");
    println!("    \"separate-options\"");
    println!("  ]");
    println!("}}");
}

fn set_default_logger(log_level: LevelFilter) {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", log_level.to_string());
    }
    env_logger::init();
}

fn initialize_logging(opt: &Opt) {
    let log_level = if opt.compat_debug {
        LevelFilter::Debug
    } else {
        opt.log_level
    };

    if opt.syslog {
        if let Err(e) = syslog::init(syslog::Facility::LOG_USER, log_level, None) {
            set_default_logger(log_level);
            warn!("can't enable syslog: {e}");
        }
    } else {
        set_default_logger(log_level);
    }
}

fn set_signal_handlers() {
    use vmm_sys_util::signal;

    extern "C" fn handle_signal(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
        unsafe { libc::_exit(1) };
    }
    let signals = vec![libc::SIGHUP, libc::SIGTERM];
    for s in signals {
        if let Err(e) = signal::register_signal_handler(s, handle_signal) {
            error!("Setting signal handlers: {e}");
            process::exit(1);
        }
    }
}

fn parse_modcaps(
    default_caps: Vec<&str>,
    modcaps: Option<String>,
) -> (HashSet<String>, HashSet<String>) {
    let mut required_caps: HashSet<String> = default_caps.iter().map(|&s| s.into()).collect();
    let mut disabled_caps = HashSet::new();

    if let Some(modcaps) = modcaps {
        for modcap in modcaps.split(':').map(str::to_string) {
            if modcap.is_empty() {
                error!("empty modcap found: expected (+|-)capability:...");
                process::exit(1);
            }
            let (action, cap_name) = modcap.split_at(1);
            let cap_name = cap_name.to_uppercase();
            if !matches!(action, "+" | "-") {
                error!("invalid modcap action: expecting '+'|'-' but found '{action}'");
                process::exit(1);
            }
            if let Err(error) = capng::name_to_capability(&cap_name) {
                error!("invalid capability '{cap_name}': {error}");
                process::exit(1);
            }

            match action {
                "+" => {
                    disabled_caps.remove(&cap_name);
                    required_caps.insert(cap_name);
                }
                "-" => {
                    required_caps.remove(&cap_name);
                    disabled_caps.insert(cap_name);
                }
                _ => unreachable!(),
            }
        }
    }
    (required_caps, disabled_caps)
}

fn drop_capabilities(inode_file_handles: InodeFileHandlesMode, modcaps: Option<String>) {
    let default_caps = vec![
        "CHOWN",
        "DAC_OVERRIDE",
        "FOWNER",
        "FSETID",
        "SETGID",
        "SETUID",
        "MKNOD",
        "SETFCAP",
    ];
    let (mut required_caps, disabled_caps) = parse_modcaps(default_caps, modcaps);

    if inode_file_handles != InodeFileHandlesMode::Never {
        let required_cap = "DAC_READ_SEARCH".to_owned();
        if disabled_caps.contains(&required_cap) {
            error!("can't disable {required_cap} when using --inode-file-handles={inode_file_handles:?}");
            process::exit(1);
        }
        required_caps.insert(required_cap);
    }

    capng::clear(capng::Set::BOTH);
    // Configure the required set of capabilities for the child, and leave the
    // parent with none.
    if let Err(e) = capng::updatev(
        capng::Action::ADD,
        capng::Type::PERMITTED | capng::Type::EFFECTIVE,
        required_caps.iter().map(String::as_str).collect(),
    ) {
        error!("can't set up the child capabilities: {e}");
        process::exit(1);
    }
    if let Err(e) = capng::apply(capng::Set::BOTH) {
        error!("can't apply the child capabilities: {e}");
        process::exit(1);
    }
}

fn main() {
    let opt = parse_compat(Opt::parse());

    // Enable killpriv_v2 only if user explicitly asked for it by using
    // --killpriv-v2 or -o killpriv_v2. Otherwise disable it by default.
    let killpriv_v2 = opt.killpriv_v2;

    // Disable announce submounts if the user asked for it
    let announce_submounts = !opt.no_announce_submounts;

    if opt.print_capabilities {
        print_capabilities();
        return;
    }

    initialize_logging(&opt);
    set_signal_handlers();

    let shared_dir = match opt.shared_dir.as_ref() {
        Some(s) => s,
        None => {
            error!("missing \"--shared-dir\" or \"-o source\" option");
            process::exit(1);
        }
    };

    let shadir_path = Path::new(shared_dir);
    if !shadir_path.is_dir() && !shadir_path.is_file() {
        error!("{shared_dir} does not exist");
        process::exit(1);
    }

    if opt.compat_foreground {
        warn!("Use of deprecated flag '-f': This flag has no effect, please remove it");
    }
    if opt.compat_debug {
        warn!("Use of deprecated flag '-d': Please use the '--log-level debug' option instead");
    }
    if opt.compat_options.is_some() {
        warn!("Use of deprecated option format '-o': Please specify options without it (e.g., '--cache auto' instead of '-o cache=auto')");
    }
    if opt.inode_file_handles == InodeFileHandlesCommandLineMode::Fallback {
        warn!("Use of deprecated value 'fallback' for '--inode-file-handles': Please use 'prefer' instead");
    }

    // Check migration argument compatibility
    match opt.migration_mode {
        MigrationMode::FindPaths => (), // all allowed

        MigrationMode::FileHandles => {
            if opt.migration_confirm_paths || opt.migration_verify_handles {
                if opt.migration_confirm_paths {
                    error!("Cannot use --migration-confirm-paths with --migration-mode=file-handles (because it is unnecessary)");
                }
                if opt.migration_verify_handles {
                    error!("Cannot use --migration-verify-handles with --migration-mode=file-handles (because it is unnecessary)");
                }
                process::exit(1);
            }
        }
    }

    if opt.posix_acl != NegotiationMode::Never
        && (!opt.translate_uid.is_empty() || !opt.translate_gid.is_empty())
    {
        error!(
            "Cannot use --translate-uid or --translate-gid with --posix-acl={}, \
             translating UIDs/GIDs would break posix ACLs",
            opt.posix_acl
        );
        process::exit(1);
    }

    let xattrmap = opt.xattrmap.clone();
    let xattr = xattrmap.is_some()
        || opt.posix_acl != NegotiationMode::Never
        || opt.security_label != NegotiationMode::Never
        || opt.xattr;
    let thread_pool_size = opt.thread_pool_size;
    let readdirplus = match opt.cache {
        CachePolicy::Never => false,
        _ => !opt.no_readdirplus,
    };

    let timeout = match opt.cache {
        CachePolicy::Never => Duration::from_secs(0),
        CachePolicy::Metadata => Duration::from_secs(86400),
        CachePolicy::Auto => Duration::from_secs(1),
        CachePolicy::Always => Duration::from_secs(86400),
    };

    let umask = if opt.socket_group.is_some() {
        libc::S_IROTH | libc::S_IWOTH | libc::S_IXOTH
    } else {
        libc::S_IRGRP
            | libc::S_IWGRP
            | libc::S_IXGRP
            | libc::S_IROTH
            | libc::S_IWOTH
            | libc::S_IXOTH
    };

    // We need to keep _pid_file around because it maintains a lock on the pid file
    // that prevents another daemon from using the same pid file.
    let (listener, socket_path, _pid_file) = match opt.fd.as_ref() {
        Some(fd) => unsafe { (Listener::from_raw_fd(*fd), None, None) },
        None => {
            // Set umask to ensure the socket is created with the right permissions
            let _umask_guard = oslib::ScopedUmask::new(umask);

            let socket = opt.socket_path.as_ref().unwrap_or_else(|| {
                warn!("use of deprecated parameter '--socket': Please use the '--socket-path' option instead");
                opt.socket.as_ref().unwrap() // safe to unwrap because clap ensures either --socket or --socket-path are passed
            });

            let socket_parent_dir = Path::new(socket).parent().unwrap_or_else(|| {
                error!("Invalid socket file name");
                process::exit(1);
            });

            if !socket_parent_dir.as_os_str().is_empty() && !socket_parent_dir.exists() {
                error!(
                    "{} does not exist or is not a directory",
                    socket_parent_dir.to_string_lossy()
                );
                process::exit(1);
            }

            let pid_file_name = socket.to_owned() + ".pid";
            let pid_file_path = Path::new(pid_file_name.as_str());
            let pid_file = write_pid_file(pid_file_path).unwrap_or_else(|error| {
                error!("Error creating pid file '{pid_file_name}': {error}");
                process::exit(1);
            });

            let listener = Listener::new(socket, true).unwrap_or_else(|error| {
                error!("Error creating listener: {error}");
                process::exit(1);
            });

            (listener, Some(socket.clone()), Some(pid_file))
        }
    };

    if let Some(group_name) = opt.socket_group {
        let c_name = CString::new(group_name).expect("invalid group name");
        let group = unsafe { libc::getgrnam(c_name.as_ptr()) };
        if group.is_null() {
            error!("Couldn't resolve the group name specified for the socket path");
            process::exit(1);
        }

        // safe to unwrap because clap ensures --socket-group can't be specified alongside --fd
        let c_socket_path = CString::new(socket_path.unwrap()).expect("invalid socket path");
        let ret = unsafe { libc::chown(c_socket_path.as_ptr(), u32::MAX, (*group).gr_gid) };
        if ret != 0 {
            error!(
                "Couldn't set up the group for the socket path: {}",
                std::io::Error::last_os_error()
            );
            process::exit(1);
        }
    }

    let fd_count_limit = limits::setup_rlimit_nofile(opt.rlimit_nofile).unwrap_or_else(|error| {
        error!("Error increasing number of open files: {error}");
        process::exit(1)
    });

    // Account for guest FDs that are created first and only accounted for then (see doc comment on
    // `INTERNAL_FD_RESERVE`
    let internal_fd_reserve = INTERNAL_FD_RESERVE + cmp::max(opt.thread_pool_size as u64, 1);
    let guest_fd_limit = fd_count_limit.checked_sub(internal_fd_reserve).unwrap_or_else(|| {
        error!("Maximum number of file descriptors too small: Limit is {fd_count_limit}, must be at least {internal_fd_reserve}");
        process::exit(1)
    });

    // Warn the user if there is a suspiciously low limit on the guest FD count that will make it
    // hard to actually do something; the number of `128` is completely arbitrary.
    if guest_fd_limit < 128 {
        warn!("File descriptor count limit is very small, leaving only {guest_fd_limit} file descriptors for the guest");
    }

    let mut sandbox = Sandbox::new(
        shared_dir.to_string(),
        opt.sandbox,
        opt.uid_map,
        opt.gid_map,
    );

    let mountinfo_prefix = sandbox.get_mountinfo_prefix().unwrap_or_else(|error| {
        error!("Error getting mountinfo prefix: {error}");
        process::exit(1)
    });

    // Enter the sandbox, from this point the process will be isolated (or not)
    // as chosen in '--sandbox'.
    let listener = sandbox.enter(listener).unwrap_or_else(|error| {
        error!("Error entering sandbox: {error}");
        process::exit(1)
    });

    let fs_cfg = passthrough::Config {
        entry_timeout: timeout,
        attr_timeout: timeout,
        cache_policy: opt.cache,
        root_dir: sandbox.get_root_dir(),
        mountinfo_prefix,
        xattr,
        xattrmap,
        proc_sfd_rawfd: sandbox.get_proc_self_fd(),
        proc_mountinfo_rawfd: sandbox.get_mountinfo_fd(),
        announce_submounts,
        inode_file_handles: opt.inode_file_handles.into(),
        readdirplus,
        writeback: opt.writeback,
        allow_direct_io: opt.allow_direct_io,
        killpriv_v2,
        security_label: opt.security_label,
        posix_acl: opt.posix_acl,
        clean_noatime: !opt.preserve_noatime,
        allow_mmap: opt.allow_mmap,
        migration_on_error: opt.migration_on_error,
        migration_verify_handles: opt.migration_verify_handles,
        migration_confirm_paths: opt.migration_confirm_paths,
        migration_mode: opt.migration_mode,
        uid_map: Some(opt.translate_uid),
        gid_map: Some(opt.translate_gid),
        guest_fd_limit,
        ..Default::default()
    };

    // Must happen before we start the thread pool
    match opt.seccomp {
        SeccompAction::Allow => {}
        _ => enable_seccomp(opt.seccomp, opt.syslog).unwrap(),
    }

    // We don't modify the capabilities if the user call us without
    // any sandbox (i.e. --sandbox=none) as unprivileged user
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        drop_capabilities(fs_cfg.inode_file_handles, opt.modcaps);
    }

    if opt.readonly {
        let fs = PassthroughFsRo::new(fs_cfg).unwrap_or_else(|e| {
            error!("Failed to create internal filesystem representation: {e}");
            process::exit(1);
        });
        run_generic_fs(fs, listener, thread_pool_size, opt.tag);
    } else {
        let fs = PassthroughFs::new(fs_cfg).unwrap_or_else(|e| {
            error!("Failed to create internal filesystem representation: {e}");
            process::exit(1);
        });
        run_generic_fs(fs, listener, thread_pool_size, opt.tag);
    }
}

// Use a generic function for the main loop so we don't need to use Box<dyn FileSystem>
fn run_generic_fs<F: FileSystem + SerializableFileSystem + Send + Sync + 'static>(
    fs: F,
    mut listener: Listener,
    thread_pool_size: usize,
    tag: Option<String>,
) {
    let fs_backend = Arc::new(
        VhostUserFsBackendBuilder::default()
            .set_thread_pool_size(thread_pool_size)
            .set_tag(tag)
            .build(fs)
            .unwrap_or_else(|error| {
                error!("Error creating vhost-user backend: {error}");
                process::exit(1)
            }),
    );

    let mut daemon = VhostUserDaemon::new(
        String::from("virtiofsd-backend"),
        fs_backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .unwrap();

    info!("Waiting for vhost-user socket connection...");

    if let Err(e) = daemon.start(&mut listener) {
        error!("Failed to start daemon: {e:?}");
        process::exit(1);
    }

    info!("Client connected, servicing requests");

    if let Err(e) = daemon.wait() {
        match e {
            HandleRequest(Disconnected) => info!("Client disconnected, shutting down"),
            _ => error!("Waiting for daemon failed: {e:?}"),
        }
    }
}
