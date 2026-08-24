mod dispatcher;
mod display;
mod keycodes;
mod keyboard;
mod platform;
mod text_system;
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;
mod window;

use gpui::Platform;
use std::rc::Rc;

// Platform construction moved into OhosPlatformFactoryImpl below; gpui_platform
// now reaches the platform through gpui_ohos_linker instead of this crate.
//
// pub fn current_platform(_headless: bool) -> std::rc::Rc<dyn gpui::Platform> {
//     std::rc::Rc::new(
//         platform::OhosPlatform::new()
//             .inspect_err(|err| {
//                 log::error!("Failed to initialize OHOS platform: {}", err);
//             })
//             .unwrap_or_else(|_| panic!("Failed to initialize OHOS platform")),
//     )
// }

pub struct OhosPlatformFactoryImpl;

impl gpui_ohos_linker::OhosPlatformFactory for OhosPlatformFactoryImpl {
    fn current_platform(&self, _headless: bool) -> Rc<dyn Platform> {
        let platform = Rc::new(
            platform::OhosPlatform::new()
                .inspect_err(|err| {
                    log::error!("Failed to initialize OHOS platform: {err}");
                })
                .unwrap_or_else(|_| panic!("Failed to initialize OHOS platform")),
        );
        platform
    }
}

/// Register the OHOS platform factory into gpui_ohos_linker. Called by
/// launch-zed before Zed starts, so gpui_platform can construct the platform.
pub fn register_platform() {
    gpui_ohos_linker::register(Box::new(OhosPlatformFactoryImpl));
}
