# zcoder 文件共享方案：virtio-fs 替换 virtio-9p 详细设计

## 1. 背景与目标

zcoder 在 OHOS 上以嵌入式 QEMU（dlopen `libqemu-system-aarch64.so`）运行一个 Linux 访客虚拟机，LSP / git / 终端等命令经 cmd-agent 转发到访客执行。宿主（OHOS）与访客之间通过 QEMU 的共享文件系统交换数据。

**原始方案**：virtio-9p。宿主侧用 9p 的 `local` 后端（`security_model=mapped-file` / `passthrough`）把目录导出给访客，访客 `mount -t 9p`。性能实测极差（见第 6 节）：工作目录上 stat 单次往返约 5.7ms（慢 224 倍）、目录扫描慢 64 倍。

**目标**：把 9p 全部替换为 **virtio-fs**（9p 的官方继任者，基于 vhost-user + FUSE），显著提升共享文件系统性能，同时保证**挂载路径与 9p 时 100% 一致**（避免 LSP 索引缓存因路径变化失效）。

**约束**：
- 不做任何基于 9p 的"调优"（cache 参数、msize 等），只换方案。
- OHOS 沙箱禁止 spawn 外部进程，virtiofsd 必须**嵌入 zcoder 进程内**（同 QEMU 的 dlopen 方式）。
- guest 挂载点（`/sandbox`、`/tools`、工作目录设备路径同名）必须与 9p 时完全一致。

**最终方案**：zcoder 使用 **virtio-fs**（不再使用 9p）。本文档记录完整设计与实现。

## 2. 最终方案总览

virtio-fs 的组成：

- **前端**：QEMU 的 `vhost-user-fs-pci` 设备（vhost-user 传输）。
- **后端**：virtiofsd（Rust 实现），把宿主目录通过 FUSE 语义暴露给访客。
- **共享内存**：vhost-user 协议要求 guest RAM 为 memfd 共享内存（`memory-backend-memfd`）。

三段数据流：

```
zcoder 进程（OHOS）
 ├─ QEMU 引擎（dlopen libqemu-system-aarch64.so）
 │    ├─ -machine virt,memory-backend=mem（memfd 共享 RAM）
 │    ├─ -object memory-backend-memfd,id=mem,size=8G
 │    ├─ sandbox/tools 静态 vhost-user-fs-pci（启动参数）
 │    └─ 工作目录 vhost-user-fs-pci（运行时 QMP device_add 热插拔）
 └─ virtiofsd（Rust crate 嵌入，每个共享目录一个线程）
      ├─ sandbox/tools 后端（启动即起，监听 fs_*.sock）
      └─ 工作目录后端（mount_folder 时动态起，监听 fs_work<N>.sock）
访客（Linux 6.18.7）
 └─ mount -t virtiofs <tag> <path>（tag = sandbox/tools/ztag<N>）
```

覆盖的挂载点（与 9p 时一一对应）：

| 9p 时 | virtio-fs 时 | 挂载路径 |
|---|---|---|
| sandbox tag（静态，mapped-file） | sandbox tag（静态 vhost-user-fs） | `/sandbox` |
| tools tag（静态，none 只读） | tools tag（静态 vhost-user-fs，只读） | `/tools` |
| ztag<N>（动态，QMP fsdev-add） | ztag<N>（动态，QMP device_add vhost-user-fs） | 工作目录设备路径同名 |

## 3. 架构设计

### 3.1 静态挂载（/sandbox、/tools）

QEMU 启动参数（`cmd-agent/src/lib.rs` 的 `build_argv`）：

```
-M virt,memory-backend=mem
-object memory-backend-memfd,id=mem,size=8G
-chardev socket,path=<port_dir>/fs_tools.sock,id=fs_tools
-device vhost-user-fs-pci,id=fs_tools,chardev=fs_tools,tag=tools,queue-size=1024
-chardev socket,path=<port_dir>/fs_sandbox.sock,id=fs_sandbox
-device vhost-user-fs-pci,id=fs_sandbox,chardev=fs_sandbox,tag=sandbox,queue-size=1024
```

- 原 9p 的 `-fsdev local,security_model=...` + `virtio-9p-pci` 移除。
- `-object memory-backend-memfd` 是 vhost-user 的前提（guest RAM 共享给 virtiofsd）；**memfd_create 在 OHOS 沙箱可用是本方案成败的第一判定点**（实测通过，见第 7 节）。
- `-chardev socket,path=...,id=fs_*` 为 client 模式，连接 virtiofsd 的监听 socket。

