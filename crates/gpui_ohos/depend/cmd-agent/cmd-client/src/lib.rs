//! cmd-client: SSH client that talks to the on-device zcoderd command server.
//!
//! zcoder (whose app sandbox forbids exec'ing external programs) links this
//! crate: `util::command`'s OHOS path routes remote commands through a
//! [`SshCommandExecutor`] registered here, which connects to zcoderd over
//! loopback SSH (management bootstrap on 4023, command pool on 4022). This
//! crate is self-contained: it depends on no other gpui_ohos crate and carries
//! its own command-execution contract ([`types`]).

pub mod bootstrap;
pub mod command;
pub mod executor;
pub mod pool;
pub mod protocol;
pub mod types;

use std::sync::{Arc, OnceLock};

pub use executor::SshCommandExecutor;
pub use types::{ExecSpec, FdMode, RemoteChild, RemoteCommandExecutor, Signal};

static EXECUTOR: OnceLock<Arc<dyn RemoteCommandExecutor>> = OnceLock::new();

/// Registers the process-wide remote command executor. Called once at startup
/// by launch-zed after the executor is constructed.
pub fn init_executor(executor: Arc<dyn RemoteCommandExecutor>) -> Result<(), ()> {
    EXECUTOR.set(executor).map_err(|_| ())
}

/// Returns the registered executor, or `None` if not yet initialized.
pub fn executor() -> Option<Arc<dyn RemoteCommandExecutor>> {
    EXECUTOR.get().cloned()
}

/// True once a command executor is registered and ready for use.
pub fn is_ready() -> bool {
    EXECUTOR.get().is_some()
}
