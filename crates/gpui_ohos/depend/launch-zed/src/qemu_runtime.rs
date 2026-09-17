//! QEMU guest backend: engine boot and dynamic work-directory mounts.
//!
//! Compiled only under the `qemu-agent` feature. Called once at launch, before
//! Zed starts: provisions the guest files under the app sandbox `files/qemu/`,
//! boots the guest and registers the guest command executor, falling back to
//! the on-device command service on any failure.
//!
//! The QEMU settings are read here straight from the user settings file because
//! the SettingsStore is not initialized yet at launch time. The settings are
//! also authoritative through the UI; both write the same keys.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cmd_client::{
    CommandEndpoint, ExecSpec, RemoteChild, RemoteCommandExecutor, Signal,
    SshCommandExecutor,
};
use openharmony_ability::OpenHarmonyApp;
use qemu_manager::{GuestShell, MountRegistry, QemuConfig, QemuPaths};

use crate::launch_app::{
    boot_trace, read_management_keys, register_executor, register_ohos_backend, resource_dir,
};

/// Resfile subdir holding the guest daemon management keys.
const GUEST_KEY_SUBDIR: &str = "hicodeerd-mgmt-guest";
/// Resfile subdir carrying the guest daemon binary staged at startup.
const RES_QEMU_SUBDIR: &str = "qemu-guest";
/// Guest daemon management server-half files.
const MGMT_HOST_KEY_FILE: &str = "mgmt_host_key";
const MGMT_AUTHORIZED_KEYS_FILE: &str = "authorized_keys";
/// The daemon binary file staged under `files/qemu/bin`.
const GUEST_BIN_FILE: &str = "hicodeerd";
/// Dir under the sandbox `files/` root where guest assets live.
const QEMU_DIR: &str = "qemu";
/// File under `files/qemu/guest-conf` carrying the customer-data root absolute
/// path. The guest boot scripts read it to mount that directory (tag
/// "customer_data") at the same path; the guest cannot learn it any other way,
/// since the sandbox mount does not cover a data root chosen outside `files/`.
const CUSTOMER_DATA_PATH_FILE: &str = "customer_data_path";
/// Resfile name of the provisioned guest-disk golden image (qcow2 with the
/// full system layer already unpacked). The runtime never modifies it: it is
/// copied to `files/qemu/disk.qcow2` as the working disk, and a corrupt or
/// missing working disk is restored by copying the golden again.
const GOLDEN_DISK_FILE: &str = "golden.qcow2";
/// Working-disk file name inside `files/qemu/`.
const DISK_FILE: &str = "disk.qcow2";
/// File under `base_path` recording the user-chosen home directory. Written by
/// both the ArkTS setup page and `start_zed_main`; it is the only place the
/// launch-time reader can learn where the data root (and thus the settings
/// file) actually lives, because `paths::config_dir` is not initialized yet.
const HOME_DIRECTORY_RECORD_FILE: &str = "custom_data_dir";
/// Subdirectory of the home directory holding the user settings file.
const HOME_CONFIG_SUBDIR: &str = "config";
/// User settings file name.
const SETTINGS_FILE_NAME: &str = "settings.json";

/// Parsed QEMU launch settings (defaults when the file cannot be read).
struct LaunchQemuSettings {
    enabled: bool,
    cores: u32,
    mem_gb: u32,
    disk_gb: u32,
}

impl Default for LaunchQemuSettings {
    fn default() -> Self {
        // QEMU is OFF unless the user settings file turns it on: the guest is
        // still under development, so the on-device backend is the default and
        // the guest is opt-in via `qemu_enabled`.
        //
        // When it is turned on, one vCPU / 4 GB / 128 GB is the default: the
        // guest is pure TCG emulation and the bulk of what it runs (git,
        // language servers, shells) is serial, so extra vCPUs add
        // synchronisation cost instead of parallelism, and the smaller RAM
        // leaves more for the host.
        Self {
            enabled: false,
            cores: 1,
            mem_gb: 4,
            disk_gb: 128,
        }
    }
}