virtiofsd 后端（`cmd-agent/src/virtiofs.rs`）：`start()` 在 QEMU 启动前创建 sandbox / tools 两个后端线程，每个用 `PassthroughFs`（tools 用 `PassthroughFsRo` 只读）+ `VhostUserFsBackendBuilder` + `VhostUserDaemon` 监听对应 socket，等 QEMU 的 chardev 连接并完成 vhost-user 握手。

### 3.2 动态工作目录挂载

工作目录路径只在用户打开文件夹时才知道，无法在 QEMU 启动参数里静态声明，因此运行时热插拔（`cmd-agent/src/executor.rs` 的 `run_mount`）：

1. **启动 virtiofsd 后端**：`virtiofs::spawn_workdir(port_dir, sequence, shared_dir, tag)` 创建 `PassthroughFs` 后端线程，监听 `fs_work<seq>.sock`（`port_dir` 由 QMP socket 的父目录推导）。
2. **QMP 热插拔**（`cmd-agent/src/qmp.rs` 的 `create_workdir_vhost_fs`）：
   - `chardev-add`：新增一个 client socket chardev，指向 virtiofsd 的 `fs_work<seq>.sock`。
   - `device_add`：`vhost-user-fs-pci`，绑定该 chardev，`tag=ztag<seq>`，挂到预创建的 `rp<seq>` 根端口。
3. **通知访客挂载**：`MountFolder2QEMU { uri, mount_tag=ztag<seq>, guest_path=设备路径 }` 经管理连接发出，等待 `MountOk`。
4. **访客挂载**：cmd-agentd 的 `worker_run_mount` 执行 `mount -t virtiofs ztag<seq> <guest_path>`。

编号分配沿用 9p 时的机制：`mount_counter` 自增 → `device_id=virtiofs<N>`、`chardev_id=vfwork<N>`、`mount_tag=ztag<N>`、`bus=rp<N>`。幂等：已挂载集合去重。

### 3.3 挂载路径一致性

工作目录仍挂载到**设备路径同名**的 guest 路径（如 `/storage/Users/currentUser/workspace/warp-ohos`），与 9p 时完全一致，保证 clangd 等 LSP 缓存跨重启有效。`/sandbox`、`/tools` 挂载点不变（`rootfs/etc/init.d/S40sandbox` 只把 `mount -t 9p ...` 改为 `mount -t virtiofs <tag> <path>`，tag 与路径均不变）。

## 4. 依赖软件修改

virtiofsd 依赖链中有 3 个 crate 在 OHOS 上无法直接编译/运行，均在 `patches/` 下做本地补丁，全部用 `cfg(target_env = "ohos")` 包裹，**其他平台编译与上游一致**。

### 4.1 virtiofsd（patches/virtiofsd）

**问题**：`PassthroughFs::new()` 内部 `OsFacts::new()` 用 `openat2` syscall 探测内核能力（Linux 5.6+ 优化）。OHOS 沙箱的 seccomp 策略禁 `openat2`（SYS_openat2，与项目里 cap-primitives 的 openat2 记录一致），直接 SIGSYS 杀死整个进程。

**修改**（`src/oslib.rs`）：
```rust
#[cfg(target_env = "ohos")]
let has_openat2 = false;

#[cfg(not(target_env = "ohos))]
let has_openat2 = { /* 原 openat2 探测逻辑 */ };
```
`has_openat2=false` 时 PassthroughFs 回退用 `openat`（沙箱允许）。`openat2` 只是性能优化，不影响功能。

### 4.2 cap-ng（patches/cap-ng）

**问题**：
1. cap-ng 的 `build.rs` **无条件**声明 `rustc-link-lib=dylib=cap-ng`，导致链接 `-lcap-ng`；OHOS 无 libcap-ng 库 → 链接失败。
2. `bindings.rs` 的 `#[link(name = "cap-ng")] extern "C" { capng_* }` 引用 libcap-ng 符号；OHOS 沙箱无 capability 语义，virtiofsd 的 capng 调用路径在非 root 下从不执行。

**修改**：
- `src/bindings.rs`：extern 块加 `#[cfg(not(target_env = "ohos"))]`；OHOS 下提供 20 个 `capng_*` 的 no-op stub（`mod capng_stubs`，返回 0/空）。
- `build.rs`：OHOS 下（`CARGO_CFG_TARGET_ENV == "ohos"`）生成一个空 `libcap-ng.a`（`!<arch>\n`）并 `rustc-link-search` + `rustc-link-lib=static=cap-ng`，让 `-lcap-ng` 声明解析到空库（stub 提供全部符号，运行时无需实际库）。

