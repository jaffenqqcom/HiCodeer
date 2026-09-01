# QEMU 后端 LSP 路径设置四连环（URI 映射 / 工作区挂载 / HOME 缓存 / 编译数据库）

## 问题描述

zcoder 移植到 HarmonyOS NEXT，命令执行后端从 OpenEuler VM（共享盘挂载在 `/mnt/linux_share`）切换到 QEMU guest（initramfs + 9p 挂载）后，LSP（clangd 等）暴露出 **4 个连环的路径设置问题**：LSP URI 被错误转成 VM 专属路径、clangd 收到的工作区路径在 guest 里打不开、索引缓存落在内存文件系统重启即丢、编译数据库找不到。四者根子都是同一个——**QEMU guest 的路径语义既不同于 OpenEuler VM（`/mnt/linux_share`），也不同于设备（`/storage/Users/currentUser`）**，而旧代码只覆盖了 VM 的路径语义。逐一修复后，clangd 在 QEMU 下能正确打开工作区文件、缓存持久、编译数据库加载成功。

## 问题表现

- **问题 1（URI 映射错目标）**：QEMU 构建下，LSP 出站/入站消息里的 URI 仍被 `map_uri_device_to_vm` / `map_uri_vm_to_device` 转成 `file:///mnt/linux_share/...`——这是 VM 专属路径，QEMU guest 里不存在。
- **问题 2（工作区路径 guest 打不开）**：clangd 报 `VFS: failed to set CWD to /storage/Users/currentUser/workspace/warp-ohos/hap/entry/src/main/cpp/services: No such file or directory`。
- **问题 3（缓存不持久）**：每次启动 clangd 都全量重索引（慢），索引缓存没落到持久盘。
- **问题 4（编译数据库找不到）**：clangd 打开每个 C++ 文件都报 `I[...] Failed to find compilation database for /storage/Users/currentUser/workspace/warp-ohos/hap/entry/src/main/cpp/bridge/shell_bridge_process.cpp`（Info 级，clangd 走 fallback 参数，宏/include 不准）。

## 问题原因

四层独立根因，但共同根因是 **QEMU guest 的路径布局与 VM/设备都不同**：

### 层 1：LSP URI 映射只按 target_env="ohos" 包裹，QEMU 下也执行

`crates/lsp/src/lsp.rs` 的 `map_uri_device_to_vm` / `map_uri_vm_to_device` 用 `#[cfg(target_env = "ohos")]` 包裹——只区分 OHOS 与非 OHOS，**不区分 VM 后端与 QEMU 后端**。QEMU 构建（`--no-default-features --features qemu`）下它们仍把设备路径转成 `/mnt/linux_share`（VM 的共享盘挂载点），但 QEMU guest 里这个路径不存在 → LSP 收发的 URI 全部指向无效路径。

关键前提：Cargo feature 是 **per-crate** 的，`launch-zed` 的 `qemu` feature 不会自动传到 `lsp` crate，必须沿直接依赖链 `launch-zed → zed → language → lsp` 逐层转发。

### 层 2：工作区挂载到 `/ws/{n}`，设备路径 URI 在 guest 不存在；且编号每次重启变化

QEMU host 侧 `run_mount`（`cmd-agent/src/executor.rs`）给每个打开的工作区分配 `guest_path = /ws/{n}`（n 是递增编号），guest 里工作区挂在 `/ws/{n}`。于是：
- clangd 收到的设备路径 URI（`/storage/Users/currentUser/...`）在 guest 里不存在 → VFS set CWD 失败（问题 2）。
- 即使 clangd 用 `/ws/{n}`，**编号每次重启、每次打开都变**，会破坏 clangd 缓存里保存的路径一致性（用户判断：`/ws/{n}` 方案下历史分析数据不可复用）。

### 层 3：guest 是 initramfs，HOME 缓存落内存 rootfs

