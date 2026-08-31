# QEMU 9p 读写文件性能优化（guest 读为主 + host/guest 双写场景）

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植）内嵌 QEMU guest 承载 git/LSP/终端等命令转发（OHOS 沙箱禁止 spawn 子进程）。guest 通过 virtio-9p 挂载三类目录：**workdir**（`security_model=passthrough`，host 编辑器和 guest 双写）、**sandbox**（`mapped-file`，放 LSP/node_modules）、**tools**（`none` 只读，放 clangd/python3/头文件）。guest 以读为主，尤其 **clangd LSP 需要大量读取文件**——而 9p 每文件一次 virtio 往返，1921 个小文件读完要约 40 秒。核心问题：**是否有比 9p 更快且能在 OHOS 架构下落地的技术？9p 参数（msize/cache/async/security_model）哪些真实有效、值多少？**

## 问题表现

- guest 顺序读 1921 文件/9.8MB 模拟源码树（`find -exec cat`）约 **40 秒**（每文件 ~30ms 往返，正是 LSP 痛点）。
- 元数据操作 `find` / `ls -laR` 各 **8-11 秒**。
- virtiofs 理论上最快，但需要独立 `virtiofsd` 进程（QEMU 官方文档到 v9.0 均无 in-process 模式），而 OHOS 沙箱禁 spawn → **架构上不可用**。
- 双写 + 读为主约束下，9p 缓存档位选择受限（见原因），需要实测数据而非猜测。
- 复现：guest 每次挂载后读文件即可触发。

## 问题原因

**核心：小文件读是"延迟绑定"不是"带宽绑定"，且 9p 无缓存失效机制。**

1. **9p 协议开销**：guest 每个文件操作（open/read/close/stat）是一次 virtio 请求-响应往返，约 30ms/文件。1921 个文件就是 40s。这类负载对**每次往返的延迟**敏感，对**单次传输带宽**不敏感。
2. **9p 无缓存失效**（Linux 9p 维护者原话："We don't have cache invalidation in 9p"）：`cache=loose` 下 host 写的文件 guest 看不到，只适合"独享、只读"挂载。
3. **`cache=mmap` 有损坏风险**：走 writeback 缓存，有 O_APPEND 写重复、以及 clang 编译系统头文件时数据损坏的记录（clangd 正是 clang 系，直接命中）。
4. **`mapped-file` 每操作读写 xattr 元数据**，比 passthrough/none 慢约 12%。
5. **passthrough 写需要 cap_chown**：guest root（uid 0）写 passthrough 挂载，QEMU 进程要在 host 侧创建 uid 0 的文件——非 root 且无 cap_chown 则 EPERM。真机 app 非 root 但有 cap_chown（曾有的 mount 写测试能通过），写正常；本实验环境（host uid 1000 无 cap_chown）复现了 EPERM。
6. **结论**：没有比 9p 更快且能落地的挂载技术（virtiofs 需外部进程、NFS 需 host 服务端、cache=mmap 有损坏风险）；性能只能从"减少 9p 往返"和"选择正确的缓存/安全模型"拿。

## 解决方案

### 独立 QEMU 基准（同内核 6.18.7 + rootfs.cpio.zst + cortex-a76/4核/8G）

生成 1921 文件/9.8MB 模拟源码树（src/include/lib/mod/tests 小文件 + 20 个大文件 + .git 元数据），用 `qemu-system-aarch64` 独立启动，guest 内测：**顺序读两遍**（READ1 冷 / READ2 热）、`find`（元数据）、`ls -laR`、**小文件写**（cp 320 文件进挂载）、**大文件写**（dd 16MB）。基准脚本在 `/tmp/9pbench/`。

**关键实测数据（秒，越小越好）：**

