# HiCodeer 文件共享方案：virtio-fs 替换 virtio-9p 详细设计

> **修订：2026-09-12**。宿主侧实现随 QEMU 承载层重写（`ohos-qemu-agent/cmd-agent` → `qemu-mngt/qemuctrl`）而迁移，guest 栈整体换代 HiSH（Alpine + linux 6.12.60）。本文已按当前代码重写：路径、常量、tag 命名、挂载点全部对齐 `qemuctrl/src/*` 与 `guest-init/*`。
>
> §6 的性能数据是**历史实测值**，保留原样——它们测的是同一台设备上 virtio-fs 与 9p 之差，与后来换引擎、换发行版无关，结论（virtio-fs 全面胜出）不受影响。相关文档：QEMU 运行时总设计见 `2026-09-08-ohos-qemu-runtime-design.md`（下称 A）。

## 1. 背景与目标

HiCodeer 在 OHOS 上以嵌入式 QEMU（`dlopen libqemu-system-aarch64.so`）运行一个 Linux 访客虚拟机，LSP / git / 终端等命令经 cmd-client 转发到访客执行。宿主（OHOS）与访客之间通过 QEMU 的共享文件系统交换数据。

**原始方案**：virtio-9p。宿主侧用 9p 的 `local` 后端（`security_model=mapped-file` / `passthrough`）把目录导出给访客，访客 `mount -t 9p`。性能实测极差（见 §6）：工作目录上 stat 单次往返约 5.7ms（慢 224 倍）、目录扫描慢 64 倍。

**目标**：把 9p 全部替换为 **virtio-fs**（9p 的官方继任者，基于 vhost-user + FUSE），显著提升共享文件系统性能。

**约束**（今昔相同）：
- 不做任何基于 9p 的"调优"（cache 参数、msize 等），只换方案。
- OHOS 沙箱禁止 spawn 外部进程，virtiofsd 必须**嵌入 HiCodeer 进程内**（同 QEMU 的 dlopen 方式）。
- 不做路径翻译：guest 看到的路径必须与宿主路径一致，避免 LSP 索引缓存因路径变化失效。

**最终方案**：HiCodeer 使用 **virtio-fs**（不再使用 9p）。本文记录完整设计与实现。

> **与 9p 时代的一处重大差异**：9p 时 guest 侧使用翻译后的挂载点（`/sandbox`、`/tools`）。现在**不再有翻译层**——三个 share 都挂到"宿主什么绝对路径、guest 就什么绝对路径"。原因见 §3.3。

## 2. 最终方案总览

virtio-fs 的组成：

- **前端**：QEMU 的 `vhost-user-fs-pci` 设备（vhost-user 传输）。
- **后端**：virtiofsd（Rust 实现），把宿主目录通过 FUSE 语义暴露给访客。
- **共享内存**：vhost-user 协议要求 guest RAM 为 memfd 共享内存（`memory-backend-memfd`）。

三段数据流：

```
HiCodeer 进程（OHOS）
 ├─ QEMU 引擎（dlopen libqemu-system-aarch64.so；HiSH release，QEMU 10.2.0）
 │    ├─ -M virt,memory-backend=mem（memfd 共享 RAM）
 │    ├─ -object memory-backend-memfd,id=mem,size=<mem>G
 │    ├─ sandbox / customer_data 静态 vhost-user-fs-pci（启动参数）
 │    └─ 工作目录 vhost-user-fs-pci（运行时 QMP device_add 到 rp<N>）
 └─ virtiofsd（Rust crate 嵌入，每个共享目录一个线程）
      ├─ sandbox / customer_data 后端（启动即起，监听 fs_sandbox.sock / fs_data.sock）
      └─ 工作目录后端（mount_workdir 时动态起，监听 fs_work<N>.sock）
访客（Alpine 3.22 / linux 6.12.60）
 └─ mount -t virtiofs <tag> <path>（tag = sandbox / customer_data / work<N>）
```

覆盖的挂载点：

| share | tag | 宿主侧来源 | guest 挂载路径 | 后端 socket | 建立时机 |
|---|---|---|---|---|---|
| 应用沙箱 | `sandbox` | 应用 `files/` 目录 | 同宿主绝对路径 | `fs_sandbox.sock` | 启动参数（静态） |
| 用户数据根 | `customer_data` | `<数据根>`（仅当它不在沙箱内） | 同宿主绝对路径 | `fs_data.sock` | 启动参数（静态，按需） |
| 工作目录 | `work<N>` | 用户打开的目录 | 同宿主绝对路径 | `fs_work<N>.sock` | QMP 热插拔（动态） |

