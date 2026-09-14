# 集成 QEMU 虚拟机到 HiCodeer —— 设计文档

- 日期：2026-09-08（初稿）
- **2026-09-09 方案修订**：抛弃 initramfs + switch_root + overlay + 空盘首启灌系统；改为**内核直启磁盘系统 `root=/dev/vda` + golden.qcow2 母盘复制为工作盘**。
- **2026-09-12 方案修订（当前权威）**：guest 栈整体换代 **HiSH**——QEMU 引擎、内核、根文件系统、用户态工具全部换成 HiSH 的一套；`zcoderd` → `hicodeerd`（含环境变量、HNP 包名、resfile 子目录）；用户数据根作为第二个静态 share 挂进 guest；默认档位改为单核 / 4G / 128G；guest 守护进程加监督自愈；Node `os.userInfo` 垫片改为无条件注入；`bundle-ohos` 增加 guest-init 与 golden 的 mtime 依赖检测。本文已按上述状态全面重写。
- 关联：`bugfix/2026-09-09-qemu-provision-missing-disk-abort.md`（provision Missing 盘）、`bugfix/2026-09-09-zcoderd-pty-slave-readonly.md`（pty slave 只读）、`design/2026-09-10-ohos-data-home-directory-design.md`（数据根）、`design/2026-09-12-zcoderd-auto-restart-and-rename-design.md`（自愈与改名）、`qemu-mngt/QEMU-HiSH-替换方案.md`（换代方案）、`qemu-mngt/images/kernel-build.md`（内核重建）

## 一、背景与目标

HarmonyOS 沙箱禁止 spawn 子进程，HiCodeer 现靠 `cmd-client → hicodeerd(127.0.0.1:4022/4023)` 转发 git/LSP/终端命令。目标：**把 QEMU 引擎（dlopen `libqemu-system-aarch64.so`）集成进 HiCodeer**，将 LSP 与 Terminal 迁移到 guest，**去除对 HarmonyOS 命令行与本机命令守护进程的依赖**；命令执行由 guest 内的 **hicodeerd(Linux aarch64)** 承担。

2026-09-12 进一步确立：guest 的全部底层组件（引擎、内核、根文件系统、工具集）以 **HiSH** 为基线，只做"能力补齐 + TCG 性能回还"两类最小增量——原因是 HiSH 的基座是为 512 MiB 单核 guest 调优的，而本 guest 是承载 git + language server + 终端的多核 TCG guest。详见 `QEMU-HiSH-替换方案.md`。

## 二、设计决策

1. **cmd-client 同一时刻只连一个命令守护进程**：不并行、不热切，按启动时是否启用 QEMU 选定端点。
2. **guest 需要外网**：slirp NAT 出网；host 经**静态 hostfwd** 暴露 guest 4022/4023 为 host 侧 4122/4123。
3. **QEMU 默认开启，默认档位单核 / 4 GB / 128 GB**：`settings.json` 可调（档位只影响初始工作盘尺寸与 RAM/vCPU 分配）。单核是默认值，因为 guest 是纯 TCG 模拟、其负载（git/LSP/shell）本身串行，多 vCPU 只增加同步开销（见七）。
4. **磁盘 = 单一 qcow2 工作盘，直接作根盘引导**：构建期生成 **golden.qcow2 母盘**（qcow2，内已完整落好 Alpine 用户态系统层 + qemu-init），随 HAP resfile 分发、运行期只读；每次启动 provision 将其复制为**工作盘 disk.qcow2** 挂作 `/dev/vda` 根。guest 对盘的所有写入只落在工作盘副本上，母盘永不被改。
5. **母盘更新即刷新工作盘**：工作盘为母盘的字节级副本。启动校验 `inspect_qcow2`：**Valid 保留**（跨启动持久）；**Missing / Corrupt / SizeMismatch 删盘重拷** 母盘。开发迭代期替换 resfile 的 golden 后，旧的 working disk 因虚拟容量不符（SizeMismatch）会被自动重拷；若容量恰好相同则仍会保留旧系统——故以"替换 golden 后清沙箱或 `hdc uninstall`"为过渡手段，**内容指纹**仍是待办（见 4.6.4）。
6. **无 initramfs、无 switch_root、无 overlay、无运行时空盘代码生成、无首启解包**：内核 `root=/dev/vda rw init=/usr/lib/qemu-init/init` 直接把**盘内系统**引导为 PID 1（见 4.6.4）。guest 系统层在构建期已写入盘，运行期零解包。
7. **不用 tools.tar.zst**；guest 内工具随系统层烧入（Alpine 自带 busybox applet 具备 mount/ip/date/md5sum/pkill 等），hicodeerd 经 sandbox 从 host staging 提供（见 4.6.4）。
8. **guest 系统一律改用 Alpine**：musl + OpenRC + busybox。换代动机是**性能**——旧 openEuler 用户态在 TCG 下 exec 密集负载的 78% 时间花在 sys，换成 musl/busybox 后 B1 基准快 1.62×（见七）。
9. **LSP 下载 libc 探测随后端**：`ldd --version` 经 `util::command` 已转发到当前后端（guest 系统层内置 glibc ldd / OHOS 本机）。
10. **动态 workdir 挂载沿用原逻辑**：挂载对象是 worktree 根普通绝对路径；对已被静态 share 覆盖的目录（sandbox、数据根）跳过，不占热插拔槽位。
11. **Terminal 探测式选后端 + 真 pty**：新建终端不读 QEMU 开关，先 `open_remote_shell` 探测；能连即用当前后端守护进程的 pty，失败回退本机 `/bin/sh`。守护进程支持 pty-req 会话（guest/host 内 openpty + `/bin/sh` 交互 + 双向桥 + resize）。**pty slave 必须 O_RDWR 打开**，否则 shell 写 stdout EBADF → 会话成功却零回显。
12. **guest 时间同步**：guest 就绪后经 cmd-client 推一次墙钟——先 `ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime` 再 `date -s @epoch`，对"守护进程未就绪"按 2 s 间隔重试至多 60 次。
13. **virtiofsd fd 耗尽修复**：guest 内守护进程内置周期任务每 **15 s** 写 `/proc/sys/vm/drop_caches=2`（`HICODEERD_DROP_CACHES` 控制），触发 FUSE_FORGET 释放 O_PATH fd。
14. **qemuctrl 只保留 QEMU 管理**：删除执行命令/SSH/virtio-serial 代码，不留残留；guest 侧命令一律经 cmd-client。
15. **guest init 自愈契约**：盘内 `/usr/lib/qemu-init/init` 为 PID 1；任何阶段失败以干净收尾（见 4.6.4）。宿主侧 provision 每启动判工作盘状态，损坏即从母盘恢复。
16. **QEMU 退出后引擎自动重启（事件驱动，无监督线程）**：guest poweroff/异常退出后引擎在同一线程 relaunch（见 4.5.1），并带崩溃循环熔断（60 s 窗口内 3 次快速退出即放弃）。
17. **libqemu .so 一经 dlopen 终身不 dlclose**：dlclose 后引擎线程退出时 musl TLS 析构会跳进已卸载代码段 → SIGSEGV 杀死整个 App。句柄有意泄漏，与进程同生命周期；重启只 relaunch QEMU main。
18. **guest 守护进程由盘内监督壳自愈**（2026-09-12 新增）：`S30cmd-daemon` 用子 shell 监督循环拉起守护进程，1 s 固定重试；**脚本名刻意不含守护进程名**，否则 `pkill -f <守护进程名>` 会把监督者一起杀掉，留下无人复活的死局（见 4.6.4）。
19. **用户数据根挂进 guest**（2026-09-12 新增）：用户选定的数据根（`custom_data_dir` 记录）持有 Linux 版 Node 运行时、下载的 language server 与 debug adapter，guest 必须在**原始 host 绝对路径**上执行它们。当该目录位于沙箱之外时，为它起**第二个静态 share**（tag `customer_data`），路径经 `files/qemu/guest-conf/customer_data_path` 告知 guest。
20. **Node `os.userInfo()` 垫片：内嵌 + 数据根对账自愈**（2026-09-12 修订）：垫片脚本用 `include_str!` **编进守护进程二进制**，启动时（`passwd_shim::install`）定位用户数据根、把脚本对账写盘到 `<数据根>/node/shim/osuser-shim.js`，再以 `NODE_OPTIONS=--require <该路径>` **无条件**交给所有派生的 Node 子进程；垫片先调真实实现、仅在失败时兜底，故在查得到用户的宿主上是无副作用的空操作。选数据根是因为它由宿主与 guest **同名同址**可见，垫片与守护进程跑在哪一侧无关。派生子进程前另做一次 `stat` 兜底：数据根同时是 Node 的 scratch 区，托管运行时重下 Node 时会 `remove_dir_all(<数据根>/node)`，垫片一旦被连坐删除立即重写（否则 `NODE_OPTIONS` 指向不存在的文件会让 Node 直接起不来）。
21. **guest-init 变更即重建 golden**（2026-09-12 新增）：`bundle-ohos` 用 mtime 比较 `guest-init/` 与 `golden.qcow2`，前者更新则自动重建母盘（实测 14 ms/次），避免"改了引导脚本却装了个带旧脚本的母盘"。
22. **引擎不裁剪 HiSH 的启动线**：`-M`/`-cpu`/`-rtc`/`-overcommit`/磁盘与网卡选型整体取自 HiSH 的参考启动线，只补充本工程必需项（sandbox/customer_data share、8 个 pcie-root-port、RNG 熵源），并在常量旁逐条记录"为什么保留/为什么不照搬"。

