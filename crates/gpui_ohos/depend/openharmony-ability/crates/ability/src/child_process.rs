//! Native child-process capability (libchild_process.so).
//!
//! Wraps `OH_Ability_StartNativeChildProcess` / `OH_Ability_KillChildProcess` /
//! `OH_Ability_RegisterNativeChildProcessExitCallback` so application code can
//! start a same-package `.so` entry as a controlled child process — the way
//! the cmd-agent daemon is launched.

use std::ffi::{CString, c_char};

use napi_ohos::{Error, Result};

/// NCP_ISOLATION_MODE_NORMAL: the child shares the parent's sandbox/network.
const NCP_ISOLATION_MODE_NORMAL: i32 = 0;
/// NCP_NO_ERROR from `Ability_NativeChildProcess_ErrCode`.
const NCP_NO_ERROR: i32 = 0;

/// Matches `NativeChildProcess_Args` from `native_child_process.h`.
#[repr(C)]
struct NativeChildProcess_Fd {
    fd_name: *mut c_char,
    fd: i32,
    next: *mut NativeChildProcess_Fd,
}

#[repr(C)]
struct NativeChildProcess_FdList {
    head: *mut NativeChildProcess_Fd,
}

#[repr(C)]
struct NativeChildProcess_Args {
    entry_params: *mut c_char,
    fd_list: NativeChildProcess_FdList,
}

/// Matches `NativeChildProcess_Options` from `native_child_process.h`.
#[repr(C)]
struct NativeChildProcess_Options {
    isolation_mode: i32,
    reserved: i64,
}

#[link(name = "child_process")]
unsafe extern "C" {
    fn OH_Ability_StartNativeChildProcess(
        entry: *const c_char,
        args: NativeChildProcess_Args,
        options: NativeChildProcess_Options,
        pid: *mut i32,
    ) -> i32;
    fn OH_Ability_KillChildProcess(pid: i32) -> i32;
    fn OH_Ability_RegisterNativeChildProcessExitCallback(
        on_process_exit: unsafe extern "C" fn(pid: i32, signal: i32),
    ) -> i32;
}

/// Starts a native child process that loads `entry` (`"libxxx.so:Entry"`) and
/// returns its pid. `entry_params` is a space-separated argument string the
/// entry function receives.
pub fn spawn_native_child_process(entry: &str, entry_params: &str) -> Result<i32> {
    let entry_c = CString::new(entry)
        .map_err(|_| Error::from_reason("entry contains a NUL byte"))?;
    let params_c = CString::new(entry_params)
        .map_err(|_| Error::from_reason("entry_params contains a NUL byte"))?;
    let args = NativeChildProcess_Args {
        entry_params: params_c.as_ptr() as *mut c_char,
        fd_list: NativeChildProcess_FdList {
            head: std::ptr::null_mut(),
        },
    };
    let options = NativeChildProcess_Options {
        isolation_mode: NCP_ISOLATION_MODE_NORMAL,
        reserved: 0,
    };
    let mut pid: i32 = 0;
    let code = unsafe {
        OH_Ability_StartNativeChildProcess(entry_c.as_ptr(), args, options, &mut pid)
    };
    if code != NCP_NO_ERROR {
        return Err(Error::from_reason(format!(
            "OH_Ability_StartNativeChildProcess failed with code {code}"
        )));
    }
    log::info!("native child process started: entry={entry}, pid={pid}");
    Ok(pid)
}

/// Terminates a child process started by the current process.
pub fn kill_native_child_process(pid: i32) -> Result<()> {
    let code = unsafe { OH_Ability_KillChildProcess(pid) };
    if code != NCP_NO_ERROR {
        return Err(Error::from_reason(format!(
            "OH_Ability_KillChildProcess failed with code {code}"
        )));
    }
    log::info!("native child process {pid} killed");
    Ok(())
}

/// Registers a callback invoked when a native child process exits.
pub fn register_native_child_process_exit_callback(
    callback: unsafe extern "C" fn(pid: i32, signal: i32),
) -> Result<()> {
    let code = unsafe { OH_Ability_RegisterNativeChildProcessExitCallback(callback) };
    if code != NCP_NO_ERROR {
        return Err(Error::from_reason(format!(
            "OH_Ability_RegisterNativeChildProcessExitCallback failed with code {code}"
        )));
    }
    log::info!("native child process exit callback registered");
    Ok(())
}
