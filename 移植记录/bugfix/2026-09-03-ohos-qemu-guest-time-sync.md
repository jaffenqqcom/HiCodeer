# zcoder QEMU guest 系统时间停在 1970：经 ssh-agent 一次性同步 host 墙钟

## 问题描述 (Problem Description)

zcoder 内嵌 QEMU 的 guest（busybox initramfs 小系统）里，系统时钟永远停在 `1970-01-01T00:00:00`，不随 host（OHOS 设备）走。这会导致 TLS 证书校验失败（rust-analyzer/clangd 等 LSP 经 guest 内 curl 下载元数据）、文件时间戳/缓存错乱、`make` 的增量判断失真等一连串问题。本记录覆盖从"为什么时间不对"到"最终实现并修通编译"的完整过程，重点是**如何利用现有 ssh-agent 通道同步一次 host 墙钟**，以及在实现中踩的两个 Rust `impl` 块编译坑。

## 问题表现 (Symptoms)

- guest 内 `date` 显示 `1970-01-01`，与 OHOS 设备当前时间相差数十年。
- LSP/工具链在 guest 内访问需要有效期校验的 HTTPS 资源时，因证书"尚未生效/已过期"而失败。
- guest 内创建的文件 `mtime` 落在 1970，host 侧按时间戳判断的新旧/缓存逻辑失准。

## 问题原因 (Root Cause)

1. **guest 内核起来时钟就是 epoch**：Linux 内核把 `CLOCK_REALTIME` 初始化为 0；正常由两种机制搬入真实时间——内核态 `CONFIG_RTC_HCTOSYS` + RTC 驱动（aarch64 `virt` 上是 PL031），或用户态 init 跑 `hwclock -s` / systemd `systemd-timesyncd`。本 guest 是 **BusyBox initramfs**（`crates/gpui_ohos/depend/ohos-qemu-agent/images/kernel-build.md:31` 原文 "running BusyBox initramfs + cmd-agentd"），无 systemd、init 不读 RTC，所以系统时钟永远停在 1970。
2. **TCG 软模拟下还会漂移**：guest 在 TCG 下系统时钟会随时间偏离真实时间，单靠启动设一次仍会慢慢偏。
3. **QEMU 没有"主动推时间给 guest"的内置 API**：
   - QMP（QEMU 主管理接口）只有 `query-rtc`（只读）、`RTC_CHANGE`（事件，非命令）、`-rtc base=`（仅启动初值）——**无任何 set-time 命令**。QEMU 只模拟 RTC 硬件，不知道 guest 内 `CLOCK_REALTIME`。
   - aarch64 `virt` + TCG 无标准 pvclock 半虚拟化时钟（x86 才有 `kvm-clock`/`pvclock`）。
   - 唯一官方"设 guest 时间"的接口是 **QEMU Guest Agent 的 `guest-set-time`**，但它要求 guest 内另跑一个 `qemu-ga` 守护进程、并新挂一条 `org.qemu.guest_agent.0` virtio-serial 通道——对极简 rootfs 而言需额外交叉编译二进制 + 加启动参数 + 拉起守护进程，性价比低。

## 解决方案 (Solution)

**复用现有 ssh-agent 控制通道直接发一条 `date` 命令**，零新增二进制、零新增端口。

**路由依据（已查实）**：OHOS 上 `util::command`（`crates/util/src/command/ohos.rs`）的 `spawn` 把 `ExecSpec` 交给 `qemu_cmd_agent_linker::executor()` 全局注册的 `RemoteCommandExecutor`；`launch-zed/src/launch_app.rs:308` 注册的正是 `SshCommandExecutor`（`ssh-agent`）。即当前 `util::command` 在 OHOS 上 = 经 ssh-agent 在 guest 内以 **root** 跑 `sh -c`（guest 进程能做 mount / kill 进程组，必然 root，满足 `CAP_SYS_TIME`）。所以设时间无需新协议，直接发命令即可。

**调用哪个程序**：guest 只有 busybox，**唯一能设 `CLOCK_REALTIME` 的就是 busybox 的 `date` applet**（`date -s`）。python3 / 专门 `clock_settime` 包装 / 读 RTC 的 `hwclock` 均不存在。故用：
```
date -s @<epoch秒>     # busybox date 支持绝对秒写法，时区无关；需 busybox >= 1.20
```
偏旧 busybox 不支持 `@` 时，回退 `date -u -s "<YYYY-MM-DD HH:MM:SS>"`（UTC 字符串，同样时区无关）。

**最终落点（用户决策演化）**：
- 最初想周期同步（每 30s 调 `date`）→ 用户否："不要周期，guest 启动后设一次就行"。
- 想挂在 `mount_folder` 里（"延迟 mount 成功后设"）→ 用户否："**千万不要在 `mount_folder` 里调用**"，应在延迟 mount 成功后、而非 mount 函数内部。
- 探索发现：代码里命名 `mount-deferred` 的线程在 cmd-agent（`cmd-agent/src/executor.rs`），但当前 `launch_app.rs:308` 注册的是 **ssh-agent**；ssh-agent 的 `mount_folder` 本身是阻塞等 guest SSH 就绪的"延迟挂载"，没有独立 deferred 线程。在 cmd-agent 后端路径加同步，在当前运行时不会触发。
- **最终落地 = 方案一（当前生效）**：在 `start_qemu` 注册 `SshCommandExecutor` 之后，spawn 一个**一次性**后台任务，内部 `pool.allocate()` 会阻塞等到 guest 的 SSH server 起来（guest 真正能跑命令），然后调一次 `sync_system_time()` 设时间，任务结束、**不循环**。语义满足"guest 启动后设一次、不周期、不碰 `mount_folder`"。

