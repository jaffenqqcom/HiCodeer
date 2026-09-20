#[cfg(not(target_env = "ohos"))]
#[path = "crashes_desktop.rs"]
mod desktop;

#[cfg(target_env = "ohos")]
mod ohos {
    use serde::{Deserialize, Serialize};
    use std::{
        panic::Location,
        path::{Path, PathBuf},
        pin::Pin,
        sync::Arc,
        time::Duration,
    };
    use system_specs::GpuSpecs;

    #[derive(Debug, Default)]
    pub struct Client;

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct CrashInfo {
        pub init: InitCrashHandler,
        pub panic: Option<CrashPanic>,
        pub minidump_error: Option<String>,
        pub abort_message: Option<String>,
        pub gpus: Vec<system_specs::GpuInfo>,
        pub active_gpu: Option<system_specs::GpuSpecs>,
        pub user_info: Option<UserInfo>,
    }

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct InitCrashHandler {
        pub session_id: String,
        pub zed_version: String,
        pub binary: String,
        pub release_channel: String,
        pub commit_sha: String,
    }

    #[derive(Deserialize, Serialize, Debug, Clone)]
    pub struct CrashPanic {
        pub message: String,
        pub span: String,
    }

    #[derive(Deserialize, Serialize, Debug, Clone)]
    pub struct UserInfo {
        pub metrics_id: Option<String>,
        pub is_staff: Option<bool>,
    }

    /// Install the panic hook used on OHOS.
    ///
    /// The desktop variant only raises `RUST_BACKTRACE`, leaving the default
    /// hook to print to stderr, which is readable there. Neither stdout nor
    /// stderr is visible from an OHOS app, so the message has to be routed
    /// through `log::error!` instead, which zlog forwards to hilog.
    pub fn force_backtrace() {
        std::panic::set_hook(Box::new(|payload| {
            panic_hook(
                Arc::new(Client),
                payload.payload_as_str().unwrap_or("<non-string panic payload>"),
                payload.location(),
            )
        }));
    }

    pub fn init<F, S, C, P>(
        crash_init: InitCrashHandler,
        spawn: S,
        socket_path: P,
        wait_timer: C,
    ) -> impl Future<Output = Arc<Client>> + use<F, C, S, P>
    where
        F: Future<Output = ()> + Send + Sync + 'static,
        C: (Fn(Duration) -> F) + Send + Sync + 'static,
        S: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
        P: FnOnce(u32) -> PathBuf,
    {
        let _ = (crash_init, spawn, socket_path, wait_timer);
        async { Arc::new(Client) }
    }

    pub fn set_gpu_info(_crash_client: &Arc<Client>, _specs: GpuSpecs) {}

    pub fn set_user_info(_crash_client: &Arc<Client>, _info: UserInfo) {}

    pub fn panic_hook(_crash_client: Arc<Client>, message: &str, location: Option<&Location>) {
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("<unnamed>");
        let location = location.map_or_else(|| "<unknown>".to_owned(), |location| location.to_string());
        log::error!("thread '{thread_name}' panicked at {location}:\n{message}");
        std::process::abort();
    }

    pub fn crash_server(_socket: &Path, _logs_dir: PathBuf) {
        log::info!("Crash handler is not available on OHOS");
    }
}

#[cfg(not(target_env = "ohos"))]
pub use desktop::*;

#[cfg(target_env = "ohos")]
pub use ohos::*;
