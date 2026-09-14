# QEMU 工作目录挂载失败：guest mount 无热插 grace/重试，tag 未出现即一次性失败

## 问题描述

zcoder 打开的工作目录（位于 host `/storage/Users/currentUser/...`，静态 sandbox `files/` 之外）应经**动态 workdir 挂载**进入 QEMU guest，使 git/LSP/terminal 在 guest 侧能访问。实际表现：工作目录**从未挂载进 guest**——懒挂（`WorkdirAwareExecutor` spawn 时按 cwd 挂载）每次尝试都在 guest mount 一步失败、burn 掉 hotplug slot，工作目录对 guest 不可见。

## 问题表现

- QEMU 模式启动后，工作目录（如 `/storage/Users/currentUser/workspace/warp-oh`）在 guest 中不可见/不可访问。
- zcoderd / host 日志中 guest mount 失败：
  - `virtiofs-work1: client connected, serving requests`（backend 已起、QMP 已 device_add）
  - `mount: guest mount work1 failed; slot 1 burned (device stays hotplugged)`
  - `qemu_runtime: lazy mount /storage/Users/currentUser/workspace/warp-oh failed: mount: guest mount work1 failed`
- 对照日志：guest 侧 `virtiofs virtioN: discovered new tag: workX` 出现在 mount 失败**之后**——失败时 tag 根本还没被 guest 枚举出来。

## 问题原因

`qemuctrl/src/mount.rs` 的 `mount_workdir` 第 3 步，QMP `device_add` 热插 vhost-user-fs 设备后**立即**向 guest 发一次性 mount：

```rust
let command = format!("mkdir -p {mount_path} && mount -t virtiofs {mount_tag} {mount_path}");
if let Err(err) = guest_shell.run(&command) { /* 失败即 burn slot + return Err */ }
```

**关键遗漏（迁移 qemuctrl 裁剪时丢失）**：vhost-user-fs 设备经 QMP hotplug 进 QEMU 后，guest 侧需要时间——pciehp 枚举新 pcie-root-port → virtio-pci 发现设备 → virtiofs 驱动 probe 绑定 → `tag` 才存在。在这个窗口内 `mount -t virtiofs <tag>` 必然失败（`virtio-fs: tag <workX> not found`）。旧实现（`unused-agent-backup/ohos-qemu-agent-unused/ssh-agent/qemu-ssh-agent/src/executor.rs` 的 `mount_folder`）对此有明确处理：

```rust
// 3. SSH `mkdir -p <path> && mount -t virtiofs <tag> <path>`. The guest ...
//    the mount is retried in-guest until the tag appears (bounded window...)
"mkdir -p {} && for i in 1 2 3 4 5 6 7 8 9 10; do mount -t virtiofs {} {} && exit 0; sleep 0.3 ...; done; exit 1"
```

参考实现做对了两点：**device_add 后 guest mount 带重试窗口**（10 次、0.3s 间隔，tag 出现即成功），且 QMP 后留 grace。qemuctrl 重写时丢掉了这段，改为一次性 mount，于是**每一次**动态挂载都在"tag 尚未出现"的窗口内失败——工作目录自然永远挂不上。

## 解决方案

把 guest mount 命令改回重试循环（对齐参考实现 `mount_folder`）：`mkdir -p` 后 `for i in 1..10; do mount ... && exit 0; sleep 0.3; done`，mount 成功即退出，10 次仍失败才 burn slot。

```rust
// qemuctrl/src/mount.rs
// 顶部新增命名常量
const GUEST_MOUNT_RETRIES: usize = 10;
const GUEST_MOUNT_RETRY_DELAY: &str = "0.3";

// mount_workdir 第 3 步：retry 窗口让 guest 先枚举热插 tag 再 mount
let retry_slots = (1..=GUEST_MOUNT_RETRIES)
    .map(|i| i.to_string())
    .collect::<Vec<_>>()
    .join(" ");
let command = format!(
    "mkdir -p {mount_path} && for i in {retry_slots}; do mount -t virtiofs {mount_tag} {mount_path} && exit 0; sleep {GUEST_MOUNT_RETRY_DELAY} 2>/dev/null || sleep 1; done; exit 1"
);
```

验证（真机日志，QEMU 模式）：
- 第 1 次 mount 尝试 `virtio-fs: tag <work2> not found`（预期内失败，tag 未枚举）
- 0.3s 重试 → guest `virtiofs virtio5: discovered new tag: work2`
- 重试命中 → `mount: /storage/Users/currentUser/workspace/warp-oh mounted in guest as work2`
- 后续 `guest shell ready cwd=.../warp-oh`、`terminal hosted in the guest shell`——工作目录对 guest 可见可用。

**次生观察（启动竞态，未阻塞）**：zcoder 启动早期（guest zcoderd 尚未就绪、cmd-client 未连）会有一次懒挂尝试落到 `guest shell run` 失败而 burn 一个 slot。8 个 slot 余量充足，且后续重试换新 slot 成功，不影响结果；可作为后续优化（guest 就绪前不触发懒挂）。

## 修改文件

- `crates/gpui_ohos/depend/qemu-mngt/qemuctrl/src/mount.rs` — `mount_workdir` 的 guest mount 由一次性 `mkdir && mount` 改为 10 次重试循环（0.3s 间隔、成功即退出），使 mount 等到 QMP 热插后 guest 枚举出 tag 再成功；新增命名常量 `GUEST_MOUNT_RETRIES` / `GUEST_MOUNT_RETRY_DELAY`

参考实现：`crates/gpui_ohos/depend/unused-agent-backup/ohos-qemu-agent-unused/ssh-agent/qemu-ssh-agent/src/executor.rs`（`mount_folder`，含热插 grace + retry）。与 [[ohos-debug-lessons]] 排查经验相关；同链路另见 [[2026-09-09-qemu-provision-missing-disk-abort.md]]、[[2026-09-09-zcoderd-pty-slave-readonly.md]]。
