---
name: hicodeer-codemap
description: HiCodeer（Zed → HarmonyOS NEXT 移植）项目的代码架构地图，记录程序启动流程、模块入口与跨运行时边界（NAPI / XComponent / 事件循环）、terminal 运行路径（面板 / 视图 / 网格元素 / pty / shell 后端与输入输出）、hicodeerd 守护进程自身的运行路径（双 SSH listener / 会话分发 / 子进程与进程组回收）、GPUI 平台后端接口清单（13 个平台 trait 的定义位置 → OHOS 实现位置与缺口口径）与 profiler 任务耗时采样链路（dispatcher 三处上报 → 全局按线程存储 → hang_detection / task_traces / miniprofiler 消费方），以及 Zed 官方服务的出网总闸与各服务出网点（遥测 / 扩展市场 / 自动更新 / Cloud 账号 / Cloud LLM / Zed 编辑预测 / web search / 协作 RPC / MCP OAuth）。用于快速定位各模块的入口函数与触发方式。
---

# hicodeer-codemap：HiCodeer 启动流程架构地图

本文件记录 HiCodeer（代码库为 Zed，产品名 HiCodeer，移植到 HarmonyOS NEXT）的**程序启动流程**。只记录模块入口、跨文件跳转、跨运行时跳转，不追踪完整执行路径。

所有路径均为相对项目根目录的相对路径。

## 启动链路总览

HiCodeer 的启动是「ArkTS 壳 → NAPI 桥 → Rust 入口 → GPUI 事件循环」四段式：

```
系统拉起 HAP
  → EntryAbility (ArkTS, RustAbility 基类)
  → 加载 libhicodeer.so
  → NAPI init（launch-zed crate 的 #[ability] 宏生成）
  → launch_app(app)（launch-zed/src/launch_app.rs，Rust 侧入口）
  → set_global_app(app.clone()) + zed::start_zed_main(app.base_path())
  → start_zed_main()（crates/zed/src/main.rs；设 data_dir → 调 main）
  → main()（Zed 完整启动逻辑）
  → build_application()（ohos 分支创建 OhosPlatform；OhosPlatform::new 从全局 GLOBAL_APP 取 app）
  → app.run() → OhosPlatform::run()（注册 run_loop 事件回调）
  → 收到 SurfaceCreate 事件 → 触发 on_finish_launching → 打开窗口
```

## 模块入口

### 应用壳（HAP / ArkTS）

```
HAP 到 EntryAbility 在 hap/entry/src/main/ets/entryability/EntryAbility.ets  [由系统按 module.json5 的 mainElement 在应用启动时创建]
RustAbility 基类 在 node_modules/@ohos-rs/ability  [由 EntryAbility extends 继承；加载 moduleName 对应的 native 库 libhicodeer.so 并调用 NAPI init]
```

关键配置：`EntryAbility.moduleName = "hicodeer"`（对应 `libhicodeer.so`；launch-zed 的 `[lib] name = "hicodeer"` 使产物直接叫 `libhicodeer.so`，`NAPI_BUILD_TARGET_NAME=hicodeer` 使 NAPI 模块名与之统一）。

### Rust 入口（launch-zed + crates/zed）

```
入口 到 launch_app() 在 crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/launch_app.rs  [由 NAPI init 宏生成代码调用；接收 OpenHarmonyApp]
入口 到 set_global_app() 在 openharmony-ability crates/ability/src/app.rs  [由 launch_app() 调用；把 OpenHarmonyApp 存入 GLOBAL_APP 全局，供 OhosPlatform 构造时取用]
入口 到 start_zed_main() 在 crates/zed/src/main.rs  [由 launch_app() 跨 crate 调用（launch-zed 依赖 zed）；接收 base_path → 设 data_dir → 调 main()]
ZED 到 main() 在 crates/zed/src/main.rs  [由 start_zed_main() 在 ohos 下调用（桌面端由系统直接调 main）；执行 Zed 完整启动逻辑]
```

### GPUI 应用装配

```
GPUI 到 build_application() 在 crates/zed/src/main.rs  [由 main() 调用；ohos 分支经 gpui_platform::current_platform 创建 OhosPlatform，gpui 不直接接触 OpenHarmonyApp]
GPUI 到 OhosPlatform::new() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 current_platform() 触发；内部从 openharmony_ability::global_app() 取 app 并 set_app（存 app + 建 primary display + 设 waker + 注册插件）]
GPUI 到 app.run() 在 crates/zed/src/main.rs  [由 main() 调用；把 Zed 的 on_finish_launching 回调传给 Platform::run]
```

### Zed 初始化流程（on_finish_launching 闭包内，crates/zed/src/main.rs）

on_finish_launching 闭包是 `app.run(move |cx| {...})` 的回调，仅在收到 `Event::SurfaceCreate` 时由 `OhosPlatform::handle_ohos_event` 触发一次。闭包内按固定顺序调用全部模块的 `init`。每个模块 init 前后均有 `[boot] enter/exit init: <模块>` 日志（hilog tag=HiCodeer），日志停在哪个 `enter init` 未出现 `exit init`，即初始化卡在该模块。

模块初始化顺序（启动时自动顺序执行，非用户操作触发）：

```
[顺序执行] trusted_worktrees → menu → zed_actions → release_channel → gpui_tokio → settings → zlog_settings
→ git_hosting_providers → extension → debug_adapter_extension → languages → language_extension → zed
→ project::Project → debugger_ui → debugger_tools → client → feature_flags::FeatureFlagStore
→ auto_update → dap_adapters → auto_update_ui → reliability → extension_host → theme_settings
→ theme_extension → command_palette → copilot_chat → copilot_ui → language_model → language_models
→ acp_tools → zed::telemetry_log → zed::remote_debug → edit_prediction_ui → web_search
→ web_search_providers → snippet_provider → edit_prediction_registry → agent_ui → repl
→ recent_projects → dev_container → editor → image_viewer → repl::notebook → diagnostics → audio
→ workspace → ui_prompt → go_to_line → file_finder → tab_switcher → outline → project_symbols
→ project_panel → outline_panel → tasks_ui → snippets_ui → channel → search → lsp_locations → vim
→ terminal_view → journal → encoding_selector → language_selector → line_ending_selector
→ toolchain_selector → theme_selector → settings_profile_selector → language_tools → call
→ notifications → collab_ui → git_ui → feedback → markdown_preview → csv_preview → svg_preview
→ onboarding → settings_ui → keymap_editor → extensions_ui → edit_prediction → inspector_ui
→ json_schema_store → miniprofiler_ui → which_key → component_preview
```

- 每个模块入口：`<模块>::init(cx)` 或 `<模块>::init(args, cx)`，均在 `crates/zed/src/main.rs` 的 on_finish_launching 闭包内。
- 初始化完成后调 `initialize_workspace()` 创建首个 workspace 窗口 → 触发 `OhosPlatform::open_window` → `OhosWindow::initialize_renderer` → `WgpuContext::new` / `WgpuRenderer::new`。
- 早期入口（logger 未注册，直连 hilog，tag=hicodeer-boot）：`launch_app`（NAPI 入口，无日志）→ `start_zed_main`（direct_hilog）→ `main` → `zlog::init`（此后 log 宏进 hilog，tag=HiCodeer）。

跨文件跳转：
```
on_finish_launching 闭包在 crates/zed/src/main.rs 到 settings::init() 在 crates/settings/src/settings.rs  [settings 全局初始化，早期关键模块；内嵌资源经 rust_embed 加载]
on_finish_launching 闭包在 crates/zed/src/main.rs 到 workspace::init() 在 crates/workspace  [workspace 子系统初始化]
on_finish_launching 闭包在 crates/zed/src/main.rs 到 editor::init() 在 crates/editor  [编辑器子系统初始化]
on_finish_launching 闭包在 crates/zed/src/main.rs 到 audio::init() 在 crates/audio/src/audio.rs  [音频子系统初始化]
on_finish_launching 闭包在 crates/zed/src/main.rs 到 initialize_workspace() 在 crates/zed/src/main.rs  [创建首个 workspace 窗口]
```

### OHOS 平台事件循环（crates/gpui_ohos）

```
OHOS 到 OhosPlatform::run() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 Application::run() 调用；注册 OpenHarmonyApp::run_loop 事件回调]
OHOS 到 handle_ohos_event() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 run_loop 回调每收到一个 Event 时调用；先跑 foreground tasks，再在 SurfaceCreate 时触发 on_finish_launching，最后路由给各窗口]
OHOS 到 open_window() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 Zed 在 on_finish_launching 里调 cx.open_window() 触发；创建 OhosWindow + 初始化 WgpuRenderer]
```

## 跨文件跳转

```
launch_app() 在 crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/launch_app.rs 到 set_global_app() 在 openharmony-ability crates/ability/src/app.rs  [存 app 到 GLOBAL_APP]
launch_app() 在 crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/launch_app.rs 到 start_zed_main() 在 crates/zed/src/main.rs  [依赖反转：launch-zed（能力库一侧）依赖 zed]
start_zed_main() 在 crates/zed/src/main.rs 到 main() 在 crates/zed/src/main.rs  [原始 Zed main]
main() 在 crates/zed/src/main.rs 到 build_application() 在 crates/zed/src/main.rs  [构建 Application]
build_application() 在 crates/zed/src/main.rs 到 gpui_platform::current_platform() 在 crates/gpui_platform/src/gpui_platform.rs  [ohos 分支创建 OhosPlatform]
OhosPlatform::new() 在 crates/gpui_ohos/src/ohos/platform.rs 到 global_app() 在 openharmony-ability crates/ability/src/app.rs  [从 GLOBAL_APP 取 app；gpui 不直接接触 OpenHarmonyApp]
main() 在 crates/zed/src/main.rs 到 app.run() 在 crates/gpui/src/app.rs  [启动 GPUI]
app.run() 在 crates/gpui/src/app.rs 到 OhosPlatform::run() 在 crates/gpui_ohos/src/ohos/platform.rs  [注册事件循环]
OhosPlatform::run() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OpenHarmonyApp::run_loop() 在 openharmony-ability crates/ability/src/app.rs  [注册 ArkTS 事件回调]
handle_ohos_event() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OhosWindow::handle_event() 在 crates/gpui_ohos/src/ohos/window.rs  [事件路由到各窗口]
```

## 跨运行时跳转

### ArkTS → Rust（NAPI）

```
EntryAbility (ArkTS) 到 [NAPI init] 到 launch_app() 在 crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/launch_app.rs  [RustAbility 基类加载 libhicodeer.so 后调用 init；launch-zed 的 #[ability] 宏生成代码调 launch_app]
```

### Rust 内事件循环（openharmony-ability Event → GPUI）

```
OpenHarmonyApp::run_loop 回调 到 [Event 枚举] 到 handle_ohos_event() 在 crates/gpui_ohos/src/ohos/platform.rs  [ArkTS 侧窗口/surface/输入/生命周期事件经 NAPI 转成 Event 后推给 run_loop 回调]
handle_ohos_event() 到 [Event::SurfaceCreate] 到 on_finish_launching 回调（Zed 的 app.run 闭包）  [on_finish_launching 只在收到 SurfaceCreate 时触发；此时才创建窗口]
OpenHarmonyWaker::wake() 到 [Event::UserEvent] 到 handle_ohos_event() 在 crates/gpui_ohos/src/ohos/platform.rs  [TSFN 回调向 event_loop 发 UserEvent；handle_ohos_event 的 UserEvent 分支跑 run_due_timers + run_foreground_tasks]
handle_ohos_event(UserEvent) 到 run_foreground_tasks() 在 crates/gpui_ohos/src/ohos/platform.rs  [执行 main_receiver 里排队的 GPUI foreground 任务，含窗口创建任务 restore_or_create_workspace]
OhosDispatcher::dispatch_on_main_thread() 在 crates/gpui_ohos/src/ohos/dispatcher.rs 到 OpenHarmonyWaker::wake() 在 openharmony-ability crates/ability/src/waker.rs  [GPUI 任务入 main_sender 后唤醒主线程；wake 必须实时读全局 WAKER（见常见坑 WAKER 时序）]
```

### 窗口创建后的渲染（OpenHarmonyApp → wgpu surface）

```
Zed 的 cx.open_window() 到 [AnyWindowHandle + WindowParams] 到 OhosPlatform::open_window() 在 crates/gpui_ohos/src/ohos/platform.rs  [创建 OhosWindow 并立即调 initialize_renderer()]
OhosWindow 到 initialize_renderer() 在 crates/gpui_ohos/src/ohos/window.rs  [从 OpenHarmonyApp.native_window() 取 OH_NativeWindow，经 HasWindowHandle/HasDisplayHandle 建 wgpu surface]
OhosWindow 到 WgpuRenderer 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [wgpu 29 GL 后端渲染；PresentMode::Fifo]
```

## 功能模块地图

以下按功能模块记录 HiCodeer 已分析的代码运行路径。模块名到入口函数，标注触发方式。

### 渲染模块（wgpu）

```
渲染 到 OhosWindow::initialize_renderer() 在 crates/gpui_ohos/src/ohos/window.rs  [由 OhosPlatform::open_window() 创建窗口时调用；SurfaceCreate 事件也会经 OhosWindow::handle_event 再次触发（幂等，renderer 已建则直接返回）；从 OpenHarmonyApp.native_window() 取 OH_NativeWindow + content_rect 尺寸后建 WgpuContext 与 WgpuRenderer]
渲染 到 WgpuContext::new() 在 crates/gpui_ohos/src/ohos/wgpu_context.rs  [由 initialize_renderer() 首次创建窗口时懒调用，结果缓存到 OhosPlatform.gpu_context（后续窗口复用）；读 WGPU_BACKEND 环境变量（未设则默认 GL）建 wgpu Instance + Adapter + Device/Queue，ohos 下禁用 dual_source_blending]
渲染 到 select_adapter() 在 crates/gpui_ohos/src/ohos/wgpu_context.rs  [由 WgpuContext::new() 内部调用；按 ZED_DEVICE_ID 过滤或 request_adapter 选 GPU]
渲染 到 WgpuRenderer::new() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [由 initialize_renderer() 调用；create_surface_unsafe(RawHandle) 从 OH_NativeWindow 建 wgpu surface + get_capabilities + configure（PresentMode::Fifo）+ create_pipelines]
渲染 到 create_pipelines() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [由 WgpuRenderer::new() 内部调用；编译 shaders.wgsl 建 quads/shadows/path_rasterization/paths/underlines/mono_sprites/subpixel_sprites/poly_sprites 共 8 个 render pipeline]
渲染 到 WgpuRenderer::draw() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [由 OhosWindow::draw() 每帧调用；渲染 Scene 到 surface]
渲染 到 update_drawable_size() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [由 OhosWindow 收到 WindowResize 事件时调用；更新 surface 尺寸]
渲染 到 sprite_atlas() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [由 GPUI 获取图集时调用；返回 WgpuAtlas]
渲染 到 gpu_specs() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [由 GPUI 查询 GPU 能力时调用]
```

跨文件跳转：
```
OhosPlatform::open_window() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OhosWindow::initialize_renderer() 在 crates/gpui_ohos/src/ohos/window.rs  [创建窗口时立即初始化渲染]
OhosWindow::initialize_renderer() 在 crates/gpui_ohos/src/ohos/window.rs 到 WgpuContext::new() 在 crates/gpui_ohos/src/ohos/wgpu_context.rs  [首次懒创建，缓存到 OhosPlatform.gpu_context 供后续窗口复用]
OhosWindow::initialize_renderer() 在 crates/gpui_ohos/src/ohos/window.rs 到 WgpuRenderer::new() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs  [从 OpenHarmonyApp.native_window() 取 OH_NativeWindow，经 raw-window-handle 建 surface]
OhosWindow::draw() 在 crates/gpui_ohos/src/ohos/window.rs 到 WgpuRenderer::draw() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs
```

跨运行时跳转：
```
OhosWindow::initialize_renderer() 到 [OpenHarmonyApp.native_window()] 到 wgpu SurfaceTargetUnsafe::RawHandle  [OH_NativeWindow 指针 → wgpu GL surface]
```

### 按需渲染与 vsync 驱动（避免空转渲染）

XComponent `on_frame_callback`（`OH_NativeXComponent_RegisterOnFrameCallback`）一旦注册就持续每帧回调。为免空转渲染（CPU 高），OHOS 移植实现按需注册/注销帧回调，由 GPUI 的 `frame_waker`（`wake_platform` 信号）驱动；启停挂在独立 `VisibilityChanged` 事件（windowVisibilityChange）上，不挂在 focus 事件，`enable/disable_frame_callback` 内部用 `FRAME_CALLBACK_ENABLED`（AtomicBool）做幂等（修复最小化时 DisplaySync DelFromPipeline nullptr，见 bugfix 2026-08-21 后续修复章节）。

```
渲染 到 OhosWindowHandle::frame_waker() 在 crates/gpui_ohos/src/ohos/window.rs  [由 GPUI 需要渲染时（wake_platform：dirty/动画）调用；无条件置 PENDING_REDRAW=true（隐藏期间也不丢帧），窗口可见（window_visibility()）时 enable_frame_callback 注册帧回调（内部幂等）]
渲染 到 OpenHarmonyApp::enable_frame_callback() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs  [由 frame_waker / VisibilityChanged(true) 调用；is_frame_callback_enabled 幂等门控（已注册直接 return），注册成功才置 enabled 标志；回调仅在有渲染需求（enabled）且 surface 活跃时发 WindowRedraw]
渲染 到 OpenHarmonyApp::disable_frame_callback() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs  [由 WindowRedraw 消费完无后续需求（PENDING_REDRAW 清空）/ VisibilityChanged(false) 调用；is_frame_callback_enabled 幂等门控（已注销直接 return），注销后清 enabled 标志；OH_NativeXComponent_UnregisterOnFrameCallback 真正注销，空闲完全停止唤醒主线程；幂等防重复注销（DisplaySync DelFromPipeline nullptr 修复）]
渲染 到 window_visibility() / set_window_visibility() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/input/mod.rs  [由 frame_waker 可见性判断 / lifecycle 的 windowVisibilityChange 回调调用；ArkTS windowVisibilityChange 事件驱动，true=可见 false=隐藏]
渲染 到 WindowRedraw 处理 在 crates/gpui_ohos/src/ohos/window.rs  [由 XComponent on_frame_callback 产生 Event::WindowRedraw；request_frame 后 PENDING_REDRAW.swap(false)，无后续需求则 disable_frame_callback（内部幂等），不再外部 set enabled 标志]
```

跨文件跳转：
```
OhosWindowHandle::frame_waker() 在 crates/gpui_ohos/src/ohos/window.rs 到 OpenHarmonyApp::enable_frame_callback() 在 openharmony-ability crates/ability/src/app.rs  [经 openharmony_ability::global_app() 取 app]
OhosWindow WindowRedraw 分支 在 crates/gpui_ohos/src/ohos/window.rs 到 OpenHarmonyApp::disable_frame_callback() 在 openharmony-ability crates/ability/src/app.rs  [PENDING_REDRAW 无后续需求时]
NativeAbility.ets 的 onWindowVisibilityChange 到 lifecycle.rs 的 on_window_visibility_change 回调 在 openharmony-ability crates/ability/src/lifecycle.rs  [ArkTS win.on('windowVisibilityChange') → NAPI → 存 WINDOW_VISIBLE + 发 Event::VisibilityChanged(visible)；不再路由 GainedFocus/LostFocus（focus 回归纯焦点语义）]
```

跨运行时跳转：
```
GPUI wake_platform → [frame_waker 闭包] → XComponent on_frame_callback 注册  [GPUI 需要帧时按需注册 vsync 回调]
XComponent on_frame_callback → [Event::WindowRedraw] → OhosWindow::handle_event  [每帧 vsync 仅在有渲染需求时投递]
ArkTS win.on('windowVisibilityChange') → [NAPI on_window_visibility_change → Event::VisibilityChanged] → OhosWindow::handle_event  [窗口最小化（false）/ 恢复（true）时经独立可见性事件启停帧回调（false→disable_frame_callback，true 且 PENDING_REDRAW→enable_frame_callback），不再经 focus 事件，避免重复注销 DisplaySync]
```

### 字体与文本模块（cosmic-text）

