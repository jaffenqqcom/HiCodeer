# QEMU + Linux guest 启动速度优化（63s → 4.9s）

## 问题描述

zcoder（Zed → HarmonyOS NEXT 移植）在设备上内嵌 QEMU 虚拟机（guest 跑精简 Linux，承载 git/LSP/终端等命令转发——OHOS 沙箱禁止 spawn 子进程）。QEMU guest **冷启动极慢（约 63 秒）**：用户打开项目后，要等一分多钟 git/LSP/终端才可用，体验无法接受。启动路径 = QEMU 拉起 + Linux 内核 boot + initrd（rootfs）解压 + init 脚本（端口链接、9p 挂载、cmd-agentd）。

## 问题表现

- guest 冷启动约 **63 秒**（从 `aa start` 到 cmd-agentd 可服务）。
- initrd 是 **gzip 压缩的 `rootfs.cpio.gz`（161MB）**，initramfs 解压展开到内存耗时数秒，且占用大量 guest 内存。
- rootfs 里塞满了工具（clangd 29M、libLLVM 118M、libclang 57M、python3、ssh/scp/sftp、golang 等，展开后 480M+），QEMU 每次启动都解压加载，**但实际用到的只是其中一小部分**。
- QEMU TCG **单线程**模拟：guest 多核（-smp 多核）被串行执行，无法利用 host 多核并行。
- 启动阶段 init 脚本（S41virtioports 的 29 个 virtio 端口链接）**串行执行**。
- 复现：每次 guest 冷启动必现（QEMU 重启即触发）。

## 问题原因

四个叠加因素拖慢启动：

1. **TCG 单线程模拟**：QEMU TCG 默认单线程跑 vCPU，即使 `-smp 8`，guest 多核任务（init 并行、解压、服务启动）仍串行，等于没用上 host 的核。
2. **initrd 压缩格式低效**：gzip 压缩率低、解压慢；161MB 的 rootfs 解压到 tmpfs 内存是本启动的最大单点耗时。
3. **rootfs 臃肿**：工具全部打进 initramfs（rootfs 的 cpio），导致：(a) initrd 体积大 → 解压慢；(b) 展开后占大量 guest 内存；(c) QEMU 每次冷启动都重复解压这些工具——而工具只在用 git/LSP 时才会加载。
4. **工具无法从 HAP resfile 直接给 guest 用**：resfile 安装解压到只读的 `el1/bundle`（`application_resource_dir()` 返回，`context.resourceDir`），**不在 QEMU 的 9p 挂载范围（el2/base 沙箱）内**，guest 默认看不到 resfile 内容。此前 cmd-agentd 靠 launch_app 手动 `std::fs::copy` 到沙箱才能被 guest 看到——若把 480M 工具也 copy 到沙箱，启动要写 480M 到设备 flash，同样慢。

### 排查过程中的死路（重要教训）

- **误判"6 核 MTTCG 更快"**：先试 6 核 MTTCG，boot 反而 **36 秒**（比单线程 19 秒更慢）——启动阶段 init 是单线程负载，vCPU 增多只增加 MTTCG 同步开销。改为 **4 核 MTTCG** 才最快（12.8s）。核数不是越多越好，要看负载特征。
- **误判"工具扁平化到 resfile 根就能给 guest 用"**：把 tools 从 `tools/` 子目录扁平化到 resfile 根（`bin/`、`lib64/` 直接放 resfile 根），部署后 guest 仍看不到。曾误以为是"resfile 只解压根级文件、不支持子目录"，实际根因是 **resfile 解压到 el1/bundle，整个都不在 guest 的 9p 挂载树内**（cmd-agentd 之所以可见是 launch_app 手动 copy 的）。扁平化无效。
- **误判"复制 tools 到沙箱"**：曾考虑 zcoder 启动时把 480M 工具 copy 到设备沙箱再 9p 挂给 guest。用户指出几百兆复制一次时间很长，否决；最终用 QEMU 直接只读挂载 el1/bundle 的 resfile，零复制。

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

### 2. initrd 压缩 gzip → zstd

- `images/rootfs.cpio.gz`（161MB）→ `images/rootfs.cpio.zst`（18MB，zstd -9）。
- 内核需 `CONFIG_RD_ZSTD`（已加进自编译内核 6.18.7）。
- `launch_app.rs` initrd 路径 `rootfs.cpio.gz` → `rootfs.cpio.zst`。
- `bundle-ohos` 拷贝 zstd 版本、删除旧 gz。
- 效果：解压快、体积小 9 倍（内存占用低）。

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

### 4. init 脚本并行

`S41virtioports` 的 29 个 virtio 端口链接从串行改为并行，缩短 init 阶段。

## 修改文件

- `crates/gpui_ohos/depend/ohos-qemu-agent/cmd-agent/src/lib.rs` — `CPU_SMP="4"`、`MEM_SIZE="8G"`、`build_argv` 加 `-accel tcg,thread=multi`（MTTCG）、去掉 `-icount`；新增 `MOUNT_TAG_TOOLS`、`QemuPaths.tools_mount`、resfile 只读 9p 挂载（security_model=none, readonly）。
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — initrd 路径 `rootfs.cpio.gz` → `rootfs.cpio.zst`；`QemuPaths` 构造传 `tools_mount=resource_dir`（el1/bundle resfile）。
- `script/bundle-ohos` — initrd 改拷贝 `rootfs.cpio.zst` 并删除旧 gz；tools 扁平化到 resfile 根（bin/lib64/include/lib）；打包前删旧 `tools/` 目录；注释更新为"QEMU 挂载 resfile → /tools"。
- `crates/gpui_ohos/depend/ohos-qemu-agent/images/rootfs.cpio.zst` — 裁剪后的 rootfs（工具移出），zstd 压缩（18MB）。
- `crates/gpui_ohos/depend/ohos-qemu-agent/images/tools/` — 工具树（clangd、python3、ssh、libLLVM 等 + 补齐 libffi/libedit/libz/libtinfo）。
- rootfs init 脚本（`/etc/init.d/S40sandbox`、`S42cmdagentd`、`rcS`、`/etc/profile`）— `S40sandbox` 先挂只读 `/tools` 再挂可写 `/sandbox`；`S42cmdagentd` 优先 `/tools/cmd-agentd`；`rcS`/`profile` PATH 含 `/tools/bin` 与 `/sandbox/.../zcoder/languages` 两个挂载路径，LD_LIBRARY_PATH/CPATH/PYTHONHOME 指向 `/tools`。
- `S41virtioports` — 29 个 virtio 端口链接串行 → 并行。

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
