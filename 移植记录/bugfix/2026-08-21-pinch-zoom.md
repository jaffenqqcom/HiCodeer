# OHOS 双指捏合缩放不生效（Pinch → Ctrl+滚轮）

## 问题描述

OHOS 移植版（zcoder）触控板/触摸屏双指捏合（pinch）时无任何反应。桌面端编辑器通过 Ctrl+滚轮实现字体缩放，而 OHOS 的捏合手势事件没有接入 GPUI。

## 问题表现

- 双指捏合：画面无任何缩放变化
- GPUI 的 `PinchEvent` 在 editor 里不被消费（只有 image_viewer 消费），直接分发 `PinchEvent` 也达不到缩放效果
- 无崩溃、无报错，纯粹是手势未接入

## 问题原因

1. OHOS 没有提供 NDK 级别的捏合手势通道，需要自己用 ArkTS `PinchGesture` 在 XComponent 上层叠加识别。
2. 即使把手势数据送进 GPUI 的 `PinchEvent`，editor 也不消费它，缩放必须转成 **Ctrl+ScrollWheel**（即 Ctrl+滚轮的 zoom 手势）。

## 解决方案

采用 openharmony-ability plugin 架构，事件驱动（无轮询）：

1. **ArkTS 层（`PinchPlugin.ets`）**：透明全屏 overlay 绑定 `PinchGesture`，把 `pinch-begin` / `pinch-update` / `pinch-end` 经 `invokeNativeSync` 推到 Rust，每次携带累计 scale（1.0 == 手势起点）和捏合中心点。
2. **Rust 插件层（`plugin-pinch` crate）**：`on_main_thread_event` 在主线程解析事件，计算增量 delta（当前 scale - 上次 scale），Begin/End 重置基线，回调交给 gpui_ohos。
3. **gpui_ohos（`dispatch_pinch_event`）**：把 pinch 累积值累加，**越过 `PINCH_ZOOM_THRESHOLD`（0.15，即累计 15% 缩放变化）才发一步 Ctrl+滚轮**，让缩放速率平缓。增量符号决定放大（+1 行）或缩小（-1 行），捏合中心作为滚轮位置。

```rust
const PINCH_ZOOM_THRESHOLD: f32 = 0.15;

fn dispatch_pinch_event(callbacks, accumulator, sample) {
    // Gesture end resets the accumulator so sub-threshold residue from one
    // gesture never bleeds into the next.
    if sample.phase == PinchPhase::End {
        accumulator.set(0.0);
    }
    let accumulated = accumulator.get() + sample.delta;
    if accumulated.abs() < PINCH_ZOOM_THRESHOLD {
        accumulator.set(accumulated);
        return;
    }
    accumulator.set(0.0);
    let lines = if accumulated > 0.0 { 1.0 } else { -1.0 };
    // dispatch ScrollWheelEvent with control:true modifier
}
```

## 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/plugins/pinch/src/main/ets/PinchPlugin.ets` — 新增捏合手势 overlay 插件
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-pinch/src/lib.rs` — 新增 pinch 桥接插件（增量 delta 计算、相位映射、thread_local 回调）
- `crates/gpui_ohos/src/ohos/window.rs` — `dispatch_pinch_event`（阈值累积 + Ctrl+滚轮转换）、`register_platform_event_handlers` 注册回调
- `crates/gpui_ohos/src/ohos/platform.rs` — `OhosPlatform::register_plugins` 注册 `PinchBridgePlugin`
- `hap/` 相关配置 — 插件模块声明与依赖

[[ohos-debug-lessons]]
