# QEMU + Linux guest 启动速度优化（63s → 4.9s）

> ## ⚠️ 现状修订（2026-09-12）：本文描述的是**已退役**的 `ohos-qemu-agent` 架构
>
> 本文记录 2026-08-30 的启动优化，对象是当时的 `ohos-qemu-agent/cmd-agent`：initramfs 承载根文件系统、`cmd-agentd` 作 guest 命令守护进程、resfile 只读 9p 挂给 guest `/tools`。
>
> 此后 QEMU 承载层整体重写为 `qemu-mngt/qemuctrl`，guest 栈整个换成 HiSH（Alpine + linux 6.12.60），**启动路径与本文描述已无一处相同**。本文作为历史记录保留（其中若干结论仍可复用），但**不可当作现架构的说明**——现架构见 `design/2026-09-08-ohos-qemu-runtime-design.md`（下称 A；virtio-fs 文件共享内容见其 §4.6）。
>
> | 本文所述（2026-08-30） | 现状（2026-09-12） | 见 |
> | --- | --- | --- |
> | `ohos-qemu-agent/cmd-agent`、`launch_app.rs` | `qemu-mngt/qemuctrl`、`launch-zed/qemu_runtime.rs` | A §4.5 / §4.7 |
> | initramfs：`rootfs.cpio.zst`（18 MB）解压进内存 | **无 initramfs**：`root=/dev/vda rw` 直接引导盘内系统（golden 母盘复制为工作盘） | A §4.6.3 / §4.6.4 |
> | guest 命令守护进程 `cmd-agentd` | `hicodeerd`（OHOS 宿主版进 HNP，guest 版静态链接进沙箱） | A §4.4 |
> | resfile 只读 9p 挂载 → guest `/tools` | **`/tools` 这一层已不存在**：host 侧二进制改由**用户数据根**承载，经第二个静态 virtio-fs share `customer_data` 挂入 guest 同名绝对路径 | A §4.6.1 |
> | 引导脚本 `S40sandbox` / `S41virtioports` / `S42cmdagentd` | `init` / `rcS` / `S00mount` / `S10sandbox` / `S12data` / `S30cmd-daemon` | A §4.6.4 |
> | 内核 6.18.7（自编译，为解压 initrd 而开 `CONFIG_RD_ZSTD`） | 6.12.60（HiSH `arm64_virt` 基座 + 5 项能力增量 + 6 项性能增量） | A §4.6.2 |
> | 引擎：自建 `libqemu-system-aarch64.so`（19.7 MB，外挂 5 个 so） | HiSH release 的 QEMU 10.2.0（52.5 MB，`DT_NEEDED` 只剩 3 个 so） | A §4.6.1 |
> | 启动参数含 `kpti=off`（照搬自参考实现） | **已移除**：本内核 `CONFIG_UNMAP_KERNEL_AT_EL0=n`，该 early 参数从未注册，留着只会多一行 unknown parameter | A §4.6.2 |
> | 根文件系统 openEuler | Alpine 3.22（musl + OpenRC + busybox） | A §4.6.3 |
>
> **仍然成立、可直接复用的三条结论**：
>
> 1. **MTTCG 是对的**（`-accel tcg,thread=multi`），且 `-icount` 与它不兼容、必须去掉。现状仍是 `tcg,thread=multi,tb-size=2048`。
> 2. **"核数不是越多越好"**：启动阶段是单线程负载，加核只付 MTTCG 同步税。此结论后续被进一步强化——2026-09-12 起**默认档位改为单核**（guest 工作流本身串行，多 vCPU 只增加屏障翻译、TLB shootdown/IPI、BQL 与 virtio 队列争用）。
> 3. **"把大体积用户态工具从系统里移出去、按需共享"** 的方向没变；只是承载方式从"resfile 只读 9p"换成了"用户数据根 + 第二个 virtio-fs share"。
>
> 另：本文"修改文件"一节列出的路径**全部失效**（`crates/gpui_ohos/depend/ohos-qemu-agent/` 目录已从仓库移除）。

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植）在设备上内嵌 QEMU 虚拟机（guest 跑精简 Linux，承载 git/LSP/终端等命令转发——OHOS 沙箱禁止 spawn 子进程）。QEMU guest **冷启动极慢（约 63 秒）**：用户打开项目后，要等一分多钟 git/LSP/终端才可用，体验无法接受。启动路径 = QEMU 拉起 + Linux 内核 boot + initrd（rootfs）解压 + init 脚本（端口链接、9p 挂载、cmd-agentd）。

