# OHOS 终端与 LSP 互抢输入：SSH channel id 跨连接复用导致路由撞车

## 问题描述

zcoderd 的 SSH 服务端把 `russh` 的 `ChannelId` 当成了全局唯一标识，但它是**每条连接各自
从同一低位重新计数**的裸 `u32`。由于命令通道是连接池（pty 与 exec 各占一条连接），
多条连接并存时同一个 channel id 会被反复使用，于是进程级共享的路由表发生撞键。

两个表象完全不同、根因同一个：

- **agent / LSP 子进程收不到 stdin**：pty 的表项截胡了 exec 通道的输入转发；
- **LSP 会话中途掉线**：并发 exec 互相覆盖运行中子进程的 stdin 管道。

## 问题表现

- 交互终端一切正常，但同一时刻通过 exec 通道跑的进程卡死、不响应输入。
- clangd 在回复完 `initialize` 之后立刻死亡，报
  `Transport error: Input/output error`——它的 stdin 管道被另一条连接的同号 channel 顶掉。
- 现象与并发相关：串行执行时正常，一旦终端与 LSP 同时在跑就复现。

## 问题原因

### channel id 是 per-connection 的

`SSH_MSG_CHANNEL_OPEN` 的接收端编号由每条连接自己分配，从 2 开始。zcoder 的 cmd-client
是连接池（`pool.rs` 的 `MIN_IDLE=5` / `MAX_IDLE=16`，pty 与 exec 各 allocate 一条），
因此"连接 A 的 channel 2"和"连接 B 的 channel 2"是两条不同的会话，却在同一个
`HashMap<ChannelId, _>` 里争同一个槽位。

### 截胡发生在输入转发路径上

`sshd.rs` 的 `data()` 先问 pty 表，命中就**不再**往下走 exec 的转发：

```rust
// sshd.rs:125-129
// Interactive pty channels route input to the pty master; everything
// else goes to the running exec child's stdin pipe.
if !crate::pty::forward_input(self.conn_id, channel, data).await {
    exec::forward_stdin(&self.children, channel, data).await;
}
```

于是只要 pty 表里存在一条同号 channel，exec 子进程的 stdin 就永远收不到字节，一直阻塞。

### 另一条隐蔽路径：children 表

即便 pty 表修对了，`children`（channel → 运行中子进程的 stdin）若跨连接共享，后来的命令
仍会在同一个 id 下插入自己的句柄，把前一条会话的管道顶掉。它必须是**每连接私有**的
（`sshd.rs:32-50` 的注释就是这段历史）。

## 解决方案

### 关键点

进程级共享的表一律以 `(conn_id, channel)` 为键；能放进连接对象的状态就不放进全局表。

### 修改

**`crates/gpui_ohos/depend/cmd-agent/zcoderd/src/pty.rs`**

- `MASTERS` 由 `HashMap<ChannelId, _>` 改为 `HashMap<(u64, ChannelId), _>`（`pty.rs:45`），
  注释写明 channel id 是 per-connection 的；
- `master_for` / `resize` / `forward_input` / `run_pty_shell` 全部带上 `conn_id`
  （`:129`、`:135`、`:149`、`:181`），注册与清理也改成复合键（`:285-288`、`:305-308`、`:352-355`）；
- 保留原语义：`forward_input` 在表中未命中时返回 `false`，让调用方回落到 exec 的 stdin 转发。

**`crates/gpui_ohos/depend/cmd-agent/zcoderd/src/sshd.rs`**

- `ConnectionHandler` 新增 `conn_id: u64`（`:42`），取 `NEXT_CONNECTION_ID.fetch_add(1, Relaxed)`（`:55`）；
- `children` 与 `ptys` 都是**实例字段**（`:46`、`:49`），每个被接受的连接新建一个 handler
  （`SshServer::new_connection`），从结构上杜绝跨连接共享；
- `data()` 把 `self.conn_id` 传给 `pty::forward_input`（`:127`）；
- `window_change_request` 传 `conn_id` 给 `pty::resize`（`:183`）；
- `exec_request` 取到本次 `conn_id` 后交给 pty 中继任务（`:207-210`）；
- `pty_request` 只写本连接的 `ptys`（`:162`）。

### 验证

- `./script/bundle-ohos` 全量编译通过，装机成功。
- 并发场景（终端 + LSP 同时活动）的实机回归验证**尚未做**，属未验证项。

### 走过的弯路（供后来者避免）

- 第一轮只盯 exec 侧，以为是"子进程 stdin 管道没接好"，反复查 `exec::forward_stdin`。
  真正的元凶是**进程级 pty 表跨连接撞键**——输入在到达 exec 之前就被 pty 分支截走了。
- 修 pty 表时差点漏掉 `children`：两张表的撞键机制相同，只改一张仍会复现 LSP 掉线。
- 判断"是否需要每连接私有"的判据很简单：**key 是不是对方自己分配的**。对方分配的编号
  就不能当全局唯一键。

## 修改文件

- `crates/gpui_ohos/depend/cmd-agent/zcoderd/src/pty.rs` — `MASTERS` 改复合键，相关函数
  签名补 `conn_id`
- `crates/gpui_ohos/depend/cmd-agent/zcoderd/src/sshd.rs` — `ConnectionHandler` 加
  `conn_id`，`children`/`ptys` 明确为每连接私有，各回调透传 `conn_id`

## 关联

- 同文件相邻问题：[[2026-09-09-zcoderd-pty-slave-readonly.md]]
- 命令通道整体结构：[[2026-08-24-ohos-cmd-agent-architecture.md]]

[[ohos-debug-lessons]]
