fn main() {
    // NAPI module registration must be set up by the final cdylib that exports
    // the module init symbol (see zed/src/lib.rs history; previously done there).
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "ohos" {
        napi_build_ohos::setup();

        // Every path-taking libc call inside the final cdylib is redirected to
        // `ohos-libc-shim`, which re-activates the OHOS picker grant when a call
        // fails and then retries once. The redirection has to happen at link
        // time: a preload would only reach the processes this crate spawns, not
        // the app process itself. The list below is the set of path-taking libc
        // symbols `libhicodeer.so` actually references.
        for symbol in [
            "open",
            "openat",
            "access",
            "stat",
            "lstat",
            "fstatat",
            "opendir",
            "realpath",
            "syscall",
        ] {
            println!("cargo:rustc-link-arg=-Wl,--wrap={symbol}");
        }
    }
}