> **现状**：这条启动路径已无 initrd 解压那一段。现在是 QEMU 拉起 → 内核 boot → 盘内 `init` 跑 `rcS` 依次执行引导脚本 → 守护进程起监听。见上方现状修订。

## 问题表现

- guest 冷启动约 **63 秒**（从 `aa start` 到 cmd-agentd 可服务）。
- initrd 是 **gzip 压缩的 `rootfs.cpio.gz`（161MB）**，initramfs 解压展开到内存耗时数秒，且占用大量 guest 内存。
- rootfs 里塞满了工具（clangd 29M、libLLVM 118M、libclang 57M、python3、ssh/scp/sftp、golang 等，展开后 480M+），QEMU 每次启动都解压加载，**但实际用到的只是其中一小部分**。
- QEMU TCG **单线程**模拟：guest 多核（-smp 多核）被串行执行，无法利用 host 多核并行。
- 启动阶段 init 脚本（S41virtioports 的 29 个 virtio 端口链接）**串行执行**。
- 复现：每次 guest 冷启动必现（QEMU 重启即触发）。

> **现状**：initramfs 与 `S41virtioports` 均已不存在。那 29 个 virtio 串口端口链接整体消失——命令通道改为 SSH over loopback（host 4122/4123 → guest 4022/4023），不再逐端口建字符设备。守护进程的启动与自愈见 A §4.6.4。

## 问题原因

四个叠加因素拖慢启动：

1. **TCG 单线程模拟**：QEMU TCG 默认单线程跑 vCPU，即使 `-smp 8`，guest 多核任务（init 并行、解压、服务启动）仍串行，等于没用上 host 的核。
2. **initrd 压缩格式低效**：gzip 压缩率低、解压慢；161MB 的 rootfs 解压到 tmpfs 内存是本启动的最大单点耗时。
3. **rootfs 臃肿**：工具全部打进 initramfs（rootfs 的 cpio），导致：(a) initrd 体积大 → 解压慢；(b) 展开后占大量 guest 内存；(c) QEMU 每次冷启动都重复解压这些工具——而工具只在用 git/LSP 时才会加载。
4. **工具无法从 HAP resfile 直接给 guest 用**：resfile 安装解压到只读的 `el1/bundle`（`application_resource_dir()` 返回，`context.resourceDir`），**不在 QEMU 的 9p 挂载范围（el2/base 沙箱）内**，guest 默认看不到 resfile 内容。此前 cmd-agentd 靠 launch_app 手动 `std::fs::copy` 到沙箱才能被 guest 看到——若把 480M 工具也 copy 到沙箱，启动要写 480M 到设备 flash，同样慢。

> **现状**：第 1 条（TCG 单线程）已由 MTTCG 解决；第 2、3 条（initrd 与 rootfs 臃肿）随"无 initramfs + 母盘/工作盘"方案整体消失；第 4 条（resfile 不在挂载树内）在实现层仍然成立，但**已不再是问题**——host 侧二进制现在都放在用户数据根，由 virtio-fs 第二 share 挂入 guest，不再依赖 resfile 被 guest 看见。见 C §3.1。

### 排查过程中的死路（重要教训）

- **误判"6 核 MTTCG 更快"**：先试 6 核 MTTCG，boot 反而 **36 秒**（比单线程 19 秒更慢）——启动阶段 init 是单线程负载，vCPU 增多只增加 MTTCG 同步开销。改为 **4 核 MTTCG** 才最快（12.8s）。核数不是越多越好，要看负载特征。
- **误判"工具扁平化到 resfile 根就能给 guest 用"**：把 tools 从 `tools/` 子目录扁平化到 resfile 根（`bin/`、`lib64/` 直接放 resfile 根），部署后 guest 仍看不到。曾误以为是"resfile 只解压根级文件、不支持子目录"，实际根因是 **resfile 解压到 el1/bundle，整个都不在 guest 的 9p 挂载树内**（cmd-agentd 之所以可见是 launch_app 手动 copy 的）。扁平化无效。
- **误判"复制 tools 到沙箱"**：曾考虑 zcoder 启动时把 480M 工具 copy 到设备沙箱再 9p 挂给 guest。用户指出几百兆复制一次时间很长，否决；最终用 QEMU 直接只读挂载 el1/bundle 的 resfile，零复制。