```
文本 到 OhosTextSystem::new() 在 crates/gpui_ohos/src/ohos/text_system.rs  [由 OhosPlatform::new() 在平台初始化时调用；创建 cosmic-text FontSystem + SwashCache]
文本 到 ensure_system_fonts_loaded() 在 crates/gpui_ohos/src/ohos/text_system.rs  [首次 font_id 查询时调用；扫描 /system/fonts 等目录 + fontdb load_system_fonts]
文本 到 font_id() 在 crates/gpui_ohos/src/ohos/text_system.rs  [由 GPUI 请求字体时调用；按 family/weight/style 用 font-kit find_best_match 打分]
文本 到 layout_line() 在 crates/gpui_ohos/src/ohos/text_system.rs  [由 GPUI 排版文本行时调用；cosmic-text ShapeLine 布局]
文本 到 rasterize_glyph() 在 crates/gpui_ohos/src/ohos/text_system.rs  [由 GPUI 光栅化字形时调用；SwashCache 取位图，emoji 交换 BGRA]
文本 到 font_metrics() / advance() / glyph_for_char() 在 crates/gpui_ohos/src/ohos/text_system.rs  [由 GPUI 查询字体度量/步进/字形映射时调用]
```

跨文件跳转：
```
OhosPlatform::new() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OhosTextSystem::new() 在 crates/gpui_ohos/src/ohos/text_system.rs
```

### 输入模块（触摸屏 / 鼠标 / 触控板 / 键盘 / IME）

每个物理外设走自己的事件通道，不共用：触摸屏走 `DispatchTouchEvent`（NDK touch）、鼠标走 `RegisterMouseEventCallback`、键盘走 `RegisterKeyEventCallback`、触控板/滚轮走 `RegisterUIInputEventCallback`（Axis，该回调仅支持 Axis）。

触摸屏（`InputEvent::TouchEvent` → GPUI Mouse/Scroll，手指触摸专属通道）：
```
触摸屏 到 dispatch_touch_event() C 回调 在 crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs  [系统触摸事件进 Rust 第一站；构造 TouchEventData 转发给 X_COMPONENT_CALLBACKS.dispatch_touch_event]
触摸屏 到 on_touch_event 闭包 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs  [XComponent DispatchTouchEvent 回调；仅 tool_type == Finger 的触摸才推 Event::Input(TouchEvent) 入 event_loop；鼠标/触控板被此过滤排除，各自走独立通道]
触摸屏 到 handle_input_event() 的 TouchEvent 分支 在 crates/gpui_ohos/src/ohos/window.rs  [Down→MouseDown、Move→拖动、Up→MouseUp/滚动/惯性；触摸 slop 检测区分点击与滚动，滚动经 dispatch_touch_scroll_wheel]
```

鼠标（`InputEvent::MouseEvent` → GPUI MouseMove/MouseDown/MouseUp，独立通道）：
```
鼠标 到 on_mouse_event() C 回调 在 crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs  [系统鼠标事件进 Rust 第一站；查询 ExtraMouseEventInfo 修饰键填 data.modifiers，构造 MouseEventData(含 button_mask) 转发给 X_COMPONENT_CALLBACKS.on_mouse_event]
鼠标 到 on_mouse_event 闭包 + register_mouse_event_callback() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs  [XComponent RegisterMouseEventCallback；推 Event::Input(MouseEvent) 入 event_loop]
鼠标 到 handle_input_event() 的 MouseEvent 分支 在 crates/gpui_ohos/src/ohos/window.rs  [Move→MouseMove(pressed_button 从 button_mask 解析)、Press→MouseDown、Release→MouseUp；修饰键从事件即时查询，不缓存状态]
鼠标 到 pressed_button_from_mask() / modifiers_from_key_mask() 在 crates/gpui_ohos/src/ohos/window.rs  [Move 事件从 button 位掩码即时解析按下的键；修饰键从事件携带值重建]
鼠标 到 ClickTracker::on_button_press/current_count/on_click_complete 在 crates/gpui_ohos/src/ohos/window.rs  [双击/三击选词计数器；400ms/5px 阈值对齐 Linux，鼠标 Press/Release 与触摸屏 Down/Up 共享；Press 算 count、Release 记新双击基线]
```

触控板 / 鼠标滚轮（`InputEvent::AxisEvent` → GPUI ScrollWheel，Axis 通道按 tool_type 区分设备）：
```
滚轮 到 on_ui_input_event() C 回调 在 crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs  [UIInputEvent(Axis) 进 Rust 第一站；转发给 X_COMPONENT_CALLBACKS.on_ui_input_event]
滚轮 到 on_ui_input_event(UIInputEvent::Axis) 闭包 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs  [OH_NativeXComponent_RegisterUIInputEventCallback 仅支持 Axis 事件；注册 UIInputEvent::Axis，推 InputEvent 入 event_loop]
滚轮 到 ui_input_event_to_input_event() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/input/mod.rs  [Axis 事件转 AxisEventData；tool_type Touchpad→AxisToolType::Touchpad、Mouse→AxisToolType::Mouse，区分触控板双指与鼠标滚轮]
滚轮 到 handle_input_event() 的 AxisEvent 分支 在 crates/gpui_ohos/src/ohos/window.rs  [转 ScrollWheelEvent 分发]
滚轮 到 handle_axis_input() 在 crates/gpui_ohos/src/ohos/window.rs  [modifiers.shift 按下时 swap 横纵轴（shift+滚轮水平滚动，对齐 gpui_linux）；Mouse→ScrollDelta::Lines(每 120 单位 3 行)、Touchpad→ScrollDelta::Pixels(1:1 不放大)；两分支对 scroll_vertical/horizontal 取反与触摸屏方向一致]
```

IME 输入（ArkTS 插件持有 `InputMethodController`，控制面与输入面各占一条 NAPI 桥；不再是 NDK/TSFN 路径）：
```
IME 到 ImePlugin 注册 在 hap/entry/src/main/ets/entryability/EntryAbility.ets  [EntryAbility.bridgePlugins 以 LazyPlugin 登记；窗口 stage 创建时由 NativeAbility 安装]
IME 到 onInstall() 在 crates/gpui_ohos/depend/openharmony-ability/plugins/ime/src/main/ets/ImePlugin.ets  [插件安装时调用一次；注册 windowSizeChange/windowRectChange 用于重算候选框位置]
IME 到 invokeAsync() 在 .../plugins/ime/src/main/ets/ImePlugin.ets  [Rust 经异步桥调用；按 action 分派 attach / detach / update-cursor]
IME 到 attach() → bindWithRetries() 在 .../ImePlugin.ets  [Rust 请求 attach；attachWithUIContext + showTextInput 后 attached=true，回调注册由 callbacksRegistered 单独保证只做一次。已 attached 的请求也必须真正重绑——窗口隐藏时系统会收走会话而该标志仍为 true（2026-09-15 修复）]
IME 到 stopInputSession() 在 .../ImePlugin.ets  [Rust 请求 detach；attached=false 并结束系统会话，controller 与回调保留复用]
IME 到 updateCursor() → computeCursorScreenPos() 在 .../ImePlugin.ets  [Rust 请求 update-cursor，或 windowSizeChange/windowRectChange 触发；窗口坐标换算成屏幕坐标后喂 controller.updateCursor]
IME 到 register_plugins() 在 crates/gpui_ohos/src/ohos/platform.rs  [OhosPlatform::new 启动时注册 ImeBridgePlugin（插件 ID "ohos.ime"）]
IME 到 ImeBridgePlugin::on_main_thread_event() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-ime/src/lib.rs  [ArkTS invokeNativeSync 送来的主线程事件入口；按事件名分派 insert-text/delete-left/delete-right/function-key/keyboard-status/preview-text]
IME 到 push_input() 在 .../crates/plugin-ime/src/lib.rs  [把 IME 回调转成 Event::Input(InputEvent::ImeEvent)，经 global_app().dispatch_input_event 入事件循环]
IME 到 ImeClient::attach()/detach()/update_cursor() 在 .../crates/plugin-ime/src/lib.rs  [控制面入口，由 ImeExt::ime() 取得；经异步桥调 ArkTS 对应 action]
IME 到 handle_input_event() 的 ImeEvent 分支 在 crates/gpui_ohos/src/ohos/window.rs  [ImeEvent 消费点；经 foreground_executor.spawn 异步处理]
IME 到 handle_ime_backspace()/handle_ime_delete_forward()/handle_ime_enter() 在 crates/gpui_ohos/src/ohos/window.rs  [有组合(marked)文本→走 IME 文本层；无组合→派发真实 backspace/delete/enter KeyDown]
IME 到 show_keyboard_if_needed() 在 crates/gpui_ohos/src/ohos/window.rs  [SurfaceCreate / 窗口获焦(GainedFocus) / update_ime_position 推光标时调用；受 ime_attached 缓存守卫防重复]
IME 到 hide_keyboard_if_needed() 在 crates/gpui_ohos/src/ohos/window.rs  [窗口失焦(LostFocus) 时调用，向 ArkTS 下发 detach]
IME 到 push_ime_cursor_rect()/refresh_ime_cursor() 在 crates/gpui_ohos/src/ohos/window.rs  [光标或窗口几何变化时把 caret rect 推给 ArkTS，驱动候选框跟随]
```

窗口生命周期与 IME（关键时序，决定输入法能否被重新激活）：
```
IME 到 onWindowStageEvent() 在 crates/gpui_ohos/depend/openharmony-ability/native_ability/src/main/ets/ability/NativeAbility.ets  [windowStage 注册；windowStageEvent 与 windowVisibilityChange 都由这里转发]
IME 到 window_stage_event 闭包 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lifecycle.rs  [ArkTS 送来的 event_type 原始整数映射为 Event：SHOWN(1)→Start、ACTIVE(2)→GainedFocus、INACTIVE(3)→LostFocus、HIDDEN(4)→Stop]
```
实测时序（tablet）：最小化 `INACTIVE`→`HIDDEN`；恢复 `SHOWN`→`ACTIVE`，且 `windowVisibilityChange(true)` 比 `SHOWN` 晚约 30ms、`ACTIVE` 再晚约 100ms。**只有 `ACTIVE`(GainedFocus) 处于「窗口已可见且已获焦」**，窗口恢复后的 IME 会话只有在此时建立才不会失败；在 `SHOWN` 上发起 attach 会落在「不可见、未获焦」的空窗——`attachWithUIContext`/`showTextInput` 不抛异常、ack 也正常，但系统不建会话。

跨运行时跳转：
```
IME 到 ImePlugin.invokeAsync() 在 .../plugins/ime/src/main/ets/ImePlugin.ets 到 [ohos.ime attach / detach / update-cursor] 到 ImeClient::attach()/detach()/update_cursor() 在 .../crates/plugin-ime/src/lib.rs  [控制面：Rust → ArkTS 异步桥]
IME 到 registerCallbacksOnce() 注册的回调 在 .../ImePlugin.ets 到 [insert-text / delete-left / delete-right / function-key / keyboard-status / preview-text] 到 ImeBridgePlugin::on_main_thread_event() 在 .../crates/plugin-ime/src/lib.rs  [输入面：ArkTS → Rust 主线程同步桥；回调不可注销，重复注册会让一次按键裂成 N 个 insert-text]
IME 到 push_input() 在 .../crates/plugin-ime/src/lib.rs 到 [Event::Input(InputEvent::ImeEvent)] 到 OhosWindow::handle_input_event() 在 crates/gpui_ohos/src/ohos/window.rs
IME 到 window_stage_event 闭包 在 .../crates/ability/src/lifecycle.rs 到 [Event::GainedFocus / Event::LostFocus] 到 OhosWindow::handle_event() 在 crates/gpui_ohos/src/ohos/window.rs  [窗口前后台与获焦变化驱动 IME 的拆除与重建]
```

键盘输入（物理按键 `InputEvent::KeyEvent` → GPUI Keystroke + 文本兜底）：
```
键盘 到 on_key_event() 注册方法 在 crates/gpui_ohos/depend/ohos-xcomponent-binding/src/native_xcomponent.rs  [封装 OH_NativeXComponent_RegisterKeyEventCallback；由 xcomponent.rs render() 初始化时调用，把 C 回调挂到 XComponent 上]
键盘 到 key_event() C 回调 在 crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs  [NDK 按键事件进入 Rust 第一站；查询 code/action/modifier_state/capslock 构造 KeyEventData，转发给 X_COMPONENT_CALLBACKS.on_key_event]
键盘 到 on_key_event 闭包 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs  [render() 里 xcomponent.on_key_event(...) 注册的 Rust 闭包；XComponent 按键事件触发，推 Event::Input(InputEvent::KeyEvent) 入 event_loop]
键盘 到 handle_input_event() 的 KeyEvent 分支 在 crates/gpui_ohos/src/ohos/window.rs  [收到 KeyDown 时调用；无状态处理，不缓存任何修饰键状态]
键盘 到 key_event_to_keystroke() 在 crates/gpui_ohos/src/ohos/keycodes.rs  [KeyEvent → GPUI Keystroke；key_char 字母大小写按 shift XOR capslock 计算，与桌面 xkb 一致；功能键（方向键/回车/删除/F1-F12）key_char 为 None]
键盘 到 dispatch_input() 在 crates/gpui_ohos/src/ohos/window.rs  [KeyDown 分发到 GPUI；GPUI 未消费（propagate）且 key_char 有值且修饰键只含 shift 时，X11 式兜底：input_handler.replace_text_in_range(None, key_char) 输入字符]
键盘 到 modifiers_from_modifier_state() 在 crates/gpui_ohos/src/ohos/keycodes.rs  [modifier_state 位掩码 → GPUI Modifiers；每次按键补发 ModifiersChanged(完整状态) 给 GPUI，先于 KeyDown]
键盘 到 capslock() 在 crates/gpui_ohos/src/ohos/window.rs  [PlatformWindow::capslock 当前恒返回 default；capslock 状态经 ModifiersChanged 同步给 GPUI]
键盘 到 begin_key_repeat()/end_key_repeat()/is_modifier_key() 在 crates/gpui_ohos/src/ohos/window.rs  [OHOS KeyAction 无 repeat 位，合成 auto-repeat：KeyDown 记录 code+generation，spawn 任务延迟 KEY_REPEAT_DELAY(500ms) 后每 KEY_REPEAT_INTERVAL(33ms) 分发 KeyDownEvent{is_held:true}；KeyUp/换键/窗口销毁经 generation+window_alive 取消；修饰键(Ctrl/Shift/Alt/Meta/CapsLock/Fn)永不重复]
```

键盘焦点与输入路由（关键约束，影响按键是否到达 Rust）：
```
焦点 到 set_focusable/set_default_focus 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs  [XComponent 必须可聚焦并成为默认焦点；否则系统把按键路由给 ArkTS 层焦点节点（DefaultXComponent 的 Row/secure_field），NDK on_key_event 永不触发（2026-08-18 修复）]
焦点 到 window.handle_input() 在 crates/gpui/src/window.rs  [编辑器渲染时注册 input handler，仅当 focus_handle.is_focused 时生效；draw 结束 set_input_handler 到平台窗口，字符输入/IME 依赖它存在]
键盘 到 OH_NativeXComponent_GetKeyEventCapsLockState 在 crates/gpui_ohos/depend/ohos-xcomponent-binding/src/events/native_callbacks.rs  [查询按键事件时的 CapsLock 状态入 KeyEventData.capslock；modifier_state 无 capslock 位]
```

跨运行时跳转：
```
XComponent 四回调(DispatchTouchEvent / RegisterMouseEventCallback / RegisterKeyEventCallback / RegisterUIInputEventCallback-Axis) 到 [Event::Input(InputEvent)] 到 OhosWindow::handle_input_event() 在 crates/gpui_ohos/src/ohos/window.rs  [四类设备输入经 NDK C 回调(native_callbacks.rs) → xcomponent.rs 闭包 → event_loop → 窗口；触摸屏/鼠标/键盘/触控板各占一个回调，互不共用]
OpenHarmonyApp run_loop 到 [Event::Input(InputEvent)] 到 OhosWindow::handle_input_event() 在 crates/gpui_ohos/src/ohos/window.rs  [ArkTS/NDK 输入事件经 NAPI → run_loop → 窗口]
OhosWindow::handle_input_event() 到 [foreground_executor.spawn] 到 InputHandler 方法  [IME 处理在异步任务中执行]
OhosWindow::dispatch_input() 到 [PlatformInput::KeyDown] 到 gpui dispatch_event() 在 crates/gpui/src/window.rs  [进入 GPUI 按键分发（binding 匹配/key_listener）]
OhosWindow::dispatch_input() 兜底 到 input_handler.replace_text_in_range() 在 crates/gpui/src/platform.rs  [GPUI 未消费的纯字符键 → 文本输入；与 IME 输入共用同一 input handler 通道]
```

### 手势与拖放插件（pinch / filedrop，openharmony-ability plugin）

捏合缩放和文件拖放走 openharmony-ability 插件架构：ArkTS 透明 overlay 绑定手势/拖放事件 → `invokeNativeSync` → Rust `on_main_thread_event` → `thread_local!` 回调 → `OhosWindow::register_platform_event_handlers` 注册的回调 → GPUI 分发。全部**事件驱动，无轮询**；`on_main_thread_event` 恒在主线程，回调存 `thread_local!`（可捕获 `Rc` 窗口状态）。

捏合（pinch）：
```
捏合 到 PinchPlugin.ets onAction 在 crates/gpui_ohos/depend/openharmony-ability/plugins/pinch/src/main/ets/PinchPlugin.ets  [透明 overlay 绑定 PinchGesture；invokeNativeSync(pinch-begin/update/end, PinchSample{scale, center_x, center_y})]
捏合 到 PinchBridgePlugin::on_main_thread_event 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-pinch/src/lib.rs  [解析 PinchSample；Begin/End 重置 LAST_SCALE、Update 算增量 delta = scale - last_scale；thread_local PINCH_CALLBACK 回调]
捏合 到 dispatch_pinch_event() 在 crates/gpui_ohos/src/ohos/window.rs  [累积 delta，越过 PINCH_ZOOM_THRESHOLD(0.15) 才发一步 Ctrl+ScrollWheel(Lines±1)；End 清零累积器；GPUI editor 不消费 PinchEvent，转 Ctrl+滚轮实现 zoom]
捏合 到 register_platform_event_handlers() 在 crates/gpui_ohos/src/ohos/window.rs  [set_pinch_callback 注册 thread_local 回调，主线程事件驱动]
```

文件拖放（filedrop）：
```
拖放 到 FileDropPlugin.ets onDragEnter/onDragMove/onDrop 在 crates/gpui_ohos/depend/openharmony-ability/plugins/filedrop/src/main/ets/FileDropPlugin.ets  [透明 overlay 绑定拖放事件；onDrop 用 event.getData().getRecords() 取 File URI + getWindowX/Y 取位置，invokeNativeSync(drag-enter/drag-move/drop-files)]
拖放 到 FileDropBridgePlugin::on_main_thread_event 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-filedrop/src/lib.rs  [解析 DragMoveData/DropFilesData；构造 FileDropEventData(Enter/Move/Drop) 经 thread_local FILEDROP_CALLBACK 回调]
拖放 到 dispatch_filedrop_enter/dispatch_filedrop_move/dispatch_drop_files 在 crates/gpui_ohos/src/ohos/window.rs  [Enter→空 paths FileDrop::Entered 建 drag 态；Move→MouseMove+FileDrop::Pending 保持 hover 链；Drop→path_from_uri 解析授权→带路径 Entered+Submit 同步触发 on_drop]
拖放 到 path_from_uri() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs  [OH_FileUri_GetPathFromUri 映射 URI→沙箱路径 + OH_FileShare_PersistPermission 持久化授权]
拖放 到 gpui dispatch_event() 的 FileDrop 分支 在 crates/gpui/src/window.rs  [首次 Entered 建 active_drag（空 paths）；后续 Entered 在 active_drag 已存在且载荷为 ExternalPaths 时刷新 paths（2026-08-21 修复）；Submit→MouseUp→on_drop→handle_external_paths_drop→open_paths]
```

### 光标模块（鼠标光标样式）

光标变化由 GPUI 驱动：鼠标移动改变 hover 元素 → `reset_cursor_style` → `Platform::set_cursor_style`。两个前置条件缺一不可——① 窗口 hovered 为 true（由 XComponent `DispatchHoverEvent` 驱动）；② `is_window_hovered()` 返回 true（OHOS 单窗口恒 true，见 `OhosWindow::is_hovered`）。windowId 由 ArkTS `CursorPlugin` 在窗口 stage 就绪时一次性传给 Rust，之后每次改样式都是纯系统调用。

