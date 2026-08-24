use crate::{Action, EventSource, KeyCode};

#[derive(Debug, Clone)]
pub struct KeyEventData {
    pub code: KeyCode,
    pub action: Action,
    pub device_id: i64,
    pub source: EventSource,
    pub timestamp: i64,
    /// Bit mask of the combined modifier keys; see ArkUI_ModifierKeyName for the values
    /// (CTRL=1<<0, SHIFT=1<<1, ALT=1<<2, FN=1<<3).
    /// Populated by OH_NativeXComponent_GetKeyEventModifierKeyStates; carried with each key event.
    pub modifier_state: u64,
    /// Caps Lock state at the time of this key event.
    /// Populated by OH_NativeXComponent_GetKeyEventCapsLockState; carried with each key event.
    pub capslock: bool,
}
