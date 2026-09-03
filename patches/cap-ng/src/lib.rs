// Copyright (C) 2020 Red Hat, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::convert::TryFrom;
use std::error;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::File;
use std::os::raw::c_char;
use std::os::unix::io::AsRawFd;

#[macro_use]
extern crate bitflags;

mod bindings;

pub type Pid = i32;
pub type Capability = u32;

#[derive(Debug)]
pub enum Error {
    /// Failed to sync capabilities with the kernel.
    ApplyCapabilities,
    /// Failed to write capabilities to the extended attributes of File.
    ApplyCapsFile(File),
    /// Failed to change the target process ID.
    ChangeTargetId,
    /// Failed to convert capability name to a C-compatible representation.
    ConvertCapabilityName,
    /// Failed to get the ID for a capability's name.
    GetCapabilityId(String),
    /// Failed to read the capabilities from the extended attributes of File.
    GetCapsFile(File),
    /// Failed to get process capabilities.
    GetProcessCapabilities,
    /// Invalid value for HaveCapsResult enum.
    InvalidHaveCapsResult(i32),
    /// Failed to lock capabilities.
    LockCapabilities,
    /// Failed to find the name corresponding to Capability.
    NameToCapability(Capability),
    /// Failed to update the capability's status.
    UpdateCapability(Capability),
}

impl error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use Error::*;
        match self {
            ApplyCapabilities => write!(f, "failed to sync capabilities with the kernel"),
            ApplyCapsFile(file) => write!(
                f,
                "failed to write capabilities to the extended attributes of {:?}",
                file
            ),
            ChangeTargetId => write!(f, "failed to change target process id"),
            ConvertCapabilityName => write!(
                f,
                "failed to convert capability name to a C-compatible representation"
            ),
            GetCapabilityId(cap_name) => write!(
                f,
                "failed to get the ID for the capability with name {}",
                cap_name
            ),
            GetCapsFile(file) => write!(
                f,
                "failed to read the capabilities from the extended attributes of {:?}",
                file
            ),
            GetProcessCapabilities => write!(f, "failed to get process capabilities"),
            InvalidHaveCapsResult(value) => write!(f, "invalid value {} for HaveCapsResult", value),
            LockCapabilities => write!(f, "failed to lock capabilities"),
            NameToCapability(cap) => write!(f, "failed to find the name for capability {}", cap),
            UpdateCapability(cap) => write!(
                f,
                "failed to update the status of the capability with name {}",
                cap
            ),
        }
    }
}

pub type Result<T> = ::std::result::Result<T, Error>;

bitflags! {
    pub struct Type: u32 {
        const EFFECTIVE = 1;
        const PERMITTED = 2;
        const INHERITABLE = 4;
        const BOUNDING_SET = 8;
    }
}

bitflags! {
    pub struct Set: u32 {
        const CAPS = 16;
        const BOUNDS = 32;
        const BOTH = Self::CAPS.bits() | Self::BOUNDS.bits();
    }
}

bitflags! {
    pub struct Flags: u32 {
        const DROP_SUPP_GRP = 1;
        const CLEAR_BOUNDING = 2;
        const INIT_SUPP_GRP = 4;
    }
}

#[derive(Copy, Clone)]
pub enum Action {
    DROP = 0,
    ADD = 1,
}

#[derive(Copy, Clone)]
pub enum Print {
    STDOUT = 0,
    BUFFER = 1,
}

#[derive(PartialEq, Debug)]
pub enum HaveCapsResult {
    FAIL = -1,
    NONE = 0,
    PARTIAL = 1,
    FULL = 2,
}

impl TryFrom<i32> for HaveCapsResult {
    type Error = crate::Error;

    fn try_from(n: i32) -> Result<Self> {
        match n {
            -1 => Ok(HaveCapsResult::FAIL),
            0 => Ok(HaveCapsResult::NONE),
            1 => Ok(HaveCapsResult::PARTIAL),
            2 => Ok(HaveCapsResult::FULL),
            _ => Err(Error::InvalidHaveCapsResult(n)),
        }
    }
}

pub struct CUpdate {
    pub action: Action,
    pub cap_type: Type,
    pub capability: Capability,
}

pub struct CapngState {
    opaque: *mut ::std::os::raw::c_void,
}

unsafe impl Send for CapngState {}

pub fn clear(set: Set) {
    // Safe because it doesn't modify any local memory.
    unsafe {
        bindings::capng_clear(set.bits() as u32);
    }
}

pub fn fill(set: Set) {
    // Safe because it doesn't modify any local memory.
    unsafe {
        bindings::capng_fill(set.bits() as u32);
    }
}

