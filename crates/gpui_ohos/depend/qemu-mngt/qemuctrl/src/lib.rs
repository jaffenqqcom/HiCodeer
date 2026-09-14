//! In-process QEMU manager for HarmonyOS NEXT.
//!
//! Boots the embedded aarch64 guest (`dlopen("libqemu-system-aarch64.so")`) on
//! its own thread and manages the virtio-fs mounts that expose host directories
//! to it. There is deliberately **no SSH / command-execution code** here: guest
//! commands run through `cmd-client` against a daemon that lives inside the
//! guest. File sharing is virtio-fs (see the virtio-fs design doc).

pub mod engine;
pub mod qmp;
#[cfg(target_env = "ohos")]
pub mod mount;
#[cfg(target_env = "ohos")]
pub mod virtiofs;

#[cfg(target_env = "ohos")]
pub use mount::{GuestShell, MountRegistry};

pub use engine::RestartHook;

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Name of the QEMU engine library shipped in the HAP native-libs directory.
pub const QEMU_LIB_NAME: &str = "libqemu-system-aarch64.so";
/// Symbol exported by the QEMU engine that takes over the current thread and
/// runs the machine until it exits.
pub const QEMU_ENTRY_SYMBOL: &[u8] = b"main\0";

/// Machine type matching the proven guest.
pub const MACHINE_TYPE: &str = "virt";
/// Machine options, adopted from HiSH's QEMU launch line.
///
/// What each one does for THIS guest (checked against the guest kernel config
/// in `images/linux-6.12.60.config` and the QEMU sources in-tree):
///   - `memory-backend=mem` binds the machine RAM to the memfd object built in
///     `build_argv`. It has to stay a memfd: the sandbox mount is a
///     vhost-user-fs device, and vhost-user requires guest RAM that can be
///     handed to the backend as a file descriptor.
///   - `gic-version=max` resolves to GICv3 here. Without it the machine takes
///     its "no selection" default and, for eight or fewer vCPUs, picks GICv2.
///     GICv3 acknowledges interrupts through system registers instead of the
///     GICv2 MMIO EOI page, and interrupt handling is where this syscall-heavy
///     guest spends its time. `virtualization=off` keeps GICv4 out of the
///     max-resolution, and the guest kernel has CONFIG_ARM_GIC_V3=y.
///   - `iommu=none`, `usb=off`, `virtualization=off`: nothing in this guest uses
///     them. The MSI controller is deliberately left at its `auto` default, and
///     `its` is NOT set to `off`: `pcie-root-port` requires MSI-X, and under
///     GICv3 (what `gic-version=max` resolves to here) `its=off` makes
///     `finalize_msi_controller()` in `hw/arm/virt.c` select
///     `VIRT_MSI_CTRL_NONE`. That leaves the global `msi_nonbroken` flag false,
///     so every `pcie-root-port` fails to realize with "MSI-X is not supported
///     by interrupt controller" and QEMU exits. `auto` resolves to ITS here,
///     which sets `msi_nonbroken`; the guest kernel supports it through
///     `CONFIG_ARM_GIC_V3_ITS=y`.
///   - `compact-highmem=on`, `hmat=off`: no high-memory region bookkeeping.
///   - `dump-guest-core=off`: a guest crash must never try to core-dump the
///     whole guest RAM out of the app process.
///   - `mem-merge=off`: KSM on guest RAM turns guest writes into COW faults. The
///     backend picks this up at construction (`hostmem.c`), so it covers the
///     memfd object as well.
///
/// Deliberately NOT adopted: `acpi=on`. QEMU only builds ACPI tables when a
/// firmware loaded the kernel, and this machine is started with `-kernel`, so
/// the option would be inert - while still feeding the FDT topology branches in
/// `hw/arm/virt.c`.
pub const MACHINE_OPTIONS: &str = "memory-backend=mem,gic-version=max,iommu=none,usb=off,virtualization=off,compact-highmem=on,dump-guest-core=off,mem-merge=off,hmat=off";
/// CPU model and qualifiers, adopted from HiSH's launch line.
///
/// `max` under TCG is every feature TCG implements. HiSH asks for SVE off
/// explicitly (TCG's SVE is slow) and for the cheap IMPDEF pointer
/// authentication algorithm. The guest kernel builds neither pointer auth nor
/// SVE (`CONFIG_ARM64_PTR_AUTH` and `CONFIG_ARM64_SVE` are off in
/// `images/linux-6.12.60.config`) but does build LSE atomics
/// (`CONFIG_ARM64_USE_LSE_ATOMICS=y`, kept from the HiSH base), so it issues the
/// single-instruction atomic ops instead of the load/store-exclusive loops that
/// TCG has to emulate as a retry loop. What the guest can actually use is
/// decided by its kernel config, not by this model string.
pub const CPU_MODEL: &str = "max,pauth-impdef=on,sve=off,pmu=off";
/// CPU topology, adopted from HiSH: one socket, one thread per core.
pub const SMP_SOCKETS: u32 = 1;
pub const SMP_THREADS_PER_CORE: u32 = 1;
/// TCG translation-block cache size in MB, raised from 1024 to HiSH's 2048.
/// A larger cache keeps hot guest code (libc, busybox, language servers)
/// translated across loops instead of being re-translated on every eviction.
///
/// The cost is address space, not resident memory: this is one anonymous mmap
/// reservation. aarch64's `MAX_CODE_GEN_BUFFER_SIZE` is `(size_t)-1`, so the
/// value is not clamped, and split-wx is off in this build (it only defaults on
/// with `CONFIG_DEBUG_TCG`), so there is a single mapping and resident pages
/// only appear as translation blocks are actually written into it.
pub const TCG_TB_SIZE_MB: u32 = 2048;
/// RTC policy, adopted from HiSH: the guest clock is UTC and follows the host
/// clock instead of running free.
pub const RTC_OPTIONS: &str = "base=utc,clock=host";
/// `-overcommit cpu-pm=off`, adopted from HiSH. Under TCG this one is inert (it
/// only controls whether the host may enter CPU power states between guest
/// exits, which is a KVM concern); it is kept because it is part of the
/// reference launch line and costs nothing.
pub const OVERCOMMIT_OPTIONS: &str = "cpu-pm=off";
/// Serial console common to both boot modes.
pub const KERNEL_CONSOLE: &str = "console=ttyAMA0,115200";
/// Boot command line: NO initramfs, NO switch_root. The kernel mounts the guest
/// disk (/dev/vda) directly as root and runs the disk system's own
/// `/usr/lib/qemu-init/init` as PID 1, which mounts proc/sys/dev, mounts the
/// shared sandbox once (first mount of the tag, so it succeeds cleanly) and
/// starts the guest daemon. Within one launch there is exactly ONE QEMU
/// `main`. Guest relaunches after poweroff (restart hook, see 4.5.1 of the
/// design doc) DO re-run `main` on the same thread — acceptable because
/// relaunches are rare (user-initiated poweroff / crash) and the engine leaks
/// only its globals per main, which bounds the practical relaunch count.
///
/// The two parameters after `init=` are the only ones from HiSH's kernel
/// command line that this kernel actually acts on:
///   - `mitigations=off`: CONFIG_CPU_MITIGATIONS=y, so the parameter is live.
///     It is the arm64 switch that drops the mitigation sequence (and the extra
///     barriers it places on every kernel entry and exit).
///   - `TERM=xterm`: the value PID 1 and the console shell see on a serial
///     console that has no terminal to ask.
///
/// HiSH's `kpti=off` is deliberately NOT carried over. This kernel is built
/// without CONFIG_UNMAP_KERNEL_AT_EL0 (the arm64_virt base leaves it off), so
/// the `kpti` early parameter is never registered and the token would only add
/// an "unknown kernel command line parameter" line at boot.
///
/// The rest of HiSH's line was dropped as inert against this config:
///   - `transparent_hugepage=`, `nohz=on`, `nohz_full=`: this kernel turns THP
///     on unconditionally (CONFIG_TRANSPARENT_HUGEPAGE_ALWAYS) and is already
///     tickless (CONFIG_NO_HZ_FULL, which per its own Kconfig behaves as
///     tickless-idle unless a `nohz_full=` CPU list is passed), so those
///     parameters only restate the build defaults. `skew_tick=1` is in the same
///     position: it needs a nohz_full CPU list to have a subject.
///   - `cpuidle.off=1` / `idle=halt`: CONFIG_CPU_IDLE is built, but under TCG a
///     guest WFI already halts the vCPU, so a deeper idle state has nothing to
///     save.
///   - `rng_core.default_quality=`: the hwrng core already defaults to the
///     maximum quality of 1024. CPU random needs no parameter either:
///     CONFIG_ARCH_RANDOM was removed upstream and the kernel uses RNDR whenever
///     the CPU advertises FEAT_RNG, which `-cpu max` does.
///   - debug/hardening switches (`slub_debug`, `page_poison`, `nmi_watchdog`,
///     `init_on_free`): all default off in this config, so passing them would
///     only add guest work.
///
/// Deliberately omitted: `init_on_alloc=1` from HiSH's line. This kernel
/// defaults it off (`CONFIG_INIT_ON_ALLOC_DEFAULT_ON` is not set), so copying
/// the parameter would turn allocation zeroing ON and slow the guest down.
pub const GUEST_CMDLINE: &str = "console=ttyAMA0,115200 root=/dev/vda rw init=/usr/lib/qemu-init/init mitigations=off TERM=xterm";

