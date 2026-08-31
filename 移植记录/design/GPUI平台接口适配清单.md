# GPUI 平台接口适配清单（Platform trait 全量）

本文件完整列出 Zed 本地新版（1.17.0）GPUI `Platform` trait 及关联 trait 的全部接口与用途，标注每个接口在 OHOS 平台（`gpui_ohos` crate）的适配状态。依据：`crates/gpui/src/platform.rs`（2920 行，已逐行审阅）与 `crates/gpui_ohos/src/ohos/*`。

适配状态标记：
- ✅ 已实现：`gpui_ohos` 已实现，且可行。
- 🔶 降级：`gpui_ohos` 已实现但为降级（返回 None/Err/空，显式降级）。
- ⬜ 默认空：未实现，走 trait 默认空实现（功能暂时缺失，待补）。
- 🔴 缺失需补：影响 Zed 核心功能，必须补。

## 一、`trait Platform`（`platform.rs` 第 125-329 行）

平台级能力，`OhosPlatform` 实现。

- `background_executor(&self) -> BackgroundExecutor`：返回后台执行器。✅ 已实现（`OhosDispatcher`）。
- `foreground_executor(&self) -> ForegroundExecutor`：返回前台执行器。✅ 已实现。
- `text_system(&self) -> Arc<dyn PlatformTextSystem>`：返回文本系统。✅ 已实现（`OhosTextSystem`）。
- `run(&self, on_finish_launching)`：启动应用，OHOS 下不阻塞，注册 `OpenHarmonyApp::run_loop` 回调。✅ 已实现（`platform.rs` 第 166-183 行）。
- `quit(&self)`：退出应用。✅ 已实现（`app.exit(0)`）。
- `restart(&self, binary_path)`：重启。🔶 已实现为 no-op（"Not supported on OHOS"）。
- `activate(&self, ignoring_other_apps)`：激活应用。🔶 no-op。
- `hide(&self)` / `hide_other_apps(&self)` / `unhide_other_apps(&self)`：隐藏/隐藏其他/显示其他。🔶 均 no-op。
- `displays(&self) -> Vec<Rc<dyn PlatformDisplay>>`：显示器列表。✅ 已实现（单显示器 `OhosDisplay`）。
- `primary_display(&self) -> Option<Rc<dyn PlatformDisplay>>`：主显示器。✅ 已实现。
- `active_window(&self) -> Option<AnyWindowHandle>`：当前激活窗口。⬜ 返回 None（OHOS 单窗口，可由 Zed 管理）。
- `window_stack(&self)`：窗口栈。⬜ 返回 None。
- `is_screen_capture_supported(&self)`：屏幕捕获支持。✅ 返回 false。
- `screen_capture_sources(&self)`：屏幕捕获源。✅ 返回 Err（不支持）。
- `open_window(&self, handle, options) -> Result<Box<dyn PlatformWindow>>`：创建窗口。✅ 已实现（`OhosWindow` + `OhosWindowHandle`）。
- `window_appearance(&self) -> WindowAppearance`：窗口外观（亮/暗）。🔶 返回 Light（未跟随系统）。
- `set_window_appearance(&self, _appearance)`：覆盖外观。⬜ 默认空。
- `button_layout(&self)`：窗口按钮布局。⬜ 默认 None。
- `open_url(&self, url)`：打开 URL。🔶 no-op（Zed 打开链接需补，走 ArkTS `openUrl`）。
- `on_open_urls(&self, callback)`：注册 URL 打开回调。🔶 no-op。
- `register_url_scheme(&self, url)`：注册 URL scheme。🔶 返回 Err。
- `prompt_for_paths(&self, options)`：文件选择器（打开文件/目录）。🔶 返回 Ok(None)（未实现，Zed 打开文件需补，参考 warp `OhosFilePicker`）。
- `prompt_for_new_path(&self, directory, name)`：新建文件对话框。🔶 返回 Ok(None)。
- `can_select_mixed_files_and_dirs(&self)`：是否可选混合文件+目录。✅ 返回 false。
- `reveal_path(&self, path)`：在文件管理器中显示路径。🔶 no-op。
- `open_with_system(&self, path)`：用系统应用打开。🔶 no-op。
- `on_quit(&self, callback)`：注册退出回调。🔶 no-op（OHOS 生命周期由 `OpenHarmonyApp` 管理）。
- `on_reopen(&self, callback)`：重新打开回调。🔶 no-op。
- `on_app_lifecycle(&self, callback)`：移动生命周期回调（Active/Inactive/Background/Foreground）。⬜ 默认空（**需补**：把 `Event::Resume/Pause/Stop` 映射到 `AppLifecyclePhase`）。
- `on_memory_warning(&self, callback)`：内存告警。⬜ 默认空（OHOS `Event::LowMemory` 可映射）。
- `gestures(&self) -> Option<Rc<dyn PlatformGestures>>`：平台手势识别。⬜ 默认 None（用 GPUI 便携识别器）。
- `set_menus(&self, menus, keymap)`：应用菜单。🔶 no-op。
- `get_menus(&self)`：获取菜单。⬜ 返回 None。
- `set_dock_menu(&self, menu, keymap)`：Dock 菜单。🔶 no-op。
- `perform_dock_menu_action(&self, action)`：执行 Dock 菜单动作。⬜ 默认空。
- `add_recent_document(&self, path)`：加入最近文档。⬜ 默认空。
- `update_jump_list(&self, menus, entries)`：Windows 跳转列表。⬜ 返回空 Task。
- `on_app_menu_action(&self, callback)` / `on_will_open_app_menu` / `on_validate_app_menu_command`：菜单回调。🔶 均 no-op。
- `thermal_state(&self)` / `on_thermal_state_change`：热状态。✅ 返回 Nominal / ⬜ 默认空。
- `set_app_identity(&self, identifier, name)`：进程身份。⬜ 默认空。
- `show_system_notification(&self, notification)`：系统通知。⬜ 默认空（**需补**：走 NAPI 桥接 ArkTS 通知，参考 warp `service_notification.cpp`）。
- `dismiss_system_notification(&self, tag)`：撤销通知。⬜ 默认空。
- `on_system_notification_response(&self, callback)`：通知响应回调。⬜ 默认空。
- `compositor_name(&self)`：合成器名。✅ 返回 "OHOS"。
- `app_path(&self)`：应用路径。🔶 返回 Err（OHOS 沙箱无此概念）。
- `path_for_auxiliary_executable(&self, name)`：辅助可执行文件路径。🔶 返回 Err。
- `set_cursor_style(&self, style)`：光标样式。🔶 no-op（OHOS 光标由系统管理，或走 NAPI 光标控制器）。
- `hide_cursor_until_mouse_moves(&self)` / `is_cursor_visible(&self)`：光标隐藏/可见。⬜ 默认空 / 默认实现。
- `should_auto_hide_scrollbars(&self)`：自动隐藏滚动条。✅ 返回 false。
- `read_from_clipboard(&self) -> Option<ClipboardItem>`：读剪贴板。🔶 返回 None（**需补**：读受限，PasteButton 或权限，参考 warp `service_clipboard.cpp`）。
- `write_to_clipboard(&self, item)`：写剪贴板。🔶 no-op（**需补**：写不需要权限，应实现）。
- `read_from_primary(&self)` / `write_to_primary`：Linux 主剪贴板。⬜ 已用 `not(target_env = "ohos")` 排除，OHOS 编译不涉及。
- `write_credentials(&self, ...)` / `read_credentials` / `delete_credentials`：凭据存储。🔶 返回 Err/None（OHOS 无 Keychain）。
- `keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout>`：键盘布局。✅ 已实现（`OhosKeyboardLayout`）。
- `keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper>`：键盘映射。🔶 已实现但为 no-op 透传（**需补**：OHOS 键码 → GPUI Keystroke）。
- `on_keyboard_layout_change(&self, callback)`：布局变化回调。🔶 no-op。

