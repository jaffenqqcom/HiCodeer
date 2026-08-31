# wasm 扩展在 OHOS 上无法运行及运行时 SIGSYS crash（权限缺失 + openat2 被 seccomp 拦截）

## 问题描述 (Problem Description)

zcoder 用 **wasmtime** 作为 wasm 运行时执行编辑器扩展（语言扩展、tree-sitter wasm grammar 等）。在 OHOS 移植过程中，wasm 扩展先后遇到两个障碍：

1. **wasm 无法运行**：缺少允许 JIT 创建可执行内存的权限，wasmtime 编译阶段即失败。
2. **wasm 运行时 crash**：权限补齐后可编译，但进程在运行时被内核 `SIGSYS` 杀死，原因是 `cap-primitives` 调用了 OHOS 沙箱不允许的 `openat2`(syscall 437)。

两个问题均属"wasm 在 OHOS 上运行"这一功能的递进障碍，合并记录。

## 问题表现 (Symptoms)

### 故障一：wasmtime JIT 无法创建可执行内存（权限缺失）

- zcoder 日志：`failed to compile wasm component: unable to make memory executable: Invalid argument (os error 22)`。
- 所有 wasm 扩展（html / java / nix / tsgo 等）无法实例化，对应语言无高亮 / LSP / 补全。
- tree-sitter wasm grammar 同样受限。
- 早期曾被误判为"OHOS 沙箱禁止 JIT，wasm 这条路走不通"（见旧报告 `2026-08-25-ohos-java-wasm-unsupported.md`），该结论因本次权限修复而失效，需同步更新。

### 故障二：wasm 运行时 SIGSYS crash（openat2 被 seccomp 拦截）

- 补齐权限后 wasm 可编译，但设备进程被杀，崩溃日志 `cppcrash-com.zcoder.studio-*.log` 关键行：`Reason:Signal:SIGSYS(SYS_SECCOMP) syscall number is 437`。
- syscall 437 = `openat2`，不在 OHOS 应用 seccomp 白名单中；内核在 syscall 入口直接发 `SIGSYS` 终止进程（**不是返回错误码**），故表现为无堆栈可捕获的硬杀。

## 问题原因 (Root Cause)

### 故障一根因（权限）

HarmonyOS NEXT 应用沙箱默认**禁止应用自建 JIT 创建可执行内存**（安全加固，防 ROP / 注入）。wasmtime 用 Cranelift JIT 把 wasm 编译成机器码，需要 `mmap(PROT_READ|PROT_WRITE)` 后 `mprotect(PROT_EXEC)`，`mprotect(PROT_EXEC)` 因沙箱限制返回 `EINVAL`（os error 22），导致 `make memory executable` 失败。

该限制可通过受限开放权限 `ohos.permission.kernel.ALLOW_WRITABLE_CODE_MEMORY`（system_basic 级别，API 14+）放行。`mprotect(PROT_EXEC)` 在内核侧需要 `PROT_EXEC` 白名单，该权限正是为此存在。早期把"沙箱禁止 JIT"当成绝对限制，忽略了此权限，导致了错误结论。

### 故障二根因（openat2）

wasmtime 的 WASI 能力型 FS 层 **`cap-primitives`** 在 Linux/OHOS 上用 `openat2` 的 `RESOLVE_BENEATH` 标志做路径逃逸防护（`src/rustix/linux/fs/open_impl.rs` 的 `open_beneath`）。

OHOS 应用 seccomp 白名单只含 `openat` / `newfstatat` / `statx` 等常规文件 syscall，**不含 `openat2`(437)，且没有任何应用权限可放行它**（经调研确认：应用 seccomp 白名单外的 syscall 不可经权限申请开放）。因此 `cap-primitives` 一旦调用 `openat2`，内核直接在入口发 `SIGSYS` 杀进程。

`openat` 与 `openat2` 的区别：`openat` 是旧式打开文件接口，OHOS 白名单允许；`openat2` 额外支持 `RESOLVE_BENEATH` / `RESOLVE_NO_MAGICLINKS` 等"安全解析"标志，用于防止路径逃逸，但 OHOS 沙箱未放行该 syscall。

## 解决方案 (Solution)

### 故障一修复：声明 ALLOW_WRITABLE_CODE_MEMORY 权限

在 HAP 配置中声明受限开放权限，使 wasmtime JIT 的 `mprotect(PROT_EXEC)` 在真机放行：

- `hap/entry/src/main/module.json5`：`requestPermissions` 新增该权限（含 `reason` 引用与 `usedScene`）。
- `hap/entry/src/main/resources/base/element/string.json`：新增 `perm_writable_code_memory_reason` 字符串，说明用途（为 wasmtime JIT 分配可写可执行内存以运行编辑器插件）。

**验证（真机实测）**：装带验证日志的本地包后，hilog 出现 `compiling / compiled / started wasm extension` 四组（html / java / nix / tsgo），证明权限生效、wasm 真正运行。验证手段为在 `crates/extension_host/src/wasm_host.rs` 注入 wasm 生命周期日志（编译 / 编译完成 / 启动），该日志属调试验证用途，未作为正式代码保留。