## 三、目标架构

```
HiCodeer 进程（HarmonyOS App, el2 sandbox, base_path=/data/storage/el2/base/haps/entry/files）
  launch_app (launch-zed/qemu_runtime.rs::start_command_backend)
    ├─ 关 QEMU：cmd_client::Endpoint::ohos_default() → OHOS hicodeerd 127.0.0.1:4022/4023
    └─ 开 QEMU：
          1) provision_guest_files：resfile → base_path/qemu/{bin/hicodeerd, guest-conf/, ports/, disk.qcow2}
             disk.qcow2 = golden.qcow2 的副本（Valid 保留 / Missing/损坏/SizeMismatch 重拷）
             guest-conf/customer_data_path = 用户数据根绝对路径（仅当它在 sandbox 之外）
          2) qemu_manager::start_with_restart(paths, cfg, hook)  [主线程起，qemu-machine 引擎线程跑 guest]
          3) cmd_client::Endpoint::qemu_guest() → host 127.0.0.1:4122(cmd)/4123(mgmt)
  cmd-client 全局 executor；QEMU 模式外层包 WorkdirAwareExecutor（spawn 按 cwd 懒挂载）

QEMU 引擎线程（dlopen libqemu-system-aarch64.so; QEMU 10.2.0; slirp NAT; guest eth0 10.0.2.15/24）
  argv 关键项：-M virt,memory-backend=mem,gic-version=max,... + -cpu max,pauth-impdef=on,sve=off,pmu=off
              -smp cpus=N,sockets=1,cores=N,threads=1  -accel tcg,thread=multi,tb-size=2048
              -object memory-backend-memfd,id=mem,size=<mem>G  -rtc base=utc,clock=host
              -kernel Image + -append "console=ttyAMA0,115200 root=/dev/vda rw init=/usr/lib/qemu-init/init mitigations=off TERM=xterm"
              -drive disk.qcow2(virtio-blk=/dev/vda, iothread) + 静态 hostfwd 4122/4123
              + sandbox [customer_data] virtiofs + virtio-rng + pcie-root-port×8
  guest kernel 6.12.60（HiSH arm64_virt 基线 + 5 项能力 + 6 项性能增量 + n_tty_resize 补丁）
  guest 根盘 /dev/vda = golden 工作盘（Alpine 3.22 用户态 + busybox + qemu-init）
    PID 1 = /usr/lib/qemu-init/init（跑 rcS → S00mount/S10sandbox/S12data/S30cmd-daemon，然后空转保活）
  virtio-fs tag sandbox: host base_path/ → guest 同名（静态；hicodeerd/guest-conf 由此进入 guest）
  virtio-fs tag customer_data: host 数据根 → guest 同名（静态；仅当数据根在 sandbox 之外）
  动态 workdir：host 目录经 tag workN virtiofs 挂 guest 同名（只挂不卸，共 8 槽）
  hostfwd: tcp:127.0.0.1:4122-:4022 / tcp:127.0.0.1:4123-:4023
```