/// Static writable sandbox virtio-fs tag: host `files/` -> guest same path.
pub const MOUNT_TAG_SANDBOX: &str = "sandbox";
/// Static writable customer-data virtio-fs tag: the user's chosen data root ->
/// guest same path. Present only when that directory sits outside the sandbox,
/// where the sandbox share cannot reach it.
pub const MOUNT_TAG_DATA: &str = "customer_data";
/// Virtio-fs backend socket names under the port dir.
pub const FS_SOCKET_SANDBOX: &str = "fs_sandbox.sock";
/// Backend socket for the customer-data share (see [`MOUNT_TAG_DATA`]).
pub const FS_SOCKET_DATA: &str = "fs_data.sock";
/// Prefix for virtio-fs backend sockets of dynamically mounted work dirs.
pub const FS_WORK_PREFIX: &str = "fs_work";
/// QMP control socket for runtime device_add of work-directory mounts.
pub const QMP_SOCKET: &str = "qmp.sock";
/// Number of pre-created pcie-root-ports for runtime hotplug of work-directory
/// virtio-fs devices (each exposes one slot).
pub const WORKDIR_MOUNT_SLOTS: usize = 8;

/// Disk cache and AIO policy, adopted from HiSH.
///   - `cache=writeback` and `aio=threads` are already QEMU's defaults for this
///     drive; they are stated explicitly to match the reference launch line.
///   - `discard=unmap` lets a guest TRIM punch holes in the qcow2 instead of
///     letting the image grow without bound. It costs nothing in practice here
///     because the guest only issues discards if its root filesystem is mounted
///     with `discard` or something runs `fstrim`, and neither happens.
pub const DISK_DRIVE_OPTIONS: &str = "cache=writeback,aio=threads,discard=unmap";
/// Dedicated IO thread for the guest disk, adopted from HiSH. Block I/O then
/// completes on its own host thread instead of on the vCPU thread that
/// submitted it, so a vCPU waiting on the disk no longer holds up block I/O for
/// the other vCPUs. Under TCG this is available because `virtio-blk-pci`
/// registers the ioeventfd transport by default (`VIRTIO_PCI_FLAG_USE_IOEVENTFD`
/// is a transport property, not a KVM-only one).
pub const DISK_IOTHREAD_ID: &str = "iothread0";