```
光标 到 set_cursor_style() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 GPUI reset_cursor_style 在鼠标 hover 元素变化时调用（Platform trait）；cursor_style_to_pointer_style 映射 21 个 CursorStyle → Input_PointerStyle → last_cursor_style 去重（相同跳过）→ app.set_cursor_style（CursorExt）]
光标 到 CursorExt::set_cursor_style() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/src/lib.rs  [由 OhosPlatform::set_cursor_style 调用；读 WINDOW_ID 全局（None 时 warn 返回 false）→ OH_Input_SetPointerStyle(window_id, pointer_style)，非 0 返回值 log error]
光标 到 handle_hover_event() 在 crates/gpui_ohos/src/ohos/window.rs  [由 handle_input_event 的 HoverEvent 分支调用；仿 Linux set_hovered（take 回调 → 调用 → 放回），触发 hover_status_change(is_hover) 更新 GPUI hovered]
光标 到 handle_ohos_event() 的 HoverEvent(false) 分支 在 crates/gpui_ohos/src/ohos/platform.rs  [鼠标离开窗口时重置 last_cursor_style 去重缓存 + 恢复默认光标（pointer_style=0），避免再次进入时去重跳过导致光标不恢复]
光标 到 cursor_style_to_pointer_style() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 set_cursor_style 调用；CursorStyle 各变体 → OHOS Input_PointerStyle 枚举值（Arrow→0 / PointingHand→19 / IBeam→26 等）]
```

windowId 初始化链路（ArkTS → Rust，只在窗口 stage 就绪时发生一次）：
```
光标 到 CursorBridgePlugin::on_main_thread_event() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/src/lib.rs  [由 ArkTS CursorPlugin.onInstall 经 invokeNativeSync 发 "window-id" 事件触发；decode::<i32> 存入 WINDOW_ID，响应 WindowIdResponse；WindowStageDestroyed 生命周期清空]
```

跨文件跳转：
```
OhosPlatform::set_cursor_style() 在 crates/gpui_ohos/src/ohos/platform.rs 到 CursorExt::set_cursor_style() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/src/lib.rs  [CursorExt trait 扩展 OpenHarmonyApp，由 openharmony-ability-plugin-cursor crate 提供]
OhosWindow::handle_hover_event() 在 crates/gpui_ohos/src/ohos/window.rs 到 hover_status_change 回调 在 crates/gpui/src/window.rs  [GPUI 注册的回调：window.hovered.set(is_hover) + window.refresh()；is_window_hovered 读 hovered 决定 reset_cursor_style 是否生效]
OhosWindow::handle_mouse_input() 在 crates/gpui_ohos/src/ohos/window.rs 到 dispatch_input(MouseMove) 在 crates/gpui/src/window.rs  [鼠标移动进 GPUI → dispatch_mouse_event → hit_test 变化才 reset_cursor_style → cx.platform.set_cursor_style]
```

跨运行时跳转：
```
CursorPlugin.onInstall() 在 crates/gpui_ohos/depend/openharmony-ability/plugins/cursor/src/main/ets/CursorPlugin.ets 到 ["window-id" main-thread 事件] 到 CursorBridgePlugin::on_main_thread_event() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/src/lib.rs  [ArkTS getWindow().getWindowProperties().id（getWindow 失败回退 getWindowStage().getMainWindowSync()）→ invokeNativeSync → Rust 插件 on_main_thread_event]
XComponent DispatchHoverEvent 到 [Event::Input(HoverEvent(bool))] 到 OhosWindow::handle_hover_event() 在 crates/gpui_ohos/src/ohos/window.rs  [系统鼠标进入/离开窗口 → native_callbacks on_hover_event → xcomponent.rs on_hover_event 闭包 → event_loop → 窗口；is_hover=true/false 驱动 GPUI hovered（仿 X11 XinputEnter/Leave → set_hovered）]
XComponent DispatchMouseEvent 到 [Event::Input(MouseEvent)] 到 OhosWindow::handle_mouse_input() 在 crates/gpui_ohos/src/ohos/window.rs  [系统鼠标移动/按键 → native_callbacks on_mouse_event → xcomponent.rs on_mouse_event 闭包 → event_loop → 窗口 → GPUI MouseMove/Down/Up]
CursorExt::set_cursor_style() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-cursor/src/lib.rs 到 [OH_Input_SetPointerStyle] 到 系统光标服务  [libohinput.so（API 22+）；window_id + Input_PointerStyle 值（0=默认 / 19=手型 / 26=IBeam）]
```

### 窗口与事件模块

```
窗口 到 OhosWindow::new() 在 crates/gpui_ohos/src/ohos/window.rs  [由 OhosPlatform::open_window() 调用；记录 bounds/scale，renderer 延迟到 SurfaceCreate 初始化]
窗口 到 handle_event() 在 crates/gpui_ohos/src/ohos/window.rs  [由 OhosPlatform::handle_ohos_event() 对每个存活窗口调用；分发 SurfaceCreate/WindowResize/ContentRectChange/AvoidAreaChange/WindowRedraw/Input/GainedFocus/LostFocus/VisibilityChanged/ConfigChanged/WindowDestroy/KeyboardEvent；VisibilityChanged 分支启停帧回调]
窗口 到 initialize_renderer() 在 crates/gpui_ohos/src/ohos/window.rs  [由 SurfaceCreate 事件或首次 draw() 调用；从 native_window 建 WgpuRenderer]
窗口 到 keyboard_overlap_from_avoid_area_device_px() 在 crates/gpui_ohos/src/ohos/window.rs  [由 AvoidAreaChange/KeyboardEvent 事件调用；计算软键盘+系统手势遮挡区间并集，驱动 insets]
窗口 到 emit_resize_callback() 在 crates/gpui_ohos/src/ohos/window.rs  [由尺寸/遮挡变化时调用；通知 GPUI resize 回调]
窗口 到 dispatch_input_with_callbacks() 在 crates/gpui_ohos/src/ohos/window.rs  [分发输入事件到 on_input 回调]
```

跨文件跳转：
```
OhosPlatform::handle_ohos_event() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OhosWindow::handle_event() 在 crates/gpui_ohos/src/ohos/window.rs  [事件路由到每个窗口]
OhosPlatform::open_window() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OhosWindow::new() 在 crates/gpui_ohos/src/ohos/window.rs  [创建窗口并立即 initialize_renderer]
OhosWindow::handle_event() 的 WindowResize 分支 到 WgpuRenderer::update_drawable_size() 在 crates/gpui_ohos/src/ohos/wgpu_renderer.rs
```

### 调度器与执行器模块

```
调度 到 OhosDispatcher::new() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 OhosPlatform::new() 调用；创建主线程优先级队列 + 驻留 worker pool + TSFN waker（无独立定时器线程，延迟任务走 FFRT，见定时器模块）]
调度 到 set_waker() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 OhosPlatform::set_app() 调用；注册 OpenHarmonyWaker 唤醒主线程]
调度 到 dispatch() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 GPUI 后台任务调用；按 High/Medium/Low 档位交给驻留 worker pool 执行，不经主线程]
调度 到 dispatch_on_main_thread() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由要求在主线程执行的 GPUI 任务调用；进 PriorityQueueSender 后 waker.wake()，由 run_loop 消费]
调度 到 run_foreground_tasks() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 handle_ohos_event 的 UserEvent 分支调用；从 main_receiver 取出排队的 foreground 任务逐条执行]
调度 到 execute_runnable() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 run_foreground_tasks() 在主线程调用；runnable.run() 执行 GPUI foreground 任务]
调度 到 WorkerPool 在 crates/gpui_ohos/depend/openharmony-ability/crates/worker-pool/src/lib.rs  [由 OhosDispatcher::new() 创建：线程名前缀 gpui-ohos-bg（每 worker 名为 {prefix}-{index}），worker 数 = available_parallelism().clamp(1,4)（查询失败回退 1）；worker 空闲时阻塞在 condvar 上，不空转]
```

跨运行时跳转：
```
OhosDispatcher::dispatch_on_main_thread() 到 [main_sender + OpenHarmonyWaker::wake] 到 OpenHarmonyApp run_loop 收 UserEvent  [入队后唤醒主线程；wake 每次实时读全局 WAKER（见常见坑 WAKER 时序）]
OhosDispatcher::dispatch() 到 [worker pool 三档优先级加权抽签（High 60 / Medium 30 / Low 10）] 到 gpui-ohos-bg-N worker 线程  [后台任务在驻留 worker 线程执行。Priority::RealtimeAudio 正常由 spawn_realtime 分流不会走 dispatch，若真到达则用独立 thread::spawn 兜底]
OhosDispatcher::dispatch_after() 到 [ffrt_timer_start] 到 FFRT worker 线程  [见定时器模块；FFRT 不可用时 inline 执行 callback]
```

注（2026-09-20 修正）：本节此前写「`dispatch` 每个任务 `std::thread::spawn` 一个新线程执行（不经主线程，与 Linux 的 Worker 线程池不同）」，已过期——现为驻留 worker pool，代码注释自述这是替换 per-task spawn（`crates/gpui_ohos/src/ohos/dispatcher.rs:43-44`）。`execute_runnable` 此前记为「由 FFRT 定时回调 / run_loop 消费任务时调用」，实际只由 `run_foreground_tasks()` 在 `crates/gpui_ohos/src/ohos/platform.rs` 调用（FFRT 回调走的是 `dispatch_after` 自己的闭包，不经 `execute_runnable`）。

### GPUI 平台后端接口（trait 定义 → OHOS 实现位置）

GPUI 在 `crates/gpui/src` 下共 54 个 `pub trait`，其中只有 13 个是需要平台 crate 实现的平台后端接口；其余（`Action`、`Element`、`Render`、`Global`、`View`、`Focusable` 等）是框架/应用层，与平台无关。

```
平台接口 到 Platform 在 crates/gpui/src/platform.rs:125 到 OhosPlatform 在 crates/gpui_ohos/src/ohos/platform.rs  [平台总入口：窗口 / 剪贴板 / 凭据 / 菜单 / 光标 / 通知等]
平台接口 到 PlatformWindow 在 crates/gpui/src/platform.rs:804 到 OhosWindowHandle / OhosWindow 在 crates/gpui_ohos/src/ohos/window.rs  [supertrait 是 raw-window-handle 的 HasWindowHandle + HasDisplayHandle，OHOS 亦已实现]
平台接口 到 PlatformDispatcher 在 crates/gpui/src/platform.rs:1013 到 OhosDispatcher 在 crates/gpui_ohos/src/ohos/dispatcher.rs
平台接口 到 PlatformDisplay 在 crates/gpui/src/platform.rs:332 到 OhosDisplay 在 crates/gpui_ohos/src/ohos/display.rs
平台接口 到 PlatformTextSystem 在 crates/gpui/src/platform.rs:1056 到 OhosTextSystem 在 crates/gpui_ohos/src/ohos/text_system.rs
平台接口 到 PlatformAtlas 在 crates/gpui/src/platform.rs:1308 到 crates/gpui_ohos/src/ohos/wgpu_atlas.rs
平台接口 到 PlatformKeyboardLayout / PlatformKeyboardMapper 在 crates/gpui/src/platform/keyboard.rs:6,:14 到 crates/gpui_ohos/src/ohos/keyboard.rs
平台接口 到 InputHandler 在 crates/gpui/src/platform.rs:1651  [无需平台实现：GPUI 自身在 crates/gpui/src/input.rs:117 提供 ElementInputHandler；平台只通过 PlatformWindow::set_input_handler 收下 PlatformInputHandler，并在 IME 事件里回调它]
平台接口 到 PlatformGestures 在 crates/gpui/src/gestures.rs:178  [OHOS 无 impl；该 trait 方法全有默认实现，Platform::gestures() 默认返回 None，不构成缺口]
```

OHOS 未实现、且与 Linux/Windows 齐平（非 OHOS 特有缺口）：
```
平台接口 到 ScreenCaptureSource / ScreenCaptureStream 在 crates/gpui/src/platform.rs:426,:440  [仅 macOS 实现（crates/gpui_macos/src/screen_capture.rs），Linux/Windows 均无；OHOS 在 crates/gpui_ohos/src/ohos/platform.rs 的 screen_capture_sources() 显式返回 Err("Screen capture not supported on OHOS")]
平台接口 到 PlatformHeadlessRenderer 在 crates/gpui/src/platform.rs:977  [仅 gpui 自身测试/bench 使用（crates/gpui/src/platform/test/platform.rs、crates/gpui/src/app/bench_context.rs），无任何生产平台实现]
```

审计口径（2026-09-20）：13 个接口中 8 个已实现、1 个无需平台实现（InputHandler）、1 个全走默认实现（PlatformGestures）、3 个未实现（上述）。**必须实现的方法共 117 个，按「与 Linux/Windows 齐平」口径 OHOS 真缺口为 0**（未实现的 6 个方法全落在上述 3 个 trait 里）。另有约 50 个方法（Platform 12 个、PlatformWindow 35 个，含 `on_app_lifecycle`、`on_memory_warning`、`show_soft_keyboard`/`hide_soft_keyboard`、`set_back_handler`、`a11y_*`）OHOS 走的是 trait 默认实现——属可选能力而非缺口，但这些是移动平台语义相关项，留待逐个确认是否有意省略。

### Profiler 采样链路（任务耗时统计与消费方）

GPUI 的任务耗时统计是「平台执行点上报 → 按线程全局存储 → 上层读取」三段式。**OHOS 曾完全缺席上报**，导致 profiler 在 OHOS 上读不到任何任务（见常见坑）。

```
Profiler 到 update_running_task() / save_task_timing() 在 crates/gpui/src/profiler.rs:664,:671  [全局自由函数、无 cfg 门控；未启用 profiler feature 时内部为空实现。必须在 runnable.run() 前后成对调用——save_task_timing 内部对 running 做 expect，只 save 不 update（或同线程任务嵌套）会 panic]
Profiler 到 OhosDispatcher 的 3 个执行点 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [execute_runnable（主线程）、dispatch（worker pool 闭包）、dispatch_after（FFRT 回调）三处各自成对上报；与 crates/gpui_windows/src/dispatcher.rs:91-98 逐字同构]
Profiler 到 THREAD_TIMINGS / GLOBAL_THREAD_TIMINGS 在 crates/gpui/src/profiler.rs:509,:529  [thread_local，每线程各自一把 spin 锁；线程首次上报时把 Weak 句柄注册进全局表。逐条任务历史仅在 set_trace_enabled(true) 时才保留]
```

消费方（两条路径语义不同，改动时勿混淆）：
```
Profiler 到 take_all_stats() 在 crates/gpui/src/profiler.rs:42 到 hang_detection 在 crates/zed/src/reliability/hang_detection.rs:105  [每 monitor_interval 取一次；collect_and_reset 语义——取走即清空，用于 telemetry 上报与触发 hang-*.miniprof.json 落盘]
Profiler 到 get_all_timings() 在 crates/gpui/src/profiler.rs:30 到 task_traces 在 crates/zed/src/reliability/hang_detection/task_traces.rs  [只读不重置；传 TasksIncluded::CompletedAndRunning 时会把「当前正在执行、尚未返回的任务」合成为一条，这是卡死在单次 poll 里的任务唯一可见的途径]
Profiler 到 get_all_timings() 到 miniprofiler_ui 在 crates/miniprofiler_ui/src/miniprofiler_ui.rs  [**OHOS 上不可用**：它靠新开窗口展示 profiler，而 OHOS 拒绝第二窗口（见平台事件循环节 open_window）]
Profiler 到 spawn_profiler_sampler() 在 crates/gpui_ohos/src/ohos/platform.rs  [临时诊断设施：由 OhosPlatform::run() 启动一个名为 gpui-ohos-profiler 的线程，每 5 秒读一次 get_all_timings(CompletedAndRunning) 并 log::warn! 打印。输出两类行——executing（当前正在执行且已跑很久的任务，对应「卡死在单次 poll」型自旋）/ slowest（上轮以来完成的最慢任务，用于识别「任务都短但 CPU 高」的洪流型自旋）]
```

注：诊断时**不要**开 `set_trace_enabled(true)`——它会保留逐条任务历史（每线程上限 `MAX_TASK_TIMINGS` ≈ 16MB，见 `crates/gpui/src/profiler.rs:404`），并使 `get_all_timings` 在持 spin 锁的状态下拷贝整段历史，反过来拖慢乃至阻塞被测线程；另外任何新增的统计消费者都必须避开 `take_all_stats` 的 reset 语义，否则会抢空 hang_detection 的数据。

### 定时器模块

统一上层 API 是 GPUI 的 `ForegroundExecutor::timer` / `BackgroundExecutor::timer`（crates/gpui/src/executor.rs），内部经 `Scheduler::timer` → `PlatformScheduler::timer`（crates/gpui/src/platform_scheduler.rs）创建 oneshot 通道 + async_task Runnable，交给平台 `dispatch_after` 安排到点执行，返回 `Timer`（oneshot Receiver）。到点后 runnable 发出信号，await 该 Timer 的调用方被唤醒；定时器回调本身在平台后台线程执行，不经主线程。

```
定时器 到 ForegroundExecutor::timer() / BackgroundExecutor::timer() 在 crates/gpui/src/executor.rs  [由需要延迟/周期执行的任务调用；duration 为 0 时立即 Task::ready，否则经 scheduler.timer 下发]
定时器 到 PlatformScheduler::timer() 在 crates/gpui/src/platform_scheduler.rs  [由 executor.timer 调用；创建 oneshot 通道 + async_task Runnable，交给 dispatcher.dispatch_after 到点执行，返回 Timer]
定时器 到 LinuxDispatcher::dispatch_after() 在 crates/gpui_linux/src/linux/dispatcher.rs  [Linux 分支；calloop channel 发 TimerAfter{duration, runnable} 给 Timer 线程]
定时器 到 Timer 线程 在 crates/gpui_linux/src/linux/dispatcher.rs  [独立 calloop 事件循环线程；insert_source calloop::Timer::from_duration，到点 runnable.run()（在 Timer 线程执行）]
定时器 到 OhosDispatcher::dispatch_after() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [OHOS 分支；OpenHarmonyTimer::start 排定 FFRT 定时器，回调在 FFRT worker 线程执行 runnable.run()；FFRT 不可用时 inline fallback 执行（log::error 后直接 callback()）]
定时器 到 OpenHarmonyTimer::start() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/timer.rs  [由 OhosDispatcher::dispatch_after 调用；ffrt_timer_start(QoS=ffrt_qos_default) 排定一次性回调，到点 ffrt_timer_callback 在 FFRT worker 线程执行（catch_unwind 包住），handle<0 时把回调交回调用方 fallback]
```

主要消费者（全部是 `executor.timer(...).await` 的延迟/周期任务）：
```
定时器 到 扩展 reload debounce 在 crates/extension_host/src/extension_host.rs:426,434  [扩展列表/工作区变化后 debounce 再 reload；cx.background_executor().timer(RELOAD_DEBOUNCE_DURATION)]
定时器 到 wasm epoch interruption 在 crates/extension_host/src/wasm_host.rs:579  [每 100ms 递增 wasmtime engine epoch，强制扩展让出 CPU；executor.timer(EPOCH_INTERVAL) 循环，引擎 drop 时退出]
定时器 到 键盘 auto-repeat 在 crates/gpui_ohos/src/ohos/window.rs:1853,1856  [OHOS 合成按键重复（无 KeyAction repeat 位）；background_executor.timer(KEY_REPEAT_DELAY) 后每 KEY_REPEAT_INTERVAL 分发 KeyDownEvent{is_held:true}]
定时器 到 内存占用轮询 在 crates/zed/src/reliability.rs:154  [周期采集内存占用上报；executor.timer(MEMORY_USAGE_POLL_INTERVAL)]
定时器 到 telemetry 周期上报 在 crates/zed/src/zed.rs:479  [周期上报遥测事件；cx.background_executor().timer(TELEMETRY_INTERVAL)]
```

跨运行时跳转：
```
PlatformScheduler::timer() 到 [dispatch_after + oneshot Timer] 到 await Timer 的调用方  [到点后 runnable 发出信号，await 方（foreground 或 background）被唤醒；定时器回调本身不经主线程]
LinuxDispatcher::dispatch_after() 到 [calloop channel TimerAfter] 到 Timer 线程 calloop::Timer  [Linux 专用 Timer 线程（独立 calloop 事件循环）到点执行 runnable]
OhosDispatcher::dispatch_after() 到 [ffrt_timer_start] 到 FFRT worker 线程 ffrt_timer_callback  [FFRT 定时器到点后回调在 worker 线程执行 runnable.run()；不占用 ArkTS/N-API 主线程，重定时任务不卡 UI]
```

注：`dispatch_after` 的定时任务直接在平台 Timer/FFRT 线程执行 runnable，不切主线程；只有 `dispatch_on_main_thread` 入队的任务才经 OpenHarmonyWaker 唤醒主线程。tokio 侧（reqwest 超时、wasmtime 内部 IO）用 tokio 自带定时器，不经 GPUI 调度器。

### 显示器模块

```
显示 到 OhosDisplay::new() 在 crates/gpui_ohos/src/ohos/display.rs  [由 OhosPlatform::set_app() 创建主显示器；持有 OpenHarmonyApp]
显示 到 bounds() 在 crates/gpui_ohos/src/ohos/display.rs  [由 GPUI 查询显示器边界时调用；从 app.content_rect() 换算（除以 app.scale()）]
显示 到 uuid() / visible_bounds() 在 crates/gpui_ohos/src/ohos/display.rs  [由 GPUI 查询显示器 ID/可见区域时调用]
```

