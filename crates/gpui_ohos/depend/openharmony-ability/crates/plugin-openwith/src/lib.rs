//! Open-with (file association) bridge plugin.
//!
//! When the system opens a file with the host application — via double-click or the "open with"
//! sheet — the ArkTS `OpenWithPlugin` forwards the file URIs through the
//! synchronous main-thread bridge. `gpui_ohos` resolves them to paths and delivers
//! them to Zed through gpui's `on_open_urls` callback, which routes into Zed's
//! open-listener and opens the files through the normal workspace path.
//!
//! `on_main_thread_event` always runs on the Ability main thread, so the callback
//! is stored in a `thread_local` (single-threaded) slot instead of a `Send + Sync`
//! static; that lets gpui_ohos capture its platform state in the closure.

use std::cell::RefCell;

use napi_derive_ohos::napi;
use napi_ohos::{Error, Result, bindgen_prelude::Unknown};
use openharmony_ability::{
    AsyncBridge, BridgeContextRequirement, BridgeMainThreadEvent, BridgePlugin,
    impl_bridge_napi_type,
};

/// Main-thread event name pushed by the ArkTS `OpenWithPlugin`.
const OPEN_WITH_EVENT: &str = "open-with";

/// File URIs the system asked the host application to open.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct OpenWithData {
    pub uris: Vec<String>,
}

impl_bridge_napi_type!(OpenWithData, "ohos.openwith.OpenWithData");

/// Acknowledgement of an open-with event.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct OpenWithAck {
    pub accepted: bool,
}

impl_bridge_napi_type!(OpenWithAck, "ohos.openwith.OpenWithAck");

/// Single-threaded callback slot, filled by gpui_ohos on the main thread and
/// invoked from `on_main_thread_event` (also the main thread).
thread_local! {
    static OPEN_WITH_CALLBACK: RefCell<Option<Box<dyn Fn(Vec<String>)>>> = RefCell::new(None);
}

/// Registers the open-with handler. Replaces any previously registered handler.
pub fn set_openwith_callback(callback: Box<dyn Fn(Vec<String>)>) {
    log::info!(
        "open-with: set_openwith_callback registered on thread {:?}",
        std::thread::current().id()
    );
    OPEN_WITH_CALLBACK.with(|cell| *cell.borrow_mut() = Some(callback));
}

/// Clears the open-with handler. Called when the platform is torn down.
pub fn clear_openwith_callback() {
    OPEN_WITH_CALLBACK.with(|cell| *cell.borrow_mut() = None);
}

pub struct OpenWithBridgePlugin;

impl BridgePlugin for OpenWithBridgePlugin {
    type Mode = AsyncBridge;

    const ID: &'static str = "ohos.openwith";
    const REQUIRED_CONTEXTS: &'static [BridgeContextRequirement] =
        &[BridgeContextRequirement::UiContext];

    fn on_main_thread_event<'env>(
        &self,
        event: BridgeMainThreadEvent<'env>,
    ) -> Result<Unknown<'env>> {
        log::info!(
            "open-with: on_main_thread_event '{}' on thread {:?}",
            event.name(),
            std::thread::current().id()
        );
        if event.name() != OPEN_WITH_EVENT {
            return Err(Error::from_reason(format!(
                "Unsupported ohos.openwith main-thread event '{}'",
                event.name()
            )));
        }
        let data = event.decode::<OpenWithData>()?;
        log::info!("open-with: decoded {} uri(s)", data.uris.len());
        let mut callback_invoked = false;
        OPEN_WITH_CALLBACK.with(|cell| {
            if let Some(callback) = cell.borrow().as_ref() {
                callback_invoked = true;
                callback(data.uris.clone());
            }
        });
        log::info!("open-with: callback_invoked = {callback_invoked}");
        event.respond(OpenWithAck { accepted: true })
    }
}
