---
name: zcoder-codemap
description: zcoder（Zed → HarmonyOS NEXT 移植）项目的代码架构地图，记录程序启动流程、模块入口与跨运行时边界（NAPI / XComponent / 事件循环）。用于快速定位启动链路上各模块的入口函数与触发方式。
---

# zcoder-codemap：zcoder 启动流程架构地图

本文件记录 zcoder（代码库为 Zed，产品名 zcoder，移植到 HarmonyOS NEXT）的**程序启动流程**。只记录模块入口、跨文件跳转、跨运行时跳转，不追踪完整执行路径。

所有路径均为相对项目根目录（`/mnt/linux_share/workspace/zcoder`）的相对路径。

## 启动链路总览

zcoder 的启动是「ArkTS 壳 → NAPI 桥 → Rust 入口 → GPUI 事件循环」四段式：

```
系统拉起 HAP
  → EntryAbility (ArkTS, RustAbility 基类)
  → 加载 libzcoder.so
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
RustAbility 基类 在 node_modules/@ohos-rs/ability  [由 EntryAbility extends 继承；加载 moduleName 对应的 native 库 libzcoder.so 并调用 NAPI init]
```

关键配置：`EntryAbility.moduleName = "zcoder"`（对应 `libzcoder.so`；launch-zed 的 `[lib] name = "zcoder"` 使产物直接叫 `libzcoder.so`，`NAPI_BUILD_TARGET_NAME=zcoder` 使 NAPI 模块名与之统一）。

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