跨文件跳转：
```
OhosPlatform::set_app() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OhosDisplay::new() 在 crates/gpui_ohos/src/ohos/display.rs
OhosDisplay::bounds() 在 crates/gpui_ohos/src/ohos/display.rs 到 OpenHarmonyApp::content_rect() 在 openharmony-ability crates/ability/src/app.rs
```

### NAPI 桥接模块（openharmony-ability）

```
桥接 到 OpenHarmonyApp::run_loop() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [由 OhosPlatform::run() 调用；注册 ArkTS 事件回调，收到 Event 转给 run_loop 闭包]
桥接 到 native_window() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [由 OhosWindow::initialize_renderer() 调用；返回 RawWindow(OH_NativeWindow)]
桥接 到 content_rect() / window_rect() / avoid_area() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [由 OhosDisplay/OhosWindow 查询窗口几何/遮挡时调用]
桥接 到 show_keyboard() / hide_keyboard() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [由 OhosWindow 软键盘显隐时调用]
桥接 到 create_waker() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [由 OhosPlatform::set_app() 调用；创建 TSFN waker]
桥接 到 bridge() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [由 Rust 侧调用 ArkTS 插件能力时使用；TSFN 桥接]
桥接 到 config() 在 zed-ohos-gpui/openharmony-ability/crates/ability/src/app.rs  [查询应用配置（主题/密度等）]
```

Event 枚举（`zed-ohos-gpui/openharmony-ability/crates/ability/src/event.rs`）：`WindowCreate`/`WindowDestroy`、`SurfaceCreate`/`SurfaceDestroy`、`WindowResize`、`ContentRectChange`、`AvoidAreaChange`、`ConfigChanged`、`GainedFocus`/`LostFocus`、`VisibilityChanged(bool)`、`Start`/`Stop`/`Resume`/`Pause`、`SaveState`、`Create`/`Destroy`、`Input(InputEvent)`、`KeyboardEvent(i32)`、`LowMemory`、`UserEvent`。

跨运行时跳转：
```
ArkTS 窗口/表面/输入/生命周期事件 到 [NAPI → Event 枚举] 到 OhosPlatform::handle_ohos_event() 在 crates/gpui_ohos/src/ohos/platform.rs  [openharmony-ability 把 ArkTS 侧事件封装成 Event 推给 run_loop 回调]
```

### 剪贴板模块（复制/粘贴）

```
剪贴板 到 read_from_clipboard() / write_to_clipboard() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 GPUI 编辑器复制/粘贴操作触发（Platform trait 实现）；ClipboardItem ↔ String 适配在平台层]
剪贴板 到 read_text() / write_text() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/clipboard.rs  [由 OhosPlatform::read/write_to_clipboard 调用；Rust FFI 直调 NDK C API（OH_Pasteboard + UDMF），免 READ_PASTEBOARD 权限]
```

跨文件跳转：
```
OhosPlatform::read_from_clipboard() 在 crates/gpui_ohos/src/ohos/platform.rs 到 openharmony_ability::read_text() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/clipboard.rs
OhosPlatform::write_to_clipboard() 在 crates/gpui_ohos/src/ohos/platform.rs 到 openharmony_ability::write_text() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/clipboard.rs
```

跨运行时跳转：
```
clipboard.rs 到 [NDK C API OH_Pasteboard_* / OH_UdmfData_*] 到 系统剪贴板服务  [libpasteboard.so + libudmf.so；绕过 ArkTS @ohos.pasteboard 权限校验，warp 同款方案]
```

### 文件选择器模块（打开文件/目录/保存）

```
文件选择 到 prompt_for_paths() / prompt_for_new_path() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 GPUI 打开文件/目录/保存操作触发（Ctrl+O=OpenFiles、Ctrl+K Ctrl+O=Open）；directories→OPEN_FOLDER、multiple→allow_many，foreground_executor.spawn 异步 + oneshot 返回]
文件选择 到 show_file_dialog() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-files/src/lib.rs  [由 OhosPlatform::prompt_for_paths 调用；FilesExt trait 扩展 OpenHarmonyApp，构造 FileDialogOptions 经 bridge 发给 ArkTS]
文件选择 到 path_from_uri() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs  [由 platform.rs 对返回的 URI 数组调用；OH_FileUri_GetPathFromUri 转本地路径，内部先 OH_FileShare_PersistPermission 固化授权（打开即持久化）]
文件选择 到 register_plugins() 在 crates/gpui_ohos/src/ohos/platform.rs  [由 OhosPlatform::set_app() 调用；集中注册 FilesBridgePlugin（平台能力插件一律在此注册，禁止 zed 应用层注册）]
```

跨文件跳转：
```
OhosPlatform::prompt_for_paths() 在 crates/gpui_ohos/src/ohos/platform.rs 到 OpenHarmonyApp::show_file_dialog() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-files/src/lib.rs  [FilesExt trait 扩展方法]
OhosPlatform::prompt_for_paths() 在 crates/gpui_ohos/src/ohos/platform.rs 到 path_from_uri() 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs  [URI → PathBuf，同时固化授权]
```

跨运行时跳转：
```
show_file_dialog() 在 crates/gpui_ohos/depend/openharmony-ability/crates/plugin-files/src/lib.rs 到 [bridge call_async "file-dialog"] 到 FilesPlugin.invokeAsync() 在 crates/gpui_ohos/depend/openharmony-ability/plugins/files/src/main/ets/FilesPlugin.ets  [Rust facade 经 TSFN 调 ArkTS 插件；FileDialogOptions/Response 走 impl_bridge_napi_type 命名类型]
FilesPlugin.invokeAsync() 到 [DocumentViewPicker.select/save] 到 系统文件选择器  [ArkTS 侧；目录模式 selectMode=FOLDER 不做 canIUse 预检直接让系统决定（warp 同款）；保存走 save()]
```

### Extensions 安装模块（列举 → 下载 → 解压 → reload）

UI 在 `crates/extensions_ui`，后端在 `ExtensionStore`（`crates/extension_host/src/extension_host.rs`）。安装动作在主线程置状态（notify），下载/解压全部在后台线程，完成后切回主线程 reload + emit 事件刷新 UI。常规安装不创建进程；dev 扩展编译（git/cargo/rustup）与语言服务器二进制（运行期）才是进程创建点。

```
扩展安装 到 fetch_extensions_from_api() 在 crates/extension_host/src/extension_host.rs  [由扩展面板加载/搜索时调用；GET http_client.build_zed_api_url 拉远端列表（extensions API），解析 GetExtensionsResponse，剔除 SUPPRESSED_EXTENSIONS；本地已装列表来自 extension_index 磁盘扫描]
扩展安装 到 install_extension() 在 crates/extension_host/src/extension_host.rs  [由 UI 安装按钮 on_click（extension_card.rs）或 auto_install_extensions 触发；经 install_or_upgrade_extension 进 install_or_upgrade_extension_at_endpoint]
扩展安装 到 install_or_upgrade_extension_at_endpoint() 在 crates/extension_host/src/extension_host.rs  [安装核心；outstanding_operations 置 Install/Upgrade（UI 据此显示 Installing/Upgrading）→ cx.notify() → cx.spawn 编排 → cx.background_spawn 下载解压]
扩展安装 到 download + unpack 在 crates/extension_host/src/extension_host.rs  [background 任务：http_client.get 下载 tar.gz 到内存 read_to_end → GzipDecoder + tar Archive::unpack 解压到 staging 临时目录 → fs.remove_dir 旧目录 → fs.rename 到 installed_dir；全程后台线程（RealFs/smol 异步 fs）]
扩展安装 到 reload() 在 crates/extension_host/src/extension_host.rs  [下载完成后 this.update 切回主线程调用；重扫 installed_dir 重建 extension_index + WasmHost::load_extension 加载各 wasm 扩展，emit ExtensionsUpdated / ExtensionInstalled]
扩展安装 到 install_dev_extension() 在 crates/extension_host/src/extension_host.rs  [由 "Install Dev Extension" 菜单触发；走 extension_builder 用 git fetch/checkout + rustup/cargo 编译（创建子进程），产物装到 installed_dir]
```

跨文件跳转：
```
extensions_ui 安装按钮 在 crates/extensions_ui/src/components/extension_card.rs 到 ExtensionStore::install_extension() 在 crates/extension_host/src/extension_host.rs  [UI 经 store.update(cx, |store, cx| store.install_extension(...)) 触发]
ExtensionStore::install_or_upgrade_extension_at_endpoint() 在 crates/extension_host/src/extension_host.rs 到 ReqwestClient::send() 在 crates/reqwest_client/src/reqwest_client.rs  [HTTP 下载；handle.spawn(async { request.send().await }) 委托给 tokio runtime，GPUI 后台线程只 await 结果（不占线程）]
ExtensionStore::reload() 在 crates/extension_host/src/extension_host.rs 到 WasmHost::load_extension() 在 crates/extension_host/src/wasm_host.rs  [加载安装的 wasm 扩展（见运行模块）]
```

跨运行时跳转：
```
install_or_upgrade_extension_at_endpoint() 到 [cx.background_spawn] 到 download/unpack 任务  [下载解压在 GPUI 后台执行器（Linux=Worker 线程池 / OHOS=每任务 std::thread::spawn 线程），主线程只 notify 状态]
download 完成 到 [this.update + emit ExtensionInstalled] 到 UI cx.observe/subscribe_in 在 crates/extensions_ui/src/extensions_ui.rs  [后台任务切回主线程 reload + emit，UI 收到事件刷新列表]
ReqwestClient::send() 到 [handle.spawn] 到 tokio runtime（gpui_tokio 2 worker）  [网络 IO 在 tokio reactor 非阻塞执行，见调度器/定时器模块的线程全景]
```

### Extensions 运行模块（WASM 组件 / WIT 双向调用）

扩展本体是 **WASM 组件**（component model），由 wasmtime 在 HiCodeer 主进程内运行，不经独立 extension host 进程。跨运行时边界是 WIT ABI + 消息通道。扩展声明的语言服务器/调试适配器/context server 由 HiCodeer 创建独立二进制进程。

```
扩展运行 到 WasmHost::new() 在 crates/extension_host/src/wasm_host.rs  [由 ExtensionStore 初始化时创建；建 wasmtime Engine（wasm_component_model + async_support + epoch_interruption + 增量编译缓存）+ MainThreadCall 主线程消息通道，读 ExtensionSettings.granted_capabilities]
扩展运行 到 load_extension() 在 crates/extension_host/src/wasm_host.rs  [由 reload() 对每个已装扩展调用；Component::from_binary 编译（background）→ build_wasi_ctx 建 WASI 沙箱 → Store<WasmState> → Extension::instantiate_async 实例化 → call_init_extension 初始化 → 建 ExtensionCall 消息循环（gpui_tokio::Tokio::spawn 到 tokio runtime，因 wasmtime_wasi 内部用 tokio）]
扩展运行 到 WasmExtension::call() 在 crates/extension_host/src/wasm_host.rs  [所有对扩展导出函数的调用入口（language_server_command / run_slash_command / terminal_provider 等）；发 ExtensionCall 到消息循环，oneshot 收结果；循环在 tokio 线程池执行 wasm，epoch 每 100ms 让出防死循环]
扩展运行 到 WasmState::on_main_thread() 在 crates/extension_host/src/wasm_host.rs  [扩展需访问 window/workspace 等主线程状态时调用；经 MainThreadCall 通道切回主线程执行后返回]
扩展运行 到 process::Host::run_command() 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [扩展在 wasm 内请求执行命令；先 CapabilityGranter::grant_exec 校验 manifest capabilities + 用户 granted_extension_capabilities，通过后 util::command 创建子进程执行；默认 granted_capabilities 为空即全部拒绝]
扩展运行 到 ExtensionLspAdapter::get_language_server_command() 在 crates/language_extension/src/extension_lsp_adapter.rs  [扩展声明语言服务器时注册的 LSP 适配器；调扩展 language_server_command（跨 wasm）拿命令+参数，path_from_extension 映射到扩展目录内二进制，win 路径反斜杠还原]
扩展运行 到 LanguageServer 进程启动 在 crates/lsp/src/lsp.rs  [语言服务器作为独立二进制子进程启动（util::command::new_command(&binary.path).spawn()）；扩展通路的真实进程创建点]
```

跨文件跳转：
```
ExtensionStore::reload() 在 crates/extension_host/src/extension_host.rs 到 WasmHost::load_extension() 在 crates/extension_host/src/wasm_host.rs  [已装扩展 → wasm 组件加载]
WasmExtension::call() 在 crates/extension_host/src/wasm_host.rs 到 wit::Extension（WIT 生成） 在 crates/extension_host/src/wasm_host/wit/  [导出函数调用跨 WIT 层]
ExtensionLspAdapter::get_language_server_command() 在 crates/language_extension/src/extension_lsp_adapter.rs 到 WasmExtension::language_server_command() 在 crates/extension_host/src/wasm_host.rs  [LSP 命令经 wasm 扩展解析]
```

跨运行时跳转：
```
HiCodeer 到 WasmExtension::call() 到 [ExtensionCall channel] 到 wasm 导出函数（Extension 组件）  [HiCodeer → 扩展：init/language_server_command/run_slash_command 等导出函数在 tokio 线程池的 Store<WasmState> 上执行]
WasmState 到 [MainThreadCall channel] 到 主线程（window/workspace 委托）  [扩展 → HiCodeer 主线程：需主线程状态时切回执行]
WasmState process::Host::run_command() 到 [CapabilityGranter + util::command] 到 子进程  [扩展请求执行命令 → manifest + 用户设置双重授权后 HiCodeer 创建子进程]
ExtensionLspAdapter 到 [path_from_extension] 到 扩展目录内二进制 → lsp.rs spawn 子进程  [语言服务器二进制从扩展包内解析并以独立进程启动]
```

### Extensions 跨平台运行分析（Linux 下安装/运行可行性）

**结论：能，且无需任何修改。** 扩展系统在代码层面完全平台无关：`extension_host` / `language_extension` / `theme_extension` 三个 crate **零 `cfg(target_env = "ohos")`**；`main.rs` 的 `extension::init` 与 `extension_host::init` 无条件调用（无 cfg 包裹）；`paths::extensions_dir()` 无 ohos 分支（Linux 下即 `data_dir()/extensions`）。wasm 扩展是 **wasm32-wasip2 组件**，编译产物是平台无关的纯 wasm，wasmtime 在 x86_64/aarch64 Linux 原生运行。这本质就是 Zed 桌面版的标准架构，OHOS 移植靠不改扩展核心保持一致——唯一平台差异（子进程执行）收敛在 `util::command` 这一个抽象点。

```
扩展跨平台 到 util::command::new_command() 在 crates/util/src/command.rs  [唯一平台抽象点；cfg(target_env = "ohos") 分支先试本地 HNP 快照、未命中的投给 daemon（见命令后端模块），其余平台走 smol::process::Command 本机执行；process::Host::run_command 与 LSP 二进制 spawn 全走此函数]
```

#### 通信接口清单（wasm ↔ HiCodeer，WIT 双向）

```
扩展通信 到 ExtensionCall 消息循环 → wasm 导出函数 在 crates/extension_host/src/wasm_host.rs  [HiCodeer → 扩展；call_init_extension / call_language_server_command / call_run_slash_command / call_context_server_command / call_get_dap_binary / call_labels_for_completions 等，WIT 层在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs（latest，带版本化兼容）]
扩展通信 到 process::Host::run_command() 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [扩展 → HiCodeer 请求执行命令；CapabilityGranter 双重校验（manifest allow_exec + 用户 granted_extension_capabilities），默认 granted_capabilities 为空即全部拒绝]
扩展通信 到 ExtensionImports::download_file() 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [扩展请求下载；grant_download_file 校验 URL → writeable_path_from_extension 路径逃逸检查（symlink / .. 拒绝）→ 下载到扩展 work_dir]
扩展通信 到 ExtensionImports::npm_install_package() 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [扩展请求 npm 安装；grant_npm_install_package 校验包名]
扩展通信 到 ExtensionImports::get_settings() 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [扩展读设置；经 WasmState::on_main_thread 切回主线程，按 language / lsp / context_servers 分类返回 JSON]
扩展通信 到 HostProject / HostWorktree / HostKeyValueStore 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [Rust 委托对象（Arc<dyn WorktreeDelegate> 等）经 ResourceTable 压入 Store，wasm 侧持 Resource 句柄调用]
扩展通信 到 MainThreadCall channel 在 crates/extension_host/src/wasm_host.rs  [扩展 → HiCodeer 主线程；mpsc unbounded 通道，主线程异步执行后 oneshot 返回]
扩展通信 到 dap::Host::resolve_tcp_template / make_file_executable / set_language_server_installation_status 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs  [其余 import 接口：DAP TCP 解析、设可执行位、语言服务器状态上报]
```

跨文件跳转：
```
extension::init() 在 crates/zed/src/main.rs 到 ExtensionStore::new() 在 crates/extension_host/src/extension_host.rs  [无条件调用，无 cfg 限制；Linux 桌面分支同样启用扩展系统]
process::Host::run_command() 在 crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs 到 util::command::new_command() 在 crates/util/src/command.rs  [跨平台抽象点；OHOS 经 cmd-agent 远程执行（见 Git 操作模块跨运行时跳转）]
```

### 应用层功能模块

```
路径 到 temp_dir() 在 crates/paths/src/paths.rs  [ohos 分支返回 data_dir()/cache（沙箱缓存目录映射）]
音频 到 init() 在 crates/audio/src/audio.rs  [由 main() 的 audio::init 调用；ohos 下走 audio_pipeline_ohos 降级实现]
音频 到 resolve_device() / open_input_stream() / open_test_output() 在 crates/audio/src/audio_pipeline_ohos.rs  [OHOS 降级：返回 Err("not available on OHOS yet")]
崩溃 到 init() 在 crates/crashes/src/crashes.rs  [ohos 分支走 mod ohos 降级：init 返回 Arc<Client>，panic_hook 直接 abort]
终端 到 write_to_primary() 在 crates/terminal/src/terminal.rs  [Linux/freebsd 分支排除 ohos（无主剪贴板）]
```

跨文件跳转：
```
main() 在 crates/zed/src/main.rs 到 audio::init() 在 crates/audio/src/audio.rs  [由 app.run 回调的 audio::init(cx) 触发]
main() 在 crates/zed/src/main.rs 到 reliability::init() 在 crates/zed/src/reliability.rs  [崩溃上报初始化，ohos 降级]
```

### 设置界面模块（Settings UI，OHOS Tab 方案）

OHOS 上设置界面以 **tab** 形式加入主窗口（单 XComponent surface 无法开第二个独立窗口；桌面端仍是独立 SettingsWindow 窗口）。所有 OHOS 分支用 `cfg(target_env = "ohos")` 包裹，桌面端逻辑零改动。入口全部在 `crates/settings_ui/src/settings_ui.rs`。

```
设置 到 open_settings_editor_with() 在 crates/settings_ui/src/settings_ui.rs  [由 open_settings_editor()/open_settings_editor_to_page()/open_settings_editor_at_target() 调用；核心入口，接收 workspace_handle + 回调]
设置 到 open_settings_editor() 在 crates/settings_ui/src/settings_ui.rs  [由 OpenSettings action 触发（settings_ui::init 的 cx.on_action + 每 workspace register_action）；菜单 Settings / 快捷键打开]
设置 到 open_settings_editor_in_tab() 在 crates/settings_ui/src/settings_ui.rs  [由 open_settings_editor_with 的 OHOS 分支调用（cfg ohos）；已有设置 tab → workspace.activate_item 聚焦，否则新建 SettingsWindow + workspace.add_item_to_active_pane 加入主窗口 tab 栏]
设置 到 SettingsWindow::new() 在 crates/settings_ui/src/settings_ui.rs  [由 open_settings_editor_in_tab 新建时调用；桌面端同步观察 workspaces + fetch_files/build_ui，OHOS 走 initialize_as_tab]
设置 到 initialize_as_tab() 在 crates/settings_ui/src/settings_ui.rs  [由 SettingsWindow::new 的 OHOS 分支调用（cfg ohos，ohos_settings_tab_impls mod）；cx.defer_in 延迟 observe_workspace_projects + fetch_files + build_ui，避开 workspace mid-update 的 double lease panic]
设置 到 open_current_settings_file() 在 crates/settings_ui/src/settings_ui.rs  [由 "Edit in settings.json" 按钮 on_click 触发（UnimplementedSettingField renderer，OpenCurrentFile action）；User 分支桌面开独立窗口后 remove_window、OHOS 延迟到 workspace lease 释放后处理]
设置 到 open_settings_file() 在 crates/settings_ui/src/settings_ui.rs  [由 open_current_settings_file 的 OHOS defer 回调调用（cfg ohos）；with_local_or_wsl_workspace 打开 settings.json 为 tab + 遍历 pane 关闭设置 tab]
```

