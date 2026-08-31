//! Wire message types for the cmd-agent protocol.
//!
//! All messages are JSON objects, wrapped in a length-prefixed frame
//! (see [`crate::frame`]). The `type` field discriminates the message kind.
//!
//! Data path is raw bytes: after the handshake (Hello/Spawn/SpawnOk) a spawn
//! connection turns into a pure byte stream between the client and the child
//! process, so no output frames exist. Control messages (exit codes, signals,
//! errors) travel over the dedicated management connection.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::bytes;

/// Protocol version negotiated at handshake time.
pub const PROTOCOL_VERSION: u32 = 1;

/// Mapping between the OHOS-side path root and the VM-side path root.
///
/// The server translates values in `ExecSpec::path_args` and
/// `ExecSpec::cwd_path` from the OHOS root to the VM root using this map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootMap {
    pub ohos_root: String,
    pub vm_root: String,
}

/// How one of the child's standard descriptors is wired on the server side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FdMode {
    /// The descriptor is connected to the data connection(s): stdin/stdout to
    /// the main connection, stderr to the dedicated stderr connection.
    #[default]
    Piped,
    /// The descriptor is redirected to `/dev/null`.
    Null,
}

/// One execution request: spawn a binary with argv and stream its stdio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Program that originated the command (e.g. "git", "rust-analyzer").
    pub source_program: String,
    pub binary: String,
    /// Full argument vector in original order. Arguments whose index appears
    /// in `path_arg_indices` are paths and get OHOS-root -> VM-root mapping.
    pub args: Vec<String>,
    /// Indices into `args` that denote path arguments.
    #[serde(default)]
    pub path_arg_indices: Vec<usize>,
    /// Working directory, also a path subject to root mapping.
    pub cwd_path: Option<String>,
    pub env: HashMap<String, String>,
    /// Optional stdin payload written by the client right after SpawnOk. The
    /// server never forwards it; the client writes it onto the data
    /// connection, which is the child's stdin once it is spawned.
    #[serde(with = "bytes", default)]
    pub stdin: Vec<u8>,
    /// Optional timeout in milliseconds; the child is killed when exceeded.
    pub timeout_ms: Option<u64>,
    /// Wiring of the child's three standard descriptors on the server side.
    #[serde(default)]
    pub stdin_mode: FdMode,
    #[serde(default)]
    pub stdout_mode: FdMode,
    #[serde(default)]
    pub stderr_mode: FdMode,
}

impl ExecSpec {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            source_program: String::new(),
            binary: binary.into(),
            args: Vec::new(),
            path_arg_indices: Vec::new(),
            cwd_path: None,
            env: HashMap::new(),
            stdin: Vec::new(),
            timeout_ms: None,
            stdin_mode: FdMode::Piped,
            stdout_mode: FdMode::Piped,
            stderr_mode: FdMode::Piped,
        }
    }
}

/// Signals that can be delivered to a running child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    SigInterrupt,
    SigTerm,
    SigKill,
}

/// Messages sent from the client to the server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        version: u32,
        root_map: Option<RootMap>,
    },
    /// Marks this connection as the management connection: it carries
    /// heartbeats and is the liveness marker for the client. Control messages
    /// (exit results, signals) also travel over it.
    Manage,
    /// Keeps the management connection alive.
    Heartbeat,
    /// Spawn a child. After `SpawnOk` the connection becomes a raw byte
    /// stream between the client and the child's stdio.
    Spawn {
        session_id: u64,
        spec: ExecSpec,
    },
    /// Marks this connection as the dedicated stderr channel of `session_id`.
    /// Must arrive before `Spawn` for the same session; the server then hands
    /// this connection's socket to the child as its stderr (fd 2).
    SpawnStderr {
        session_id: u64,
    },
    /// Delivers a signal to a running child, addressed by session id.
    Signal {
        session_id: u64,
        signal: Signal,
    },
    /// Begin a file-sync session on a dedicated connection. File sync mirrors
    /// device-sandbox downloads onto the VM: control commands travel as frames,
    /// file content as a raw byte stream (see `FileBegin`). No per-op
    /// confirmation is sent; TCP reliability and connection-close errors are
    /// the failure channel.
    FileSyncStart {
        sync_id: u64,
    },
    /// Declare the start of `path`'s content stream: the next `len` raw bytes
    /// (NOT framed, NOT base64) are appended to `path.ing` on the server.
    FileBegin {
        sync_id: u64,
        path: String,
        len: u64,
    },
    /// Atomically rename `path.ing` -> `path` on the server. Also serves as the
    /// content-stream terminator: it is sent right after the last byte of a
    /// `FileBegin` body, so the server knows the stream for `path` is complete.
    FileRename {
        sync_id: u64,
        path: String,
    },
    /// Delete `path` (file or directory, recursive) on the server. The server
    /// only honors deletes under the sync-mirrored directories (`is_sync_path`).
    FileDelete {
        sync_id: u64,
        path: String,
    },
    /// Create a directory (and parents) on the server.
    FileCreateDir {
        sync_id: u64,
        path: String,
    },
    /// End of the file-sync session; the connection closes after this.
    FileSyncEnd {
        sync_id: u64,
    },
    Query,
    Shutdown,
}

/// Messages sent from the server to the client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    HelloOk {
        server_version: String,
    },
    /// Spawn succeeded; the connection now carries raw bytes. The client
    /// writes `spec.stdin` next (if any), then the child's stdio streams.
    SpawnOk {
        session_id: u64,
    },
    /// Exit summary of a spawned child, delivered over the management
    /// connection. The data connection is raw bytes and cannot carry frames.
    ExecResult {
        session_id: u64,
        exit_code: Option<i32>,
        timed_out: bool,
    },
    Error {
        session_id: Option<u64>,
        message: String,
    },
}
