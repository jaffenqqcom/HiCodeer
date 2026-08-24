//! Mouse cursor capability plugin.
//!
//! The window id of the app's main window can only be obtained from ArkTS
//! (`window.getWindowProperties().id`); no public NDK API exposes it. This plugin
//! receives the window id once from the ArkTS `CursorPlugin` on the window-stage
//! main-thread event `window-id`, then every `CursorExt::set_cursor_style` call
//! is a plain OH_Input_SetPointerStyle system call from Rust.

use std::sync::RwLock;

use napi_derive_ohos::napi;
use napi_ohos::{Error, Result, bindgen_prelude::Unknown};
use openharmony_ability::{
    AsyncBridge, BridgeContextRequirement, BridgeMainThreadEvent, BridgePlugin, OpenHarmonyApp,
    PluginLifecycleEvent, impl_bridge_napi_type,
};

/// Name of the window-stage main-thread event that carries the window id.
const WINDOW_ID_EVENT: &str = "window-id";

/// Window id pushed by the ArkTS `CursorPlugin` once the window-stage is ready.
static WINDOW_ID: RwLock<Option<i32>> = RwLock::new(None);

/// OHOS pointer style result code for a successful call.
const INPUT_SUCCESS: i32 = 0;

/// libohinput.so (API 22+).
#[link(name = "ohinput")]
unsafe extern "C" {
    /// Sets the mouse pointer style for a window.
    fn OH_Input_SetPointerStyle(window_id: i32, pointer_style: i32) -> i32;
}

pub struct CursorBridgePlugin;

impl BridgePlugin for CursorBridgePlugin {
    type Mode = AsyncBridge;

    const ID: &'static str = "ohos.cursor";
    const REQUIRED_CONTEXTS: &'static [BridgeContextRequirement] =
        &[BridgeContextRequirement::WindowStage];

    fn on_main_thread_event<'env>(
        &self,
        event: BridgeMainThreadEvent<'env>,
    ) -> Result<Unknown<'env>> {
        match event.name() {
            WINDOW_ID_EVENT => {
                let window_id = event.decode::<i32>()?;
                *WINDOW_ID
                    .write()
                    .map_err(|_| Error::from_reason("Failed to write WINDOW_ID"))? = Some(window_id);
                log::info!("cursor: received main-window id {window_id}");
                event.respond(WindowIdResponse { accepted: true })
            }
            other => Err(Error::from_reason(format!(
                "Unsupported ohos.cursor main-thread event '{other}'"
            ))),
        }
    }

    fn on_lifecycle(&self, event: &PluginLifecycleEvent) -> Result<()> {
        if matches!(event, PluginLifecycleEvent::WindowStageDestroyed) {
            *WINDOW_ID
                .write()
                .map_err(|_| Error::from_reason("Failed to write WINDOW_ID"))? = None;
        }
        Ok(())
    }
}

/// Acknowledgement of the `window-id` event.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct WindowIdResponse {
    pub accepted: bool,
}

impl_bridge_napi_type!(WindowIdResponse, "ohos.cursor.WindowIdResponse");

/// Extension trait supplied by the capability package, never by `openharmony-ability` core.
pub trait CursorExt {
    /// Sets the system mouse cursor style for the app's main window.
    /// `style` is an OHOS `Input_PointerStyle` value. Returns whether the system call succeeded.
    fn set_cursor_style(&self, style: i32) -> bool;
}

impl CursorExt for OpenHarmonyApp {
    fn set_cursor_style(&self, style: i32) -> bool {
        let window_id = match *WINDOW_ID
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            Some(window_id) => window_id,
            None => {
                log::warn!("cursor::set_cursor_style: window id not available yet");
                return false;
            }
        };
        let result = unsafe { OH_Input_SetPointerStyle(window_id, style) };
        if result != INPUT_SUCCESS {
            log::error!("cursor::set_cursor_style: OH_Input_SetPointerStyle({window_id}) -> {result}");
        }
        result == INPUT_SUCCESS
    }
}
