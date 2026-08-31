# OHOS 物理键盘输入不可用：从"按键到达不了 Rust"到完整支持字符输入 + CapsLock

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植）在 OHOS 设备上**物理键盘输入完全不可用**：按字母/数字键，编辑器光标在闪烁但字符不上屏；方向键、回车、Backspace 等无反应；CapsLock 无效。软键盘（IME）输入正常。开发键值映射功能时发现：键盘事件在系统层被路由到 ArkTS 焦点节点（`Row/secure_field`），NDK 的 `on_key_event` 回调从不触发，按键内容到达不了 zed 处理模块。

## 问题表现

- **物理键盘按键无反应**：字符不上屏，方向键/回车/Backspace 无效果；软键盘（IME）输入正常。
- **按 Space 等非字母数字键即崩溃**：SIGABRT，栈指向 `keycodes::numpad_digit_index` 的 `panic_const_sub_overflow`（整数下溢）。
- **hilog 三段诊断全无**：`[diag] key_event native callback fired` / `handle_input_event KeyEvent` / `key_event_to_keystroke` 一条都不出现（修复焦点前）。
- **CapsLock 无效**：按 CapsLock 无反应，字母不变大写。
- 复现：物理键盘（蓝牙/USB）连接设备，打开编辑器按任意键；崩溃按 Space 必现。

## 问题原因

四个连续断点叠加，逐层修复：

**① XComponent 无焦点 → 按键进不了 Rust（第一断点）**
`XComponent::new()` 创建的节点未设置 `focusable`/`default_focus`，不参与 ArkUI 焦点系统。系统按键路由给 ArkTS 层焦点节点——DefaultXComponent 的 `Row/secure_field`（`AceFocus` 日志 `current focus node: (Row/26)`），NDK `on_key_event` 回调永不触发。此前未暴露是因为旧版无 XComponent 焦点，按键从不进入 Rust。

**② keycodes 索引函数下溢 panic（焦点修复后暴露）**
`letter_index`/`digit_index`/`numpad_digit_index` 用 `bool::then_some(急切值)`——`(raw - start)` 即使越界也求值，`u32` 减法下溢 panic。焦点修复后按键进入 Rust，按 Space（raw 值 < Numpad0）即崩。此 bug 之前被"按键到不了 Rust"掩盖。

**③ GPUI 真实按键路径不处理 key_char → 字符输不进去（第三断点）**
GPUI 的 `dispatch_key_event`（真实按键）**不调 input handler**——桌面字符输入靠系统 IME（macOS NSTextInputContext、Wayland text-input），X11 在平台层兜底：GPUI 未消费 KeyDown 且 key_char 有值时调 `input_handler.replace_text_in_range(None, key_char)`（`gpui_linux/x11/window.rs:1157-1170`）。OHOS 无系统 IME，`OhosWindow::dispatch_input` 只派发 GPUI 无兜底 → 字符丢失。

**④ capslock 状态不传递 → CapsLock 无效**
`OhosWindow::capslock()` 恒返回 `Capslock::default()`；`modifier_state` 无 capslock 位；`keycodes` 未映射 CapsLock 键、字母大小写只认 shift。

### 排查过程中的死路（重要教训）

- **误判 libnative_ability.so 未加载是根因**：用户怀疑 NativeAbility 的 so 未加载导致 XComponent 回调没注册。经查证，`libnative_ability.so` 的 C++ 源码是 DevEco 模板示例（只导出 `add` 函数），与按键桥接完全无关；HAP 里确有该 so（17KB）且能加载。假设被排除。
- **误判 focus 不是问题**：日志显示 `focus=Some(FocusId(...))`、`input_handler=true`，以为焦点正常。实际焦点在编辑器内部子节点（`view=None`），键值映射对了但 GPUI 真实按键不调 input handler——只有对比 X11/macOS 源码才定位到"字符输入靠平台层兜底/IME"。
- **零敲碎打加日志效率低**：多轮"加一行日志→编译→验证"，用户明确批评后改为一次性加全日志（GPUI dispatch_event/dispatch_key_event/key_down_up 的完整链路 + 丢弃点），才快速定位。
- **capslock 不能缓存状态**：OHOS 无 `ModifiersChanged` 系统通知（桌面 X11 有 `XkbStateNotify`），若维护 last_modifiers/capslock 状态机，失焦丢 Release 会永久卡住。必须每次按键即时重建（warp 无状态原则）。

## 解决方案

