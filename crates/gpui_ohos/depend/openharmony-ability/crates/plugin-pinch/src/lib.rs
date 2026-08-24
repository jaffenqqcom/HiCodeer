//! Pinch gesture capability plugin.
//!
//! The ArkTS `PinchPlugin` attaches a transparent pinch-gesture overlay to the
//! XComponent UI tree and pushes `pinch-begin` / `pinch-update` / `pinch-end`
//! main-thread events through `invokeNativeSync`. Each event carries the current
//! cumulative scale (1.0 == gesture start); this facade computes the incremental
//! delta against the previous sample and invokes the registered callback
//! immediately (event-driven — no polling). gpui_ohos registers a callback that
//! turns each sample into a GPUI `PinchEvent`.
//!
//! `on_main_thread_event` always runs on the Ability main thread, so the callback
//! is stored in a `thread_local` (single-threaded) slot instead of a `Send + Sync`
//! static; that lets gpui_ohos capture its `Rc` window state in the closure.

use std::{
    cell::RefCell,
    sync::RwLock,
};

use napi_derive_ohos::napi;
use napi_ohos::{Error, Result, bindgen_prelude::Unknown};
use openharmony_ability::{
    AsyncBridge, BridgeContextRequirement, BridgeMainThreadEvent, BridgePlugin, impl_bridge_napi_type,
};

/// Main-thread event names pushed by the ArkTS `PinchPlugin`.
const PINCH_BEGIN_EVENT: &str = "pinch-begin";
const PINCH_UPDATE_EVENT: &str = "pinch-update";
const PINCH_END_EVENT: &str = "pinch-end";

/// A pinch gesture sample from the ArkTS overlay.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct PinchSample {
    /// Cumulative scale relative to the gesture start (1.0 == no zoom).
    pub scale: f64,
    /// Pinch center in window points.
    pub center_x: f64,
    pub center_y: f64,
}

impl_bridge_napi_type!(PinchSample, "ohos.pinch.PinchSample");

/// Acknowledgement of a pinch event.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct PinchAck {
    pub accepted: bool,
}

impl_bridge_napi_type!(PinchAck, "ohos.pinch.PinchAck");

/// Phase of a pinch gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinchPhase {
    Begin,
    Update,
    End,
}

/// An incremental pinch delta consumed by gpui_ohos to build a GPUI `PinchEvent`.
/// `delta` is positive when zooming in (relative to the previous sample).
#[derive(Debug, Clone, Copy)]
pub struct PinchEventData {
    pub phase: PinchPhase,
    pub delta: f32,
    pub center_x: f32,
    pub center_y: f32,
}

/// Single-threaded callback slot, filled by gpui_ohos on the main thread and
/// invoked from `on_main_thread_event` (also the main thread). No cross-thread
/// handoff, so the closure may capture `Rc` window state.
thread_local! {
    static PINCH_CALLBACK: RefCell<Option<Box<dyn Fn(PinchEventData)>>> = RefCell::new(None);
}

/// Cumulative scale of the last pinch update, used to compute incremental deltas.
static LAST_SCALE: RwLock<f64> = RwLock::new(1.0);

/// Registers the pinch handler. Replaces any previously registered handler.
pub fn set_pinch_callback(callback: Box<dyn Fn(PinchEventData)>) {
    PINCH_CALLBACK.with(|cell| *cell.borrow_mut() = Some(callback));
}

/// Clears the pinch handler. Called when the window that owns it is destroyed.
pub fn clear_pinch_callback() {
    PINCH_CALLBACK.with(|cell| *cell.borrow_mut() = None);
}

pub struct PinchBridgePlugin;

impl BridgePlugin for PinchBridgePlugin {
    type Mode = AsyncBridge;

    const ID: &'static str = "ohos.pinch";
    const REQUIRED_CONTEXTS: &'static [BridgeContextRequirement] =
        &[BridgeContextRequirement::UiContext];

    fn on_main_thread_event<'env>(
        &self,
        event: BridgeMainThreadEvent<'env>,
    ) -> Result<Unknown<'env>> {
        match event.name() {
            PINCH_BEGIN_EVENT | PINCH_UPDATE_EVENT | PINCH_END_EVENT => {
                let sample = event.decode::<PinchSample>()?;
                let phase = match event.name() {
                    PINCH_BEGIN_EVENT => PinchPhase::Begin,
                    PINCH_UPDATE_EVENT => PinchPhase::Update,
                    _ => PinchPhase::End,
                };
                let delta = match phase {
                    PinchPhase::Begin => {
                        let mut last_scale = LAST_SCALE
                            .write()
                            .map_err(|_| Error::from_reason("Failed to write LAST_SCALE"))?;
                        *last_scale = 1.0;
                        0.0
                    }
                    PinchPhase::Update => {
                        let mut last_scale = LAST_SCALE
                            .write()
                            .map_err(|_| Error::from_reason("Failed to write LAST_SCALE"))?;
                        let delta = sample.scale - *last_scale;
                        *last_scale = sample.scale;
                        delta as f32
                    }
                    PinchPhase::End => {
                        *LAST_SCALE
                            .write()
                            .map_err(|_| Error::from_reason("Failed to write LAST_SCALE"))? = 1.0;
                        0.0
                    }
                };
                let data = PinchEventData {
                    phase,
                    delta,
                    center_x: sample.center_x as f32,
                    center_y: sample.center_y as f32,
                };
                PINCH_CALLBACK.with(|cell| {
                    if let Some(callback) = cell.borrow().as_ref() {
                        callback(data);
                    }
                });
                event.respond(PinchAck { accepted: true })
            }
            other => Err(Error::from_reason(format!(
                "Unsupported ohos.pinch main-thread event '{other}'"
            ))),
        }
    }
}
