//! Wire messages for the QEMU cmd-agentd protocol.
//!
//! A self-contained copy of the cmd-agent wire protocol plus the QEMU-specific
//! extensions (ExecResultAck, MountFolder2QEMU / UnmountFolder2QEMU). The
//! guest-side agent owns this definition so the OpenEuler cmd-agent tree stays
//! untouched.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Protocol version negotiated at handshake time.
pub const PROTOCOL_VERSION: u32 = 1;

/// One zcoder-side path root mapped to a guest mount point.
///
/// The mapping table is keyed by `host_root` (a zcoder-side URI or path); the
/// table is maintained exclusively by MountFolder2QEMU / UnmountFolder2QEMU.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootMap {
    pub host_root: String,
    pub guest_root: String,
}

/// How one of the child's standard descriptors is wired on the server side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FdMode {
    /// The descriptor is connected to the data connection: stdin/stdout to
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
    /// Full argument vector in original order.
    pub args: Vec<String>,
    /// Working directory, also subject to root mapping.
    pub cwd_path: Option<String>,
    pub env: HashMap<String, String>,
    /// Optional stdin payload written by the client right after SpawnOk.
    #[serde(default)]
    pub stdin: Vec<u8>,
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
            cwd_path: None,
            env: HashMap::new(),
            stdin: Vec::new(),
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

/// Messages sent from the cmd-agent (zcoder side) to cmd-agentd (guest side).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        version: u32,
    },
    /// Marks this connection as the management connection: it carries
    /// heartbeats and is the liveness marker for the client.
    Manage,
    /// Keeps the management connection alive and carries the sender's active
    /// session ids, so both sides can reconcile which sessions truly exist and
    /// clean up the other side's leftovers.
    Heartbeat {
        sessions: Vec<u64>,
    },
    /// Spawn a child. After `SpawnOk` the connection becomes a raw byte
    /// stream between the client and the child's stdio.
    Spawn {
        session_id: u64,
        spec: ExecSpec,
    },
    /// Marks this connection as the dedicated stderr channel of `session_id`.
    SpawnStderr {
        session_id: u64,
    },
    /// Delivers a signal to a running child, addressed by session id.
    Signal {
        session_id: u64,
        signal: Signal,
    },
    /// Tells cmd-agentd the client has closed the child's stdin. Carried over
    /// the management connection: stdin bytes travel over the data connection
    /// (a raw byte stream after SpawnOk), and half-closing that shared socket
    /// would poison the reusable port, so stdin EOF must be signalled out-of-band.
    StdinEof {
        session_id: u64,
    },
    /// Acknowledge receipt of `ServerMessage::ExecResult`; cmd-agentd
    /// reclaims the session's ports only after this (or a 2s timeout).
    ExecResultAck {
        session_id: u64,
    },
    /// Mount a zcoder-opened folder into the guest. cmd-agentd runs
    /// `mount -t 9p <mount_tag> <guest_path>` and records uri -> guest_path.
    MountFolder2QEMU {
        uri: String,
        mount_tag: String,
        guest_path: String,
    },
    /// Unmount a previously mounted folder. Kept for completeness; zcoder
    /// does not actively call it while an LSP may still be scanning.
    UnmountFolder2QEMU {
        uri: String,
    },
    Query,
    Shutdown,
}

/// Messages sent from cmd-agentd (guest side) to the cmd-agent.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    HelloOk {
        server_version: String,
    },
    /// Spawn succeeded; the connection now carries raw bytes.
    SpawnOk {
        session_id: u64,
    },
    /// Folder mounted and uri -> guest_path registered.
    MountOk {
        uri: String,
    },
    /// Exit summary of a spawned child, delivered over the management
    /// connection.
    ExecResult {
        session_id: u64,
        exit_code: Option<i32>,
    },
    /// Acknowledgement of `ClientMessage::Heartbeat`, proving the guest agent
    /// is alive, and carrying the guest's active session ids for the same
    /// session reconciliation.
    HeartbeatOk {
        sessions: Vec<u64>,
    },
    Error {
        session_id: Option<u64>,
        message: String,
    },
}