跨文件跳转：
```
open_settings_editor_with() 在 crates/settings_ui/src/settings_ui.rs 到 open_settings_editor_in_tab() 在 crates/settings_ui/src/settings_ui.rs  [OHOS 分支；在 App 级 cx.defer 回调内调用（非 defer_in），避免回调内再 update 主窗口导致嵌套窗口 lease]
open_settings_editor_in_tab() 在 crates/settings_ui/src/settings_ui.rs 到 workspace.activate_item() / workspace.add_item_to_active_pane() 在 crates/workspace/src/workspace.rs  [SettingsWindow 作为 Item 激活或加入主窗口 tab 栏]
open_settings_file() 在 crates/settings_ui/src/settings_ui.rs 到 workspace.with_local_or_wsl_workspace() 在 crates/workspace/src/workspace.rs  [打开用户 settings.json 为 tab]
open_settings_file() 在 crates/settings_ui/src/settings_ui.rs 到 pane.close_item_by_id() 在 crates/workspace/src/pane.rs  [SaveIntent::Skip 关闭设置 tab；返回的 Task 必须 .detach()，直接丢弃会取消（Task drop 语义）]
```

### LSP 启动模块（语言服务器 binary 判定与启动）

binary 判定链：`start_language_server` → `get_language_server_binary`（settings 覆盖 / adapter 查找 / 下载）→ `get_language_server_command`（本机 PATH → 进程缓存 → 下载目录+下载任务双候选）→ 竞速合并。判定顺序核心在 `crates/language/src/language.rs:807` 的 `DynLspInstaller` 默认实现。

```
LSP 到 get_or_insert_language_server() 在 crates/project/src/lsp_store.rs  [由缓冲区语言变化/语言服务器按需注册触发；同 LanguageServerSeed 已运行则复用，否则 start_language_server]
LSP 到 start_language_server() 在 crates/project/src/lsp_store.rs  [由 get_or_insert_language_server 触发；对启用的 language adapter 各启动一个语言服务器，binary 未定前状态为 Starting]
LSP 到 get_language_server_binary() 在 crates/project/src/lsp_store.rs  [由 start_language_server 调用；第一层判定：settings 显式 binary.path 有则直接用，否则走 adapter 判定]
LSP 到 CachedLspAdapter::get_language_server_command() 在 crates/language/src/language.rs  [由 get_language_server_binary 调用；持进程内 cached_binary 锁后转发 adapter 实现]
LSP 到 LspAdapter::get_language_server_command() 默认实现 在 crates/language/src/language.rs  [判定顺序核心；返回 (existing_binary, maybe_download_binary) 双候选]
LSP 到 LanguageServer::new() 在 crates/lsp/src/lsp.rs  [由 start_language_server 的 pending_server 任务在拿到 binary 后调用；创建语言服务器进程 + 建 LSP 消息通道]
```

binary 判定顺序（`get_language_server_command` 默认实现 + `lsp_store.rs` 合并逻辑）：
```
1. settings binary.path 显式配置 → 直接用（最高优先级，绕过一切）
2. allow_path_lookup（默认 true，settings ignore_system_version 可关）→ check_if_user_installed 查本机 → 命中直接返回、不下载
3. 进程内 cached_binary（本次会话下载过且 pre_release 匹配）→ 直接复用
4. allow_binary_download=false → Err，不下载
5. 否则返回 (Zed 下载目录已有 binary, 下载任务) 双候选
6. 合并：本机已有 + 下载任务并行竞速，SERVER_DOWNLOAD_TIMEOUT 内下载完成用新版，超时/失败回退已有
```

跨文件跳转：
```
get_language_server_binary() 在 crates/project/src/lsp_store.rs 到 CachedLspAdapter::get_language_server_command() 在 crates/language/src/language.rs  [settings 未显式配 binary 时走 adapter 判定]
get_language_server_command() 默认实现 在 crates/language/src/language.rs 到 check_if_user_installed() 在 crates/languages/src/rust.rs / go.rs / python.rs  [allow_path_lookup 时查本机 PATH/rustup；rust.rs 找到后 try_exec --help 验证二进制可运行]
get_language_server_command() 默认实现 在 crates/language/src/language.rs 到 cached_server_binary() 在 crates/languages/src/rust.rs / go.rs  [查 Zed 下载目录已存在的 binary]
get_language_server_command() 默认实现 在 crates/language/src/language.rs 到 try_fetch_server_binary() 在 crates/language/src/language.rs  [allow_binary_download 时创建下载任务]
try_fetch_server_binary() 在 crates/language/src/language.rs 到 fetch_latest_server_version() / fetch_server_binary() 在 crates/languages/src/rust.rs / go.rs  [查 GitHub 最新版本并下载]
start_language_server() 在 crates/project/src/lsp_store.rs 到 LanguageServer::new() 在 crates/lsp/src/lsp.rs  [binary 确定后启动语言服务器进程]
```

跨运行时跳转：
```
check_if_user_installed() 到 [delegate.which] 到 which() 在 crates/project/src/lsp_store.rs  [OHOS 分支（cfg target_env=ohos，14786/14787）把 which 交给 util::command 在设备上执行并短暂重试——executor 是异步注册的，早于注册的 spawn 会被误判成"二进制不存在"；非 ohos 分支（14825）用 which crate 本机查 PATH]
which miss 到 [子进程 exit != 0] 到 回落"未安装"  [设备侧没有 dnf/自动安装这条路：which 未命中即视为缺失，交由 LSP adapter 走正常的下载分支]
```

### Git 操作模块（git 子进程执行路径与线程归属）

git 操作 100% 走 git CLI 子进程（`crates/git` 无 git2/libgit2 依赖）；OHOS 上子进程经 `util::command::Command` 分流——命中 `/data/app/bin` 快照的本地 HNP 工具（本项目含 git/ssh/curl）在 app 沙箱内本地 fork，未命中的（chmod、LSP、node 等）才投给 daemon（hicodeerd，独立 uid，看不到调用方沙箱）。线程归属分两派：**读/普通写操作在后台（BackgroundExecutor）；commit / reset / checkout_files / push / pull / fetch 与 clone 对话框在前台（UI 主线程）**。前台操作在 OHOS 上因 `Command::spawn()` 的同步阻塞握手而真实卡 UI（最长 20s，SPAWN_REPLY_TIMEOUT）；git 命中本地 HNP 走本地 fork，不受此限，只有走 daemon 的前台命令才会卡。

后台执行（`self.executor.spawn` BackgroundExecutor / `cx.background_spawn`，OHOS 每任务 `std::thread::spawn` 线程）：
```
Git 到 status() 在 crates/git/src/repository.rs  [由 git_store 状态刷新触发；executor.spawn 后台，git status 子进程]
Git 到 diff_tree() / diff() / diff_stat() 在 crates/git/src/repository.rs  [由 diff 计算触发；executor.spawn 后台]
Git 到 blame() / blame_at_revision() 在 crates/git/src/repository.rs  [由编辑器 git blame 触发；executor.spawn 后台]
Git 到 stage_paths() / unstage_paths() 在 crates/git/src/repository.rs  [由 git_panel stage/unstage 触发；executor.spawn 后台]
Git 到 stash_paths() / stash_pop() / stash_apply() / stash_drop() 在 crates/git/src/repository.rs  [由 git_panel stash 操作触发；executor.spawn 后台]
Git 到 create_branch() / delete_branch() / checkout_branch_in_worktree() 在 crates/git/src/repository.rs  [由 git_panel 分支操作触发；executor.spawn 后台]
Git 到 restore_checkpoint() / diff_checkpoints() 在 crates/git/src/repository.rs  [由 checkpoint 恢复/对比触发；executor.spawn 后台]
Git 到 open_repo() 在 crates/fs/src/fs.rs  [由 git_store::LocalRepositoryState::new 的 background_spawn 调用；git2 Repository::open 后台执行]
Git 到 git_init() / git_clone() / git_config() 在 crates/fs/src/fs.rs  [由 git_store 的 background_executor().spawn 包裹调用]
```

前台执行（UI 主线程；OHOS 上同步阻塞卡 UI）：
```
Git 到 commit() 在 crates/git/src/repository.rs  [由 git_panel commit 触发；inline BoxFuture 不经 executor.spawn，job 在前台 worker 执行；commit 注释明确"不能放后台线程，要阻塞等待 credential helper 弹窗"；OHOS Command::spawn 同步握手卡 UI]
Git 到 reset() 在 crates/git/src/repository.rs  [由 git_panel reset 触发；同上前台+OHOS阻塞]
Git 到 checkout_files() 在 crates/git/src/repository.rs  [由 git_panel revert 触发；同上]
Git 到 push() / pull() / fetch() 在 crates/git/src/repository.rs  [由 git_panel push/pull/fetch 触发；同上]
Git 到 clone_and_open() 在 crates/git_ui/src/clone.rs  [由 clone 对话框触发；cx.spawn 前台直接 await fs.git_clone，绕过 git_store 后台包装]
```

前台 worker 机制（承载上述写操作）：
```
Git 到 spawn_local_git_worker() 在 crates/project/src/git_store.rs  [由 GitStore 初始化时调用；cx.spawn 在前台启动，循环消费 job_sender 里的 job]
Git 到 send_job() / send_keyed_job() 在 crates/project/src/git_store.rs  [由 git_panel/editor 各 git 操作触发；job 闭包套 cx.spawn 在前台 await backend 方法]
```

跨文件跳转：
```
git_ui commit 在 crates/git_ui/src/git_panel.rs 到 repo.commit() 在 crates/project/src/git_store.rs  [返回 oneshot Receiver；job 在前台 worker 执行]
git_store::commit() 在 crates/project/src/git_store.rs 到 backend.commit() 在 crates/git/src/repository.rs  [send_job 包装；backend.commit 是 inline BoxFuture，前台 poll 时跑 git 子进程]
git_store::checkout_files() 在 crates/project/src/git_store.rs 到 backend.checkout_files() 在 crates/git/src/repository.rs  [send_job 包装；前台执行]
git_ui clone 在 crates/git_ui/src/clone.rs 到 fs.git_clone() 在 crates/fs/src/fs.rs  [cx.spawn 前台直接 await；不走 git_store 后台包装]
git_store::spawn_local_git_worker() 在 crates/project/src/git_store.rs 到 send_keyed_job() 在 crates/project/src/git_store.rs  [job 经 job_sender 投递到前台 worker 执行]
```

跨运行时跳转：
```
Git 到 util::command::Command::spawn() 在 crates/util/src/command/ohos.rs 到 [命中本地 HNP 快照 → smol::process 本地 fork；未命中 → ExecSpec + SpawnReply 投 daemon] 到 CmdClientExecutor::spawn() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs  [daemon 路是同步阻塞握手（超时上限 20s）；git/ssh/curl 命中本地快照，走本地 fork 不经 daemon]
```

### Git Panel 运行路径（提交 / 凭据 / 子进程 / 刷新）

面板装配：`git_ui::init()`（`crates/git_ui/src/git_ui.rs`）在 zed 启动时调用（`crates/zed/src/main.rs`、`crates/zed/src/zed.rs`），面板本身按 workspace 的序列化 panel 注册。提交入口是 `git::Commit` action（默认 `ctrl-enter` / `cmd-enter`，见 `assets/keymaps/default-*.json`），面板上的按钮派发同一个 action。

模块入口：
```
Git Panel 到 on_commit() 在 crates/git_ui/src/git_panel.rs  [由 git::Commit action 触发；先取提交信息，信息为空则聚焦输入框后返回]
Git Panel 到 commit() 在 crates/git_ui/src/git_panel.rs  [on_commit 调用；有 staged 变更直接 commit，否则在 cx.spawn 里先 stage_entries 再 commit]
Git Panel 到 askpass_delegate() 在 crates/git_ui/src/git_panel.rs  [commit / push / pull / fetch 各自前置调用；内部 new 出 AskPassModal 把凭据框挂到 UI]
Git Panel 到 schedule_update() 在 crates/git_ui/src/git_panel.rs  [收到 GitStore 事件后重算面板内容；render() 是绘制入口]
```

跨文件跳转：
```
Git Panel 到 GitStore::commit() 在 crates/project/src/git_store.rs  [作者参数传 None——提交身份交给 git 自己按 user.name/user.email 解析，移植侧不注入]
GitStore::commit() 在 crates/project/src/git_store.rs 到 RealGitRepository::commit() 在 crates/git/src/repository.rs  [send_job 投给前台 worker]
RealGitRepository::commit() 在 crates/git/src/repository.rs 到 run_git_command() 在 crates/git/src/repository.rs  [commit 与 push/pull/fetch 四个前台写操作的唯一收口；build_command 带 --quiet -m --cleanup=strip]
run_git_command() 在 crates/git/src/repository.rs 到 AskPassSession::new() 在 crates/askpass/src/askpass.rs  [env 里没有 GIT_ASKPASS 时无条件建会话：tempdir + 写 askpass.sh + chmod +x + 绑 Unix socket；本地提交根本用不到凭据，但这条照样会走]
AskPassSession::new() 在 crates/askpass/src/askpass.rs 到 make_file_executable() 在 crates/util/src/fs.rs  [给生成的脚本加执行位；OHOS 上必须是进程内本地 chmod]
run_git_command() 在 crates/git/src/repository.rs 到 run_askpass_command() 在 crates/git/src/repository.rs  [select_biased：git 子进程输出 与 askpass 任务 竞速，谁先完成谁定结果]
GitStore 事件到 subscribe_in 回调 在 crates/git_ui/src/git_panel.rs  [订阅 GitStoreEvent：StatusesChanged / HeadChanged / RepositoryAdded / RepositoryRemoved / ActiveRepositoryChanged 触发 schedule_update；IndexWriteError 弹 workspace 错误]
```

跨运行时跳转（凭据回环）：
```
Git 到 run_git_command() 在 crates/git/src/repository.rs 到 [GIT_ASKPASS / SSH_ASKPASS = askpass.sh 路径，SSH_ASKPASS_REQUIRE=force] 到 git 子进程
askpass.sh 到 [Unix socket 上的提示串] 到 AskPassSession 的 socket 任务 在 crates/askpass/src/askpass.rs  [脚本内容是 printf '%s\0' "$@" | <app 二进制> --askpass=<socket>；ASKPASS_PROGRAM 取 current_exe，即回 exec app 自身]
AskPassSession socket 任务 在 crates/askpass/src/askpass.rs 到 [get_password 回调] 到 askpass_delegate() 闭包 在 crates/git_ui/src/git_panel.rs 到 AskPassModal 在 crates/git_ui_core/src/askpass_modal.rs  [跨到 UI 线程弹凭据框；用户提交后密码原路经 socket 回传 → 脚本 stdout → git]
```

> **提交身份**：`GitPanel::commit()` 的作者参数是 `None`，身份完全由 git 按 `$HOME/.gitconfig` 解析。app 的 `$HOME` 是用户在启动页挑的目录下的 `HiCodeer` 子目录（`hap/entry/src/main/ets/entryability/Setup.ets` + `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`），**不是**系统终端的 `~`，所以系统终端里配好的 git 身份不会被 app 内的 git 继承。报 `unable to auto-detect name` 说明 email 已生效、只缺 `user.name`。

### Terminal 运行路径（面板 → 视图 → 元素 → pty → shell 后端）

终端横跨三个 crate：`crates/terminal/`（pty + VT 解析 + 输入写入，无 UI）、`crates/terminal_view/`（面板 / 视图 / 网格元素，UI 层）、`crates/project/`（终端实例的创建与登记）。OHOS 上的关键分叉是 **pty 建在哪一侧**：探到命令后端（见下一节）时 pty 开在 daemon 侧、host 只做中继；探不到则回退沙箱内的本地 `/bin/sh`。

模块入口：
```
终端视图 到 init() 在 crates/terminal_view/src/terminal_view.rs  [由 crates/zed/src/zed.rs:6163 的启动流程调用；内部先调 terminal_panel::init()]
终端面板 到 init() 在 crates/terminal_view/src/terminal_panel.rs  [由 terminal_view::init() 调用；注册 Toggle / ToggleFocus / new_terminal / open_terminal 四个 action]
终端面板 到 load() 在 crates/terminal_view/src/terminal_panel.rs  [由 workspace 加载 dock 面板时调用；先试反序列化，失败才 TerminalPanel::new()]
终端面板 到 new_terminal() 在 crates/terminal_view/src/terminal_panel.rs  [用户操作：新建终端标签页]
终端面板 到 open_terminal() 在 crates/terminal_view/src/terminal_panel.rs  [用户操作：打开终端面板]
终端 到 create_terminal_task() 在 crates/project/src/terminals.rs  [由终端面板 / agent / 调试器调用；解析 cwd / env / shell 后建 TerminalBuilder]
终端 到 TerminalBuilder::new() 在 crates/terminal/src/terminal.rs  [由 create_terminal_task() 调用；异步构造，OHOS 上在此探后端]
终端 到 TerminalBuilder::subscribe() 在 crates/terminal/src/terminal.rs  [由 create_terminal_task() 在 cx.new() 内调用；起 pty 事件循环，返回 Terminal 实体]
终端视图 到 TerminalView::new() 在 crates/terminal_view/src/terminal_view.rs  [由终端面板 / agent 面板 / 调试器调用；订阅 Terminal 事件并建 IME 状态]
终端视图 到 TerminalElement::request_layout() 在 crates/terminal_view/src/terminal_element.rs  [GPUI 每帧布局时调用；终端网格的唯一渲染入口]
```

跨文件跳转：
```
create_terminal_task() 在 crates/project/src/terminals.rs 到 TerminalBuilder::new() 在 crates/terminal/src/terminal.rs
TerminalBuilder::new() 在 crates/terminal/src/terminal.rs 到 probe() 在 crates/terminal/src/ohos_shell.rs  [仅 target_env=ohos；探命令后端能否供 pty，结果决定走 guest 还是本地]
probe() 在 crates/terminal/src/ohos_shell.rs 到 open_remote_shell() 在 crates/util/src/command/ohos.rs  [探到后端时；交互 shell 的 pty 开在 daemon 侧]
open_shell_pty() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs 到 shell_command() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs  [拼出 daemon 侧要跑的 exec payload]
TerminalBuilder::new() 在 crates/terminal/src/terminal.rs 到 open_pty() 在 crates/terminal/src/ohos_shell.rs  [guest 分支：建本地 pty 对 + 中继桥]
open_pty() 在 crates/terminal/src/ohos_shell.rs 到 start_bridge() 在 crates/terminal/src/ohos_shell.rs  [起 INPUT_THREAD / OUTPUT_THREAD 两条中继线程]
TerminalBuilder::new() 在 crates/terminal/src/terminal.rs 到 open_pty() 在 crates/terminal/src/alacritty.rs  [无后端时的本地回退；pty_options() 组装 shell 与 env]
TerminalView 到 process_keystroke() 在 crates/terminal_view/src/terminal_view.rs 到 try_keystroke() 在 crates/terminal/src/terminal.rs  [keymap 命中的按键走这条]
TerminalView 到 commit_text() 在 crates/terminal_view/src/terminal_view.rs 到 input() 在 crates/terminal/src/terminal.rs  [IME 上屏文本走这条]
```

跨运行时跳转：
```
pty IO 线程 到 [PtyEvent 经 futures mpsc] 到 TerminalBuilder::subscribe() 的事件循环 在 crates/terminal/src/terminal.rs  [后台线程 → GPUI 前台；4ms 合批，Wakeup 单独处理]
Terminal 到 [Event::Wakeup / Event::Bell / Event::BlinkChanged] 到 subscribe_for_terminal_events() 在 crates/terminal_view/src/terminal_view.rs  [实体事件；TerminalView 据此 cx.notify() 重绘]
TerminalView 到 [pty_tx.notify(bytes)] 到 spawn_event_loop() 起的 alacritty EventLoop 线程 在 crates/terminal/src/alacritty.rs  [GPUI 前台 → PTY 写线程]
TerminalElement 到 [TerminalInputHandler 注册进 Window::handle_input()] 到 InputHandler 实现 在 crates/terminal_view/src/terminal_element.rs  [平台层把键盘与 IME 文本交给元素；:1630 构造、:1662 注册、:1799 实现]
OHOS 键盘与 IME 到 [platform input event / IME 上屏] 到 dispatch_input() 在 crates/gpui_ohos/src/ohos/window.rs  [OHOS 平台入口；键盘走 GPUI 事件分发，IME 文本由 ArkTS 插件回调]
OHOS 中继 到 [本地 pty slave ⇄ socketpair] 到 start_bridge() 的两条中继线程 在 crates/terminal/src/ohos_shell.rs  [读写各一条线程；终端界面看到的字节就是经它转发的远端数据]
OHOS 中继 到 [cmd-client socketpair ⇄ SSH channel data] 到 open_shell_pty() 的中继任务 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs  [经 loopback SSH 打到 daemon 侧 pty]
```

