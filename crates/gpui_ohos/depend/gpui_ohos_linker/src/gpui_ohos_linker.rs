//! Abstraction boundary between the Zed mainline and the OHOS platform
//! implementation.
//!
//! The mainline (`gpui_platform`) depends only on this crate; the OHOS
//! implementation (`gpui_ohos`) registers its platform factory here at startup.
//! Decoupling this way keeps a change in the OHOS implementation from
//! recompiling the whole Zed dependency graph.

#![cfg(target_env = "ohos")]

use std::rc::Rc;

use gpui::Platform;

/// Factory that constructs the OHOS platform. Implemented and registered by the
/// implementation side (`gpui_ohos`).
pub trait OhosPlatformFactory: Send + Sync {
    fn current_platform(&self, headless: bool) -> Rc<dyn Platform>;
}

static FACTORY: std::sync::OnceLock<Box<dyn OhosPlatformFactory>> =
    std::sync::OnceLock::new();

/// Register the OHOS platform factory. Must be called exactly once, before any
/// `current_platform` call (launch-zed does this before starting Zed).
pub fn register(factory: Box<dyn OhosPlatformFactory>) {
    FACTORY
        .set(factory)
        .unwrap_or_else(|_| panic!("gpui_ohos_linker::register called more than once"));
    log::info!("gpui_ohos_linker::register: OHOS platform factory registered");
}

/// Construct the OHOS platform implementation through the registered factory.
pub fn current_platform(headless: bool) -> Rc<dyn Platform> {
    log::info!("gpui_ohos_linker::current_platform: headless={headless}");
    FACTORY
        .get()
        .unwrap_or_else(|| {
            panic!("gpui_ohos_linker::register must be called before current_platform")
        })
        .current_platform(headless)
}

/// The VM's CPU architecture as `uname -m` reports it (e.g. "aarch64"). The
/// VM is where OHOS commands actually run, so downloads must be built for the
/// VM's architecture rather than the device's.
static VM_ARCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Records the VM's CPU architecture once the cmd-agent connection is up
/// (launch-zed queries `uname -m` on the VM and calls this). At most once.
pub fn set_vm_arch(arch: String) {
    if VM_ARCH.set(arch).is_err() {
        log::warn!("gpui_ohos_linker::set_vm_arch called more than once");
    }
}

/// The VM's CPU architecture, or `None` before the connection captures it.
pub fn vm_arch() -> Option<&'static str> {
    VM_ARCH.get().map(|arch| arch.as_str())
}
