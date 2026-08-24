fn main() {
    // NAPI module registration must be set up by the final cdylib that exports
    // the module init symbol (see zed/src/lib.rs history; previously done there).
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "ohos" {
        napi_build_ohos::setup();
    }
}