补充要点（实现决策，非追踪细节）：
- **pty 归谁由 probe 决定**：探到后端 → pty 开在 daemon 侧，host 侧那个本地 pty 的 child 只是**驻留进程**（`HOLD_SHELL` + FIFO，`crates/terminal/src/ohos_shell.rs:37` / `:62` / `:200`），从不读 slave；探不到 → 沙箱内起本地 `/bin/sh` 子进程。
- **本地回退受沙箱限制**：`crates/terminal/src/ohos_shell.rs:3` 注明沙箱只允许 exec `/bin/sh`；`crates/terminal/src/terminal.rs:1134` 也把 `Shell::System` 固定成 `/bin/sh`，并绕开被 `load_login_shell_environment` 覆写成 `/bin/bash` 的 `SHELL` 变量（`crates/terminal/src/terminal.rs:1083`）。
- **交互 shell 名不在 terminal 决定**：guest 分支下 `alacritty_shell` 被 `guest.local_shell_argv()` 覆盖（`crates/terminal/src/terminal.rs:1239`），用户设置里的 shell 会被丢弃；真正决定远端 shell 的是 cmd-agent 拼的 exec payload（`crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs` 的 `shell_command()`，现为 `exec /usr/bin/zsh`）。属已知分层缺陷，见 `移植记录/bugfix/2026-09-19-ohos-terminal-shell-hardcoded-sh.md`。
- **两条输入路径别混**：keymap 命中的按键走 `TerminalView::process_keystroke` → `Terminal::try_keystroke`；IME 上屏文本走 `TerminalInputHandler` → `TerminalView::commit_text`。两者最终都汇到 `Terminal::input`（`crates/terminal/src/terminal.rs:2112`）。
- **两套事件别混**：`PtyEvent`（`crates/terminal/src/terminal.rs:764`，pty 线程 → GPUI）与 `Terminal::Event`（`crates/terminal/src/terminal.rs:673`，Terminal → TerminalView）是不同层的两个枚举。
- **`TerminalView` 也服务非终端场景**：agent 面板、调试器、REPL 用 `new_display_only()` 的 display-only 终端（无 pty，`crates/terminal/src/terminal.rs:941`），渲染路径相同但不走 `probe`。

### 命令后端模块（cmd-agent：cmd-client + hicodeerd）

设备沙箱禁 exec 外部程序，非本地 HNP 的命令统一经 `util::command` 投给设备上的守护进程 **hicodeerd**。它由系统以独立 uid 拉起、不随 HAP 覆盖安装重启（`install-local.sh` 结尾亦如此提示），app 内没有它的启动代码。两个 crate 同在 `crates/gpui_ohos/depend/cmd-agent/`：`cmd-client`（HiCodeer 进程内 host 侧，实现 `RemoteCommandExecutor`）与 `hicodeerd`（daemon 侧二进制）。传输是 **loopback SSH**（russh），不是裸 TCP；端口定义在 `cmd-client/src/protocol.rs`——`COMMAND_PORT=4022`（命令）、`MANAGEMENT_PORT=4023`（管理），命令口每次运行换动态密钥、管理口固定密钥。旧的 QEMU/OpenEuler 后端及其 9p 挂载已删除：`workspace::mount_opened_dirs()`（`crates/workspace/src/workspace.rs:10471`）现在是空实现，QEMU 代码只剩 `depend/qemu-mngt`，仅在 `qemu-agent` feature 下参与。

模块入口：
```
命令后端 到 main() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [daemon 进程入口；由系统以独立 uid 拉起（public HNP），不由 app 启动]
命令后端 到 run() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [main 调用；在 bind_host（默认 127.0.0.1）上建 4022/4023 两个 listener]
命令后端 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs  [SSH 会话的 exec 请求入口；解析 sid 前缀后转 spawn_command]
命令后端 到 pty_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs  [SSH 会话的 shell 请求入口；PTY 由 daemon 侧开]
launch-zed 到 launch_app() 在 crates/gpui_ohos/depend/launch-zed/src/launch_app.rs  [NAPI 启动入口；先 init_local_tools() 快照本地 HNP 工具集]
launch-zed 到 start_command_backend() 在 crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs  [由 launch_app 调用；文件名含 qemu，实为后端选择的公共入口]
launch-zed 到 register_executor() 在 crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs  [链路就绪后注册：cmd_client::init_executor() + util::command::init()]
命令 到 Command::spawn() 在 crates/util/src/command/ohos.rs  [业务代码（git/LSP/终端）的唯一入口；先试本地 HNP 快照，未命中才转 executor.spawn()]
```

跨文件跳转：
```
register_ohos_backend() 在 crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs 到 SshCommandExecutor::new() 在 crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs  [建 host 侧 SSH 执行器]
SshCommandExecutor::new() 到 CommandEndpoint::ohos_default() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/endpoint.rs  [取 loopback 端点参数]
bootstrap 到 start() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/bootstrap.rs 到 fetch_ssh_info() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/bootstrap.rs  [轮询管理口 4023 取 SshInfo，兼作心跳与就绪判据]
连接池 到 Pool::new() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pool.rs 到 connect() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pool.rs  [按 host key 建立/复用 SSH 连接]
命令构造 到 build_command() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/command.rs 到 sid_payload() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/protocol.rs  [ExecSpec → 单条 POSIX sh 串 + session id 前缀]
终端 到 open_remote_shell() 在 crates/util/src/command/ohos.rs 到 open_shell_pty() 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs  [交互式 shell；PTY 建在 daemon 侧，host 只做中继]
daemon 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs 到 spawn_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs  [exec 请求 → 起子进程并桥接 stdio]
daemon 到 pty_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs 到 run_pty_shell() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/pty.rs  [shell 请求 → daemon 侧 openpty 并跑 shell]
```

跨运行时跳转（loopback SSH 上的 exec / signal / bootstrap）：
```
host spawn 到 [exec 首行的 __hicodeerd_sid__ 前缀] 经命令口 4022 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs  [发送端 executor.spawn() → pump → channel.exec()；接收端 split_sid_payload() 解析]
daemon 子进程 stdio 到 [SSH channel data / extended_data] 到 host pump 在 crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs  [daemon 侧 bridge_until_exit() 转发 stdout/stderr 与退出码；host 回灌 socketpair]
host signal 到 [signal_command()] 经命令口到 signal_session() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs  [由 executor.signal() 触发；daemon 侧 kill 整个进程组]
daemon 就绪 到 [BOOTSTRAP_COMMAND / SshInfo] 经管理口 4023 到 management.rs 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs  [daemon 发布 SshInfo，host 的 bootstrap 轮询消费]
```

补充要点（实现决策，非追踪细节）：
- **管理口只剩 bootstrap**：现在只服务取 SshInfo；没有 Mount/Unmount，Signal 走命令口。
- **辅助模块**：`pool.rs` host 侧连接池；`command.rs` ExecSpec → sh 串；`pty.rs` host 中继 / daemon 侧 openpty；`session_tmp.rs` daemon 采纳 data root 并导出 TMPDIR；`shim.rs` 给 Node 注入 NODE_OPTIONS 预载；`sign_elf.rs` npm 安装后补 `.codesign` 段；`keygen.rs` 每次运行生成命令口动态密钥。
- **两个 feature**：`launch-zed/Cargo.toml` 的 `daemon-agent` 与 `qemu-agent`，`default` 同时含两者（`Cargo.toml:17`），`script/bundle-ohos` 不传 `--features` 故两者都编；运行期由设置决定（默认关 QEMU）→ 实际走 daemon。注意 `script/bundle-ohos:368-370` 的注释称「qemu-agent 已淘汰」，与 Cargo.toml 矛盾，**以 Cargo.toml 为准**。
- **daemon 从哪来**：`script/bundle-ohos:438` 用 `cargo build --release --manifest-path $DAEMON_MANIFEST` 编出 hicodeerd，再经 hnpcli 打成 **public** 的 `hicodeerd.hnp`（声明见 `hap/entry/src/main/module.json5` 的 hnpPackages）。public 意味着它不在 app 的 `/data/app/bin` 本地工具集里，因此 `chmod` 之类未打包的命令只能投给它、且它看不到 app 沙箱路径。

### hicodeerd 守护进程运行路径（daemon 侧：启动装配 → 双 listener → SSH 会话 → 子进程）

上一节讲的是 host 侧 cmd-client；这一节讲 **daemon 自身的代码运行路径**。hicodeerd 是设备上独立运行的守护进程（public HNP `hicodeerd.hnp` 的 `bin/hicodeerd`），由系统以独立 uid 拉起，**app 内没有它的启动代码**，它也不随 HAP 覆盖安装重启。源码全在 `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/`，12 个模块：`main`（装配）、`logger`、`shim`、`keygen`、`protocol`、`sshd`（命令口）、`management`（管理口）、`exec`（普通命令）、`pty`（交互式 shell）、`peers`（进程组登记与回收）、`session_tmp`（TMPDIR）、`sign_elf`（补签名）。

模块入口：
```
hicodeerd 到 main() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [进程入口；由系统以独立 uid 拉起，app 不参与；只认 --log / --help（--help 在装 logger 之前就返回）]
hicodeerd 到 logger::init() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/logger.rs  [main 调用，参数即是否带 --log；不带则不装 logger，所有 log::xxx! 被静默丢弃；带则 OHOS 经 OH_LOG_Print 进 hilog]
hicodeerd 到 run() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [main 调用；启动装配顺序：shim::install → conf_dir/read_mgmt_keys → keygen::generate → 建 tokio multi-thread runtime → 绑两个 listener]
hicodeerd 到 shim::install() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/shim.rs  [run 的第一件事；由 package_root() 解析 <pkg>/shim/，给本进程导出 NODE_OPTIONS=--require shim.js 与 LD_PRELOAD=musllib-shim.so（另设 HICODEERD_OSUSER），此后所有子进程继承；载荷缺失只 warn 不致命]
hicodeerd 到 keygen::generate() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/keygen.rs  [run 调用；为本次运行现生成命令口动态密钥对（host key + client private key），只经管理口的 SshInfo 交给客户端，不落盘]
hicodeerd 到 accept_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [run 里 tokio::try_join! 的一个分支；4022 上循环 accept，每连接新建 ConnectionHandler 后 tokio::spawn 给 russh::run_stream]
hicodeerd 到 accept_management() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [run 里 try_join! 的另一分支；4023 上循环 accept，每连接经 ManagementServer::new_connection 新建 ManagementHandler]
hicodeerd 到 peers::spawn_sweeper() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/peers.rs  [run 内调用；5s 一次的常驻任务，30s 没心跳的客户端连同它启动的全部进程组一起被回收]
hicodeerd 到 maybe_spawn_drop_caches() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs  [run 内调用；仅当 HICODEERD_DROP_CACHES=1（guest 模式）才起 15s 周期任务写 /proc/sys/vm/drop_caches，OHOS 上直接 return]
```

跨文件跳转：
```
hicodeerd 到 run() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs 到 SshServer::new() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs  [命令口 handler 工厂，注入本次运行的动态 client key]
hicodeerd 到 run() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs 到 ManagementServer::new() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs  [管理口 handler 工厂，注入固定 client key 与已序列化的 SshInfo]
hicodeerd 到 accept_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs 到 ConnectionHandler::new() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs  [每连接一份私有 children/ptys map，因为 ChannelId 跨连接会从同一低值重来]
hicodeerd 到 accept_management() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs 到 ManagementServer::new_connection() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs
hicodeerd 到 auth_publickey() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs 到 peers::touch() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/peers.rs  [SSH 用户名即客户端身份，鉴权成功即心跳]
hicodeerd 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs 到 spawn_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs  [无 pty 的普通 exec 走这条]
hicodeerd 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs 到 run_pty_shell() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/pty.rs  [该 channel 先来过 pty_request 时走这条；relay 必须另起 task，否则后续 data/window_change 无处投递]
hicodeerd 到 data()/window_change_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs 到 forward_input()/resize() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/pty.rs  [键盘输入与窗口尺寸中转]
hicodeerd 到 spawn_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs 到 split_sid_payload()/parse_signal_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/protocol.rs  [解析会话 id 前缀与预留信号命令]
hicodeerd 到 spawn_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs 到 sign_tree() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sign_elf.rs  [先经同文件 rewrite_npm_para 识别 npm install 并记下安装根；子进程退出码为 0 后扫描该树补签名]
hicodeerd 到 register_session() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs 到 peers::add_group() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/peers.rs  [把新进程组登记到该客户端身份名下]
hicodeerd 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs 到 session_tmp::adopt() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/session_tmp.rs  [采纳客户端报来的 data root：建 <root>/tmp 并导出 TMPDIR，只生效一次]
hicodeerd 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs 到 logger::attach_file() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/logger.rs  [同一个 root：日志镜像到 <root>/logs/hicodeerd.log，打开时回放此前的 backlog]
```

跨运行时跳转：
```
host cmd-client 到 [管理口 exec: "hicodeerd-bootstrap <data_root>"] 经 4023 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs  [接收端 parse_bootstrap_command；回包是本次运行的 SshInfo JSON，该轮询同时充当客户端心跳]
host cmd-client 到 [命令口 exec: 首行 "__hicodeerd_sid__ <sid>"] 经 4022 到 exec_request() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/sshd.rs  [接收端 split_sid_payload]
host cmd-client 到 [命令口 exec: "__hicodeerd_signal__ <sid> <sig>"] 经 4022 到 spawn_command() 在 crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs  [不 spawn 子进程，直接 kill 整个进程组]
daemon 子进程 到 [SSH channel Data / ExtendedData + exit status] 到 host 侧 executor 的 pump  [daemon 侧 bridge_until_exit 转发 stdout/stderr 与退出码，host 侧回灌 socketpair]
daemon 到 [libc::kill(-pgid, SIGTERM → 1s → SIGKILL)] 到 该客户端启动的整棵进程树  [peers::retire；跨进程信号而非消息，负 pid 打整个进程组]
daemon 到 [/proc/self/exe] 到 HNP 版本目录 <pkg>.org/<pkg>_<version>/  [package_root() 定位 <pkg>/conf 与 <pkg>/shim；read_link 返回的是真实路径，不是 /data/service/hnp/bin 那层软链]
daemon 到 [进程环境 NODE_OPTIONS / LD_PRELOAD / TMPDIR] 到 每个被 spawn 的 sh -c 与 PTY shell  [继承而非消息：preload 由 shim::install 一次性导出、TMPDIR 由 session_tmp::adopt 一次性导出，Node/npm、语言服务器、构建工具都靠它]
```

补充要点（实现决策，非追踪细节）：
- **两个口两套密钥**：4022 用每次启动现生成的动态密钥对；4023 用包内 `conf/` 的固定密钥（`mgmt_host_key` + `authorized_keys`，目录可用 `HICODEERD_CONF_DIR` 覆盖）。客户端先打 4023 取 SshInfo，再拿动态密钥连 4022；两个 listener 的 `inactivity_timeout` 都是 `None`（host 侧保持长连接池，不能让空闲连接被服务端回收）。
- **身份即 SSH 用户名**：客户端身份是 `hk-<random>` 用户名（`protocol::CLIENT_ID_PREFIX`）。daemon 以它为单位登记该实例启动的所有进程组；两条回收路径——30s 无心跳，或一个新身份出现（旧实例的一切连同子进程一起下掉）。不带前缀的裸账号名不被跟踪，行为与加此机制前一致。
- **会话与信号是预留命令字，不是新协议**：`__hicodeerd_sid__` 前缀把 exec 与会话关联，`__hicodeerd_signal__` 在命令口内发信号；管理口只服务 bootstrap，没有挂载类命令。
- **PTY 建在 daemon 侧**：host 只做中继。pty master 注册表按 `(conn_id, channel)` 双键索引——`ChannelId` 每个连接从同一个低值重来，单键会让另一个连接的 exec 子进程被抢走 stdin。
- **`which` 未命中要显式补一行 stdout**：`which` 找不到时本来什么都不打印，daemon 代它输出 `which: not found: <prog>`，供命令面板探测 PATH。
- **npm install 补签名**：npm 解包出的原生二进制没有 OHOS 签名，daemon 在该安装命令退出码为 0 后扫描安装树给缺 `.codesign` 段的 ELF 补签；安装过程中脚本自己跑的东西已随跑随签，故不做二次回放。补签失败不影响它所跟随的命令。
- **日志**：只有 `--log` 才装 logger。hilog 之外还镜像到 `<data_root>/logs/hicodeerd.log`；data root 要等客户端 bootstrap 才报来，所以启动头几行先缓存成 backlog（上限 256 条），文件打开时回放，使文件从"客户端已连接"那句开始就完整。
- **guest/QEMU 模式复用同一份代码**（`script/bundle-ohos:585` 的 GUEST_TARGET 分支），靠环境变量分流：`HICODEERD_BIND_ADDR` 改绑定地址（guest 必须 0.0.0.0 才能被 hostfwd 打到，OHOS 保持 127.0.0.1），`HICODEERD_DROP_CACHES=1` 周期回收 guest dcache 以释放 virtiofsd 持有的 O_PATH fd。guest 侧 `<pkg>/shim/` 为空目录，两个 preload 都不装，daemon 照常服务。
- **shim 载荷随包安装**：`<pkg>/shim/` 的 `shim.js`（NODE_OPTIONS）与 `musllib-shim.so`（LD_PRELOAD）由 HNP 一并安装、随重装整包替换，运行时不写、不检查、不自愈；缺失只 warn。编译与打包见 `script/bundle-ohos`。

### AI Agent 模块（agent_ui / agent / language_models：进程内 agent 引擎）

Agent Panel 是 Zed 自家实现的**进程内 agent**：UI（crates/agent_ui）→ 引擎（crates/agent 的 Thread tool-call 循环）→ 工具集（crates/agent/src/tools，进程内操作 fs/Buffer）→ LLM（crates/language_models 各 provider）。整个 agent 层**零 `cfg(target_env = "ohos")`**；除 LLM 推理（出网）与第三方/远程 agent（ACP 子进程）外全在 HiCodeer 主进程内，无独立 agent 进程。OHOS 差异全部在下层（进程执行 util::command → 本地 HNP 快照 / daemon，见命令后端模块）。会话存本地 SQLite（thread_store + crates/db）。

模块入口：
```
Agent 到 agent_ui::init() 在 crates/agent_ui/src/agent_ui.rs  [由 zed 启动初始化序列调用（crates/zed/src/main.rs on_finish_launching，agent_ui 模块 init）；注册 AgentPanel / AgentRegistry / inline assistant]
Agent 到 AgentPanel 在 crates/agent_ui/src/agent_panel.rs  [由 agent_ui::init 注册为 workspace 面板，crates/zed/src/zed.rs:520/861/902 register_action(toggle/focus)；用户点侧栏 agent 图标或快捷键打开]
Agent 到 Agent::server() 在 crates/agent_ui/src/agent_ui.rs  [AgentPanel 连接时调用；enum Agent 默认 #[default] NativeAgent（:425/426），server()（:483）NativeAgent 分支返回 NativeAgentServer::new(fs, thread_store)；仅用户显式选 custom/第三方 agent 才走 ACP]
Agent 到 NativeAgentServer::connect() 在 crates/agent/src/native_agent_server.rs  [server() 的 connect 触发（:32）；cx.new(NativeAgent::new(thread_store, templates, fs, cx))（:46）进程内创建，不 spawn 任何子进程]
Agent 到 NativeAgent 的 Thread::new() 在 crates/agent/src/agent.rs  [NativeAgent 实体 agent.rs:404；新会话/子 agent 时 Thread::new（agent.rs:741）+ AcpThread::new（:774）包成 ACP 适配实体；会话/草稿经 ThreadStore 存本地 SQLite]
Agent 到 Thread::run_turn() / run_turn_internal() 在 crates/agent/src/thread.rs  [用户发消息/回复继续/spawn_agent_tool 触发；Thread 是 gpui Entity（thread.rs:1229），run_turn（:2655）→ run_turn_internal（:2719）是 tool-call 循环：model.stream_completion → 解析 tool call → run_tool → 回填结果后下一轮]
Agent 到 run_tool() 在 crates/agent/src/thread.rs  [循环内执行工具；按 NAME 调 tool.run()，文件编辑等先经 tool_permissions::authorize_file_edit 授权（tools/tool_permissions.rs）]
```

