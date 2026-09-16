//! Link-time shim that re-activates the OHOS picker grant without the callers
//! knowing about it.
//!
//! OHOS grants the app read/write access to the directory tree the user picked
//! in `DocumentViewPicker` and remembers that grant across restarts, but every
//! new app start has to re-activate it before the paths below it are reachable
//! again. Doing that from application code means every file-using crate needs
//! an OHOS-specific call - the thing this shim removes.
//!
//! Instead it intercepts the path-taking libc entry points. Each wrapper runs
//! the real call first and only when that call fails does it derive the URI
//! from the path and activate the grant, retrying once when that activation
//! succeeded. Callers observe either the original result or a retried one, so
//! no OHOS-specific code is left on their side.
//!
//! Only a genuine authorization failure is worth that round trip: a path under
//! the sandbox is always reachable, and a missing file stays missing however
//! often its grant is activated. Each wrapper therefore checks both the `errno`
//! the real call set and the path prefix before acting.
//!
//! Wired up by `launch-zed`'s build script through `-Wl,--wrap=<symbol>`. That
//! is a link-time binding: every undefined reference to `<symbol>` inside the
//! final `libhicodeer.so` is redirected to `__wrap_<symbol>` here, while the
//! real implementation stays reachable as `__real_<symbol>`. Unlike
//! `LD_PRELOAD` this does not depend on the runtime symbol lookup order, and it
//! covers the app process itself - a preload only reaches the processes we
//! spawn ourselves.
//!
//! `syscall` is wrapped as well as the individual entry points. rustix is the
//! layer `crates/fs` reads directories through, and on OHOS it would normally
//! use its linux_raw backend, which issues syscalls through inline assembly and
//! never touches a libc symbol. The `rustix_use_libc` cfg in
//! `.cargo/config.toml` moves it to the libc backend, and one of its open paths
//! calls `syscall(SYS_openat, ...)` directly, which is why that entry point is
//! covered too.
//!
//! Activation applies to a whole directory tree, so once it succeeds every
//! later access below that directory goes straight through the fast path and
//! never reaches the activation code again. No cache of activated paths is
//! needed for that - the success of the real call is the cache.
//!
//! Nothing here logs. The shim is meant to be invisible to the code it
//! forwards, so it adds no output of its own on any path.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr, CString};
use std::sync::Mutex;

/// `libohfileuri.so` (API 12+): maps a local path to its OHOS URI form.
#[link(name = "ohfileuri")]
unsafe extern "C" {
    fn OH_FileUri_GetUriFromPath(
        path: *const c_char,
        length: c_uint,
        result: *mut *mut c_char,
    ) -> c_int;
}

/// `libohfileshare.so` (API 12+): activates a previously persisted grant.
#[link(name = "ohfileshare")]
unsafe extern "C" {
    fn OH_FileShare_ActivatePermission(
        policies: *const PolicyInfo,
        policy_size: c_uint,
        error_result: *mut *mut PolicyErrorResult,
        result_num: *mut c_uint,
    ) -> c_int;
    fn OH_FileShare_ReleasePolicyErrorResult(
        error_result: *mut PolicyErrorResult,
        result_num: c_uint,
    );
}

/// `FileShare_OperationMode`: READ_MODE = 1, WRITE_MODE = 2, so read+write = 3.
/// Mirrors `file_uri` so the grant is activated with the mode it was persisted
/// with.
const OPERATION_MODE_READ_WRITE: c_uint = 3;

/// Number of policies handed to the activation call: exactly the one URI
/// derived from the failing path.
const POLICY_COUNT: c_uint = 1;

/// Prefix of user-public-directory paths that live outside the app sandbox.
/// Only these paths can carry a picker grant, so every other path skips the
/// activation work entirely.
const USER_PUBLIC_PATH_PREFIX: &str = "/storage/Users/currentUser";

