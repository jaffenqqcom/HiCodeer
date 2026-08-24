use collections::HashMap;

use crate::{KeybindingKeystroke, Keystroke, PlatformKeyboardLayout, PlatformKeyboardMapper};

pub(crate) struct OhosKeyboardLayout;

impl PlatformKeyboardLayout for OhosKeyboardLayout {
    fn id(&self) -> &str {
        "ohos-default"
    }

    fn name(&self) -> &str {
        "OHOS Default"
    }
}

pub(crate) struct OhosKeyboardMapper;

impl PlatformKeyboardMapper for OhosKeyboardMapper {
    fn map_key_equivalent(
        &self,
        keystroke: Keystroke,
        _use_key_equivalents: bool,
    ) -> KeybindingKeystroke {
        KeybindingKeystroke::from_keystroke(keystroke)
    }

    fn get_key_equivalents(&self) -> Option<&HashMap<char, char>> {
        None
    }
}
