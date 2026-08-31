# clangd 启动时 which 找不到（resfile 挂载不可见 → 缺库 → include 路径 → which 时序竞态）

## 问题描述

zcoder 打开 C++ 项目（warp-ohos）时，clangd LSP 无法启动。核心症状是 **`which clangd` 在 guest 里找不到工具**，zcoder 误判"clangd 未安装"而走下载（aarch64 无预编译）→ "Failed to start language server" 报错。排查后发现这是一个**四层叠加的问题链**：resfile 挂载不可见 → clangd 缺动态库 → C++ 标准库头路径未配置 → which 启动时序竞态。解决完前几层后，`which clangd` 才真正返回 `/tools/bin/clangd`，clangd 17.0.6 成功启动并解析 C++ 文件。

## 问题表现

- `which clangd` 在 guest 里返回不存在；S42cmdagentd 诊断输出 `tools NOT at /sandbox/haps/entry/files/bin`（早期尝试）或 `ls: /tools/bin/clangd: No such file`。
- zcoder 报 `Failed to start language server "clangd": Clangd does not provide prebuilt binaries for aarch64 to fetch from GitHub`——误判 clangd 不存在去下载。
- 补库后 clangd spawn 成功但 **exit 127**：`/tools/bin/clangd: error while loading shared libraries: libffi.so.8: cannot open shared object file`。
- 再后 clangd 能跑但 **`#include <cstdio>` file not found**（C++ 标准库头找不到）。
- 最后观察：`which clangd` 前两次（启动早期）0.01s 立即失败，第三次（guest 就绪后）才成功——**启动时序竞态**。
- 复现：每次冷启动 C++ 项目必现（在修复前）。

## 问题原因

四层独立根因叠加：

### 层 1：resfile 解压位置不在 guest 9p 挂载树（最核心）

HarmonyOS resfile 安装时解压到**只读 `el1/bundle/<module>/resources/resfile`**（`application_resource_dir()` 返回，即 ArkTS 的 `context.resourceDir`）。而 QEMU 给 guest 的 9p 挂载只有 **el2/base（应用可写沙箱）→ `/sandbox`**。**resfile 整个不在 9p 挂载树内**，guest 看不到 `bin/clangd`、`cmd-agentd` 等一切 resfile 内容。

- cmd-agentd 之所以 guest 可见，是因为 `launch_app.rs` 启动时手动 `std::fs::copy` 到 `base_path`（files/）——tools 没有这个 copy。
- **曾误判**：以为"resfile 只解压根级文件、不支持子目录"，把 tools 从 `tools/` 子目录扁平化到 resfile 根（`bin/`、`lib64/` 直接放根）——部署后仍不可见。根因是 **resfile 整体在 el1/bundle，跟挂载树（el2/base）无关**，扁平化无效。

### 层 2：clangd 缺动态库（libLLVM 的间接依赖）

clangd → `libLLVM-17.so` → 依赖 `libffi.so.8`/`libedit.so.0`/`libz.so.1`/`libtinfo.so.6`。裁剪后的 rootfs 只有基础库（libc/libm/libstdc++/libgcc_s），tools/lib64 只有 libLLVM/libclang-cpp/libpython，**4 个库都没有** → clangd 加载 libLLVM 时找不到 libffi → exit 127。注意 `readelf -d clangd` 直接 NEEDED 里没有 libffi（是 libLLVM 的间接依赖），容易漏查。

### 层 3：C++ 标准库头路径未配置

`cstdio` 在 `/tools/include/c++/12/`（GCC 12 libstdc++），arch 特定的 `bits/c++config.h` 在 `/tools/include/c++/12/aarch64-openEuler-linux/`。zcoder 没生成 compile_commands.json，clangd 走 fallback 模式只用内置默认路径 + 环境变量，`CPATH=/tools/include` 只覆盖 C 头（stdio.h），不搜 `c++/12/` 子目录 → `#include <cstdio>` 找不到。

### 层 4：which 启动时序竞态（executor 未就绪被误判"不存在"）

OHOS 版 `which`（`lsp_store.rs`）走 `util::command`（cmd-agent 通道），内部 `executor()` 从 `qemu_cmd_agent_linker::executor()`（OnceLock）取全局 executor。**cmd-agent executor 是异步注册的**（QEMU guest 起来后）。LSP 触发 `which clangd` 早于 executor 注册时，`output()` 返回 Err，旧代码 `.ok()?` **把 Err 当 None**（= 程序不存在）→ 适配器走下载 → aarch64 无预编译 → 报错。实际 clangd 是存在的，只是那一刻 executor 没就绪。zcoder 之后会重试启动 LSP（间隔 ~96s），guest 就绪后第三次 which 才成功。

## 解决方案

四层逐一修复，`which clangd` 从"找不到"到 `/tools/bin/clangd`：

### 1. resfile 零复制只读挂载 → guest /tools

**关键洞察**：既然 resfile 在 el1/bundle（app 进程可读），QEMU 也在 app 进程内（dlopen 的 engine），**让 QEMU 直接把 el1/bundle 的 resfile 只读 9p 挂给 guest**，零复制、零启动开销。