工具集（crates/agent/src/tools/，全进程内真实实现）：
```
Agent 到 read_file/list_directory/grep/find_path/create_directory/delete_path/move_path 在 crates/agent/src/tools/*.rs  [run_tool 按 NAME 分发；运行时 project.read_with(|p,_| p.fs().clone()) 拿 Arc<dyn Fs> → crates/fs RealFs（std::fs::read_to_string/write/read_dir，fs.rs:911/1000）；grep 用 project.search 进程内搜，不起外部进程；list 用 worktree snapshot]
Agent 到 write_file/edit_file 在 crates/agent/src/tools/write_file_tool.rs / edit_file_tool.rs  [委托 EditSession（tools/edit_session.rs）；持 buffer: Entity<Buffer> + diff: Entity<Diff>（:354/:356）]
Agent 到 EditSessionContext 在 crates/agent/src/tools/edit_session.rs  [工具运行时构造（:136）；持 project/thread/action_log/language_registry；编辑经 buffer.start_transaction→edit→end_transaction_with_source(BufferEditSource::Agent)（:1008-1026）进程内改真实 Buffer（editor 订阅该 buffer，用户实时见 diff）；落盘 ensure_buffer_saved 调 project.format + project.save_buffer（:186-215）经 RealFs 写]
Agent 到 go_to_definition/find_references/apply_code_action/rename_symbol/diagnostics 在 crates/agent/src/tools/*.rs  [调 project.definitions/references/apply_code_action/perform_rename + lsp_store 拉诊断（LspToolFeatureFlag）；进程内对 LSP server]
Agent 到 TerminalTool 在 crates/agent/src/tools/terminal_tool.rs  [run_terminal_tool 经 environment.create_terminal（:929）执行；授权 + sandbox 判定在工具内完成]
Agent 到 fetch_tool / web_search_tool 在 crates/agent/src/tools/fetch_tool.rs / web_search_tool.rs  [fetch 走进程内 http_client 直拉 URL；web_search 走 WebSearchRegistry 外部搜索 provider]
```

跨文件跳转：
```
Agent::server() 在 crates/agent_ui/src/agent_ui.rs 到 NativeAgentServer::new() 在 crates/agent/src/native_agent_server.rs
NativeAgentServer::connect() 在 crates/agent/src/native_agent_server.rs 到 NativeAgent::new() 在 crates/agent/src/agent.rs
NativeAgent（agent.rs:404）到 Thread::new() 在 crates/agent/src/thread.rs  [会话实体创建；agent.rs:741]
Thread::run_turn_internal() 在 crates/agent/src/thread.rs 到 model.stream_completion() 在 crates/language_model 的 LanguageModel 实现  [请求模型流式推理]
ThreadEnvironment::create_terminal()（trait thread.rs:756）在 crates/agent/src/agent.rs（NativeThreadEnvironment:3138）到 project.create_terminal_task() 在 crates/project/src/terminals.rs:64  [→ TerminalBuilder → crates/terminal open_pty（alacritty_terminal::tty::new）真 fork/exec PTY shell；OHOS 重活经 util::command → 本地 HNP 快照 / daemon]
EditSession buffer 编辑 在 crates/agent/src/tools/edit_session.rs 到 project.save_buffer()/format() 在 crates/project/src/project.rs  [落盘到 RealFs]
```

LLM provider（crates/language_models）：
```
Agent 到 language_models::init() 在 crates/zed/src/main.rs  [初始化序列（main.rs:744）；注册全部内置 provider 到全局 LanguageModelRegistry]
Agent 到 register_language_model_providers() 在 crates/language_models/src/language_models.rs  [逐个 registry.register_provider(Arc::new(...))：Cloud（zed.dev 云，注册表第一个，需 Zed 账号）/ Anthropic / OpenAI / Ollama / LM Studio / LlamaCpp / DeepSeek / Google / Bedrock / OpenRouter / CopilotChat 等（:216-344，各实现在 crates/language_models/src/provider/*.rs）]
Agent 到 Thread::ensure_model() / model() 在 crates/agent/src/thread.rs  [run_turn 前从 LanguageModelRegistry 取模型（:1971/:1963）；按 thread 存的语言模型设置选 provider/model，可本地 localhost（Ollama/LM Studio）或直连云 API]
Agent 到 DeepSeekLanguageModelProvider 在 crates/language_models/src/provider/deepseek.rs  [stream_completion → crates/deepseek（DEEPSEEK_API_URL=https://api.deepseek.com/v1）→ http_client::HttpClient 进程内发 HTTP]
```

跨运行时跳转：
```
Thread run_turn_internal 到 [stream_completion HTTP 流] 到 云 LLM API / 本地模型  [唯一默认出网环节：zed.dev 云或 Anthropic/OpenAI/DeepSeek 等厂商 API 或 Ollama localhost；取决于所选 provider]
AgentPanel 选 custom/第三方 agent 在 crates/agent_ui/src/agent_ui.rs 到 AcpConnection::stdio() 在 crates/agent_servers/acp.rs  [spawn 独立子进程走 ACP stdin/stdout；远程项目场景 agent 命令在远端 zed host 执行]
Agent 到 AgentRegistryStore 在 crates/project/src/agent_server_store.rs 到 https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json  [列出可用第三方 agent（agent_registry_store.rs）]
TerminalTool 到 [create_terminal → PTY] 到 本地 shell 子进程  [普通平台本地 fork/exec；OHOS 沙箱禁 exec，重活经 util::command → 本地 HNP 快照 / daemon（见命令后端模块）；PTY 本体 /bin/sh 本地 exec + rustix-openpty 补丁]
Agent 工具编辑 到 [Buffer transaction + save_buffer] 到 打开文件的编辑器视图  [同进程 buffer 共享，改 buffer 即实时可见 diff，用户可撤销]
```

补充要点：
- **进程内 vs 远程**：Agent Panel 默认 native agent 全进程内；ACP/agent_servers 只在 custom agent（spawn 外部 ACP 进程）与远程项目场景出现。会话持久化本地 SQLite（thread_store.rs ThreadsDatabase::connect），无云端存储。
- **编辑落点是 Buffer 而非 Editor**：工具结构体无 Entity\<Editor\>/Workspace，编辑目标是 project 打开的真实 Buffer（被 editor 共享），因此 agent 编辑天然有实时 diff 与 undo。

#### 对话输入与 Add Context 上下文注入（MessageEditor / MentionSet）

输入框是 `MessageEditor`（`crates/agent_ui/src/message_editor.rs`），内部包一个 gpui `Editor`（`self.editor`）与一个 `MentionSet`（`crates/agent_ui/src/mention_set.rs`，负责 `@` 提及的 crease 与内容注入）。Add Context 菜单挂在输入框左侧 "+" 按钮上。

模块入口：
```
Agent 到 ThreadView::build_add_context_menu() 在 crates/agent_ui/src/conversation_view/thread_view.rs  [用户点输入框 "+"（render_add_context_button :5478 / OpenAddContextMenu :12023）展开 PopoverMenu 时构建菜单项]
Agent 到 MessageEditor::insert_context_type() 在 crates/agent_ui/src/message_editor.rs  [菜单 "Files & Directories"（非 OHOS）/"Symbols"/"Threads" 项点击；插 "@file"/"@symbol"/"@thread" 前缀并弹补全菜单（:957）]
Agent 到 MessageEditor::add_images_from_picker() 在 crates/agent_ui/src/message_editor.rs  [菜单 "Image" 项点击；打开系统文件选择器，选中图整张注入（:1618）]
Agent 到 MessageEditor::add_file_paths_from_picker() 在 crates/agent_ui/src/message_editor.rs  [菜单 "Files & Directories"（OHOS，#[cfg(target_env = "ohos")]）项点击；打开系统文件选择器，选中路径以 ", " 连接的纯文本插入光标处，不注入文件内容（:1671）]
Agent 到 MessageEditor::insert_skill_crease() 在 crates/agent_ui/src/message_editor.rs  [菜单 "Skills" 子项点击（:1525）]
Agent 到 MessageEditor::insert_branch_diff_crease() 在 crates/agent_ui/src/message_editor.rs  [菜单 "Branch Diff" 项点击（:1439）]
Agent 到 MessageEditor::send() 在 crates/agent_ui/src/message_editor.rs  [用户回车/Chat；emit MessageEditorEvent::Send（:944/:950）]
```

跨文件跳转：
```
Add Context 到 "Selection" handler 在 crates/agent_ui/src/conversation_view/thread_view.rs 到 ConversationView::insert_selection() 在 crates/agent_ui/src/conversation_view.rs  [dispatch AddSelectionToThread → agent_panel.rs:643 handler → :713 active_conversation_view()]
Add Context 到 ConversationView::insert_selection() 在 crates/agent_ui/src/conversation_view.rs 到 MessageEditor::insert_selections() 在 crates/agent_ui/src/message_editor.rs  [（:1580）]
Add Context 到 ConversationView::insert_dragged_files() 在 crates/agent_ui/src/conversation_view.rs 到 MessageEditor::insert_dragged_files() 在 crates/agent_ui/src/message_editor.rs  [拖拽文件/目录进输入框（:3169 → :1405）]
Agent 到 MessageEditor::add_images_from_picker() 在 crates/agent_ui/src/message_editor.rs 到 insert_images_as_context() 在 crates/agent_ui/src/mention_set.rs  [load_external_image_from_path 读图（:1017）→ 建图片 crease（:882）]
Agent 补全 到 PromptCompletionProvider 确认 在 crates/agent_ui/src/completion_provider.rs 到 MentionSet::confirm_mention_completion() 在 crates/agent_ui/src/mention_set.rs  [选中补全项（:1965）；内部 confirm_mention_for_file（:384）读取文件内容建 mention]
Agent 到 MessageEditor::insert_skill_crease() 在 crates/agent_ui/src/message_editor.rs 到 MentionSet::confirm_mention_completion() 在 crates/agent_ui/src/mention_set.rs
```

跨运行时跳转：
```
Agent 到 MessageEditor::send() 在 crates/agent_ui/src/message_editor.rs 到 [MessageEditorEvent::Send] 到 ThreadView 订阅 在 crates/agent_ui/src/conversation_view/thread_view.rs  [gpui 实体事件（:1132 MessageEditorEvent::Send => self.send）]
Agent 到 ThreadView::send_content() 在 crates/agent_ui/src/conversation_view/thread_view.rs 到 AcpThread::send() 在 crates/acp_thread/src/acp_thread.rs  [ContentBlock 列表发给会话（:3630）；native 走进程内 Thread::run_turn，custom/远程走 ACP 子进程]
Agent 到 MessageEditor::add_images_from_picker() / add_file_paths_from_picker() 在 crates/agent_ui/src/message_editor.rs 到 [cx.prompt_for_paths] 到 OhosPlatform::prompt_for_paths() 在 crates/gpui_ohos/src/ohos/platform.rs  [OHOS 平台层开系统文件选择器，oneshot 回传 PathBuf（见文件选择器模块）]
```

补充要点：
- **两种注入语义**：`insert_context_type`（非 OHOS 的 Files & Directories / Symbols / Threads）只插 `@keyword` + 弹补全菜单，用户选中后由 mention_set 读取内容建 crease（会把文件内容注入 prompt）；OHOS 的 "Files & Directories" 改走 `add_file_paths_from_picker`，仅插入纯路径文本、不注入内容（省 token，模型按需自读文件）。
- **Image 不分平台**：Image 无论平台都 `insert_images_as_context` 注入整张图；只有 Files & Directories 在 OHOS 走纯路径。
- **发送只是 emit**：`MessageEditor::send` 仅发事件，真正入队、起 turn、发给 agent 在 ThreadView（send → send_impl → send_content）。

### Collab Panel 与 Edit Prediction 模块

两模块都属 Zed 协作/AI 功能的 UI 与运行时，路径均不含 `ohos`，属通用上游代码；OHOS 上无裁剪（编译与运行与其它 OS 一致）。

#### Collab Panel（协作面板，左侧/右侧 Dock）

编译/初始化路径：
- `collab_ui` 是 workspace 成员（`Cargo.toml:39`），`crates/zed` 依赖它（`crates/zed/Cargo.toml:91`）。
- 启动初始化序列里 `collab_ui` 在 on_finish_launching 闭包中调用 `collab_ui::init`（crates/zed/src/main.rs:869）。

模块入口：
```
Collab Panel 到 collab_ui::init() 在 crates/zed/src/main.rs  [由 on_finish_launching 初始化序列调用（第77步）；模块初始化]
Collab Panel 到 CollabPanel::load() 在 crates/zed/src/zed.rs  [由 workspace 初始化时调用（zed.rs:786）；把面板注册进 workspace 的 dock 面板集合]
Collab Panel 到 icon_tooltip "Collab Panel" 在 crates/collab_ui/src/collab_panel.rs  [Panel 实现返回 tooltip 字符串（collab_panel.rs:3990），dock 图标由 GPUI dock 渲染；图标为 UserGroup]
Collab Panel 到 ToggleFocus action 在 crates/zed/src/zed/app_menus.rs  [菜单项 "Collab Panel"（app_menus.rs:43）由用户点击触发]
Collab Panel 到 toggle_panel_focus::<CollabPanel>() 在 crates/zed/src/zed.rs  [由 ToggleFocus action 触发（zed.rs:1304）；打开/聚焦面板]
```

跨文件跳转：
```
on_finish_launching 到 collab_ui::init() 在 crates/zed/src/main.rs  [第77步模块初始化]
CollabPanel 状态 到 ChannelStore / UserStore / NotificationStore 在 crates/collab_ui/src/collab_panel.rs  [面板状态源；内容分区见 enum Section（collab_panel.rs:297）：ActiveCall / FavoriteChannels / Channels / ChannelInvites / ContactRequests / Contacts / Online / Offline]
CollabPanel 到 client.status() / rpc::proto 在 crates/collab_ui/src/collab_panel.rs  [登录态与协作数据走 Client → RPC（proto）连 Zed 协作服务端；未登录显示登录引导视图]
```

跨运行时跳转：
```
Collab Panel 到 [client RPC（proto）] 到 Zed 协作服务端  [频道/联系人/通话/通知全走 client 出网；OHOS 单机若无协作后端则面板停在登录页]
```

补充要点：
- **受保护代码**：`crates/collab_ui` 与调用点 `crates/zed/src/zed.rs`、`crates/zed/src/main.rs`、`crates/zed/src/zed/app_menus.rs` 路径均不含 `ohos`，属上游受保护文件。要裁剪/不编译（如 OHOS 不显示协作）需逐处授权，不能在协作功能外擅改。
- **停靠位置**：仅 Left/Right（`position_is_valid` 只放行二者，collab_panel.rs:3950）；隐藏状态栏按钮走 `collaboration_panel.button = false`（`hide_button_setting`，collab_panel.rs:4010）。

#### Edit Prediction（AI 代码预测补全，状态栏按钮 + 多 provider）

编译/初始化路径：
- 启动初始化序列三步：`edit_prediction_ui`（main.rs:800）、`edit_prediction_registry`（main.rs:804）、`edit_prediction`（main.rs:879），均在 on_finish_launching 闭包中。
- `crates/edit_prediction/Cargo.toml:6` 协议为 `GPL-3.0-or-later`（强 copyleft）。

模块入口：
```
Edit Prediction 到 edit_prediction_ui::init() 在 crates/zed/src/main.rs  [初始化序列（main.rs:800）；注册 RatePredictions action + OpenEditPredictionContextView 渲染]
Edit Prediction 到 edit_prediction_registry::init() 在 crates/zed/src/main.rs  [初始化序列（main.rs:804）；observe_new 每个 Editor → 按 settings provider 分配 provider；订阅 user_store 与 SettingsStore 变更]
Edit Prediction 到 edit_prediction::init() 在 crates/zed/src/main.rs  [初始化序列（main.rs:879）；核心 store/telemetry 初始化]
Edit Prediction 到 EditPredictionButton::new() 在 crates/zed/src/zed.rs  [UI 装配（zed.rs:588-646）；状态栏右侧 item]
Edit Prediction 到 status_bar.add_right_item(edit_prediction_ui) 在 crates/zed/src/zed.rs  [由 UI 装配调用（zed.rs:646）；状态栏右侧显示 provider 状态按钮 + 下拉菜单]
Edit Prediction 到 assign_edit_prediction_provider() 在 crates/zed/src/zed/edit_prediction_registry.rs  [由 editor 创建 / 用户切 provider / 设置变更 触发；把选中的 delegate 设给 editor]
Edit Prediction 到 edit_prediction_provider_config_for_settings() 在 crates/zed/src/zed/edit_prediction_registry.rs  [由 assign 调用前读取 settings.edit_predictions.provider 决定走哪条 delegate（edit_prediction_registry.rs:114-157）]
```

跨文件跳转：
```
edit_prediction_registry::init() 到 EditPredictionStore::global() 在 crates/edit_prediction/src/edit_prediction.rs  [store 全局单例（edit_prediction.rs:977）]
assign_edit_prediction_provider() 到 ZedEditPredictionDelegate::new() 在 crates/edit_prediction/src/zed_edit_prediction_delegate.rs  [Zed 自营 provider 分支，需 Zed 账号 + 组织开通（edit_prediction_registry.rs:279-313）]
assign_edit_prediction_provider() 到 CopilotEditPredictionDelegate 在 crates/copilot  [Copilot provider 分支，需 GitHub 登录，不要求 Zed 账号]
assign_edit_prediction_provider() 到 CodestralEditPredictionDelegate 在 crates/codestral  [Codestral provider 分支，用 client.http_client() + Mistral API key]
status_bar.add_right_item 到 EditPredictionButton::new() 在 crates/edit_prediction_ui/src/edit_prediction_button.rs  [状态栏按钮实现；点击弹 provider 菜单]
```

跨运行时跳转：
```
Edit Prediction Zed provider 到 [client RPC（proto）/ cloud_llm_client] 到 Zed 云端 LLM  [仅 Zed 自营 provider 走；需 Zed 账号 + 组织 edit_prediction 开启]
Edit Prediction Copilot provider 到 [Copilot LSP] 到 GitHub Copilot 服务  [需 GitHub 登录]
Edit Prediction Ollama / OpenAiCompatibleApi provider 到 [http_client 直连] 到 本地或自建 API  [只需配置 api_url/model，无需任何账号登录]
```

补充要点：
- **不强制 Zed 登录**：Edit Prediction 是**多 provider 架构**，是否要登录 Zed 账号只取决于所选 provider——`Zed` 自营才强制（且组织须开启）；`Copilot` / `Ollama` / `OpenAiCompatibleApi` / `Codestral` 都不要求 Zed 账号（路由逻辑见 edit_prediction_registry.rs:117-157，唯 Zed 分支查 `current_organization_configuration().edit_prediction.is_enabled`）。
- **设置页**：配置入口在设置 UI 的 "Edit Predictions" 分组（crates/settings_ui/src/page_data.rs:10632），含 provider / 数据采集 / 语言级开关。
- **协议**：`edit_prediction` 及同族 crate 为 `GPL-3.0-or-later`，集成分发需遵守 GPL 义务。

### Zed 官方服务依赖运行路径（出网总闸 + 各服务出网点）

本节只记**运行路径与出网点**。各服务的端点全集、默认值、凭据要求、换第三方后的损失等完整分析，
见 `移植记录/design/2026-09-17-zed官方服务依赖分析.md`，不在此重述。

**总闸**：全应用只有一个带基址的 HTTP 客户端，构造点唯一（`crates/client/src/client.rs:586`），
基址取自 `ClientSettings.server_url`（默认 `https://zed.dev`，`assets/settings/default.json:2687`），
环境变量 `ZED_SERVER_URL` 优先覆盖（`crates/client/src/client.rs:63-64`）。
所有官方请求的 URL 都由三个构建器产出（`crates/http_client/src/http_client.rs`），
映射规则：白名单值映射到 `api.zed.dev` / `cloud.zed.dev`，**任何其它值原样透传**。