/// Entropy source handed to the guest, as a `-object` (host side) and a
/// `-device` (guest side) pair: an rng-random backend reading the host's
/// `/dev/urandom`, surfaced as a virtio-rng PCI device.
///
/// Why the guest needs one: its CRNG has to be seeded before anything that
/// needs unpredictable bytes can proceed, and the first such thing is a TLS
/// handshake (git over https, npm, ssh). With no entropy device the kernel can
/// only credit timer jitter and timing noise, which is a slow and weak source.
/// The virtio-rng driver feeds the hwrng core directly, and the hwrng core
/// defaults to a quality of 1024 (i.e. maximum, see `default_quality` in
/// drivers/char/hw_random/core.c), so the CRNG is seeded instead of groping.
/// The guest kernel side is `CONFIG_HW_RANDOM` + `CONFIG_HW_RANDOM_VIRTIO`.
///
/// The `-non-transitional` name is used so the guest only ever negotiates the
/// modern (virtio 1.0) interface; no legacy window is built for it.
pub const RNG_BACKEND_OPTION: &str = "rng-random,id=rng0,filename=/dev/urandom";
/// Guest-side counterpart of [`RNG_BACKEND_OPTION`].
pub const RNG_DEVICE_OPTION: &str = "virtio-rng-pci-non-transitional,rng=rng0";

