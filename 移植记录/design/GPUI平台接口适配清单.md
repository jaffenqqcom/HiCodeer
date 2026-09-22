# GPUI 平台接口适配清单（Platform trait 全量）

本文件完整列出 GPUI `Platform` trait 及关联 trait 的全部接口与用途，标注每个接口在 OHOS 平台
（`gpui_ohos` crate）的适配状态。

**核对基准（2026-09-22）**：`crates/gpui/src/platform.rs`（2919 行）与
`crates/gpui_ohos/src/ohos/*`。trait 的行号范围为核对时点，代码变动后会漂移；方法名与
"已 override / 走默认"的判定按当时代码逐个比对得出。

适配状态标记：
- ✅ 已实现：`gpui_ohos` 已实现，且可用。
- 🔶 降级：`gpui_ohos` 已实现但为降级（返回 None/Err/固定值、no-op、显式忽略）。
- ⬜ 默认空：未 override，走 trait 默认实现（功能缺失，待补）。
- 🔴 缺失需补：影响 Zed 核心功能，必须补。

## 一、`trait Platform`（`platform.rs:125-331`，68 个方法）

平台级能力，`OhosPlatform` 实现（`crates/gpui_ohos/src/ohos/platform.rs`，override 54 个）。

- `background_executor` / `foreground_executor` / `text_system`：✅ 返回执行器与文本系统。
- `run(on_finish_launching)`：✅ 不阻塞，注册 `OpenHarmonyApp::run_loop` 回调。
- `quit`：🔶 只记录退出意图，实际终止交给系统（OHOS Ability 生命周期），不调 `app.exit`。
- `restart` / `activate` / `hide` / `hide_other_apps` / `unhide_other_apps`：🔶 均 no-op。
- `displays` / `primary_display`：✅ 单显示器 `OhosDisplay`。
- `active_window`：✅ 从内部窗口表里挑激活窗口（不再返回 None）。
- `window_stack`：✅ 列出全部窗口。
- `is_screen_capture_supported`：✅ 返回 false。
- `screen_capture_sources`：✅ 返回 Err（不支持）。
- `open_window`：✅ 已实现，但受单 XComponent 限制——第二个 GPUI 窗口（如设置窗口）无法配置，
  见实现内注释。
- `window_appearance`：✅ **跟随系统深浅色**（读 `app.config().color_mode` → `appearance_from_mode`）。
- `set_window_appearance`：⬜ 默认空。
- `button_layout`：⬜ 默认 None。
- `open_url`：✅ 走 `app.open_url(url)`。
- `on_open_urls`：✅ 存回调并冲刷启动期挂起的 URL。
- `register_url_scheme`：🔶 返回 Err。
- `prompt_for_paths`：✅ 已实现（走 `app.show_file_dialog`；`directories` 决定 FOLDER/FILE 模式）。
- `prompt_for_new_path`：✅ 已实现（保存对话框）。
- `can_select_mixed_files_and_dirs`：✅ 返回 false。
- `reveal_path` / `open_with_system`：🔶 no-op。
- `on_quit`：🔶 no-op（生命周期归 `OpenHarmonyApp`）。
- `on_reopen`：🔶 no-op。
- `on_system_wake`：🔶 no-op（本版 trait 新增方法；系统唤醒由 `OpenHarmonyApp` 管理）。
- `on_app_lifecycle`：⬜ 默认空（**待补**：把 `Event::Resume/Pause/Stop` 映射到 `AppLifecyclePhase`）。
- `on_memory_warning`：⬜ 默认空（`Event::LowMemory` 可映射）。
- `gestures`：⬜ 默认 None（用 GPUI 便携识别器）。
- `set_menus` / `get_menus`：✅ 存入并读回菜单表。
- `set_dock_menu`：🔶 no-op。
- `perform_dock_menu_action` / `add_recent_document` / `update_jump_list`：⬜ 默认空。
- `on_app_menu_action` / `on_will_open_app_menu` / `on_validate_app_menu_command`：🔶 均 no-op。
- `thermal_state`：✅ 返回 Nominal；`on_thermal_state_change`：⬜ 空实现。
- `set_app_identity`：⬜ 默认空。
- `show_system_notification` / `dismiss_system_notification` / `on_system_notification_response`：
  ⬜ 默认空（**待补**：走 NAPI 桥接 ArkTS 通知）。
