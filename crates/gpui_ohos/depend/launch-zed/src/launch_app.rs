use openharmony_ability_derive::ability;

// VM parameters for the cmd-agent daemon: the remote server runs on the VM and
// is reached over TCP; SSH credentials let the daemon auto-redeploy it. The VM
// host is resolved dynamically at startup (see compute_vm_host); these are the
// fallbacks used when the WVMBrEulerOS bridge interface is missing.
#[cfg(feature = "openeuler-agent")]
const SSH_HOST: &str = "172.16.100.2";
#[cfg(feature = "openeuler-agent")]
const SSH_PORT: u16 = 22;
#[cfg(feature = "openeuler-agent")]
const SSH_USER: &str = "user";
#[cfg(feature = "openeuler-agent")]
const SSH_PASS: &str = "12345678";
#[cfg(feature = "openeuler-agent")]
const REMOTE_DIR: &str = "/home/user/cmd-agent";
#[cfg(feature = "openeuler-agent")]
const AGENT_PORT: u16 = 4040;
/// Number of attempts to initialize the business-side client after the daemon
/// thread starts (the daemon needs a moment to bind its unix socket).
#[cfg(feature = "openeuler-agent")]
const CLIENT_INIT_ATTEMPTS: usize = 10;
/// Delay between client init attempts.
#[cfg(feature = "openeuler-agent")]
const CLIENT_INIT_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

// Replaces the NAPI launch entry that used to live in crates/zed/src/lib.rs.
// Dependency direction is now openharmony-ability -> zed: this entry depends on
// zed and only passes it the information zed truly needs (the sandbox base path).
#[ability]
pub fn launch_app(app: openharmony_ability::OpenHarmonyApp) {
    // Hand the app to the platform layer immediately; OhosPlatform picks it up from
    // the global on construction, so gpui never sees the OpenHarmonyApp type.
    openharmony_ability::set_global_app(app.clone());
    // [ohos] Pin the terminal child shell to /bin/sh before Zed starts. The
    // sandbox only execs /bin/sh and the app uid has no /etc/passwd entry
    // (every present entry resolves to /bin/false), so alacritty's shell
    // discovery would otherwise fail before spawn. See ensure_terminal_shell_env.
    ensure_terminal_shell_env(app.base_path());
    // Register the OHOS platform factory before Zed constructs any platform.
    gpui_ohos::register_platform();
    // Start the QEMU guest and/or the OpenEuler VM cmd-agent path depending on
    // the enabled backend feature, so remote command execution is ready before
    // Zed starts issuing git/LSP commands. The two features can coexist during
    // the transition: qemu boots the guest while openeuler-agent keeps the
    // existing command path alive.
    #[cfg(feature = "qemu-agent")]
    start_qemu(&app);
    #[cfg(feature = "openeuler-agent")]
    start_cmd_agent(&app);
    // Launch Zed with only the information it truly needs: the sandbox base path.
    zed::start_zed_main(app.base_path());
}

