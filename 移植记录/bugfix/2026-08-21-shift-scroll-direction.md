# OHOS Shift+滚轮不能水平滚动（缺 shift 滚动换向）

## 问题描述

OHOS 移植版（zcoder）按住 Shift 同时滚动鼠标滚轮/触控板双指，本应把垂直滚动换向为水平滚动（代码编辑器的常见习惯），但 zcoder 上没有任何水平滚动效果。

## 问题表现

- 按住 Shift + 滚轮：内容不横向滚动
- 桌面端（Linux `gpui_linux`）按下 Shift 滚轮会横向滚动（编辑器水平移动光标/滚动长行）
- 无崩溃、无报错，纯粹是缺换向逻辑

## 问题原因

`handle_axis_input` 直接把轴事件的 `scroll_vertical` / `scroll_horizontal` 转成 GPUI `ScrollDelta`，**没有在按住 Shift 时交换横纵轴**。Linux 参考实现里滚轮事件带 Shift 修饰时会换向，OHOS 透传时丢失了这一步。

## 解决方案

在 `handle_axis_input` 里对齐 `gpui_linux` 的语义：当 `modifiers.shift` 按下时，交换 `scroll_horizontal` 和 `scroll_vertical`，再按轴类型换算。这样：

- Shift + 鼠标滚轮：垂直滚动 → 水平滚动（编辑器横向滚动长行）
- Shift + 触控板双指：垂直滑动 → 水平滑动

```rust
// Shift+wheel maps vertical scrolling to horizontal, mirroring gpui_linux.
let mut scroll_horizontal = data.scroll_horizontal;
let mut scroll_vertical = data.scroll_vertical;
if modifiers.shift {
    std::mem::swap(&mut scroll_horizontal, &mut scroll_vertical);
}
// then map to ScrollDelta::Lines (mouse) / ScrollDelta::Pixels (touchpad)
```

换向后，鼠标滚轮仍按 `AXIS_WHEEL_UNIT`（120 单位/格）转 Lines、触控板仍按像素 1:1 转 Pixels，两分支各自方向取反与触摸屏对齐。

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — `handle_axis_input` 中 `modifiers.shift` 时交换横纵轴

[[ohos-debug-lessons]]
