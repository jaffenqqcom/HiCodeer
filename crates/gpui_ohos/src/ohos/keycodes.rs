//! OHOS key code → GPUI Keystroke mapping.
//!
//! Stateless principle: modifier state is carried by each key event
//! (`KeyEventData.modifier_state`, bit definitions in ArkUI_ModifierKeyName);
//! this module does not maintain any pressed/released key state.

use crate::{Keystroke, Modifiers};
use openharmony_ability::xcomponent::{KeyCode, KeyEventData};

/// ArkUI_ModifierKeyName bit definitions (return value of OH_NativeXComponent_GetKeyEventModifierKeyStates)
const MODIFIER_CTRL_BIT: u64 = 1 << 0;
const MODIFIER_SHIFT_BIT: u64 = 1 << 1;
const MODIFIER_ALT_BIT: u64 = 1 << 2;
const MODIFIER_FN_BIT: u64 = 1 << 3;

/// Shifted characters for digit keys (US layout)
const SHIFTED_DIGITS: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];

/// Convert an OHOS key event into a GPUI Keystroke.
pub fn key_event_to_keystroke(event: &KeyEventData) -> Keystroke {
    let mut modifiers = modifiers_from_modifier_state(event.modifier_state);
    let key = key_name(event.code, modifiers.shift);
    let key_char = key_char(event.code, modifiers.shift, event.capslock);

    // Consistent with the Linux keystroke_from_xkb convention: only uppercase letters
    // keep the shift modifier; for digits/symbols the shift result is already in `key`,
    // so clear the shift flag.
    if modifiers.shift && key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase() {
        modifiers.shift = false;
    }

    Keystroke {
        modifiers,
        key,
        key_char,
    }
}

/// Build GPUI Modifiers from a modifier bit mask (stateless; reflects only the combo keys carried by the event).
pub(crate) fn modifiers_from_modifier_state(state: u64) -> Modifiers {
    Modifiers {
        control: state & MODIFIER_CTRL_BIT != 0,
        alt: state & MODIFIER_ALT_BIT != 0,
        shift: state & MODIFIER_SHIFT_BIT != 0,
        platform: false,
        function: state & MODIFIER_FN_BIT != 0,
    }
}

/// GPUI key name (convention follows Linux keystroke_from_xkb).
/// Letters are always lowercase; the shift result of digit/symbol keys is reflected in `key`.
fn key_name(code: KeyCode, shift: bool) -> String {
    if let Some(index) = letter_index(code) {
        return char::from(b'a' + index).to_string();
    }
    if let Some(index) = digit_index(code) {
        return (if shift {
            SHIFTED_DIGITS[index]
        } else {
            char::from(b'0' + index as u8)
        })
        .to_string();
    }
    if let Some(number) = f_key_number(code) {
        return format!("f{number}");
    }
    if let Some(index) = numpad_digit_index(code) {
        return char::from(b'0' + index as u8).to_string();
    }

    match code {
        KeyCode::Enter | KeyCode::NumpadEnter => "enter".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Space => "space".to_string(),
        KeyCode::Del => "backspace".to_string(),
        KeyCode::ForwardDel => "delete".to_string(),
        KeyCode::Escape => "escape".to_string(),
        KeyCode::MoveHome => "home".to_string(),
        KeyCode::MoveEnd => "end".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::DpadUp => "up".to_string(),
        KeyCode::DpadDown => "down".to_string(),
        KeyCode::DpadLeft => "left".to_string(),
        KeyCode::DpadRight => "right".to_string(),
        KeyCode::ShiftLeft | KeyCode::ShiftRight => "shift".to_string(),
        KeyCode::CtrlLeft | KeyCode::CtrlRight => "control".to_string(),
        KeyCode::AltLeft | KeyCode::AltRight => "alt".to_string(),
        KeyCode::MetaLeft | KeyCode::MetaRight => "super".to_string(),
        KeyCode::CapsLock => "capslock".to_string(),
        KeyCode::Comma => symbol_char(',', '<', shift).to_string(),
        KeyCode::Period => symbol_char('.', '>', shift).to_string(),
        KeyCode::Slash => symbol_char('/', '?', shift).to_string(),
        KeyCode::Semicolon => symbol_char(';', ':', shift).to_string(),
        KeyCode::Apostrophe => symbol_char('\'', '"', shift).to_string(),
        KeyCode::LeftBracket => symbol_char('[', '{', shift).to_string(),
        KeyCode::RightBracket => symbol_char(']', '}', shift).to_string(),
        KeyCode::Backslash => symbol_char('\\', '|', shift).to_string(),
        KeyCode::Minus => symbol_char('-', '_', shift).to_string(),
        KeyCode::Equals => symbol_char('=', '+', shift).to_string(),
        KeyCode::Grave => symbol_char('`', '~', shift).to_string(),
        KeyCode::At => "@".to_string(),
        KeyCode::Plus => "+".to_string(),
        KeyCode::Star => "*".to_string(),
        KeyCode::Pound => "#".to_string(),
        KeyCode::NumpadDivide => "/".to_string(),
        KeyCode::NumpadMultiply => "*".to_string(),
        KeyCode::NumpadSubtract => "-".to_string(),
        KeyCode::NumpadAdd => "+".to_string(),
        KeyCode::NumpadDot => ".".to_string(),
        _ => String::new(),
    }
}