on_finish_launching 闭包是 `app.run(move |cx| {...})` 的回调，仅在收到 `Event::SurfaceCreate` 时由 `OhosPlatform::handle_ohos_event` 触发一次。闭包内按固定顺序调用全部模块的 `init`。每个模块 init 前后均有 `[boot] enter/exit init: <模块>` 日志（hilog tag=Zcoder），日志停在哪个 `enter init` 未出现 `exit init`，即初始化卡在该模块。

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
- 早期入口（logger 未注册，直连 hilog，tag=zcoder-boot）：`launch_app`（NAPI 入口，无日志）→ `start_zed_main`（direct_hilog）→ `main` → `zlog::init`（此后 log 宏进 hilog，tag=Zcoder）。

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
EntryAbility (ArkTS) 到 [NAPI init] 到 launch_app() 在 crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/src/launch_app.rs  [RustAbility 基类加载 libzcoder.so 后调用 init；launch-zed 的 #[ability] 宏生成代码调 launch_app]
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

以下按功能模块记录 zcoder 已分析的代码运行路径。模块名到入口函数，标注触发方式。

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

IME 输入（`InputEvent::ImeEvent` → GPUI InputHandler）：
```
IME 到 handle_input_event() 的 ImeEvent 分支 在 crates/gpui_ohos/src/ohos/window.rs  [收到 IME 事件时调用；经 foreground_executor.spawn 异步处理]
IME 到 ImeEvent::TextInputEvent → replace_text_in_range / unmark_text 在 crates/gpui_ohos/src/ohos/window.rs  [文本提交]
IME 到 ImeEvent::EnterEvent → replace_text_in_range("\n") 在 crates/gpui_ohos/src/ohos/window.rs  [回车]
IME 到 ImeEvent::BackspaceEvent → 按 selection 删字符 在 crates/gpui_ohos/src/ohos/window.rs  [退格]
IME 到 ImeEvent::ImeStatusEvent(Hide) → unmark_text + notify_keyboard_hidden 在 crates/gpui_ohos/src/ohos/window.rs  [键盘隐藏]
IME 到 IME::new + insert_text/on_status_change/on_backspace/on_enter 在 crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs  [on_surface_created 时创建 IME 并注册系统软键盘回调；经 TSFN 转 ImeEvent 入 event_loop]
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
调度 到 OhosDispatcher::new() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 OhosPlatform::new() 调用；创建主线程优先级队列 + TSFN waker（无独立定时器线程，延迟任务走 FFRT，见定时器模块）]
调度 到 set_waker() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 OhosPlatform::set_app() 调用；注册 OpenHarmonyWaker 唤醒主线程]
调度 到 dispatch() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 GPUI 后台任务调用；每个任务 std::thread::spawn 一个新线程执行（不经主线程，与 Linux 的 Worker 线程池不同）]
调度 到 dispatch_on_main_thread() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 GPUI 主线程任务调用；进 PriorityQueueSender 后 waker.wake()，由 run_loop 消费（handle_ohos_event 的 UserEvent 分支跑 run_foreground_tasks）]
调度 到 execute_runnable() 在 crates/gpui_ohos/src/ohos/dispatcher.rs  [由 FFRT 定时回调 / run_loop 消费任务时调用；runnable.run() 执行 GPUI 任务]
```

跨运行时跳转：
```
OhosDispatcher::dispatch_on_main_thread() 到 [main_sender + OpenHarmonyWaker::wake] 到 OpenHarmonyApp run_loop 收 UserEvent  [入队后唤醒主线程；wake 每次实时读全局 WAKER（见常见坑 WAKER 时序）]
```

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

扩展本体是 **WASM 组件**（component model），由 wasmtime 在 zcoder 主进程内运行，不经独立 extension host 进程。跨运行时边界是 WIT ABI + 消息通道。扩展声明的语言服务器/调试适配器/context server 由 zcoder 创建独立二进制进程。

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
zcoder 到 WasmExtension::call() 到 [ExtensionCall channel] 到 wasm 导出函数（Extension 组件）  [zcoder → 扩展：init/language_server_command/run_slash_command 等导出函数在 tokio 线程池的 Store<WasmState> 上执行]
WasmState 到 [MainThreadCall channel] 到 主线程（window/workspace 委托）  [扩展 → zcoder 主线程：需主线程状态时切回执行]
WasmState process::Host::run_command() 到 [CapabilityGranter + util::command] 到 子进程  [扩展请求执行命令 → manifest + 用户设置双重授权后 zcoder 创建子进程]
ExtensionLspAdapter 到 [path_from_extension] 到 扩展目录内二进制 → lsp.rs spawn 子进程  [语言服务器二进制从扩展包内解析并以独立进程启动]
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
check_if_user_installed() 到 [delegate.which] 到 which() 在 crates/project/src/lsp_store.rs  [OHOS 分支（cfg ohos，14780）经 cmd-agent 远程在 OpenEuler VM 上执行 which；非 ohos 分支（14799）用 which crate 本机查 PATH]
which miss 到 [cmd-agent spawn -> exit != 0] 到 ensure_program_installed() 在 crates/gpui_ohos/depend/ohos-openeuler-agent/cmd-agent-server/src/install.rs  [which 未命中触发 VM 后台 dnf 自动安装（单 worker 串行），下次查询命中即直接用 VM 已装 LSP binary，跳过下载]
```

### 关键配置与产物

- `hap/entry/src/main/ets/entryability/EntryAbility.ets`：`moduleName = "zcoder"`。
- `script/bundle-ohos`：`cargo build --lib -p launch-zed`（`CRATE="launch-zed"`）→ 产物直接是 `libzcoder.so`（launch-zed 的 `[lib] name = "zcoder"`），复制到 HAP `entry/libs/arm64-v8a/`（`OHOS_LIB_NAME` 可覆盖）。
- `crates/gpui_ohos/depend/openharmony-ability/crates/launch-zed/`：NAPI 入口 crate（cdylib，libzcoder.so）。`src/launch_app.rs` 的 `#[ability] launch_app` + `src/lib.rs` 的 `pthread_mutex_*` 补丁符号 + `build.rs` 的 `napi_build_ohos::setup()`。
- `crates/zed/src/lib.rs`：仅 `#[cfg(target_env = "ohos")] include!("main.rs")`。
- `crates/zed/src/main.rs`：`#[cfg(target_env = "ohos")] pub fn start_zed_main(base_path: Option<String>)`（设 data_dir → main）。
- `crates/zed/Cargo.toml`：`[lib] crate-type = ["rlib"]`；ohos 分支无 `openharmony-ability`/`napi` 依赖（依赖反转：launch-zed 依赖 zed）。
- `.cargo/config.toml`：ohos 目标 rustflags `--cfg gles` + `target-feature=+fp16`。

## 常见坑

- **on_finish_launching 只在 SurfaceCreate 触发**：窗口创建代码必须放在 app.run 的回调里，且依赖 SurfaceCreate 事件已到达（native_window 可用）。若在事件到达前建窗口会拿不到 renderer。
- **moduleName 与库名强绑定**：`NAPI_BUILD_TARGET_NAME`（=zcoder）必须与 so 文件名 `libzcoder.so` 一致。launch-zed 是叶子 crate（无消费者），可用 `[lib] name = "zcoder"` 直接产出 `libzcoder.so`；**不能改 zed 的 `[lib] name`**（会把 Rust crate 名改掉，`use zed::` 全断）。
- **include!("main.rs") 只在 ohos 启用**：桌面端 crates/zed 仍是二进制 crate（`[[bin]] name = "zed"`）。
- **GPU 初始化在平台层、surface 在窗口层**：`WgpuContext::new()`（Instance/Adapter/Device）在 `OhosPlatform::new()` 时创建，可在 app 设置前完成；`WgpuRenderer`（surface）在 `OhosWindow::initialize_renderer()` 时才建，必须等 `native_window` 可用（SurfaceCreate 后）。两个阶段分离，排查黑屏先确认哪一步失败。
- **dispatch() 直接线程 spawn 的风险**：`OhosDispatcher::dispatch` 用 `std::thread::spawn` 跑后台任务，若任务内直接触 NAPI 会 SIGABRT（NAPI 只能在创建线程调用）。跨线程的 NAPI 调用必须走 `OpenHarmonyApp::bridge()` 的 TSFN 封装。
- **初始化卡点定位**：hilog（tag=Zcoder）里 `[boot] enter init: <模块>` 出现而对应 `exit init` 未出现，即初始化卡在该模块；`enter init` 一个都没出现则卡在更早（看 `zcoder-boot` tag 的 `start_zed_main` / `building application` / `calling app.run` / `on_finish_launching entered`；launch_app 无入口日志，若连 `start_zed_main` 都没有则卡在 NAPI init 之前）。
- **日志双 tag 体系**：`zlog::init()` 之前（`start_zed_main`）用 `direct_hilog_info` 直连，tag=`zcoder-boot`；`zlog::init()` 之后所有 `log::xxx!` 走重定向，tag=`Zcoder`。launch_app（NAPI 入口）不打印日志。抓日志两个 tag 都要过滤。
- **WAKER 时序 bug（黑屏根因）**：`OhosPlatform::set_app` → `create_waker()`（读全局 WAKER）早于 ArkTS `init` → `create_lifecycle_handle()`（写全局 WAKER）。`wake()` 若用 `create_waker` 返回的 None 快照则永远静默失败 → UserEvent 死掉 → `run_foreground_tasks` 不驱动 → 窗口创建任务饿死 → 黑屏。修复：`wake()` 必须每次实时读全局 WAKER（`(*WAKER).read()`），不能存快照。详见 `移植记录/bugfix/2026-08-17-ohos-black-screen-waker.md`。
- **UserEvent 是 foreground executor 的唯一驱动**：GPUI `cx.spawn` 的窗口创建任务（`restore_or_create_workspace`）只在 `handle_ohos_event` 收到 `Event::UserEvent` 时经 `run_foreground_tasks` 执行。SurfaceCreate 只触发 on_finish_launching，不驱动任务队列；若 UserEvent 不来，窗口永远不创建（黑屏症状）。排查这类问题先看 UserEvent 是否到达。
- **XComponent 必须 focusable + default_focus 才能收按键**：`XComponent::new()` 后必须 `set_focusable(true)` + `set_default_focus(true)`，否则系统把按键路由给 ArkTS 层焦点节点（日志里表现为 `Row/secure_field`），NDK `on_key_event` 回调永不触发，键盘输入到达不了 Rust（2026-08-18 修复）。现象：`[diag] key_event native callback fired` 一条都没有。
- **OHOS 不能缓存修饰键状态**：OHOS 无 `ModifiersChanged` 系统通知，只能在每次按键事件里读取 `modifier_state` 即时重建，**禁止保存 last_modifiers/capslock 状态机**——失焦丢 Release 会永久卡住（一直以为 Ctrl 按着）。正确做法：每次 KeyDown 前补发 `ModifiersChanged`（完整状态：modifiers + capslock），无状态。capslock 经 `OH_NativeXComponent_GetKeyEventCapsLockState` 查询入 `KeyEventData.capslock`。
- **OHOS 物理键盘字符输入需 X11 式兜底**：GPUI 的 `dispatch_key_event` 对真实按键不处理 key_char（桌面靠系统 IME，macOS NSTextInputContext / Wayland text-input）。OHOS 物理键盘无 IME，必须在 `OhosWindow::dispatch_input` 里加 X11 式兜底：GPUI 未消费 KeyDown（propagate）且 key_char 有值且修饰键 `is_subset_of(shift)` → `input_handler.replace_text_in_range(None, key_char)`。功能键（方向键/回车/F1-F12）key_char 为 None 天然安全，不误触发。
- **keycodes 索引函数必须惰性求值**：`letter_index`/`digit_index`/`numpad_digit_index` 用 `bool::then_some(急切值)` 时，`(raw - start)` 在越界也会求值，`u32` 减法下溢 panic（SIGABRT）。必须用 `then(|| ...)` 惰性求值。同类 bug 复用此模式要警惕。
- **设置 tab 不能嵌套 lease 主窗口**：`open_current_settings_file` 的 OHOS 分支必须用 App 级 `cx.defer`（回调里 `with_window` 已持有主窗口 lease），若用 `cx.defer_in` 则回调内再 `original_window.update` 会**嵌套窗口 lease 返回 Err**（被 `.ok()` 吞掉 → json 打不开、设置 tab 关不掉、无任何报错）。关闭 tab 的 `close_item_by_id` 返回异步 `Task`，必须 `.detach()`（丢弃即取消，tab 不关闭）。排查设置 tab 打不开 json / 不关闭，先确认这两点。
- **设置 tab 的 Esc 挂死**：`SettingsWindow` 键盘上下文 `key_context("SettingsWindow")` 的 `escape`/`ctrl-w` 在桌面 keymap 绑定 `workspace::CloseWindow`，OHOS 上设置是 tab 非独立窗口，触发 CloseWindow 会挂死。必须用 OHOS 专用 keymap（`assets/keymaps/default-ohos.json`，删 5 处 CloseWindow；`DEFAULT_KEYMAP_PATH` 在 `crates/settings/src/settings.rs` 加 `#[cfg(target_env = "ohos")]` 分支）。
- **最小化报 DisplaySync DelFromPipeline CurrentContext is nullptr**：帧回调启停**不要**挂在 `GainedFocus`/`LostFocus` 上——该事件被真实焦点（`StageEventType::Active/Inactive`）和窗口可见性（`windowVisibilityChange` 路由）两个来源复用，最小化时可能重复触发 `disable_frame_callback` → 第二次 `UnregisterOnFrameCallback` 时 DisplaySync 管道 context 已删 → `DelFromPipeline CurrentContext is nullptr`。修复（2026-08-22）：`windowVisibilityChange` 走独立 `Event::VisibilityChanged(bool)`，帧回调启停移入 `window.rs` 的 VisibilityChanged 分支；`enable/disable_frame_callback` 用 `FRAME_CALLBACK_ENABLED`（AtomicBool）幂等（已注册/已注销直接 return，标志收进函数内部维护），`window.rs` 不再外部 set。详见 `移植记录/bugfix/2026-08-21-ohos-idle-cpu-on-demand-vsync.md` 的"后续修复"章节。
