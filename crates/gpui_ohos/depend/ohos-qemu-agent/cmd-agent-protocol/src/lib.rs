//! Wire protocol for the QEMU cmd-agent channel.
//!
//! A self-contained copy of the cmd-agent wire protocol plus the QEMU-specific
//! extensions (ExecResultAck, MountFolder2QEMU / UnmountFolder2QEMU), owned by
//! this crate so the OpenEuler cmd-agent tree stays untouched. The host-side
//! cmd-agent and the guest-side cmd-agentd both speak it over the virtio-serial
//! port pool.

pub mod frame;
pub mod messages;