> **现状**：第一条教训仍然成立且被强化（现默认单核）；第二、三条涉及 resfile 挂载的部分已随 `/tools` 一起退役。

## 解决方案

四个改动叠加，boot 从 63s → **4.9s**：

### 1. 启用 MTTCG（多线程 TCG）+ 4 核

`build_argv`（`cmd-agent/src/lib.rs`）：
```rust
// 前：单线程 TCG，多核 guest 串行
"-accel", "tcg,thread=single", "-smp", "6", "-icount", "sleep=on"

// 后：MTTCG 多线程模拟 vCPU 并行；-icount 与 MTTCG 不兼容必须去掉
"-accel", "tcg,thread=multi", "-smp", "4"
const CPU_SMP: &str = "4";
const MEM_SIZE: &str = "8G";
```
实测：单线程 19s → **4 核 MTTCG 12.8s**（6 核反而 36s，负优化，见死路）。启动阶段单线程负载 + 运行时多线程并行（LSP、编译）的最优平衡点是 4 核。

> **现状**：MTTCG 保留（`qemuctrl/src/lib.rs` 的 `-accel tcg,thread=multi,tb-size=2048`，`-icount` 未使用）；但 **"最优是 4 核"已被推翻**——2026-09-12 起默认 **单核**，核数按 `settings.json` 的 `qemu_cpu_cores` 档位可调。理由：guest 工作流（git/LSP/shell）本身串行，多 vCPU 只付 MTTCG 同步税；实测 B1（exec 密集）单核 45.23 s 反快于多核。详见 A §7.5。

### 2. initrd 压缩 gzip → zstd

- `images/rootfs.cpio.gz`（161MB）→ `images/rootfs.cpio.zst`（18MB，zstd -9）。
- 内核需 `CONFIG_RD_ZSTD`（已加进自编译内核 6.18.7）。
- `launch_app.rs` initrd 路径 `rootfs.cpio.gz` → `rootfs.cpio.zst`。
- `bundle-ohos` 拷贝 zstd 版本、删除旧 gz。
- 效果：解压快、体积小 9 倍（内存占用低）。

> **现状**：**整节已废**——没有 initrd 了，因此也没有压缩格式可选。内核 6.12.60 也不再需要 `CONFIG_RD_ZSTD`。取代方案是"母盘 golden.qcow2 → 复制为工作盘，`root=/dev/vda` 直接引导"，见 A §4.6.3/§4.6.4。

### 3. rootfs 裁剪：工具移出，QEMU 只读挂载 resfile → guest /tools（零复制）

工具（clangd/python3/ssh/libLLVM/libclang/libffi 等）从 rootfs 移出，放到 HAP resfile；QEMU **直接把 el1/bundle 的 resfile 只读 9p 挂载给 guest**，guest 从 `/tools/bin/...` 直接用，**设备 flash 零写入**：

- `cmd-agent/src/lib.rs`：`MOUNT_TAG_TOOLS = "tools"`、`QemuPaths.tools_mount`、`build_argv` 加只读 fsdev + device：
  ```rust
  "-fsdev", "local,security_model=none,id=fsdev_tools,path=<resource_dir>,readonly=on",
  "-device", "virtio-9p-pci,id=fs_tools,fsdev=fsdev_tools,mount_tag=tools",
  ```
  **只读目录不能用 `security_model=mapped-file`**（要写映射元数据），必须 `none`。