- `compositor_name`：✅ 返回 "OHOS"。
- `app_path` / `path_for_auxiliary_executable`：🔶 返回 Err。
- `set_cursor_style`：✅ 已实现（样式去重后调系统光标）。
- `hide_cursor_until_mouse_moves`：🔶 no-op；`is_cursor_visible`：✅ 返回 true。
- `should_auto_hide_scrollbars`：✅ 返回 false。
- `read_from_clipboard` / `write_to_clipboard`：✅ 已实现（`openharmony_ability::read_text` /
  `write_text`），读写都通。
- `read_from_primary` / `write_to_primary`：🔶 已 override 为 None / 空实现（Linux 主剪贴板语义，
  OHOS 无对应物）。
- `read_from_find_pasteboard` / `write_to_find_pasteboard`：⬜ 默认空（本版 trait 新增方法）。
- `write_credentials` / `read_credentials` / `delete_credentials`：🔶 Err / None / Err
  （OHOS 无 Keychain）。
- `keyboard_layout`：✅ `OhosKeyboardLayout`。
- `keyboard_mapper`：🔶 已实现但为透传 no-op（**待补**：OHOS 键码 → GPUI Keystroke）。
- `on_keyboard_layout_change`：🔶 no-op。

## 二、`trait PlatformWindow`（`platform.rs:804-976`，89 个方法）

窗口级能力，`OhosWindow` 实现（`crates/gpui_ohos/src/ohos/window.rs`，override 49 个）；
`OhosWindowHandle` 另有一层转发实现（override 41 个）。

几何与状态：
- `bounds`：✅ 从 `content_rect` 换算。
- `is_maximized`：🔶 返回 false；`window_bounds`：🔶 返回 Windowed。
- `content_size`：✅ 已实现（`effective_content_size`，扣除键盘遮挡）。
- `resize`：✅ 已实现（更新 bounds）。
- `scale_factor`：✅ 已实现（`app.scale()`）。
- `appearance`：✅ 跟随窗口 color_mode（与 `Platform::window_appearance` 同源）。
- `display`：✅ `OhosDisplay`。
- `mouse_position`：🔶 返回 (0,0)。
- `modifiers`：🔶 返回 `Modifiers::default()`（无修饰键状态维护，**待补**）。
- `capslock`：🔶 返回 `Capslock::default()`。

输入与 IME：
- `set_input_handler` / `take_input_handler`：✅ 已实现。
- `prompt`：🔶 返回 None（未实现对话框）。
- `activate`：🔶 no-op；`is_active`：🔶 返回 true。
- `is_hovered`：✅ 返回 **true**（单窗口平台，输入只会发给本窗口；返回 true 才能让 GPUI 的
  cursor-style 管线持续运行——实现内注释说明了理由）。
- `set_title`：🔶 no-op（窗口标题由系统管理）。
- `set_background_appearance`：🔶 no-op；`background_appearance`：✅ 返回 Opaque。
- `minimize` / `zoom` / `toggle_fullscreen`：🔶 均 no-op；`is_fullscreen`：🔶 返回 false。
- `update_ime_position`：✅ 已实现——缓存光标矩形并 `push_ime_cursor_rect` 上报 ArkTS 定位候选框；
  绑定状态由 `update_ime_enabled`（`window.rs:1019`）每帧 + 边沿检测决定，本路径不重复请求。
- `insets`：🔶 已 override 但返回 `WindowInsets::default()`——**有意为之**：OHOS 的键盘/系统避让
  走 `content_size` 驱动的 resize（`keyboard_inset_for_overlap` → `effective_content_size`），
  不走 insets 机制。
- `on_insets_changed`：⬜ 默认空。
- `set_back_handler` / `set_back_enabled`：⬜ 默认空（**待补**：系统返回键尚未接线）。
- `show_soft_keyboard` / `hide_soft_keyboard`：⬜ 默认空。`OhosWindow` 已有固有方法
  `show_keyboard_if_needed`（`window.rs:1040`）/ `hide_keyboard_if_needed`（`:1068`），并由
  `update_ime_enabled` 每帧调用——**能力已有，只是没接到 trait 方法上**。
- `text_input_state_changed`：⬜ 默认空。

