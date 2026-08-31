# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> ⚠️ **⚠️ 最高优先级规则
> **文件名和路径名都不含 "ohos" 关键字的文件，不可以自行修改，必须询问征求意见！代码里禁止使用魔鬼数字**
>
> **文件名和路径名都不含 "ohos" 关键字的文件，不可以自行修改，必须询问征求意见！代码里禁止使用魔鬼数字**
>
> **文件名和路径名都不含 "ohos" 关键字的文件，不可以自行修改，必须询问征求意见！代码里禁止使用魔鬼数字**
>
> 例外：路径中任意一级包含 "ohos"（如 `gpui_ohos`）可直接修改。

> **禁止执行删除目录的指令。删除目录必须获得明确的授权。** 包括但不限于 `rm -rf`、`cargo clean`、构建脚本的 `--clean` 参数等。

> **对全路径or文件名不带“ohos“的，或者不是在hap目录下的代码进行修改，必须使用target_env = "ohos"包裹。不要允许在原有的代码里插入是大段的代码，大段代表必须封装成函数。

> **对全路径or文件名带“ohos“的，或者在hap目录下的代码进行修改，不使用target_env = "ohos"包裹。

> **功能修改必须先讨论方案，获得授权后才能改代码。** 包括但不限于新增功能、修改功能、加日志、加参数等任何代码改动。必须先向用户说明原因、影响范围、修改方案，用户明确同意后再动手。

> **不准执行卸载程序的指令。


## 快速开始

### 开发环境

> **开发环境是一个HarmonyOS的PC，其上还跑了一个虚拟机，虚拟机是OpenEuler操作系统。claude（你）和ssh server是跑在虚拟机上，被调试程序是跑在HarmonyOS上

> **HarmonyOS和虚拟机通过目录挂载的方式共享这同一个磁盘，文件在在HarmonyOS的路径是/storage/Users/currentUser/，在虚拟机上的路径是/mnt/linux_share

### 构建（HAP 打包）

```bash
# 完整构建：Rust 交叉编译 + HAP 打包（debug 版，保留调试符号）
script/bundle-ohos

> **resfile 路径注意**：hvigor 打包 resfile 从 `hap/entry/src/main/resources/resfile/` 读取（**不是** `src/main/resfile/`）。`bundle-ohos` 的 `SERVER_RESFILE_DIR` 必须指向 `src/main/resources/resfile/`，放错目录会静默把旧缓存的 server 打进 HAP（曾导致 daemon 部署 13.8MB 旧 server，spawn 全部 EBADF 失败）。
### 调试

hdc（HarmonyOS Device Connector，`/usr/bin/hdc`，版本 3.2.0b）用于远程连接和调试鸿蒙设备。

```bash
# 远程连接设备（使用 tconn，非标准 connect）
hdc tconn <IP>:<端口>
hdc tconn 192.168.3.57:37581

# 查看连接状态
hdc list targets -v

# 查看日志（按 tag 和级别过滤）
hdc hilog -t zcoder -l I       # 只看 zcoder 的 Info 日志
hdc hilog -t zcoder -l E       # 只看 zcoder 的 Error 日志
hdc hilog -l I | grep "关键词"  # 多关键词过滤
timeout 5 hdc hilog          # 带超时获取（非阻塞）

# 抓日志强制流程（必须严格按此顺序，禁止违规）：
# 1. 先清空日志：hdc shell hilog -r
# 2. 再启动日志抓取，重定向到一个文件：nohup hdc hilog > /tmp/xxx.log 2>&1 &
# 3. 最后再启动 APP：hdc shell aa start -b com.zcoder.studio -a EntryAbility
# 顺序不可颠倒、不可跳过。中途找/杀抓取进程用 pgrep -f "hdc hilog" + kill <PID>，
# 禁止 pkill -f "hdc hilog"（会误杀含该字符串的自身 shell 导致日志文件为空）。
```

以下为 HarmonyOS NEXT 移植专项规则，必须严格遵守。

### 1.1.8 迭代器格式化

- 不要将 `Itertools::format` 的结果直接传给日志宏。使用 `iter.join(", ")` 等生成可复用的 String

### 1.1.9 注释管理

- **代码注释（`//`、`///`、`/* */`）必须全部使用英文，禁止使用中文注释**
- 不要删除无关变更中的现有注释
- 只有在逻辑变更时才移除或修改注释