/// Guest-internal SSH ports that the guest daemon listens on.
pub const GUEST_COMMAND_PORT: u16 = 4022;
pub const GUEST_MANAGEMENT_PORT: u16 = 4023;
/// Host-side loopback ports for the static slirp hostfwd rules that reach the
/// guest daemon (must match `cmd_client::endpoint::GUEST_*_PORT`).
pub const HOST_COMMAND_PORT: u16 = 4122;
pub const HOST_MANAGEMENT_PORT: u16 = 4123;

/// Filesystem layout handed to the guest.
#[derive(Clone)]
pub struct QemuPaths {
    pub kernel: PathBuf,
    /// Unused by the boot path: there is no initramfs (see GUEST_CMDLINE), the
    /// guest disk is the root filesystem. Kept because the runtime provisioning
    /// still passes the path through.
    pub initrd: PathBuf,
    /// Directory holding the QMP socket and the virtio-fs backend sockets.
    pub port_dir: PathBuf,
    /// Host directory shared to the guest at the same path (tag "sandbox").
    pub sandbox_mount: PathBuf,
    /// Host customer-data root shared to the guest at the same path (tag
    /// "customer_data"), when the user chose a data root outside
    /// `sandbox_mount`. `None` when it already lives inside the sandbox and
    /// needs no second share.
    pub data_mount: Option<PathBuf>,
    /// Host qcow2 disk image attached as the guest disk (`/dev/vda`).
    pub disk_path: PathBuf,
}

/// Runtime resource allocation for this guest.
#[derive(Clone, Copy)]
pub struct QemuConfig {
    pub cores: u32,
    pub mem_gb: u32,
    pub disk_gb: u32,
}

/// Mount registry handed out by [`start_with_restart`]. Held in a mutex rather
/// than a `OnceLock` so a registry left behind by a dead engine (crash-loop
/// give-up, QEMU disabled in settings, provisioning failure) is detected on
/// the next start and replaced by a fresh boot instead of being handed out
/// forever while nothing is actually running.
static REGISTRY: Mutex<Option<Arc<MountRegistry>>> = Mutex::new(None);

/// Starts the QEMU guest: virtio-fs static backend first, then the engine on
/// its own thread. Returns the mount registry used to add dynamic
/// work-directory mounts. Idempotent while the engine lives: subsequent calls
/// return the registry from the already-running guest. If a previous engine
/// exited (crash-loop give-up, QEMU disabled, provisioning failure), the
/// stale registry is discarded and a fresh guest is booted instead.
///
/// cfg-gated to OHOS: `MountRegistry` only exists there (it drives the
/// in-process virtiofsd backends, which are OHOS-only).
#[cfg(target_env = "ohos")]
pub fn start(paths: QemuPaths, cfg: QemuConfig) -> Option<Arc<MountRegistry>> {
    start_with_restart(paths, cfg, Box::new(|| None))
}