/// Pins the process environment that alacritty's terminal shell discovery
/// (`ShellUser::from_env`) reads, so an OHOS terminal always execs `/bin/sh`.
///
/// Why this is required on OHOS:
///   - The sandbox whitelist only allows `execve` of `/bin/sh`.
///   - `/etc/passwd` has no entry for the app uid, and every present entry
///     resolves its shell to `/bin/false`.
/// Without `SHELL`/`USER`/`HOME` all set, `ShellUser::from_env` errors out and
/// opening a terminal fails before the child is even spawned.
fn ensure_terminal_shell_env(base_path: Option<String>) {
    std::env::set_var("SHELL", "/bin/sh");
    if std::env::var_os("USER").is_none() {
        std::env::set_var("USER", "app");
    }
    // HOME should be the app's el2 sandbox directory (base_path), not whatever
    // the process launcher seeded (e.g. /storage/Users/currentUser): base_path
    // is the app-private, writable directory the device-local shell can rely on.
    if let Some(base_path_ref) = base_path.as_deref() {
        std::env::set_var("HOME", base_path_ref);
    }
    // Extend PATH with the hap's el1 (resfile resources, reachable via
    // application_resource_dir) and el2 (app sandbox files, i.e. base_path)
    // directories, plus the resfile curl/ dir, so device-local tools bundled in
    // the resfile (e.g. curl) are reachable from the terminal shell.
    let mut curl_lib_dir: Option<String> = None;
    let mut path_extra: Vec<String> = Vec::new();
    if let Some(base_path_ref) = base_path.as_deref() {
        path_extra.push(base_path_ref.to_string());
    }
    const APP_MODULE_NAME: &str = "entry";
    match openharmony_ability::application_resource_dir(APP_MODULE_NAME) {
        Ok(resource_dir) => {
            let curl_dir = format!("{resource_dir}/curl");
            curl_lib_dir = Some(curl_dir.clone());
            path_extra.push(resource_dir);
            path_extra.push(curl_dir);
        }
        Err(err) => log::warn!(
            "ensure_terminal_shell_env: application_resource_dir unavailable: {err}"
        ),
    }
    if !path_extra.is_empty() {
        let current_path = std::env::var("PATH").unwrap_or_default();
        let mut full_path = current_path;
        for extra in &path_extra {
            if !full_path.is_empty() {
                full_path.push(':');
            }
            full_path.push_str(extra);
        }
        std::env::set_var("PATH", full_path);
    }
    // Let the dynamic loader find curl's bundled libraries: the resfile curl/
    // dir holds the transitive .so closure next to the curl binary.
    if let Some(curl_dir) = curl_lib_dir {
        let current = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        let full_lib_path = if current.is_empty() {
            curl_dir
        } else {
            format!("{curl_dir}:{current}")
        };
        std::env::set_var("LD_LIBRARY_PATH", full_lib_path);
    }
}

/// Starts the cmd-agent daemon (background thread) and initializes the
/// business-side client so `util::command` can execute remote commands.
#[cfg(feature = "openeuler-agent")]
fn start_cmd_agent(app: &openharmony_ability::OpenHarmonyApp) {
    let Some(base_path) = app.base_path() else {
        log::error!("start_cmd_agent: no base path, cmd-agent not started");
        return;
    };
    let socket_path = format!("{base_path}/cmd-agent.sock");
    log::info!("start_cmd_agent: socket_path={socket_path}");
    // The packaged server binary lives in the module resfile; hand its
    // read-only path to the daemon so `recover_vm` can redeploy it. The
    // resource-dir API takes the application module name (module.json5 `name`,
    // here "entry"), not the native-library module name used by EntryAbility.
    const APP_MODULE_NAME: &str = "entry";
    let server_binary = openharmony_ability::application_resource_dir(APP_MODULE_NAME)
        .ok()
        .map(|resource_dir| format!("{resource_dir}/cmd-agentd"));
    log::info!("start_cmd_agent: server_binary={server_binary:?}");
    // Process-internal shared state between the daemon thread and the business
    // client: spawn confirmations, exit results, and the signal channel. The
    // daemon fills the tables from VM events; the client reads them directly,
    // so the client needs no management connection and no threads.
    let shared = cmd_agent::daemon::SharedControl::new();
    // Resolve the VM host dynamically: the VM's bridge IP can change between
    // reboots, so query the WVMBrEulerOS bridge interface (IPv4 + 1) and only
    // fall back to the hardcoded default when the interface is missing.
    let vm_host = compute_vm_host().unwrap_or_else(|| SSH_HOST.to_string());
    log::info!("start_cmd_agent: vm_host={vm_host}");
    let args = cmd_agent::daemon::Args {
        unix_socket: std::path::PathBuf::from(&socket_path),
        vm_addr: format!("{vm_host}:{AGENT_PORT}"),
        ssh: Some(cmd_agent::deploy::SshConfig {
            host: vm_host.clone(),
            port: SSH_PORT,
            user: SSH_USER.to_string(),
            password: SSH_PASS.to_string(),
            remote_dir: REMOTE_DIR.to_string(),
        }),
        server_binary: server_binary.map(std::path::PathBuf::from),
        agent_port: AGENT_PORT,
        shared: shared.clone(),
        // Pass only the sandbox base; the daemon derives the sync roots from it
        // (base/zcoder/<subdir>). Never touch paths::data_dir here: it would
        // initialize CURRENT_DATA_DIR and trip set_custom_data_dir's guard in
        // start_zed_main.
        sandbox_base: Some(base_path.clone()),
    };
    if let Err(err) = cmd_agent::daemon::spawn_daemon(args) {
        log::error!("start_cmd_agent: failed to spawn cmd-agent daemon thread: {err}");
        return;
    }
    // The daemon binds its unix socket shortly after starting; retry the
    // client init until the connection succeeds. The business-side client is
    // registered as the remote-command executor, which `util::command` uses
    // through the stable cmd-agent-linker interface.
    for attempt in 0..CLIENT_INIT_ATTEMPTS {
        match cmd_agent::client::Client::connect(&socket_path, None, shared.clone()) {
            Ok(client) => {
                if cmd_agent_linker::init_executor(client).is_err() {
                    log::warn!("cmd-agent executor already registered");
                }
                if let Err(err) = util::command::init(&socket_path) {
                    log::warn!("cmd-agent util init failed: {err}");
                }
                capture_vm_arch();
                log::info!("cmd-agent client initialized (attempt {attempt})");
                return;
            }
            Err(err) => {
                log::debug!("cmd-agent client connect attempt {attempt} failed: {err}");
                std::thread::sleep(CLIENT_INIT_DELAY);
            }
        }
    }
    log::error!("cmd-agent client init failed after retries");
}

