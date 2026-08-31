# zed 启动入口迁出 openharmony-ability（launch-zed 依赖反转）

## 问题描述

zed crate 直接依赖 openharmony-ability（NAPI 桥接库），NAPI 启动入口 `#[ability] openharmony_app(app: OpenHarmonyApp)` 就放在 zed 的 cdylib（`libzcoder.so`）里。由此产生两个问题：① 每次修改 openharmony-ability 的桥接代码，都会触发 cargo 级联重编 zed 及整条依赖链（`gpui_ohos → gpui_platform → gpui → zed`），编译极其缓慢；② 架构上 zed 是应用层，不应看到 openharmony-ability（平台能力库），平台具名类型 `OpenHarmonyApp` 出现在 zed 的启动链路里。

目标：把启动入口迁移到能力库一侧，依赖方向反转为「openharmony-ability 侧依赖 zed、zed 不再依赖 openharmony-ability」。

## 问题表现

- `crates/zed/src/lib.rs` 用 `#[ability] pub fn openharmony_app(app: openharmony_ability::OpenHarmonyApp)` 承载 NAPI 启动入口（宏生成 `init`/`render`/`dispose_render`/`on_back_press_intercept`/`on_bridge_sync_event` 等一批导出符号）
- `crates/zed/src/main.rs` 的 `run_with_ability_entry(app)` 直接调 `app.base_path()`，函数签名暴露 `OpenHarmonyApp`
- `crates/zed/Cargo.toml` ohos 分支带 `openharmony-ability` / `openharmony-ability-derive` / `napi-ohos` / `napi-derive-ohos`，build-deps 带 `napi-build-ohos`；`[lib] crate-type = ["cdylib", "rlib"]`
- `crates/zed/src/lib.rs` 还带两个 `pthread_mutex_*` 补丁符号（OHOS libc 缺 robust mutex）
- 修改 openharmony-ability → 全链级联重编，动辄数分钟

## 问题原因

- **入口位置历史包袱**：`libzcoder.so` 由 zed crate 承载（cdylib），而 NAPI `init` 必须导出在最终 so 里，于是 `#[ability]` 入口、NAPI 注册 `napi_build_ohos::setup()`、`pthread_mutex_*` 补丁符号全部堆积在 zed。
- **依赖方向错误**：`zed → gpui → gpui_platform → gpui_ohos → openharmony-ability`（运行时能力），且 zed 直接依赖 openharmony-ability。cargo 增量编译粒度是 crate，上游 rlib 一变，整条下游链逐个重编，`zed` 是这条链的下游，必然被波及。
- **成环约束（关键）**：若按直觉把入口放 openharmony-ability crate 内并让 ability 依赖 zed（调用 zed 的启动函数），会与既有依赖形成环：`openharmony-ability → zed → gpui_platform → gpui_ohos → openharmony-ability`，cargo 直接拒绝 cyclic package dependency，编译不过。破环唯一路径是让入口 crate 站在依赖图顶端、不被 gpui_ohos 依赖。

## 解决方案

**新增独立入口 crate `launch-zed`，承载全部 NAPI 启动职责，依赖方向反转。**

依赖图（无环）：

```
launch-zed (cdylib → libzcoder.so，[lib] name = "zcoder")
   ├── zed (rlib，提供 start_zed_main)      ← 能力库一侧依赖 zed
   └── openharmony-ability (rlib，OpenHarmonyApp / set_global_app / base_path)
```

关键决策：

1. **入口必须是独立 crate**：`launch_app` 依赖 zed（调 `start_zed_main`），就必须站在依赖图顶端；放 openharmony-ability crate 内必然成环（见问题原因）。它作为 openharmony-ability 仓库下的一个独立 crate 存在（`crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/`），既是"能力库一侧"，又避开环。
2. **三个接口职责划分**：
   - `launch_app(app: OpenHarmonyApp)`（`#[ability]` 宏，NAPI 入口）：`set_global_app(app.clone())` + `zed::start_zed_main(app.base_path())`。`OpenHarmonyApp` 只在此入口签名出现，zed 业务代码不再感知。
   - `zed::start_zed_main(base_path: Option<String>)`：`base_path → data_dir → set_custom_data_dir` → `main()`。zed 只暴露这一个纯数据接口（替换原 `run_with_ability_entry(app)`）。
   - `OpenHarmonyApp::base_path()`：launch-app 取 zed 真正需要的信息（沙箱路径）。