/// Starts the command backend and registers it as the process-wide executor.
/// `app.base_path()` is the el2 sandbox `files/` directory.
pub fn start_command_backend(app: &OpenHarmonyApp) {
    boot_trace("start_command_backend entered");
    let base_path = match app.base_path() {
        Some(base) if !base.is_empty() => PathBuf::from(base),
        _ => {
            boot_trace("start_command_backend: no base_path; OHOS fallback");
            log::error!("qemu_runtime: no base_path; falling back to OHOS backend");
            register_ohos_backend(app);
            return;
        }
    };
    boot_trace(&format!("start_command_backend: base_path={}", base_path.display()));
    let settings = read_launch_qemu_settings(&base_path);
    boot_trace(&format!(
        "start_command_backend: settings enabled={} cores={} mem={}G disk={}G",
        settings.enabled, settings.cores, settings.mem_gb, settings.disk_gb
    ));

    if settings.enabled {
        match provision_guest_files(&base_path, &settings) {
            Some((paths, cfg, host_pub, client_key)) => {
                boot_trace("provision_guest_files OK; starting qemu");
                // Directories the guest already sees through a static share.
                // Nothing under them may go through the lazy mount path: that
                // path burns one of the few hotplug slots per directory.
                let mut covered_roots = vec![paths.sandbox_mount.clone()];
                covered_roots.extend(paths.data_mount.clone());
                let restart_hook = make_restart_hook(base_path.clone());
                let registry = match qemu_manager::start_with_restart(paths, cfg, restart_hook) {
                    Some(registry) => {
                        boot_trace("qemu_manager::start_with_restart OK");
                        registry
                    }
                    None => {
                        boot_trace("qemu_manager::start_with_restart FAILED; OHOS fallback");
                        log::warn!("qemu_runtime: guest failed to start; falling back to OHOS");
                        register_ohos_backend(app);
                        return;
                    }
                };
                let inner = match SshCommandExecutor::new(
                    CommandEndpoint::qemu_guest(),
                    client_key,
                    host_pub,
                ) {
                    Ok(executor) => Arc::new(executor),
                    Err(err) => {
                        boot_trace(&format!(
                            "SshCommandExecutor::new FAILED: {err}; OHOS fallback"
                        ));
                        log::error!(
                            "qemu_runtime: create guest SshCommandExecutor: {err}; falling back to OHOS"
                        );
                        register_ohos_backend(app);
                        return;
                    }
                };
                let executor: Arc<dyn RemoteCommandExecutor> =
                    Arc::new(WorkdirAwareExecutor::new(inner.clone(), registry, covered_roots));
                register_executor(executor);
                // Best-effort guest clock sync once the guest daemon is up.
                start_time_sync(inner);
                boot_trace("QEMU guest backend ACTIVE");
                log::info!("qemu_runtime: QEMU guest backend active");
            }
            None => {
                boot_trace("provision_guest_files returned None; OHOS fallback");
                log::warn!("qemu_runtime: guest failed to start; falling back to OHOS");
                register_ohos_backend(app);
            }
        }
    } else {
        boot_trace("settings.enabled=false; OHOS fallback");
        register_ohos_backend(app);
    }
}