/// The actual character this key can type; None for non-character keys.
/// For letters, Caps Lock toggles the same case as Shift, matching the
/// desktop xkb convention where Shift inverts Caps Lock (`shift XOR capslock`).
fn key_char(code: KeyCode, shift: bool, capslock: bool) -> Option<String> {
    if let Some(index) = letter_index(code) {
        let upper = shift ^ capslock;
        return Some(
            (if upper {
                char::from(b'A' + index)
            } else {
                char::from(b'a' + index)
            })
            .to_string(),
        );
    }
    if let Some(index) = digit_index(code) {
        return Some(
            (if shift {
                SHIFTED_DIGITS[index]
            } else {
                char::from(b'0' + index as u8)
            })
            .to_string(),
        );
    }
    if let Some(index) = numpad_digit_index(code) {
        return Some(char::from(b'0' + index as u8).to_string());
    }

    match code {
        KeyCode::Space => Some(" ".to_string()),
        KeyCode::Comma => Some(symbol_char(',', '<', shift).to_string()),
        KeyCode::Period => Some(symbol_char('.', '>', shift).to_string()),
        KeyCode::Slash => Some(symbol_char('/', '?', shift).to_string()),
        KeyCode::Semicolon => Some(symbol_char(';', ':', shift).to_string()),
        KeyCode::Apostrophe => Some(symbol_char('\'', '"', shift).to_string()),
        KeyCode::LeftBracket => Some(symbol_char('[', '{', shift).to_string()),
        KeyCode::RightBracket => Some(symbol_char(']', '}', shift).to_string()),
        KeyCode::Backslash => Some(symbol_char('\\', '|', shift).to_string()),
        KeyCode::Minus => Some(symbol_char('-', '_', shift).to_string()),
        KeyCode::Equals => Some(symbol_char('=', '+', shift).to_string()),
        KeyCode::Grave => Some(symbol_char('`', '~', shift).to_string()),
        KeyCode::At => Some("@".to_string()),
        KeyCode::Plus => Some("+".to_string()),
        KeyCode::Star => Some("*".to_string()),
        KeyCode::Pound => Some("#".to_string()),
        _ => None,
    }
}

/// Index of a letter key within the A..=Z declaration range (relies on fieldless
/// enum variants being contiguous in declaration order)
fn letter_index(code: KeyCode) -> Option<u8> {
    let raw = code as u32;
    let start = KeyCode::A as u32;
    let end = KeyCode::Z as u32;
    (start..=end).contains(&raw).then(|| (raw - start) as u8)
}

/// Index of a main-keyboard digit within the Key0..=Key9 declaration range
fn digit_index(code: KeyCode) -> Option<usize> {
    let raw = code as u32;
    let start = KeyCode::Key0 as u32;
    let end = KeyCode::Key9 as u32;
    (start..=end).contains(&raw).then(|| (raw - start) as usize)
}

/// Index of a numpad digit within the Numpad0..=Numpad9 declaration range
fn numpad_digit_index(code: KeyCode) -> Option<usize> {
    let raw = code as u32;
    let start = KeyCode::Numpad0 as u32;
    let end = KeyCode::Numpad9 as u32;
    (start..=end).contains(&raw).then(|| (raw - start) as usize)
}

/// Function key number (1..=24).
/// F1..F12 and F13..F24 are each contiguous in the enum declaration, but variants
/// such as NumLock/Numpad are interleaved between F12 and F13, so the range must
/// be checked in two segments instead of one F1..=F24 span.
fn f_key_number(code: KeyCode) -> Option<u8> {
    let raw = code as u32;
    let f1 = KeyCode::F1 as u32;
    let f12 = KeyCode::F12 as u32;
    let f13 = KeyCode::F13 as u32;
    let f24 = KeyCode::F24 as u32;
    if (f1..=f12).contains(&raw) {
        Some((raw - f1) as u8 + 1)
    } else if (f13..=f24).contains(&raw) {
        Some((raw - f13) as u8 + 13)
    } else {
        None
    }
}

/// Choose the unshifted or shifted character.
fn symbol_char(base: char, shifted: char, shift: bool) -> char {
    if shift { shifted } else { base }
}