## 二、`trait PlatformWindow`（`platform.rs` 第 804-973 行）

窗口级能力，`OhosWindowHandle` 实现（实现于 `crates/gpui_ohos/src/ohos/window.rs`）。

- `bounds(&self) -> Bounds<Pixels>`：窗口边界。✅ 已实现（从 `content_rect` 换算）。
- `is_maximized(&self)`：是否最大化。✅ 返回 false。
- `window_bounds(&self)`：窗口边界状态。✅ 返回 Windowed。
- `content_size(&self)`：内容尺寸。✅ 已实现（`effective_content_size`，减键盘遮挡）。
- `resize(&mut self, size)`：调整尺寸。✅ 已实现（更新 bounds）。
- `scale_factor(&self)`：缩放因子。✅ 已实现（`app.scale()`）。
- `appearance(&self)`：外观。🔶 返回 Light。
- `display(&self)`：所属显示器。✅ 已实现（`OhosDisplay`）。
- `mouse_position(&self)`：鼠标位置。🔶 返回 (0,0)。
- `modifiers(&self)`：修饰键状态。🔶 返回默认（无修饰键状态维护，**需补**，参考 warp 手动维护修饰键）。
- `capslock(&self)`：CapsLock 状态。🔶 返回默认。
- `set_input_handler(&mut self, handler)` / `take_input_handler`：输入处理器。✅ 已实现。
- `prompt(&self, level, msg, detail, answers)`：对话框。🔶 返回 None（未实现）。
- `activate(&self)`：激活窗口。🔶 no-op。
- `request_attention(&self)`：请求注意。⬜ 默认空。
- `is_active(&self)`：是否激活。✅ 返回 true。
- `is_hovered(&self)`：是否悬停。✅ 返回 false。
- `background_appearance(&self)` / `set_background_appearance`：背景外观。✅ Opaque / 🔶 no-op。
- `set_title(&mut self, title)`：设置标题。🔶 no-op。
- `minimize(&self)` / `zoom(&self)` / `toggle_fullscreen(&self)` / `is_fullscreen(&self)`：窗口控制。🔶 均 no-op / false。
- `frame_waker(&self)`：帧唤醒。⬜ 默认 None。
- `on_request_frame(&self, callback)`：请求帧回调。✅ 已实现（`Event::WindowRedraw` 触发）。
- `on_input(&self, callback)`：输入回调。✅ 已实现。
- `on_active_status_change(&self, callback)`：激活状态变化。✅ 已实现（`GainedFocus`/`LostFocus`）。
- `on_hover_status_change(&self, callback)`：悬停变化。⬜ 默认空。
- `on_resize(&self, callback)`：尺寸变化。✅ 已实现。
- `on_moved(&self, callback)`：移动变化。⬜ 默认空。
- `on_should_close(&self, callback)`：关闭前询问。✅ 已实现（`WindowDestroy`）。
- `on_hit_test_window_control(&self, callback)`：命中测试窗口控件。⬜ 默认空。
- `on_close(&self, callback)`：关闭回调。✅ 已实现。
- `on_appearance_changed(&self, callback)`：外观变化。✅ 已实现。
- `on_button_layout_changed(&self, callback)`：按钮布局变化。⬜ 默认空。
- `draw(&self, scene)`：绘制场景。✅ 已实现（`WgpuRenderer`）。
- `completed_frame(&self)`：帧完成。✅ 已实现。
- `sprite_atlas(&self)`：精灵图集。✅ 已实现（`WgpuAtlas`）。
- `is_subpixel_rendering_supported(&self)`：亚像素渲染。✅ 返回 false。
- `update_ime_position(&self, bounds)`：IME 光标位置。🔶 no-op（**需补**：光标上报 ArkTS 定位候选框）。
- 移动端方法（本地新版 trait 新增，均有默认空实现，`OhosWindow` 未 override）：
  - `insets(&self) -> WindowInsets`：安全区/键盘遮挡。⬜ 默认空（`OhosWindow` 有独立的 `keyboard_inset_for_overlap` 逻辑，可迁移到 insets）。
  - `on_insets_changed(&self, callback)`：insets 变化回调。⬜ 默认空。
  - `set_back_handler(&self, callback)`：系统返回键。⬜ 默认空（OHOS 有 `on_back_press_intercept` 可接）。
  - `show_soft_keyboard(&self)` / `hide_soft_keyboard`：软键盘显隐。⬜ 默认空（`OhosWindow` 有 `show_keyboard_if_needed`/`hide_keyboard_if_needed`，应 override）。
  - `text_input_state_changed(&self, change)`：输入状态变化。⬜ 默认空。

