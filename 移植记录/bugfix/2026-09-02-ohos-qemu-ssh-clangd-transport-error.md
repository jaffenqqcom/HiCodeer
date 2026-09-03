# clangd LSP 反复 "Transport error" 的三连根因（echo 劫持 fd1 / EOF 提前 break / stdin 跨连接覆盖）

## 问题描述

QEMU guest 后端（嵌入式 SSH，host 侧 `qemu-ssh-agent`，guest 侧 `ssh-agentd`，基于 russh 0.55）下，clangd LSP 启动后短暂存活、能回复 initialize/codeAction/hover，随后即死，表现为 "server shut down" / "Transport error: Input/output error"。排查发现是三个独立缺陷叠加，且修复过程中"假修复"不断：先有命令的 pid 记录前缀让 clangd 的 stdout 落进 pid 文件、再有 host pump 提前结束丢掉 exit status、最后是子进程 stdin 句柄被并发连接互相覆盖。

## 问题表现

- clangd 初始化完成后约几秒即退出，host 报 `Transport error: Input/output error`；重试多次仍失败。
- guest 侧检查运行中的 clangd 子进程 fds：**fd 1（stdout）指向 pid 文件而非管道**，stdout 管道已 EOF。
- 另一批症状：**所有短命令（which、git status 等）exit_code=None**、`output.status()` 判失败，即使 guest 里命令成功执行。
- 高并发时：运行中的 clangd 的 stdin 被静默关闭（guest 日志 `ChildStdin dropped`），clangd 读到 stdin EOF 而死。
- 复现：zcoder 打开含 C++ 文件的工作区自动拉起 clangd，或执行任意 git 命令。

## 问题原因

三个独立根因，按修复顺序：

### 根因 1：pid 记录前缀 `echo $$ > file` 让 busybox ash 劫持 fd1

host 侧每个命令前缀 `echo $$ > '/sandbox/ssh/<sid>.pid'`，意图记录 exec 后进程组的 pid，供 host `kill -KILL -<pid>`。busybox ash 对**内置 echo** 的重定向在同一 shell 进程内执行：`echo $$ > pidfile` 把当前 shell 的 fd1 重定向到 pid 文件后**没有还原**，随后的 `exec clangd` 继承了这个 fd1 → **clangd 的 stdout 全部写进 pid 文件**，其 stdout 管道对端立即 EOF。host pump 见 stdout EOF 后按旧逻辑关 stdin → clangd 读 stdin 得到 EIO → Transport error。

先尝试 `exec 8>&1; ...; exec 1>&8` 保存/恢复 fd1，实测无效：busybox ash 在 `&&` 链里的 fd 拷贝语义仍把 fd1 留在 pid 文件上。最终改用**管道** `echo $$ | cat > pidfile`：管道两端（echo、cat）都进子进程，父 shell 的 fd1 从头到尾未被重定向。

### 根因 2：host pump 收到 EOF 即 break，丢掉随后到达的 exit-status

russh 客户端 pump 原在 `ChannelMsg::Eof` 处 break。但 guest 端命令结束的顺序是：child stdout 关闭 → 发 EOF → child 被 reap → 再发 exit-status → close。pump 在 EOF 处就 break，导致每个快速命令（which/git）都以 exit_code=None 收场、`output.status()` 失败。修复：EOF 只代表"没有更多输出"，**继续等待 ExitStatus/Close/None** 才结束。

### 根因 3：guest 端 children map 跨连接共享，ChannelId 复用互相覆盖

russh 的 `ChannelId` 是裸 `u32`，**每条新连接都从低值重新计数**（每个 exec 命令独立一条 SSH 连接，通道号常为 2）。guest 端最初把"channel → 运行中子进程 stdin"的 map 设计成跨连接共享（一个 `Arc<Mutex>`），于是：连接 A 的 clangd 在 channel 2 插入句柄 → 连接 B 的新命令也在 channel 2 插入 → 覆盖并 **drop 掉 clangd 的 stdin 管道** → clangd 读 EOF → Transport error。这一条解释了为什么 clangd 常在回复 initialize 后死亡（initialize 完成时刻恰有别的命令落地）。

## 解决方案

三处修复（guest/host 两侧）：

1. **qemu-ssh-agent/src/command.rs**（host）：pid 前缀从 `echo $$ > file` 改为管道 `echo $$ | cat > file`，命令前缀变：

```sh
echo $$ | cat > '/sandbox/ssh/<sid>.pid' && ... && exec clangd
```

两端都在子进程执行，父 shell fd1 干净。这也让 `kill -KILL -$(cat <pidfile>)` 记录的 pid 依然正确（cat 在管道里 `$$` 是父 shell pid，即 exec 后进程组组长）。

2. **qemu-ssh-agent/src/executor.rs**（host pump）：`ChannelMsg::Eof` 分支不再 break，改为打日志继续循环，直到收到 `ExitStatus`/`Close`/`None`。

3. **qemu-ssh-agentd 按连接隔离**（guest）：把 handler 从共享实例改为**每连接一个 `ConnectionHandler`**，各自持有私有 children map 与自增 `conn_id`（日志可区分并发连接）；accept 循环用 `server.new_connection()` 构造（russh 0.55 `run_stream` 需要具体 Handler 而非模板，故用工厂方法而非 clone 共享）。guest exec.rs 的 spawn_command 拆到独立任务、bridge 直到 child 真正退出才关通道。

验证：clangd 能稳定服务 LSP 请求不再 "server shut down"；短命令全部拿到正确 exit code；git/clangd 命令并发不再互相踩 stdin。

## 修改文件

- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agent/src/command.rs — pid 前缀改管道 `echo $$ | cat > pidfile`，消除 busybox ash 内置 echo 对 fd1 的劫持（含注释解释为何 fd8 save/restore 无效）。
- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agent/src/executor.rs — pump 对 `ChannelMsg::Eof` 不再 break，继续等 ExitStatus/Close；stdin 侧 EOF 时发通道 EOF。
- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agentd/src/server.rs — 重构为每连接 `ConnectionHandler`（私有 children map + `conn_id`）；实现 `channel_open_session` 返回 true、`data` 转发 stdin、`exec_request` 回 success 后 spawn。
- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agentd/src/main.rs — accept 循环由共享 handler.clone() 改为 `server.new_connection()` 按连接构造 handler。
- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agentd/src/exec.rs — spawn_command 在独立任务中桥接 stdio、直到 `child.wait()` 完成才关通道并上报 exit；诊断日志（子进程 fd 检查、child exit）。

## 相关教训

[[ohos-debug-lessons]]：busybox ash 的内置命令与重定向语义和 bash 不同（内置 echo 重定向发生在当前 shell 进程）；russh `ChannelId` 会跨连接复用，任何"按 channel 索引的共享状态"都必须按连接隔离，否则并发 exec 互相踩踏。
