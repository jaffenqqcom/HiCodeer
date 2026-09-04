//! IME (soft keyboard / input method) bridge plugin facade.
//!
//! ArkTS owns the OpenHarmony `InputMethodController` (`@kit.IMEKit`). IME input
//! (insertText, deleteLeft/Right, functionKey, previewText, keyboard status) is
//! pushed from ArkTS callbacks to Rust through the synchronous main-thread bridge
//! (`invokeNativeSync` -> `BridgePlugin::on_main_thread_event`), where it is turned
//! into the same `Event::Input(InputEvent::ImeEvent(..))` stream the previous NDK
//! path produced, so the GPUI consumer (`OhosWindow::handle_input_event`) is
//! unchanged.
//!
//! Control calls (attach a session, detach it, move the candidate box to follow
//! the cursor) travel the other way, through the async bridge (`ImeClient` ->
//! ArkTS `invokeAsync`). This is the only supported direction for starting/stopping
//! the system IME, because `attachWithUIContext` must run on the ArkTS main thread.

use napi_derive_ohos::napi;
use napi_ohos::{bindgen_prelude::Unknown, Error, Result};
use openharmony_ability::{
    impl_bridge_napi_type, AsyncBridge, BridgeCallOptions, BridgeContextRequirement, BridgePlugin,
    BridgeRuntime, ImeEvent, InputEvent, OpenHarmonyApp,
};
use openharmony_ability::ime::KeyboardStatus;
use openharmony_ability::TextInputEventData;

/// Main-thread event names pushed from ArkTS IME callbacks.
const INSERT_TEXT_EVENT: &str = "insert-text";
const DELETE_LEFT_EVENT: &str = "delete-left";
const DELETE_RIGHT_EVENT: &str = "delete-right";
const FUNCTION_KEY_EVENT: &str = "function-key";
const KEYBOARD_STATUS_EVENT: &str = "keyboard-status";
const PREVIEW_TEXT_EVENT: &str = "preview-text";

/// Async actions invoked from Rust through `ImeClient`.
const ATTACH_ACTION: &str = "attach";
const DETACH_ACTION: &str = "detach";
const UPDATE_CURSOR_ACTION: &str = "update-cursor";

pub struct ImeBridgePlugin;

impl BridgePlugin for ImeBridgePlugin {
    type Mode = AsyncBridge;

    const ID: &'static str = "ohos.ime";
    const REQUIRED_CONTEXTS: &'static [BridgeContextRequirement] =
        &[BridgeContextRequirement::UiContext];

    fn on_main_thread_event<'env>(
        &self,
        event: openharmony_ability::BridgeMainThreadEvent<'env>,
    ) -> Result<Unknown<'env>> {
        match event.name() {
            INSERT_TEXT_EVENT => {
                let request = event.decode::<ImeInsertText>()?;
                Self::push_input(InputEvent::ImeEvent(ImeEvent::TextInputEvent(
                    TextInputEventData { text: request.text },
                )));
                event.respond(ImeAck { accepted: true })
            }
            DELETE_LEFT_EVENT => {
                let request = event.decode::<ImeDeleteLeft>()?;
                // Negative length is rejected by the GPUI consumer as a no-op; clamp
                // to a plain backspace so a malformed ArkTS value cannot panic.
                let length = request.length.max(0);
                Self::push_input(InputEvent::ImeEvent(ImeEvent::BackspaceEvent(length)));
                event.respond(ImeAck { accepted: true })
            }
            DELETE_RIGHT_EVENT => {
                let request = event.decode::<ImeDeleteRight>()?;
                // Forward delete-forward like deleteLeft so the window can deliver
                // a real Delete key press when no composition text is active (e.g.
                // the terminal has no forward-delete IME layer to act on).
                let length = request.length.max(0);
                Self::push_input(InputEvent::ImeEvent(ImeEvent::DeleteRightEvent(length)));
                event.respond(ImeAck { accepted: true })
            }
            FUNCTION_KEY_EVENT => {
                let request = event.decode::<ImeFunctionKey>()?;
                Self::push_input(InputEvent::ImeEvent(ImeEvent::EnterEvent(request.key)));
                event.respond(ImeAck { accepted: true })
            }
            KEYBOARD_STATUS_EVENT => {
                let request = event.decode::<ImeKeyboardStatus>()?;
                Self::push_input(InputEvent::ImeEvent(ImeEvent::ImeStatusEvent(
                    KeyboardStatus::from(request.status as u32),
                )));
                event.respond(ImeAck { accepted: true })
            }
            PREVIEW_TEXT_EVENT => {
                let request = event.decode::<ImePreviewText>()?;
                // Preview (composition) text is advisory; the editor drives its own
                // marked-text path, so we acknowledge without a separate event.
                let _ = request;
                event.respond(ImeAck { accepted: true })
            }
            other => Err(Error::from_reason(format!(
                "Unsupported ohos.ime main-thread event '{other}'"
            ))),
        }
    }
}

