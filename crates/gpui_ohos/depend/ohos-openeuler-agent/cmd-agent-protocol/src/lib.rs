//! Wire protocol shared by cmd-agent server and client.

pub mod bytes;
pub mod error;
pub mod frame;
pub mod messages;

pub use error::{Error, Result, ResultContext};
pub use messages::{ClientMessage, ExecSpec, FdMode, PROTOCOL_VERSION, RootMap, ServerMessage, Signal};