## 四、分模块设计

### 4.1 Settings 数据模型/读取器

- `crates/settings_content/src/qemu.rs`：`QemuCpuCores(1..10)`（`#[default] Cpu1`）、`QemuMemGb(4/6/8/10/12)`（`#[default] Mem4`）、`QemuDiskGb(64/96/128/256/512)`。
- `crates/settings/src/settings.rs`：读取器 `unwrap_or` 缺省值同步为 `Cpu1` / `Mem4` / `Disk128`。
- `assets/settings/default.json`：`qemu_enabled=true`、`qemu_cpu_cores="cpu1"`、`qemu_mem_gb="mem4"`、`qemu_disk_gb="disk128"`。
- `launch-zed/qemu_runtime.rs::LaunchQemuSettings::default()`：`enabled=true, cores=1, mem_gb=4, disk_gb=128`（启动早于 SettingsStore 初始化，故自行读 settings.json，读不到就用这套默认值）。
- **五处默认值必须一致**，否则"代码默认单核、用户设置 4 核"这类漂移会让人误判性能数据出处。

### 4.2 settings_ui 页面/渲染器

- `languages_and_tools_page` 置顶 QEMU 节 4 控件（enable / cores / mem / disk；disk 注明"变更会重建 guest 磁盘，重启生效"）。

### 4.3 cmd-client 端点参数化

- `CommandEndpoint{mgmt_host,mgmt_port,command_host,command_port}`；bootstrap mgmt 拿 SshInfo → pool 连 command 端口。
- `endpoint.rs`：`qemu_guest()` = host 127.0.0.1 **4123/4122**（QEMU 模式），`ohos_default()` = 127.0.0.1 **4023/4022**（OHOS 模式）。注意 4122/4123 是**host 侧**端口，guest 内部守护进程仍听 4022/4023。
- `executor.rs`：`run_shell(cmd)`（一次性命令，供时间同步与 guest 侧挂载脚本用，避免与全局 executor 重入）。

### 4.4 guest/本机守护进程（hicodeerd）

`crates/gpui_ohos/depend/cmd-agent/hicodeerd/`，源码含 `main.rs`（bind/conf/drop_caches/`--log`）、`sshd.rs`（pty/exec/data/window_change）、`pty.rs`（slave O_RDWR）、`management.rs`、`passwd_shim.rs`、`shim/osuser-shim.js`。

- **双目标编译**（`bundle-ohos`）：
  - OHOS 版（`aarch64-unknown-linux-ohos`）→ 装进 `hicodeerd.hnp`（public HNP），服务设备本机命令通道；包内**不再附** `osuser-shim.js`（垫片已内嵌进二进制，运行时落到数据根）。
  - guest 版（`aarch64-unknown-linux-gnu` + `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static"`）→ **静态链接**，因为 guest 用户态是 Alpine musl，没有 glibc 供动态二进制解析。实测产物 5.22 MB、`statically linked`、无 `PT_INTERP`、无 `.dynamic`。
- **bind 与配置走环境变量**：`HICODEERD_BIND_ADDR`（guest 设 `0.0.0.0`，缺省 127.0.0.1）、`HICODEERD_CONF_DIR`（指向 sandbox 内 host staging 的 `guest-conf`）、`HICODEERD_DROP_CACHES=1`（开启 15 s 周期 `drop_caches=2`，释放 virtiofsd 的 O_PATH fd）。代码无 ohos 专属 cfg、logger 非 ohos 走 stderr，故同一份源码可编两个目标。
- **双套 mgmt 钥**：OHOS 套与 guest 套各自 `ssh-keygen`（ed25519）。服务端半边（`mgmt_host_key` / `authorized_keys`）给对应守护进程用；客户端半边（`mgmt-host.pub` / `mgmt-client-key`）给 cmd-client 读。两套都在每次构建时重生成，保证"设备上残留的旧钥永不匹配"。
- **`os.userInfo()` 垫片：内嵌 + 数据根自愈**（决策 20）：`passwd_shim::install(&conf)` 定位数据根、把内嵌脚本对账写到 `<数据根>/node/shim/osuser-shim.js`，并缓存 `NODE_OPTIONS=--require <该路径>`，交给守护进程派生的所有 Node 子进程继承。数据根三级定位：`HICODEERD_DATA_ROOT` 环境变量 → `<conf>/customer_data_path` 记录 → `<conf>/../../hicodeer`（默认数据根）；都拿不到时退到 `<conf>/osuser-shim.js`。用户在 `HICODEERD_OSUSER` 可覆盖上报的用户名（缺省 `hicodeer`）。安全栏：路径含空白 / 非 UTF-8 / 无处可写 ⇒ 不注入（**绝不**把无效 `--require` 塞进 `NODE_OPTIONS`）。背景：OHOS 上应用/服务 uid 不在 `/etc/passwd`，libuv `uv_os_get_passwd` 返回 ENOENT，`os.userInfo()` 抛 `ERR_SYSTEM_ERROR`，ACP agent 在拼请求上下文时必炸。
- **pty slave 必须 O_RDWR**：`File::open` 默认 O_RDONLY，子 shell 写 stdout 得 EBADF，表现为"pty 连上但屏幕空"。

### 4.5 qemuctrl（引擎管理）

`crates/gpui_ohos/depend/qemu-mngt/qemuctrl`（crate `qemu-manager`）：

