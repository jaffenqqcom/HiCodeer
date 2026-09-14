# OHOS 沙箱禁止 exec 任意程序：外部进程统一改经 `util::command` 路由

## 问题描述

OHOS 沙箱只允许执行应用自带的白名单程序（`/bin/sh` 等），Zed 里原先直接使用
`std::process::Command` / `smol::process::Command` 起子进程的路径全部失败，报
`127 / inaccessible or not found`。所有外部进程必须统一改走 `util::command`：
设备上已快照的 HNP 工具（git/ssh/curl）本地 fork，其余一律发 `ExecSpec` 交给 zcoderd。

受影响的调用面比想象中宽：MCP 上下文服务、调试适配器（DAP）、任务终端、Jupyter 内核
——凡是走到 `Child::spawn` 的地方。

## 问题表现

- 上述功能在 OHOS 上启动即失败，错误码 127；在桌面平台一切正常。
- 由于共享代码里多处各自 `Command::new(...).spawn()`，同样的故障在不同功能上重复出现。

## 问题原因

### 一、产品约束

鸿蒙沙箱禁止 exec 任意程序，这是平台规则，没有绕过的余地，只能改路由。

### 二、类型是编译期耦合，改签名必须动调用点

平台的 `Child::spawn(command, stdin, stdout, stderr)` 里，stdio 参数在两侧是**不同类型**：
OHOS 是路由器自己的 `Stdio`，其余平台是 `std::process::Stdio`。签名一改，所有调用点
必须同步，否则编译不过。

### 三、"零调用点改动"在 Rust 里做不到（验证过）

`std::process::Stdio` 是**不透明类型**：标准库只提供 `piped()` / `null()` / `inherit()`
和 `From<File>`，**没有任何接口能把"它是哪一种"读回来**。而路由侧必须区分三者——
`crates/dap/src/transport.rs:526` 与 `crates/terminal/src/terminal.rs:3249` 都拿 `null()`
当 stdin，不能一律按"接管管道"处理。所以只能在调用点传一个可识别的类型。

## 解决方案

### 关键点

把平台差异**收敛到一个分流点**，让其余调用点退化成"换了一个 import 路径"——
在 Linux/Windows/macOS 上看就是一句等价的普通重构。共享代码里的
`#[cfg(target_env = "ohos")]` 目标值为 **0 处**。

### 修改

**`crates/util/src/process.rs`（唯一的 cfg 分流点）**

```rust
#[cfg(target_env = "ohos")]
pub use crate::command::{Child, Stdio};
#[cfg(not(target_env = "ohos"))]
pub use std::process::Stdio;
```

原有的 `Child` 包装体（`smol::process::Child` + Windows job object）整体加
`#[cfg(not(target_env = "ohos"))]`；另外补一个 `impl Debug for Child`。后者省不掉：
OHOS 侧的路由 `Child` 不暴露 `Deref`，而 `crates/repl/src/kernels/native_kernel.rs`
的 `Debug` 实现需要 `Child: Debug`；cfg 属性没法挂在方法链中间，硬加反而更多行。
该文件的改动**全部是"加 cfg 门 + 加注释"**，没有改写既有语句，其他平台运行期行为零变化。

**`crates/util/src/command/ohos.rs`（OHOS 专属文件，零跨平台影响）**

- `Command::from_std`（`:339`）：从已构造好的 `std::process::Command` 复制 program、args、
  current_dir 与逐变量 env 覆盖。`env_clear` **有意不恢复** —— std 没有 getter，
  "被清空"与"未设置"无法区分，猜错会把调用方的环境静默泄露给子进程。
- `Child::spawn(std_command, stdin, stdout, stderr)`（`:672`）：先 `from_std` 再挂 stdio，
  最后走 `Command::spawn()`，路由规则与既有 `Command::spawn` 完全一致。
- 三个流类型由 `Box<dyn … + Send>` 放宽为 `+ Send + Sync`（`:535-545`、`:560-566`）：
  OHOS 上 `util::process::Child` 别名到这个类型，其消费方（如
  `context_server::Transport`）要求 `Send + Sync`；底层具体的 smol `Async<..>` 本来就满足。

**5 个调用点（全部只改 import）**

| 文件 | 改法 |
|---|---|
| `crates/agent_servers/src/acp.rs` | `use util::process::{Child, Stdio};` |
| `crates/context_server/src/transport/stdio_transport.rs` | 同上 |
| `crates/dap/src/transport.rs` | 同上 |
| `crates/terminal/src/terminal.rs` | `use util::process::Stdio;` |
| `crates/repl/src/kernels/native_kernel.rs` | `use util::process::Stdio;` + Debug 里 `&*self.process` → `&self.process` |

代价说明：这 5 处原本写的是成对的 `#[cfg]` 分流，改用**一行平台无关 import** 的前提是
`util::process::Stdio` 在两侧都可用（OHOS 是路由 `Stdio`，其余就是 `std::process::Stdio`
的 re-export）。已逐个核验：这些文件里 `Stdio` **只**用作 `Child::spawn` 的参数，
没有 `Stdio::from(文件)`、也没喂给 std 的命令接口，换过去行为完全等价。

覆盖面核验：全仓 `Child::spawn` 共 **6 处**（另有 1 处测试），这 6 处全改 —— 既没有漏，
也没有扩到无关的地方。

### 验证

- `./script/bundle-ohos` 全量编译通过，装机成功。
- 各消费功能（MCP / DAP / 任务终端 / Jupyter）的实机逐项验证**尚未做**，属未验证项。

### 走过的弯路（供后来者避免）

- **第一版把噪音带进了共享代码**：在 5 个调用点各写 3 行 `#[cfg]` 分流，等于往共享代码里
  塞了 10 处平台宏。被指出"对原生改动越小越好"后收敛为一行 import —— 因为那个别名
  两个平台都可用，本来就不需要分流。
- **考虑过更激进的做法**：只改 `acp.rs` 一处，MCP 上下文服务 / 调试适配器 / 任务终端 /
  Jupyter 内核留着以后再说。评估结论是不做：MCP 与任务终端在 OHOS 上是真会用的功能，
  只有 DAP 与 Jupyter 大概率用不上，为省 4 个 import 留 4 个雷不划算。
- 判别"某处改动是否必要"的口径：先看 `Child::spawn` 的调用是否真的存在（类型耦合是否
  真被触发），再看能否用 re-export 消掉 cfg。两问都过了才动手。

## 修改文件

- `crates/util/src/process.rs` — 唯一分流点；`Child`/`Stdio` 平台别名，`Child` 包装体与
  测试模块加 cfg 门，新增 `impl Debug for Child`
- `crates/util/src/command/ohos.rs` — 新增 `Command::from_std`、`Child::spawn`；
  流类型放宽为 `Send + Sync`
- `crates/agent_servers/src/acp.rs`、`crates/context_server/src/transport/stdio_transport.rs`、
  `crates/dap/src/transport.rs`、`crates/terminal/src/terminal.rs`、
  `crates/repl/src/kernels/native_kernel.rs` — 调用点 import 收敛（+ native_kernel 的
  Debug 借用调整）

## 关联

- 命令通道整体结构：[[2026-08-24-ohos-cmd-agent-architecture.md]]
- 同一通道上的输入路由问题：[[2026-09-11-ohos-pty-channel-key-collision.md]]

[[ohos-debug-lessons]]
