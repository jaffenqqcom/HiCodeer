# OHOS 黑屏：WAKER 时序导致 UserEvent 通道死掉，窗口创建任务被饿死

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植，产品名 zcoder）在 OHOS 设备上启动后**黑屏**：应用进程存活、不崩溃、日志重定向正常，但屏幕始终是纯黑色背景，没有任何 UI 内容。此前 Rust 侧 zed 初始化其实**成功了**（`on_finish_launching completed` 正常出现），但 GPUI 窗口从未创建，渲染从未发生。该问题与之前修复的 rust-embed 崩溃是独立的第二个启动阻塞点。

## 问题表现

- 设备屏幕黑屏（无任何 UI 内容，只有 ArkTS XComponent 的黑色背景）。
- 应用进程存活，CPU 持续高占用（约 657%），疑似主线程在空转等待。
- hilog（tag=Zcoder）里 `[boot] on_finish_launching completed` 出现——**zed 模块初始化全部完成**。
- 但 `[boot] open_window` / `[boot] initialize_renderer` / `[boot] WgpuRenderer initialized` 日志**从未出现**——GPUI 窗口从未创建。
- `[boot] handle_ohos_event: UserEvent received` / `[boot] run_foreground_tasks entered` 日志**从未出现**——foreground 任务队列从未被驱动。
- `[boot] create_waker: WAKER is NONE`，随后出现**大量** `[boot] OpenHarmonyWaker::wake called, has_waker=false`——wake 被频繁调用但全部静默失败。
- 复现：每次冷启动必现；无崩溃、无 panic、无错误日志。

## 问题原因

根因是 **WAKER 全局变量的时序 bug**：`create_waker()`（读全局 WAKER）早于 `create_lifecycle_handle()`（写全局 WAKER）执行，导致 OhosPlatform 持有的 waker 是 **None 快照**，之后所有 `wake()` 静默失败，UserEvent 通道永久死掉，窗口创建任务被饿死。

完整因果链：

1. `main()` → `build_application()` → `Application::with_ohos_app` → `OhosPlatform::set_ohos_app` → `set_app_from_platform` → `dispatcher.set_waker(app.create_waker())`。
2. `create_waker()` 读取**全局静态** `WAKER`（`LazyLock<RwLock<Option<Arc<ThreadsafeFunction>>>`），此时它还是 **`None`**——真正写入它的 `create_lifecycle_handle()` 在 ArkTS 侧 `init` NAPI 里，位于 `openharmony_app` **之后**才执行（`derive/src/lib.rs` 的宏生成顺序：先 `APP_CONFIGURED.get_or_init(|| openharmony_app(app))`，再 `create_lifecycle_handle(env, app)`）。
3. 所以 `create_waker()` 返回 `OpenHarmonyWaker { waker: None }`，永久存入 `OhosDispatcher.waker`。
4. GPUI `cx.spawn` 的窗口创建任务（`restore_or_create_workspace`）→ `dispatch_on_main_thread` → `main_sender.send` + `waker.wake()`；`wake()` 里 `if let Some(waker) = &self.waker` 为 false，**静默跳过**。
5. **UserEvent 永不产生** → `handle_ohos_event` 的 `Event::UserEvent` 分支（`run_due_timers` + `run_foreground_tasks`）永不执行 → **foreground executor 永不驱动**。
6. `restore_or_create_workspace`（创建窗口的异步任务）**永不执行** → `OhosPlatform::open_window` 不被调用 → **窗口从未创建**。
7. 屏幕上只剩 ArkTS XComponent 的黑色背景 → **黑屏**。

### 排查过程中的死路（重要教训）