/// Provision guest assets under `files/qemu/` (staged binaries/keys, disk
/// inspect-and-heal) and return everything needed to boot. Idempotent and
/// cheap on re-runs: staged copies are skipped when up to date. Does not
/// start the engine — the caller decides when and how to boot.
fn provision_guest_files(
    base_path: &Path,
    settings: &LaunchQemuSettings,
) -> Option<(QemuPaths, QemuConfig, String, String)> {
    let Some(resource_dir) = resource_dir() else {
        boot_trace("provision: resource_dir() None");
        return None;
    };
    let qemu_dir = base_path.join(QEMU_DIR);
    let bin_dir = qemu_dir.join("bin");
    let conf_dir = qemu_dir.join("guest-conf");
    let ports_dir = qemu_dir.join("ports");
    for dir in [&qemu_dir, &bin_dir, &conf_dir, &ports_dir] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            boot_trace(&format!("provision: create_dir_all {} failed: {err}", dir.display()));
            log::error!("qemu_runtime: create {}: {err}", dir.display());
            return None;
        }
    }
    boot_trace("provision: dirs ready; staging binaries/keys");

    // Stage the guest daemon (resfile -> sandbox so the guest can exec it).
    // Always overwrite: the bundle rebuilds the guest daemon, and a size-based
    // skip would keep a stale binary here whenever a rebuild happens to produce
    // the same file size (the stale bin is what the guest actually execs).
    let staged_bin = bin_dir.join(GUEST_BIN_FILE);
    if let Err(err) = std::fs::copy(
        &resource_dir.join(RES_QEMU_SUBDIR).join(GUEST_BIN_FILE),
        &staged_bin,
    ) {
        log::error!("qemu_runtime: stage guest hicodeerd: {err}");
        return None;
    }
    log::info!("qemu_runtime: staged guest hicodeerd at {}", staged_bin.display());
    // Stage the guest management server-half keys (the client half is read from
    // the resfile below). These must ALWAYS overwrite guest-conf: the bundle
    // regenerates the guest keys on every build, and the ed25519 files are
    // fixed-size, so a size-based skip would leave stale server keys in
    // guest-conf while cmd-client reads the fresh client half from resfile ->
    // SSH "Unknown server key" on the management port.
    let res_guest_keys = resource_dir.join(GUEST_KEY_SUBDIR);
    for (name, mode) in [
        (MGMT_HOST_KEY_FILE, 0o600),
        (MGMT_AUTHORIZED_KEYS_FILE, 0o600),
    ] {
        let dst = conf_dir.join(name);
        if let Err(err) = std::fs::copy(&res_guest_keys.join(name), &dst) {
            log::error!("qemu_runtime: stage guest key {name}: {err}");
            return None;
        }
        let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode));
    }
    // Working disk = a copy of the golden image. The golden (qcow2 with the
    // full system layer already unpacked) is shipped in the resfile and never
    // modified at runtime; the guest boots it directly as root=/dev/vda with
    // no initramfs and no first-boot unpack. A structurally healthy working
    // disk is kept (it persists); a missing or corrupt one is restored by
    // copying the golden again - a cheap, disk-corruption-safe recovery.
    let golden = resource_dir.join(RES_QEMU_SUBDIR).join(GOLDEN_DISK_FILE);
    if !golden.exists() {
        boot_trace(&format!("provision: golden missing at {}", golden.display()));
        log::error!(
            "qemu_runtime: golden guest disk missing at {}",
            golden.display()
        );
        return None;
    }
    let disk = qemu_dir.join(DISK_FILE);
    let virtual_size = golden_virtual_size(&golden).unwrap_or_else(|| {
        // A golden that cannot be read as qcow2 is a build problem: refuse to
        // boot rather than ship a bricked guest. Fall back to a generous
        // default only to keep the inspect call below well-defined.
        u64::from(settings.disk_gb) * 1024 * 1024 * 1024
    });
    let healthy = match inspect_qcow2(&disk, virtual_size) {
        Qcow2State::Valid => {
            boot_trace("provision: working disk Valid (kept)");
            true
        }
        Qcow2State::SizeMismatch { actual } => {
            boot_trace("provision: working disk SizeMismatch; will restore from golden");
            log::warn!(
                "qemu_runtime: working disk holds {}B but golden is {}B; restoring",
                actual,
                virtual_size
            );
            false
        }
        Qcow2State::Missing => {
            boot_trace("provision: working disk Missing; copying golden");
            false
        }
        Qcow2State::Corrupt(reason) => {
            boot_trace(&format!("provision: working disk Corrupt ({reason}); restoring"));
            log::warn!(
                "qemu_runtime: working disk unusable ({reason}); restoring from golden"
            );
            false
        }
    };
    if !healthy {
        // Copy the golden (overwriting any stale/corrupt working disk) as the
        // recovery path. The golden is never modified; the guest writes only to
        // this working copy. A stale/corrupt disk must be cleared first, but a
        // Missing disk (fresh provision) has nothing to remove - NotFound here
        // is the normal first-boot state, not an error, so it must not abort
        // provisioning or the guest can never be booted after the sandbox is
        // wiped.
        if let Err(err) = std::fs::remove_file(&disk) {
            if err.kind() != std::io::ErrorKind::NotFound {
                boot_trace(&format!("provision: remove stale working disk failed: {err}"));
                log::error!("qemu_runtime: remove stale working disk: {err}");
                return None;
            }
        }
        if let Err(err) = std::fs::copy(&golden, &disk) {
            boot_trace(&format!("provision: copy golden -> disk failed: {err}"));
            log::error!("qemu_runtime: copy golden guest disk: {err}");
            return None;
        }
        boot_trace("provision: restored working disk from golden");
        log::info!(
            "qemu_runtime: restored working disk from golden at {}",
            disk.display()
        );
    }

    // The user-chosen data root holds the managed Node runtime, the downloaded
    // language servers and the debug adapters - all Linux binaries the guest
    // execs at their real host paths. When it sits outside the sandbox the
    // static sandbox share does not reach it, so it gets a share of its own and
    // the guest learns the path from guest-conf.
    let data_mount = match read_data_root(base_path).as_ref() {
        Some(root) if !root.starts_with(base_path) => Some(root.clone()),
        _ => None,
    };
    let root_record = conf_dir.join(CUSTOMER_DATA_PATH_FILE);
    match data_mount.as_ref() {
        Some(root) => {
            if let Err(err) = std::fs::write(&root_record, format!("{}\n", root.display())) {
                boot_trace(&format!("provision: write data root record failed: {err}"));
                log::error!("qemu_runtime: write {}: {err}", root_record.display());
                return None;
            }
            log::info!("qemu_runtime: data root shared at {}", root.display());
        }
        None => {
            // A record left over from an earlier launch would send the guest
            // looking for a directory this launch does not share.
            if let Err(err) = std::fs::remove_file(&root_record) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("qemu_runtime: clear {}: {err}", root_record.display());
                }
            }
        }
    }

    let paths = QemuPaths {
        kernel: resource_dir.join("Image"),
        initrd: resource_dir.join("rootfs.cpio.zst"),
        port_dir: ports_dir,
        sandbox_mount: base_path.to_owned(),
        data_mount,
        disk_path: disk,
    };
    let cfg = QemuConfig {
        cores: settings.cores,
        mem_gb: settings.mem_gb,
        disk_gb: settings.disk_gb,
    };
    let (host_pub, client_key) = match read_management_keys(&res_guest_keys) {
        Some(pair) => pair,
        None => {
            boot_trace("provision: no guest management keys");
            log::error!("qemu_runtime: no guest management keys");
            return None;
        }
    };
    boot_trace("provision: OK (paths + keys ready)");
    Some((paths, cfg, host_pub, client_key))
}

