注意：GainedFocus现在也回去enable 重绘，否则窗口改变大小、从最小化恢复是，会有闪烁和黑屏的问题。下面的文档这点描述不对，以此为准。

# OHOS 空闲 CPU 占用过高（XComponent 持续帧回调）

## 问题描述

zcoder 应用在**未打开任何文件、无任何操作**时，前台和后台空转 CPU 占用高达 25-29%（top 实测），远超正常水平。最小化时 CPU 骤降（用户观察：最小化无日志、CPU 低）。

## 问题表现

- 前台无操作空转：CPU **25.9%**（15 秒采样）
- 后台（非最小化）空转：CPU 同样高（约 124Hz 事件持续）
- 最小化：帧回调自动停止（`is_render_surface_active=false`），CPU 低、无 `WindowRedraw` 日志
- 无输入事件、无 draw（`WindowRedraw` 到 `request_frame` 后 GPUI 因不 dirty 不重绘）

## 问题原因

**XComponent 的 `on_frame_callback`（`OH_NativeXComponent_RegisterOnFrameCallback`）一旦注册就持续每帧（约 124Hz）无条件回调**，无论画面是否有变化、是否有输入。该回调在**主线程**执行，每帧都：

1. 产生 `Event::WindowRedraw` → 投递到 run_loop
2. `OhosWindow::handle_event` 无条件 `request_frame(false)` → GPUI `on_request_frame` 回调执行（App borrow、`Instant::now()`、thermal 查询、`complete_frame`）
3. 主线程每 ~8ms 被唤醒一次处理事件，即使不 draw，事件处理开销累积成 25%+ CPU

Zed 桌面端（Linux/macOS）是**按需帧回调**：GPUI 需要渲染才请求帧、空闲完全停止。OHOS 移植把 XComponent 持续帧回调直接转发成渲染请求，绕过了按需协议，导致空转。

## 解决方案

采用「**按需注册/注销 XComponent 帧回调**」+「**窗口可见性状态机**」：

### 核心机制

1. **`frame_waker` 驱动**：GPUI 通过 `PlatformWindow::frame_waker()` 返回的闭包（`wake_platform` 信号）表达"我需要一帧"。闭包被调用时：
   - 无条件置 `PENDING_REDRAW=true`（记录帧需求，隐藏期间也不丢失）
   - 窗口可见（`window_visibility()`）时：`enable_frame_callback()`（注册 XComponent 帧回调）+ 置 enabled
2. **`WindowRedraw` 消费**：收到一帧后 `PENDING_REDRAW.swap(false)`：
   - 若 GPUI 又请求了下一帧（swap 出 true）→ 保持帧回调注册
   - 否则 → `disable_frame_callback()`（`OH_NativeXComponent_UnregisterOnFrameCallback` 真正注销）+ 置 disabled
   - 空闲时帧回调完全注销，主线程不再被唤醒
3. **窗口可见性管理**（`Window.on('windowVisibilityChange')`，ArkTS 事件驱动）：
   - `windowVisibilityChange(false)`（最小化/隐藏）→ `WINDOW_VISIBLE=false` + 发 `LostFocus` → 注销帧回调
   - `windowVisibilityChange(true)`（恢复可见）→ `WINDOW_VISIBLE=true` + 发 `GainedFocus` → 若 `PENDING_REDRAW=true` 则注册帧回调，渲染隐藏期间累积的帧需求

### 关键实现

```rust
// frame_waker（OhosWindowHandle::frame_waker）—— GPUI wake_platform 时被调
let waker: Rc<dyn Fn()> = Rc::new(|| {
    PENDING_REDRAW.store(true, Ordering::Release);
    if openharmony_ability::window_visibility() {
        openharmony_ability::set_frame_callback_enabled(true);
        if let Some(app) = openharmony_ability::global_app() {
            app.enable_frame_callback();
        }
    }
});

// WindowRedraw 处理（OhosWindow::handle_event）—— 每帧消费需求
self.request_frame(false);
if !PENDING_REDRAW.swap(false, Ordering::AcqRel) {
    openharmony_ability::set_frame_callback_enabled(false);
    if let Some(app) = self.app.borrow().clone() {
        app.disable_frame_callback();
    }
}
```

## 修改文件

- `crates/gpui_ohos/depend/ohos-xcomponent-binding/src/native_xcomponent.rs`：新增 `NativeXComponent::off_frame_callback`（封装 `OH_NativeXComponent_UnregisterOnFrameCallback`）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/input/mod.rs`：新增全局 `WINDOW_VISIBLE` / `FRAME_CALLBACK_ENABLED` + `set_window_visibility`/`window_visibility`/`set_frame_callback_enabled`/`is_frame_callback_enabled`
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lifecycle.rs`：`WindowStageEventCallback` 新增 `on_window_visibility_change` 回调（存全局 + 发 `GainedFocus`/`LostFocus`）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs`：新增 `OpenHarmonyApp::enable_frame_callback`/`disable_frame_callback`（真正注册/注销 XComponent 帧回调）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs`：删除无条件 `on_frame_callback` 注册
- `crates/gpui_ohos/depend/openharmony-ability/native_ability/src/main/ets/ability/type.ets`：`WindowStageEventCallback` 接口新增 `onWindowVisibilityChange`
- `crates/gpui_ohos/depend/openharmony-ability/native_ability/src/main/ets/ability/NativeAbility.ets`：注册 `win.on("windowVisibilityChange")` 并转发给 lifecycle 回调
- `crates/gpui_ohos/src/ohos/window.rs`：`PENDING_REDRAW` 全局 + `frame_waker`（OhosWindowHandle）+ `WindowRedraw`/`GainedFocus`/`LostFocus` 启停帧回调