/// Mirrors the NDK `FileShare_PolicyInfo` struct.
#[repr(C)]
struct PolicyInfo {
    uri: *mut c_char,
    length: c_uint,
    operation_mode: c_uint,
}

/// Mirrors the NDK `FileShare_PolicyErrorResult` struct.
#[repr(C)]
struct PolicyErrorResult {
    uri: *mut c_char,
    code: c_int,
    message: *mut c_char,
}

/// Serialises activation attempts across threads.
///
/// Every failing call wants the same grant activated, and one activation is a
/// round trip through the system file-share service. Deciding to "skip while
/// busy" would drop the retry of every caller that failed while another thread
/// was activating - exactly the concurrent burst a directory scan produces, and
/// the reason the worktree root could not be rescued before. Waiting on the
/// mutex lets each caller retry once the activation in flight has completed.
static ACTIVATION_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Marks that this thread is already inside an activation.
    ///
    /// The activation call opens files itself. A failure inside it would
    /// otherwise re-enter the wrapper and try to take `ACTIVATION_LOCK` again,
    /// which a non-reentrant mutex cannot satisfy.
    static ACTIVATING_ON_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Decides whether a failed call is worth an activation attempt.
///
/// Called immediately after the real call failed, so the `errno` it reads is
/// still that call's. Anything but a permission denial would come back
/// unchanged after activating the grant, and is skipped.
///
/// # Safety
/// `path` must be a valid NUL-terminated string, or null.
unsafe fn should_activate(path: *const c_char) -> bool {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    if errno != libc::EPERM && errno != libc::EACCES {
        return false;
    }
    // SAFETY: forwarded straight from the caller's contract.
    unsafe { is_user_public_path(path) }
}

/// Whether `path` lies under the user-public directory the picker can grant.
///
/// # Safety
/// `path` must be a valid NUL-terminated string, or null.
unsafe fn is_user_public_path(path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    // SAFETY: null was handled above and the caller guarantees termination.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    bytes.starts_with(USER_PUBLIC_PATH_PREFIX.as_bytes())
}

/// Derives the URI of the failing path and activates the grant that covers it.
/// Called only after a real call failed with a permission error on a path the
/// picker can grant, so the fast path stays free of any of this work.
///
/// Returns whether the grant is now active. A `false` means the retry would
/// only repeat the original error, which is why the wrappers skip it.
///
/// # Safety
/// `path` must be a valid NUL-terminated string.
unsafe fn activate_grant_for(path: *const c_char) -> bool {
    if ACTIVATING_ON_THIS_THREAD.with(Cell::get) {
        return false;
    }
    let Ok(_lock) = ACTIVATION_LOCK.lock() else {
        // Poisoned only if an earlier activation panicked. Running the same
        // code again is not worth risking a second panic.
        return false;
    };
    ACTIVATING_ON_THIS_THREAD.with(|flag| flag.set(true));
    // SAFETY: the caller guarantees `path` is NUL-terminated.
    let uri = unsafe { path_to_uri(path) };
    ACTIVATING_ON_THIS_THREAD.with(|flag| flag.set(false));
    match uri {
        Some(uri) => activate_uri(&uri),
        None => false,
    }
}

/// `OH_FileUri_GetUriFromPath` wrapper. Returns `None` when the system cannot
/// derive a URI, which is the case for paths outside any user-visible location.
///
/// # Safety
/// `path` must be a valid NUL-terminated string.
unsafe fn path_to_uri(path: *const c_char) -> Option<String> {
    // SAFETY: `path` is NUL-terminated per this function's contract.
    let length = unsafe { libc::strlen(path) } as c_uint;
    if length == 0 {
        return None;
    }
    let mut result: *mut c_char = std::ptr::null_mut();
    // SAFETY: `path` is valid for `length` bytes, and `result` is a live local
    // the call writes the system-allocated string into.
    let err = unsafe { OH_FileUri_GetUriFromPath(path, length, &mut result) };
    if err != 0 || result.is_null() {
        return None;
    }
    // SAFETY: a zero return guarantees `result` holds a NUL-terminated string
    // owned by us, released with the matching `free` below.
    let uri = unsafe { CStr::from_ptr(result).to_string_lossy().into_owned() };
    // SAFETY: `result` came from the system allocator that `free` serves.
    unsafe { libc::free(result as *mut c_void) };
    Some(uri)
}

