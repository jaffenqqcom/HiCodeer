//! External file drop capability plugin.
//!
//! The ArkTS `FileDropPlugin` attaches a transparent drop-target overlay to the
//! XComponent UI tree and pushes the full drag-and-drop event stream to Rust:
//! `drag-enter` (drag entered the window), `drag-move` (continuous position,
//! mirroring X11's `XdndPosition`), and `drop-files` (release, carrying the file
//! URIs). Each event is forwarded immediately (event-driven — no polling) to a
//! callback registered by gpui_ohos, which turns the stream into GPUI
//! `FileDropEvent`s so the workspace can open dropped files.
//!
//! `on_main_thread_event` always runs on the Ability main thread, so the callback
//! is stored in a `thread_local` (single-threaded) slot instead of a `Send + Sync`
//! static; that lets gpui_ohos capture its `Rc` window state in the closure.

use std::cell::RefCell;

use napi_derive_ohos::napi;
use napi_ohos::{Error, Result, bindgen_prelude::Unknown};
use openharmony_ability::{
    AsyncBridge, BridgeContextRequirement, BridgeMainThreadEvent, BridgePlugin, impl_bridge_napi_type,
};

/// Main-thread event names pushed by the ArkTS `FileDropPlugin`.
const DRAG_ENTER_EVENT: &str = "drag-enter";
const DRAG_MOVE_EVENT: &str = "drag-move";
const DROP_FILES_EVENT: &str = "drop-files";

/// A drag-move sample: the pointer position in window points (vp).
#[napi(object)]
#[derive(Clone, Debug)]
pub struct DragMoveData {
    pub position_x: f64,
    pub position_y: f64,
}

impl_bridge_napi_type!(DragMoveData, "ohos.filedrop.DragMoveData");

/// URIs of files dropped onto the window, plus the drop position in window points.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct DropFilesData {
    pub files: Vec<String>,
    /// Drop position X in window points (vp).
    pub position_x: f64,
    /// Drop position Y in window points (vp).
    pub position_y: f64,
}

impl_bridge_napi_type!(DropFilesData, "ohos.filedrop.DropFilesData");

/// Acknowledgement of a drag/drop event.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct DropAck {
    pub accepted: bool,
}

impl_bridge_napi_type!(DropAck, "ohos.filedrop.DropAck");

/// A file-drop event forwarded to gpui_ohos.
pub enum FileDropEventData {
    /// The drag entered the window; establishes the GPUI drag state.
    Enter,
    /// The drag moved; mirror X11 `Pending` to keep the hover hit test current.
    Move { position_x: f64, position_y: f64 },
    /// The file was released; carries the URIs and the drop position.
    Drop {
        files: Vec<String>,
        position_x: f64,
        position_y: f64,
    },
}

/// Single-threaded callback slot, filled by gpui_ohos on the main thread and
/// invoked from `on_main_thread_event` (also the main thread).
thread_local! {
    static FILEDROP_CALLBACK: RefCell<Option<Box<dyn Fn(FileDropEventData)>>> = RefCell::new(None);
}

/// Registers the file-drop handler. Replaces any previously registered handler.
pub fn set_filedrop_callback(callback: Box<dyn Fn(FileDropEventData)>) {
    FILEDROP_CALLBACK.with(|cell| *cell.borrow_mut() = Some(callback));
}

/// Clears the file-drop handler. Called when the window that owns it is destroyed.
pub fn clear_filedrop_callback() {
    FILEDROP_CALLBACK.with(|cell| *cell.borrow_mut() = None);
}

pub struct FileDropBridgePlugin;

impl BridgePlugin for FileDropBridgePlugin {
    type Mode = AsyncBridge;

    const ID: &'static str = "ohos.filedrop";
    const REQUIRED_CONTEXTS: &'static [BridgeContextRequirement] =
        &[BridgeContextRequirement::UiContext];

    fn on_main_thread_event<'env>(
        &self,
        event: BridgeMainThreadEvent<'env>,
    ) -> Result<Unknown<'env>> {
        let data = match event.name() {
            DRAG_ENTER_EVENT => Some(FileDropEventData::Enter),
            DRAG_MOVE_EVENT => {
                let data = event.decode::<DragMoveData>()?;
                Some(FileDropEventData::Move {
                    position_x: data.position_x,
                    position_y: data.position_y,
                })
            }
            DROP_FILES_EVENT => {
                let data = event.decode::<DropFilesData>()?;
                Some(FileDropEventData::Drop {
                    files: data.files,
                    position_x: data.position_x,
                    position_y: data.position_y,
                })
            }
            other => {
                return Err(Error::from_reason(format!(
                    "Unsupported ohos.filedrop main-thread event '{other}'"
                )));
            }
        };
        if let Some(data) = data {
            FILEDROP_CALLBACK.with(|cell| {
                if let Some(callback) = cell.borrow().as_ref() {
                    callback(data);
                }
            });
        }
        event.respond(DropAck { accepted: true })
    }
}
