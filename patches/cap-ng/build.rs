use std::env;

const LIBCAPNG_LIB_NAME: &str = "cap-ng";
const LIBCAPNG_LIB_PATH: &str = "LIBCAPNG_LIB_PATH";
const LIBCAPNG_LINK_TYPE: &str = "LIBCAPNG_LINK_TYPE";

fn main() {
    // On OHOS libcap-ng does not exist and the capability API is meaningless
    // (sandboxed, non-root); the bindings are stubbed out in src/bindings.rs.
    // The build script still emits `-lcap-ng`, so satisfy it with an empty
    // static archive -- the stubs provide every symbol, so nothing is pulled
    // from the archive at runtime.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("ohos") {
        let out = env::var("OUT_DIR").expect("OUT_DIR");
        let lib = std::path::Path::new(&out).join("libcap-ng.a");
        std::fs::write(&lib, b"!<arch>\n").expect("write empty libcap-ng.a");
        println!("cargo:rustc-link-search=native={}", out);
        println!("cargo:rustc-link-lib=static={}", LIBCAPNG_LIB_NAME);
        return;
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={}", LIBCAPNG_LIB_PATH);
    println!("cargo:rerun-if-env-changed={}", LIBCAPNG_LINK_TYPE);

    if let Ok(path) = env::var(LIBCAPNG_LIB_PATH) {
        println!("cargo:rustc-link-search=native={}", path);
    }

    let link_type = match env::var(LIBCAPNG_LINK_TYPE) {
            Ok(val) if matches!(val.as_str(), "dylib" | "static") => val,
            _ => String::from("dylib"),
    };

    println!("cargo:rustc-link-lib={}={}", link_type, LIBCAPNG_LIB_NAME);
}
