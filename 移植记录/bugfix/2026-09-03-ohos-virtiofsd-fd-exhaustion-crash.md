# zcoder 周期性崩溃：QEMU virtiofsd 进程内 fd 耗尽

## 问题描述 (Problem Description)

zcoder 应用运行约 20~70 秒后周期性崩溃（SIGABRT）。崩溃与是否操作、是否打开工程无关，重启 HarmonyOS 系统后仍复现，且连续重启后崩溃时间逐次缩短（68s → 20s → 3s，反映系统级残留未回收）。用户将应用卸载重装、重启设备均无法解决。崩溃表层表现为 wgpu 渲染 `Out of Memory` / `Invalid surface` panic，曾误导排查指向 GPU/Vulkan。

## 问题表现 (Symptoms)

- 崩溃进程最终死于 `SIGABRT`（appspawn 日志 `exit with signal:6`），触发点是 `default_error_handler` panic：`wgpu error: Out of Memory`（`wgpu_core.rs:3925` get_current_texture 分支）或 `Invalid surface`（`wgpu_core.rs:3879` configure 分支，错误实为 hal 层 swapchain 创建失败）。
- 崩溃前 fd 数量异常：进程 `/proc/self/fd` 计数从启动后几百平滑增长到 **32768**（进程软上限），随后任何需分配 fd 的操作失败。
- 泄漏的 fd 经诊断分类**几乎全部是普通文件（file）**，路径全部指向打开工作区下的 cargo 构建产物树：`…/warp-ohos/warp/build/…/release(-lto)/.fingerprint/<crate>/invoked.timestamp`、`lib-<crate>.json`、`lib-<crate>`、`dep-lib-<crate>` 等。
- 每秒约 550~600 个新 fd，平滑单调增长，从不回落；anon_inode/socket/pipe 数量恒定（几十~几百），排除图形 buffer、网络、管道泄漏。
- 系统日志 `hiview/FdLeakDetector` 持续追踪该进程（fd 泄漏监控），佐证泄漏真实且量级异常。
- 早期崩溃报错 `Out of Memory`，后期改报 `Invalid surface`，二者是同一泄漏在不同崩溃路径（get_current_texture / configure）的表现。
- 界面在崩溃前正常（无提前卡死），进程在 fd 顶满后 1~2 秒内直接 abort。

## 问题原因 (Root Cause)

真正的根因不是 wgpu/Vulkan，也不是 GPU 内存，而是 **zcoder 进程内嵌 QEMU 的 virtiofsd 文件共享层把 fd 无限缓存不释放**。

**机制链（完整因果）**：

1. zcoder 内嵌 QEMU，用 rust-vmm virtiofsd（`patches/virtiofsd`）把工作区目录共享给 guest（QEMU 内的 busybox 小系统）。guest 里的 git/LSP 通过这个共享层读写 host 文件。
2. virtiofsd 为 guest **lookup 过的每一个 inode 打开一个 `O_PATH` fd**（`passthrough/mod.rs` `do_lookup` → `guest_fds.allocate()`），存入**无界** inode 缓存（`passthrough/inode_store.rs` 的 `BTreeMap<Inode, Arc<InodeData>>`）。这是 virtiofsd 的**正常设计**：持有 fd 以便后续对同一 inode 快速操作，免去重复路径解析。
3. fd 的释放**唯一**由 guest 发 `FUSE_FORGET` 驱动（`forget_one` 把 inode refcount 减到 0 才 `remove` 并 drop `GuestFile` 关 fd）。
4. **guest 为什么不来 FORGET**：Linux VFS 里 positive dentry 只在内核有内存压力做 shrink 时才逐出并触发 FORGET。guest 有充足内存（数 GB），3 万个 dentry（几十 MB）不构成压力，dcache **永不逐出** → FORGET 几乎不来 → virtiofsd 的 fd 只进不出。
5. **触发灌数据的行为**：guest 的 git 每轮 `git status` 会递归 stat 整个工作树。`warp-ohos/warp/.gitignore` 只忽略 `/target`、`/app/target`，**没有忽略 `/build`**，因此 guest git 每轮遍历 `warp/build` 下 3 万个 cargo 构建产物，把 3 万个 inode 喂进 virtiofsd 缓存（这同时解释了 git panel 约 8 秒卡顿）。
6. `readdirplus: true`（默认）让 guest 一次 readdir 就对目录内每个条目做 lookup → 遍历放大成每秒几百次 open。
7. 嵌入式 Config 用 `..Default::default()`，三层保护全缺：
   - `guest_fd_limit = u64::MAX`（官方 CLI 会设为 `rlimit − 内部预留`，这里是默认 u64::MAX，`GuestFdSemaphore` 永不触发）；
   - `inode_file_handles = Never`（每 inode 持 O_PATH fd；官方 `--inode-file-handles` 用 `name_to_handle_at` 让 inode 不占 fd）；
   - 进程 `RLIMIT_NOFILE` 未按官方默认抬到 100 万（`limits.rs` `DEFAULT_NOFILE`），停在 OHOS 的 32768。
8. fd 顶到 32768 后，任何需开 fd 的分配（包括每帧图形 buffer 的获取/swapchain 创建）失败 → wgpu panic → abort。

**为什么早期报 OOM、后期报 Invalid surface**：都是 fd 耗尽后不同渲染调用失败的表象。曾误判为 GPU 内存问题，实则进程 fd 先被吃光。

**为什么另一个用同一 wgpu + Vulkan 的应用不崩**：它没有内嵌 QEMU/virtiofsd，guest 不会经这层读文件，没有 O_PATH fd 累积的路径。