/// Activates the persisted grant for `uri`, reporting whether the system call
/// succeeded.
fn activate_uri(uri: &str) -> bool {
    let Ok(uri_c) = CString::new(uri) else {
        // A NUL byte means the returned string is not a usable URI.
        return false;
    };
    let policy = PolicyInfo {
        uri: uri_c.as_ptr() as *mut c_char,
        length: uri.len() as c_uint,
        operation_mode: OPERATION_MODE_READ_WRITE,
    };
    let mut error_result: *mut PolicyErrorResult = std::ptr::null_mut();
    let mut result_num: c_uint = 0;
    // SAFETY: `policy` outlives the call and its `uri` points into `uri_c`,
    // which also outlives it; both out-parameters are live locals.
    let result = unsafe {
        OH_FileShare_ActivatePermission(&policy, POLICY_COUNT, &mut error_result, &mut result_num)
    };
    if !error_result.is_null() && result_num > 0 {
        // SAFETY: the system returns an array of `result_num` entries, released
        // exactly once here.
        unsafe { OH_FileShare_ReleasePolicyErrorResult(error_result, result_num) };
    }
    result == 0
}

/// Real implementations, bound by the linker through `build.rs`'s
/// `-Wl,--wrap=<symbol>` arguments. Without the wrapping these symbols would be
/// plain libc calls; with it they are the only way to reach libc from here.
unsafe extern "C" {
    fn __real_open(path: *const c_char, flags: c_int, mode: c_int) -> c_int;
    fn __real_openat(fd: c_int, path: *const c_char, flags: c_int, mode: c_int) -> c_int;
    fn __real_access(path: *const c_char, mode: c_int) -> c_int;
    fn __real_stat(path: *const c_char, buf: *mut c_void) -> c_int;
    fn __real_lstat(path: *const c_char, buf: *mut c_void) -> c_int;
    fn __real_fstatat(fd: c_int, path: *const c_char, buf: *mut c_void, flags: c_int) -> c_int;
    fn __real_opendir(path: *const c_char) -> *mut c_void;
    fn __real_realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char;
    fn __real_syscall(
        number: c_long,
        arg1: c_long,
        arg2: c_long,
        arg3: c_long,
        arg4: c_long,
        arg5: c_long,
        arg6: c_long,
    ) -> c_long;
}

/// `open` reports failure as a negative descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    // SAFETY: every argument is forwarded unchanged to the real libc entry
    // point, and `path` follows the contract the caller already gave it.
    unsafe {
        let result = __real_open(path, flags, mode);
        if result >= 0 || !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_open(path, flags, mode)
    }
}

/// `openat` reports failure the same way. A relative `path` is unusable here -
/// it only means something together with `fd` - so only `AT_FDCWD` (the
/// absolute case) gets the activation retry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_openat(
    fd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: c_int,
) -> c_int {
    // SAFETY: see `__wrap_open`.
    unsafe {
        let result = __real_openat(fd, path, flags, mode);
        if result >= 0
            || fd != libc::AT_FDCWD
            || !should_activate(path)
            || !activate_grant_for(path)
        {
            return result;
        }
        __real_openat(fd, path, flags, mode)
    }
}

/// `access` reports failure as a non-zero return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_access(path: *const c_char, mode: c_int) -> c_int {
    // SAFETY: see `__wrap_open`.
    unsafe {
        let result = __real_access(path, mode);
        if result == 0 || !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_access(path, mode)
    }
}