> **历史对照**（9p 时代）：sandbox → `/sandbox`、tools → `/tools`（只读）、`ztag<N>` → 工作目录设备路径同名。tools 这一层现已不存在，见 §5.2 说明。

## 3. 架构设计

### 3.1 静态挂载（`sandbox`、`customer_data`）

QEMU 启动参数（`qemuctrl/src/lib.rs` 的 `build_argv`）节选：

```
-M virt,memory-backend=mem,gic-version=max,iommu=none,usb=off,virtualization=off,...
-object memory-backend-memfd,id=mem,size=<mem>G      # vhost-user 的前提
-append console=ttyAMA0,115200 root=/dev/vda rw init=/usr/lib/qemu-init/init mitigations=off TERM=xterm
-chardev socket,path=<port_dir>/fs_sandbox.sock,id=fs_sandbox
-device vhost-user-fs-pci,id=fs_sandbox,chardev=fs_sandbox,tag=sandbox,queue-size=1024
# 仅当用户数据根位于沙箱之外时追加：
-chardev socket,path=<port_dir>/fs_data.sock,id=fs_data
-device vhost-user-fs-pci,id=fs_data,chardev=fs_data,tag=customer_data,queue-size=1024
```

- 原 9p 的 `-fsdev local,security_model=...` + `virtio-9p-pci` 已全部移除。
- `-object memory-backend-memfd` 是 vhost-user 的前提（guest RAM 共享给 virtiofsd）；**memfd_create 在 OHOS 沙箱可用是本方案成败的第一判定点**（实测通过，见 §7）。
- `-chardev socket,path=...,id=fs_*` 为 client 模式，连接 virtiofsd 的监听 socket。
- 常量：`MOUNT_TAG_SANDBOX="sandbox"`、`MOUNT_TAG_DATA="customer_data"`、`FS_SOCKET_SANDBOX="fs_sandbox.sock"`、`FS_SOCKET_DATA="fs_data.sock"`。

virtiofsd 后端（`qemuctrl/src/virtiofs.rs`）：`start(&paths)` 在 QEMU 启动前为 `sandbox` 起一个后端线程；`paths.data_mount` 为 `Some` 时再为 `customer_data` 起一个。每个后端用 `PassthroughFs` + `VhostUserFsBackendBuilder` + `VhostUserDaemon` 监听对应 socket，等 QEMU 的 chardev 连接并完成 vhost-user 握手。缓存策略为 `CachePolicy::Auto`（`qemuctrl/src/virtiofs.rs::run_backend`）。

**guest 侧由谁挂载**：引导脚本 `guest-init/S10sandbox`、`guest-init/S12data`：

- `S10sandbox`：`mkdir -p /data/storage/el2/base/haps/entry/files` 后 `mount -t virtiofs sandbox <同名路径>`，并在其下建 `home`、`qemu/logs`、`qemu/guest-conf`。
- `S12data`：先读 `.../qemu/guest-conf/customer_data_path`（宿主在启动前写入的数据根路径）。**文件不存在就直接退出**——没有自定义数据根时本次不建这个 share，guest 也就没有可挂的东西。有值则 `mkdir -p <该路径>` 后 `mount -t virtiofs customer_data <该路径>`，并在串口回显一行 `[qemu-init] data root mounted at <路径>: <顶层目录列表>`（这是 guest 侧证明"挂上了且内容是真的"的唯一凭据）。

**为什么数据根要单独占一个 share**：用户数据根可以选在应用沙箱之外（`<数据根>/node`、`<数据根>/languages`、`<数据根>/debug_adapters` 全是宿主预置的 linux-arm64 二进制），而 `sandbox` share 只覆盖沙箱内的 `files/`。这些二进制必须由 guest 在**原本的宿主绝对路径**上执行，所以需要第二个 share 把它们原样呈现进 guest。

### 3.2 动态工作目录挂载

工作目录路径只在用户打开文件夹时才知道，无法在 QEMU 启动参数里静态声明，因此运行时热插拔。

宿主侧的编排在 `qemuctrl/src/mount.rs` 的 `MountRegistry::mount_workdir`（三段式，add-only、从不卸载）：

