use std::path::{Path, PathBuf};
use std::sync::Arc;

use cmd_client::{CommandEndpoint, RemoteCommandExecutor, SshCommandExecutor};
use openharmony_ability_derive::ability;

/// Environment variable carrying the app sandbox's `files/` directory.
///
/// Set by `ensure_shell_env` on the main thread, because
/// `openharmony_ability::global_app()` is a thread_local: work running off the
/// main thread (such as the terminal's shell probe) can only reach the sandbox
/// through this name.
const SANDBOX_FILES_DIR_ENV: &str = "SANDBOX_FILES_DIR";

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
    // discovery would otherwise fail before spawn. See ensure_shell_env.
    ensure_shell_env(app.base_path(), app.home_directory());
    // Snapshot the private HNP install dir (/data/app/bin) once: the set of
    // on-device tools never changes while the process lives, so util::command
    // routes local vs the daemon from this snapshot without re-reading the directory.
    util::command::init_local_tools();
    // [diag] Report which on-device HNP tools (git/ssh/curl) resolved once the
    // zlog->hilog redirect is live; a missing tool shows one clear line instead
    // of a runtime error later.
    log_local_tools_delayed();
    // Start the on-device daemon client (management bootstrap + command pool)
    // so remote command execution is ready before Zed starts issuing git/LSP
    // commands. The daemon replaces the retired openeuler-VM and QEMU backends.
    start_daemon_client(&app);
    // Launch Zed with only the information it truly needs: the sandbox base path
    // and the home directory the ets side resolved before this native module loaded.
    start_zed_main(app.base_path(), app.home_directory());
}

/// Records the resolved data root so the next launch's ets-side check can find it
/// without touching `paths` (which must not be initialized before it is set).
const HOME_DIRECTORY_RECORD_FILE: &str = "custom_data_dir";

/// Resolves the data root, publishes it to `paths`, then enters Zed's own `main`.
///
/// Zed is entered through `zed::main` rather than a bespoke entry point so the
/// whole upstream start-up path (argument parsing, single-instance check, logging
/// setup) stays in charge; only the data root differs from a desktop launch.
fn start_zed_main(base_path: Option<String>, home_directory: Option<String>) {
    // Must be set before `zed::main` runs: upstream selects the accessible
    // application only when this is "1", and the OHOS platform has no
    // accessibility backend - so the flag keeps that upstream shape.
    std::env::set_var("ZED_EXPERIMENTAL_A11Y", "1");
    let base_path = base_path.filter(|path| !path.is_empty());
    let home_directory = home_directory.filter(|path| !path.is_empty());

    match resolve_home_directory(home_directory.as_deref()) {
        Some(home_directory) => {
            let data_dir = paths::set_custom_data_dir(&home_directory);
            if let Some(base_path) = base_path.as_deref() {
                write_home_directory_record(base_path, &data_dir.to_string_lossy());
            }
        }
        None => {
            // No usable home directory (none chosen, or it became unreachable):
            // fall back to the sandbox so the app still starts.
            log::warn!("start_zed_main: no usable home directory; falling back to the sandbox");
            if let Some(base_path) = base_path {
                // Product data subdirectory (not `zed`): the QEMU guest mounts the whole
                // sandbox, so downloaded programs live under this directory of base_path.
                let data_dir = PathBuf::from(base_path).join("hicodeer");
                if let Some(data_dir) = data_dir.to_str() {
                    paths::set_custom_data_dir(data_dir);
                }
            }
        }
    }
    zed::main();
}

/// Resolves the directory to use as the data root. Prefers the user home
/// directory and makes sure it exists first. Returns `None` when it cannot be
/// used, so the caller falls back to the sandbox instead of letting
/// `set_custom_data_dir` panic on an unreachable path.
fn resolve_home_directory(home_directory: Option<&str>) -> Option<String> {
    let home_directory = home_directory?;
    match std::fs::create_dir_all(home_directory) {
        Ok(()) => Some(home_directory.to_owned()),
        Err(err) => {
            log::error!("start_zed_main: home directory {home_directory} is unusable: {err}");
            None
        }
    }
}