QEMU guest 用 `-initrd rootfs.cpio.zst` 启动，rootfs 是**内存文件系统**。guest 的 init 脚本（`S42cmdagentd` 等）没有显式设置 HOME，cmd-agentd 以非登录 shell 拉起，clangd 继承的 HOME 指向 rootfs 内（如 `/root`），`~/.cache/clangd/index` 写在内存里 → QEMU 重启缓存全部丢失。guest 里唯一可写且持久的是 9p 挂载：`/sandbox`（= 设备 `/data/storage/el2/base`）、工作区（动态挂载）。

### 层 4：compile_commands.json 已生成但 clangd 找不到（位置 + 路径双重问题）

warp-ohos 构建时 CMake 已导出 compile_commands.json，但有两份、各有问题：
- `.cxx/default/default/release/arm64-v8a/compile_commands.json`：VM 上 hvigor 构建生成，路径是 `/mnt/linux_share/...`（VM 路径），与 QEMU guest 里的设备路径不匹配。
- `hap/.bitfun/.deveco/.cxx/compile_commands.json`：DevEco IDE 在设备侧生成，路径是设备路径 `/storage/Users/currentUser/...`（**正确**），但位于 `.bitfun/.deveco/.cxx/` 深目录——clangd 从源文件目录（`src/main/cpp/bridge/`）逐级向上找 compile_commands.json，**不会经过 `.bitfun/`** → 找不到。

`Failed to find compilation database` 与索引缓存无关，是 clangd 每次打开文件找编译参数（compile_commands.json）失败，与"扫描结果没保存"无关。

## 解决方案

### 方案 1：`feature = "qemu"` 宏，QEMU 下不调用 LSP URI 映射

核心洞察：QEMU 下 URI 应保持**设备路径原样**（配合方案 2 的同名挂载），而不是转成 `/mnt/linux_share`。做法：
- `lsp` crate 定义 `qemu = []` feature，沿 `launch-zed → zed → language → lsp` 转发。
- `lsp.rs` 两个 map 函数的转换版定义 cfg 从 `#[cfg(target_env = "ohos")]` 改为 `#[cfg(all(target_env = "ohos", not(feature = "qemu")))]`；出站调用点包 `#[cfg(not(feature = "qemu"))]`。
- `input_handler.rs` 入站调用点分支1 cfg 改为 `#[cfg(all(target_env = "ohos", not(feature = "qemu")))]`（QEMU 下走 `Cow::Borrowed` 原样），分支2 相应改为 `#[cfg(not(all(...)))]`。

```rust
// lsp.rs — QEMU 下转换版不编译、调用点不调用
#[cfg(all(target_env = "ohos", not(feature = "qemu")))]
pub(crate) fn map_uri_device_to_vm(message: &str) -> String { /* ...VM 路径替换... */ }

// 调用点：QEMU 下这行消失，message 保持原值
#[cfg(not(feature = "qemu"))]
let message = map_uri_device_to_vm(&message);
```

### 方案 2：工作区挂载改为**设备路径同名**（方案 B）

关键决策：不用 `/ws/{n}`（编号每次变化、破坏缓存路径），让 QEMU guest 里工作区路径与设备**完全一致**。改 host 侧 `run_mount`：

```rust
// 之前
let guest_path = format!("/ws/{sequence}");
// 之后：挂载到设备路径同名
let guest_path = path.to_string();
```

guest 侧 `worker_run_mount` 已有 `create_dir_all(guest_path)`（递归建目录链）+ `mount -t 9p`，无需改。path_map 登记 `{host_root=设备路径, guest_root=设备路径}` → 命令 cwd/args 透传。sandbox 映射（`/data/storage/el2/base → /sandbox`）保持不变。**工作区路径转换在 cmd-agentd 里不再需要**（同名挂载天然透传），但 sandbox 转换仍需要。

### 方案 3：注入持久 HOME，LSP 缓存落设备盘