/// Best-effort capture of the VM's CPU architecture once the cmd-agent
/// executor is up, so LSP downloads are keyed to the VM platform rather than
/// the device's. Runs on a background thread; if it fails (e.g. the VM is
/// still being deployed), `vm_platform()` stays `None` and callers fall back
/// to the device architecture.
#[cfg(feature = "openeuler-agent")]
fn capture_vm_arch() {
    std::thread::spawn(|| {
        let arch = smol::block_on(async {
            util::command::new_command("uname")
                .arg("-m")
                .output()
                .await
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| {
                    let arch = String::from_utf8(output.stdout).ok()?.trim().to_string();
                    if arch.is_empty() {
                        None
                    } else {
                        Some(arch)
                    }
                })
        });
        match arch {
            Some(arch) => {
                log::info!("capture_vm_arch: captured VM arch: {arch}");
                gpui_ohos_linker::set_vm_arch(arch);
            }
            None => log::warn!(
                "capture_vm_arch: failed to query VM arch; downloads fall back to device arch"
            ),
        }
    });
}

/// Computes the VM host as the local `WVMBrEulerOS` bridge interface IPv4 + 1
/// (the VM sits one IP past the bridge, e.g. interface 172.16.105.1 -> VM
/// 172.16.105.2). The same scheme as warp-ohos's shell_bridge_process.cpp.
/// Returns None when the bridge interface is missing or has no IPv4, so
/// callers fall back to a hardcoded default.
#[cfg(feature = "openeuler-agent")]
fn compute_vm_host() -> Option<String> {
    const VM_BRIDGE_IFACE: &str = "WVMBrEulerOS";
    const VM_HOST_IP_OFFSET: u32 = 1;
    // SAFETY: getifaddrs allocates a linked list that must be released with
    // freeifaddrs; the nodes are valid until that call.
    let mut ifaddr: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut ifaddr) } != 0 {
        log::warn!("compute_vm_host: getifaddrs failed");
        return None;
    }
    let mut result = None;
    let mut current = ifaddr;
    while !current.is_null() {
        // SAFETY: walking the ifaddrs linked list; each node is valid until
        // freeifaddrs.
        let ifa = unsafe { &*current };
        if !ifa.ifa_addr.is_null()
            && unsafe { (*ifa.ifa_addr).sa_family } == libc::AF_INET as libc::sa_family_t
        {
            // SAFETY: ifa_name is a NUL-terminated C string owned by the node.
            let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
                .to_string_lossy()
                .into_owned();
            if name == VM_BRIDGE_IFACE {
                // SAFETY: an AF_INET ifa_addr points to a sockaddr_in.
                let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let host_ip =
                    u32::from_be(sin.sin_addr.s_addr).wrapping_add(VM_HOST_IP_OFFSET);
                result = Some(std::net::Ipv4Addr::from(host_ip).to_string());
                break;
            }
        }
        current = ifa.ifa_next;
    }
    // SAFETY: matching the getifaddrs allocation above.
    unsafe { libc::freeifaddrs(ifaddr) };
    if result.is_none() {
        log::warn!("compute_vm_host: {VM_BRIDGE_IFACE} not found, using fallback");
    }
    result
}