1. **启动 virtiofsd 后端**：`virtiofs::spawn_workdir(port_dir, slot, shared_dir)` 创建后端线程，监听 `<port_dir>/fs_work<slot>.sock`，返回 socket 路径。线程起不来就返回 `Err`——绝不把 QEMU 指向一个没人服务的 socket。
2. **QMP 热插拔**（`qemuctrl/src/qmp.rs::create_workdir_vhost_fs`）：
   - `chardev-add`：新增一个 client socket chardev（id `fs_work<slot>`），指向 virtiofsd 的 `fs_work<slot>.sock`。
   - `device_add`：`vhost-user-fs-pci`，绑定该 chardev，`id=workdev<slot>`、`tag=work<slot>`，挂到启动时预建的根端口 `rp<slot>`。
3. **通知访客挂载**：guest 侧命令为
   ```sh
   mkdir -p '<宿主路径>' && for i in 1..10; do mount -t virtiofs 'work<slot>' '<宿主路径>' && exit 0; sleep 0.3 2>/dev/null || sleep 1; done; exit 1
   ```
   设备刚热插拔完，guest 需要先枚举 pcie 端口、再绑定 virtiofs 驱动，tag 才会出现，所以第一个 `mount` 常常太早——这就是 10×0.3s 重试窗口的来历（`GUEST_MOUNT_RETRIES` / `GUEST_MOUNT_RETRY_DELAY`）。
   该命令**不自己讲 SSH**：它经 `GuestShell` trait 由调用方注入（cmd-client 的 `run_shell` 打到 guest 守护进程），`qemuctrl` 因此不依赖 cmd-client。

**槽位分配**（与 9p 时代的自增计数器不同）：启动时按 `WORKDIR_MOUNT_SLOTS = 8` 预建 8 个 `pcie-root-port`（`rp0..rp7`）。挂载时取第一个空槽 `slots.iter().position(Option::is_none)`，编号即槽号：

| 用途 | 命名 |
|---|---|
| 后端 socket | `fs_work<N>.sock` |
| chardev id | `fs_work<N>` |
| device id | `workdev<N>` |
| mount tag | `work<N>` |
| pcie 端口 | `rp<N>` |

**失败即烧槽**（`mount.rs:87-150` 的既定语义）：任何一步失败都把该槽标记为已占用。理由写在代码注释里——后端线程已把 socket 绑住（同槽重试会叠第二个听众），`device_add` 成功后 QEMU 也保留 chardev/device id，所以失败后让下一次重试换新槽，而不是在同一槽上叠设备。8 槽用尽时报 `all 8 workdir slots are occupied`。

**幂等**：`mounted: HashSet<PathBuf>` 按 canonicalize 后的路径去重，已挂的直接返回 `Ok`。

**guest 重启**：`MountRegistry::reset()` 清空去重集合与全部槽位。新 guest 什么都不挂，旧实例的"已挂载"记录与烧掉的槽位都不能留——否则调用方会被告知某个路径还挂着，而新 guest 里其实空空如也。

### 3.3 挂载路径一致性

三个 share 一律挂到**与宿主完全相同的绝对路径**：

- 工作目录 → 如 `/storage/Users/currentUser/workspace/zcoder`
- 应用沙箱 → `/data/storage/el2/base/haps/entry/files`
- 用户数据根 → 如 `/storage/Users/currentUser/HiCodeer`

原因有二：

1. **LSP 缓存跨重启有效**：clangd 等按路径建索引，路径一变缓存全废。
2. **宿主下发的命令无需翻译**：宿主把 `<数据根>/node/node-vXX-linux-arm64/bin/node` 交给 guest 执行时，该路径在 guest 里**就是**同样的字符串。9p 时代那种"宿主路径 → guest 挂载点"的翻译层因此整体消失，`PATH`、语言服务器注册表、debug adapter 路径都不必再带映射规则。

## 4. 依赖软件修改

virtiofsd 依赖链中有 3 个 crate 在 OHOS 上无法直接编译/运行，均在仓库根 `patches/` 下做本地补丁，全部用 `cfg(target_env = "ohos")` 包裹，**其他平台编译与上游一致**。

引入方式（`Cargo.toml`）：`[patch.crates-io]` 把 `virtiofsd` / `capng` / `vmm-sys-util` 指到 `patches/<name>`；`qemuctrl/Cargo.toml` 只在 `[target.'cfg(target_env = "ohos")'.dependencies]` 下依赖 `virtiofsd = { workspace = true }`，并把 `vhost`(0.16) / `vhost-user-backend`(0.22) / `vm-memory`(0.17.1) 钉到与 virtiofsd 解析结果一致的版本，避免 cargo 编出第二份。

### 4.1 virtiofsd（`patches/virtiofsd`）