/// Like [`start`], but the engine thread invokes `restart` after the machine
/// exits (guest poweroff or crash): returning a new argv sequence boots again
/// on the same thread, so a guest relaunch needs no extra supervisor thread.
#[cfg(target_env = "ohos")]
pub fn start_with_restart(
    paths: QemuPaths,
    cfg: QemuConfig,
    restart: RestartHook,
) -> Option<Arc<MountRegistry>> {
    // Serialize starts under the registry lock: a concurrent caller either
    // gets the winner's registry below or boots fresh when the engine is dead.
    let mut registry_guard = REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(registry) = registry_guard.as_ref() {
        if engine::is_running() {
            return Some(registry.clone());
        }
        // The engine thread is gone (crash-loop give-up, QEMU disabled in
        // settings, relaunch provisioning failure). The old registry describes
        // mounts of a guest that no longer exists: drop it and boot fresh.
        log::warn!(
            "qemu_manager::start: engine not running; discarding stale registry and booting a fresh guest"
        );
        *registry_guard = None;
    }
    // No host-RAM boot gate: the guest RAM backend is a lazily-allocated memfd
    // (`size=` with no `prealloc`), so booting does not need the guest's full
    // memory size free up front. Trusting the guest kernel to grow into RAM as
    // it runs is preferable to refusing to boot whenever unrelated host
    // processes have consumed most of the device memory.
    // Start the sandbox virtio-fs backend before QEMU so its listening socket
    // exists when the vhost-user-fs-pci chardev connects.
    virtiofs::start(&paths);

    // Single-stage boot: the guest disk (/dev/vda) is the root filesystem and
    // its own `/usr/lib/qemu-init/init` runs as PID 1, all inside one engine
    // `main`. The engine library is single-instance and keeps its globals
    // across mains, so relaunches (restart hook) trade a small per-main leak
    // for a supervisor-free design (see the design doc 4.5.1); within one
    // launch there is still exactly one main.
    let Some(argv) = build_argv(&paths, cfg) else {
        log::error!("qemu_manager::start: failed to build QEMU argv; not booting guest");
        return None;
    };
    if !engine::spawn_with_restart(vec![argv], restart) {
        log::error!("qemu_manager::start: failed to spawn qemu-machine thread");
        return None;
    }
    let registry = Arc::new(MountRegistry::new(paths.port_dir));
    // The registry lock is still held here, so no concurrent start can have
    // replaced the (None) entry in between: the engine we just spawned is the
    // one this registry describes.
    *registry_guard = Some(registry.clone());
    Some(registry)
}

/// Re-arms the sandbox virtio-fs backend and rebuilds the machine argv for a
/// relaunch on the still-running engine thread. Also resets the mount
/// registry's state: the relaunching guest boots with no workdir mounts, so
/// dedup entries and burned slots from the previous guest instance must not
/// survive it (callers then re-mount through the same registry cleanly).
#[cfg(target_env = "ohos")]
pub fn prepare_reboot(paths: &QemuPaths, cfg: QemuConfig) -> Option<Vec<CString>> {
    virtiofs::start(paths);
    // Called on the engine thread during a relaunch; the lock is only held
    // briefly by start/ mount calls, so this never deadlocks.
    if let Some(registry) = REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner()).as_ref() {
        registry.reset();
    }
    build_argv(paths, cfg)
}