impl ImeBridgePlugin {
    /// Forwards an ability-level input event to the registered event-loop handler.
    ///
    /// Runs on the ArkTS/N-API main thread (the IME callbacks arrive there), so the
    /// process-wide `OpenHarmonyApp` is available through `global_app()`.
    fn push_input(event: InputEvent) {
        match openharmony_ability::global_app() {
            Some(app) => app.dispatch_input_event(event),
            None => log::warn!("ime: global OpenHarmonyApp unavailable; dropping IME input"),
        }
    }

}

// ── Main-thread input event payloads (ArkTS -> Rust) ──────────────────────────

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeInsertText {
    pub text: String,
}

impl_bridge_napi_type!(ImeInsertText, "ohos.ime.InsertText");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeDeleteLeft {
    pub length: i32,
}

impl_bridge_napi_type!(ImeDeleteLeft, "ohos.ime.DeleteLeft");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeDeleteRight {
    pub length: i32,
}

impl_bridge_napi_type!(ImeDeleteRight, "ohos.ime.DeleteRight");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeFunctionKey {
    pub key: i32,
}

impl_bridge_napi_type!(ImeFunctionKey, "ohos.ime.FunctionKey");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeKeyboardStatus {
    pub status: i32,
}

impl_bridge_napi_type!(ImeKeyboardStatus, "ohos.ime.KeyboardStatus");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImePreviewText {
    pub text: String,
    pub start: i32,
    pub end: i32,
}

impl_bridge_napi_type!(ImePreviewText, "ohos.ime.PreviewText");

/// Acknowledgement returned for every main-thread IME event.
///
/// For `attach`, `accepted` reports whether the session is really bound, not
/// merely that the call was received — ArkTS resolves it only after its internal
/// retry loop finishes, so the caller can tell a rejected binding from a working
/// one and retry on the next focus or caret event.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeAck {
    pub accepted: bool,
}

impl_bridge_napi_type!(ImeAck, "ohos.ime.Ack");

// ── Async control request payloads (Rust -> ArkTS) ────────────────────────────

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeAttachRequest {}

impl_bridge_napi_type!(ImeAttachRequest, "ohos.ime.AttachRequest");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeDetachRequest {}

impl_bridge_napi_type!(ImeDetachRequest, "ohos.ime.DetachRequest");

#[napi(object)]
#[derive(Clone, Debug)]
pub struct ImeCursorRequest {
    /// Cursor rectangle in physical pixels (logical px already multiplied by the
    /// device scale factor on the Rust side), relative to the XComponent surface.
    /// `f64` because napi-ohos does not implement `FromNapiValue` for `f32`.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl_bridge_napi_type!(ImeCursorRequest, "ohos.ime.CursorRequest");

/// Worker-safe facade for IME session control.
///
/// Mirrors `WindowClient`: every method is a thin async wrapper over the ArkTS
/// `ImePlugin.invokeAsync` implementation, which owns the actual
/// `InputMethodController` on the main thread.
#[derive(Clone)]
pub struct ImeClient {
    bridge: BridgeRuntime,
}

impl ImeClient {
    fn new(app: &OpenHarmonyApp) -> Result<Self> {
        Ok(Self {
            bridge: app.bridge()?,
        })
    }

