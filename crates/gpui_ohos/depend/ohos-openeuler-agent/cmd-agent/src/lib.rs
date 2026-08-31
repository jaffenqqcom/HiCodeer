//! cmd-agent daemon and deployment tooling.
//!
//! The daemon is a local proxy between business code (zcoder) and the remote
//! cmd-agent server. It listens on a unix socket: the business process opens
//! one management connection (heartbeats, exit results, signals) and one data
//! connection per spawn. Data connections are pure byte relays, so business
//! code talks to the child process's stdio directly through the relay.
//!
//! Northbound protocol (business <-> daemon) is the same frame protocol as
//! the daemon <-> server link: connect, `Hello`, then `Manage` (management)
//! or `Spawn` (data). A spawn session yields a raw byte stream plus an exit
//! code delivered over the management connection.

pub mod client;
pub mod daemon;
pub mod deploy;
pub mod error;
pub mod hilog_logger;
// The device-sandbox -> VM mirror sync engine. Internal to the daemon: spawned
// by `daemon::spawn_daemon`, never exposed outside this crate.
pub(crate) mod sync_engine;
