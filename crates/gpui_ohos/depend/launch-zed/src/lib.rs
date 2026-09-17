// Keeps `ohos-libc-shim` linked into the final cdylib: the `__wrap_*` symbols
// in it are where build.rs redirects the path-taking libc calls, and no Rust
// code calls into the crate directly.
#[cfg(target_env = "ohos")]
use ohos_libc_shim as _;

mod launch_app;

// QEMU guest backend: engine boot and dynamic work-directory mounts. Compiled
// only when the `qemu-agent` feature is on; the on-device command service needs
// none of it and lives in launch_app.
#[cfg(all(target_env = "ohos", feature = "qemu-agent"))]
mod qemu_runtime;

// OHOS libc lacks robust mutexes (pthread_mutexattr_setrobust /
// pthread_mutex_consistent), which std depends on. Export no-op shims so the
// final cdylib (libhicodeer.so) satisfies those undefined symbols. Moved here
// from crates/zed/src/lib.rs when the NAPI entry relocated to this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_setrobust(
    _attr: *mut std::ffi::c_void,
    _robustness: std::ffi::c_int,
) -> std::ffi::c_int {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_consistent(
    _mutex: *mut std::ffi::c_void,
) -> std::ffi::c_int {
    0
}
