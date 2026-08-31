# OHOS 键盘长按不重复（合成 key repeat）

## 问题描述

OHOS 移植版（zcoder）物理键盘按住一个键不放时，字符只输入一次，不会像桌面端那样自动连续重复输入（如按住 Backspace 连续删除、按住字母连续输入）。

## 问题表现

- 按住任意字符键不放：只输入一次，不再重复
- 按住 Backspace / 方向键：只触发一次删除/移动
- 无崩溃、无报错，纯粹是缺少重复输入行为

## 问题原因

OHOS 的 XComponent 键盘事件 `KeyAction` 只有 `Down` / `Up` 两种，**没有 repeat 动作位**。系统层的按键重复由 ArkTS 焦点控件处理，而 zcoder 的按键经 `RegisterKeyEventCallback` 进入 Rust 后直接透传，没有合成 repeat，导致按住不重复。

## 解决方案

在 `OhosWindow` 里自研合成 key repeat（事件驱动，无轮询）：

1. **begin_key_repeat**：收到 `KeyDown` 时记录按键 code 和单调递增的 generation，在 `foreground_executor` 上 spawn 一个任务：先等 `KEY_REPEAT_DELAY`（500ms，桌面默认初始延迟），然后每 `KEY_REPEAT_INTERVAL`（33ms，约 30 cps）分发一次 `KeyDownEvent { is_held: true }`。
2. **end_key_repeat**：收到 `KeyUp`（或窗口销毁）时清空 repeat 状态，generation 校验失败使任务退出循环。
3. **修饰键排除**：`is_modifier_key` 检查 Ctrl/Shift/Alt/Meta/CapsLock/Fn，修饰键永不重复（只触发自身按键事件）。
4. **取消机制**：任务循环条件 `window_alive.get() && repeat_active(...)`——窗口销毁（`window_alive` 置位）或按键变化（generation/code 不匹配）都会立即停止 repeat，不会残留后台任务。

```rust
const KEY_REPEAT_DELAY: Duration = Duration::from_millis(500);
const KEY_REPEAT_INTERVAL: Duration = Duration::from_millis(33);

fn begin_key_repeat(&self, key_event: &KeyEventData, keystroke: Keystroke) {
    if Self::is_modifier_key(key_event.code) {
        return;
    }
    let generation = self.key_repeat.borrow().map_or(0, |s| s.generation) + 1;
    *self.key_repeat.borrow_mut() = Some(KeyRepeatState { code: key_event.code, generation });
    // ... spawn: timer(500ms) -> loop { dispatch_repeat_key_down; timer(33ms) }
}
```

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — 新增 `KeyRepeatState` 结构、`begin_key_repeat` / `end_key_repeat` / `is_modifier_key` / `repeat_active` / `dispatch_repeat_key_down`，在 `KeyDown` / `KeyUp` 处理中调用

[[ohos-debug-lessons]]
