use log::warn;

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
};

use anyhow::Result;
use futures::channel::oneshot;
use openharmony_ability::{ColorMode, Event, InputEvent, OpenHarmonyApp, path_from_uri};
use openharmony_ability_plugin_cursor::{CursorBridgePlugin, CursorExt};
use openharmony_ability_plugin_files::{
    FileDialogOptions, FilesBridgePlugin, FilesExt, dialog_type,
};
use openharmony_ability_plugin_pinch::PinchBridgePlugin;
use openharmony_ability_plugin_filedrop::FileDropBridgePlugin;
use openharmony_ability_plugin_openwith::OpenWithBridgePlugin;
use openharmony_ability_plugin_ime::ImeBridgePlugin;
use openharmony_ability_plugin_url::{UrlBridgePlugin, UrlExt};

use crate::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle, ForegroundExecutor,
    Keymap, Menu, MenuItem, OwnedMenu, PathPromptOptions, Platform, PlatformDisplay,
    PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow,
    PriorityQueueReceiver, Result as GpuiResult, RunnableVariant, Task, ThermalState,
    WindowAppearance, WindowParams,
};

use super::{
    dispatcher::OhosDispatcher, display::OhosDisplay, text_system::OhosTextSystem,
    wgpu_context::WgpuContext, window::{OhosWindow, appearance_from_mode},
};

pub(crate) struct OhosPlatform {
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    dispatcher: Arc<OhosDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    primary_display: Rc<RefCell<Option<OhosDisplay>>>,
    main_receiver: PriorityQueueReceiver<RunnableVariant>,
    gpu_context: Rc<RefCell<Option<Arc<WgpuContext>>>>,
    windows: Rc<RefCell<Vec<Weak<RefCell<OhosWindow>>>>>,
    /// Last pointer style pushed to the system, to skip duplicate system calls.
    last_cursor_style: Cell<Option<i32>>,
    /// Application menus registered via `set_menus`, surfaced to the custom
    /// title bar's `ApplicationMenu` (OHOS has no native system menu bar to
    /// host them, unlike macOS).
    menus: Rc<RefCell<Vec<OwnedMenu>>>,
    /// Paths requested via the system "open with" action before any window existed.
    /// Flushed into the first window that gets created (cold start from a file tap).
    pending_open_with: Rc<RefCell<Vec<PathBuf>>>,
    /// Callback registered by gpui (`App::on_open_urls`) that forwards URLs to Zed's
    /// open-listener. The open-with flow delivers resolved file paths here so they are
    /// opened through Zed's normal workspace path rather than the drag-and-drop state
    /// machine (which requires an element drop target under the pointer).
    open_urls_callback: Rc<RefCell<Option<Box<dyn FnMut(Vec<String>)>>>>,
    /// Whether a system file dialog is already pending. The system picker is not a
    /// gpui modal, so nothing upstream keeps a second request out while the first
    /// is still open; each extra request would launch another UIExtension.
    path_prompt_busy: Rc<Cell<bool>>,
}

impl OhosPlatform {
    pub(crate) fn new() -> Result<Self> {
        let (main_sender, main_receiver) = PriorityQueueReceiver::new();
        let dispatcher = Arc::new(OhosDispatcher::new(main_sender));
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = Arc::new(OhosTextSystem::new());

        let platform = Self {
            app: Rc::new(RefCell::new(None)),
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            primary_display: Rc::new(RefCell::new(None)),
            main_receiver,
            gpu_context: Rc::new(RefCell::new(None)),
            windows: Rc::new(RefCell::new(Vec::new())),
            last_cursor_style: Cell::new(None),
            menus: Rc::new(RefCell::new(Vec::new())),
            pending_open_with: Rc::new(RefCell::new(Vec::new())),
            open_urls_callback: Rc::new(RefCell::new(None)),
            path_prompt_busy: Rc::new(Cell::new(false)),
        };
        // The ArkTS host provides the OpenHarmonyApp on the main thread before this
        // platform is constructed; own it from creation, mirroring how MacPlatform /
        // Linux platforms hold their native objects (no injection channel via gpui).
        if let Some(app) = openharmony_ability::global_app() {
            platform.set_app(app);
        }
        Ok(platform)
    }

