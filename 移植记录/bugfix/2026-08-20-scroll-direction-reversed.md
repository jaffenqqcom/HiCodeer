# OHOS 鼠标滚轮/触控板滚动方向与触摸屏相反

## 问题描述

OHOS 移植版（zcoder）鼠标滚轮和触控板双指滑动的滚动方向与触摸屏相反：触摸屏手指上滑内容上移（自然方向），而鼠标滚轮/触控板滚动时内容往相反方向滚动，体验割裂。

## 问题表现

- 鼠标滚轮向上滚，内容向下滚动（与预期相反）
- 触控板双指向上滑，内容向下滚动（与触摸屏方向相反）
- 触摸屏（DispatchTouchEvent 通道）方向正常，仅 Axis 通道（鼠标滚轮/触控板）相反
- 无崩溃、无报错，纯粹是方向体验问题

## 问题原因

轴事件（滚轮/触控板）与触摸事件走不同的通道：
- 触摸屏走 `DispatchTouchEvent`，坐标/滚动方向由 NDK 触摸事件提供
- 鼠标滚轮/触控板走 `RegisterUIInputEventCallback`（仅 Axis 事件），`handle_axis_input` 里把 `scroll_vertical`/`scroll_horizontal` 直接转为 GPUI `ScrollDelta`，**未做正负号对齐**

OHOS 的 Axis 事件正负号约定与触摸屏/GPUI 的滚动方向约定相反，导致两通道方向不一致。

## 解决方案

在 `handle_axis_input` 里对 `scroll_horizontal`/`scroll_vertical` 取反，与触摸屏方向对齐。同时修正鼠标滚轮速度（离散滚轮，每 120 单位 = 1 格）：

```rust
// 取反 + 速度换算（WHEEL_LINES_PER_NOTCH 后续按用户反馈调至 3.0）
let delta = point(
    -(data.scroll_horizontal / AXIS_WHEEL_UNIT * WHEEL_LINES_PER_NOTCH) as f32,
    -(data.scroll_vertical / AXIS_WHEEL_UNIT * WHEEL_LINES_PER_NOTCH) as f32,
);
```

触控板分支同样对 `scroll_horizontal`/`scroll_vertical` 取反（连续像素滚动）。

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — `handle_axis_input` 的 Mouse（滚轮）和 Touchpad（触控板）分支对 `scroll_vertical`/`scroll_horizontal` 取反，与触摸屏方向一致；`WHEEL_LINES_PER_NOTCH` 从 2.0 调至 3.0（滚轮速度）

[[ohos-debug-lessons]]