```rust
// cmd-agent/src/lib.rs build_argv（sandbox fsdev 之前加）
"-fsdev", "local,security_model=none,id=fsdev_tools,path=<resource_dir>,readonly=on",
"-device", "virtio-9p-pci,id=fs_tools,fsdev=fsdev_tools,mount_tag=tools",
```
- **只读目录不能用 `security_model=mapped-file`**（要写映射元数据到 host 目录，el1/bundle 只读会失败），必须 `none`。
- guest `S40sandbox`：**先挂只读 `/tools`，再挂可写 `/sandbox`**。
- `S42cmdagentd`：cmd-agentd 优先 `/tools/cmd-agentd`（sandbox copy 兜底）。
- `rcS`/`profile`：`PATH=/tools/bin:/sandbox/haps/entry/files/zcoder/languages:$PATH`（两个挂载路径，zcoder 下载 LSP 到可写沙箱）、`LD_LIBRARY_PATH=/tools/lib64`、`PYTHONHOME=/tools`、`CPATH=/tools/include`。
- `launch_app.rs`：`QemuPaths.tools_mount = resource_dir`。

### 2. 补齐 clangd 缺的 4 个库

从 VM（**glibc 2.38 与 guest rootfs 完全一致**，可直接收集）复制到 `images/tools/lib64`：
```
libffi.so.8 → libffi.so.8.1.2
libedit.so.0 → libedit.so.0.0.72
libz.so.1 → libz.so.1.2.13
libtinfo.so.6 → libtinfo.so.6.4
```
（含符号链接；4 库自身依赖 libc/ld-linux/libtinfo，依赖闭环）。

### 3. 配置 clangd include 路径（全局 config.yaml）

guest `/root/.config/clangd/config.yaml`（打包进 rootfs）：
```yaml
CompileFlags:
  Add:
    - "-isystem"
    - "/tools/include"
    - "-isystem"
    - "/tools/include/c++/12"
    - "-isystem"
    - "/tools/include/c++/12/aarch64-openEuler-linux"
    - "-isystem"
    - "/tools/include/c++/12/backward"
```
用 `-isystem`（不 `-I`，避免系统头被当普通头报误警）；绝对路径（clangd 相对路径按源文件目录解析）。**clangd 全局配置位置**：项目 `.clangd` 或 `~/.config/clangd/config.yaml`（Linux 的 `$XDG_CONFIG_HOME/clangd/`）。

### 4. which executor 未就绪时重试

`lsp_store.rs` OHOS `which` 区分两种失败：
- **executor 未就绪**（`output()` 返回 Err）→ **重试**（`WHICH_EXECUTOR_RETRIES=15` × `WHICH_EXECUTOR_RETRY_INTERVAL=2s`，共 30s 上限）
- **命令真正不存在**（`which` 运行成功但非零/空）→ 返回 None（正确）

调用链无超时（`get_language_server_command(...).await.await`，`SERVER_DOWNLOAD_TIMEOUT` 只套下载分支），重试结果一定被使用；which 跑在 background_executor（不阻塞 UI）。效果：clangd 就绪从 4 分钟（等 zcoder LSP 重试 ~96s/次）缩短到 ~8 秒。

## 修改文件

- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agent/src/lib.rs` — `MOUNT_TAG_TOOLS="tools"`、`QemuPaths.tools_mount`、`build_argv` 加 resfile 只读 fsdev+device（security_model=none, readonly）。
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — `QemuPaths` 构造传 `tools_mount=resource_dir`（el1/bundle resfile 路径）。
- rootfs `etc/init.d/S40sandbox` — 先挂只读 `/tools` 再挂可写 `/sandbox`（容错不阻断 boot）。
- rootfs `etc/init.d/S42cmdagentd` — cmd-agentd 优先 `/tools/cmd-agentd`，fallback `/sandbox`。
- rootfs `etc/init.d/rcS` + `etc/profile` — PATH 含 `/tools/bin` 与 `/sandbox/.../zcoder/languages`，LD_LIBRARY_PATH/PYTHONHOME/CPATH 指向 `/tools`。
- rootfs `root/.config/clangd/config.yaml` — clangd 全局 include 路径（-isystem 4 个 /tools 头目录）。
- `script/bundle-ohos` — tools 扁平化到 resfile 根；打包前删旧 `tools/` 目录（消除 HAP 里 250M 重复）。
- `crates/gpui_ohos/depend/ohos-qemu-agent/images/tools/lib64/` — 补 `libffi.so.8`/`libedit.so.0`/`libz.so.1`/`libtinfo.so.6`（含符号链接）。
- `crates/project/src/lsp_store.rs` — OHOS `which` 改为 executor 未就绪时重试（15×2s），不再把 Err 当"命令不存在"；新增 `WHICH_EXECUTOR_RETRIES`/`WHICH_EXECUTOR_RETRY_INTERVAL` 常量。

排查辅助日志（诊断用，保留）：
- `S42cmdagentd` — 打印 cmd-agentd 路径、PATH、`ls /tools/bin/clangd` 与 `ls /tools`。

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