## 验证结果

- 前台空闲 CPU：**25-29% → 0-3.7%**（多次采样 0.0%/3.7%/3.7%；剩余为 XCollie 等系统活动）
- 渲染器正常初始化（`Selected GPU adapter: Maleoon 916B (Gl)`，`WgpuRenderer initialized`）
- 按需渲染正常：有内容变化才渲染，空闲完全停止

## 排查方法记录

- 用 `[diag]` 计数日志确认：前台空转 15 秒 1862 个 `WindowRedraw`（≈124Hz），无输入事件、无 draw、无定时器（`UserEvent=0`）
- 关键区分：`on_frame_callback` 持续回调 ≠ `WindowRedraw` 事件；仅用 `enabled` 标志门控不够（回调本身在主线程每帧触发），必须真正 `unregister`
- `OH_NativeXComponent_RegisterOnFrameCallback` 是持续回调；`OH_NativeVSync` 单次 `RequestFrame` 是 warp 用的按需方式（本修复选用前者 + 按需注销，改动更小）

## 附注：代码检视发现的问题

- **时序 bug（已修复）**：`frame_waker` 最初在 `window_visibility()=false` 时不设 `PENDING_REDRAW`，导致隐藏期间帧需求丢失、恢复可见不渲染。改为无条件记录需求，仅对 arm vsync 做可见性门控。
- **类型 bug（已修复）**：`frame_waker` 最初加在 `impl PlatformWindow for OhosWindow`（底层窗口），但 GPUI 实际使用的是 `OhosWindowHandle`（`open_window` 返回 `Box<OhosWindowHandle>`），导致 `frame_waker` 走默认 `None`。已移到 `OhosWindowHandle`。
- **错误处理（已修复）**：`enable/disable_frame_callback` 的 FFI 调用失败加 `log::warn`（遵循日志规范）。
- **已知风险**：`on_frame_callback` 在主线程执行（实测 PID==TID），`event_loop.borrow_mut()` 依赖主线程；若 OHOS 版本回调线程变化需改为线程安全投递。

## 后续修复：最小化时 DisplaySync DelFromPipeline nullptr（2026-08-22）

### 问题

每次窗口最小化，hilog 出现系统级异常：

```
E [(-1:100000:singleton)] [DisplaySync] DelFromPipeline CurrentContext is nullptr.
```

`DelFromPipeline` 是 DisplaySync 从 vsync 管道删除 context 的操作，即 `disable_frame_callback` → `OH_NativeXComponent_UnregisterOnFrameCallback` 内部路径；报 nullptr 说明注销时管道里已无有效 context。

### 根因

1. **帧回调启停挂在 `GainedFocus`/`LostFocus` 上，而该事件被两个来源复用**：`StageEventType::Active/Inactive`（真实焦点）和 `windowVisibilityChange`（可见性，`lifecycle.rs` 把它路由成 `GainedFocus`/`LostFocus`）。最小化时若两条事件都触发，`disable_frame_callback` 执行两次 → 第二次 Unregister 时 context 已删 → nullptr。
2. **`enable/disable_frame_callback` 无幂等保护**：无条件 `off_frame_callback` / 先 off 再 on，且 `FRAME_CALLBACK_ENABLED` 由调用方（`window.rs`）预先 set，函数内部无法用它做幂等判断。

### 修复

- **`windowVisibilityChange` 走独立可见性事件，不再复用 focus**：`event.rs` 新增 `Event::VisibilityChanged(bool)`；`lifecycle.rs` 的 `on_window_visibility_change` 改为发 `VisibilityChanged(visible)`；`window.rs` 新增 `VisibilityChanged` 分支启停帧回调（false→`disable_frame_callback`，true 且 `PENDING_REDRAW`→`enable_frame_callback`），`GainedFocus`/`LostFocus` 剥离帧回调、回归纯焦点语义（`active_status_change`、触摸复位、键盘隐藏）。
- **`enable/disable_frame_callback` 幂等化**：`FRAME_CALLBACK_ENABLED`（AtomicBool）收进两函数内部维护——`enable` 已注册直接 return、注册成功才置 true；`disable` 未注册直接 return、注销后置 false；删除 `enable` 里"先 off 再 on"冗余注销。`window.rs` 删除 4 处外部 `set_frame_callback_enabled` 调用点。

### 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/event.rs`
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lifecycle.rs`
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs`
- `crates/gpui_ohos/src/ohos/window.rs`

### 验证结果

**待真机验证**（静态检视已通过）：最小化不再重复注销，`DelFromPipeline CurrentContext is nullptr` 应消失。

### 行为变化与边界

- 帧回调启停现在**完全由 `VisibilityChanged` 驱动**；`GainedFocus` 只更新 `active_status_change`。若平台只发焦点事件、不发可见性事件，恢复前台时帧回调不会自动恢复（2in1 上可见性事件是可靠信号，需真机确认手机切前台场景）。
- **边界未覆盖**：若某设备最小化触发 `on_surface_destroyed`（系统清理 DisplaySync context 而 `FRAME_CALLBACK_ENABLED` 仍 true），后续 `disable` 仍可能 nullptr。最小化通常是 `surface_active=false`（deactivate 而非 destroy），若真机仍报错，需在 `on_surface_destroyed` 同步 `set_frame_callback_enabled(false)`。
