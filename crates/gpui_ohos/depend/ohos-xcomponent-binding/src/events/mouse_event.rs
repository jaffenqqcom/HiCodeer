use crate::{MouseAction, MouseButton};
use ohos_xcomponent_sys::OH_NativeXComponent_MouseEvent;

// OH_NativeXComponent_MouseEventButton bitmask values.
const MOUSE_BUTTON_LEFT: u32 = 0x01;
const MOUSE_BUTTON_RIGHT: u32 = 0x02;
const MOUSE_BUTTON_MIDDLE: u32 = 0x04;
const MOUSE_BUTTON_BACK: u32 = 0x08;
const MOUSE_BUTTON_FORWARD: u32 = 0x10;

fn mouse_button_from_mask(mask: u32) -> MouseButton {
    match mask {
        MOUSE_BUTTON_LEFT => MouseButton::LeftButton,
        MOUSE_BUTTON_RIGHT => MouseButton::RightButton,
        MOUSE_BUTTON_MIDDLE => MouseButton::MiddleButton,
        MOUSE_BUTTON_BACK => MouseButton::BackButton,
        MOUSE_BUTTON_FORWARD => MouseButton::ForwardButton,
        // Combined masks or unknown values fall back to the none button rather
        // than panicking through the derived EnumFrom.
        _ => MouseButton::NoneButton,
    }
}

#[derive(Debug, Clone)]
pub struct MouseEventData {
    pub x: f32,
    pub y: f32,
    pub screen_x: f32,
    pub screen_y: f32,
    pub timestamp: i64,
    pub action: MouseAction,
    pub button: MouseButton,
    /// Raw button bitmask carried by the event (0x01 Left, 0x02 Right, ...).
    /// For Move events this reports the currently pressed buttons, so the
    /// consumer can read pressed-button state without caching it.
    pub button_mask: u32,
    /// Modifier-key bitmask of the mouse event (ArkUI_ModifierKeyName),
    /// queried live from the extra mouse event info (never cached).
    pub modifiers: u64,
}

impl From<OH_NativeXComponent_MouseEvent> for MouseEventData {
    fn from(value: OH_NativeXComponent_MouseEvent) -> Self {
        Self {
            x: value.x,
            y: value.y,
            screen_x: value.screenX,
            screen_y: value.screenY,
            timestamp: value.timestamp,
            action: value.action.into(),
            button: mouse_button_from_mask(value.button),
            button_mask: value.button,
            modifiers: 0,
        }
    }
}