**① XComponent 焦点修复**（`xcomponent.rs` render()）：
```rust
// 修复前：XComponent::new() 后只设背景色
xcomponent_native
    .set_focusable(true)
    .map_err(|e| Error::from_reason(e.reason.to_string()))?;
xcomponent_native
    .set_default_focus(true)
    .map_err(|e| Error::from_reason(e.reason.to_string()))?;
```
修复后系统按键路由给 XComponent，NDK `on_key_event` 触发，`AceFocus` 日志从 `Row/secure_field` 变为 `XComponent/30`。

**② keycodes 下溢修复**（`then_some` → `then` 惰性求值）：
```rust
// 修复前：(start..=end).contains(&raw).then_some((raw - start) as usize)  // 越界也求值 → 下溢
// 修复后：(start..=end).contains(&raw).then(|| (raw - start) as usize)    // 惰性求值，安全
```
三个 index 函数（letter/digit/numpad_digit）同样处理。

**③ X11 式字符兜底**（`window.rs` dispatch_input，拷贝自 `gpui_linux/x11/window.rs` 适配）：
```rust
fn dispatch_input(&self, input: PlatformInput) {
    let result = Self::dispatch_input_with_callbacks(&self.callbacks, input.clone());
    // X11-compatible fallback: GPUI 未消费 KeyDown 且 key_char 有值、修饰键只含 shift → 文本输入
    if result.propagate {
        if let PlatformInput::KeyDown(event) = input {
            if event.keystroke.modifiers.is_subset_of(&Modifiers::shift()) {
                let mut handler_ref = self.input_handler.borrow_mut();
                if let Some(mut input_handler) = handler_ref.take() {
                    drop(handler_ref);
                    if let Some(key_char) = event.keystroke.key_char {
                        input_handler.replace_text_in_range(None, &key_char);
                    }
                    *self.input_handler.borrow_mut() = Some(input_handler);
                }
            }
        }
    }
}
```
功能键（方向键/回车/F1-F12）key_char 为 None 天然安全不误触发；Ctrl/Alt/Super 组合被 `is_subset_of(shift)` 拦截，快捷键走 GPUI binding。

**④ CapsLock 支持（无状态，key_event 入口集中处理）**：
- `KeyEventData` 加 `capslock: bool`，`native_callbacks.rs` 用 `OH_NativeXComponent_GetKeyEventCapsLockState` 查询填充。
- `keycodes.rs`：`key_name` 加 `KeyCode::CapsLock => "capslock"`；`key_char` 字母大小写 = `shift XOR capslock`（与 xkb 语义一致，CapsLock+Shift → 小写）。
- `window.rs` KeyEvent 分支（key_event 入口）：每次按键即时重建完整状态，先补发 `ModifiersChanged { modifiers, capslock }` 再发 KeyDown，**不缓存任何状态**。

**验证结果**（设备日志）：
- `AceFocus` 日志 `Node XComponent/30 handle KeyEvent`（焦点到 XComponent），`key_event native callback fired` 触发。
- 字符上屏（X11 兜底触发 `replace_text_in_range`），方向键/回车/Backspace 走 GPUI binding 正常。
- 不再崩溃（then 惰性求值），CapsLock 字母大写（shift XOR capslock）。
- Ctrl+S 走快捷键而非输入 's'（`is_subset_of(shift)` 拦截）。

## 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs` — ① 核心：XComponent 加 `set_focusable(true)` + `set_default_focus(true)`；清理 KEY_EVENT_NATIVE_CB_COUNT 计数器与 [diag] 日志。
- `crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/key_event.rs` — ④ `KeyEventData` 加 `capslock: bool` 字段。
- `crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs` — ④ key_event 回调查询 `OH_NativeXComponent_GetKeyEventCapsLockState` 填充 capslock；import 相应绑定。
- `crates/gpui_ohos/src/ohos/keycodes.rs` — ② 三个 index 函数 `then_some`→`then` 惰性求值修下溢；④ `key_name` 加 CapsLock 映射、`key_char` 加 capslock 参数（shift XOR capslock）、`modifiers_from_modifier_state` 改 `pub(crate)`；删 [diag] 日志。
- `crates/gpui_ohos/src/ohos/window.rs` — ③ X11 式字符兜底；④ KeyEvent 分支每次按键补发 `ModifiersChanged`（无状态）；import `ModifiersChangedEvent`；清理 dispatch_input/touch/KeyEvent 的 [diag] 日志。
- `crates/gpui/src/window.rs` — 清理 13 处 [diag] 诊断日志（dispatch_event/dispatch_key_event/key_down_up/丢弃点），恢复 Zed 原样。
- `crates/gpui/src/key_dispatch.rs` — 清理为诊断添加的 `DispatchNode::view_id()` 访问器，恢复原样。

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