/// `stat` reports failure as a non-zero return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_stat(path: *const c_char, buf: *mut c_void) -> c_int {
    // SAFETY: see `__wrap_open`.
    unsafe {
        let result = __real_stat(path, buf);
        if result == 0 || !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_stat(path, buf)
    }
}

/// `lstat` reports failure the same way as `stat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_lstat(path: *const c_char, buf: *mut c_void) -> c_int {
    // SAFETY: see `__wrap_open`.
    unsafe {
        let result = __real_lstat(path, buf);
        if result == 0 || !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_lstat(path, buf)
    }
}

/// `fstatat` needs the same `AT_FDCWD` restriction as `openat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_fstatat(
    fd: c_int,
    path: *const c_char,
    buf: *mut c_void,
    flags: c_int,
) -> c_int {
    // SAFETY: see `__wrap_open`.
    unsafe {
        let result = __real_fstatat(fd, path, buf, flags);
        if result == 0
            || fd != libc::AT_FDCWD
            || !should_activate(path)
            || !activate_grant_for(path)
        {
            return result;
        }
        __real_fstatat(fd, path, buf, flags)
    }
}

/// `opendir` reports failure as a null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_opendir(path: *const c_char) -> *mut c_void {
    // SAFETY: see `__wrap_open`.
    unsafe {
        let result = __real_opendir(path);
        if !result.is_null() || !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_opendir(path)
    }
}

/// `realpath` reports failure as a null pointer. `std::fs::canonicalize` goes
/// through it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_realpath(
    path: *const c_char,
    resolved: *mut c_char,
) -> *mut c_char {
    // SAFETY: see `__wrap_open`; both pointers are forwarded unchanged.
    unsafe {
        let result = __real_realpath(path, resolved);
        if !result.is_null() || !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_realpath(path, resolved)
    }
}

/// The syscall entry point, wrapped so calls that bypass the dedicated libc
/// wrappers are covered as well: rustix's libc backend issues `SYS_openat` this
/// way from `openat_via_syscall`. Only path-taking syscalls are inspected;
/// everything else (futex, read, write, ...) pays one forwarded call and
/// returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_syscall(
    number: c_long,
    arg1: c_long,
    arg2: c_long,
    arg3: c_long,
    arg4: c_long,
    arg5: c_long,
    arg6: c_long,
) -> c_long {
    // SAFETY: all arguments are forwarded unchanged to the real syscall entry
    // point. Syscalls taking fewer arguments ignore the extra registers.
    unsafe {
        let result = __real_syscall(number, arg1, arg2, arg3, arg4, arg5, arg6);
        if result >= 0 {
            return result;
        }
        // `syscall_path_argument` only reads its arguments and leaves `errno`.
        let Some(path) = syscall_path_argument(number, arg1, arg2) else {
            return result;
        };
        if !should_activate(path) || !activate_grant_for(path) {
            return result;
        }
        __real_syscall(number, arg1, arg2, arg3, arg4, arg5, arg6)
    }
}

/// Returns the path argument of a path-taking syscall, or `None` for any other
/// syscall. Relative paths are skipped: they only mean something together with
/// their directory descriptor, so no URI can be derived from them here.
fn syscall_path_argument(number: c_long, arg1: c_long, arg2: c_long) -> Option<*const c_char> {
    let path_is_second_argument = matches!(
        number,
        libc::SYS_openat
            | libc::SYS_openat2
            | libc::SYS_newfstatat
            | libc::SYS_faccessat
            | libc::SYS_faccessat2
            | libc::SYS_statx
            | libc::SYS_readlinkat
    );
    if path_is_second_argument {
        if arg1 != libc::AT_FDCWD as c_long {
            return None;
        }
        return Some(arg2 as *const c_char);
    }
    if number == libc::SYS_statfs {
        return Some(arg1 as *const c_char);
    }
    None
}
