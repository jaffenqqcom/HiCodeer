# rust-embed debug 构建下资源读取崩溃（SIGABRT）

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植，产品名 zcoder，so 为 `libzcoder.so`）在 OHOS 设备上启动即崩溃。应用在 SurfaceCreate 回调中初始化 settings 时，`SettingsAssets::get("settings/default.json")` 返回 `None`，`asset_str` 里的 `.expect(path)` 触发 panic 并 `abort`，进程 SIGABRT。崩溃仅在 OHOS 设备上出现，桌面端（Linux/macOS）debug 构建启动完全正常。

## 问题表现

启动后数秒内 SIGABRT，设备产生崩溃日志 `cppcrash-com.zcoder.studio-*.log`：

- `LastFatalMessage: [OnSurfaceCreated] crash occured on callback`
- 崩溃栈关键帧（由深到浅）：
  - `core::option::expect_failed`
  - `<core::option::Option<rust_embed_utils::EmbeddedFile>>::expect`
  - `util::asset_str::<settings::SettingsAssets>`
  - `settings::default_settings`
  - `settings::init`
  - `zed::main::{closure#12}`
- 完整调用链：`OnSurfaceCreated`（ACE 引擎）→ `ohos_xcomponent_binding::events::native_callbacks::on_surface_created` → `OpenHarmonyApp::run_loop` 回调 → `OhosPlatform::handle_ohos_event` → `Application::run` 闭包 → `zed::main::{closure#12}` → `settings::init` → `default_settings` → `asset_str::<SettingsAssets>`
- 设备崩溃栈各帧 so 的 build-id 均为 `8b2ad00cb7650a01`
- 可复现：每次启动必崩，固定在同一处；桌面端同代码 debug 构建正常

## 问题原因

根因是 **debug 构建下 rust-embed 8.11.0 采用 `dynamic` 实现，运行时从编译机磁盘绝对路径读取资源，设备上不存在该路径**。

rust-embed 的 `#[derive(RustEmbed)]` 派生宏会生成**两套互斥实现**（见 `rust-embed-impl 8.11.0` 源码 `generate_assets`）：

- `embedded` 实现（`#[cfg(not(debug_assertions))]`，仅 release 编译）：编译期把文件数据内嵌进二进制，`get()` 用静态数组二分查找，不依赖磁盘。
- `dynamic` 实现（`#[cfg(debug_assertions)]`，仅 debug 编译）：编译期只记录 `folder` 的**绝对路径字符串**，运行时 `get()` 执行 `Path::new(#folder_path).join(file_path)` 拼出磁盘路径，再 `canonicalize()` 校验并 `read_file_from_fs` 从磁盘读取。

其中 `#folder_path` 是编译时固化的**开发机绝对路径** `/mnt/linux_share/workspace/zcoder/assets`（来自 `SettingsAssets` 的 `#[folder = "../../assets"]`，按 `CARGO_MANIFEST_DIR` 解析到项目根）。

zcoder 用 `script/bundle-ohos` 默认构建 **debug 模式**（无 `--release`）。部署到 OHOS 设备后：

1. `settings::init` 调 `asset_str::<SettingsAssets>("settings/default.json")`
2. `get()` 尝试读取 `/mnt/linux_share/workspace/zcoder/assets/settings/default.json`
3. 该路径只在开发机存在，设备沙箱内必然不存在 → `canonicalize()` 返回 `Err`
4. `get()` 返回 `None`
5. `asset_str` 的 `.expect(path)` panic → `std::process::abort` → SIGABRT

桌面 debug 构建正常，正是因为编译机路径在本地存在，`dynamic` 能读到文件。同一份代码到设备上就必然失败。

### 诊断过程中的死路（重要教训）

- **误判"内嵌资源为空"**：最初用 `strings` 在 so 里搜到 `settings/default.json` 字符串，据此推断"内嵌表里有路径但数据缺失"。实际该字符串是 `asset_str::<SettingsAssets>("settings/default.json")` 调用处的**代码字面量**，与内嵌数据无关，此判断误导了排查方向。
- **关键转折 1——build-id 对比**：设备崩溃栈所有帧的 so build-id 为 `8b2ad00cb7650a01`，与本地编译产物 `readelf -n` 的 Build ID **完全一致**，排除了"设备装的是旧 so"假设，证明问题在当前构建里。
- **关键转折 2——绝对路径特征**：在 so 里 `strings` 搜到 `/mnt/linux_share/workspace/zcoder/assets`（2 次），这是 `dynamic` 实现固化的 `folder_path` 字符串，直接坐实 debug 动态读取根因。

## 解决方案

给 workspace 级 `rust-embed` 依赖启用 **`debug-embed`** feature。

`debug-embed` 使 rust-embed 的 `embedded` 实现（编译期内嵌 + 二分查找）在 debug 构建下也**无条件编译**，彻底绕开 `dynamic` 实现的磁盘路径读取。

修改根 `Cargo.toml`（第 788 行）：

```toml
# 修改前
rust-embed = { version = "8.11", features = ["include-exclude"] }

# 修改后
rust-embed = { version = "8.11", features = ["include-exclude", "debug-embed"] }
```

**方案对比**：
- 方案 A：`script/bundle-ohos` 改用 `--release` 构建。零代码改动，但全量 release 编译预计 20-40 分钟、无调试符号不利后续定位。
- 方案 B（采用）：加 `debug-embed` feature。保持 debug 构建快速、保留调试符号，同时资源内嵌。桌面 debug 构建仅二进制体积变大，行为一致。

**验证结果**：
- 重新构建后检查新 so：编译机绝对路径字符串消失（0 次，dynamic 实现已移除）；`settings/default.json` 出现在 embedded ENTRIES 表（2 次）；default.json 独有键 `agent_buffer_font_family` 内容真正内嵌。
- 部署到设备：进程 `com.zcoder.studio` 稳定运行 1 分钟以上无崩溃，设备无新 crash 日志。

## 修改文件

- `Cargo.toml` — 根 workspace 依赖表：`rust-embed` 增加 `debug-embed` feature，使 debug 构建也内嵌资源，修复 OHOS 设备上 `get()` 读不到编译机路径导致的启动崩溃。

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