### 1.1.10 鸿蒙新增代码规范

- **所有新增的鸿蒙相关代码必须遵循以上所有规范**
- 鸿蒙特定代码放在 `#[cfg(target_env = "ohos")]` 条件编译块中（注：鸿蒙的 target_os = "linux"，通过 target_env = "ohos" 区分）
- 鸿蒙平台模块命名为 `ohos`，遵循 `mac`、`windows`、`wasm` 的命名惯例支
- `#[cfg(target_env = "ohos")]` 仅在代码无法编译（如依赖鸿蒙特有 API）时使用

### 1.1.11 日志接口规范

1. **Rust 侧**：统一使用 `log::xxx!()`（如 `log::info!()`、`log::error!()`），**禁止直接调用 HiLog C API**
2. **ArkTS 侧**（HAP 目录）：统一使用 `hilog.xxx()` 从 `@kit.PerformanceAnalysisKit`，**禁止使用 `console.log()`**
3. **格式化参数**：必须使用 `%{public}s` / `%{private}d` 等标准格式，禁止字符串拼接
4. **domain**：固定为 `0x0001`（Warp 应用领域），新增模块需在文档中注册
5. **tag**：使用当前模块的简短英文标识，最长 31 字节

### 1.1.12 cmd-agent 与 LSP 运行机制（OHOS）

> OHOS 沙箱禁止 spawn 子进程，git/LSP/终端等命令经 cmd-agent 转发到 OpenEuler VM 执行。代码在 `crates/gpui_ohos/depend/ohos-openeuler-agent/`。crate 逻辑名与物理名已彻底改名：`cmd-agent`（原 cmd-agent-client，lib `cmd_agent`）、`cmd-agentd`（原 cmd-agent-server）、`cmd-agent-protocol`、`cmd-agent-linker`——目录名、编译产物、VM 部署程序名均随改名（`cmd-agent/`、`cmd-agentd/`，产物 `cmd-agent`、`cmd-agentd`）。

- 三段式：业务代码 → zcoder 进程内 daemon（3 线程，双 executor 隔离）→ VM 上 cmd-agent-server。Client↔Daemon 走 unix socket，Daemon↔Server 走 TCP
- **路径映射**（cmd-agent-server/src/spawn.rs，判断路径时必须心算）：
  - Rule A：设备沙箱 `/data/storage/el2/base/haps/entry/files/...` → VM `/home/user/cmd-agent/...`
  - Rule B：设备工作区 `/storage/Users/currentUser/...` → VM `/mnt/linux_share/...`
- 下载落盘以 `data_dir()` 为根（OHOS 设备沙箱），**全部走 http 直连落设备**（GitHub 二进制曾改用 util::command+curl 落 VM，因引入"沙箱→VM 同步"已回退）：`languages/`（LSP）、`extensions/`、`external_agents/`、`copilot/`、`prettier/`、`node/`、`debug_adapters/`，经同步引擎镜像到 VM `/home/user/cmd-agent/zed/...`
- 沙箱→VM 同步：zcoder 内独立后台线程用 notify 监听下载目录（只处理写入完成事件，含 rename），经 cmd-agent 协议 FileSync 消息推 VM（`FileContent` 写 `.ing` → `FileRename` 原子改名，保证 binary 完整）；**VM 侧 spawn 时 binary 缺失且属同步目录 → 每 50ms 轮询查 `binary`/`binary.ing`：出现过 `.ing` 等 `.ing`→`binary` 最多 30s（大文件），从未出现 `.ing` 最多等 1000ms，超时失败**；不属于同步目录立即失败；**cmd-agent-server 启动时扫描同步目录删除残留 `*.ing` 半成品**（异常退出遗留）
- LSP 安装：GitHub 二进制 http 直连落设备沙箱；npm 包经 `npm install`（util::command，VM 侧执行）装到 `<server_dir>/node_modules`；gopls 走 PATH 检测启动
- 已修的路径映射坑：`--flag=<path>` 等号内联参数必须拆分映射；映射后 cwd 在 VM 不存在时 spawn 前自动 `create_dir_all`
- **硬约束**：cmd-agent client 零线程，绝不在调用线程 `block_on`（GPUI 主线程嵌套 async-io reactor 会死锁）