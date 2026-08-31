# gpui 依赖 OHOS 平台类型（openharmony-ability）架构解耦

## 问题描述

通用 UI 框架 gpui 直接依赖了 OHOS 平台的 `openharmony-ability` crate，把平台类型 `OpenHarmonyApp` 暴露进 gpui 的公开 API。这不仅架构不合理（gpui 是跨平台 UI 框架，不应感知任何平台的具名类型），还导致每次修改 openharmony-ability 都会触发 **gpui 重编 → 全 zed 应用栈重编**（gpui 是几乎全部 crate 的基石），编译极其缓慢。

## 问题表现

- `crates/gpui/Cargo.toml` 在 `[target.'cfg(target_env = "ohos")'.dependencies]` 依赖 openharmony-ability
- `crates/gpui/src/platform.rs` 的 `Platform` trait 有 `#[cfg(target_env = "ohos")] fn set_ohos_app(&self, app: openharmony_ability::OpenHarmonyApp)` —— **平台具名类型进 gpui trait**
- `crates/gpui/src/app.rs` 有 `with_ohos_app(app: openharmony_ability::OpenHarmonyApp)`
- 修改 openharmony-ability（input/xcomponent/插件）→ gpui 指纹变 → 全栈数百个 crate 重编，每次约 10 分钟

## 问题原因

OHOS 移植时为了把 ArkTS 侧的 `OpenHarmonyApp` 注入 `OhosPlatform`，临时加了 `with_ohos_app`/`set_ohos_app` 通道，把具体平台类型直接暴露给 gpui。对比 macOS/Linux：

- macOS/Linux 的 `Platform` trait 平台特有方法（如 `set_traffic_light_position`）**全部使用 gpui 自己的类型**（`Point<Pixels>`、`bool`），从不暴露平台具名类型
- macOS/Linux 平台对象在 `MacPlatform::new()` / `X11Platform::new()` **创建时自包含**系统对象（NSApplication、X11 连接），通过 `Rc<dyn Platform>` 传给 gpui，**没有注入通道**
- 唯独 OHOS 的 `set_ohos_app` 把 `OpenHarmonyApp` 塞进 gpui trait —— 移植取巧，成为唯一例外

## 解决方案

对齐 macOS/Linux 模式：**平台对象创建时自包含 app，gpui 零平台类型、零注入通道**。

1. **openharmony-ability 全局 app 存储**：`app.rs` 加 `thread_local!` + `set_global_app(app)` / `global_app()`（`OpenHarmonyApp` 含 `Arc<RefCell>` 非 Send，不能用 `RwLock` 全局，用 thread_local —— 入口和平台创建都在主线程）
2. **zed 入口转手**：`openharmony_app(app)`（NAPI 入口，签名无法避免 OpenHarmonyApp）立即 `set_global_app(app.clone())`，zed 不再存储/传递/注入
3. **平台创建时读取**：`OhosPlatform::new()` 里 `global_app()` 读到 app 并持有（时序：openharmony_app 早于 build_application 的平台创建）
4. **删除 gpui 通道**：`with_ohos_app`（app.rs）、`set_ohos_app`（platform.rs trait）、`gpui/Cargo.toml` 的 openharmony-ability 依赖

```rust
// before: gpui Platform trait 暴露平台类型
#[cfg(target_env = "ohos")]
fn set_ohos_app(&self, _app: openharmony_ability::OpenHarmonyApp) {}

// after: gpui 不再有任何 OHOS 依赖，平台对象自包含
// OhosPlatform::new() 里：
if let Some(app) = openharmony_ability::global_app() {
    platform.set_app(app);
}
```

**收益**：
- gpui 彻底零 OHOS 类型/通道，与 macOS/Linux 一致
- 修改 openharmony-ability **不再触发 gpui 重编** → 不再触发全栈重编，编译风暴从源头消解
- zed 只在 NAPI 入口签名接触 `OpenHarmonyApp`，业务代码不感知

## 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs` — 加 `thread_local` 全局 app 存储：`set_global_app` / `global_app`
- `crates/gpui/src/app.rs` — 删除 `with_ohos_app` 方法（注入通道移除）
- `crates/gpui/src/platform.rs` — 删除 `set_ohos_app` trait 方法
- `crates/gpui/Cargo.toml` — 删除 `[target.'cfg(target_env = "ohos")'.dependencies]` 的 openharmony-ability；新增 OHOS dev-dependencies（供 ohos_hello 示例编译，不影响主 crate 指纹）
- `crates/gpui_ohos/src/ohos/platform.rs` — `OhosPlatform::new()` 读 `global_app()` 持有 app；删除 `set_ohos_app`、`set_app_from_platform`
- `crates/zed/src/lib.rs` — `openharmony_app(app)` 调 `set_global_app(app.clone())`
- `crates/zed/src/main.rs` — 删除 `OHOS_ENTRY_APP`、`set_ohos_entry_app`、`build_application` 的 with_ohos_app 分支
- `crates/gpui/examples/ohos_hello.rs` — 改用 `set_global_app(app)`，删除 `with_ohos_app`

[[ohos-debug-lessons]]
