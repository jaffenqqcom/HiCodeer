# QEMU guest ssh-agentd 未把 russh 通道 EOF 转成子进程 stdin 关闭，git 面板 changes 永不显示

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植）的 git 面板长期显示空白、固定 "No changes to commit"，但 `git status` 实际返回 200+ tracked 改动。命令执行走 QEMU guest 的嵌入式 SSH server（`ssh-agentd`，基于 russh 0.55 内嵌实现）。问题只在 guest 后端出现，git 数据本身没有丢，是面板**永远收不到仓库快照**导致的显示层空态。

## 问题表现

- git 面板一直空白，显示 "No changes to commit"；改动文件列表从不出现。
- 加了 `[diag]` 探针后确认（全日志）：`compute_snapshot`（git_store.rs）每轮都在 `git status OK entries=229 untracked=29 tracked_changed=200` 之后**卡死**，"reached final repository update" 标记一次都没出现过 → `StatusesChanged` 事件从不 emit。
- `merge_details::update` 内的 `revparse_batch`（git_store.rs:6066 附近）是卡点：**`git rev-parse` 的命令从未到达 guest**（guest exec 日志 0 次 rev-parse）。
- 一轮 compute_snapshot 在 guest 上的 git 命令全部正常 exit code=0（status 2.7s、三条 diff --numstat 各 7.6s），唯独 `revparse_batch` 挂死。
- 复现：打开任意 git 仓库工作区（warp-ohos、zcoder 均触发），等待 >5 分钟面板仍空。

## 问题原因

**russh 是无主见的嵌入式 SSH 协议引擎：它只把 SSH 线上消息翻译成 `Handler` trait 回调，通道↔子进程 stdio 的桥接完全由使用者负责**（OpenSSH 这类完整 sshd 把这一层白送了：客户端 EOF → 自动关子进程 stdin，客户端 close → 自动终止子进程）。我们当时只桥接了 stdin 转发（`data`）和命令执行（`exec_request`），**漏掉了 EOF → 关 stdin 这个方向**。

具体卡死链：

```
git_store::compute_snapshot
  └─ MergeDetails::update
       └─ backend.revparse_batch([MERGE_HEAD, CHERRY_PICK_HEAD, ...])
            └─ git cat-file --batch-check=%(objectname)   ← 交互式命令
                 │  向 stdin 逐行写 5 个 ref 名，然后关闭 stdin，等它输出 5 行 sha/missing 后退出
                 │
                 ├─ guest ssh-agentd: 收到客户端写入的 63 字节 stdin ✓（forwarded）
                 ├─ guest ssh-agentd: 收到客户端 EOF（ChannelMsg::Eof）
                 │     但 Handler 没实现 channel_eof → EOF 没传播到子进程 stdin
                 └─ cat-file 永远等 stdin 更多输入 → 进程永不退出
                      → child.wait() 永不返回
                      → process.output() 永不完成
                      → revparse_batch 挂
                      → compute_snapshot 挂（background merge task 永不 DONE）
                      → StatusesChanged 永不 emit
                      → git 面板永远空白 "No changes to commit"
```

关键背景：`git cat-file --batch-check` 是**行式交互命令**，必须靠 stdin EOF 才会退出。桌面端本地 git 直接由 OS 管道交付 EOF，天然正常；嵌入 SSH 后 EOF 语义需要自行从 SSH 通道搬到子进程管道，这一环缺失即命中。

排查中先排除了的假说：guest git 命令 fd 耗尽（日志偶现 `No file descriptors available`，但这轮已无）、diff --numstat 慢 IO（这次 3 秒完成，非死锁）、`background_spawn` 线程池阻塞（探针显示 merge task 实际 STARTED）。

## 解决方案

在 guest `ssh-agentd` 的 `ConnectionHandler` 里实现 russh `Handler::channel_eof`：收到客户端 EOF 时，从该连接的 children map 移除对应 channel 的 `ChildHandle`。移除即 drop 其 `stdin`（tokio `ChildStdin` 写端）→ 子进程读到 EOF → 交互命令正常退出。

核心改动（qemu-ssh-agentd/src/server.rs，原 `impl server::Handler` 补一个方法）：

```rust
async fn channel_eof(&mut self, channel: ChannelId, _session: &mut Session)
    -> Result<(), Self::Error> {
    log::info!("ssh: conn {} channel {channel} client eof; closing child stdin", self.conn_id);
    self.children.lock().await.remove(&channel);   // drop ChildHandle → child.stdin 关闭 → cat-file 收到 EOF 退出
    Ok(())
}
```

随后按同一思路补了同族缺口 `channel_close`（用户排查 russh 全部回调后要求补齐）：close 语义比 eof 强——客户端明确放弃该通道，应终止常驻子进程而非只关 stdin。为此 `ChildHandle` 增加 `pid` 字段，新增 `exec::kill_child`（`kill -KILL -<pid>` 收割进程组）。

验证（guest 探针日志，修复后 compute_snapshot 首次完整走通）：

```
22:42:26.423 ssh-agentd: conn 15 channel 2 client eof; closing child stdin   ← 新回调触发
22:42:26.575 merge_details::update: revparse_batch done                       ← cat-file 终于退出
22:42:26.576 compute_snapshot: merge_details bg task DONE
22:42:26.578 compute_snapshot: reached final repository update                ← 修复前从未出现
```

修复前该轮卡了 11 小时都不走完；修复后 3 秒内完成。revparse_batch 从"挂死"变 200ms。

## 修改文件

- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agentd/src/server.rs — **核心修复**：实现 `Handler::channel_eof`（客户端 EOF → 移除该 channel 的 ChildHandle，关闭子进程 stdin）；后续加固 `channel_close`（客户端关通道 → `exec::kill_child` 终止并收割子进程）。
- crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agentd/src/exec.rs — `ChildHandle` 增加 `pid` 字段（spawn 时已有 `child.id()`，此前仅用于日志）；新增 `pub async fn kill_child`（`channel_close` 配套：移除句柄 + `kill -KILL -<pid>` 杀进程组）。
- crates/project/src/git_store.rs — 仅排查用 `[diag]` 探针（join3 resolved / merge_details bg task STARTED·DONE / merge_message / revparse done / reached final），非修复代码；探针属临时诊断，后续可清理。

## 相关教训

[[ohos-debug-lessons]]：嵌入式 SSH/命令桥接层要完整实现 channel 生命周期（EOF、Close、ExitStatus、Signal）到子进程 stdio 的映射，缺一个方向就会让某类命令（行式交互命令如 `git cat-file --batch-check`）永久挂起，且症状会出现在完全不同的上游（git 面板空白而非 SSH 层报错）。
