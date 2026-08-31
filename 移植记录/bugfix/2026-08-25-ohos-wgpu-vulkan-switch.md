# OHOS 渲染后端 OpenGL → Vulkan 切换（本地 patches/wgpu + A/B 性能对比）

## 问题描述 (Problem Description)

zcoder 在 HarmonyOS NEXT 上默认使用 wgpu 的 **OpenGL ES 后端**渲染（`wgpu::Backends::GL`）。目标是切换到 **Vulkan 后端**以降低 CPU 驱动开销、获得更好的 GPU 驱动支持。但 OHOS 设备的窗口句柄是 `raw-window-handle` 的 **`OhosNdk`** 类型，而 **crates.io 原版 wgpu 的 Vulkan 后端不支持 OHOS surface 创建**（Vulkan surface 创建依赖平台的 `vkCreateXXXSurfaceKHR`，原版只有 Android 的 `VK_KHR_android_surface`）。需要验证：基于哪个 wgpu 版本、如何配置，才能让 zcoder 在 OHOS 上跑通 Vulkan。

## 问题表现 (Symptoms)

- zcoder 在 OHOS 上渲染后端硬编码 `wgpu::Backends::GL`，无法走 Vulkan。
- crates.io 原版 wgpu 29.0.4 的 `wgpu-hal/src/vulkan/instance.rs` 里，`create_surface` 的 `match (window_handle, display_handle)` 只有 `Wayland / Xlib / Xcb / Drm / AndroidNdk / Win32 / AppKit / UiKit` 分支，**没有 `OhosNdk` 分支**，OHOS 窗口会落到默认分支报 `"window handle OhosNdk is not a Vulkan-compatible handle"`。
- crates.io 最新稳定版 **v30.0.1**（2026-07-01 发布）的 Vulkan 后端同样**没有 `OhosNdk` surface 分支**，只在入口加载层支持 OHOS（`#[cfg(target_env = "ohos")] ash::Entry::load_from("libvulkan.so")`）；其 GLES 后端才支持 `OhosNdk`（`gles/egl.rs` 的 `(WindowKind::Unknown, Rwh::OhosNdk(handle)) => handle.native_window.as_ptr()`）。
- 依赖路径的坑：把 wgpu 复制到 zcoder `patches/` 下后用 `[patch.crates-io]` 引用，`cargo update` 报 `error inheriting version from workspace root manifest's workspace.package.version ... was not defined`。

## 问题原因 (Root Cause)

### 根因 1：原版 wgpu 的 Vulkan 后端不支持 OHOS surface

OHOS 的窗口句柄（`OhosNdkWindowHandle`，来自 raw-window-handle 0.6.2）在 `create_surface` 的 match 中没有对应分支，Vulkan surface 无法创建。OHOS 的 Vulkan 呈现需要 **`VK_OHOS_surface`** 扩展 + `vkCreateSurfaceOHOS` 函数（该函数用 `vkGetInstanceProcAddr` 动态加载）。

warp-ohos 项目的 wgpu（29.0.3）在 `wgpu-hal/src/vulkan/instance.rs` 中手工补了这段支持（`create_surface_ohos`，约 499-572 行）：

```rust
#[cfg(target_env = "ohos")]
fn create_surface_ohos(&self, window: *mut std::ffi::c_void)
    -> Result<super::Surface, crate::InstanceError> {
    // 1. 检查驱动支持 VK_OHOS_surface 扩展
    // 2. 通过 vkGetInstanceProcAddr 加载 vkCreateSurfaceOHOS
    // 3. 手写 VkSurfaceCreateInfoOHOS 结构体（s_type = 1000685000）
    // 4. 创建 surface
}

// create_surface match 里新增分支：
#[cfg(target_env = "ohos")]
(Rwh::OhosNdk(handle), _) => {
    self.create_surface_ohos(handle.native_window.as_ptr() as *mut std::ffi::c_void)
}
```

所有 OHOS 改动都包在 `#[cfg(target_env = "ohos")]` 下，桌面平台编译与上游一致。

### 根因 2：nested workspace 未被父 workspace 识别

把 wgpu 复制到 zcoder `patches/wgpu/`（zcoder workspace 目录树内），它有自己的 `[workspace]`（独立 workspace，`workspace.package.version = "29.0.3"`），但 **cargo 要求嵌套 workspace 必须在父 workspace 的 `exclude` 中声明**。未声明时，`patches/wgpu/naga/Cargo.toml` 里的 `version.workspace = true` 被 cargo 错误地解析到 zcoder 的 workspace（`[workspace.package]` 只有 `publish`/`edition`，**没有 `version`**），于是报 `workspace.package.version was not defined`。

## 解决方案 (Solution)

### 步骤 1：引入本地 patch 版 wgpu

把 warp-ohos 的 wgpu 源码复制到 zcoder 项目内，实现自包含（不依赖外部目录）。复制时排除隐藏文件（`.git`/`.github`/`.cargo` 等均为非编译所需，全部可删，`build.rs` 不引用 `.git`）：