/// Builds the single QEMU argv for one launch. `-append` is a single C-string
/// that embeds spaces, so each QEMU option pair is one vector element. There is
/// no initramfs: the guest disk is the root filesystem and its own
/// `/usr/lib/qemu-init/init` is PID 1 (see GUEST_CMDLINE).
/// Returns `None` when a host path cannot be represented as a C string
/// (interior NUL — never expected for sandbox paths, but not worth panicking
/// over on the boot path).
pub fn build_argv(paths: &QemuPaths, cfg: QemuConfig) -> Option<Vec<CString>> {
    let kernel = path_cstring(&paths.kernel, "kernel")?;
    let port_dir = paths.port_dir.to_string_lossy().into_owned();
    let cmdline = CString::new(GUEST_CMDLINE).expect("static cmdline");
    let mem = format!("{}G", cfg.mem_gb);
    let cores = cfg.cores;

    let mut argv = vec![
        CString::new("qemu-system-aarch64").expect("static"),
        CString::new("-nodefaults").expect("static"),
        CString::new("-no-user-config").expect("static"),
        // Exit instead of rebooting when the guest requests a reset. Without it
        // a guest-side reboot (kernel panic/reboot) makes QEMU re-init TCG on
        // every reset in-process, growing memory until the host OOMs.
        CString::new("-no-reboot").expect("static"),
        CString::new("-M").expect("static"),
        CString::new(format!("{MACHINE_TYPE},{MACHINE_OPTIONS}")).expect("format"),
        CString::new("-cpu").expect("static"),
        CString::new(CPU_MODEL).expect("static"),
        CString::new("-smp").expect("static"),
        CString::new(format!(
            "cpus={cores},sockets={SMP_SOCKETS},cores={cores},threads={SMP_THREADS_PER_CORE}"
        ))
        .expect("format"),
        CString::new("-accel").expect("static"),
        CString::new(format!("tcg,thread=multi,tb-size={TCG_TB_SIZE_MB}")).expect("format"),
        CString::new("-rtc").expect("static"),
        CString::new(RTC_OPTIONS).expect("static"),
        CString::new("-overcommit").expect("static"),
        CString::new(OVERCOMMIT_OPTIONS).expect("static"),
        CString::new("-object").expect("static"),
        CString::new(format!("memory-backend-memfd,id=mem,size={mem}")).expect("format"),
        CString::new("-m").expect("static"),
        CString::new(mem).expect("format"),
        CString::new("-kernel").expect("static"),
        kernel,
        // No initramfs: the disk system is booted directly as the root
        // filesystem (/dev/vda) with its own init as PID 1 (GUEST_CMDLINE).
        CString::new("-append").expect("static"),
        cmdline,
    ];
    argv.extend_from_slice(&[
        CString::new("-display").expect("static"),
        CString::new("none").expect("static"),
        CString::new("-monitor").expect("static"),
        CString::new("none").expect("static"),
        // QMP control channel: runtime device_add for work-dir mounts.
        CString::new("-qmp").expect("static"),
        cstring_checked(
            format!("unix:{port_dir}/{QMP_SOCKET},server=on,wait=off"),
            "qmp socket path",
        )?,
        CString::new("-serial").expect("static"),
        // Debug builds forward the guest serial console to QEMU stdout (into
        // hilog); release builds use "null" so the guest never touches the
        // process stdin/stdout.
        #[cfg(feature = "qemu_debug_assertions")]
        CString::new("stdio").expect("static"),
        #[cfg(not(feature = "qemu_debug_assertions"))]
        CString::new("null").expect("static"),
        // Writable sandbox mount over virtio-fs (host files/ -> guest same path).
        CString::new("-chardev").expect("static"),
        cstring_checked(
            format!("socket,path={port_dir}/{FS_SOCKET_SANDBOX},id=fs_sandbox"),
            "sandbox chardev path",
        )?,
        CString::new("-device").expect("static"),
        CString::new(format!(
            "vhost-user-fs-pci,id=fs_sandbox,chardev=fs_sandbox,tag={MOUNT_TAG_SANDBOX},queue-size=1024"
        ))
        .expect("static format"),
        // Dedicated block-IO thread for the guest disk (see the DISK_IOTHREAD_*
        // constants above). `poll-max-ns` is deliberately NOT set, deviating
        // from HiSH's `poll-max-ns=2000000`: it makes the IO thread spin for up
        // to N ns after every I/O burst (it re-runs the poll handlers with a
        // zero timeout until N ns elapse — see `try_poll_mode` /
        // `run_poll_handlers` in util/aio-posix.c). That trades a host CPU for
        // I/O latency, and host CPU is the scarcest resource here: the guest is
        // TCG with no hardware virtualisation, so every cycle this thread spins
        // is a cycle the vCPU threads do not get. This disk only carries the
        // guest root filesystem; the tree the guest actually hammers lives on
        // the virtio-fs mounts, which never go through this IO thread.
        CString::new("-object").expect("static"),
        CString::new(format!("iothread,id={DISK_IOTHREAD_ID}")).expect("format"),
        // Guest disk: the disk image (guest system root) over virtio-blk. The
        // device stays virtio-blk and NOT HiSH's virtio-scsi + scsi-hd. The
        // kernel now carries CONFIG_SCSI and CONFIG_SCSI_VIRTIO (both come with
        // the arm64_virt base), so a scsi-hd would bind - but virtio-blk is the
        // shorter path (the SCSI transport adds a command-parsing layer this
        // device does not have), and keeping it also keeps the launch line and
        // the guest's device expectations bit-for-bit what already boots.
        CString::new("-drive").expect("static"),
        cstring_checked(
            format!(
                "file={},if=none,id=drive0,format=qcow2,{DISK_DRIVE_OPTIONS}",
                paths.disk_path.display()
            ),
            "disk image path",
        )?,
        CString::new("-device").expect("static"),
        CString::new(format!(
            "virtio-blk-pci,drive=drive0,id=virtblk0,iothread={DISK_IOTHREAD_ID}"
        ))
        .expect("static format"),
        // Guest user-mode network via slirp NAT (outbound downloads). The guest
        // The daemon (4022/4023) is reached through the static hostfwd rules.
        CString::new("-netdev").expect("static"),
        CString::new(format!(
            "user,id=net0,hostfwd=tcp:127.0.0.1:{HOST_COMMAND_PORT}-:{GUEST_COMMAND_PORT},hostfwd=tcp:127.0.0.1:{HOST_MANAGEMENT_PORT}-:{GUEST_MANAGEMENT_PORT}"
        ))
        .expect("format"),
        CString::new("-device").expect("static"),
        CString::new("virtio-net-pci,id=net0dev,netdev=net0,romfile=").expect("static"),
        // Host-backed entropy for the guest CRNG (see the RNG_* constants).
        CString::new("-object").expect("static"),
        CString::new(RNG_BACKEND_OPTION).expect("static"),
        CString::new("-device").expect("static"),
        CString::new(RNG_DEVICE_OPTION).expect("static"),
    ]);
    // Customer-data share, only when the data root was chosen outside the
    // sandbox. Its binaries (the managed Node runtime, the downloaded language
    // servers and debug adapters) are Linux builds the guest execs at their
    // real host paths, and the sandbox share does not cover that directory.
    if paths.data_mount.is_some() {
        argv.extend_from_slice(&[
            CString::new("-chardev").expect("static"),
            cstring_checked(
                format!("socket,path={port_dir}/{FS_SOCKET_DATA},id=fs_data"),
                "customer-data chardev path",
            )?,
            CString::new("-device").expect("static"),
            CString::new(format!(
                "vhost-user-fs-pci,id=fs_data,chardev=fs_data,tag={MOUNT_TAG_DATA},queue-size=1024"
            ))
            .expect("static format"),
        ]);
    }
    // PCIe root ports for runtime hotplug of work-directory virtio-fs devices.
    for rp_index in 0..WORKDIR_MOUNT_SLOTS {
        argv.push(CString::new("-device").expect("static"));
        argv.push(
            CString::new(format!(
                "pcie-root-port,id=rp{rp_index},chassis={},bus=pcie.0",
                rp_index + 1
            ))
            .expect("format"),
        );
    }
    Some(argv)
}

/// Converts a host path to a `CString`, mapping an interior-NUL failure (not
/// expected for sandbox paths) to `None` with an error log instead of a panic.
fn path_cstring(path: &std::path::Path, what: &str) -> Option<CString> {
    cstring_checked(path.to_string_lossy().into_owned(), what)
}

/// Converts a formatted option string to a `CString`, mapping an interior-NUL
/// failure to `None` with an error log instead of a panic.
fn cstring_checked(value: String, what: &str) -> Option<CString> {
    match CString::new(value) {
        Ok(cstring) => Some(cstring),
        Err(err) => {
            log::error!("qemu_manager: build_argv: {what} contains interior NUL: {err}");
            None
        }
    }
}
