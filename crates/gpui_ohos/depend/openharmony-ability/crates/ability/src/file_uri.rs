//! OHOS file URI handling via NDK C APIs.
//!
//! - `OH_FileUri_GetPathFromUri` (`libohfileuri.so`, API 12+) maps a `file://docs/...` URI
//!   returned by `DocumentViewPicker` to the local sandbox path that `std::fs` can open.
//!   The returned string is allocated by the system and must be released with C `free`.
//! - `OH_FileShare_PersistPermission` / `OH_FileShare_ActivatePermission`
//!   (`libohfileshare.so`, API 12+) persist read/write authorization for the URI so opened
//!   files/directories stay accessible across app restarts.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::path::PathBuf;

/// `libohfileuri.so` (API 12+).
#[link(name = "ohfileuri")]
unsafe extern "C" {
    fn OH_FileUri_GetPathFromUri(uri: *const c_char, length: c_uint, result: *mut *mut c_char) -> c_int;
    fn OH_FileUri_GetUriFromPath(path: *const c_char, length: c_uint, result: *mut *mut c_char) -> c_int;
}

/// `libohfileshare.so` (API 12+).
#[link(name = "ohfileshare")]
unsafe extern "C" {
    fn OH_FileShare_PersistPermission(
        policies: *const PolicyInfo,
        policy_size: c_uint,
        error_result: *mut *mut PolicyErrorResult,
        result_num: *mut c_uint,
    ) -> c_int;
    fn OH_FileShare_ActivatePermission(
        policies: *const PolicyInfo,
        policy_size: c_uint,
        error_result: *mut *mut PolicyErrorResult,
        result_num: *mut c_uint,
    ) -> c_int;
    fn OH_FileShare_ReleasePolicyErrorResult(error_result: *mut PolicyErrorResult, result_num: c_uint);
}

/// `FileShare_OperationMode`: READ_MODE = 1, WRITE_MODE = 2. Combined read+write = 3.
const OPERATION_MODE_READ_WRITE: c_uint = 3;

/// Mirrors the NDK `FileShare_PolicyInfo` struct.
#[repr(C)]
struct PolicyInfo {
    uri: *mut c_char,
    length: c_uint,
    operation_mode: c_uint,
}

/// Mirrors the NDK `FileShare_PolicyErrorResult` struct (per-policy failure detail).
#[repr(C)]
struct PolicyErrorResult {
    uri: *mut c_char,
    code: c_int,
    message: *mut c_char,
}

/// C `free` for the system-allocated result buffer (`libc.so`, already linked by std).
#[link(name = "c")]
unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Map an OHOS file URI to the local sandbox path, persisting the URI authorization first
/// so the opened file or directory stays accessible across restarts. Returns `None` when
/// the conversion fails.
pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    persist_permission(uri);
    uri_to_local_path(uri)
}

/// Convert an OHOS URI to the local sandbox path. Pure conversion with no
/// authorization side effects.
fn uri_to_local_path(uri: &str) -> Option<PathBuf> {
    let uri_c = CString::new(uri).ok()?;
    let mut result: *mut c_char = std::ptr::null_mut();
    let err = unsafe {
        OH_FileUri_GetPathFromUri(uri_c.as_ptr(), uri.len() as c_uint, &mut result)
    };
    if err != 0 || result.is_null() {
        log::error!("file_uri::uri_to_local_path: OH_FileUri_GetPathFromUri failed err={err}");
        return None;
    }
    let path = unsafe {
        let path = CStr::from_ptr(result).to_string_lossy().into_owned();
        free(result as *mut c_void);
        path
    };
    Some(PathBuf::from(path))
}

/// Prefix of user-public-directory paths that live outside the app sandbox.
/// `std::fs` can only access them while the picker-returned URI authorization
/// is active.
const USER_PUBLIC_PATH_PREFIX: &str = "/storage/Users/currentUser";

/// Ensure the picker authorization for a worktree root is active so `std::fs`
/// can access it. The URI was persisted when the picker returned the path; this
/// re-activates it after a restart (e.g. recent-projects restore). Paths inside
/// the app sandbox, and any activation failure, return the input unchanged so
/// the call is idempotent and safe for every worktree root.
pub fn ensure_root_authorized(path: &str) -> PathBuf {
    if !path.starts_with(USER_PUBLIC_PATH_PREFIX) {
        return PathBuf::from(path);
    }
    let Some(uri) = path_to_uri(path) else {
        return PathBuf::from(path);
    };
    if !activate_permission(&uri) {
        return PathBuf::from(path);
    }
    uri_to_local_path(&uri).unwrap_or_else(|| PathBuf::from(path))
}

/// Convert a local path to its OHOS URI form via `OH_FileUri_GetUriFromPath`.
fn path_to_uri(path: &str) -> Option<String> {
    let c_path = CString::new(path).ok()?;
    let mut result: *mut c_char = std::ptr::null_mut();
    let err = unsafe {
        OH_FileUri_GetUriFromPath(c_path.as_ptr(), path.len() as c_uint, &mut result)
    };
    if err != 0 || result.is_null() {
        log::error!(
            "file_uri::path_to_uri: OH_FileUri_GetUriFromPath failed err={err} path={path}"
        );
        return None;
    }
    let uri = unsafe {
        let uri = CStr::from_ptr(result).to_string_lossy().into_owned();
        free(result as *mut c_void);
        uri
    };
    Some(uri)
}

/// Persist read-write authorization for a picker-returned URI so it survives restarts.
pub fn persist_permission(uri: &str) -> bool {
    call_permission_api(uri, false)
}

/// Activate a persisted URI authorization (required after an app/device restart).
pub fn activate_permission(uri: &str) -> bool {
    call_permission_api(uri, true)
}

fn call_permission_api(uri: &str, activate: bool) -> bool {
    let c_uri = match CString::new(uri) {
        Ok(value) => value,
        Err(error) => {
            log::error!("file_uri: invalid URI bytes: {error}");
            return false;
        }
    };
    let policy = PolicyInfo {
        uri: c_uri.as_ptr() as *mut c_char,
        length: uri.len() as c_uint,
        operation_mode: OPERATION_MODE_READ_WRITE,
    };
    let mut error_result: *mut PolicyErrorResult = std::ptr::null_mut();
    let mut result_num: c_uint = 0;
    let api_name = if activate {
        "ActivatePermission"
    } else {
        "PersistPermission"
    };
    let result = unsafe {
        if activate {
            OH_FileShare_ActivatePermission(&policy, 1, &mut error_result, &mut result_num)
        } else {
            OH_FileShare_PersistPermission(&policy, 1, &mut error_result, &mut result_num)
        }
    };
    if result != 0 {
        log::error!("file_uri::{api_name} failed: err={result} uri={uri}");
        return false;
    }
    if !error_result.is_null() && result_num > 0 {
        unsafe { OH_FileShare_ReleasePolicyErrorResult(error_result, result_num) };
    }
    true
}