## 三、`trait PlatformDisplay`（`platform.rs` 第 332-360 行）

显示器能力，`OhosDisplay` 实现（`crates/gpui_ohos/src/ohos/display.rs`）。

- `id(&self) -> DisplayId`：显示器 ID。✅ 返回 0。
- `uuid(&self) -> Result<Uuid>`：稳定 UUID。✅ 返回固定值。
- `bounds(&self) -> Bounds<Pixels>`：边界。✅ 从 `app.content_rect()` 换算。
- `visible_bounds(&self)`：可见边界。✅ 返回 bounds（OHOS 无任务栏差异）。

## 四、`trait PlatformTextSystem`（`platform.rs` 第 1056-1087 行）

文本系统能力，`OhosTextSystem` 实现（`crates/gpui_ohos/src/ohos/text_system.rs`）。

- `add_fonts(&self, fonts)`：添加字体字节。✅ 已实现（fontdb `load_font_data`）。
- `all_font_names(&self)`：全部字体名。✅ 已实现（fontdb faces）。
- `font_id(&self, font)`：字体 ID。✅ 已实现（font-kit 匹配打分）。
- `font_metrics(&self, font_id)`：字体度量。✅ 已实现（swash metrics）。
- `typographic_bounds(&self, font_id, glyph_id)`：字形排版边界。✅ 已实现。
- `advance(&self, font_id, glyph_id)`：字形步进。✅ 已实现。
- `glyph_for_char(&self, font_id, ch)`：字符→字形。✅ 已实现（swash charmap）。
- `glyph_raster_bounds(&self, params)`：字形光栅边界。✅ 已实现（SwashCache）。
- `rasterize_glyph(&self, params, raster_bounds)`：字形光栅化。✅ 已实现（SwashCache，含 emoji BGRA 交换）。
- `layout_line(&self, text, font_size, runs)`：行排版。✅ 已实现（cosmic-text ShapeLine）。
- `recommended_rendering_mode(&self, font_id, font_size)`：渲染模式。✅ 返回 Grayscale。
- `glyph_dilation_for_color(&self, color)`：字形膨胀。⬜ 默认 0。