回调注册：
- `on_request_frame`：✅ 已实现（`Event::WindowRedraw` 触发）。
- `on_input`：✅ 已实现。
- `on_active_status_change`：✅ 已实现（`GainedFocus`/`LostFocus`）。
- `on_hover_status_change`：✅ 已实现（存回调）。
- `on_resize`：✅ 已实现。
- `on_moved`：✅ 已实现（存回调）。
- `on_should_close`：✅ 已实现（`WindowDestroy`）。
- `on_hit_test_window_control`：✅ 已实现（存回调）。
- `on_close`：✅ 已实现。
- `on_appearance_changed`：✅ 已实现。
- `on_button_layout_changed`：⬜ 默认空。

绘制：
- `draw`：✅ 已实现（`WgpuRenderer`，按需惰性初始化）。
- `completed_frame`：✅ `OhosWindowHandle` 实现为转发到固有方法
  `OhosWindow::update_ime_enabled()`（每帧 IME 绑定判定）。
- `sprite_atlas`：✅ 已实现（`WgpuAtlas`）。
- `is_subpixel_rendering_supported`：🔶 返回 false。
- `gpu_specs`：✅ 从 WGPU renderer 取。
- `can_start_external_drag` / `start_external_drag`：⬜ 默认空（**待补**：拖拽尚未接线）。
- `render_to_image`：⬜ 默认空。
- `as_test`：⬜ 默认空。

窗口装饰与标签页（桌面语义，OHOS 单窗口下多数走默认）：
- `request_decorations` / `show_window_menu` / `start_window_move` / `start_window_resize`：
  🔶 均 no-op。
- `window_decorations`：🔶 返回 `Decorations::Server`。
- `set_app_id`：🔶 no-op。
- `map_window`：✅ 返回 Ok(())（OHOS 无显式映射步骤）。
- `window_controls`：✅ 返回全 false 的 `WindowControls`。
- `set_client_inset`：🔶 有意忽略（键盘避让走 content_size）。
- `set_exclusive_zone` / `set_exclusive_edge` / `set_input_region` / `inner_window_bounds` /
  `get_raw_handle`：⬜ 默认空。
- `get_title`：⬜ 默认空。
- `tabbed_windows` / `tab_bar_visible` / `set_edited` / `set_document_path` /
  `set_traffic_light_position` / `show_character_palette` / `titlebar_double_click` /
  `on_move_tab_to_new_window` / `on_merge_all_windows` / `on_select_previous_tab` /
  `on_select_next_tab` / `on_toggle_tab_bar` / `merge_all_windows` / `move_tab_to_new_window` /
  `toggle_window_tab_overview` / `set_tabbing_identifier`：⬜ 默认空（macOS/Windows 标签页语义）。
- `request_attention` / `frame_waker`：⬜ 默认 None / 空。
- `play_system_bell`：⬜ 默认空。
- `a11y_init` / `a11y_tree_update` / `a11y_update_window_bounds`：⬜ 默认空。

## 三、`trait PlatformDisplay`（`platform.rs:332-425`，5 个方法）

`OhosDisplay` 实现（`crates/gpui_ohos/src/ohos/display.rs`，override 4 个）。

- `id`：✅ 返回 0。
- `uuid`：✅ 返回固定值。
- `bounds`：✅ 从 `app.content_rect()` 换算。
- `visible_bounds`：✅ 返回 bounds（OHOS 无任务栏差异）。
- `default_bounds`：⬜ 默认实现（未 override）。

## 四、`trait PlatformTextSystem`（`platform.rs:1056` 起，12 个方法）

`OhosTextSystem` 实现（`crates/gpui_ohos/src/ohos/text_system.rs`，override 11 个）。

- `add_fonts`：✅（fontdb `load_font_data`）。
- `all_font_names`：✅（fontdb faces）。
- `font_id`：✅（font-kit 匹配打分）。
- `font_metrics`：✅（swash metrics）。
- `typographic_bounds` / `advance` / `glyph_for_char`：✅ 已实现（swash charmap）。
- `glyph_raster_bounds` / `rasterize_glyph`：✅ 已实现（SwashCache，含 emoji BGRA 交换）。
- `layout_line`：✅ 已实现（cosmic-text ShapeLine）。
- `recommended_rendering_mode`：✅ 返回 Grayscale。
- `glyph_dilation_for_color`：⬜ 默认 0。