> 注：该权限为 system_basic 受限开放权限，正式发布需按鸿蒙规范走白名单申请 + 重签流程。早期关于"改 `UnsgnedReleasedProfileTemplate.json` 放开 apl 以放行"的判断是**错误**的——HarmonyOS 应用签名由 AGC 签发，改本地 Release 模板无效；调试应走 Debug 签名 / 调试证书。

### 故障二修复：cap-primitives OHOS patch（规避 openat2）

在 `patches/cap-primitives/` 对 `cap-primitives` 3.4.4 打补丁（与 nix / wgpu 统一的 `[patch.crates-io]` + path 覆盖机制），让 OHOS 下跳过 `openat2`、回退到用户态等价实现 `manually::open`（底层用白名单允许的 `openat`）。

核心改动（`patches/cap-primitives/src/rustix/linux/fs/open_impl.rs`）：

- 原 `open_beneath`（调用 `openat2`）的 cfg 由 `#[cfg(target_os = "linux")]` 改为 `#[cfg(all(target_os = "linux", not(target_env = "ohos")))]`，OHOS 下不编译 `openat2` 路径。
- 顶部 `openat2` 相关 import 的 cfg 同样加 `not(target_env = "ohos")`。
- `open_impl` 的 linux 分支（`#[cfg(all(target_os = "linux", not(target_env = "ohos")))]`）整体包住 `openat2` 尝试，OHOS 下直接落到 `manually::open`。
- 新增 OHOS 桩函数，永远返回 `ENOSYS`：
  ```rust
  #[cfg(all(target_os = "linux", target_env = "ohos"))]
  pub(crate) fn open_beneath(
      _start: &fs::File,
      _path: &Path,
      _options: &OpenOptions,
  ) -> io::Result<fs::File> {
      Err(rustix::io::Errno::NOSYS.into())
  }
  ```
- `mod.rs` 的 re-export 保持 `#[cfg(target_os = "linux")] pub(crate) use open_impl::open_beneath;`（OHOS 下指向桩，不把符号 gate 掉，避免 `E0432 unresolved import`）。
- 三个调用方（`canonicalize_impl.rs` / `open_entry_impl.rs` / `stat_impl.rs`）无需改动：经桩拿到 `ENOSYS` 后自动回退 `manually::*`，其注释已显式标注 "`ENOSYS` from `open_beneath` means `openat2` is unavailable"。

`Cargo.toml`（~992 行 `[patch.crates-io]` 段，含说明注释）：

```toml
# Local patched cap-primitives 3.4.4: on OpenHarmony `openat2` (syscall 437) is
# not in the app seccomp allow-list and terminates the process with SIGSYS,
# so this patch makes `open_beneath` fall back to `manually::open` (openat).
cap-primitives = { path = "patches/cap-primitives" }
```

**方案对比**：

- 方案 A（采用）：cap-primitives 加 OHOS cfg + 桩函数，调用方自动回退 `manually::open`。改动局部、与既有 patch 机制一致、静态审计自洽。
- 方案 B：整条 WASI FS 链路换实现——工作量大、风险高，未采用。
- 备选"申请权限放行 openat2"：经调研确认 OHOS 无任何权限可放行应用 seccomp 白名单外的 syscall（openat2 不在白名单且不可申请），**不可行**，排除。

**验证状态**：

- 静态审计：`openat2` 相关代码在 OHOS 下被 cfg 排除，符号由桩提供 `ENOSYS`，调用方自动回退 `manually::open`（底层用白名单允许的 `openat`），不再触达 `openat2`，cfg 分支自洽，无 `E0432` 未解析引用。
- 用户已确认问题解决（真机不再 crash）。推荐在开发机执行最终编译验证：`cargo check -p cap-primitives --target aarch64-unknown-linux-ohos`，并装包设备触发 wasm 文件操作（如打开各类 wasm 扩展语言文件）确认无 `SIGSYS`。

## 修改文件 (Modified Files)

- `hap/entry/src/main/module.json5` — `requestPermissions` 新增 `ohos.permission.kernel.ALLOW_WRITABLE_CODE_MEMORY`（含 reason 引用与 usedScene），放行 wasmtime JIT 可执行内存。
- `hap/entry/src/main/resources/base/element/string.json` — 新增 `perm_writable_code_memory_reason` 权限说明字符串。
- `Cargo.toml` — `[patch.crates-io]` 注册 `cap-primitives = { path = "patches/cap-primitives" }`，引入 OHOS openat2 规避补丁。
- `patches/cap-primitives/`（从 crates.io 下载 3.4.4 源码至 `zcoder/patches` 下）— `src/rustix/linux/fs/open_impl.rs` 加 OHOS cfg 保护与 `ENOSYS` 桩；`src/rustix/linux/fs/mod.rs` re-export 保持；调用方无需改动。

## 关联 (Related)

- 本文推翻旧报告 `2026-08-25-ohos-java-wasm-unsupported.md` 中"OHOS 沙箱禁止 JIT，wasm 走不通"的结论——该权限（`ALLOW_WRITABLE_CODE_MEMORY`）已使 wasm 真正运行，建议同步更新旧报告以免误导。
- 同类 seccomp / 沙箱限制问题参见目录内其他 `ohos-*` bugfix 记录与 `[[ohos-debug-lessons]]`。