/// Best-effort one-shot guest clock sync retried until the guest daemon is
/// reachable (the pool allocate fails fast while it is down).
fn start_time_sync(executor: Arc<SshCommandExecutor>) {
    std::thread::Builder::new()
        .name("qemu-time-sync".to_string())
        .spawn(move || {
            const MAX_ATTEMPTS: usize = 60;
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
            for _ in 0..MAX_ATTEMPTS {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Point the guest at the device's local timezone before syncing
                // the clock: the golden ships tzdata but no /etc/localtime, so
                // `date` would print UTC (8h behind the device's CST). The
                // symlink lands on the writable working disk, so it survives
                // relaunches and is re-applied on every boot anyway.
                let cmd = format!(
                    "ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime; date -s @{secs}"
                );
                match executor.run_shell(&cmd) {
                    Ok(()) => {
                        log::info!("qemu_runtime: guest clock synced to {secs}");
                        return;
                    }
                    Err(err) => {
                        log::debug!("qemu_runtime: guest not ready for time sync yet: {err}");
                    }
                }
                std::thread::sleep(RETRY_DELAY);
            }
            log::warn!("qemu_runtime: gave up on guest clock sync");
        })
        .ok();
}

/// Build the engine-thread restart hook: after the guest exits (poweroff or
/// crash) it re-provisions, rebuilds the argv and hands it back to the engine
/// thread, which boots again on the same thread — event-driven, so no
/// supervisor thread and no polling anywhere. Three exits within 60s of the
/// previous boot are treated as a crash loop and end the relaunch attempts.
fn make_restart_hook(base_path: PathBuf) -> qemu_manager::RestartHook {
    const BACKOFF: std::time::Duration = std::time::Duration::from_secs(3);
    const FAST_EXIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    const MAX_FAST_EXITS: u32 = 3;
    let mut last_boot = std::time::Instant::now();
    let mut fast_exits: u32 = 0;
    Box::new(move || {
        // Called on the engine thread right after QEMU main returned.
        std::thread::sleep(BACKOFF);
        if last_boot.elapsed() < FAST_EXIT_WINDOW {
            fast_exits += 1;
        } else {
            fast_exits = 0;
        }
        if fast_exits >= MAX_FAST_EXITS {
            log::error!(
                "qemu_runtime: guest exited {}x within {:?}; giving up relaunch",
                fast_exits,
                FAST_EXIT_WINDOW
            );
            return None;
        }
        let settings = read_launch_qemu_settings(&base_path);
        if !settings.enabled {
            log::info!("qemu_runtime: guest exited; QEMU disabled in settings, not relaunching");
            return None;
        }
        let Some((paths, cfg, _, _)) = provision_guest_files(&base_path, &settings) else {
            log::error!("qemu_runtime: relaunch provisioning failed; not relaunching");
            return None;
        };
        match qemu_manager::prepare_reboot(&paths, cfg) {
            Some(argv) => {
                log::info!("qemu_runtime: relaunching guest after exit");
                last_boot = std::time::Instant::now();
                Some(vec![argv])
            }
            None => {
                log::error!("qemu_runtime: prepare_reboot failed; not relaunching");
                None
            }
        }
    })
}

