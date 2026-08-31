use napi_ohos::{Env, Error, Result};
use ohos_arkui_binding::component::attribute::ArkUICommonAttribute;
use ohos_arkui_binding::{ArkUIHandle, RootNode, XComponent};
use ohos_arkui_input_binding::{ArkUIInputEvent, UIInputEvent};
use ohos_xcomponent_binding::TouchPointTool;

use crate::{input, Event, InputEvent, OpenHarmonyApp, Rect, Size};

/// create lifecycle object and return to arkts
pub fn render(
    env: &Env,
    slot: ArkUIHandle,
    render_owner: String,
    app: OpenHarmonyApp,
) -> Result<RootNode> {
    let mut root = RootNode::new(slot);
    let xcomponent_native =
        XComponent::new().map_err(|e| Error::from_reason(e.reason.to_string()))?;
    xcomponent_native
        .background_color(0x0000_0000)
        .map_err(|e| Error::from_reason(e.reason.to_string()))?;
    // XComponent must be focusable and hold the default focus, otherwise the system routes
    // key events to the ArkTS layer focus node (Row/secure_field in DefaultXComponent) and
    // the NDK on_key_event callback is never fired.
    xcomponent_native
        .set_focusable(true)
        .map_err(|e| Error::from_reason(e.reason.to_string()))?;
    xcomponent_native
        .set_default_focus(true)
        .map_err(|e| Error::from_reason(e.reason.to_string()))?;

    let xcomponent = xcomponent_native.native_xcomponent();

    let xc = xcomponent.clone();

    let on_surface_created_app = app.clone();
    let on_surface_created_owner = render_owner.clone();

    xcomponent.on_surface_created(move |xc_raw, win| {
        let size = xc_raw.size(win).unwrap();
        let offset = xc_raw.offset(win).unwrap();
        let rect = Rect {
            top: offset.y as _,
            left: offset.x as _,
            width: size.width as _,
            height: size.height as _,
        };
        if !on_surface_created_app.activate_render_surface(
            &on_surface_created_owner,
            xc.native_window(),
            rect,
        ) {
            return Ok(());
        }

        if let Some(ref mut h) = *on_surface_created_app.event_loop.borrow_mut() {
            h(Event::SurfaceCreate)
        }

        Ok(())
    });

    let on_surface_destroyed_app = app.clone();
    let on_surface_destroyed_owner = render_owner.clone();
    xcomponent.on_surface_destroyed(move |_, _| {
        if on_surface_destroyed_app.deactivate_render_surface(&on_surface_destroyed_owner) {
            on_surface_destroyed_app.dispatch_surface_destroy();
        }
        Ok(())
    });

    let on_surface_changed_app = app.clone();
    let on_surface_changed_owner = render_owner.clone();
    xcomponent.on_surface_changed(move |xc, win| {
        let size = xc.size(win).unwrap();
        let offset = xc.offset(win).unwrap();
        if on_surface_changed_app.update_render_surface_rect(
            &on_surface_changed_owner,
            Rect {
                top: offset.y as _,
                left: offset.x as _,
                width: size.width as _,
                height: size.height as _,
            },
        ) {
            if let Some(ref mut h) = *on_surface_changed_app.event_loop.borrow_mut() {
                h(Event::WindowResize(Size {
                    width: size.width as _,
                    height: size.height as _,
                }))
            }
        }
        Ok(())
    });

    let on_touch_event_app = app.clone();
    let on_touch_event_owner = render_owner.clone();
    xcomponent.on_touch_event(move |_, _, data| {
        if !on_touch_event_app.is_render_surface_active(&on_touch_event_owner) {
            return Ok(());
        }
        // Only touchscreen (Finger) touches are kept here; mouse and touchpad
        // input arrive through their own NDK/UIInputEvent callbacks.
        let tool_type = data
            .touch_points
            .first()
            .map(|point| point.event_tool_type)
            .unwrap_or(TouchPointTool::Unknown);
        if tool_type == TouchPointTool::Finger {
            if let Some(ref mut h) = *on_touch_event_app.event_loop.borrow_mut() {
                h(Event::Input(InputEvent::TouchEvent(data)));
            }
        }
        Ok(())
    });

    let on_key_event_app = app.clone();
    let on_key_event_owner = render_owner.clone();
    let _ = xcomponent.on_key_event(move |_, _, data| {
        if !on_key_event_app.is_render_surface_active(&on_key_event_owner) {
            return Ok(());
        }
        if let Some(ref mut h) = *on_key_event_app.event_loop.borrow_mut() {
            h(Event::Input(InputEvent::KeyEvent(data)));
        }
        Ok(())
    });

    let on_mouse_event_app = app.clone();
    let on_mouse_event_owner = render_owner.clone();
    xcomponent.on_mouse_event(move |_, _, data| {
        if !on_mouse_event_app.is_render_surface_active(&on_mouse_event_owner) {
            return Ok(());
        }
        if let Some(ref mut h) = *on_mouse_event_app.event_loop.borrow_mut() {
            h(Event::Input(InputEvent::MouseEvent(data)));
        }
        Ok(())
    })?;

    // Pointer enter/leave of the XComponent window. Mirrors the X11 EnterNotify/LeaveNotify
    // handling in gpui_linux: only the enter/leave transition drives the GPUI hovered state.
    let on_hover_event_app = app.clone();
    let on_hover_event_owner = render_owner.clone();
    xcomponent.on_hover_event(move |_, is_hover| {
        if !on_hover_event_app.is_render_surface_active(&on_hover_event_owner) {
            return Ok(());
        }
        if let Some(ref mut h) = *on_hover_event_app.event_loop.borrow_mut() {
            h(Event::Input(InputEvent::HoverEvent(is_hover)));
        }
        Ok(())
    })?;
    xcomponent.register_mouse_event_callback()?;

    // Scroll input arrives through the UIInputEvent channel.
    // OH_NativeXComponent_RegisterUIInputEventCallback supports Axis events only.
    let on_ui_input_event_app = app.clone();
    let on_ui_input_event_owner = render_owner.clone();
    let ui_input_callback = move |_, event: ArkUIInputEvent| {
        if !on_ui_input_event_app.is_render_surface_active(&on_ui_input_event_owner) {
            return Ok(());
        }
        if let Some(input_event) = input::ui_input_event_to_input_event(&event) {
            if let Some(ref mut h) = *on_ui_input_event_app.event_loop.borrow_mut() {
                h(Event::Input(input_event));
            }
        }
        Ok(())
    };
    xcomponent.on_ui_input_event(UIInputEvent::Axis, ui_input_callback)?;

    xcomponent.register_callback()?;

    app.begin_render(&render_owner, xcomponent_native.clone())?;
    if let Err(error) = root.mount(xcomponent_native) {
        app.release_render(&render_owner);
        return Err(Error::from_reason(error.reason.to_string()));
    }

    Ok(root)
}