pub fn setpid(pid: Pid) {
    // Safe because it doesn't modify any local memory.
    unsafe {
        bindings::capng_setpid(pid);
    }
}

pub fn get_caps_process() -> Result<()> {
    // Safe because it doesn't modify any local memory.
    let ret = unsafe { bindings::capng_get_caps_process() };

    if ret == 0 {
        Ok(())
    } else {
        Err(Error::GetProcessCapabilities)
    }
}

pub fn update(updates: Vec<CUpdate>) -> Result<()> {
    for u in updates {
        // Safe because this doesn't modify any local memory.
        let ret = unsafe {
            bindings::capng_update(u.action as u32, u.cap_type.bits() as u32, u.capability)
        };
        if ret < 0 {
            return Err(Error::UpdateCapability(u.capability));
        }
    }

    Ok(())
}

pub fn updatev(action: Action, _type: Type, names: Vec<&str>) -> Result<()> {
    for name in names {
        let cap = name_to_capability(name)?;
        // Safe because this doesn't modify any local memory.
        let ret = unsafe { bindings::capng_update(action as u32, _type.bits() as u32, cap) };
        if ret < 0 {
            return Err(Error::UpdateCapability(cap));
        }
    }

    Ok(())
}

pub fn apply(set: Set) -> Result<()> {
    // Safe because this doesn't modify any local memory.
    let ret = unsafe { bindings::capng_apply(set.bits() as u32) };

    if ret == 0 {
        Ok(())
    } else {
        Err(Error::ApplyCapabilities)
    }
}

pub fn lock() -> Result<()> {
    // Safe because this doesn't modify any local memory.
    let ret = unsafe { bindings::capng_lock() };

    if ret == 0 {
        Ok(())
    } else {
        Err(Error::LockCapabilities)
    }
}

pub fn change_id(uid: i32, gid: i32, flags: Flags) -> Result<()> {
    // Safe because this doesn't modify any local memory.
    let ret = unsafe { bindings::capng_change_id(uid, gid, flags.bits() as u32) };

    if ret == 0 {
        Ok(())
    } else {
        Err(Error::ChangeTargetId)
    }
}

pub fn get_caps_file(file: File) -> Result<()> {
    // Safe because this doesn't modify any local memory and doesn't alter
    // the offset of the file descriptor.
    let ret = unsafe { bindings::capng_get_caps_fd(file.as_raw_fd()) };

    if ret == 0 {
        Ok(())
    } else {
        Err(Error::GetCapsFile(file))
    }
}

pub fn apply_caps_fd(file: File) -> Result<()> {
    // Safe because this doesn't modify any local memory and doesn't alter
    // the offset of the file descriptor.
    let ret = unsafe { bindings::capng_apply_caps_fd(file.as_raw_fd()) };

    if ret == 0 {
        Ok(())
    } else {
        Err(Error::ApplyCapsFile(file))
    }
}

pub fn have_capabilities(set: Set) -> Result<HaveCapsResult> {
    // Safe because this doesn't modify any local memory.
    let ret = unsafe { bindings::capng_have_capabilities(set.bits() as u32) };

    HaveCapsResult::try_from(ret)
}

pub fn have_permitted_capabilities() -> Result<HaveCapsResult> {
    // Safe because this doesn't modify any local memory.
    let ret = unsafe { bindings::capng_have_permitted_capabilities() };

    HaveCapsResult::try_from(ret)
}

pub fn have_capability(which: Type, capability: Capability) -> bool {
    // Safe because this doesn't modify any local memory.
    let ret = unsafe { bindings::capng_have_capability(which.bits() as u32, capability) };

    ret == 1
}

pub fn print_caps_numeric(print: Print, set: Set) -> Option<String> {
    // Safe because it doesn't modify any local memory, we check the buffer and
    // move its contents to local memory.
    let buffer = unsafe { bindings::capng_print_caps_numeric(print as u32, set.bits() as u32) };
    match print {
        Print::STDOUT => None,
        Print::BUFFER => {
            if buffer.is_null() {
                None
            } else {
                // Safe as long capng_print_caps_numeric behaves as expected.
                let caps = unsafe { CStr::from_ptr(buffer).to_string_lossy().into_owned() };
                unsafe { libc::free(buffer as *mut ::core::ffi::c_void) };
                Some(caps)
            }
        }
    }
}

pub fn print_caps_text(print: Print, which: Type) -> Option<String> {
    // Safe because it doesn't modify any local memory, we check the buffer and
    // move its contents to local memory.
    let buffer = unsafe { bindings::capng_print_caps_text(print as u32, which.bits() as u32) };
    match print {
        Print::STDOUT => None,
        Print::BUFFER => {
            if buffer.is_null() {
                None
            } else {
                // Safe as long capng_print_caps_text behaves as expected.
                let caps = unsafe { CStr::from_ptr(buffer).to_string_lossy().into_owned() };
                unsafe { libc::free(buffer as *mut ::core::ffi::c_void) };
                Some(caps)
            }
        }
    }
}