/// Executor wrapper used in QEMU mode: lazily mounts the command's work
/// directory into the guest before delegating to the guest daemon executor.
struct WorkdirAwareExecutor {
    inner: Arc<SshCommandExecutor>,
    mount: Arc<MountRegistry>,
    /// Host roots already visible to the guest through a static share. Anything
    /// under one of them needs no extra mount; a data root outside the sandbox
    /// appears here as a second root.
    covered_roots: Vec<PathBuf>,
}

impl WorkdirAwareExecutor {
    fn new(
        inner: Arc<SshCommandExecutor>,
        mount: Arc<MountRegistry>,
        covered_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            inner,
            mount,
            covered_roots,
        }
    }

    fn ensure_mounted(&self, cwd: &str) {
        if cwd.is_empty() {
            return;
        }
        let cwd_path = PathBuf::from(cwd);
        if self.covered_roots.iter().any(|root| cwd_path.starts_with(root)) {
            // Already visible to the guest through a static share.
            return;
        }
        let Some(root) = find_repo_root(&cwd_path, &self.covered_roots) else {
            return;
        };
        if self.mount.is_mounted(&root) {
            return;
        }
        let guest_shell = GuestShellAdapter {
            inner: self.inner.clone(),
        };
        if let Err(err) = self.mount.mount_workdir(root.clone(), &guest_shell) {
            // best-effort: the command still runs; its guest-side file access
            // will fail visibly if the directory truly is not visible.
            log::warn!(
                "qemu_runtime: lazy mount {} failed: {err}",
                root.display()
            );
        }
    }
}

impl RemoteCommandExecutor for WorkdirAwareExecutor {
    fn spawn(&self, spec: ExecSpec) -> std::io::Result<RemoteChild> {
        if let Some(cwd) = spec.cwd_path.as_deref() {
            self.ensure_mounted(cwd);
        }
        self.inner.spawn(spec)
    }

    fn signal(&self, session_id: u64, signal: Signal) -> std::io::Result<()> {
        self.inner.signal(session_id, signal)
    }

    fn try_exit(&self, session_id: u64) -> Option<Option<i32>> {
        self.inner.try_exit(session_id)
    }

    fn wait_exit_async(&self, session_id: u64) -> cmd_client::types::ExitFuture<'_> {
        self.inner.wait_exit_async(session_id)
    }

    fn open_shell_pty<'a>(
        &self,
        cols: u32,
        rows: u32,
        cwd: Option<&'a str>,
    ) -> cmd_client::types::ShellPtyFuture<'a> {
        // The shell's directory must be guest-visible too: mount it before the
        // backend opens the pty, otherwise `cd` in the shell command fails.
        if let Some(cwd) = cwd {
            self.ensure_mounted(cwd);
        }
        self.inner.open_shell_pty(cols, rows, cwd)
    }
}

/// Bridges cmd-client's `run_shell` into the `GuestShell` contract used by the
/// mount registry.
struct GuestShellAdapter {
    inner: Arc<SshCommandExecutor>,
}

impl GuestShell for GuestShellAdapter {
    fn run(&self, command: &str) -> std::io::Result<()> {
        self.inner.run_shell(command)
    }
}

/// Walks up from `path` to the nearest ancestor containing a `.git` entry.
/// Returns `None` when `path` lies under one of the `covered` roots (already
/// guest-visible).
fn find_repo_root(path: &Path, covered: &[PathBuf]) -> Option<PathBuf> {
    let mut current = path.to_owned();
    loop {
        if covered.iter().any(|root| current.starts_with(root)) {
            return None;
        }
        if current.join(".git").exists() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_owned(),
            None => return Some(path.to_owned()),
        }
    }
}