## 五、`trait PlatformDispatcher`（`platform.rs` 第 1013-1053 行）

调度器能力，`OhosDispatcher` 实现（`crates/gpui_ohos/src/ohos/dispatcher.rs`）。

- `is_main_thread(&self)`：是否主线程。✅ 已实现。
- `dispatch(&self, runnable, priority)`：后台执行。✅ 已实现（`std::thread::spawn`，**注意**：后台任务若触 NAPI 有 SIGABRT 风险，需确认 TSFN 封装）。
- `dispatch_on_main_thread(&self, runnable, priority)`：主线程执行。✅ 已实现（进 `PriorityQueueSender`，由 run_loop 消费）。
- `dispatch_after(&self, duration, runnable)`：延迟执行。✅ 已实现（Condvar + BinaryHeap 定时器堆）。
- `dispatch_on_main_thread_when_idle`：空闲时主线程执行。⬜ 默认实现。
- `idle_time_remaining(&self)`：空闲剩余时间。⬜ 默认 None。
- `spawn_realtime(&self, f)`：实时线程。✅ 已实现（`thread::spawn`）。
- `now(&self) -> Instant`：当前时间。✅ 已实现。
- `increase_timer_resolution`：提高定时器精度。⬜ 默认空。

## 六、`trait PlatformAtlas`（`platform.rs` 第 1307-1320 行）

图集能力，`WgpuAtlas` 实现（`crates/gpui_ohos/src/ohos/wgpu_atlas.rs`）。

- `get_or_insert_with(&self, key, build)`：获取或插入图集块。✅ 已实现。
- `remove(&self, key)`：移除图集块。✅ 已实现。

## 七、`trait PlatformKeyboardLayout` / `PlatformKeyboardMapper`（`platform.rs` 顶部引用，定义于 `platform/keyboard.rs`）

键盘能力，`OhosKeyboardLayout` / `OhosKeyboardMapper` 实现（`crates/gpui_ohos/src/ohos/keyboard.rs`）。

- `PlatformKeyboardLayout::id(&self) -> &str`：布局 ID。✅ 返回 "ohos-default"。
- `PlatformKeyboardLayout::name(&self) -> &str`：布局名。✅ 返回 "OHOS Default"。
- `PlatformKeyboardMapper::map_key_equivalent(&self, keystroke, use_key_equivalents)`：键等价映射。🔶 透传 no-op（**需补**）。
- `PlatformKeyboardMapper::get_key_equivalents(&self)`：键等价表。🔶 返回 None。

## 八、OHOS 优先级补全建议

按「先跑通、后补能力」策略，优先级排序：

1. **键盘映射**（🔴 最高）：Zed 编辑器核心输入，`map_key_equivalent` 透传 + 无键码转换会导致按键无法用。参考 warp `ohos_keymap.rs` + `keycodes.rs`。
2. **剪贴板写**（🔴）：`write_to_clipboard` 空实现，复制功能失效。写不需要权限，应实现。
3. **IME 光标**（🔴）：`update_ime_position` no-op，中文输入候选框无法定位。参考 warp 第 13 章。
4. **软键盘**（🟡）：`show_soft_keyboard`/`hide_soft_keyboard` override 到 `OhosWindow::show_keyboard_if_needed`（逻辑已有，只是没接到 trait 方法）。
5. **文件选择器**（🟡）：`prompt_for_paths` 返回 None，打开文件/目录需补。参考 warp `OhosFilePicker`。
6. **剪贴板读**（🟡）：受限权限，PasteButton 或降级。
7. **生命周期**（🟡）：`on_app_lifecycle` 映射 `Event::Resume/Pause/Stop`。
8. **通知**（🟢）：`show_system_notification` 走 NAPI 桥接 ArkTS。
9. **返回键**（🟢）：`set_back_handler` 接 `on_back_press_intercept`。
