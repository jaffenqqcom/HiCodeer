//! Protocol error type.
//!
//! Matches the convention of the openharmony-ability crates: a crate-local
//! error enum with `Display` and `std::error::Error`, no `anyhow`.

use std::fmt;

/// Error type for the cmd-agent protocol.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// Frame-level protocol violation (oversized frame, malformed header).
    Protocol(String),
    /// An error wrapped with added context; `source` retains the original
    /// error so its type and `io::ErrorKind` stay inspectable.
    Context {
        source: Box<Error>,
        message: String,
    },
    /// Generic error.
    Message(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "io error: {err}"),
            Error::Json(err) => write!(f, "json error: {err}"),
            Error::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Error::Context { message, source } => write!(f, "{message}: {source}"),
            Error::Message(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Json(err) => Some(err),
            Error::Context { source, .. } => Some(source.as_ref()),
            Error::Protocol(_) | Error::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(err)
    }
}

/// Result alias for the protocol crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Adds a context message to a fallible operation, mirroring anyhow's
/// `Context`, so callers keep a human-readable breadcrumb on failure.
///
/// The original error is preserved as `Error::Context::source` (downcast to
/// `Error::Io`/`Error::Json` when possible), so error kinds remain
/// inspectable (e.g. `is_peer_closed` can still see an `UnexpectedEof`).
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
                Err(boxed) => match boxed.downcast::<serde_json::Error>() {
                    Ok(json_err) => Error::Json(*json_err),
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
