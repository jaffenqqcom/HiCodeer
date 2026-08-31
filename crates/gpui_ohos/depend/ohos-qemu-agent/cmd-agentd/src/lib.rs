//! Guest-side path mapping for the QEMU cmd-agent channel.
//!
//! The guest-side `cmd-agentd` binary (see main.rs) owns the path mapping
//! table; the wire protocol and frame codec live in the shared
//! `qemu-cmd-agent-protocol` crate.

pub mod path_map;
