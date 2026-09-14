# zcoder QEMU guest 磁盘（qcow2）损坏：去除模板生成器 + 启动校验自愈

## 问题描述 (Problem Description)

zcoder 内嵌 QEMU 的 guest 完全无法启动：host 侧日志里 QEMU 反复报虚拟盘 `/dev/vda` 结构性损坏，guest 内核里 mkfs/ext4 挂载全部失败，QEMU 直接把盘判死。排查确认**损坏的不是 guest 内的写操作，而是 HAP 里打包的 qcow2 磁盘模板文件本身在生成时就是坏的**。本记录覆盖"字节级定位生成器 bug → 移除致坏代码 → 落地启动时校验自愈闭环"的完整过程。用户的核心诉求（原文）：

> 1、即使 /dev/vda 损坏了，在系统下次启动时，也可以自行修复好，最差的情况，就是重新生成空的 /dev/vda，就像第一次启动一样；2、修改导致本次 /dev/vda 损坏的原因。

即：**不只修这一次的坏盘，而是让"任何坏盘"都能自愈，并消灭致坏源头**。用户明确否决了"重新生成一个好模板"的一次性方案。

## 问题表现 (Symptoms)

- QEMU stderr 报 `L2 table at offset 0` 类损坏判词，guest 内 mkfs.ext4 / mount 全部失败。
- 用 `qemu-img check` / 字节级解析 qcow2 header：cluster 0（qcow2 header 自身所在簇）的 refcount 为 **0**——元数据簇被标记为"未分配"，任何合规的 QEMU 都会拒盘。
- 坏盘与 bundle-ohos 每次生成的模板 md5 完全一致：**生成器是确定性地生成坏盘**。

## 问题原因 (Root Cause)

`script/bundle-ohos` 内置了一段 Python qcow2 生成器（已移除），用 `struct.pack(">IIII", 1, 1, 1, 1)` 写 refcount block——**4 字节（32-bit）条目**。而 qcow2 v2 规范在默认 `refcount_order=4` 下，**每个 refcount 条目只有 2 字节（16-bit）**。

后果是字节错位：refcount block 实际内容变成 `00 01 00 01 ...`（按 2 字节读出来是 `[0,1,0,1,...]`），簇 0（header）、簇 1（L1 表）等关键元数据簇的 refcount 命中为 0 → QEMU 校验失败判整盘损坏。guest 首启 mkfs 自然全部失败。

**为什么不能只换一个好模板**：致坏的是生成器代码本身，重跑一次它还会生成同样的坏盘；且打包模板方案下"HAP 里的文件被外部破坏/掉电写坏"时同样没有出路。

## 解决方案 (Solution)

双层自愈闭环（host 判损重建 + guest 干净退出），外加致坏代码移除：

**1. 移除致坏源头**：`script/bundle-ohos` 中整段删除 qcow2 模板生成逻辑，HAP 不再打包任何盘文件（见设计文档 §4.6.4 决策 14）。

**2. host 侧代码生成合法空盘**（`launch-zed/src/qemu_runtime.rs`）：
- `write_empty_qcow2`：纯代码生成**合法 qcow2 v2 空盘**（256KB / 4 簇：header / 空 L1 表 / refcount 表 / refcount block），refcount 条目一律 2 字节且四簇 refcount 均 ≥1，virtual_size 按档位（64/96/128/256/512G）参数化。
- `inspect_qcow2`：**每次启动**校验 magic / version(2|3) / cluster_bits / virtual_size / L1 与 refcount 表偏移在文件范围内 / cluster 0 refcount ≥1。结果三分支：
  - **Valid** → 保留使用；
  - **SizeMismatch**（升降档）→ 保留不删、仅告警（盘上有数据，档位解绑）；
  - **Missing / Corrupt** → 删盘 → `write_empty_qcow2` 重建 → 本次启动等价"第一次启动"。