```bash
rsync -a --exclude='/.*' /mnt/linux_share/workspace/warp-ohos/depends/wgpu/ /mnt/linux_share/workspace/zcoder/patches/wgpu/
```

### 步骤 2：配置 [patch.crates-io] 指向本地副本

`zcoder/Cargo.toml`：

```toml
[workspace]
resolver = "2"
# patches/wgpu is a self-contained nested workspace (own [workspace] + Cargo.lock),
# so exclude it from this workspace to keep its workspace-inherited values intact.
exclude = ["patches/wgpu"]
members = [ ... ]

# 依赖版本：29.0.4 → 29.0.3（patch 版本须满足依赖声明）
wgpu = "29.0.3"

[patch.crates-io]
# ... 现有条目（nix、ohos-xcomponent-binding 等）不动 ...
wgpu       = { path = "patches/wgpu/wgpu" }
wgpu-core  = { path = "patches/wgpu/wgpu-core" }
wgpu-hal   = { path = "patches/wgpu/wgpu-hal" }
wgpu-types = { path = "patches/wgpu/wgpu-types" }
naga       = { path = "patches/wgpu/naga" }
```

### 步骤 3：同步子 crate 版本（避免双版本共存）

`crates/gpui_wgpu/Cargo.toml`（dev-dependencies 直接依赖 naga）：

```toml
# 修改前
naga = { version = "29.0.4", features = ["wgsl-in"] }
# 修改后
naga = { version = "29.0.3", features = ["wgsl-in"] }
```

### 步骤 4：切换后端默认值为 Vulkan

`crates/gpui_ohos/src/ohos/wgpu_context.rs`：

```rust
// 修改前
let default_backends = wgpu::Backends::GL;
// 修改后
let default_backends = wgpu::Backends::VULKAN;
```

保留 `wgpu::Backends::from_env()` 的环境变量覆盖机制（`WGPU_BACKEND=gles` 可临时切回 GL）。

### 步骤 5：更新 Cargo.lock 并编译验证

`cargo update -p wgpu -p wgpu-core -p wgpu-hal -p wgpu-types -p naga` 强制重新解析到本地 patch，然后 `./script/bundle-ohos` 编译部署。

**验证日志**（patch 版 wgpu + Vulkan 跑通）：
```
OHOS WGPU backends configured: Backends(VULKAN)
Selected GPU adapter: "Maleoon 916B" (Vulkan)
[boot] WgpuRenderer: surface created
[boot] WgpuRenderer initialized successfully, adapter "Maleoon 916B"
```

### A/B 性能对比（OpenGL vs Vulkan，单核 CPU 占用，越低越好）

设备 16 核、麒麟 Maleoon 916B GPU，同一文件、稳定负载下各采样 6×10s：

| 负载 | Vulkan | OpenGL | 结论 |
|------|--------|--------|------|
| 持续滚动大文件 | 平均 105% | 平均 111-117% | Vulkan 低约 7% |
| 光标闪烁静止 | 平均 93% | 平均 111%+ | Vulkan 低约 16-30% |

**结论：Vulkan 在两种负载下 CPU 占用都更低**，静止负载优势更明显，与 Vulkan 低驱动开销的理论一致。因此保留 Vulkan 为默认后端。

### 回退方法

仅需改 `wgpu_context.rs` 一处：`default_backends = wgpu::Backends::GL`（wgpu 依赖 patch 可保留，本地 patch 的 GLES 同样可用）。

### 附加调研结论

- crates.io 原版 wgpu **最新稳定版 30.0.1** 的 Vulkan 后端**仍不支持** OHOS surface（仅 GLES 支持 `OhosNdk`）；OHOS Vulkan surface 支持存在于上游 **master/trunk**（未发布到 crates.io）。因此当前只能用本地 patch 版 wgpu 跑 OHOS Vulkan。
- wgpu 30 相对 29 是 major 升级（破坏性 API 变更），无公开基准证明性能更好，v30 的性能改动（NVIDIA 帧时间修复、外部信号量等）对 zcoder 场景收益甚微。

## 修改文件 (Modified Files)

- `patches/wgpu/**` — 新增：warp-ohos 的 wgpu 29.0.3 本地副本（含 `create_surface_ohos` / `VK_OHOS_surface` 支持），排除全部隐藏文件
- `zcoder/Cargo.toml` — `[workspace]` 加 `exclude = ["patches/wgpu"]`（nested workspace 识别）；`[patch.crates-io]` 追加 wgpu/wgpu-core/wgpu-hal/wgpu-types/naga 五个本地 path；`wgpu` 版本 29.0.4 → 29.0.3
- `crates/gpui_wgpu/Cargo.toml` — dev-dependencies 的 `naga` 版本 29.0.4 → 29.0.3（避免双版本共存）
- `crates/gpui_ohos/src/ohos/wgpu_context.rs` — `default_backends` 由 `GL` 改为 `VULKAN`，保留 `WGPU_BACKEND` 环境变量覆盖，更新英文注释（含 A/B 测试结论）

参考：[[ohos-debug-lessons]]