pub fn name_to_capability(name: &str) -> Result<Capability> {
    let cstr = CString::new(name).map_err(|_| Error::ConvertCapabilityName)?;

    // Safe because this doesn't modify any local memory and we have converted
    // name to a CString.
    let cap_id: i32 = unsafe { bindings::capng_name_to_capability(cstr.as_ptr() as *const c_char) };
    if cap_id < 0 {
        return Err(Error::GetCapabilityId(name.to_string()));
    }

    Ok(cap_id as Capability)
}

pub fn capability_to_name(capability: Capability) -> Result<String> {
    // Safe because this doesn't modify any local memory.
    let name_ptr = unsafe { bindings::capng_capability_to_name(capability) };
    if name_ptr.is_null() {
        return Err(Error::NameToCapability(capability));
    }
    // Only safe if capng_capability_to_name behaves properly.
    let name = unsafe { CStr::from_ptr(name_ptr).to_string_lossy().into_owned() };

    Ok(name)
}

pub fn save_state() -> Option<CapngState> {
    // Safe because this doesn't modify any local memory and we check opaque.
    let opaque = unsafe { bindings::capng_save_state() };

    if opaque.is_null() {
        None
    } else {
        Some(CapngState { opaque })
    }
}

pub fn restore_state(state: CapngState) {
    // Safe because this only consumes state.opaque, and we're consuming
    // CapngState here.
    unsafe {
        let mut opaque: *mut ::std::os::raw::c_void = state.opaque;
        let opaque_ptr: *mut *mut ::std::os::raw::c_void = &mut opaque;
        bindings::capng_restore_state(opaque_ptr)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_tests() {
        clear(Set::BOTH);
        assert_eq!(have_capabilities(Set::BOTH).unwrap(), HaveCapsResult::NONE);

        fill(Set::BOTH);
        assert_eq!(have_capabilities(Set::BOTH).unwrap(), HaveCapsResult::FULL);
    }

    #[test]
    fn print_tests() {
        fill(Set::BOTH);
        assert!(print_caps_numeric(Print::BUFFER, Set::CAPS).is_some());
        assert!(print_caps_text(Print::BUFFER, Type::EFFECTIVE).is_some());
    }

    #[test]
    fn update_tests() {
        for i in 0..5 {
            clear(Set::BOTH);
            update(vec![CUpdate {
                action: Action::ADD,
                cap_type: Type::EFFECTIVE,
                capability: i,
            }])
            .unwrap();
            assert!(have_capability(Type::EFFECTIVE, i));
            assert_eq!(
                have_capabilities(Set::CAPS).unwrap(),
                HaveCapsResult::PARTIAL
            );

            update(vec![CUpdate {
                action: Action::ADD,
                cap_type: Type::BOUNDING_SET,
                capability: i,
            }])
            .unwrap();
            assert!(have_capability(Type::BOUNDING_SET, i));
            assert_eq!(
                have_capabilities(Set::BOUNDS).unwrap(),
                HaveCapsResult::PARTIAL
            );

            let text = print_caps_text(Print::BUFFER, Type::EFFECTIVE).unwrap();
            let name = capability_to_name(i).unwrap();
            assert_eq!(text, name);

            fill(Set::BOTH);
            update(vec![CUpdate {
                action: Action::DROP,
                cap_type: Type::EFFECTIVE,
                capability: i,
            }])
            .unwrap();
            assert_eq!(
                have_capabilities(Set::CAPS).unwrap(),
                HaveCapsResult::PARTIAL
            );
            update(vec![CUpdate {
                action: Action::ADD,
                cap_type: Type::EFFECTIVE,
                capability: i,
            }])
            .unwrap();
            assert_eq!(have_capabilities(Set::CAPS).unwrap(), HaveCapsResult::FULL);
        }
    }

    #[test]
    fn update_multiple_test() {
        let caps_text = vec!["CHOWN", "FOWNER", "KILL"];
        let mut caps: Vec<Capability> = Vec::new();
        for c in caps_text {
            caps.push(name_to_capability(c).unwrap());
        }

        let mut cap_updates: Vec<CUpdate> = Vec::new();
        for c in &caps {
            cap_updates.push(CUpdate {
                action: Action::ADD,
                cap_type: Type::EFFECTIVE,
                capability: *c,
            });
        }

        clear(Set::BOTH);
        update(cap_updates).unwrap();

        for c in caps {
            assert!(have_capability(Type::EFFECTIVE, c));
        }
    }
}