/// Starts the in-process QEMU guest as the command backend. Kernel and
/// initramfs are packaged in the module resfile and read directly (QEMU only
/// reads them, so no sandbox copy is needed); the sandbox and the user
/// workspace are exposed to the guest over virtio-9p.
#[cfg(feature = "qemu-agent")]
fn start_qemu(app: &openharmony_ability::OpenHarmonyApp) {
    let Some(base_path) = app.base_path() else {
        log::error!("start_qemu: no base path, QEMU not started");
        return;
    };
    // The resource-dir API takes the application module name (module.json5
    // `name`, here "entry"), not the native-library module name.
    const APP_MODULE_NAME: &str = "entry";
    let resource_dir = match openharmony_ability::application_resource_dir(APP_MODULE_NAME) {
        Ok(dir) => dir,
        Err(err) => {
            log::error!("start_qemu: application_resource_dir failed: {err}");
            return;
        }
    };
    // Console log and host-command socket live under the sandbox qemu dir.
    let qemu_dir = format!("{base_path}/qemu");
    if let Err(err) = std::fs::create_dir_all(&qemu_dir) {
        log::error!("start_qemu: create qemu dir {qemu_dir} failed: {err}");
        return;
    }
    // Port pool sockets live under qemu/ports; QEMU binds them as its chardev
    // servers, and the cmd-agent executor connects to them.
    let port_dir = format!("{qemu_dir}/ports");
    if let Err(err) = std::fs::create_dir_all(&port_dir) {
        log::error!("start_qemu: create port dir {port_dir} failed: {err}");
        return;
    }
    // Stage cmd-agentd into the sandbox root (base_path), where the guest
    // sees it via the 9p mount as /sandbox/cmd-agentd. The HAP resfile path
    // returned by application_resource_dir is NOT under base_path, so a plain
    // resfile deploy is invisible to the guest.
    let resfile_agentd = format!("{resource_dir}/ssh-agentd");
    let sandbox_agentd = format!("{base_path}/ssh-agentd");
    if std::fs::metadata(&resfile_agentd).map(|m| m.is_file()).unwrap_or(false) {
        match std::fs::copy(&resfile_agentd, &sandbox_agentd) {
            Ok(_) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &sandbox_agentd,
                    std::fs::Permissions::from_mode(0o755),
                );
                log::info!("start_qemu: staged ssh-agentd -> {sandbox_agentd}");
            }
            Err(err) => log::error!("start_qemu: stage ssh-agentd {resfile_agentd}: {err}"),
        }
    } else {
        log::warn!("start_qemu: ssh-agentd missing in resfile {resfile_agentd}");
    }
    // Mount the whole sandbox root (el2/base) instead of base_path: it exposes
    // the full app data area (files/cache/temp) to the guest under one 9p tag.
    // base_path is <sandbox_root>/haps/<module>/files, so the root sits at the
    // "/haps/" segment. cmd-agentd is staged into base_path by the copy below
    // and stays visible because base_path lives under the sandbox root.
    let sandbox_root = sandbox_root_path(&base_path);
    let paths = qemu_ssh_agent::QemuPaths {
        kernel: std::path::PathBuf::from(format!("{resource_dir}/Image")),
        initrd: std::path::PathBuf::from(format!("{resource_dir}/rootfs.cpio.zst")),
        port_dir: std::path::PathBuf::from(&port_dir),
        sandbox_mount: std::path::PathBuf::from(&sandbox_root),
        // Read-only resfile (el1/bundle) mounted to the guest as /tools, so the
        // prebundled binaries (clangd, python3, ssh) are visible there without
        // copying them into the sandbox.
        tools_mount: std::path::PathBuf::from(&resource_dir),
    };
    log::info!(
        "start_qemu: base={base_path} resource_dir={resource_dir} sandbox_mount={sandbox_root}"
    );
    // [diag] start_qemu logs before the zlog->hilog redirect is live (the
    // redirect happens inside zed::main), so the line above never reaches
    // hilog. Re-emit the key paths from a delayed thread once the redirect is
    // live so the mount/stage layout can be verified from hilog.
    log_qemu_paths_delayed(base_path.clone(), resource_dir, sandbox_root.clone());
    if !qemu_ssh_agent::start(paths) {
        log::error!("start_qemu: failed to start QEMU guest");
        return;
    }
    // Register the remote-command executor over the virtio-serial port pool.
    // The management thread retries until the guest cmd-agentd comes up, so
    // commands issued during guest boot simply queue for the port connection.
    // The sandbox root is passed so the management thread mounts it through
    // MountFolder2QEMU, registering the fixed host-root -> /sandbox mapping.
    match qemu_ssh_agent::SshCommandExecutor::new(
        std::path::PathBuf::from(&port_dir),
        Some(sandbox_root),
    ) {
        Ok(executor) => {
            let executor = std::sync::Arc::new(executor);
            if qemu_ssh_agent_linker::init_executor(executor.clone()).is_err() {
                log::warn!("start_qemu: cmd-agent executor already registered");
            } else {
                log::info!("start_qemu: SshCommandExecutor registered");
            }
            // Pin the guest wall clock to the host once, after the guest SSH
            // server is up. The guest has no RTC init (boots at 1970) and
            // drifts under TCG; a single pin at boot is enough (no periodic
            // re-sync). `sync_system_time_once` blocks on pool.allocate until
            // the guest is ready, so it is safe to call right after register.
            executor.clone().sync_system_time_once();
            if qemu_ssh_agent_linker::init_mounter(executor).is_err() {
                log::warn!("start_qemu: folder mounter already registered");
            } else {
                log::info!("start_qemu: FolderMounter registered");
            }
        }
        Err(err) => log::error!("start_qemu: create SshCommandExecutor: {err}"),
    }
}

