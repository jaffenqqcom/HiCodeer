# cmd-client SessionState::set_exit 用 tokio watch send，无 receiver 时拒收退出值，git 面板空白 + CPU 100% 自旋

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植）在 git hnp 移出 HAP、改为调用系统 git 后，git 面板长期空白；同时应用"空闲"状态 CPU 占用飙到一个完整核心（~100% 单核）。命令执行走 cmd-agent 桥：应用侧 `cmd-client`（smol）经 socketpair ↔ zcoderd（guest 侧执行）。git 数据本身没丢，是应用侧 `Child::output` **永远收不到子进程退出状态**导致的挂死连带。

## 问题表现

- git 面板一直空白；repository 初始化永不完成，后续 git 命令全部不再发出。
- 卡点日志（全链路）：`stdout read done` → `stderr read 0 bytes` → `wait_exit start` 之后**永久沉默**。
- 空闲 CPU 实测 **500 ticks/5s ≈ 100% 单核**，且永不回落。
- 复现：应用启动即自动发 git 命令，只有第 1 条能完成（`status=0`），从第 2 条起全部挂死。
- 快命令才触发：慢命令（如 `git status` 2.7s）执行期间 `wait_exit_async` 已完成订阅，不命中。

## 问题原因

**tokio `watch::Sender::send()` 在通道无任何存活 receiver 时返回 `Err` 且值不被存储（拒收）**——与 `set_exit` 原注释"with no subscribed receivers this is a no-op"的假设相反。而 `SessionState::new` 里 `watch::channel(None)` 的初始 receiver 被直接丢弃，receiver 数清零。

具体挂死链：

```
util::command::Child::output（ohos.rs）
  └─ stdout.read_to_end → stderr.read_to_end → executor.wait_exit_async().await
       └─ wait_exit_async: let mut exit_rx = state.tx.subscribe()
            └─ *exit_rx.borrow() 为 None → exit_rx.changed().await   ← 永久等待点

时序竞态（快命令）：
pump 收到 ExitStatus → state.set_exit(Some(0))
  ├─ exit Mutex 写入 Some(Some(0)) ✓（互斥量有值）
  └─ self.tx.send(value) → 此刻 receiver 数=0 → 拒收，值丢失 ✗（watch 无值）
随后 wait_exit_async 才 subscribe → borrow()=None → changed().await 永久挂起
     → Child::output 挂死 → repository 初始化挂死 → git 面板空白
```

CPU 自旋是挂死的**连带效应**：堆积的阻塞任务 + panel 层重试 + fs 事件→git 刷新循环。修复根因后空闲 CPU 从 500 ticks/5s 降到 ~1%，证实不存在独立的反应器自旋问题。

排查中用判别实验逐一排除的假说：

1. **async-io(smol) 反应器在 OHOS 上自旋且不派发** —— 裸 waker 计数探针 + `smol::block_on` socketpair 健康探针 24/24 全绿，反应器派发正常，用法无误；
2. **dispatcher `std::thread::spawn` 失败丢唤醒** —— DIAG_SPAWN_FAILURES=0；
3. **双 SessionState 实例**（两端各建一份）—— `Arc::as_ptr` 指针比对全部同一实例；
4. **fd 层 EOF 未到达** —— dup + 独立 epoll 探针确认三个 fd EOF 全部到达。

最终由 watchdog 旁观者线程直接暴露矛盾：每 500ms 同时读 `exit` Mutex 和 `tx.borrow()`——**Mutex 恒为 `Some(Some(0))` 而 watch 恒为 `None`**。临时改 `send_replace` 后 5 分钟 37 条命令全部 `status=0`（修复前仅 1 条），实锤。

## 解决方案

`set_exit` 改用 `send_replace`：**无条件存储最新值**，不受 receiver 是否存活影响；迟到的订阅者经 `borrow()` 立即看到终态，`changed().await` 与初始 `borrow()` 双路径都闭合。

核心改动（cmd-client/src/executor.rs，仅一行 + 注释）：

```rust
// Store the terminal value unconditionally: `send()` rejects the value
// when no receiver is alive yet (the initial receiver was dropped), so
// a fast-exiting child would lose its exit status and a later
// subscriber of `wait_exit_async` would block forever. `send_replace`
// always stores, so late waiters observe the value via `borrow()`.
self.tx.send_replace(value);   // 原: let _ = self.tx.send(value);
```

验证（修复版装机后，应用启动自动发 git 命令）：

```
21:25:42.914 Child::output: session_id=182 status=ExitStatus(unix_wait_status(0)), stdout_bytes=0
21:25:43.121 Child::output: session_id=183 status=ExitStatus(unix_wait_status(0))
21:25:43.409 Child::output: session_id=184 status=ExitStatus(unix_wait_status(0))
...（session 182~187 连续全部完成，每条 ~200ms，git 面板数据流恢复）
```

CPU 复测（线程级 /proc 差分采样）：

| 状态 | 空闲 CPU（单核基准） |
|---|---|
| 修复前 | 500 ticks/5s = 100% |
| 修复后 5s 窗口 | 4 ticks = <1% |
| 修复后 15s 窗口 | 16/1500 ticks ≈ 1% |

启动初期主线程有一次 ~89% 的短促突发（top 瞬时值），为启动 git 仓库扫描的真实工作量（连续 `git cat-file --batch`），扫完即停，非自旋。

## 修改文件

- crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs — **核心修复（一行）**：`set_exit` 中 `let _ = self.tx.send(value);` → `self.tx.send_replace(value);`，附拒收语义注释。
- 排查期间的全部 `[diag]` 探针（executor.rs fd 探针/健康探针/watchdog/wait_exit 细粒度日志、dispatcher.rs 派发计数、ohos.rs/launch_app.rs 日志、Cargo.toml libc 依赖）**已全部回退**，本修复版为干净代码 + 一行修复。

## 相关教训

[[ohos-debug-lessons]]：tokio `watch::Sender::send()` 的拒收语义（无 receiver → Err 且不存储）极易被"best-effort broadcast 无害"的注释掩盖；**凡是"先存 Mutex、再广播 watch"的双通道状态，广播失败必须显式处理或改用 `send_replace`**。另外本例症状（面板空白 + CPU 自旋）出现在完全不同的上游（git panel / 反应器），定位时先做判别实验排除假说（裸 waker 探针、指针比对、watchdog 旁观者）比直接读代码更有效。