**问题**：`PassthroughFs::new()` 内部 `OsFacts::new()` 用 `openat2` syscall 探测内核能力（Linux 5.6+ 优化）。OHOS 沙箱的 seccomp 策略禁 `openat2`（SYS_openat2，与项目里 cap-primitives 的 openat2 记录一致），直接 SIGSYS 杀死整个进程。

**修改**（`src/oslib.rs:36-39`）：
```rust
#[cfg(target_env = "ohos")]
let has_openat2 = false;

#[cfg(not(target_env = "ohos"))]
let has_openat2 = { /* 原 openat2 探测逻辑 */ };
```
`has_openat2=false` 时 PassthroughFs 回退用 `openat`（沙箱允许）。`openat2` 只是性能优化，不影响功能。

### 4.2 cap-ng（`patches/cap-ng`）

**问题**：
1. cap-ng 的 `build.rs` **无条件**声明 `rustc-link-lib=dylib=cap-ng`，导致链接 `-lcap-ng`；OHOS 无 libcap-ng 库 → 链接失败。
2. `bindings.rs` 的 `#[link(name = "cap-ng")] extern "C" { capng_* }` 引用 libcap-ng 符号；OHOS 沙箱无 capability 语义，virtiofsd 的 capng 调用路径在非 root 下从不执行。

**修改**：
- `src/bindings.rs`：extern 块加 `#[cfg(not(target_env = "ohos"))]`；OHOS 下提供 20 个 `capng_*` 的 no-op stub，返回 0/空。
- `build.rs`：OHOS 下（`CARGO_CFG_TARGET_ENV == "ohos"`）生成一个空 `libcap-ng.a`（`!<arch>\n`）并 `rustc-link-search` + `rustc-link-lib=static=cap-ng`，让 `-lcap-ng` 声明解析到空库（stub 提供全部符号，运行时无需实际库）。

### 4.3 vmm-sys-util（`patches/vmm-sys-util`）

**问题**（OHOS libc 为 bionic 风格，与 crate 假设的 glibc 布局不同）：
- `sock_ctrl_msg.rs`：`libc::msghdr` 有私有字段（`__pad1`/`__pad2`），struct literal 构造失败；`msg_iovlen` 为 `i32`、`msg_controllen` 为 `u32`。
- `ioctl.rs`：`IoctlRequest` 应为 `c_int`（OHOS libc 的 ioctl request 是 i32），crate 却用了 `c_ulong`。
- `seek_hole.rs`：`lseek64` / `SEEK_DATA` / `SEEK_HOLE` 未从 libc 导入（OHOS 走非 musl 分支，import 列表无覆盖）。

**修改**（3 处，均加 `cfg(target_env = "ohos")`）：
- `unix/sock_ctrl_msg.rs`：`new_msghdr` / `set_msg_controllen` 在 OHOS 下走 zeroed 构造分支（与 musl 分支相同语义）。
- `linux/ioctl.rs`：OHOS 下 `IoctlRequest = c_int`。
- `linux/seek_hole.rs`：OHOS 下补 `use libc::{lseek64, ENXIO, SEEK_DATA, SEEK_HOLE};`。

## 5. 其他改动

### 5.1 QEMU 引擎（自建 → HiSH release）

- **现状**：引擎直接取 **HiSH release** 的 `libqemu-system-aarch64.so`（QEMU 10.2.0，52.5 MB，`DT_NEEDED` 只剩 `libslirp.so.0` / `libz.so` / `libc.so`），仓库里**不再自建 QEMU**，`qemu-inhap` 目录与 `configure-ohos.sh` 均已移除。
- **它自带 vhost-user**：virtio-fs 挂载当前可用，即为引擎含 `CONFIG_VHOST_USER*` 的实证。
- **历史**（自建时代，已不适用）：需要在 `configure-ohos.sh` 把 `--disable-vhost-user` 改为 `--enable-vhost-user`；在 `configs/devices/aarch64-softmmu/ohos.mak` 加 `CONFIG_VHOST_USER=y` / `CONFIG_VHOST_USER_FS=y`；`subprojects/libvhost-user` 与 OHOS sysroot 的 bionic 头双份定义冲突（`_UAPI_` vs `_LINUX_` guard），用 `__OHOS__` 条件让 libvhost-user 统一用 bionic 头。

### 5.2 guest 内核