/// Derives the OHOS sandbox root from the app files dir. base_path follows the
/// fixed layout `<sandbox_root>/haps/<module>/files`, so everything before the
/// "/haps/" segment is the sandbox root. Falls back to base_path when the
/// layout does not match.
#[cfg(feature = "qemu-agent")]
fn sandbox_root_path(base_path: &str) -> String {
    const HAPS_SEGMENT: &str = "/haps/";
    match base_path.rfind(HAPS_SEGMENT) {
        Some(index) => base_path[..index].to_string(),
        None => base_path.to_string(),
    }
}

/// [diag] Re-emits start_qemu's key paths from a background thread after the
/// zlog->hilog redirect is live, plus whether the staged cmd-agentd copy exists
/// in both the resfile and the sandbox. start_qemu's own logs are dropped
/// because they run before the redirect (see start_qemu).
#[cfg(feature = "qemu-agent")]
fn log_qemu_paths_delayed(base_path: String, resource_dir: String, sandbox_root: String) {
    std::thread::Builder::new()
        .name("qemu-path-diag".to_string())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let resfile_agentd = format!("{resource_dir}/cmd-agentd");
            let staged_agentd = format!("{base_path}/cmd-agentd");
            let resfile_ok = std::fs::metadata(&resfile_agentd)
                .map(|m| m.is_file())
                .unwrap_or(false);
            let staged_ok = std::fs::metadata(&staged_agentd)
                .map(|m| m.is_file())
                .unwrap_or(false);
            log::info!(
                "[diag] start_qemu paths: base={base_path} resource_dir={resource_dir} sandbox_mount={sandbox_root} resfile_cmd_agentd={resfile_ok} staged_cmd_agentd={staged_ok}"
            );
        })
        .ok();
}