- `lib.rs`：常量 + `start_with_restart` + `prepare_reboot` + `build_argv`；`QemuPaths{kernel, initrd, port_dir, sandbox_mount, data_mount, disk_path}`、`QemuConfig{cores, mem_gb, disk_gb}`。
- **引擎常量（全部取自 HiSH 参考启动线，逐条备注）**：
  - `MACHINE_TYPE="virt"`；`MACHINE_OPTIONS="memory-backend=mem,gic-version=max,iommu=none,usb=off,virtualization=off,compact-highmem=on,dump-guest-core=off,mem-merge=off,hmat=off"`。
  - `CPU_MODEL="max,pauth-impdef=on,sve=off,pmu=off"`；`SMP_SOCKETS=1`、`SMP_THREADS_PER_CORE=1`。
  - `TCG_TB_SIZE_MB=2048`（翻译缓存，地址空间而非常驻内存）；`RTC_OPTIONS="base=utc,clock=host"`；`OVERCOMMIT_OPTIONS="cpu-pm=off"`。
  - `DISK_DRIVE_OPTIONS="cache=writeback,aio=threads,discard=unmap"`；`DISK_IOTHREAD_ID="iothread0"`（专用块 IO 线程）。
  - `RNG_BACKEND_OPTION`/`RNG_DEVICE_OPTION`：`-object rng-random,filename=/dev/urandom` + `-device virtio-rng-pci-non-transitional`，让 guest 的 CRNG 尽早播种（首个消费者是 TLS 握手）。
  - 端口常量 `GUEST_{COMMAND,MANAGEMENT}_PORT=4022/4023`、`HOST_{COMMAND,MANAGEMENT}_PORT=4122/4123`。
  - `MOUNT_TAG_SANDBOX="sandbox"`、`MOUNT_TAG_DATA="customer_data"`、`FS_SOCKET_{SANDBOX,DATA}`、`FS_WORK_PREFIX="fs_work"`、`QMP_SOCKET="qmp.sock"`、`WORKDIR_MOUNT_SLOTS=8`。
- **cmdline**：`console=ttyAMA0,115200 root=/dev/vda rw init=/usr/lib/qemu-init/init mitigations=off TERM=xterm`——无 initramfs（无 `-initrd`）、无 switch_root。相对 HiSH 的启动线去掉 `kpti=off`（本内核 `UNMAP_KERNEL_AT_EL0=n`，参数未注册）与 `init_on_alloc=1`（本内核默认关，传了反而开启零化、拖慢 guest），以及一批与本 config 相比恒为默认值的项。**无 host-RAM 启动门槛**（guest RAM 是 lazy memfd，不需要整块空闲）。
- `engine.rs`：dlopen/dlsym(main) 专用线程（线程名 `qemu-machine`）跑 guest；**不 dlclose**；`engine::is_running()` 供幂等判断；串口转发仅在 `qemu_debug_assertions` feature 下开。
- `virtiofs.rs`：静态 sandbox backend + 按需的 customer_data backend + 动态 workdir backend；每个 share 一个 `virtiofsd-<tag>` 线程，走 `virtiofsd` crate 库内调用（无 CLI、无 seccomp）。
- `mount.rs`：动态 workdir 三段式（backend → QMP `device_add` → guest `mkdir` + `mount -t virtiofs`，重试 10 × 0.3 s）；`MountRegistry` 去重 + 槽位记录；失败即"烧槽"；guest 重启时 `reset()` 清空。
- `qmp.rs`：仅 `device_add`（热插拔）用。
- 文件共享 = 同名路径：host `files/` → guest 同名 `/data/storage/el2/base/haps/entry/files`，cmd-client 的 host 绝对 cwd 在 guest 无需路径翻译。

#### 4.5.1 引擎退出感知与自动重启（事件驱动）

- 崩溃根因（dlclose）与"不 dlclose"见决策 17。
- 重启 = 事件驱动：engine 线程跑完一轮 `main` 后调注入 restart hook：`Some(new_argv)` → 记 `relaunching guest after exit` → 立即 relaunch；`None` → 线程退出。
- `make_restart_hook(base_path)`（qemu_runtime.rs）每轮重启都重跑 `provision_guest_files`（盘校验/从母盘恢复）+ `prepare_reboot`（重启 virtiofs backend + 重置 mounts 注册表 + 重出 argv），实现"每次重启都自愈"。
- **崩溃循环熔断**：60 s 窗口内累计 3 次快速退出即 `giving up relaunch`；每轮固定 3 s 退避；重启前重新读 settings，若用户已关 QEMU 则不再重启。
- 边界：guest 主动 poweroff 亦走同一路径（视作显式重启语义）。

### 4.6 guest 镜像（引擎 + 内核 + 根文件系统 + 引导脚本）

构建期在 host（aarch64 openEuler VM）完成，产物落入 HAP resfile/libs，由 `bundle-ohos` 打包。

#### 4.6.1 引擎（`images/libqemu-system-aarch64.so`）

- 来源：HiSH release。**QEMU 10.2.0**，52.5 MB；`DT_NEEDED` 只剩 `libslirp.so.0`、`libz.so`、`libc.so`——glib / pixman / pcre2 / intl 已内联，不再需要外挂 5 个 so。
- 导出入口：同时导出 `main` 与 `qemu_system_entry`，函数体等价，故 `engine.rs` 的 `dlsym(b"main\0")` 无需改动。
- `libz.so` 的处理：引擎要的是**无版本后缀**的 `libz.so`，而设备 `/system/lib64` 下没有 zlib。做法是把既有 `libz.so.1` 的 `DT_SONAME` 重写为 `libz.so` 另存；`libhicodeer.so` 不依赖 zlib（`readelf -d` 已核），故删掉旧 `libz.so.1` 安全。

#### 4.6.2 内核（`images/Image`，linux 6.12.60）