guest 侧 `cmd-agentd` 让所有命令的 HOME 指向持久挂载 `/sandbox/home`（= 设备 `/data/storage/el2/base/home`）：
- `cmd-agentd/src/exec.rs`：定义 `pub(crate) const GUEST_PERSISTENT_HOME: &str = "/sandbox/home"`，在 `build_command` 里 `cmd.env("HOME", GUEST_PERSISTENT_HOME)` 注入每个命令。
- `cmd-agentd/src/main.rs`：启动时 `create_dir_all("/sandbox/home")` 一次性建好（`/sandbox` 在 cmd-agentd 启动时必已挂载，因为 S42cmdagentd 自己就是从 `/sandbox` 里找到 cmd-agentd 启动的）。**不在 spawn 时 mkdir**——HOME 指向的路径必须从一开始就确定存在。

效果：clangd 缓存 → `/sandbox/home/.cache/clangd/index/`（设备持久盘），rust-analyzer 等其他 LSP 的 `~` 缓存同样落盘。

### 方案 4：把设备路径版的 compile_commands.json 放到项目根

warp-ohos 的 `.bitfun/.deveco/.cxx/compile_commands.json`（设备路径版，177KB、零 `/mnt/linux_share` 残留、含 shell_bridge_process）正是 clangd 需要的，复制到项目根即可让 clangd 向上找到：

```bash
cp hap/.bitfun/.deveco/.cxx/compile_commands.json <项目根>/compile_commands.json
```

clangd 从 `src/main/cpp/bridge/` 向上 → `warp-ohos/compile_commands.json` ✓，且条目路径（设备路径）与 QEMU guest 的设备同名挂载匹配。实机验证：clangd 日志出现 `Loaded compilation database from /storage/Users/currentUser/workspace/warp-ohos/compile_commands.json`。

## 修改文件

- `crates/gpui_ohos/depend/launch-zed/Cargo.toml` — `qemu` feature 追加 `"zed/qemu"`，打通到 zed。
- `crates/zed/Cargo.toml` — 新增 `qemu = ["language/qemu"]`，转发到 language。
- `crates/language/Cargo.toml` — 新增 `qemu = ["lsp/qemu"]`，转发到 lsp。
- `crates/lsp/Cargo.toml` — 新增 `qemu = []`，使 lsp crate 内可用 `#[cfg(feature = "qemu")]`。
- `crates/lsp/src/lsp.rs` — 两个 map 函数转换版 cfg 改 `all(ohos, not(qemu))`；出站调用点（原 801）包 `#[cfg(not(feature = "qemu"))]`，QEMU 下不调用。
- `crates/lsp/src/input_handler.rs` — 入站调用点分支1 cfg 改 `all(ohos, not(qemu))`（QEMU 下走 `Cow::Borrowed` 不调用 `map_uri_vm_to_device`），分支2 cfg 同步改。
- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agent/src/executor.rs` — `run_mount` 的 `guest_path` 从 `/ws/{n}` 改为 `path.to_string()`（设备路径同名）；删除 `GUEST_WS_PREFIX` 常量；修正 mount_counter 注释。
- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agentd/src/exec.rs` — 新增 `pub(crate) const GUEST_PERSISTENT_HOME = "/sandbox/home"`；`build_command` 里 `cmd.env("HOME", GUEST_PERSISTENT_HOME)` 注入所有 guest 命令。
- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agentd/src/main.rs` — 启动时 `create_dir_all("/sandbox/home")`（先判断不存在才建）。
- `crates/gpui_ohos/depend/ohos-qemu-agent/DESIGN.md` — 6 处文档同步：动态条目 `URI→URI` 同名、第 8/9/9.1 节挂载点=设备同名、检视清单 guest_path 不再按序号分配。
- `warp-ohos/compile_commands.json` — 从 `hap/.bitfun/.deveco/.cxx/compile_commands.json` 复制设备路径版到项目根（手动，非仓库代码改动）。

## 关联

[[ohos-debug-lessons]]
[[ohos-qemu-9p-file-performance]]
[[2026-08-30-clangd-which-not-found]]
