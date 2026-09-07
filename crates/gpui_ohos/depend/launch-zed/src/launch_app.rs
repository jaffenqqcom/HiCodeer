use openharmony_ability_derive::ability;

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
    // Snapshot the private HNP install dir (/data/app/bin) once: the set of
    // on-device tools never changes while the process lives, so util::command
    // routes local vs zcoderd from this snapshot without re-reading the directory.
    util::command::init_local_tools();
    // [diag] Report which on-device HNP tools (git/ssh/curl) resolved once the
    // zlog->hilog redirect is live; a missing tool shows one clear line instead
    // of a runtime error later.
    log_local_tools_delayed();
    // Register the OHOS platform factory before Zed constructs any platform.
    gpui_ohos::register_platform();
    // Start the on-device zcoderd client (management bootstrap + command pool)
    // so remote command execution is ready before Zed starts issuing git/LSP
    // commands. zcoderd replaces the retired openeuler-VM and QEMU backends.
    start_zcoderd_client(&app);
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
    // directories, so device-local tools bundled in the resfile are reachable
    // from the terminal shell.
    let mut path_extra: Vec<String> = Vec::new();
    if let Some(base_path_ref) = base_path.as_deref() {
        path_extra.push(base_path_ref.to_string());
    }
    const APP_MODULE_NAME: &str = "entry";
    const CA_BUNDLE_FILE: &str = "ca-bundle.crt";
    let resource_dir = match openharmony_ability::application_resource_dir(APP_MODULE_NAME) {
        Ok(resource_dir) => resource_dir,
        Err(err) => {
            log::warn!(
                "ensure_terminal_shell_env: application_resource_dir unavailable: {err}"
            );
            String::new()
        }
    };
    if !resource_dir.is_empty() {
        path_extra.push(resource_dir.clone());
        // Point TLS at the CA bundle shipped in the resfile so device-local
        // git/ssh/curl over https validates certificates instead of failing
        // "unable to get local issuer certificate". Child processes inherit it.
        let ca_bundle = format!("{resource_dir}/{CA_BUNDLE_FILE}");
        if std::fs::metadata(&ca_bundle)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            std::env::set_var("SSL_CERT_FILE", &ca_bundle);
            std::env::set_var("CURL_CA_BUNDLE", &ca_bundle);
        } else {
            log::warn!("ensure_terminal_shell_env: {ca_bundle} missing in resfile");
        }
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
    // Library loading for local HNP tools (ssh/curl/git) is handled by their
    // own official DT_RUNPATH ($ORIGIN/../lib on executables, $ORIGIN on the
    // bundled .so) - deliberately NOT via LD_LIBRARY_PATH. A process-level
    // LD_LIBRARY_PATH would be searched before that RUNPATH and shadow the
    // package's own libs, so no LD_LIBRARY_PATH is set here at all.
}

/// Starts the on-device zcoderd client and registers it as the remote command
/// executor so `util::command` executes commands through zcoderd over loopback
/// SSH.
///
/// The client half of the fixed management keys is read from the module resfile
/// (`<resfile>/zcoderd-mgmt/{mgmt-host.pub,mgmt-client-key}`); the server half
/// lives inside the zcoderd public HNP. `cmd_client::SshCommandExecutor::new`
/// spawns a background bootstrap thread that keeps re-fetching zcoderd's
/// dynamic command keys (recovering across zcoderd restarts), so this returns
/// as soon as the executor is constructed and never blocks a calling thread.
fn start_zcoderd_client(_app: &openharmony_ability::OpenHarmonyApp) {
    const APP_MODULE_NAME: &str = "entry";
    const MGMT_KEY_SUBDIR: &str = "zcoderd-mgmt";
    const MGMT_HOST_PUB_FILE: &str = "mgmt-host.pub";
    const MGMT_CLIENT_KEY_FILE: &str = "mgmt-client-key";

    let resource_dir = match openharmony_ability::application_resource_dir(APP_MODULE_NAME) {
        Ok(resource_dir) => resource_dir,
        Err(err) => {
            log::error!("start_zcoderd_client: application_resource_dir failed: {err}");
            return;
        }
    };
    let key_dir = std::path::Path::new(&resource_dir).join(MGMT_KEY_SUBDIR);
    let mgmt_host_pub = match std::fs::read_to_string(key_dir.join(MGMT_HOST_PUB_FILE)) {
        Ok(text) => text,
        Err(err) => {
            log::error!(
                "start_zcoderd_client: read {} failed: {err}",
                MGMT_HOST_PUB_FILE
            );
            return;
        }
    };
    let mgmt_client_key = match std::fs::read_to_string(key_dir.join(MGMT_CLIENT_KEY_FILE)) {
        Ok(text) => text,
        Err(err) => {
            log::error!(
                "start_zcoderd_client: read {} failed: {err}",
                MGMT_CLIENT_KEY_FILE
            );
            return;
        }
    };
    log::info!("start_zcoderd_client: loading management keys from {}", key_dir.display());

    match cmd_client::SshCommandExecutor::new(mgmt_client_key, mgmt_host_pub) {
        Ok(executor) => {
            let executor = std::sync::Arc::new(executor);
            if cmd_client::init_executor(executor.clone()).is_err() {
                log::warn!("start_zcoderd_client: executor already registered");
            } else {
                log::info!("start_zcoderd_client: SshCommandExecutor registered");
            }
            if let Err(err) = util::command::init("") {
                log::warn!("start_zcoderd_client: util init failed: {err}");
            }
        }
        Err(err) => log::error!("start_zcoderd_client: create SshCommandExecutor: {err}"),
    }
}

/// [diag] From a background thread ~3s after launch (once the zlog->hilog
/// redirect is live) resolve each on-device HNP tool (git/ssh/curl) and log
/// whether it is present and executable. Mirrors `util::command`'s local routing
/// decision so a missing HNP surfaces as a single clear startup line.
fn log_local_tools_delayed() {
    const DELAY_SECS: u64 = 3;
    std::thread::Builder::new()
        .name("local-tools-diag".to_string())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(DELAY_SECS));
            for program in util::command::local_tool_programs() {
                let status = util::command::local_tool_status(program);
                match &status.resolved {
                    Some(path) => log::info!(
                        "[diag] local tool {} -> {} (executable={})",
                        status.program,
                        path.display(),
                        status.executable
                    ),
                    None => log::warn!(
                        "[diag] local tool {} MISSING: {}",
                        status.program,
                        status.error.as_deref().unwrap_or("unknown error")
                    ),
                }
            }
        })
        .ok();
}
