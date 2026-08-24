//! Client error type.
//!
//! Follows the openharmony-ability convention: a crate-local error enum with
//! `Display` and `std::error::Error`, no `anyhow`.

use std::fmt;

/// Error type for the cmd-agent client.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Protocol(cmd_agent_protocol::Error),
    Ssh(russh::Error),
    /// An error wrapped with added context; `source` retains the original.
    Context {
        source: Box<Error>,
        message: String,
    },
    /// Generic error.
    Message(String),
}

impl Error {
    /// Builds a generic message error.
    pub fn message(message: impl Into<String>) -> Self {
        Error::Message(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "io error: {err}"),
            Error::Protocol(err) => write!(f, "protocol error: {err}"),
            Error::Ssh(err) => write!(f, "ssh error: {err}"),
            Error::Context { message, source } => write!(f, "{message}: {source}"),
            Error::Message(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Protocol(err) => Some(err),
            Error::Ssh(err) => Some(err),
            Error::Context { source, .. } => Some(source.as_ref()),
            Error::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<cmd_agent_protocol::Error> for Error {
    fn from(err: cmd_agent_protocol::Error) -> Self {
        Error::Protocol(err)
    }
}

impl From<russh::Error> for Error {
    fn from(err: russh::Error) -> Self {
        Error::Ssh(err)
    }
}

/// Result alias for the client crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Adds a context message to a fallible operation, mirroring anyhow's
/// `Context`, so callers keep a human-readable breadcrumb on failure.
///
/// Implemented for any `Result<T, E>` with a displayable error, so io,
/// serde, ssh, and other fallible calls can all be annotated.
pub trait ResultContext<T, E> {
    fn with_context(self, message: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E> ResultContext<T, E> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn with_context(self, message: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|err| {
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let source = match boxed.downcast::<std::io::Error>() {
                Ok(io_err) => Error::Io(*io_err),
                Err(boxed) => match boxed.downcast::<russh::Error>() {
                    Ok(ssh_err) => Error::Ssh(*ssh_err),
                    Err(boxed) => Error::Message(boxed.to_string()),
                },
            };
            Error::Context {
                source: Box::new(source),
                message: message(),
            }
        })
    }
}