### 4.3 vmm-sys-util（patches/vmm-sys-util）

**问题**（OHOS libc 为 bionic 风格，与 crate 假设的 glibc 布局不同）：
- `sock_ctrl_msg.rs`：`libc::msghdr` 有私有字段（`__pad1`/`__pad2`），struct literal 构造失败；`msg_iovlen` 为 `i32`、`msg_controllen` 为 `u32`。
- `ioctl.rs`：`IoctlRequest` 应为 `c_int`（OHOS libc 的 ioctl request 是 i32），crate 却用了 `c_ulong`。
- `seek_hole.rs`：`lseek64` / `SEEK_DATA` / `SEEK_HOLE` 未从 libc 导入（OHOS 走非 musl 分支，import 列表无覆盖）。

**修改**（3 处，均加 `cfg(target_env = "ohos")`）：
- `sock_ctrl_msg.rs`：`new_msghdr` / `set_msg_controllen` 在 OHOS 下走 zeroed 构造分支（与 musl 分支相同语义）。
- `ioctl.rs`：OHOS 下 `IoctlRequest = c_int`。
- `seek_hole.rs`：OHOS 下补 `use libc::{lseek64, ENXIO, SEEK_DATA, SEEK_HOLE};`。

## 5. 其他改动

### 5.1 QEMU 构建（qemu-inhap）

- `configure-ohos.sh`：`--disable-vhost-user` 改为 `--enable-vhost-user`。
- `configs/devices/aarch64-softmmu/ohos.mak`：新增 `CONFIG_VHOST_USER=y`、`CONFIG_VHOST_USER_FS=y`。
- `subprojects/libvhost-user` 头冲突：OHOS sysroot 的 bionic 头用 `_UAPI_` guard，与 QEMU 自带 standard-headers（`_LINUX_` guard）双份定义 → 编译失败。用 `__OHOS__` 条件让 libvhost-user 统一用 bionic 头（`libvhost-user.h` / `libvhost-user.c` 各一处）。

### 5.2 guest 内核

原 6.18.7 自编译内核**未开 `CONFIG_VIRTIO_FS`**，导致 virtio-fs 设备在访客内停在 ACKNOWLEDGE、无法挂载。重新编译开启 `CONFIG_FUSE_FS=y` + `CONFIG_VIRTIO_FS=y`，`Image` 更新到 `ohos-qemu-agent/images/`。

### 5.3 HAP 打包（bundle-ohos）

新编译的 libqemu 对 libslirp / libz / libpixman / libglib **动态链接**（脚本原注释误写为"静态链接"），dlopen 时缺库失败。把 6 个依赖 `.so`（含传递依赖 libintl、libpcre2）放入 `images/`，`bundle-ohos` 统一拷进 HAP libs。

## 6. 性能对比

工作目录（LSP/git/编译主战场）实测（TCG 模拟 + OHOS 设备）：

| 指标 | 9p | virtio-fs | 提升 |
|---|---|---|---|
| stat_hot（stat 同文件 500 次） | 2865ms | 14ms | 204 倍 |
| scan500（创建+stat 500 文件） | 45197ms | 5627ms | 8 倍 |
| read1M（读 1MiB） | 368ms | 10ms | 37 倍 |
| write1M（写 1MiB） | 268ms | 103ms | 2.6 倍 |

stat 单次往返从约 5.7ms 降到约 0.03ms，**仅比 guest 本地 tmpfs 慢 1.4 倍**，达成"个位数倍"目标。

## 7. 验证结果与遗留事项

**设备实测全链路通过**：
- memfd_create 沙箱可用（QEMU memory-backend-memfd 创建成功，guest 正常 boot）。
- 静态挂载 `/sandbox`、`/tools` 与动态工作目录均通过 virtio-fs 挂载成功，路径与 9p 时一致。
- 访客 cmd-agentd 心跳/命令执行正常，多工作目录（warp-ohos、zcoder）并发挂载正常。
- 最终产物为 **virtio-fs**，9p 已从挂载路径、QEMU 参数、访客挂载脚本中移除（残留的仅注释/常量命名）。

**遗留事项**：
- HAP 当前为 debug 构建约 1.7GB（debug 版 libzcoder.so 1.3GB），release 构建会显著缩小。
- scan500 相对 tmpfs 仍慢约 42 倍，可进一步调 virtiofsd 的 cache 策略 / thread_pool_size。
- 建议对本次改动（3 个 patches + cmd-agent/cmd-agentd/bundle-ohos）做一次代码 review。