3. **NAPI 相关全部随最终 cdylib 迁移**：`napi_build_ohos::setup()`（build.rs）、`pthread_mutex_*` 补丁符号（lib.rs）从 zed 移到 launch-zed。`NAPI_BUILD_TARGET_NAME=zcoder` 与 `[lib] name="zcoder"` 保持一致，产物直接叫 `libzcoder.so`，ArkTS 侧 `moduleName='zcoder'` 零改动。
4. **zed 收敛**：删 `#[ability] openharmony_app`、`openharmony_app_not_available`、pthread 符号；`Cargo.toml` 删 openharmony-ability/napi 依赖、`[lib] crate-type` 收敛为 `["rlib"]`；`build.rs` 删 `napi_build_ohos::setup()`。
5. **构建脚本**：`script/bundle-ohos` 的 `CRATE="zed"` → `CRATE="launch-zed"`，产物复制行固定 `libzcoder.so`（不再改名）。

```rust
// before: zed 直接依赖 openharmony-ability，入口在 zed cdylib
#[ability]
pub fn openharmony_app(app: openharmony_ability::OpenHarmonyApp) {
    zlog::ohos::direct_hilog_info("zcoder-boot", "[boot] openharmony_app entered");
    openharmony_ability::set_global_app(app.clone());
    run_with_ability_entry(app);   // zed 签名暴露 OpenHarmonyApp
}

// after: 入口在 launch-zed（独立 crate），zed 只出纯数据接口
// launch-zed/src/launch_app.rs
#[ability]
pub fn launch_app(app: openharmony_ability::OpenHarmonyApp) {
    openharmony_ability::set_global_app(app.clone());
    zed::start_zed_main(app.base_path());   // zed 不再接触 OpenHarmonyApp
}
// crates/zed/src/main.rs
pub fn start_zed_main(base_path: Option<String>) { /* data_dir → main() */ }
```

**失败尝试（死路）**：用户最初希望不新增 crate、把 `launch_app` 直接放 openharmony-ability crate 内并让 ability 依赖 zed。经逐一验证，该方向必然成环（见问题原因），`#[cfg(target_env="ohos")]` 与 `optional` feature 门控都无法绕过（同一 workspace 一次 resolve 只有一份 ability，feature 开启即全局成环）。最终确认必须独立 crate 破环。

**边界说明**：本次只解决"架构上 zed 不直接依赖 openharmony-ability、入口在能力库一侧"。改 openharmony-ability **运行时**代码仍会级联重编 zed——因为 `gpui_ohos → openharmony-ability` 运行时能力边仍在 zed 依赖链上。彻底消除该编译耦合需动态库化（openharmony-ability 独立 .so + C ABI），未在本期实施。

**日志说明**：`launch_app`（NAPI 入口）不打印日志（用户要求删除 `zlog::ohos` 依赖，不引入其他日志接口）；`start_zed_main` 保留 `direct_hilog_info`（tag=zcoder-boot），`zlog::init()` 之后走 hilog 重定向（tag=Zcoder）。

## 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/Cargo.toml` — 新增：cdylib，`[lib] name = "zcoder"`，依赖 zed(path) + openharmony-ability + derive + napi-ohos/napi-derive-ohos，build-deps napi-build-ohos
- `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/build.rs` — 新增：ohos 下 `napi_build_ohos::setup()`（NAPI 模块注册随最终 cdylib）
- `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/lib.rs` — 新增：`mod launch_app` + 从 zed 迁入的 `pthread_mutexattr_setrobust`/`pthread_mutex_consistent` 补丁符号
- `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/launch_app.rs` — 新增：`#[ability] pub fn launch_app(app)` → `set_global_app` + `zed::start_zed_main(app.base_path())`
- `crates/zed/src/lib.rs` — 删 `#[ability] openharmony_app`、`openharmony_app_not_available`、pthread 符号、`set_global_app` 调用，仅保留 `#[cfg(target_env="ohos")] include!("main.rs")`
- `crates/zed/src/main.rs` — `run_with_ability_entry(app)` 改为 `pub fn start_zed_main(base_path: Option<String>)`（设 data_dir → main）；删 `OpenHarmonyApp` import
- `crates/zed/Cargo.toml` — 删 ohos 分支 openharmony-ability/derive/napi 依赖与 build-deps napi-build-ohos；`[lib] crate-type` 收敛 `["rlib"]`
- `crates/zed/build.rs` — 删 ohos 分支 `napi_build_ohos::setup()`（迁入 launch-zed）
- `script/bundle-ohos` — `CRATE="zed"` → `CRATE="launch-zed"`；产物复制行固定 `libzcoder.so`（launch-zed `[lib] name` 直接产出，无需改名）
- `Cargo.toml`（根）— workspace members 追加 `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed`

[[ohos-debug-lessons]]
