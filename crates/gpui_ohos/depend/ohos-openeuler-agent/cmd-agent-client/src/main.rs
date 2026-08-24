//! cmd-agent daemon entry point.
//!
//! Usage:
//!   cmd-agent-client [--unix-socket PATH] [--vm-addr HOST:PORT]
//!       [--ssh-host HOST] [--ssh-port PORT] [--ssh-user USER] [--ssh-pass PASS]
//!       [--remote-dir DIR] [--server-binary PATH] [--agent-port PORT]
//!
//! The daemon listens on a unix socket and proxies spawn sessions to the
//! cmd-agent server running on the VM. SSH credentials and a server binary
//! path enable automatic redeployment when the VM server is unreachable.
//! Argument parsing lives in `daemon::parse_args_from`, shared with the
//! native child-process entry (`CmdAgentDaemonMain`).

use cmd_agent_client::daemon::{self, Args};
use cmd_agent_client::error::{Error, Result};

fn main() -> Result<()> {
    cmd_agent_client::hilog_logger::init();
    log::info!("cmd-agent-client main starting");
    daemon::spawn_daemon(parse_args()?)?;
    // The daemon runs on its own executor worker threads; keep this process
    // alive (parked) until it is terminated externally.
    loop {
        std::thread::park();
    }
}

/// Parses command-line arguments into daemon configuration.
fn parse_args() -> Result<Args> {
    daemon::parse_args_from(&mut std::env::args().skip(1)).map_err(Error::message)
}
