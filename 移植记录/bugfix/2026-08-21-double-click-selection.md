# OHOS 鼠标双击/三击不能选词（ClickTracker 统一 click_count）

## 问题描述

OHOS 移植版（zcoder）鼠标双击无法选中一个词、三击无法选中整行。桌面端双击/三击是编辑器选词/选行的基础交互。

## 问题表现

- 鼠标双击：光标不选中词语
- 鼠标三击：不选中整行
- 触摸屏双击同样无效（触摸屏已有单击，双击判定缺失）
- 无崩溃、无报错，纯粹是 `click_count` 没有计算

## 问题原因

OHOS 的输入事件模型没有桌面端的 `clickCount`（macOS）或 X11 的重复点击计数概念，鼠标 `Press` / 触摸屏 `Down` 事件直接透传，`click_count` 恒为 1，GPUI 无法区分单击/双击/三击。

## 解决方案

新增 `ClickTracker`，鼠标、触摸屏两条输入通路共享同一个计数函数（对齐 Linux 阈值）：

1. **阈值对齐 Linux**（`gpui_linux`）：`DOUBLE_CLICK_INTERVAL = 400ms`、`DOUBLE_CLICK_DISTANCE = 5px`。
2. **on_button_press**：每次按下时，若距上次 *completed* 点击在 400ms 内、按钮相同、位置在 5px 内 → `click_count + 1`（saturating，支持三击/四击），否则重置为 1。当前 count 供匹配的释放使用。
3. **on_click_complete**：松开时记录本次点击为新的双击基线（时间/位置/按钮）。基线的关键是"最近一次完成的点击"——触摸屏按下后滑走变成滚动，就不会污染基线。
4. **三条通路共享**：
   - 鼠标：`MouseAction::Press` → `on_button_press`，`MouseAction::Release` → `current_count` + `on_click_complete`
   - 触摸屏：`TouchEvent::Down`（slop 判定点击后才计数）→ `on_button_press`，`TouchEvent::Up`（Pending 且未超时未取消）→ `current_count` + `on_click_complete`
   - 触控板点击走对应通道，共用同一计数器

```rust
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_DISTANCE: Pixels = px(5.0);

fn on_button_press(&mut self, button: MouseButton, position: Point<Pixels>) -> usize {
    let is_repeat = self.last_click_time.is_some_and(|t| t.elapsed() < DOUBLE_CLICK_INTERVAL)
        && self.last_click_button == Some(button)
        && self.last_click_position.is_some_and(|p| Self::is_within_click_distance(p, position));
    self.current_count = if is_repeat { self.current_count.saturating_add(1) } else { 1 };
    self.current_count
}
```

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — 新增 `ClickTracker` 结构（阈值常量、on_button_press / current_count / on_click_complete），在鼠标 `handle_mouse_input` 和触摸屏 `InputEvent::TouchEvent` 两处调用

[[ohos-debug-lessons]]