**3. guest 侧 init 自愈契约**（`qemu-mngt/guest-init/init`，配套 B 项）：
- mkfs/mount 失败重试 3 次（容忍瞬态 I/O 错），全败 `poweroff -f` **干净退出**——引擎 main 返回、host 侧下次启动判损重建；**绝不 `exec /bin/sh` 挂死引擎**。
- 系统层完整性双判据：`/mnt/root/usr/lib/qemu-init/init` 且 `/mnt/root/etc/os-release` 同时存在；不完整（如解包被掉电/杀进程打断）⇒ provisioning 未完成、盘上必无用户数据 ⇒ 安全 mkfs 重置后干净重解。

## 调试过程中的关键坑（重点，避免后人重走）

- **共享盘 mtime 缓存导致 cargo 漏编**：本机 `/storage/Users/currentUser/` 与 VM `/mnt/linux_share/` 是同一磁盘。修改 `qemu_runtime.rs` 后走 VM 编译，cargo 按 mtime 判断源码"没变"，**新代码根本没编进 lib**——设备上 md5 仍是坏盘旧指纹，一度误判"修改无效"。`touch` 源文件强制重编后一次通过。凡经共享目录做增量编译，"源码对但产物旧"先怀疑 mtime 缓存。
- **hdc 无法写应用沙箱**：想推"金标准好盘"（字节级正确的参照 qcow2）进应用 `files/` 做对照实验，`file send` 直接覆盖、先 unlink 再 send、经 `/data/local/tmp` cp 三路全被 FUSE 拦截。对照实验改为让 App 走自己的自愈路径重建，间接验证生成器正确性（重建后 `qemu-img check` 干净、refcount `[1,1,1,1,0,0]` 合法）。
- **provision 日志先于日志系统初始化**：hilog 抓不到 provision 阶段日志属预期，验证生成逻辑看磁盘文件本身（`qemu-img check` / md5 / 字节解析），别死等日志。

## 修改文件 (Modified Files)

- `script/bundle-ohos` — 整段移除 qcow2 模板生成器（含 4 字节 refcount bug 源头），盘不再打包进 HAP。
- `crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs` — 新增 `inspect_qcow2`（启动校验）与 `write_empty_qcow2`（合法 v2 空盘代码生成）；provision 流程改为"校验 → Valid 保留 / Corrupt·Missing 重建 / SizeMismatch 告警保留"。
- `crates/gpui_ohos/depend/qemu-mngt/guest-init/init` — mkfs/mount 重试 3 次 → `poweroff -f` 收尾；系统层完整性双判据（`qemu-init/init` + `os-release`）不完整即重置重解；修改后按设计文档 §4.6.2 重打包 `rootfs.cpio.zst`。
- `移植记录/design/2026-09-08-ohos-qemu-runtime-design.md` — 决策 14/15、§4.6.2/§4.6.4 同步更新。

## 验证 (Verification)

- 真机：App 自愈重建的盘 `qemu-img check` 零 error，refcount `[1,1,1,1,0,0]`（v2、2 字节条目）合法。
- guest 首启全链路打通：空盘 → mkfs → 解包系统层（273MB）→ switch_root → 磁盘系统 init 运行；host 4122/4123 转发口 LISTEN。
- 残盘场景：上次启动解包中断留下的不完整盘，下次启动被 `system layer incomplete; resetting disk` 判定并干净重置重解——自愈按设计生效。

## 待验证 / 备注

- 结构性损坏的**人为注入**实验（如字节覆写 header）尚未在真机做过，理论路径 = inspect 判 Corrupt → 重建，与 Missing 分支共用同一代码路径。
- 2026-09-09 发现的遗留问题（不属于本 bugfix）：磁盘系统第二段 init 二次挂 `virtiofs sandbox` 失败（详见设计文档 §7 风险与未决），与盘自愈无关，另行排查。

关联：[2026-09-08 设计文档](../design/2026-09-08-ohos-qemu-runtime-design.md) 决策 14/15；同类经验——凡"打包预生成产物"的地方，生成器与产物必须有一次独立校验闭环（宁可运行时生成 + 校验，也不要盲信构建期产物）。