/// Structural health of the guest disk file.
enum Qcow2State {
    Valid,
    Missing,
    SizeMismatch { actual: u64 },
    Corrupt(String),
}

/// Checks the qcow2 header and the refcount covering the header cluster. The
/// refcount check is what catches the corruption the old shipped templates had
/// (4-byte entries written where qcow2 v2 uses 2-byte ones), which left the
/// header cluster marked free and made QEMU allocate an L2 table over it.
fn inspect_qcow2(path: &Path, expected_size: u64) -> Qcow2State {
    use std::io::{Read, Seek, SeekFrom};

    let file_len = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Qcow2State::Missing;
        }
        Err(err) => return Qcow2State::Corrupt(format!("stat failed: {err}")),
    };
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => return Qcow2State::Corrupt(format!("open failed: {err}")),
    };
    let mut read_at = |offset: u64, len: usize| -> Result<Vec<u8>, String> {
        file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    };
    let be32 = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let be64 = |b: &[u8], at: usize| {
        u64::from_be_bytes([
            b[at],
            b[at + 1],
            b[at + 2],
            b[at + 3],
            b[at + 4],
            b[at + 5],
            b[at + 6],
            b[at + 7],
        ])
    };

    let header = match read_at(0, 104) {
        Ok(header) => header,
        Err(err) => return Qcow2State::Corrupt(format!("header unreadable: {err}")),
    };
    if header[0..4] != *b"QFI\xfb" {
        return Qcow2State::Corrupt("bad magic".to_owned());
    }
    let version = be32(&header, 4);
    if version != 2 && version != 3 {
        return Qcow2State::Corrupt(format!("unsupported version {version}"));
    }
    let cluster_bits = be32(&header, 20);
    if !(9..=21).contains(&cluster_bits) {
        return Qcow2State::Corrupt(format!("bad cluster_bits {cluster_bits}"));
    }
    let cluster_size = 1u64 << cluster_bits;
    if file_len < cluster_size {
        return Qcow2State::Corrupt("truncated file".to_owned());
    }
    let virtual_size = be64(&header, 24);
    if virtual_size != expected_size {
        return Qcow2State::SizeMismatch { actual: virtual_size };
    }
    let l1_offset = be64(&header, 40);
    let refcount_table_offset = be64(&header, 48);
    if l1_offset == 0 || l1_offset >= file_len {
        return Qcow2State::Corrupt("L1 table out of range".to_owned());
    }
    if refcount_table_offset == 0 || refcount_table_offset >= file_len {
        return Qcow2State::Corrupt("refcount table out of range".to_owned());
    }
    // v3 carries the refcount order explicitly; v2 is fixed to 16-bit entries.
    // Non byte-sized refcount entries are rare, so skip the deep check rather
    // than misjudge a healthy image.
    let refcount_entry_bytes = if version == 3 {
        let order = be32(&header, 96);
        if !(3..=6).contains(&order) {
            return Qcow2State::Valid;
        }
        1usize << (order - 3)
    } else {
        2
    };
    let refcount_block_offset = match read_at(refcount_table_offset, 8) {
        Ok(entry) => be64(&entry, 0),
        Err(err) => return Qcow2State::Corrupt(format!("refcount table unreadable: {err}")),
    };
    if refcount_block_offset == 0 || refcount_block_offset >= file_len {
        return Qcow2State::Corrupt("refcount block out of range".to_owned());
    }
    let entry = match read_at(refcount_block_offset, refcount_entry_bytes) {
        Ok(entry) => entry,
        Err(err) => return Qcow2State::Corrupt(format!("refcount block unreadable: {err}")),
    };
    let header_refcount = entry.iter().fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    if header_refcount < 1 {
        return Qcow2State::Corrupt("header cluster marked free".to_owned());
    }
    Qcow2State::Valid
}

/// Reads the qcow2 virtual size (bytes) from a golden image's header, so the
/// working-disk inspect can compare against the golden rather than a UI tier.
fn golden_virtual_size(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(24)).ok()?;
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf).ok()?;
    Some(u64::from_be_bytes(buf))
}