```
变体          security_model  guest 挂载参数           READ1  READ2  META  LS    WRITE-小  WRITE-大
base          passthrough     msize=262144             39.7   40.3   8.0   8.5   —         —
nodflt_msize  passthrough     (内核默认)                39.1   39.7   7.9   8.5   —         —
msize1m       passthrough     msize=1048576            40.2   39.6   8.3   8.7   —         —
cache_mmap    passthrough     msize=1M,cache=mmap      34.8   34.2   7.9   8.4   —         —
cache_loose   passthrough     msize=1M,cache=loose     16.6   7.6    0.8   1.0   —         —
async         passthrough     msize=1M,async           38.8   39.4   8.0   8.6   —         —
mapped_read   mapped-file     msize=262144             39.9   39.5   8.0   8.5   —         —
none_read     none            msize=262144             38.6   38.9   7.9   8.4   —         —
write_base    none            msize=262144             39.0   38.6   7.9   8.4   11.4      2.8
write_best    none            msize=1M,async           39.8   40.2   8.1   10.3  13.3      3.1
write_mapped  mapped-file     msize=262144             44.0   43.8   9.1   9.5   13.0      3.0
```

并行读对比（passthrough，base vs async）：SEQ 43.5/43.1，PAR4 9.7/10.0，PAR8 9.8/10.0。

**结论（按杠杆排序）：**
- `cache=loose`：读快 **2-5 倍**（热读 40→7.6s、find 8→0.8s），但仅**只读挂载**安全（host 不写）→ **tools 用**。
- `mapped-file` → `passthrough/none`：读写各快约 **12%**（44→39.9s 读；小文件写 13→11.4s）。
- `msize`、`async`：**零收益**（延迟绑定；并行读 async 无提升——客户端已流水线）。
- `cache=mmap`：只快 12%，且有 clang 损坏风险 → 不用。

### 落地改动（用户选"只做改动 A"）

**tools 只读挂载加 `cache=loose`**（收益最大、零属主风险、只读 100% 安全）：

- 改 rootfs `etc/init.d/S40sandbox` 的 tools 挂载行：
  ```
  前：-o trans=virtio,version=9p2000.L,msize=262144,ro
  后：-o trans=virtio,version=9p2000.L,msize=262144,ro,cache=loose
  ```
- 重打包 `rootfs.cpio.zst`（cpio newc + zstd，路径无 `./` 前缀），替换 `images/`（原文件备份 `rootfs.cpio.zst.bak`）。
- 独立完整 init 启动测试通过：S40sandbox 用 cache=loose 挂载 /tools 成功、clangd 可见。
- `bundle-ohos` 构建成功（新 rootfs 打进 HAP）。

**未改**：workdir（passthrough）和 sandbox（mapped-file）维持现状——passthrough 写经确认在真机正常（app 有 cap_chown），sandbox 改 passthrough 有属主变乱取舍，暂缓。

### 排查过程中的死路（重要教训）

- **误判"QEMU 有 in-process virtiofs"**：曾以为较新 QEMU 支持内嵌 virtiofsd，做了 6 轮检索确认**不存在**（QEMU 官方文档到 v9.0 均为独立 vhost-user 进程），已更正。
- **误判"`-fsdev` 支持 cache/msize"**：初拟 fsdev 加 `cache=loose,msize=1M`，实测 QEMU `-fsdev local` **不支持**（报 `Invalid parameter 'msize'`）。修正：**`msize`/`cache` 都是 guest 侧 `mount -o` 选项**。
- **误判 passthrough 写是"真机隐患"**：基准复现 guest-root 写 passthrough 挂载 EPERM，初判真机 git 写会坏；经用户确认真机 app 非 root 但有 cap_chown、曾有的 mount 写测试通过 → 是实验环境（host uid 1000 无 cap_chown）的假象，非真机问题。
- **基准坑**：写测试最初写进 `/tmp`（guest 本地）没测到 9p 写；`/dev` 未挂载导致 dd 失败；单遍读测不出缓存收益（需两遍）。

## 修改文件

- rootfs `etc/init.d/S40sandbox`（在 rootfs 镜像内，仓库无源文件）— tools 挂载行加 `cache=loose`。
- `crates/gpui_ohos/depend/ohos-qemu-agent/images/rootfs.cpio.zst` — 重打包（含 cache=loose 改动）；原文件备份为 `rootfs.cpio.zst.bak`。
- `script/bundle-ohos`（未改，仅将 `images/rootfs.cpio.zst` 拷进 resfile）。

*注：基准 harness（QEMU 启动 + guest 基准脚本）为临时脚本，在 `/tmp/9pbench/`，未入库。*

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