/// Writes the resolved data root to `<base_path>/custom_data_dir`. Idempotent:
/// the ets side writes the same value when the user picks a directory.
fn write_home_directory_record(base_path: &str, home_directory: &str) {
    let record = PathBuf::from(base_path).join(HOME_DIRECTORY_RECORD_FILE);
    if let Err(err) = std::fs::write(&record, home_directory) {
        log::error!("start_zed_main: write {} failed: {err}", record.display());
    }
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
fn ensure_shell_env(base_path: Option<String>, home_directory: Option<String>) {
    // Export the sandbox files directory under its own name (see the constant).
    // The terminal's shell probe runs off the main thread, so it cannot call
    // `global_app()`, yet it needs an on-device directory: the FIFOs that park
    // its local pty child cannot live under HOME, which points at a virtiofs
    // share once the user has picked a home directory, and virtiofs rejects
    // mkfifo with EPERM.
    if let Some(base_path_ref) = base_path.as_deref() {
        std::env::set_var(SANDBOX_FILES_DIR_ENV, base_path_ref);
    }
    std::env::set_var("SHELL", "/bin/sh");
    if std::env::var_os("USER").is_none() {
        std::env::set_var("USER", "app");
    }
    // HOME is the directory the user picked (the ets side passes it as
    // homeDirectory), not the app-private sandbox dir. Tools that keep their
    // state under $HOME - git config, ~/.codebuddy, shell history - need a
    // directory the user owns, can reach outside the host application, and keeps its
    // contents across reinstalls; the sandbox dir satisfies none of those.
    // base_path stays as the fallback for the launch before a directory is chosen.
    let home = home_directory
        .filter(|dir| !dir.is_empty())
        .or_else(|| base_path.clone());
    if let Some(home_ref) = home.as_deref() {
        std::env::set_var("HOME", home_ref);
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
                "ensure_shell_env: application_resource_dir unavailable: {err}"
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
            log::warn!("ensure_shell_env: {ca_bundle} missing in resfile");
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

/// Starts the command backend and registers the process-wide executor so
/// `util::command` can execute commands. The on-device command service is the
/// always-present backend; when the QEMU guest backend is compiled in it gets
/// the first chance and falls back here on any failure.
fn start_daemon_client(app: &openharmony_ability::OpenHarmonyApp) {
    #[cfg(feature = "qemu-agent")]
    crate::qemu_runtime::start_command_backend(app);
    #[cfg(not(feature = "qemu-agent"))]
    register_ohos_backend(app);
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

// ==================== On-device command service ====================
// The command service that runs on the device itself, reached over loopback
// SSH on 4022/4023. Nothing below depends on the embedded QEMU guest backend,
// so it stays available whether or not that backend is compiled in.

/// [diag] Boot-time trace that writes straight to hilog, bypassing the global
/// logger: the command backend is chosen here, BEFORE the zlog logger is
/// installed in `start_zed_main`, so plain `log::*!` lines from here are
/// dropped silently during cold start. Keeping the whole decision chain visible
/// required a direct channel; keep this until the backend selection is stable,
/// then delete the calls (they are pure diagnostics).
pub(crate) fn boot_trace(msg: &str) {
    #[cfg(target_env = "ohos")]
    zlog::ohos::direct_hilog_info("qemu-boot", msg);
    #[cfg(not(target_env = "ohos"))]
    let _ = msg;
}

/// OHOS app module name (resfile resources live under it).
const APP_MODULE_NAME: &str = "entry";
/// Resfile subdir holding the on-device command service management keys.
const OHOS_KEY_SUBDIR: &str = "hicodeerd-mgmt";
/// Management host public key file (client half).
const MGMT_HOST_PUB_FILE: &str = "mgmt-host.pub";
/// Management client private key file (client half).
const MGMT_CLIENT_KEY_FILE: &str = "mgmt-client-key";

/// Registers the on-device command service as the process-wide command executor.
pub(crate) fn register_ohos_backend(_app: &openharmony_ability::OpenHarmonyApp) {
    boot_trace("register_ohos_backend entered");
    let resource_dir = match resource_dir() {
        Some(dir) => dir,
        None => {
            log::error!("register_ohos_backend: no resource dir");
            return;
        }
    };
    let (host_pub, client_key) =
        match read_management_keys(&resource_dir.join(OHOS_KEY_SUBDIR)) {
            Some(pair) => pair,
            None => {
                log::error!("register_ohos_backend: no OHOS management keys");
                return;
            }
        };
    let inner = match SshCommandExecutor::new(
        CommandEndpoint::ohos_default(),
        client_key,
        host_pub,
    ) {
        Ok(executor) => Arc::new(executor) as Arc<dyn RemoteCommandExecutor>,
        Err(err) => {
            log::error!("register_ohos_backend: create executor: {err}");
            return;
        }
    };
    register_executor(inner);
}

/// Registers the executor globally and initializes `util::command`.
pub(crate) fn register_executor(executor: Arc<dyn RemoteCommandExecutor>) {
    if let Err(()) = cmd_client::init_executor(executor.clone()) {
        log::warn!("launch_app: executor already registered");
    }
    if let Err(err) = util::command::init("") {
        log::warn!("launch_app: util command init failed: {err}");
    }
    boot_trace("register_executor done");
    log::info!("launch_app: command executor registered");
}

/// Reads the fixed management key pair (host public + client private) from a
/// resfile key directory.
pub(crate) fn read_management_keys(key_dir: &Path) -> Option<(String, String)> {
    let host_pub = std::fs::read_to_string(key_dir.join(MGMT_HOST_PUB_FILE)).ok()?;
    let client_key = std::fs::read_to_string(key_dir.join(MGMT_CLIENT_KEY_FILE)).ok()?;
    Some((host_pub, client_key))
}

/// Resolves the module resource directory (el1 resfile).
pub(crate) fn resource_dir() -> Option<PathBuf> {
    match openharmony_ability::application_resource_dir(APP_MODULE_NAME) {
        Ok(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        Ok(_) => {
            log::error!("launch_app: empty application_resource_dir");
            None
        }
        Err(err) => {
            log::error!("launch_app: application_resource_dir failed: {err}");
            None
        }
    }
}