- 基线 = HiSH 的 **`arm64_virt`** 配置（从其发布内核的**内嵌 ikconfig** 提取，与 HiSH `mkimg.sh` 所用逐字节一致，归档为 `images/arm64_virt.base.config`）+ HiSH 的 `KCFLAGS` + `0001-n_tty_resize.patch`（归档为 `images/n_tty_resize.patch`）。工具链 clang 17.0.6 + LLVM，产物 `Image` 13.29 MB。完整复现步骤见 `images/kernel-build.md`。
- **能力增量 5 项（缺一不可）**：`FUSE_FS`、`VIRTIO_FS`（virtio-fs 是 FUSE 协议）、`PCIEPORTBUS`、`HOTPLUG_PCI`、`HOTPLUG_PCI_PCIE`（`pcie-root-port` 热插拔）。
- **性能增量 6 项（HiSH 为小内存 guest 做的牺牲，在 TCG 下是纯损失）**：`SLUB_TINY=n`、`ARM64_HW_AFDBM=y`、`ARM64_TLB_RANGE=y`、`TRANSPARENT_HUGEPAGE(_ALWAYS)=y`、`HIGH_RES_TIMERS=y`、`NR_CPUS=32`。
- `KCFLAGS='-march=armv8.5-a+crc+crypto+lse+rcpc+rng+sm4+sha3+dotprod+fp16 …'`：让 clang 直接发扩展指令，避免内核 `alternative` 打补丁路径——`+lse` 在 TCG 下把 `ldxr/stxr` LL/SC 重试循环换成单条原子指令。

#### 4.6.3 根文件系统与 golden 母盘（`images/alpine-rootfs.qcow2` + `goldendisk/build-golden.sh`）

- 基底：HiSH 的 `rootfs_aarch64.qcow2`，即 **Alpine Linux 3.22**（musl + OpenRC + busybox，128 GiB virtual），重压为 18.2 MB。
- **机制**：母盘 = 已完整落好系统层的 qcow2，构建期一次性生成，随 HAP resfile 分发（`resfile/qemu-guest/golden.qcow2`，实测 **qcow2 v3 / 64 KiB cluster / 128 GiB virtual / 23.5 MB 压缩**），运行期只读；每次启动复制为工作盘。
- **生成步骤**（`build-golden.sh <OUT_QCOW2> <CACHE_ROOT>`，全流程每步失败即 abort）：
  1. `qemu-img convert` 把 Alpine 基底展开为稀疏 raw（`<CACHE_ROOT>/golden.raw`）；
  2. `sudo mount -o loop` 挂载；**先 `rm -rf` 镜像内 `/usr/lib/qemu-init/` 再注入**（复用 raw 时旧脚本会残留），注入清单**由 `guest-init/` 目录驱动**而非硬编码列表（新增脚本必须自动落盘，否则"构建成功但引导缺脚本"）；
  3. **补 glibc 运行时**：`ld-linux-aarch64.so.1` + `libc.so.6`/`libm.so.6`/`libpthread.so.0`/`libgcc_s.so.1` 等 17 个库 + `libstdc++.so.6`。原因：host 侧预编译的 language server 并非都认识 musl——`rust-analyzer` 运行时探测 libc 类型（`crates/languages/src/rust.rs`）会正确选 musl 版，而 `json`/`python` 适配器**硬编码** `unknown-linux-gnu`（glibc）资源。glibc 与 musl 可共存（loader 与 libc 的 soname 不同），故既保住那两个 server，又保住 musl 用户态的速度；
  4. **注入 tzdata**（Asia/Shanghai）：Alpine 基底不带时区库，缺了则运行时 `ln -sf … /etc/localtime` 是悬空链接、guest 打印 UTC；
  5. 卸载后 `qemu-img convert -c` 压成 qcow2 并安装到目标路径。
- 端到端实测 **5.3 s**；缓存目录 `<RUST_TARGET_ROOT>/<hash>-golden/`（`golden.raw` 悬留即跳过解包）。
- **运行期 provision（qemu_runtime.rs）**：读 `golden_virtual_size`（qcow2 头 offset 24）→ `inspect_qcow2(disk, size)` 判工作盘：`Valid` 保留 / `Missing` / `Corrupt` / `SizeMismatch` → `remove_file(disk)`（**容忍 NotFound**，全新首启的正常态）+ `copy golden → disk`。`inspect_qcow2` 除头部字段外还校验**覆盖头部簇的 refcount**——旧版曾出现"4 字节条目写在 qcow2 v2 的 2 字节位上"导致头簇被标为空闲、QEMU 在其上分配 L2 表的砖化案例。
- **待办（未固化）**：golden 尚无**内容指纹**（如 `sha256sum`），工作盘 `Valid` 判据只比虚拟容量，故"换母盘但容量相同"仍会保留旧系统。目标：在 golden 旁生成指纹，使判据升级为"指纹与当前 golden 一致才保留"。

#### 4.6.4 引导与盘内结构

**无 initramfs**。内核直接挂 `/dev/vda`（golden 工作盘）为根，把盘内 `/usr/lib/qemu-init/init` 拉起为 PID 1：

```
/usr/lib/qemu-init/
  init           # PID1：export PATH → 跑 rcS → while(:) sleep 3600 空转保活（PID1 退出即 kernel panic）
  rcS            # busybox applet 兜底（command -v mount 判假才 --install -s）；配 eth0 静态 IP + 默认路由；
                 #   然后遍历执行 S??*（<script> start）
  S00mount       # 挂 proc/sys/devtmpfs/devpts/run/tmp，另加 /dev/shm tmpfs + chmod 1777（node/rustc 需要真 POSIX shm）
  S10sandbox     # mkdir + mount -t virtiofs sandbox /data/storage/el2/base/haps/entry/files（同名，host files/）
                 #   + 建 sandbox/{home, qemu/logs, qemu/guest-conf}
  S12data        # 读 guest-conf/customer_data_path；有则 mkdir + mount -t virtiofs customer_data <该路径>；
                 #   无记录即 exit 0（数据根在 sandbox 内时无需第二 share）
  S30cmd-daemon  # 子 shell 监督循环起 guest 守护进程（见下）
```

要点：

