# OHOS 鼠标光标样式不生效（Platform::set_cursor_style 链路修复）

## 问题描述

OHOS 移植版（zcoder）鼠标光标样式完全不生效：鼠标悬停到文本区域、按钮、可拖拽边缘时，光标始终保持系统默认箭头，`Platform::set_cursor_style` 从未真正驱动光标变化。

## 问题表现

- 光标一直是默认箭头，不随 hover 元素变化（文本区域应为 IBeam、按钮应为手型）
- `Platform::set_cursor_style` 是桩函数（未实现），GPUI 调用被静默忽略
- 排查日志显示 `reset_cursor_style` 从未执行到 `cx.platform.set_cursor_style(...)` 分支
- windowId 无法通过纯 NDK 获取（探针实测：`OH_Input_SetPointerStyle(-1, style)` → 401 参数错误；`(0, style)` → 返回 0 但光标不变，0 不是真实窗口）

## 问题原因

三层原因叠加：

1. **`Platform::set_cursor_style` 未实现**：`OhosPlatform` 没有实现 GPUI `Platform` trait 的 `set_cursor_style`，导致调用被静默丢弃。

2. **`OhosWindow::is_hovered()` 硬编码返回 `false`**：GPUI 的 `Window::reset_cursor_style` 用 `if self.is_window_hovered()` 作为前置条件，而 `is_window_hovered()` 在 OHOS（target_os=linux）走 `self.hovered.get()`，初始值来自 `platform_window.is_hovered()`。OHOS 返回 `false` → hovered 恒为 false → `reset_cursor_style` 永不调用 `set_cursor_style`。这是光标不变化的直接根因。

3. **windowId 只能从 ArkTS 获取**：`OH_Input_SetPointerStyle(window_id, style)`（libohinput.so，API 22+）需要真实 windowId。探针实测：
   - `OH_WindowManager_GetAllMainWindowInfo` → 201 权限拒绝（需 `CUSTOM_SCREEN_CAPTURE` 系统权限）
   - 纯 NDK 无任何公开 API 暴露真实 windowId
   - windowId 只能通过 ArkTS `window.getWindowProperties().id` 获取

## 解决方案

**方案选型**：windowId 必须经 ArkTS 获取 → 采用 **openharmony-ability 插件模式**（`BridgePlugin`），ArkTS 侧一次性传 windowId，之后每次改光标都是 Rust 直接系统调用。

1. **cursor 插件**：
   - ArkTS `CursorPlugin.onInstall`（requires window-stage）获取 `getWindowProperties().id`，经 `invokeNativeSync("window-id")` 发给 Rust
   - Rust `CursorBridgePlugin::on_main_thread_event` 解码 i32 存入 `WINDOW_ID` 全局（`RwLock<Option<i32>>`）
   - `CursorExt::set_cursor_style(style)` 读 `WINDOW_ID` → `OH_Input_SetPointerStyle(window_id, style)`，失败记 error

2. **平台层 `set_cursor_style` 实现**：
   - `OhosPlatform::set_cursor_style`：`cursor_style_to_pointer_style` 映射 21 个 `CursorStyle` → `Input_PointerStyle`，`last_cursor_style` 去重（hover 高频切换，跳过相同系统调用）→ `CursorExt::set_cursor_style`

3. **hover 状态修复**（根因 2）：
   - `OhosWindow::is_hovered()` 返回 `true`（OHOS 单窗口，鼠标事件只发给本窗口，恒 hover）
   - **注册原生 `DispatchHoverEvent`**：`xcomponent.rs` 注册 `on_hover_event` → `Event::Input(HoverEvent(is_hover))` → `handle_hover_event` → `hover_status_change(is_hover)`（仿 Linux X11 的 `set_hovered` on XinputEnter/Leave）
   - 鼠标离开窗口时 `HoverEvent(false)` 触发 `reset_cursor_style` 不再生效 + 平台层恢复默认光标（pointer_style=0）

**关键代码（before/after）**：
```rust
// before: OhosWindow::is_hovered 硬编码 false
fn is_hovered(&self) -> bool { false }

// after: 单窗口恒 hover，由 DispatchHoverEvent 驱动后续翻转
fn is_hovered(&self) -> bool { true }
```

## 修改文件

- `crates/gpui_ohos/src/ohos/platform.rs` — 实现 `set_cursor_style`（含 `cursor_style_to_pointer_style` 映射、`last_cursor_style` 去重）；`register_plugins` 注册 CursorBridgePlugin；`handle_ohos_event` 处理 `HoverEvent(false)` 恢复默认光标
- `crates/gpui_ohos/src/ohos/window.rs` — `is_hovered()` 返回 true；`handle_input_event` 加 HoverEvent 分支 → `handle_hover_event` 触发 `hover_status_change`；删除 `handle_mouse_input` 里每次事件的模拟 `hover_status_change(true)`（改用原生事件驱动）
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/src/lib.rs` — 新建：`CursorBridgePlugin`（window-id 事件）、`CursorExt::set_cursor_style` → `OH_Input_SetPointerStyle`
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/Cargo.toml` — 新建：插件 crate 依赖
- `crates/gpui_ohos/depend/openharmony-ability/plugins/cursor/` — 新建 ArkTS 插件（CursorPlugin.ets、index.ets、oh-package.json5、module.json5 等），onInstall 取 windowId 并传给 Rust
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/input/mod.rs` — `InputEvent` 枚举加 `HoverEvent(bool)` 变体
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs` — 注册 `on_hover_event`（DispatchHoverEvent → event loop），仿 `on_mouse_event`
- `crates/gpui_ohos/Cargo.toml` — 加 `openharmony-ability-plugin-cursor` 依赖
- `crates/gpui_ohos/depend/openharmony-ability/Cargo.toml` — workspace 加 plugin-cursor
- `hap/entry/oh-package.json5` — 加 `@ohos-rs/ability-plugin-cursor` 依赖
- `hap/entry/src/main/ets/entryability/EntryAbility.ets` — bridgePlugins 注册 CursorPlugin
- `hap/build-profile.json5` — modules 加 `plugin_cursor` 声明（缺失会导致 hvigor 00309 相对路径导入错误）
- `.claude/skills/zcoder-codemap/SKILL.md` — 添加光标变化运行路径 codemap

[[ohos-debug-lessons]]