- `launch_app.rs`：`QemuPaths.tools_mount = resource_dir`（el1/bundle resfile 路径）。
- guest `S40sandbox`：**先挂只读 `/tools`，再挂可写 `/sandbox`**（先只读后可写，用户要求顺序）。
- `S42cmdagentd`：cmd-agentd 优先从 `/tools/cmd-agentd` 启动（sandbox copy 作兜底）。
- `rcS`/`profile`：`PATH=/tools/bin:/sandbox/haps/entry/files/zcoder/languages:$PATH`（两个挂载路径都要，zcoder 会下载各语言 LSP 到可写沙箱）、`LD_LIBRARY_PATH=/tools/lib64`、`CPATH=/tools/include`。
- `bundle-ohos`：tools 扁平化拷进 resfile（bin/lib64/include/lib 放 resfile 根），打包前删旧 `tools/` 目录（消除 HAP 里 250M 重复）。

**附加**：clangd 依赖 `libffi.so.8`/`libedit.so.0`/`libz.so.1`/`libtinfo.so.6`（libLLVM 的间接依赖），裁剪 rootfs 与 tools 都没有 → clangd exit 127。已从 VM（glibc 2.38 与 guest 一致）收集进 `images/tools/lib64`。

> **现状**：**这一层整体被取代**。9p 已全部换成 virtio-fs（见 C），`/tools` 挂载点消失。今天 host 侧的 Linux 二进制（managed Node 运行时、下载的语言服务器、debug adapter）落在**用户数据根**（`<数据根>/node`、`<数据根>/languages`、`<数据根>/debug_adapters`），由第二个静态 virtio-fs share `customer_data` 挂到 guest 的同名绝对路径；应用沙箱 `files/` 由 `sandbox` share 挂到 host 同路径。guest 侧不再有 `/sandbox`、`/tools` 这类**翻译后的**挂载点——现在三条路径都是"host 什么路径、guest 什么路径"。见 C §3.1、§3.3。

### 4. init 脚本并行

`S41virtioports` 的 29 个 virtio 端口链接从串行改为并行，缩短 init 阶段。

> **现状**：`S41virtioports` 已不存在（不再有 29 个串口端口链接）。现引导脚本序列为 `init` → `rcS` → `S00mount` / `S10sandbox` / `S12data` / `S30cmd-daemon`，见 A §4.6.4。

## 修改文件

> **现状**：本节列出的路径**全部失效**——`crates/gpui_ohos/depend/ohos-qemu-agent/` 目录已从仓库移除，`launch_app.rs` 亦不再存在。现行改动的文件级清单见 A §4 与 C §5。

- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agent/src/lib.rs` — `CPU_SMP="4"`、`MEM_SIZE="8G"`、`build_argv` 加 `-accel tcg,thread=multi`（MTTCG）、去掉 `-icount`；新增 `MOUNT_TAG_TOOLS`、`QemuPaths.tools_mount`、resfile 只读 9p 挂载（security_model=none, readonly）。
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — initrd 路径 `rootfs.cpio.gz` → `rootfs.cpio.zst`；`QemuPaths` 构造传 `tools_mount=resource_dir`（el1/bundle resfile）。
- `script/bundle-ohos` — initrd 改拷贝 `rootfs.cpio.zst` 并删除旧 gz；tools 扁平化到 resfile 根（bin/lib64/include/lib）；打包前删旧 `tools/` 目录；注释更新为"QEMU 挂载 resfile → /tools"。
- `crates/gpui_ohos/depend/ohos-qemu-agent/images/rootfs.cpio.zst` — 裁剪后的 rootfs（工具移出），zstd 压缩（18MB）。
- `crates/gpui_ohos/depend/ohos-qemu-agent/images/tools/` — 工具树（clangd、python3、ssh、libLLVM 等 + 补齐 libffi/libedit/libz/libtinfo）。
- rootfs init 脚本（`/etc/init.d/S40sandbox`、`S42cmdagentd`、`rcS`、`/etc/profile`）— `S40sandbox` 先挂只读 `/tools` 再挂可写 `/sandbox`；`S42cmdagentd` 优先 `/tools/cmd-agentd`；`rcS`/`profile` PATH 含 `/tools/bin` 与 `/sandbox/.../zcoder/languages` 两个挂载路径，LD_LIBRARY_PATH/CPATH/PYTHONHOME 指向 `/tools`。
- `S41virtioports` — 29 个 virtio 端口链接串行 → 并行。

---

*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。2026-09-12 补现状修订块。*