## 五、`trait PlatformDispatcher`（`platform.rs:1013-1055`，11 个方法）

`OhosDispatcher` 实现（`crates/gpui_ohos/src/ohos/dispatcher.rs`，override 6 个）。

- `is_main_thread`：✅ 已实现。
- `dispatch`：✅ 已实现（`std::thread::spawn`）。后台任务若触 NAPI 需确认 TSFN 封装。
- `dispatch_on_main_thread`：✅ 已实现（进 `PriorityQueueSender`，由 run_loop 消费）。
- `dispatch_after`：✅ 已实现（Condvar + BinaryHeap 定时器堆）。
- `spawn_realtime`：✅ 已实现（`thread::spawn`）。
- `now`：✅ 已实现。
- `dispatch_on_main_thread_when_idle`：⬜ 默认实现。
- `idle_time_remaining`：⬜ 默认 None。
- `increase_timer_resolution`：⬜ 默认空。
- `as_test` / `as_threaded`：⬜ 默认（仅测试/辅助路径用）。

## 六、`trait PlatformAtlas`（`platform.rs:1308` 起，2 个方法）

`WgpuAtlas` 实现（`crates/gpui_ohos/src/ohos/wgpu_atlas.rs`，override 2 个）。

- `get_or_insert_with`：✅ 已实现。
- `remove`：✅ 已实现。

## 七、键盘：`PlatformKeyboardLayout` / `PlatformKeyboardMapper`

定义在 `crates/gpui/src/platform/keyboard.rs:6` 与 `:14`（不在 `platform.rs` 内）；
`OhosKeyboardLayout` / `OhosKeyboardMapper` 实现（`crates/gpui_ohos/src/ohos/keyboard.rs`，各 override 2 个）。

- `PlatformKeyboardLayout::id`：✅ 返回 "ohos-default"。
- `PlatformKeyboardLayout::name`：✅ 返回 "OHOS Default"。
- `PlatformKeyboardMapper::map_key_equivalent`：🔶 透传 no-op（**待补**）。
- `PlatformKeyboardMapper::get_key_equivalents`：🔶 返回 None。

## 八、其它关联 trait（未逐条适配）

以下 trait 位于 `platform.rs` 内，但 `OhosPlatform` / `OhosWindow` 未提供对应实现：

- `PlatformHeadlessRenderer`（:977-1012）：`render_scene_to_image` / `render_scene` / `sprite_atlas`
  ——无头渲染路径，OHOS 未接入。
- `ScreenCaptureSource`（:426-439）/ `ScreenCaptureStream`（:440 起）：屏幕捕获，OHOS 明确不支持
  （`screen_capture_sources` 直接返回 Err）。
- `InputHandler`（:1651 起）：由 GPUI 侧统一实现，不属于平台适配面。

## 九、剩余缺口与优先级

「先跑通、后补能力」阶段已完成的项目：键盘布局、剪贴板读写、IME 光标上报、文件选择器、
系统深浅色跟随、命令执行后端。**当前仍缺的**，按影响排序：

1. **键盘映射**（🟡）：`map_key_equivalent` 仍是透传，OHOS 键码未转 GPUI Keystroke，
   会影响快捷键与部分按键。
2. **软键盘显隐接到 trait**（🟡）：能力已存在于 `OhosWindow::show_keyboard_if_needed` /
   `hide_keyboard_if_needed`，只是没 override `show_soft_keyboard` / `hide_soft_keyboard`。
3. **系统返回键**（🟡）：`set_back_handler` / `set_back_enabled` 未实现，仓库内也无对应 ArkTS
   接线点。
4. **系统通知**（🟢）：`show_system_notification` 等三个方法未实现，需 NAPI 桥接 ArkTS。
5. **应用生命周期与内存告警**（🟢）：`on_app_lifecycle` / `on_memory_warning` 未映射。
6. **拖拽外部文件**（🟢）：`can_start_external_drag` / `start_external_drag` 未实现。
7. **修饰键状态**（🟢）：`modifiers` / `capslock` 恒返回默认值。
8. **凭据存储**（🟢）：OHOS 无 Keychain，三方法固定返回 Err/None。
