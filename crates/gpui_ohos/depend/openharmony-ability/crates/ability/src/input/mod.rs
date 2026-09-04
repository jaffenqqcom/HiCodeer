use std::fmt::Debug;

use ohos_arkui_input_binding::{
    ArkUIInputEvent, ModifierKey, UIAxisEventAction, UIInputEvent, UIInputToolType,
};
use ohos_ime_binding::KeyboardStatus;
use ohos_xcomponent_binding::{KeyEventData, TouchEventData};
// Re-exported so the platform layer (gpui_ohos) can name these types when
// consuming device-routed input events.
pub use ohos_xcomponent_binding::{MouseAction, MouseButton, MouseEventData};

mod text_input;
pub use text_input::*;

/// Scroll phase of a device scroll gesture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScrollPhase {
    Begin,
    Update,
    End,
}

/// Modifier keys held when a device input event occurred.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct DeviceModifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Physical device that produced a scroll (axis) event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AxisToolType {
    Mouse,
    Touchpad,
}

/// Event data for a scroll event (mouse wheel or trackpad two-finger scroll).
///
/// Scroll events arrive through the UIInputEvent channel, whose registration
/// only supports Axis events. Mouse pointer input goes through the NDK mouse
/// callback; touchscreen touches through the NDK touch callback.
#[derive(Clone, Debug)]
pub struct AxisEventData {
    pub x: f32,
    pub y: f32,
    pub timestamp: i64,
    /// Modifier keys held when the event occurred.
    pub modifiers: DeviceModifiers,
    /// The device that produced the scroll.
    pub tool_type: AxisToolType,
    pub scroll_vertical: f64,
    pub scroll_horizontal: f64,
    pub scroll_phase: Option<ScrollPhase>,
}

#[derive(Clone)]
pub enum InputEvent {
    KeyEvent(KeyEventData),
    MouseEvent(MouseEventData),
    TouchEvent(TouchEventData),
    AxisEvent(AxisEventData),
    ImeEvent(ImeEvent),
    /// Pointer entered (true) or left (false) the XComponent window.
    HoverEvent(bool),
}

impl Debug for InputEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputEvent::KeyEvent(data) => write!(f, "KeyEvent: {:?}", data),
            InputEvent::MouseEvent(data) => write!(f, "MouseEvent: {:?}", data),
            InputEvent::TouchEvent(data) => write!(f, "TouchEvent: {:?}", data),
            InputEvent::AxisEvent(data) => write!(f, "AxisEvent: {:?}", data),
            InputEvent::ImeEvent(data) => write!(f, "ImeEvent: {:?}", data),
            InputEvent::HoverEvent(is_hover) => write!(f, "HoverEvent: {is_hover}"),
        }
    }
}

fn map_axis_phase(action: UIAxisEventAction) -> Option<ScrollPhase> {
    match action {
        UIAxisEventAction::Begin => Some(ScrollPhase::Begin),
        UIAxisEventAction::Update => Some(ScrollPhase::Update),
        UIAxisEventAction::End => Some(ScrollPhase::End),
        _ => None,
    }
}

fn read_device_modifiers(event: &ArkUIInputEvent) -> DeviceModifiers {
    // OH_ArkUI_UIInputEvent_GetModifierKeyStates (api-17+): modifier state is
    // carried per-event so the consumer does not need to cache it.
    let Ok(states) = event.modifier_key_states() else {
        return DeviceModifiers::default();
    };
    DeviceModifiers {
        control: states.contains(ModifierKey::Ctrl),
        shift: states.contains(ModifierKey::Shift),
        alt: states.contains(ModifierKey::Alt),
    }
}

/// Convert an ArkUI UIInputEvent into an InputEvent.
///
/// OH_NativeXComponent_RegisterUIInputEventCallback only supports Axis events,
/// so this produces scroll input only. Mouse pointer input arrives through the
/// NDK mouse callback; touchscreen touches through the NDK touch callback.
pub fn ui_input_event_to_input_event(event: &ArkUIInputEvent) -> Option<InputEvent> {
    if event.event_type != UIInputEvent::Axis {
        return None;
    }
    let tool_type = match event.tool_type {
        UIInputToolType::Touchpad => AxisToolType::Touchpad,
        _ => AxisToolType::Mouse,
    };
    Some(InputEvent::AxisEvent(AxisEventData {
        x: event.pointer_x(),
        y: event.pointer_y(),
        timestamp: event.event_time(),
        modifiers: read_device_modifiers(event),
        tool_type,
        scroll_vertical: event.axis_vertical_value(),
        scroll_horizontal: event.axis_horizontal_value(),
        scroll_phase: map_axis_phase(event.axis_action()),
    }))
}

#[derive(Clone)]
pub enum ImeEvent {
    TextInputEvent(TextInputEventData),
    BackspaceEvent(i32),
    ImeStatusEvent(KeyboardStatus),
    EnterEvent(i32),
    DeleteRightEvent(i32),
}

impl Debug for ImeEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImeEvent::TextInputEvent(data) => write!(f, "TextInputEvent: {:?}", data),
            ImeEvent::BackspaceEvent(len) => write!(f, "BackspaceEvent: delete length is {}", len),
            ImeEvent::ImeStatusEvent(status) => write!(f, "ImeStatusEvent: {:?}", status),
            ImeEvent::EnterEvent(key) => write!(f, "EnterEvent: {:?}", key),
            ImeEvent::DeleteRightEvent(len) => write!(f, "DeleteRightEvent: delete length is {}", len),
        }
    }
}

#[cfg(test)]
mod tests {
    use ohos_xcomponent_binding::{MouseAction, MouseButton, MouseEventData};

    use super::*;

    #[test]
    fn mouse_event_debug_output_includes_event_data() {
        let event = InputEvent::MouseEvent(MouseEventData {
            x: 12.5,
            y: 24.0,
            screen_x: 112.5,
            screen_y: 224.0,
            timestamp: 42,
            action: MouseAction::Move,
            button: MouseButton::NoneButton,
            button_mask: 0,
            modifiers: 0,
        });

        let output = format!("{event:?}");
        assert!(output.starts_with("MouseEvent: MouseEventData"));
        assert!(output.contains("action: Move"));
        assert!(output.contains("button: NoneButton"));
    }
}
