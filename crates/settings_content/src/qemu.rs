//! QEMU guest resource settings (CPU / RAM / disk tiers) shared between the
//! settings JSON schema and the launch-time reader.
//!
//! The module is cfg-gated from the crate root, so these types only exist on
//! HarmonyOS builds where an embedded QEMU guest can host the LSP servers and
//! the terminal. Each variant persists as a snake_case token (`cpu4`, `mem8`,
//! `disk128`), which is the spelling the launch-time reader parses.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

/// Number of vCPUs assigned to the QEMU guest (1..=10).
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum QemuCpuCores {
    /// Default core count. The guest is pure TCG emulation: the workloads it
    /// runs (git, language servers, shells) are largely serial, so extra vCPUs
    /// mostly buy synchronisation traffic rather than parallelism.
    #[default]
    Cpu1,
    Cpu2,
    Cpu3,
    Cpu4,
    Cpu5,
    Cpu6,
    Cpu7,
    Cpu8,
    Cpu9,
    Cpu10,
}

/// Guest RAM size tiers in gigabytes (4..=12).
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum QemuMemGb {
    /// Default memory size.
    #[default]
    Mem4,
    Mem6,
    Mem8,
    Mem10,
    Mem12,
}

/// Guest virtual disk size tiers in gigabytes.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum QemuDiskGb {
    Disk64,
    Disk96,
    /// Default disk size.
    #[default]
    Disk128,
    Disk256,
    Disk512,
}