- **误判 hilog 抓不到日志 = 应用没日志**：hdc 默认 baselevel 高于 INFO，app 的 info 日志不可见。必须 `hdc shell "hilog -b D"` 设置 baselevel 才能看到 app 日志（此前一直以为 hdc 权限不够）。
- **误判"debug 日志应可见"**：zlog 的 `filter.rs` 里 `LEVEL_ENABLED_MAX_DEFAULT = LevelFilter::Info`——**debug 级别日志被 zlog 内部过滤**，所以 `log::debug!("[boot] run_loop event")` 永远看不到，不能据此判断事件流。
- **误判 libnative_ability.so 未加载是根因**：用户怀疑 native_ability 的 so 没加载导致消息发不出。经查证，libnative_ability.so 的 C++ 源码（`napi_init.cpp`）是**纯模板示例**（只有一个 `Add` 两数相加函数），与 zcoder 桥接完全无关；且事件（SurfaceCreate）确实能发出（on_finish_launching 触发），证明桥接正常。该假设被排除。
- **WAKER 快照 vs 全局**：最终通过在 `create_waker` 加 WAKER SET/NONE 日志、`wake()` 加 `has_waker` 日志、`handle_ohos_event` 加 UserEvent 日志，一锤定音确认 `WAKER is NONE` + `has_waker=false` + `UserEvent 未到`。

## 解决方案

核心修复：`wake()` 改为**每次调用时实时从全局 `WAKER` 读取 TSFN**，不再使用创建时的快照。这样无论 `create_waker()` 何时执行（早于 `create_lifecycle_handle`），`wake()` 时都能拿到 `create_lifecycle_handle` 已写入的 TSFN。

```rust
// 修复前（waker.rs）
pub fn wake(&self) {
    if let Some(waker) = &self.waker {   // 创建时的快照，此处永远是 None
        waker.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
    }
}

// 修复后（waker.rs）
pub fn wake(&self) {
    let guard = (*WAKER).read().expect("Failed to read WAKER");
    if let Some(waker) = guard.as_ref() {   // 实时读全局 WAKER
        waker.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
    }
}
```

**方案对比**：
- 方案 A（采用）：`wake()` 实时读全局 WAKER。最小改动、健壮（不依赖时序），后续任何时刻写入 WAKER 都能生效。
- 方案 B：调整宏生成顺序，让 `create_lifecycle_handle` 先于 `openharmony_app` 执行。改动涉及 NAPI 宏与 ArkTS 调用协议，侵入大、风险高。
- 方案 C：在 `create_lifecycle_handle` 写入后重新触发 `set_waker`。需要跨 crate 反向通知，复杂且仍有时序窗口。

**验证结果**（设备日志）：
- `create_waker: WAKER is NONE`（时序早，正常）→ `create_lifecycle_handle: WAKER set`（写入成功）。
- `handle_ohos_event: UserEvent received` → `run_foreground_tasks entered`（UserEvent 链路恢复）。
- `open_window for handle WindowId(1v1)` → `WgpuRenderer initialized successfully, adapter "Maleoon 916B"`（窗口创建 + 渲染器初始化）。
- 渲染循环无 warn（`get_current_texture` 全 Success），进程稳定，用户确认屏幕正常显示。

## 修改文件

- `crates/openharmony-ability/crates/ability/src/waker.rs` — 核心修复：`wake()` 从创建时快照改为实时读全局 WAKER，修复 UserEvent 通道死掉导致的窗口不创建黑屏。
- `crates/openharmony-ability/crates/ability/src/app.rs` — `create_waker` 增加 WAKER SET/NONE 日志（定位用，保留）。
- `crates/openharmony-ability/crates/ability/src/lifecycle.rs` — `create_lifecycle_handle` 增加 "WAKER set" 日志（定位用，保留）。
- `crates/openharmony-ability/crates/ability/Cargo.toml` — 增加 `log = "0.4"` 依赖（支持上述日志）。

排查辅助日志（启动一次性日志，按用户要求保留；刷屏日志已删除）：
- `crates/zed/src/lib.rs`、`crates/zed/src/main.rs` — 启动入口与模块初始化 enter/exit 日志。
- `crates/gpui_ohos/src/ohos/platform.rs`、`window.rs`、`wgpu_renderer.rs` — 渲染初始化链路日志；已删除刷屏的 UserEvent/run_foreground_tasks/run_loop event/handle_event/draw 日志。

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
