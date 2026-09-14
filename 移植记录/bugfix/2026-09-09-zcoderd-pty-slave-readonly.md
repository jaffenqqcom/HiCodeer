# zcoderd pty slave 只读打开：terminal 会话成功却零回显

## 问题描述

terminal 通过 cmd-client 连 zcoderd（OHOS 本机 HNP 或 QEMU guest 内 gnu 版）开交互 shell，`probe` / `open_remote_shell` 全部成功、`hosted in the guest shell`，但终端**没有任何回显**——既无 prompt 也无任何命令输出。OHOS host 与 QEMU guest 两种后端表现完全一致（同一 pty 代码路径），一度让人误判为 SSH 客户端/网络问题。

## 问题表现

- zcoderd 日志显示会话"全正常"：
  - `ssh: conn N pty_request channel=2 term=xterm-256color 80x24`
  - `ssh: conn N exec_request channel=2 cmd=cd '<dir>' 2>/dev/null; exec sh`
  - `pty: interactive shell started channel=2 pid=NNNN`（shell 进程活着，无 `shell exited` 直到退出）
- terminal 无任何回显；用户敲命令也无反应（输出全丢）。
- VM 上用系统 `ssh -tt` 连 zcoderd 复现同一现象（`echo HELLO` / `pwd` 零输出），隔离了 cmd-client，定位到 zcoderd 服务端。
- 加诊断日志到 pty reader 循环后：整个生命周期**无一条 `master read`、无 `would-block`**——master 读端从未读到 shell 输出，即使 shell 已执行 `pwd`。
- 用 python 对照实验证实：只读打开的 slave 上 shell 写 stdout 失败（master 读到空 `b''`）；`O_RDWR` 打开则正常读到 `HI_FROM_SLAVE`。

## 问题原因

`zcoderd/src/pty.rs` 的 `run_pty_shell` 打开 pty slave 用了 **`File::open(&slave_path)`**，而 `std::fs::File::open` 是 **O_RDONLY（只读）**：

```rust
let slave = match File::open(&slave_path) { ... };
let [child_stdin, child_stdout, child_stderr] = slave_stdio(&slave)?;  // 三个 clone 全是只读 fd
cmd.stdin(Stdio::from(child_stdin));
cmd.stdout(Stdio::from(child_stdout));   // fd1 只读
cmd.stderr(Stdio::from(child_stderr));   // fd2 只读
```

子进程 shell 的 stdin/stdout/stderr 全部指向**只读打开的 slave fd**：
- 读 stdin 正常 → shell 能收到命令、执行（所以 `pty started`、命令到达、进程活着）。
- 写 stdout/stderr（prompt、echo、命令输出）→ 对 O_RDONLY fd 写 → **EBADF** → 全部静默丢失。

于是呈现"会话成功、shell 活着、却零输出"的诡异组合。因 zcoderd 在 OHOS（本机）与 gnu（guest）用同一份 pty 代码，两种后端都无回显。

## 解决方案

slave 必须以 **O_RDWR** 打开，让 shell 的 stdout/stderr 能写进 pty 并被 master 读端读取：

```rust
// Before: File::open == O_RDONLY，子进程写 stdout 全 EBADF
let slave = match File::open(&slave_path) { Ok(f) => f, ... };

// After: slave read-write，shell 输出可达 master
let slave = match std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&slave_path)
{ Ok(f) => f, ... };
```

修复后（VM `ssh -tt` 实证）：`pwd` 正常回显路径、shell 交互转义序列（`[?2004h`）出现、`exit` 正常关闭；真机 OHOS host 终端回显恢复。

**排查要点**：
- "pty 会话建立成功但零回显"优先怀疑 pty slave 打开方式（只读 fd 会让写全部静默 EBADF）。
- 快速隔离 client/server：VM 上跑 gnu zcoderd + 系统 `ssh -tt` 直连，绕开 cmd-client。
- VM 直接跑 resfile 里的 zcoderd 会占用可执行文件，导致后续 `bundle-ohos` cp 报 `Text file busy`——本地验证应拷 `/tmp` 副本再跑。

## 修改文件

- `crates/gpui_ohos/depend/cmd-agent/zcoderd/src/pty.rs` — `run_pty_shell` 打开 pty slave 由 `File::open`（O_RDONLY）改为 `OpenOptions::new().read(true).write(true).open`（O_RDWR），使子进程 shell 写 stdout/stderr 不再 EBADF

与 [[ohos-debug-lessons]] 排查经验相关；前置联调经验见 [[2026-09-07-ohos-cmd-client-watch-send-reject-git-panel-hang-cpu.md]] 同链路。