- **eth0 必须在 rcS 里配静态 IP**（slirp 无 DHCP）：`ip link set eth0 up`、`ip addr add 10.0.2.15/24`、`ip route add default via 10.0.2.2`；缺了 hostfwd 投递不到 guest，host 连 4123 表现为 **timed out**。
- **守护进程不在盘内烧录，经 sandbox 从 host staging 提供**：host `base_path/qemu/bin/hicodeerd`（resfile 拷入）→ sandbox virtiofs → guest 同名路径执行。改守护进程只需重拷 staging，不必重建母盘。
- **`init=` 覆盖 Alpine 自己的 `/init`**：OpenRC、getty、以及 `/etc/fstab` 里那条 `hostshare /mnt/share 9p` 都不会执行——既避免沙箱内 getty 反复失败，也使 9p 挂载需求消失。
- **`S30cmd-daemon` 的监督自愈（决策 18）**：脚本用 `( while [ ! -f /run/daemon.stop ]; do … "$DAEMON_BIN"; sleep 1; done ) &` 拉起守护进程，并在每次启动/退出打印 `guest daemon starting` / `guest daemon exited; restarting in 1s`（守护进程以**前台**方式跑在监督循环里，这些行直接落 guest 串口 ttyAMA0、再进 hilog；不带 `--log`、不写日志文件，避免污染被 watch 的工作区）。
  - **脚本名不含守护进程名**：否则 `pkill -f hicodeerd` 会同时匹配到监督壳 `/bin/sh /usr/lib/qemu-init/S30cmd-daemon start`，把监督者一起杀掉，守护进程永不再起（这正是 2026-09-12 之前"kill 后不自动重启"的根因）。
  - 守护进程环境：`HICODEERD_BIND_ADDR=0.0.0.0`、`HICODEERD_CONF_DIR=<sandbox>/qemu/guest-conf`、`HICODEERD_DROP_CACHES=1`、`HOME=/home`（并 `mkdir -p`，agent 会在此写状态）、`SHELL=/bin/sh`；`SSL_CERT_FILE` 按候选路径**存在才 export**（Alpine 用 `/etc/ssl/cert.pem`；指向不存在的路径会让所有 TLS 客户端失败）。

### 4.7 launch 分流 + qemu_runtime + 动态挂载 + 终端 pty

- `launch-zed/qemu_runtime.rs`：读 settings.json → enabled 则 `provision_guest_files` + `QemuManager::start_with_restart`；按开关选 Endpoint+keys 构造 executor。
- staging：resfile → `base_path/qemu/{bin/hicodeerd, guest-conf/…, ports/}`；工作盘 disk.qcow2 = golden 副本。staging 含**强制覆盖**语义：guest 守护进程 bin 与 guest mgmt 密钥每次 `std::fs::copy` 覆盖（ed25519 文件定长，按大小跳过会留下旧钥 → `Unknown server key`）。
- **数据根 share（决策 19）**：`read_data_root(base_path)` 读 `<base_path>/custom_data_dir`；若路径不以 `base_path` 开头则填 `QemuPaths.data_mount` 并把路径写入 `guest-conf/customer_data_path`；否则**删除**可能残留的记录文件（否则 guest 会去找一个本次并未共享的目录）。`covered_roots = [sandbox_mount] + data_mount`，两者覆盖的目录一律不走懒挂载（不烧热插拔槽）。
- 动态挂载：`WorkdirAwareExecutor` 在 spawn 与 `open_shell_pty` 时按 cwd 懒挂（静态覆盖根内跳过；否则向上找最近的含 `.git` 的祖先作挂载根；backend+QMP+guest mount 三件套；只挂不卸）。
- 终端 pty：守护进程 pty-req/openpty（**slave O_RDWR**）；新终端探测式连接、失败回退本机。
- 时间同步：独立 `qemu-time-sync` 线程，最多 60 次 × 2 s。

### 4.8 bundle/HAP/权限

- `bundle-ohos`（`script/bundle-ohos`）：
  - 编译 OHOS 版守护进程 → `hnpcli pack` 出 `hicodeerd.hnp`（public，`conf/` 内含服务端密钥），签名前注入 HAP；
  - 编译 guest 版守护进程（gnu + `crt-static`）→ `resfile/qemu-guest/hicodeerd`；
  - guest mgmt 钥每次构建重生成 → `resfile/hicodeerd-mgmt-guest/{mgmt-host.pub, mgmt-client-key, mgmt_host_key, authorized_keys}`（OHOS 套 → `resfile/hicodeerd-mgmt/`）；
  - 内核 `images/Image` → `resfile/Image`；引擎 so 三件（`libqemu-system-aarch64.so`、`libslirp.so.0`、`libz.so`）→ `hap/entry/libs/arm64-v8a/`；
  - **golden 依赖检测（决策 21）**：`find guest-init -type f -newer golden.qcow2` 非空即自动重建，`--rebuild-golden` 强制重建，构建失败立即 abort（绝不发出旧母盘）；
  - `libhicodeer.so` 入 HAP 前 strip（保留 unstrip 副本供 addr2line 符号化）。
- `module.json5`：`hnpPackages` 声明 `openssh.hnp` / `git.hnp` / `curl.hnp`（private）与 `hicodeerd.hnp`（public）；`requestPermissions` = `FILE_ACCESS_PERSIST` / `INTERNET` / `kernel.ALLOW_WRITABLE_CODE_MEMORY`（TCG 要 JIT 生成可执行内存）。加载引擎 .so 依赖 `LOAD_INDEPENDENT_LIBRARY` / `IGNORE_LIBRARY_VALIDATION` 一类系统级放行（见移植向导第 17 章）。

### 4.9 实现约束（工程红线）

- 非 ohos 文件禁止自行修改；全路径不含 `ohos` 的改动一律 `#[cfg(target_env="ohos")]` 包裹、大段新代码封装成函数/独立文件。
- 不裁剪功能、不屏蔽/删减代码解决编译/运行问题；禁空函数/桩函数。
- 代码注释只英文；禁魔鬼数字；新增代码带分级日志（异常分支必有 error）。
- 禁止 clean / 删除目录 / 卸载程序；构建仅走 `script/bundle-ohos`。

## 五、Terminal → guest 切换（验收主线）分层设计

验收标准：新建 Terminal 连上 QEMU(guest) 的 shell，而不是 OHOS 的 /bin/sh。

