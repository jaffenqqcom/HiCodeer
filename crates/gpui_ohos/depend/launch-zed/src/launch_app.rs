use openharmony_ability_derive::ability;

// VM parameters for the cmd-agent daemon: the remote server runs on the VM and
// is reached over TCP; SSH credentials let the daemon auto-redeploy it.
const VM_ADDR: &str = "172.16.100.2:4040";
const SSH_HOST: &str = "172.16.100.2";
const SSH_PORT: u16 = 22;
const SSH_USER: &str = "user";
const SSH_PASS: &str = "12345678";
const REMOTE_DIR: &str = "/home/user/cmd-agent";
const AGENT_PORT: u16 = 4040;
/// Number of attempts to initialize the business-side client after the daemon
/// thread starts (the daemon needs a moment to bind its unix socket).
const CLIENT_INIT_ATTEMPTS: usize = 10;
/// Delay between client init attempts.
const CLIENT_INIT_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

// Replaces the NAPI launch entry that used to live in crates/zed/src/lib.rs.
// Dependency direction is now openharmony-ability -> zed: this entry depends on
// zed and only passes it the information zed truly needs (the sandbox base path).
#[ability]
pub fn launch_app(app: openharmony_ability::OpenHarmonyApp) {
    // Hand the app to the platform layer immediately; OhosPlatform picks it up from
    // the global on construction, so gpui never sees the OpenHarmonyApp type.
    openharmony_ability::set_global_app(app.clone());
    // Register the OHOS platform factory before Zed constructs any platform.
    gpui_ohos::register_platform();
    // Start the cmd-agent daemon as a background thread and bring up the
    // business-side cmd-agent client, so remote command execution is ready
    // before Zed starts issuing git/LSP commands.
    start_cmd_agent(&app);
    // Launch Zed with only the information it truly needs: the sandbox base path.
    zed::start_zed_main(app.base_path());
}

/// Starts the cmd-agent daemon (background thread) and initializes the
/// business-side client so `util::command` can execute remote commands.
#[cfg(target_env = "ohos")]
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
        .map(|resource_dir| format!("{resource_dir}/cmd-agent-server"));
    log::info!("start_cmd_agent: server_binary={server_binary:?}");
    // Process-internal shared state between the daemon thread and the business
    // client: spawn confirmations, exit results, and the signal channel. The
    // daemon fills the tables from VM events; the client reads them directly,
    // so the client needs no management connection and no threads.
    let shared = cmd_agent_client::daemon::SharedControl::new();
    let args = cmd_agent_client::daemon::Args {
        unix_socket: std::path::PathBuf::from(&socket_path),
        vm_addr: VM_ADDR.to_string(),
        ssh: Some(cmd_agent_client::deploy::SshConfig {
            host: SSH_HOST.to_string(),
            port: SSH_PORT,
            user: SSH_USER.to_string(),
            password: SSH_PASS.to_string(),
            remote_dir: REMOTE_DIR.to_string(),
        }),
        server_binary: server_binary.map(std::path::PathBuf::from),
        agent_port: AGENT_PORT,
        shared: shared.clone(),
    };
    if let Err(err) = cmd_agent_client::daemon::spawn_daemon(args) {
        log::error!("start_cmd_agent: failed to spawn cmd-agent daemon thread: {err}");
        return;
    }
    // The daemon binds its unix socket shortly after starting; retry the
    // client init until the connection succeeds. The business-side client is
    // registered as the remote-command executor, which `util::command` uses
    // through the stable cmd-agent-linker interface.
    for attempt in 0..CLIENT_INIT_ATTEMPTS {
        match cmd_agent_client::client::Client::connect(&socket_path, None, shared.clone()) {
            Ok(client) => {
                if cmd_agent_linker::init_executor(client).is_err() {
                    log::warn!("cmd-agent executor already registered");
                }
                if let Err(err) = util::command::init(&socket_path, None) {
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
#[cfg(target_env = "ohos")]
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

#[cfg(not(target_env = "ohos"))]
fn start_cmd_agent(_app: &openharmony_ability::OpenHarmonyApp) {}