- **原 6.18.7 内核**未开 `CONFIG_VIRTIO_FS`，导致 virtio-fs 设备在访客内停在 ACKNOWLEDGE、无法挂载；当时重编开启 `CONFIG_FUSE_FS=y` + `CONFIG_VIRTIO_FS=y`。
- **现状（6.12.60）**：内核 = HiSH `arm64_virt` 基座 + 增量。基座的 `CONFIG_FUSE_FS=n`、无 `CONFIG_VIRTIO_FS`、`CONFIG_PCIEPORTBUS=n`、`CONFIG_HOTPLUG_PCI=n`，**五项能力全部由本项目的增量补回**（virtio-fs 需要前两项，`pcie-root-port` 热插拔需要后三项加 `HOTPLUG_PCI_PCIE`）。完整构建记录见 `qemu-mngt/images/kernel-build.md`。

> **tools share 的消失**：9p 时代 resfile 被只读挂给 guest `/tools`（clangd、python3、libLLVM 等宿主导出的工具树）。现在这些宿主编译的 linux-arm64 二进制都放在**用户数据根**下（`node/`、`languages/`、`debug_adapters/`），由 `customer_data` share 挂入——不再需要单独的 tools share，也不再需要 `/tools` 这个翻译后的挂载点。

### 5.3 HAP 打包（`script/bundle-ohos`）

- **原**：自建 libqemu 对 libslirp / libz / libpixman / libglib **动态链接**，dlopen 时缺库失败，把 6 个依赖 `.so`（含传递依赖 libintl、libpcre2）放进 HAP。
- **现状**：只需 3 个——`libqemu-system-aarch64.so`、`libslirp.so.0`、`libz.so`。glib / pixman / pcre2 / intl 已内联进引擎。`libz.so` 是**无版本后缀**的名字，而设备 `/system/lib64` 下没有 zlib，做法是把既有 `libz.so.1` 的 `DT_SONAME` 重写为 `libz.so` 另存（`libhicodeer.so` 本身不依赖 zlib，已 `readelf -d` 核对，故旧的 `libz.so.1` 可删）。

## 6. 性能对比

工作目录（LSP/git/编译主战场）实测（TCG 模拟 + OHOS 设备，**2026-09 数据**）：

| 指标 | 9p | virtio-fs | 提升 |
|---|---|---|---|
| stat_hot（stat 同文件 500 次） | 2865ms | 14ms | 204 倍 |
| scan500（创建+stat 500 文件） | 45197ms | 5627ms | 8 倍 |
| read1M（读 1MiB） | 368ms | 10ms | 37 倍 |
| write1M（写 1MiB） | 268ms | 103ms | 2.6 倍 |

stat 单次往返从约 5.7ms 降到约 0.03ms，**仅比 guest 本地 tmpfs 慢 1.4 倍**，达成"个位数倍"目标。

> 该数据至今未重测：virtio-fs 的数据通路（memfd + vhost-user + FUSE passthrough + `CachePolicy::Auto`）自本次替换起未变，换引擎/换发行版改的是别的层。

## 7. 验证结果与遗留事项

**设备实测全链路通过**（含换代后复核）：
- memfd_create 沙箱可用（QEMU `memory-backend-memfd` 创建成功，guest 正常 boot）。
- 静态 share `sandbox`、`customer_data` 与动态工作目录均通过 virtio-fs 挂载成功；guest 串口回显 `[qemu-init] sandbox mounted at ...` 与 `[qemu-init] data root mounted at <数据根>: languages node debug_adapters ...`。
- 访客守护进程心跳/命令执行正常，多工作目录并发挂载正常。
- 9p 已从挂载路径、QEMU 参数、访客挂载脚本中完全移除。

**已知行为**：
- **启动早期的预热挂载会烧槽**。`WorkdirAwareExecutor` 的预热尝试发生在 guest 引导完成（约 5 s）之前，此刻命令通道尚未就绪，`mount_workdir` 失败并把槽标为已占用；hilog 可见 `mount: guest mount work0 failed; slot 0 burned` 及 `work1` 同款。槽位表随 QEMU 进程重建，真正的按需挂载在守护进程就绪后进行，因此不影响正常使用——但**8 个槽位被烧满后工作区将永远挂不上**，属已知缺陷（`mount.rs` 的烧槽策略与预热时序叠加所致）。

**遗留事项**：
- HAP 体积：debug 构建曾达约 1.7GB，release 已降至 505MB。
- scan500 相对 tmpfs 仍慢约 42 倍，可进一步调 virtiofsd 的 cache 策略 / `thread_pool_size`。
- 3 个补丁（`patches/virtiofsd`、`patches/cap-ng`、`patches/vmm-sys-util`）仍在生效，值得一次代码 review。