1. **服务器 pty（hicodeerd）**：`pty.rs`（openpty；**slave 以 O_RDWR 打开**——O_RDONLY 会让 shell 写 stdout EBADF → 会话成功却零回显）spawn `/bin/sh -c <exec>` 挂 tty（setsid + TIOCSCTTY），master 双向 relay + `window_change` → `TIOCSWINSZ`。`sshd.rs`：`pty_request` 存尺寸、`exec_request` 当通道请求过 pty 时改走 `run_pty_shell`、`data()` 先查 pty master、`window_change_request` → resize。
2. **cmd-client RemotePty**：`open_shell_pty(cols,rows)`——allocate 连接 → `pty-req` → exec shell → 暴露读/写/`resize`。
3. **terminal 适配（crates/terminal，ohos cfg）**：新建 Terminal 先探测 `open_remote_shell`；成功用"本地 pty 作 master 给 alacritty + 后台线程桥接 guest RemotePty"（本地窗口 resize 同时下发 guest）；失败回退本机 /bin/sh。
4. **前置**：guest 能起（工作盘 provision + 引导 + 守护进程连上）。

## 六、落地顺序与验证

1. cmd-client 端点参数化 / 守护进程 bind addr / gnu 编译 → host 起 gnu 守护进程连 4022/4023。✅
2. qemuctrl 裁剪 + argv（直挂 /dev/vda、无 initramfs）→ OHOS 编译过。✅
3. **guest 引导链**：清沙箱首启（Missing → 拷 golden → 直启 /dev/vda → 盘内 init → sandbox 挂载 → 守护进程起 → cmd-client bootstrap 4123 通）。✅
4. 终端 pty：guest shell 可交互、命令回显正常。✅
5. bundle-ohos：出 HAP 解包核对 resfile/libs 内容；装设备。✅
6. launch 分流：关 QEMU OHOS 回归；开 QEMU git/LSP/terminal 走 guest。✅（2026-09-12 装机复验）
7. 动态挂载：files 外仓库首次命令触发 workN 挂载。✅（含"启动早期预热挂载因守护进程未就绪而烧槽"的已知行为）
8. **HiSH 换代**：引擎/内核/rootfs 替换 + 增量核对 + 装机。✅（见 `QEMU-HiSH-替换方案.md` §7）
9. **改名收尾**：`zcoderd` → `hicodeerd`，含 guest 引导脚本、环境变量、resfile 子目录、HNP 包名。✅
10. **数据根挂载**：`customer_data` 第二 share + `customer_data_path` + `covered_roots`。✅（hilog `data root mounted at …: languages node debug_adapters`）
11. **守护进程自愈**：`S30cmd-daemon` 监督循环 + 1 s 重试。✅（宿主侧实验：pkill 后监听者存活、守护进程 1 s 内以新 PID 复活）**
12. **golden 依赖检测**：guest-init 比 golden 新即自动重建。✅（实测 14 ms）
13. **golden 生成内容指纹**。（待办，见 4.6.3）

验证观察点（hilog）：`[qemu-boot]` provision 阶段、`[qemu-console]` guest 串口、`[qemu-init]` sandbox mounted / data root mounted / guest daemon starting|exited、`qemu_manager::start` 引擎幂等与丢弃旧注册表、`relaunching guest after exit`、drop_caches 周期、cmd-client bootstrap `reconfiguring pool to 127.0.0.1:4122`、`guest clock synced`、`guest shell ready` / `open_guest_pty`、`pty: interactive shell started`。

> **仍需主人配合的一次端到端验证**：在**应用终端**里执行 `killall hicodeerd`，观察守护进程在 1 s 内复活（该操作在应用沙箱内，助手侧 uid 无法从外部触发）。

## 七、guest 性能实测与约束

### 7.1 结论先行

- **头号瓶颈是进程创建/exec，不是文件 IO**。guest 内每次 exec 约 37~64 ms，真机同负载约 1 ms。
- **加核不是灵药**：引擎是 MTTCG，而 guest 侧工作流串行，多 vCPU 只付同步税（屏障翻译、TLB shootdown/IPI、BQL 与 virtio 队列争用、TB 缓存按 vCPU 分片，再叠加宿主抢核）。默认档位因此定为**单核**。
- **换代 HiSH 栈（Alpine/musl + 6.12.60 内核）是当前最大的一次性能收益**：同设备同负载 B1 由 45.23 s → 27.96 s（1.62×），收益主要来自 sys 段。

### 7.2 负载定义（脚本 `qemu_perf_release.sh`）

- B1 exec 密集：`2000 × md5sum`（debug 基线读真文件 `/bin/sh`，release 基线读 `/dev/null`，两者不可直接相减归因）
- B2 io/copy 密集：先建 800 个小文件（每个文件额外 exec 一次 `head`），再 `cp -a` 该目录
- B3 cpu 密集：200 万次 shell 整数加法（≈纯 TCG 热循环翻译，除启动外无 syscall）
- 计时用 bash 内建 `time`/`TIMEFORMAT`，不依赖 guest 的 `date +%N`

### 7.3 HiSH 换代前后对照（同设备、同 release 构建、4 核）

| 基准 | 负载 | 旧（openEuler + 6.18.7） | 新（Alpine + 6.12.60） | 变化 |
| --- | --- | --- | --- | --- |
| B1 | 2000 × `md5sum /dev/null` | 45.232 s（user 15.726 / sys 35.533） | **27.96 s** | **1.62×** |
| B2a | 建 800 个小文件 | 未测 | 12.81 s | — |
| B2b | `cp -a` 该目录 | 未测 | 0.19 s | — |
| B3 | 200 万次 shell 加法 | 未测 | 74.25 s | — |

B1 复测一次 27.63 s（相差 <1.5%，噪声内）。旧基线 **78% 时间在 sys**，正是 exec 的 syscall 开销——这是换 musl/busybox 后收益最大的那一块。

### 7.4 通用模型：慢在 TCG 翻译 syscall/内核路径