**实现（两个 inherent 方法，`executor.rs` 第 91 行 inherent `impl` 内）**：

```rust
// inherent impl SshCommandExecutor（第 91 行起）
pub async fn sync_system_time(&self) -> std::io::Result<()> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // busybox `date -s @<epoch>` 设 CLOCK_REALTIME；guest root 满足 CAP_SYS_TIME。
    // 秒级精度（busybox 无 clock_settime 包装）。不影响 CLOCK_MONOTONIC，make/LSP 计时不受影响。
    let command = format!("date -s @{secs}");
    let conn = self.pool.allocate()?;
    run_ssh_command(conn, &command).await
}

pub fn sync_system_time_once(self: Arc<Self>) {
    let handle = self.pool.runtime().handle().clone();
    handle.spawn(async move {
        if let Err(err) = self.sync_system_time().await {
            log::warn!("ssh executor: initial guest time sync failed: {err}");
        }
    });
}
```

`launch_app.rs` 的 `start_qemu` 在 executor 注册后调用（第 316-318 行）：
```rust
// re-sync). sync_system_time_once blocks on pool.allocate until the guest SSH
// server is up, so this is safe to call right after the executor is registered.
executor.clone().sync_system_time_once();
```

**为什么只设一次够用**：TLS 校验、文件时间戳、`make` 增量判断都是秒级粒度；单 session 内 TCG 漂移（几十秒量级）对这些场景无实质影响，无需周期纠漂。

## 编译过程中踩的两个 impl 块错误（重点，避免后人重走）

本功能实现时连续两次编译失败，根因都是**方法放错了 `impl` 块**。

**坑 1 — 方法游离在 `impl` 块之外（自由函数）**
初版把两个方法加在 `impl SshCommandExecutor { ... }` 闭合 `}` 之后，成了文件末尾的 **free function**。Rust 里 `&self` / `self: Arc<Self>` 这种接收语法只能在 `impl` 或 trait 定义内使用，自由函数不允许 → 报：
```
error: `self` parameter is only allowed in associated functions
error[E0411]: cannot find type `Self` in this scope
```
**修复**：把方法移回 `impl` 块内。

**坑 2 — 方法误放进 trait impl 块（而非 inherent impl）**
移进去时又放错了块：放进 `impl qemu_cmd_agent_linker::FolderMounter for SshCommandExecutor`（第 276 行的 **trait 实现块**），而非第 91 行的 **inherent `impl`**。trait impl 块内：① 不能写 `pub`（可见性由 trait 决定）→ `E0449: visibility qualifiers are not permitted here`；② 只能实现 trait 规定的方法，不能加自有方法 → `E0407: method ... is not a member of trait FolderMounter`；③ 连带调用处 `Arc<SshCommandExecutor>` 找不到该方法 → `E0599: no method named sync_system_time found`。
**修复**：把方法放进 **inherent `impl SshCommandExecutor`**（第 91 行那个，含 `new()`），trait impl 块只保留 trait 成员。

**可复用结论**：给"实现了 trait 的类型"加**自有方法**，必须放进 **inherent impl 块**，不能放进 trait impl 块，更不能游离于任何 impl 块之外。判断方法该放哪：看它是否为某个 trait 要求实现——是则进 trait impl，否则一律进 inherent impl。

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agent/src/executor.rs` — **正式修复**：在 inherent `impl SshCommandExecutor`（第 91 行）新增 `sync_system_time(&self)`（经 `run_ssh_command` 在 guest 跑 `date -s @<epoch>`，秒级、root 有 CAP_SYS_TIME）与 `sync_system_time_once(self: Arc<Self>)`（spawn 一次性后台任务，`pool.allocate()` 等 guest SSH 就绪后设一次，不循环）。
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — **正式修复**：`start_qemu` 在 `init_executor(executor)` 注册（第 308 行）后调用 `executor.clone().sync_system_time_once()`（第 316-318 行）。

## 待验证 / 备注

- **busybox `date -s @<epoch>` 语法**：需 busybox ≥ 1.20（当前 6.18.7 内核配套的必然支持）；偏旧 build 不支持 `@` 时，代码中应改用 `date -u -s "<YYYY-MM-DD HH:MM:SS>"`。
- **本机无 cargo/rustc**（`script/cargo` 仅 node 包装），需在开发机 `172.16.100.2` 或 CI 编译校验 `qemu_ssh_agent` + `launch-zed`。
- **cmd-agent 后端路径**：若将来把 `launch_app.rs` 切回注册 `QemuCommandExecutor`（cmd-agent），需在其 `mount-deferred` 线程（`run_mount` 成功后）另行加同步——当前路径走 ssh-agent，该处改动不触发。

关联：[ohos-debug-lessons] 同类经验——guest 内任何"系统状态缺失"（时间、缓存、fd）都应优先复用现有 ssh-agent / cmd-agent 控制通道以 root 直接修正，而非引入新协议或新守护进程；且 guest 是极简 busybox initramfs，能用的工具只有 busybox applet。
