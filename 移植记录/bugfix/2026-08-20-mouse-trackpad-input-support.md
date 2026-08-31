# OHOS 鼠标 / 触摸板输入事件支持（RegisterMouseEventCallback + UIInputEvent-Axis）

## 问题描述

OHOS 移植版（zcoder）最初只有触摸屏输入可用，鼠标（移动/点击/按键）和触摸板（双指滑动/滚轮）的输入事件无法到达 GPUI：鼠标移动不移动光标、滚轮不滚动、触控板双指滑动无响应。

## 问题表现

- 鼠标移动：光标不跟随，GPUI 收不到 `MouseMove`
- 鼠标点击：无法聚焦/点击 UI（无 `MouseDown`/`MouseUp`）
- 鼠标滚轮 / 触控板双指滑动：内容不滚动（无 `ScrollWheel`）
- 键盘可用（另有通道），触摸屏可用（`DispatchTouchEvent`），仅鼠标/触控板/滚轮失效

## 问题原因

OHOS 的 XComponent 输入事件**每种外设独立一个通道**，互不共用。移植初期只接了触摸屏（`DispatchTouchEvent`）和键盘（`RegisterKeyEventCallback`），鼠标和触控板/滚轮的两条通道没有接入：

| 外设 | XComponent 通道 | 事件 |
|---|---|---|
| 触摸屏 | `DispatchTouchEvent` | Touch（仅 `Finger` 类型；鼠标/触控板被过滤，避免误走触摸） |
| 鼠标 | `RegisterMouseEventCallback`（`DispatchMouseEvent` + `DispatchHoverEvent`） | Move / Press / Release |
| 触控板 / 鼠标滚轮 | `RegisterUIInputEventCallback`（仅支持 Axis 事件） | Axis，按 `tool_type` 区分 Touchpad / Mouse |
| 键盘 | `RegisterKeyEventCallback` | KeyDown / KeyUp |

鼠标和轴事件缺 C 回调转发、缺数据模型、缺 GPUI 分发，三条链路都要补。

## 解决方案

1. **数据模型**（`openharmony-ability/crates/ability/src/input/mod.rs`）
   - `InputEvent` 枚举加 `MouseEvent(MouseEventData)`、`AxisEvent(AxisEventData)`
   - `MouseEventData`：action（Move/Press/Release）、位置、`button_mask`、`modifiers`
   - `AxisEventData`：`scroll_vertical` / `scroll_horizontal`、`tool_type`（AxisToolType::Touchpad / Mouse）、`scroll_phase`
   - `ui_input_event_to_input_event()`：Axis 事件按 `tool_type` 区分触控板与鼠标滚轮

2. **XComponent 回调注册**（`render/xcomponent.rs`）
   - 鼠标：`xcomponent.on_mouse_event(...)` → `Event::Input(MouseEvent(data))` 入 event_loop
   - 轴：`xcomponent.on_ui_input_event(UIInputEvent::Axis, ...)` → `ui_input_event_to_input_event` 入 event_loop
   - `register_mouse_event_callback()` 统一注册（内含 DispatchMouseEvent + DispatchHoverEvent）

3. **NDK C 回调**（`ohos-xcomponent-binding/src/events/native_callbacks.rs`）
   - `on_mouse_event`：查询 `OH_NativeXComponent_GetExtraMouseEventInfo` 补 `modifiers`/`button_mask`
   - `on_ui_input_event`：转发 Axis 事件

4. **GPUI 消费**（`gpui_ohos/src/ohos/window.rs`）
   - `handle_input_event` 加 `MouseEvent` / `AxisEvent` 分支
   - `handle_mouse_input`：Move → `MouseMove`（`pressed_button` 从 `button_mask` 解析）、Press → `MouseDown`、Release → `MouseUp`
   - `handle_axis_input`：按 `tool_type` 分发——Touchpad → `ScrollDelta::Pixels`、Mouse（滚轮）→ `ScrollDelta::Lines`（每 120 单位 N 行）
   - 关键约束：**修饰键从事件即时查询，禁止缓存状态**（OHOS 无 ModifiersChanged 通知，失焦丢 Release 会卡死）

## 修改文件

- `crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs` — 鼠标 `on_mouse_event`（查 ExtraMouseEventInfo 补修饰键/button_mask）、轴 `on_ui_input_event` C 回调
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/input/mod.rs` — `InputEvent` 加 `MouseEvent`/`AxisEvent`；`MouseEventData`/`AxisEventData` 数据模型；`ui_input_event_to_input_event` 按 tool_type 区分触控板/鼠标滚轮
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs` — 注册 `on_mouse_event`（DispatchMouseEvent）、`on_ui_input_event(UIInputEvent::Axis)`；`register_mouse_event_callback`
- `crates/gpui_ohos/src/ohos/window.rs` — `handle_input_event` 消费 `MouseEvent`/`AxisEvent`；`handle_mouse_input`（Move/Down/Up）、`handle_axis_input`（滚轮/触控板，方向与速度后在本会话修正）

[[ohos-debug-lessons]]