/// Reads the QEMU launch settings from the user settings file, tolerating the
/// settings being unavailable at launch time (defaults to QEMU disabled).
fn read_launch_qemu_settings(base_path: &Path) -> LaunchQemuSettings {
    let mut settings = LaunchQemuSettings::default();
    let Some(file) = find_settings_json(base_path) else {
        boot_trace("read_launch_qemu_settings: no settings.json; defaults (enabled=false)");
        log::info!(
            "qemu_runtime: no user settings.json found under {}; using defaults",
            base_path.display()
        );
        return settings;
    };
    boot_trace(&format!("read_launch_qemu_settings: found {}", file.display()));
    let Ok(text) = std::fs::read_to_string(&file) else {
        log::warn!("qemu_runtime: read {} failed; using defaults", file.display());
        return settings;
    };
    if let Some(value) = find_bool(&text, "qemu_enabled") {
        settings.enabled = value;
    }
    if let Some(value) = find_number(&text, "qemu_cpu_cores") {
        settings.cores = value;
    }
    if let Some(value) = find_number(&text, "qemu_mem_gb") {
        settings.mem_gb = value;
    }
    if let Some(value) = find_number(&text, "qemu_disk_gb") {
        settings.disk_gb = value;
    }
    log::info!(
        "qemu_runtime: launch settings enabled={} cores={} mem={}G disk={}G (from {})",
        settings.enabled,
        settings.cores,
        settings.mem_gb,
        settings.disk_gb,
        file.display()
    );
    boot_trace(&format!(
        "read_launch_qemu_settings: enabled={} cores={} mem={}G disk={}G",
        settings.enabled, settings.cores, settings.mem_gb, settings.disk_gb
    ));
    settings
}

/// Reads the user-chosen data root from the record file under `base_path`.
/// `None` when no home directory was ever recorded, in which case the app falls
/// back to a data root inside the sandbox.
fn read_data_root(base_path: &Path) -> Option<PathBuf> {
    let record = base_path.join(HOME_DIRECTORY_RECORD_FILE);
    match std::fs::read_to_string(&record) {
        Ok(home) if !home.trim().is_empty() => Some(PathBuf::from(home.trim())),
        Ok(_) => None,
        Err(err) => {
            log::warn!(
                "qemu_runtime: read home record {} failed: {err}",
                record.display()
            );
            None
        }
    }
}

/// Locates the user settings file for this launch.
///
/// The data root is the user-chosen home directory recorded in
/// `<base_path>/custom_data_dir` (see `paths::set_custom_data_dir`), so the
/// settings live at `<home>/config/settings.json`. Only when there is no
/// recorded home directory do we fall back to scanning the sandbox, which is
/// where the data root used to live.
fn find_settings_json(base_path: &Path) -> Option<PathBuf> {
    if let Some(data_root) = read_data_root(base_path) {
        let file = data_root
            .join(HOME_CONFIG_SUBDIR)
            .join(SETTINGS_FILE_NAME);
        if file.is_file() {
            return Some(file);
        }
    }

    const MAX_DEPTH: usize = 5;
    const MAX_VISITS: usize = 800;
    // Search the whole el2 base (the app's own data area) rather than only the
    // `files/` subtree, where the user settings may or may not live.
    let root = base_path.ancestors().nth(3).unwrap_or(base_path).to_owned();
    let mut visited = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root, 0)];
    while let Some((dir, depth)) = stack.pop() {
        if visited > MAX_VISITS || depth > MAX_DEPTH {
            continue;
        }
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_VISITS {
                break;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push((path, depth + 1));
            } else if path.file_name().and_then(|n| n.to_str()) == Some("settings.json") {
                return Some(path);
            }
        }
    }
    None
}

/// Extracts a boolean JSON value for `key` (top level, tolerant of spacing).
fn find_bool(text: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let idx = text.find(&needle)?;
    let tail = &text[idx + needle.len()..];
    let value = tail.trim_start();
    let value = value.strip_prefix(':')?.trim_start();
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Extracts an enum token such as `cpu4` / `mem8` / `disk128` and maps it to
/// its numeric value, falling back to the default tier.
fn find_number(text: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\"");
    let idx = text.find(&needle)?;
    let tail = &text[idx + needle.len()..];
    let value = tail.trim_start();
    let value = value.strip_prefix(':')?.trim_start();
    let value = value.strip_prefix('"')?;
    let token: String = value.chars().take_while(|c| *c != '"').collect();
    token
        .chars()
        .skip_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .parse()
        .ok()
}
