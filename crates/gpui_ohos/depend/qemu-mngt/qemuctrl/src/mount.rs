//! Dynamic work-directory mounts (add-only, never unmounted).
//!
//! Mounting a host worktree into the guest is a three-step dance: start a
//! writable virtiofsd backend for the host directory, hotplug the matching
//! vhost-user-fs device over QMP, then ask the guest to `mkdir` the same
//! absolute path and `mount -t virtiofs <tag>` onto it. The guest mount command
//! runs through a [`GuestShell`] supplied by the caller (cmd-client's
//! `run_shell` against the guest daemon), so this crate never talks SSH
//! itself. Slots map to the pcie-root-ports created at boot
//! ([`crate::WORKDIR_MOUNT_SLOTS`]); only add-only semantics are supported.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::QMP_SOCKET;
use crate::virtiofs;

/// Number of in-guest retries for a workdir virtiofs mount. The vhost-user-fs
/// device is hotplugged over QMP moments earlier; the guest must enumerate the
/// new pcie port and bind the virtiofs driver before the tag exists, so the
/// first mount attempt often lands too early and needs a bounded retry window.
/// Mirrors the retired qemu-agent mount retry loop.
const GUEST_MOUNT_RETRIES: usize = 10;
/// Seconds between in-guest mount retries. Fractional seconds are accepted by
/// the guest's busybox `sleep`; the `|| sleep 1` fallback covers a build of it
/// that only parses integers.
const GUEST_MOUNT_RETRY_DELAY: &str = "0.3";

/// Runs one shell command inside the guest as root. Injected by the caller so
/// this crate stays free of SSH / cmd-client dependencies.
pub trait GuestShell {
    /// Executes `command` in the guest and fails on a non-zero exit.
    fn run(&self, command: &str) -> std::io::Result<()>;
}

/// Tracks the per-slot and per-directory mount state for one guest.
pub struct MountRegistry {
    port_dir: PathBuf,
    state: Mutex<MountState>,
}

struct MountState {
    /// Host directories already mounted (canonicalized), for dedup.
    mounted: HashSet<PathBuf>,
    /// Occupancy of each hotplug slot; a slot is `Some` once used.
    slots: Vec<Option<PathBuf>>,
}

impl MountRegistry {
    /// Creates an empty registry rooted at the guest's port directory.
    pub fn new(port_dir: PathBuf) -> Self {
        Self {
            port_dir,
            state: Mutex::new(MountState {
                mounted: HashSet::new(),
                slots: vec![None; crate::WORKDIR_MOUNT_SLOTS],
            }),
        }
    }

    /// Whether `host_dir` has already been mounted into the guest.
    pub fn is_mounted(&self, host_dir: &Path) -> bool {
        let key = canonical(host_dir);
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .mounted
            .contains(&key)
    }

    /// Mounts `host_dir` into the guest at the same absolute path. Idempotent.
    /// Fails when every hotplug slot is already occupied. A failed attempt
    /// burns its slot (marked occupied): the spawned backend thread keeps its
    /// socket bound and any hotplugged device stays in QEMU, so a retry gets a
    /// fresh slot instead of stacking a second backend on the same socket.
    pub fn mount_workdir(
        &self,
        host_dir: PathBuf,
        guest_shell: &dyn GuestShell,
    ) -> std::io::Result<()> {
        let key = canonical(&host_dir);
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.mounted.contains(&key) {
            return Ok(());
        }
        let Some(sequence) = state.slots.iter().position(Option::is_none) else {
            let err = format!(
                "mount: all {} workdir slots are occupied",
                crate::WORKDIR_MOUNT_SLOTS
            );
            log::warn!("{err}");
            return Err(std::io::Error::other(err));
        };
        let slot = sequence as u64;
        let tag = virtiofs::workdir_tag(slot);
        // 1) Start the writable backend for this host directory.
        let backend_socket =
            virtiofs::spawn_workdir(&self.port_dir, slot, host_dir.clone()).map_err(|err| {
                std::io::Error::new(
                    err.kind(),
                    format!("mount: spawn backend for {tag} failed: {err}"),
                )
            })?;
        // 2) Hotplug the vhost-user-fs device onto the matching root port.
        // Any failure past the backend spawn leaves the slot unusable: the
        // backend thread keeps its socket bound (a retry on the same slot
        // would stack a second listener on the same socket), and after a
        // successful device_add QEMU also keeps the chardev/device ids. So
        // mark the slot occupied ("burned") and let a retry pick a fresh one.
        let qmp_socket = self.port_dir.join(QMP_SOCKET);
        let chardev_id = format!("fs_work{slot}");
        let device_id = format!("workdev{slot}");
        let bus = format!("rp{slot}");
        if let Err(err) = crate::qmp::create_workdir_vhost_fs(
            &qmp_socket.to_string_lossy(),
            &chardev_id,
            &backend_socket.to_string_lossy(),
            &device_id,
            &tag,
            &bus,
        ) {
            log::error!("mount: QMP device_add {tag} failed; slot {sequence} burned");
            state.slots[sequence] = Some(key);
            return Err(std::io::Error::new(
                err.kind(),
                format!("mount: QMP device_add {tag} failed: {err}"),
            ));
        }
        // 3) mkdir the same path in the guest and mount the tag onto it. The
        // device was just hotplugged over QMP, so retry the mount for a bounded
        // window: the guest enumerates the pcie port and binds the virtiofs
        // driver with some delay, and the tag only exists after that.
        let mount_path = sh_quote(&host_dir.to_string_lossy());
        let mount_tag = sh_quote(&tag);
        let retry_slots = (1..=GUEST_MOUNT_RETRIES)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let command = format!(
            "mkdir -p {mount_path} && for i in {retry_slots}; do mount -t virtiofs {mount_tag} {mount_path} && exit 0; sleep {GUEST_MOUNT_RETRY_DELAY} 2>/dev/null || sleep 1; done; exit 1"
        );
        if let Err(err) = guest_shell.run(&command) {
            log::error!("mount: guest mount {tag} failed; slot {sequence} burned (device stays hotplugged)");
            state.slots[sequence] = Some(key);
            return Err(std::io::Error::new(
                err.kind(),
                format!("mount: guest mount {tag} failed: {err}"),
            ));
        }
        state.mounted.insert(key.clone());
        state.slots[sequence] = Some(key);
        Ok(())
    }

    /// Clears all mount state. Called when the guest relaunches (engine
    /// restart hook): a fresh guest boots with no workdir mounts, so dedup
    /// entries and burned slots from the previous guest instance must not
    /// survive it — otherwise callers would be told a path is still mounted
    /// when the new guest has nothing mounted.
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        if !state.mounted.is_empty() {
            log::info!(
                "mount: guest relaunch cleared {} mounted workdir(s)",
                state.mounted.len()
            );
        }
        state.mounted.clear();
        for slot in state.slots.iter_mut() {
            *slot = None;
        }
    }
}

/// Canonicalizes `path`, falling back to the raw path when it cannot be
/// resolved (e.g. the directory is not yet visible to this process).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Quotes a single shell word with single quotes (no interpolation). Embedded
/// single quotes become the standard `'\''` sequence.
fn sh_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    out.push_str(&value.replace('\'', "'\\''"));
    out.push('\'');
    out
}