- 顺序大块吞吐正常（guest 内 `/dev/vda` 读 284 / 写 130、virtiofs 写 179 MB/s）→ 磁盘参数（qcow2/压缩/cache）、线程、压缩盘**不是顺序 IO 瓶颈**。
- exec 密集 `user+sys ≈ real`（满载，非 IO 等待），且 `sys > user`：每次 fork/exec 走 guest 内核 clone/execve/mmap 装载，这些内核路径在 TCG 下逐条翻译执行，被放大约 **70×**。
- **判定口诀**：guest 快慢取决于「每单位工作的 syscall/翻译密度」，而非磁盘吞吐。dd 每 1 MiB 一次 syscall → 摊薄而快；小文件/进程/exec 密集负载（git、apk、cp 大目录、编译）每文件几十次 syscall → 全部翻译 → 慢。guest 内 `top` 的 CPU% 是**虚拟 CPU 时间**，看不到 host 侧 TCG 翻译的真实墙钟。
- drop_caches（决策 13）每 15 s 清 guest dcache，令小文件元数据周期重读（叠加干扰，次要）。
- 工作盘继承 golden 压缩属性（`convert -c`）：随机小写命中既有压缩簇触发读回/解压/COW，为写盘慢的次因（顺序写新簇不受影响）。

### 7.5 核数对照（替换前 release 基线；结论对 TCG 通用）

同一台设备（host `nproc=16`、24 GiB RAM），仅改 settings 核数（内存固定 8G）：

| 基准 | 4 vCPU | 10 vCPU | 说明 |
| --- | --- | --- | --- |
| B1（exec） | 127.049 s | 74.204 s | 1.71×，exec 密集是唯一吃得动多核的负载 |
| B2 建文件 | 19.628 s | 31.873 s | 10 核反而更慢，疑为重启后 guest 后台任务干扰，不作为"加核反噬"证据 |
| B2 `cp -a` | 0.512 s | 0.582 s | 单进程完成、无 exec → 与核数无关 |
| B3（CPU） | 227.584 s | 188.268 s | 1.21×，单 shell 循环只占一个 vCPU，加核近乎无用 |

结论：核数是唯一有效杠杆但**收益亚线性**（guest 工作流串行，多核只帮宿主侧辅助线程铺开），且不应超过宿主物理核。默认单核；确有 exec 密集且存在真并行度的场景可上调。

### 7.6 硬件加速不可得（2026-09-10 调研）

- 设备无 `/dev/kvm`、无 vhost 节点/内核模块。
- OpenHarmony ARM64 官方 defconfig 默认关 `CONFIG_VIRTUALIZATION`/`CONFIG_KVM`（不作 hypervisor host、减攻击面）。
- HarmonyOS PC 虚拟化底层是 **HMV**（微内核 Harmony Virtualization；StratoVirt 为唯一特权公民，分区签名 + IPC 管控锁死），**非 Linux KVM**、无 `/dev/kvm`，标准 QEMU KVM 加速路径不可用；`@ohos.hypervisor` 无公开开发者文档，非第三方 hap 可访问。
- **TCG 软件模拟是唯一现实路径，上述 ~70× 是物理上限。**

### 7.7 已落地优化

- **串口不转发 hilog**：`launch-zed` 不带 `qemu_debug_assertions` feature → guest 串口不被转发（消除 vCPU 阻塞于写 pipe 与 hilog 写盘）；需要看 guest 引导日志时临时加上。
- `TCG_TB_SIZE_MB=2048`（HiSH 值，原 1024）：host RAM 换更少重翻译。
- **Alpine/musl + busybox + 6.12.60 内核**（见 7.3）。
- `-object iothread` + `virtio-blk-pci,iothread=`：块 IO 在自己的宿主线程完成，不让等盘的 vCPU 卡住其他 vCPU。
- 刻意**不采** HiSH 的 `poll-max-ns=2000000`：那会让 IO 线程在每次 IO 后空转，而宿主 CPU 在本工程是最稀缺资源。
- 刻意**不采** `virtio-scsi`+`scsi-hd`：`virtio-blk` 路径更短，且与已验证引导线一致。

### 7.8 策略指引（避免重复排查）

- QEMU 参数 / smp / cache / 压缩盘在顺序 IO 上均无性能问题，无需再调。
- 真实体感慢来自 TCG 翻译上限：git / 编译 / LSP / apk 等小文件密集负载在 guest 慢是常态；改善靠应用层减 syscall/exec 密度（长驻服务、合并命令），或接受现状。

## 八、风险与未决

- **【已解决】磁盘系统第二段 init 二次挂 sandbox 失败**：随 switch_root/initramfs 一起废弃——现单盘直启，sandbox 只在盘内 init 挂一次。
- **【已解决 2026-09-12】guest 守护进程被 kill 后不再起**：根因是监督壳命令行含守护进程名、`pkill -f` 连坐；改名 `S30cmd-daemon` 解决（决策 18）。
- **【已解决 2026-09-12】数据根未挂进 guest**：node/LSP/DAP 在 guest 内不可用；新增 `customer_data` 静态 share（决策 19）。
- **母盘生成无内容指纹**：换 golden 后若虚拟容量相同，旧工作盘仍会被保留（4.6.3 待办）。
- **`mount.rs` 失败即烧槽**：同一目录挂载失败后 `is_mounted()` 仍为 false，重试会占新槽（共 8 个）。启动早期的预热挂载在守护进程就绪前失败，会连烧 `work0..work3`；槽位耗尽后工作区永远挂不上。**属既有缺陷，未修**（候选修法：失败不标 occupied，改为允许对同一目录幂等重试）。
- **终端 probe 时序**：启动早期（guest 未就绪）打开的面板会 fallback `/bin/sh` 且不重试；后续新开面板即连 guest。可作为体验优化。
- **`bundle-ohos` 第 5 步注释失效**：注释称"空 qcow2 在运行期由 `qemu_runtime.rs::write_empty_qcow2` 生成"，该函数已不存在（现由 resfile 的 golden 复制为工作盘）；`QemuPaths.initrd` 字段同样只剩占位用途（指向不存在的 `rootfs.cpio.zst`）。**属注释/字段残留，未修**。
- **HAP 体积**：签后约 580 MiB（引擎 50 MiB + golden 23.5 MiB + `libhicodeer.so` 451 MiB），开发期可用 `--hap-only` / hdc 预推替代整包重装。
