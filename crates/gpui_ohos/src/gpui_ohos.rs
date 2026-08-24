#![cfg(target_env = "ohos")]

mod ohos;

pub use gpui::*;
// pub use ohos::current_platform; // construction moved into register_platform factory
pub use ohos::register_platform;