    pub(crate) fn set_app(&self, app: OpenHarmonyApp) {
        *self.app.borrow_mut() = Some(app.clone());
        // Initialize primary display when app is set
        *self.primary_display.borrow_mut() = Some(OhosDisplay::new(app.clone()));
        self.dispatcher.set_waker(app.create_waker());
        self.register_plugins(&app);
        self.register_openwith_handler();
    }

    /// Registers every OHOS bridge plugin used by this platform layer.
    /// Keep all `register_plugin` calls in this single function so new platform
    /// capability plugins are registered centrally instead of scattered across
    /// application startup.
    fn register_plugins(&self, app: &OpenHarmonyApp) {
        if let Err(error) = app.register_plugin(CursorBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(CursorBridgePlugin) failed: {error}"
            );
        }
        if let Err(error) = app.register_plugin(FilesBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(FilesBridgePlugin) failed: {error}"
            );
        }
        if let Err(error) = app.register_plugin(PinchBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(PinchBridgePlugin) failed: {error}"
            );
        }
        if let Err(error) = app.register_plugin(FileDropBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(FileDropBridgePlugin) failed: {error}"
            );
        }
        if let Err(error) = app.register_plugin(OpenWithBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(OpenWithBridgePlugin) failed: {error}"
            );
        }
        if let Err(error) = app.register_plugin(ImeBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(ImeBridgePlugin) failed: {error}"
            );
        }
        if let Err(error) = app.register_plugin(UrlBridgePlugin) {
            log::error!(
                "register_plugins: register_plugin(UrlBridgePlugin) failed: {error}"
            );
        }
    }

    /// Registers the open-with handler. The ArkTS `OpenWithPlugin` forwards file URIs
    /// received from the system "open with" action; we resolve them to paths and deliver
    /// them to Zed through gpui's `on_open_urls` callback (the same path macOS uses for
    /// "open with"), or buffer them until that callback is registered (cold start).
    fn register_openwith_handler(&self) {
        let open_urls_callback = self.open_urls_callback.clone();
        let pending = self.pending_open_with.clone();
        openharmony_ability_plugin_openwith::set_openwith_callback(Box::new(
            move |uris: Vec<String>| {
                let mut paths: Vec<PathBuf> = Vec::new();
                for uri in &uris {
                    match path_from_uri(uri) {
                        Some(path) => paths.push(path),
                        None => log::warn!("open-with: failed to resolve URI '{uri}'"),
                    }
                }
                if paths.is_empty() {
                    return;
                }
                let mut callback = open_urls_callback.borrow_mut();
                match callback.as_mut() {
                    Some(callback) => {
                        let urls: Vec<String> = paths
                            .iter()
                            .map(|path| OhosPlatform::file_url_from_path(path))
                            .collect();
                        log::info!(
                            "open-with: delivering {} url(s) through on_open_urls",
                            urls.len()
                        );
                        callback(urls);
                    }
                    None => {
                        // gpui has not registered its on_open_urls callback yet (the app is
                        // still booting). Buffer until it does; `on_open_urls` flushes these.
                        log::info!(
                            "open-with: no on_open_urls callback yet, buffering {} path(s)",
                            paths.len()
                        );
                        pending.borrow_mut().extend(paths);
                    }
                }
            },
        ));
    }

    /// Delivers any open-with paths buffered before `on_open_urls` was registered.
    fn flush_pending_open_with_urls(&self) {
        let pending = self
            .pending_open_with
            .borrow_mut()
            .drain(..)
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }
        let mut callback = self.open_urls_callback.borrow_mut();
        match callback.as_mut() {
            Some(callback) => {
                let urls: Vec<String> = pending
                    .iter()
                    .map(|path| OhosPlatform::file_url_from_path(path))
                    .collect();
                log::info!(
                    "open-with: flushing {} buffered path(s) through on_open_urls",
                    urls.len()
                );
                callback(urls);
            }
            None => self.pending_open_with.borrow_mut().extend(pending),
        }
    }

    /// Opens any paths queued by `register_openwith_handler` before a window existed.
    /// Kept as a fallback for the case where `on_open_urls` was registered after the
    /// first window was created; the primary delivery path is `on_open_urls`.
    fn flush_pending_open_with(&self, window: &Rc<RefCell<OhosWindow>>) {
        let pending = self
            .pending_open_with
            .borrow_mut()
            .drain(..)
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            log::info!(
                "open-with: flushing {} buffered path(s) through window fallback",
                pending.len()
            );
            window.borrow().open_external_paths(pending);
        }
    }

    /// Converts a local path into a `file://` URL that Zed's `OpenRequest::parse`
    /// understands (it strips the `file://` prefix and url-decodes the remainder).
    /// ASCII path-safe bytes are kept verbatim; everything else (spaces, non-ASCII,
    /// reserved characters) is percent-encoded while `/` stays a separator.
    fn file_url_from_path(path: &Path) -> String {
        let mut url = String::from("file://");
        for byte in path.to_string_lossy().bytes() {
            match byte {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'/'
                | b'-'
                | b'_'
                | b'.'
                | b'~' => url.push(byte as char),
                _ => {
                    url.push('%');
                    url.push(char::from_digit((byte >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                    url.push(char::from_digit((byte & 0x0F) as u32, 16).unwrap().to_ascii_uppercase());
                }
            }
        }
        url
    }

    fn run_foreground_tasks(&self) {
        // Process GPUI tasks queued for the main thread
        // Similar to Windows' run_foreground_task, but simpler since OHOS doesn't have message timeouts
        let mut receiver = self.main_receiver.clone();
        while let Ok(Some(runnable)) = receiver.try_pop() {
            OhosDispatcher::execute_runnable(runnable);
        }
    }

    fn handle_ohos_event(&self, event: &Event, on_finish_launching: Option<Box<dyn FnOnce()>>) {
        if matches!(event, Event::UserEvent) {
            self.run_foreground_tasks();
            return;
        }

        // Pointer left the window: reset the cursor dedup cache and restore the default
        // cursor so a later enter re-applies the hovered style (mirrors gpui_linux).
        if matches!(event, Event::Input(InputEvent::HoverEvent(false))) {
            self.last_cursor_style.set(None);
            if let Some(app) = self.app.borrow().as_ref() {
                app.set_cursor_style(0);
            }
        }

        // Handle on_finish_launching callback first, before routing to windows.
        // This is critical because windows are created INSIDE the on_finish_launching callback,
        // so we cannot depend on windows existing before calling it.
        // This is similar to how macOS handles did_finish_launching.
        // Note: The callback is only passed when event is SurfaceCreate (checked in run() method),
        // so we can safely call it here unconditionally.
        if let Some(callback) = on_finish_launching {
            log::info!("[boot] on_finish_launching callback invoked");
            callback();
        }

        // OHOS NativeAbility exposes a single XComponent surface. Broadcasting surface/input events
        // to every GPUI platform window lets stale or background windows consume the same event.
        let mut live_windows: Vec<Rc<RefCell<OhosWindow>>> = Vec::new();
        {
            let mut windows = self.windows.borrow_mut();
            windows.retain(|weak: &Weak<RefCell<OhosWindow>>| {
                if let Some(window) = weak.upgrade() {
                    live_windows.push(window);
                    true
                } else {
                    false
                }
            });
        }

        if let Some(window) = live_windows.last() {
            window.borrow().handle_event(event);
        }
    }
}

impl Clone for OhosPlatform {
    fn clone(&self) -> Self {
        Self {
            app: self.app.clone(),
            dispatcher: self.dispatcher.clone(),
            background_executor: self.background_executor.clone(),
            foreground_executor: self.foreground_executor.clone(),
            text_system: self.text_system.clone(),
            primary_display: self.primary_display.clone(),
            main_receiver: self.main_receiver.clone(),
            gpu_context: self.gpu_context.clone(),
            windows: self.windows.clone(),
            last_cursor_style: self.last_cursor_style.clone(),
            menus: self.menus.clone(),
            pending_open_with: self.pending_open_with.clone(),
            open_urls_callback: self.open_urls_callback.clone(),
            path_prompt_busy: self.path_prompt_busy.clone(),
        }
    }
}

impl OhosPlatform {
    /// Maps a GPUI cursor style to the OHOS Input_PointerStyle value.
    fn cursor_style_to_pointer_style(style: CursorStyle) -> i32 {
        // OHOS Input_PointerStyle values.
        const POINTER_STYLE_DEFAULT: i32 = 0;
        const POINTER_STYLE_EAST: i32 = 1;
        const POINTER_STYLE_WEST: i32 = 2;
        const POINTER_STYLE_SOUTH: i32 = 3;
        const POINTER_STYLE_NORTH: i32 = 4;
        const POINTER_STYLE_WEST_EAST: i32 = 5;
        const POINTER_STYLE_NORTH_SOUTH: i32 = 6;
        const POINTER_STYLE_NORTH_EAST_SOUTH_WEST: i32 = 11;
        const POINTER_STYLE_NORTH_WEST_SOUTH_EAST: i32 = 12;
        const POINTER_STYLE_CROSS: i32 = 13;
        const POINTER_STYLE_CURSOR_COPY: i32 = 14;
        const POINTER_STYLE_CURSOR_FORBID: i32 = 15;
        const POINTER_STYLE_HAND_GRABBING: i32 = 17;
        const POINTER_STYLE_HAND_OPEN: i32 = 18;
        const POINTER_STYLE_HAND_POINTING: i32 = 19;
        const POINTER_STYLE_RESIZE_LEFT_RIGHT: i32 = 22;
        const POINTER_STYLE_RESIZE_UP_DOWN: i32 = 23;
        const POINTER_STYLE_TEXT_CURSOR: i32 = 26;
        const POINTER_STYLE_HORIZONTAL_TEXT_CURSOR: i32 = 39;
        match style {
            CursorStyle::Arrow => POINTER_STYLE_DEFAULT,
            CursorStyle::IBeam => POINTER_STYLE_TEXT_CURSOR,
            CursorStyle::IBeamCursorForVerticalLayout => POINTER_STYLE_HORIZONTAL_TEXT_CURSOR,
            CursorStyle::Crosshair => POINTER_STYLE_CROSS,
            CursorStyle::ClosedHand => POINTER_STYLE_HAND_GRABBING,
            CursorStyle::OpenHand => POINTER_STYLE_HAND_OPEN,
            CursorStyle::PointingHand => POINTER_STYLE_HAND_POINTING,
            CursorStyle::ResizeLeft => POINTER_STYLE_WEST,
            CursorStyle::ResizeRight => POINTER_STYLE_EAST,
            CursorStyle::ResizeLeftRight => POINTER_STYLE_WEST_EAST,
            CursorStyle::ResizeUp => POINTER_STYLE_NORTH,
            CursorStyle::ResizeDown => POINTER_STYLE_SOUTH,
            CursorStyle::ResizeUpDown => POINTER_STYLE_NORTH_SOUTH,
            CursorStyle::ResizeUpLeftDownRight => POINTER_STYLE_NORTH_WEST_SOUTH_EAST,
            CursorStyle::ResizeUpRightDownLeft => POINTER_STYLE_NORTH_EAST_SOUTH_WEST,
            CursorStyle::ResizeColumn => POINTER_STYLE_RESIZE_LEFT_RIGHT,
            CursorStyle::ResizeRow => POINTER_STYLE_RESIZE_UP_DOWN,
            CursorStyle::OperationNotAllowed => POINTER_STYLE_CURSOR_FORBID,
            CursorStyle::DragCopy => POINTER_STYLE_CURSOR_COPY,
            CursorStyle::DragLink | CursorStyle::ContextualMenu => POINTER_STYLE_DEFAULT,
        }
    }
}

impl Platform for OhosPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        log::info!("[boot] OhosPlatform::run entered");
        let platform = self.clone();
        let on_finish = Rc::new(RefCell::new(Some(on_finish_launching)));
        if let Some(app) = self.app.borrow().clone() {
            let on_finish_clone = on_finish.clone();
            app.run_loop(move |event: Event| {
                // Only take on_finish_launching when we receive SurfaceCreate event
                let callback = if matches!(event, Event::SurfaceCreate { .. }) {
                    on_finish_clone.borrow_mut().take()
                } else {
                    None
                };
                platform.handle_ohos_event(&event, callback);
            });
        } else {
            warn!("platform run_loop not started because app is not set");
        }
    }

    fn quit(&self) {
        // OHOS app exit is managed by the system (Ability lifecycle); OpenHarmonyApp has no exit method.
        // Here we only record the quit intent; actual termination is left to the system. This is consistent with on_quit's "Handled by OpenHarmonyApp lifecycle".
        log::warn!("quit requested on OHOS; app exit is managed by the system");
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {
        // Not supported on OHOS
    }

    fn activate(&self, _ignoring_other_apps: bool) {
        // Not supported on OHOS
    }

    fn hide_cursor_until_mouse_moves(&self) {
        // Not supported on OHOS
    }

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn hide(&self) {
        // Not supported on OHOS
    }

    fn hide_other_apps(&self) {
        // Not supported on OHOS
    }

    fn unhide_other_apps(&self) {
        // Not supported on OHOS
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        if let Some(display) = self.primary_display.borrow().as_ref() {
            vec![Rc::new(display.clone()) as Rc<dyn PlatformDisplay>]
        } else {
            vec![]
        }
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.primary_display
            .borrow()
            .as_ref()
            .map(|d| Rc::new(d.clone()) as Rc<dyn PlatformDisplay>)
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        let mut active_window = None;
        let mut windows = self.windows.borrow_mut();
        windows.retain(|weak| {
            let Some(window) = weak.upgrade() else {
                return false;
            };

            active_window = Some(window.borrow().handle());
            true
        });
        active_window
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        let mut window_stack = Vec::new();
        let mut windows = self.windows.borrow_mut();
        windows.retain(|weak| {
            let Some(window) = weak.upgrade() else {
                return false;
            };

            window_stack.push(window.borrow().handle());
            true
        });
        window_stack.reverse();
        Some(window_stack)
    }

    fn is_screen_capture_supported(&self) -> bool {
        false
    }

    fn screen_capture_sources(
        &self,
    ) -> oneshot::Receiver<GpuiResult<Vec<Rc<dyn crate::ScreenCaptureSource>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow::anyhow!("Screen capture not supported on OHOS")))
            .ok();
        rx
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        log::info!("[boot] open_window for handle {:?}", handle);
        // OHOS exposes a single XComponent surface; the main window already owns it.
        // A second GPUI window (e.g. the settings window) cannot configure its own
        // wgpu surface and would abort in wgpu's default error handler, so refuse
        // extra windows up front.
        if self.windows.borrow().iter().any(|weak| weak.upgrade().is_some()) {
            anyhow::bail!("OHOS supports a single window; cannot open a second window");
        }
        if self.app.borrow().is_some() {
            let window = OhosWindow::new(
                self.app.clone(),
                handle,
                options,
                self.gpu_context.clone(),
                self.foreground_executor.clone(),
            )?;

            // GPUI fetches sprite_atlas during window initialization and caches it.
            // Renderer must be ready at open_window time to avoid caching a broken atlas.
            window.initialize_renderer()?;
            window.register_platform_event_handlers();

            let window = Rc::new(RefCell::new(window));
            self.windows.borrow_mut().push(Rc::downgrade(&window));
            self.flush_pending_open_with(&window);
            Ok(Box::new(super::window::OhosWindowHandle::new(window)))
        } else {
            Err(anyhow::anyhow!("OpenHarmonyApp not set"))
        }
    }

    fn window_appearance(&self) -> WindowAppearance {
        let color_mode = self
            .app
            .borrow()
            .as_ref()
            .map(|app| app.config().color_mode)
            .unwrap_or(ColorMode::NoSet);
        appearance_from_mode(color_mode)
    }

    fn open_url(&self, url: &str) {
        let url = url.to_string();
        if let Some(app) = self.app.borrow().clone() {
            self.foreground_executor.spawn(async move {
                if let Err(e) = app.open_url(url).await {
                    warn!("open_url failed: {e}");
                }
            })
            .detach();
        }
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        *self.open_urls_callback.borrow_mut() = Some(callback);
        self.flush_pending_open_with_urls();
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "URL scheme registration not supported on OHOS"
        )))
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        let Some(app) = self.app.borrow().clone() else {
            tx.send(Ok(None)).ok();
            return rx;
        };
        if self.path_prompt_busy.replace(true) {
            tx.send(Ok(None)).ok();
            return rx;
        }
        let busy = self.path_prompt_busy.clone();
        // GPUI PathPromptOptions -> OHOS dialog mapping:
        //   directories=true -> open-folder picker (DocumentSelectMode.FOLDER)
        //   directories=false -> open-file picker (DocumentSelectMode.FILE)
        // `allow_many` mirrors `multiple` for files only: folder dialogs always ask for a single
        // directory, matching the setup page's picker (the ArkTS plugin honours the flags it gets).
        let dialog_type = if options.directories {
            dialog_type::OPEN_FOLDER
        } else {
            dialog_type::OPEN_FILE
        };
        let allow_many = options.multiple && !options.directories;
        let dialog_options = FileDialogOptions::new(dialog_type).allow_many(allow_many);
        self.foreground_executor.spawn(async move {
            match app.show_file_dialog(dialog_options).await {
                Ok(response) => {
                    // gpui contract: a cancelled dialog (empty selection)
                    // relays None, not an empty vec, so callers distinguish
                    // "cancel" from "chose nothing".
                    if response.files.is_empty() {
                        tx.send(Ok(None)).ok();
                    } else {
                        // path_from_uri persists each URI authorization internally.
                        let paths = response
                            .files
                            .iter()
                            .filter_map(|uri| path_from_uri(uri))
                            .collect::<Vec<_>>();
                        tx.send(Ok(Some(paths))).ok();
                    }
                }
                Err(error) => {
                    tx.send(Err(anyhow::anyhow!("file dialog failed: {error}"))).ok();
                }
            }
            busy.set(false);
        })
        .detach();
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &std::path::Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        let Some(app) = self.app.borrow().clone() else {
            tx.send(Ok(None)).ok();
            return rx;
        };
        if self.path_prompt_busy.replace(true) {
            tx.send(Ok(None)).ok();
            return rx;
        }
        let busy = self.path_prompt_busy.clone();
        let mut dialog_options = FileDialogOptions::new(dialog_type::SAVE_FILE);
        if let Some(name) = suggested_name {
            dialog_options = dialog_options.default_location(name);
        }
        self.foreground_executor.spawn(async move {
            match app.show_file_dialog(dialog_options).await {
                Ok(response) => {
                    let path = response.files.first().and_then(|uri| path_from_uri(uri));
                    tx.send(Ok(path)).ok();
                }
                Err(error) => {
                    tx.send(Err(anyhow::anyhow!("save dialog failed: {error}"))).ok();
                }
            }
            busy.set(false);
        })
        .detach();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &std::path::Path) {
        // Not supported on OHOS
    }

    fn open_with_system(&self, _path: &std::path::Path) {
        // Not supported on OHOS
    }

    fn on_quit(&self, _callback: Box<dyn FnMut()>) {
        // Handled by OpenHarmonyApp lifecycle
    }

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {
        // Not supported on OHOS
    }

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {
        // System wake is managed by OpenHarmonyApp lifecycle.
    }

    fn set_menus(&self, menus: Vec<Menu>, _keymap: &Keymap) {
        *self.menus.borrow_mut() = menus.into_iter().map(|menu| menu.owned()).collect();
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        Some(self.menus.borrow().clone())
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        // Not supported on OHOS
    }

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {
        // Not supported on OHOS
    }

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {
        // Not supported on OHOS
    }

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        // Not supported on OHOS
    }

    fn compositor_name(&self) -> &'static str {
        "OHOS"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow::anyhow!("app_path not available on OHOS"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "path_for_auxiliary_executable not available on OHOS"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        let pointer_style = Self::cursor_style_to_pointer_style(style);
        // Dedup: mouse hover switches the cursor frequently; skip the system call
        // when the style is unchanged.
        if self.last_cursor_style.get() == Some(pointer_style) {
            return;
        }
        self.last_cursor_style.set(Some(pointer_style));
        let app_ref = self.app.borrow();
        let Some(app) = app_ref.as_ref() else {
            return;
        };
        app.set_cursor_style(pointer_style);
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        openharmony_ability::read_text().map(ClipboardItem::new_string)
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            openharmony_ability::write_text(&text);
        }
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "Credential storage not supported on OHOS"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "Credential deletion not supported on OHOS"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(super::keyboard::OhosKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(super::keyboard::OhosKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {
        // Not supported on OHOS
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn read_from_primary(&self) -> Option<ClipboardItem> {
        None
    }

    fn write_to_primary(&self, _item: ClipboardItem) {}
}
