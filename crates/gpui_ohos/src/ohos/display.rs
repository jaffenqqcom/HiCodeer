use std::fmt::Debug;
use uuid::Uuid;

use openharmony_ability::OpenHarmonyApp;

use crate::{Bounds, DisplayId, Pixels, PlatformDisplay, Result, point, px, size};

#[derive(Clone)]
pub(crate) struct OhosDisplay {
    app: OpenHarmonyApp,
    id: DisplayId,
}

impl OhosDisplay {
    pub(crate) fn new(app: OpenHarmonyApp) -> Self {
        Self {
            app,
            id: DisplayId::new(0),
        }
    }
}

impl Debug for OhosDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OhosDisplay").field("id", &self.id).finish()
    }
}

impl PlatformDisplay for OhosDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<Uuid> {
        // Generate a stable UUID for the display
        Ok(Uuid::from_bytes([
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ]))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        // Get actual display bounds from content_rect (device px) and convert to logical px.
        let content_rect = self.app.content_rect();
        let scale = self.app.scale() as f32;
        if content_rect.width > 0 && content_rect.height > 0 {
            Bounds::new(
                point(
                    px(content_rect.left as f32 / scale),
                    px(content_rect.top as f32 / scale),
                ),
                size(
                    px(content_rect.width as f32 / scale),
                    px(content_rect.height as f32 / scale),
                ),
            )
        } else {
            // Fallback to default bounds if content_rect is not available yet
            Bounds::new(point(px(0.0), px(0.0)), size(px(1080.0), px(1920.0)))
        }
    }

    fn visible_bounds(&self) -> Bounds<Pixels> {
        // On OHOS, visible bounds are the same as full bounds
        self.bounds()
    }
}