    /// Attaches the system IME to the focused editor and shows the soft keyboard.
    ///
    /// The ArkTS side builds the `TextConfig` from OpenHarmony `TextInputType` /
    /// `EnterKeyType` constants, so no per-call type needs to be passed here.
    ///
    /// Idempotent and safe to re-issue: a session that is already bound is a
    /// no-op, and overlapping calls share a single attempt. Returns
    /// `ImeAck { accepted: false }` when the edit box never took focus, so the
    /// caller can retry later instead of assuming the keyboard is live.
    pub async fn attach(&self) -> Result<ImeAck> {
        let ack = self
            .bridge
            .call_async::<ImeBridgePlugin, ImeAttachRequest, ImeAck>(
                ATTACH_ACTION,
                ImeAttachRequest {},
                BridgeCallOptions::default(),
            )
            .await?;
        Ok(ack)
    }

    /// Detaches the system IME session (hides the soft keyboard).
    pub async fn detach(&self) -> Result<()> {
        self.bridge
            .call_async::<ImeBridgePlugin, ImeDetachRequest, ImeAck>(
                DETACH_ACTION,
                ImeDetachRequest {},
                BridgeCallOptions::default(),
            )
            .await?;
        Ok(())
    }

    /// Moves the IME candidate box to follow the editor cursor.
    ///
    /// `x`/`y`/`width`/`height` are physical pixels relative to the XComponent
    /// surface; the ArkTS side converts them to absolute screen coordinates.
    pub async fn update_cursor(&self, x: f64, y: f64, width: f64, height: f64) -> Result<()> {
        self.bridge
            .call_async::<ImeBridgePlugin, ImeCursorRequest, ImeAck>(
                UPDATE_CURSOR_ACTION,
                ImeCursorRequest {
                    x,
                    y,
                    width,
                    height,
                },
                BridgeCallOptions::default(),
            )
            .await?;
        Ok(())
    }
}

/// Extension trait supplied by the capability package, never by `openharmony-ability` core.
pub trait ImeExt {
    /// Returns a facade for IME session control bound to this app.
    fn ime(&self) -> Result<ImeClient>;
}

impl ImeExt for OpenHarmonyApp {
    fn ime(&self) -> Result<ImeClient> {
        ImeClient::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{ImeBridgePlugin, ImeExt};
    use openharmony_ability::{
        BridgeContextRequirement, BridgeNapiType, BridgePlugin, OpenHarmonyApp,
    };

    #[test]
    fn ime_plugin_targets_the_ui_context() {
        assert_eq!(ImeBridgePlugin::ID, "ohos.ime");
        assert_eq!(
            ImeBridgePlugin::REQUIRED_CONTEXTS,
            &[BridgeContextRequirement::UiContext]
        );
    }

    #[test]
    fn ime_event_contracts_use_stable_type_names() {
        assert_eq!(<super::ImeInsertText as BridgeNapiType>::TYPE_NAME, "ohos.ime.InsertText");
        assert_eq!(<super::ImeDeleteLeft as BridgeNapiType>::TYPE_NAME, "ohos.ime.DeleteLeft");
        assert_eq!(<super::ImeFunctionKey as BridgeNapiType>::TYPE_NAME, "ohos.ime.FunctionKey");
        assert_eq!(
            <super::ImeKeyboardStatus as BridgeNapiType>::TYPE_NAME,
            "ohos.ime.KeyboardStatus"
        );
        assert_eq!(<super::ImeAck as BridgeNapiType>::TYPE_NAME, "ohos.ime.Ack");
        assert_eq!(
            <super::ImeAttachRequest as BridgeNapiType>::TYPE_NAME,
            "ohos.ime.AttachRequest"
        );
        assert_eq!(
            <super::ImeCursorRequest as BridgeNapiType>::TYPE_NAME,
            "ohos.ime.CursorRequest"
        );
    }

    #[test]
    fn ime_ext_is_implemented_for_openharmony_app() {
        // Compile-time check: `ImeExt` must be implemented for `OpenHarmonyApp`.
        fn assert_bound<T: ImeExt>() {}
        assert_bound::<OpenHarmonyApp>();
    }
}