**排除过的死胡同（重要，避免后人重走）**：
- Vulkan/GPU 驱动泄漏：否。另一 app 同 wgpu+Vulkan 正常；fd 分类证明泄漏是普通文件非图形 buffer。
- OHOS 图形栈 buffer 泄漏：否。崩溃时 native window 的 `RequestBuffer` 错误是 fd 耗尽的**结果**不是原因。
- wgpu `get_current_texture` 本身：否。崩溃栈只是 fd 耗尽的受害者。
- **file-handles 方案不可行**：把两处 Config 改成 `InodeFileHandlesMode::Mandatory`/`Prefer` 后，virtiofsd 启动即被 OHOS 沙箱 seccomp 以 `SIGSYS`(signal 31) 击杀——`name_to_handle_at`/`open_by_handle_at` 不在 OHOS seccomp 白名单，是**信号级拦截而非可捕获错误**，Prefer 也无法降级。file-handles 整条路在 OHOS 被封死。
- gitignore build 只是减小触发面，不是机制根因：即使 ignore 掉 build，guest 遍历任何超大目录仍会复发。

## 解决方案 (Solution)

核心洞察：virtiofsd/FUSE 协议要求 host 保留 inode 直到 guest FORGET，**virtiofsd 侧不能**（也不该）主动 drop inode fd（会破坏 guest 仍持有的 nodeid）。真正的杠杆在 **guest 内核的 dcache 回收**——只要让 guest 定期回收 dcache，就会发 FORGET，virtiofsd 随之释放 fd。

**实施**：在 guest 侧的 SSH 执行代理 `qemu-ssh-agentd`（`ssh-agent/qemu-ssh-agentd/src/main.rs`）里，加一个后台 tokio 任务，每 **15 秒**写一次 `/proc/sys/vm/drop_caches`，值用 **`2`**（只回收 dentries 和 inodes，不动 page cache；`drop_caches=2` 触发的是对 virtio-fs 的 FUSE_FORGET）。

```rust
// (main.rs, run() async block 内, accept loop 之前)
tokio::spawn(async move {
    let interval = std::time::Duration::from_secs(DROP_CACHES_INTERVAL_SECS);
    loop {
        tokio::time::sleep(interval).await;
        match std::fs::write(DROP_CACHES_PATH, DROP_CACHES_VALUE) {
            Ok(()) => log::debug!("ssh-agentd: reclaimed guest dentry/inode caches"),
            Err(err) => log::warn!("ssh-agentd: drop_caches write failed: {err}"),
        }
    }
});
```

常量：`DROP_CACHES_INTERVAL_SECS = 15`、`DROP_CACHES_VALUE = "2"`、`DROP_CACHES_PATH = "/proc/sys/vm/drop_caches"`。权限：ssh-agentd 在 guest 以 root 运行，写该 sysctl 权限足够。

**为什么选这条路**：
- 直接对症"guest 不 FORGET"这一机制病根，不依赖任何触发面（不 ignore 也安全）。
- 不违反 FUSE 协议、不改 virtiofsd 内核语义、不碰被 seccomp 禁的 file-handles。
- `drop_caches=2` 安全性已核：只回收**未被引用**（refcount=0）的 dentry/inode；使用中的文件不受影响；被清缓存的文件下次访问自动从 virtiofs 重新 lookup/加载（拿到最新状态，对共享文件系统反而增强一致性）。

**为什么先排除 file-handles 再落到此**：file-handles 是官方"inode 不占 fd"的正道，但被 OHOS seccomp SIGSYS 封死（实测进程直接 signal:31 崩溃）；提高 RLIMIT_NOFILE 只延后不治本；guest_fd_limit 满时把崩溃转嫁成 guest ENFILE 且会影响正常大文件操作。drop_caches 是实测唯一既治本又不影响 guest 正常操作的手段。

**验证结果**（部署后 hilog 实测）：
- guest 每 15 秒执行 drop_caches（`drop_caches: 2` 日志）。
- 进程 fd 从"每 4 秒涨数千、50 秒顶满崩溃"变为**稳定在 ~294**（drop 后骤降并保持低位）。
- ENFILE/EMFILE 错误 0 次；302 次 git 命令全部正常执行（含 265KB 大输出）；app 长时间运行不崩。

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agentd/src/main.rs` — **正式修复**：新增周期后台任务，每 15s 写 `/proc/sys/vm/drop_caches = "2"`，强制 guest 回收 dcache → 触发 FUSE_FORGET → 进程内 virtiofsd 释放 O_PATH fd，阻断 fd 累积到 32768 导致的崩溃。含命名常量与英文注释。

以下为排查过程中产生的改动，均**已回退或待清理**：
- `crates/gpui_ohos/src/ohos/wgpu_renderer.rs` — 排查期临时诊断 `log_resource_diag`（每 4s 打 fd/RSS、每 20s 采样 fd 类型直方图/路径），**验证完成后应移除**。
- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agent/src/virtiofs.rs` 与 `crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/qemu-ssh-agent/src/virtiofs.rs` — 曾尝试 `InodeFileHandlesMode::Mandatory`，因 OHOS seccomp SIGSYS 不可行，**已回退为原默认**。

关联：[ohos-debug-lessons] 中同型案例——崩溃表层指向渲染/GPU，实为进程资源（fd）耗尽在其他分配路径上的表现；定位时优先在进程内采样 `/proc/self/fd` 的类型与路径分布，能最快把"资源耗尽"从"具体调用失败"里区分出来。