模块入口：
```
Zed 官方服务 到 Client::production() 在 crates/client/src/client.rs  [on_finish_launching 初始化序列创建 Client（client.rs:584）；此处构造唯一的 HttpClientWithUrl]
Zed 官方服务 到 HttpClientWithUrl::new_url() 在 crates/client/src/client.rs  [client.rs:586，全仓唯一构造点；基址 = ClientSettings.server_url]
Zed 官方服务 到 build_zed_api_url() / build_zed_cloud_url() / build_zed_llm_url() 在 crates/http_client/src/http_client.rs  [三个 URL 构建器（http_client.rs:277/285/317）；非白名单基址原样透传，即自建域名要同时扮演 API+Cloud+LLM 三角色]
Telemetry 到 report_event() 在 crates/client/src/telemetry.rs  [由 telemetry::event! 宏触发（telemetry.rs:566）；settings.telemetry.metrics=false 时直接 return（:573），入队后由 flush_events_inner 出网（:660）]
AutoUpdate 到 AutoUpdater::start_polling() 在 crates/auto_update/src/auto_update.rs  [由 auto_update::init（crates/zed/src/main.rs:682）按 ReleaseChannel::poll_for_updates() 决定是否轮询（auto_update.rs:261/455）；Dev 通道返回 false（crates/release_channel/src/lib.rs:201）]
AutoUpdate 到 check() 在 crates/auto_update/src/auto_update.rs  [用户点菜单 Check for Updates 触发（auto_update.rs:304，注册于 init 内的 register_action）]
崩溃上报 到 upload_previous_minidumps() 在 crates/zed/src/reliability.rs  [启动时补传历史 minidump（reliability.rs:232；上传实现 :273）；仅当 MINIDUMP_ENDPOINT 存在才启用（crates/client/src/telemetry.rs:92-102），OHOS 构建未注入该常量]
Cloud 账号 到 CloudApiClient::get_authenticated_user() 在 crates/cloud_api_client/src/cloud_api_client.rs  [需已登录凭据（cloud_api_client.rs:108）；未登录不触发]
Cloud 账号 到 CloudApiClient::create_llm_token() 在 crates/cloud_api_client/src/cloud_api_client.rs  [铸 LLM token（cloud_api_client.rs:128），所有 Cloud LLM 请求的前置步骤]
Feature flags 到 FeatureFlagStore::update_flags() 在 crates/feature_flags/src/feature_flags.rs  [由云端 websocket 消息驱动（feature_flags.rs:232）；不是独立拉取]
MCP OAuth 到 CIMD_URL 在 crates/context_server/src/oauth.rs  [MCP 服务器走 OAuth 授权时把 https://zed.dev/oauth/client-metadata.json 当 client_id（oauth.rs:37）；可自托管替换]
```

跨文件跳转：
```
各服务 到 HttpClientWithUrl 在 crates/client/src/client.rs 到 build_zed_*_url() 在 crates/http_client/src/http_client.rs  [所有官方请求 URL 的唯一产出路径]
Extension 市场 到 fetch_extensions() 在 crates/extension_host/src/extension_host.rs 到 fetch_extensions_from_api() 在 crates/extension_host/src/extension_host.rs  [列表 :574、单扩展版本 :647；真正出网在 :726（URL 构造 :732）]
Cloud LLM 到 CloudLanguageModelProvider 在 crates/language_models_cloud/src/language_models_cloud.rs 到 refresh_models() 在 crates/language_models_cloud/src/language_models_cloud.rs  [模型列表 :870（GET /models，:889）；推理走 :169 perform_llm_request]
Edit Prediction Zed provider 到 ZedEditPredictionDelegate 在 crates/edit_prediction/src/zed_edit_prediction_delegate.rs 到 zeta 请求 在 crates/edit_prediction/src/zeta.rs  [delegate 结构 :18、实现 :50；accept 上报 :871]
Web Search 到 CloudWebSearchProvider 在 crates/web_search_providers/src/cloud.rs 到 perform_web_search() 在 crates/web_search_providers/src/cloud.rs  [唯一 web search provider，id=zed.dev（cloud.rs:43），注册 crates/web_search_providers/src/web_search_providers.rs:59；出网 :61/:74]
协作 到 Client::rpc_url() 在 crates/client/src/client.rs 到 Client::establish_websocket_connection() 在 crates/client/src/client.rs  [GET /rpc 期望 302（:1283-1322），取 Location 作 websocket URL 再连（:1326）]
```

跨运行时跳转（出网到 zed 官方）：
```
Telemetry 到 report_event() 在 crates/client/src/telemetry.rs 到 [POST /telemetry/events] 到 https://api.zed.dev  [build_zed_api_url（telemetry.rs:652）；无需凭据，默认开（assets/settings/default.json:1561-1569），OHOS 那份未覆盖 → 当前会发]
Extension 市场 到 fetch_extensions() 在 crates/extension_host/src/extension_host.rs 到 [GET /extensions、/extensions/{id}/download] 到 https://api.zed.dev  [用户打开扩展面板触发；无需凭据]
AutoUpdate 到 check() 在 crates/auto_update/src/auto_update.rs 到 [GET /releases/{channel}/{version}/asset] 到 https://cloud.zed.dev  [build_zed_cloud_url_with_query（auto_update.rs:673）；Dev 通道不自动轮询，仅手动触发]
Cloud 账号 到 CloudApiClient 在 crates/cloud_api_client/src/cloud_api_client.rs 到 [GET /client/users/me、POST /client/llm_tokens、POST /client/system_settings] 到 https://cloud.zed.dev  [需登录凭据；含 /internal/users/impersonate（crates/client/src/client.rs:1580）]
账号变更 到 Client 连接循环 在 crates/client/src/client.rs 到 [WebSocket /client/users/connect + MessageToClient::UserUpdated] 到 crates/cloud_api_types/src/websocket_protocol.rs  [CBOR 编码（websocket_protocol.rs:12-27）；连接由用户登录/分享项目/加入频道触发（connect 调用点见 client.rs:705、crates/project/src/project.rs:1695、crates/zed/src/main.rs:1339）]
Cloud LLM 到 CloudLanguageModelProvider 在 crates/language_models_cloud/src/language_models_cloud.rs 到 [GET /models、POST 推理] 到 https://cloud.zed.dev（build_zed_llm_url）  [需 Zed 账号 + 组织；provider id 字面为 zed.dev（crates/language_model_core/src/provider.rs:19），注册见 crates/language_models/src/language_models.rs:223]
Edit Prediction Zed provider 到 zeta 在 crates/edit_prediction/src/zeta.rs 到 [POST /predict_edits/accept|reject|settled|raw] 到 https://cloud.zed.dev（build_zed_llm_url）  [需 Zed 账号 + 组织开通 edit_prediction；默认 provider 就是 "zed"（assets/settings/default.json:1771）]
Web Search 到 CloudWebSearchProvider 在 crates/web_search_providers/src/cloud.rs 到 [POST /web_search] 到 https://cloud.zed.dev（build_zed_llm_url）  [需 Zed 账号 + 组织（缺组织直接报 "No organization selected."）；内置唯一 provider，无替代]
反馈提交 到 CloudApiClient::submit_agent_feedback() 在 crates/cloud_api_client/src/cloud_api_client.rs 到 [POST /client/feedback/agent_thread（及 _comments、edit_prediction）] 到 https://cloud.zed.dev  [用户在 Agent 面板主动提交（cloud_api_client.rs:272/284/299）]
MCP OAuth 到 CIMD_URL 在 crates/context_server/src/oauth.rs 到 [GET https://zed.dev/oauth/client-metadata.json] 到 zed.dev  [可自托管替换]
```

补充要点：
- **未登录账号时只有两条会真的出网**：遥测（`api.zed.dev`，默认开）与扩展市场（`api.zed.dev`，需用户打开扩展面板）。
  其余（AI、账号、协作、反馈）都要求凭据；`Client::connect` 的调用点只在用户主动登录/分享/加入频道路径上
  （`crates/client/src/client.rs:705`、`crates/project/src/project.rs:1695`、`crates/collab_ui/src/collab_panel.rs:2751`、`crates/zed/src/main.rs:1339`）。
- **三个编译期常量 OHOS 构建均未注入**（`script/bundle-ohos` 与仓库内其它构建入口皆零命中）：
  `ZED_MINIDUMP_ENDPOINT`（→ 崩溃不上传）、`ZED_CLIENT_CHECKSUM_SEED`（→ 遥测 checksum 头为空）、
  `ZED_RELEASE_CHANNEL`（→ 见下条 dev 通道）。复核手法：`grep -n` 需先做阳性对照，因为 `script/bundle-ohos` 无扩展名，`psrch.py` 按后缀过滤覆盖不到它。
- **当前 `RELEASE_CHANNEL` 为 `dev`**（`crates/zed/RELEASE_CHANNEL` 内容）→ `poll_for_updates()` 返回 false → 不自动轮询更新；一旦改通道，「自动更新」那条会立刻变成活跃依赖。
- **三处默认值指向 zed，且都能在 OHOS 叠加层覆盖**（不需要改代码，因为是「改值」不是「删共享键」）：
  `telemetry.diagnostics/metrics`（`assets/settings/default.json:1561-1569`）、
  `agent.default_model.provider`（`:1091-1097`，默认 `zed.dev`）、
  `edit_predictions.provider`（`:1771`，默认 `zed`）。
- **与相邻两节的关系**：上文「Collab Panel」里的 `client RPC（proto）→ Zed 协作服务端`
  与「Edit Prediction」里的 `cloud_llm_client → Zed 云端 LLM`，其出网实现就落在本节：
  协作走 `Client::rpc_url` → websocket；Cloud LLM 走 `CloudLanguageModelProvider` → `build_zed_llm_url`。
- **`server_url` 是单点开关**：改它一处，扩展市场 / 协作 / 遥测 / 自动更新 / 账号 / AI 全部一起改向。
  但自建服务器必须实现 `/rpc`、`/extensions/*`、`/client/*`、`/models`、`/predict_edits/*`、`/web_search`、
  `/telemetry/events`、`/releases/*` 全套，否则各功能各自静默失效（都不阻塞启动）。
- **若决定「OHOS 默认不依赖官方服务」，只动 OHOS 那两份文件**：
  `assets/settings/default-ohos.json`（覆盖上面三处默认值）与 `assets/keymaps/default-ohos.json`（键位）。
  协作要移除则必须改代码 cfg（涉及路径不含 `ohos` 的受保护文件，见上文 Collab Panel 节的说明）。

### 关键配置与产物

- `hap/entry/src/main/ets/entryability/EntryAbility.ets`：`moduleName = "hicodeer"`。
- `script/bundle-ohos`：`cargo build --lib -p launch-zed`（`CRATE="launch-zed"`）→ 产物直接是 `libhicodeer.so`（launch-zed 的 `[lib] name = "hicodeer"`），复制到 HAP `entry/libs/arm64-v8a/`（`OHOS_LIB_NAME` 可覆盖）。
- `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/`：NAPI 入口 crate（cdylib，libhicodeer.so）。`src/launch_app.rs` 的 `#[ability] launch_app` + `src/lib.rs` 的 `pthread_mutex_*` 补丁符号 + `build.rs` 的 `napi_build_ohos::setup()`。
- `crates/zed/src/lib.rs`：仅 `#[cfg(target_env = "ohos")] include!("main.rs")`。
- `crates/zed/src/main.rs`：`#[cfg(target_env = "ohos")] pub fn start_zed_main(base_path: Option<String>)`（设 data_dir → main）。
- `crates/zed/Cargo.toml`：`[lib] crate-type = ["rlib"]`；ohos 分支无 `openharmony-ability`/`napi` 依赖（依赖反转：launch-zed 依赖 zed）。
- `.cargo/config.toml`：ohos 目标 rustflags `--cfg gles` + `target-feature=+fp16`。

## 常见坑

- **on_finish_launching 只在 SurfaceCreate 触发**：窗口创建代码必须放在 app.run 的回调里，且依赖 SurfaceCreate 事件已到达（native_window 可用）。若在事件到达前建窗口会拿不到 renderer。
- **moduleName 与库名强绑定**：`NAPI_BUILD_TARGET_NAME`（=hicodeer）必须与 so 文件名 `libhicodeer.so` 一致。launch-zed 是叶子 crate（无消费者），可用 `[lib] name = "hicodeer"` 直接产出 `libhicodeer.so`；**不能改 zed 的 `[lib] name`**（会把 Rust crate 名改掉，`use zed::` 全断）。
- **include!("main.rs") 只在 ohos 启用**：桌面端 crates/zed 仍是二进制 crate（`[[bin]] name = "zed"`）。
- **GPU 初始化在平台层、surface 在窗口层**：`WgpuContext::new()`（Instance/Adapter/Device）在 `OhosPlatform::new()` 时创建，可在 app 设置前完成；`WgpuRenderer`（surface）在 `OhosWindow::initialize_renderer()` 时才建，必须等 `native_window` 可用（SurfaceCreate 后）。两个阶段分离，排查黑屏先确认哪一步失败。
- **后台任务的线程归属与 NAPI 限制**：`OhosDispatcher::dispatch` 现在把后台任务交给驻留 worker pool（线程名 `gpui-ohos-bg-N`），不再是每任务 `std::thread::spawn`（2026-09-20 修正，代码注释自述见 `crates/gpui_ohos/src/ohos/dispatcher.rs:43-44`）；但结论不变——worker 仍是非主线程，任务内若直接触 NAPI 会 SIGABRT（NAPI 只能在创建线程调用）。跨线程的 NAPI 调用必须走 `OpenHarmonyApp::bridge()` 的 TSFN 封装。
- **OHOS 只能有一个窗口**：`OhosPlatform::open_window` 对第二个窗口直接 `bail!("OHOS supports a single window; cannot open a second window")`（`crates/gpui_ohos/src/ohos/platform.rs:602`）。因此任何依赖新开窗口的功能在 OHOS 上都不可用——典型受害者是 `miniprofiler_ui` 的性能分析窗口（profiler 因此需要非窗口出口，见 Profiler 采样链路节）；设置界面也正因此改用 tab 而非独立窗口（见设置界面模块）。
- **profiler 在 OHOS 必须由 dispatcher 自己上报**：GPUI 不会自动覆盖 OHOS——`OhosDispatcher` 的三个执行点（`execute_runnable` / `dispatch` / `dispatch_after`）必须各自成对调用 `crates/gpui/src/profiler.rs` 的 `update_running_task` + `save_task_timing`。漏掉任何一处，那一类任务在 profiler 里就完全不可见（历史上三处全缺，是 CPU 自旋任务定位不到的直接原因）。`save_task_timing` 内部对 `running` 做 `expect`，不成对调用会 panic。
- **初始化卡点定位**：hilog（tag=HiCodeer）里 `[boot] enter init: <模块>` 出现而对应 `exit init` 未出现，即初始化卡在该模块；`enter init` 一个都没出现则卡在更早（看 `hicodeer-boot` tag 的 `start_zed_main` / `building application` / `calling app.run` / `on_finish_launching entered`；launch_app 无入口日志，若连 `start_zed_main` 都没有则卡在 NAPI init 之前）。
- **日志双 tag 体系**：`zlog::init()` 之前（`start_zed_main`）用 `direct_hilog_info` 直连，tag=`hicodeer-boot`；`zlog::init()` 之后所有 `log::xxx!` 走重定向，tag=`HiCodeer`。launch_app（NAPI 入口）不打印日志。抓日志两个 tag 都要过滤。
- **WAKER 时序 bug（黑屏根因）**：`OhosPlatform::set_app` → `create_waker()`（读全局 WAKER）早于 ArkTS `init` → `create_lifecycle_handle()`（写全局 WAKER）。`wake()` 若用 `create_waker` 返回的 None 快照则永远静默失败 → UserEvent 死掉 → `run_foreground_tasks` 不驱动 → 窗口创建任务饿死 → 黑屏。修复：`wake()` 必须每次实时读全局 WAKER（`(*WAKER).read()`），不能存快照。详见 `移植记录/bugfix/2026-08-17-ohos-black-screen-waker.md`。
- **UserEvent 是 foreground executor 的唯一驱动**：GPUI `cx.spawn` 的窗口创建任务（`restore_or_create_workspace`）只在 `handle_ohos_event` 收到 `Event::UserEvent` 时经 `run_foreground_tasks` 执行。SurfaceCreate 只触发 on_finish_launching，不驱动任务队列；若 UserEvent 不来，窗口永远不创建（黑屏症状）。排查这类问题先看 UserEvent 是否到达。
- **XComponent 必须 focusable + default_focus 才能收按键**：`XComponent::new()` 后必须 `set_focusable(true)` + `set_default_focus(true)`，否则系统把按键路由给 ArkTS 层焦点节点（日志里表现为 `Row/secure_field`），NDK `on_key_event` 回调永不触发，键盘输入到达不了 Rust（2026-08-18 修复）。现象：`[diag] key_event native callback fired` 一条都没有。
- **OHOS 不能缓存修饰键状态**：OHOS 无 `ModifiersChanged` 系统通知，只能在每次按键事件里读取 `modifier_state` 即时重建，**禁止保存 last_modifiers/capslock 状态机**——失焦丢 Release 会永久卡住（一直以为 Ctrl 按着）。正确做法：每次 KeyDown 前补发 `ModifiersChanged`（完整状态：modifiers + capslock），无状态。capslock 经 `OH_NativeXComponent_GetKeyEventCapsLockState` 查询入 `KeyEventData.capslock`。
- **OHOS 物理键盘字符输入需 X11 式兜底**：GPUI 的 `dispatch_key_event` 对真实按键不处理 key_char（桌面靠系统 IME，macOS NSTextInputContext / Wayland text-input）。OHOS 物理键盘无 IME，必须在 `OhosWindow::dispatch_input` 里加 X11 式兜底：GPUI 未消费 KeyDown（propagate）且 key_char 有值且修饰键 `is_subset_of(shift)` → `input_handler.replace_text_in_range(None, key_char)`。功能键（方向键/回车/F1-F12）key_char 为 None 天然安全，不误触发。
- **keycodes 索引函数必须惰性求值**：`letter_index`/`digit_index`/`numpad_digit_index` 用 `bool::then_some(急切值)` 时，`(raw - start)` 在越界也会求值，`u32` 减法下溢 panic（SIGABRT）。必须用 `then(|| ...)` 惰性求值。同类 bug 复用此模式要警惕。
- **设置 tab 不能嵌套 lease 主窗口**：`open_current_settings_file` 的 OHOS 分支必须用 App 级 `cx.defer`（回调里 `with_window` 已持有主窗口 lease），若用 `cx.defer_in` 则回调内再 `original_window.update` 会**嵌套窗口 lease 返回 Err**（被 `.ok()` 吞掉 → json 打不开、设置 tab 关不掉、无任何报错）。关闭 tab 的 `close_item_by_id` 返回异步 `Task`，必须 `.detach()`（丢弃即取消，tab 不关闭）。排查设置 tab 打不开 json / 不关闭，先确认这两点。
- **设置 tab 的 Esc 挂死**：`SettingsWindow` 键盘上下文 `key_context("SettingsWindow")` 的 `escape`/`ctrl-w` 在桌面 keymap 绑定 `workspace::CloseWindow`，OHOS 上设置是 tab 非独立窗口，触发 CloseWindow 会挂死。必须用 OHOS 专用 keymap（`assets/keymaps/default-ohos.json`，删 5 处 CloseWindow；`DEFAULT_KEYMAP_PATH` 在 `crates/settings/src/settings.rs` 加 `#[cfg(target_env = "ohos")]` 分支）。
- **最小化报 DisplaySync DelFromPipeline CurrentContext is nullptr**：帧回调启停**不要**挂在 `GainedFocus`/`LostFocus` 上——该事件被真实焦点（`StageEventType::Active/Inactive`）和窗口可见性（`windowVisibilityChange` 路由）两个来源复用，最小化时可能重复触发 `disable_frame_callback` → 第二次 `UnregisterOnFrameCallback` 时 DisplaySync 管道 context 已删 → `DelFromPipeline CurrentContext is nullptr`。修复（2026-08-22）：`windowVisibilityChange` 走独立 `Event::VisibilityChanged(bool)`，帧回调启停移入 `window.rs` 的 VisibilityChanged 分支；`enable/disable_frame_callback` 用 `FRAME_CALLBACK_ENABLED`（AtomicBool）幂等（已注册/已注销直接 return，标志收进函数内部维护），`window.rs` 不再外部 set。详见 `移植记录/bugfix/2026-08-21-ohos-idle-cpu-on-demand-vsync.md` 的"后续修复"章节。
- **IME 会话的决策判据必须含窗口活跃状态，且不要缓存"是否已绑定"**：窗口恢复时事件序为 `SHOWN(1)` → `ACTIVE(2)`（后者约晚 100ms，`windowVisibilityChange(true)` 还要再晚约 30ms）。只有 `ACTIVE` 同时满足「已可见 + 已获焦」；在 `SHOWN` 上发起 attach 时 `attachWithUIContext`/`showTextInput` **不抛异常、ack 也正常**，但系统不建会话——静默失败。所以 attach/detach 的判据包含 `active`（由 `Event::GainedFocus`/`LostFocus` 维护）。当前实现（2026-09-19）：唯一决策点 `OhosWindow::update_ime_enabled()` 在 `crates/gpui_ohos/src/ohos/window.rs`，挂在每帧 `completed_frame`，判据 `active && input_handler.is_some()`，靠本地镜像 `ime_enabled` 做边沿检测——历史两轮（Rust `ime_attached`、ArkTS `ImePlugin.attached` 早退）都因缓存"已绑定"而在某个时序失同步卡死，不要再引入这类缓存。
