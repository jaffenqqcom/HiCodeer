# HiCodeer 嵌入式 QEMU 访客设计

- 日期：2026-09-08（初稿）
- **2026-09-09 方案修订**：抛弃 initramfs + switch_root + overlay + 空盘首启灌系统；改为**内核直启磁盘系统 `root=/dev/vda` + golden.qcow2 母盘复制为工作盘**。
- **2026-09-12 方案修订**：guest 栈整体换代 **HiSH**——QEMU 引擎、内核、根文件系统、用户态工具全部换成 HiSH 的一套；`zcoderd` → `hicodeerd`（含环境变量、HNP 包名、resfile 子目录）；用户数据根作为第二个静态 share 挂进 guest；默认档位改为单核 / 4G / 128G；guest 守护进程加监督自愈；Node `os.userInfo` 垫片改为无条件注入；`bundle-ohos` 增加 guest-init 与 golden 的 mtime 依赖检测。
- **2026-09-22 全面重写（当前权威）**：按**当前代码**逐条校核，并**合并**此前分散的 QEMU 关联设计文档——原 `2026-09-01-ohos-qemu-ssh-agent-design.md`、`2026-09-02-virtio-fs-replace-9p-design.md`、`2026-09-03-ohos-qemu-DESIGN.md`、`2026-09-12-zcoderd-auto-restart-and-rename-design.md`、`2026-08-26-ohos-sandbox-vm-filesync-design.md` 已删除，其有效内容并入本文（废弃者存于「九、演化史」）。本次校正的关键事实：**QEMU 默认关闭**（编译期开关 + 运行期开关双层）、**无 Settings UI 页**、**垫片改为随 HNP 包分发**（不再是数据根的 `passwd_shim`）。
- 关联：`bugfix/2026-09-09-qemu-provision-missing-disk-abort.md`、`bugfix/2026-09-09-zcoderd-pty-slave-readonly.md`、`bugfix/2026-09-13-ohos-language-server-unavailable.md`、`design/2026-09-10-ohos-data-home-directory-design.md`（数据根位置）、`design/2026-09-14-ohos-data-root-layout.md`（数据根内部布局）、`qemu-mngt/QEMU-HiSH-替换方案.md`（换代方案）、`qemu-mngt/images/kernel-build.md`（内核重建）。

---

## 一、背景与目标

HarmonyOS 沙箱禁止 spawn 子进程。HiCodeer 的常规命令后端是设备侧的 `hicodeerd`（HNP 进程，`127.0.0.1:4022/4023`），由 `cmd-client` 转发 git / LSP / 终端命令。

本设计提供**另一条互斥的命令后端**：把 QEMU 引擎（`dlopen libqemu-system-aarch64.so`）集成进 HiCodeer，将 LSP 与 Terminal 迁移到 aarch64 Linux 访客机（guest）内，**去除对 HarmonyOS 命令行与本机命令守护进程的依赖**；命令执行由 guest 内的 `hicodeerd`（Linux aarch64 版）承担。

2026-09-12 进一步确立：guest 的全部底层组件（引擎、内核、根文件系统、工具集）以 **HiSH** 为基线，只做"能力补齐 + TCG 性能回还"两类最小增量——原因是 HiSH 的基座是为 512 MiB 单核 guest 调优的，而本 guest 是承载 git + language server + 终端的多核 TCG guest。详见 `qemu-mngt/QEMU-HiSH-替换方案.md`。

两条后端由**编译期开关**与**运行期开关**共同决定，二者都关时 QEMU 相关代码与资源完全不参与（见 4.8）。

---

## 二、设计决策

1. **cmd-client 同一时刻只连一个命令守护进程**：不并行、不热切，按启动时是否启用 QEMU 选定端点。
2. **guest 需要外网**：slirp NAT 出网；host 经**静态 hostfwd** 暴露 guest 4022/4023 为 host 侧 4122/4123。
3. **QEMU 默认关闭（原文档"默认开启"已作废）**：双层开关——`script/bundle-ohos:49` 的编译期常量 `ENABLE_QEMU_AGENT=false`（无 CLI 参数，须改常量重编）；运行期 `qemu_enabled`（`settings.json` 字段，`crates/settings_content/src/settings_content.rs:266-272` 明确 `Default: false`）。两者都开才启用 guest，且运行期改动**重启应用后生效**。默认档位单核 / 4 GB / 128 GB 磁盘。单核是默认值，因为 guest 是纯 TCG 模拟、其负载（git/LSP/shell）本身串行，多 vCPU 只增加同步开销（见七）。
4. **磁盘 = 单一 qcow2 工作盘，直接作根盘引导**：构建期生成 **golden.qcow2 母盘**（qcow2，内已完整落好 Alpine 用户态系统层 + qemu-init），随 HAP resfile 分发、运行期只读；每次启动 provision 将其复制为**工作盘 disk.qcow2** 挂作 `/dev/vda` 根。guest 对盘的所有写入只落在工作盘副本上，母盘永不被改。
5. **母盘更新即刷新工作盘**：工作盘为母盘的字节级副本。启动校验 `inspect_qcow2`：**Valid 保留**（跨启动持久）；**Missing / Corrupt / SizeMismatch 删盘重拷**母盘。开发迭代期替换 resfile 的 golden 后，旧的 working disk 因虚拟容量不符（SizeMismatch）会被自动重拷；若容量恰好相同则仍会保留旧系统——故以"替换 golden 后清沙箱或 `hdc uninstall`"为过渡手段，**内容指纹**仍是待办（见 4.5.3）。
6. **无 initramfs、无 switch_root、无 overlay、无运行时空盘代码生成、无首启解包**：内核 `root=/dev/vda rw init=/usr/lib/qemu-init/init` 直接把**盘内系统**引导为 PID 1。guest 系统层在构建期已写入盘，运行期零解包。
7. **不用 tools.tar.zst**；guest 内工具随系统层烧入（Alpine 自带 busybox applet 具备 mount/ip/date/md5sum/pkill 等），`hicodeerd` 经 sandbox 从 host staging 提供（见 4.5.4）。
8. **guest 系统一律改用 Alpine**：musl + OpenRC + busybox。换代动机是**性能**——旧 openEuler 用户态在 TCG 下 exec 密集负载的 78% 时间花在 sys，换成 musl/busybox 后 B1 基准快 1.62×（见七）。
9. **LSP 下载 libc 探测随后端**：`ldd --version` 经 `util::command` 已转发到当前后端（guest 系统层内置 glibc ldd / OHOS 本机）。
10. **动态 workdir 挂载沿用原逻辑**：挂载对象是 worktree 根普通绝对路径；对已被静态 share 覆盖的目录（sandbox、数据根）跳过，不占热插拔槽位。
11. **Terminal 探测式选后端 + 真 pty**：新建终端不读 QEMU 开关，先 `open_remote_shell` 探测；能连即用当前后端守护进程的 pty，失败回退本机 `/bin/sh`。守护进程支持 pty-req 会话（guest/host 内 openpty + `/bin/sh` 交互 + 双向桥 + resize）。**pty slave 必须 O_RDWR 打开**，否则 shell 写 stdout EBADF → 会话成功却零回显。
12. **guest 时间同步**：guest 就绪后经 cmd-client 推一次墙钟——先 `ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime` 再 `date -s @epoch`，对"守护进程未就绪"按 2 s 间隔重试至多 60 次。
13. **virtiofsd fd 耗尽修复**：guest 内守护进程内置周期任务每 **15 s** 写 `/proc/sys/vm/drop_caches=2`（`HICODEERD_DROP_CACHES` 控制），触发 FUSE_FORGET 释放 O_PATH fd。
14. **qemuctrl 只保留 QEMU 管理**：删除执行命令/SSH/virtio-serial 代码，不留残留；guest 侧命令一律经 cmd-client。
15. **guest init 自愈契约**：盘内 `/usr/lib/qemu-init/init` 为 PID 1；任何阶段失败以干净收尾（见 4.5.4）。宿主侧 provision 每启动判工作盘状态，损坏即从母盘恢复。
16. **QEMU 退出后引擎自动重启（事件驱动，无监督线程）**：guest poweroff/异常退出后引擎在同一线程 relaunch（见 4.4.1），并带崩溃循环熔断（60 s 窗口内 3 次快速退出即放弃）。
17. **libqemu .so 一经 dlopen 终身不 dlclose**：dlclose 后引擎线程退出时 musl TLS 析构会跳进已卸载代码段 → SIGSEGV 杀死整个 App。句柄有意泄漏，与进程同生命周期；重启只 relaunch QEMU main。
18. **guest 守护进程由盘内监督壳自愈**：`S30cmd-daemon` 用子 shell 监督循环拉起守护进程，1 s 固定重试；**脚本名刻意不含守护进程名**，否则 `pkill -f <守护进程名>` 会把监督者一起杀掉，留下无人复活的死局（见 4.5.4）。
19. **用户数据根挂进 guest**：用户选定的数据根（`custom_data_dir` 记录）持有 Linux 版 Node 运行时、下载的 language server 与 debug adapter，guest 必须在**原始 host 绝对路径**上执行它们。当该目录位于沙箱之外时，为它起**第二个静态 share**（tag `customer_data`），路径经 `files/qemu/guest-conf/customer_data_path` 告知 guest。
20. **垫片随 HNP 包分发，不再是数据根的 `passwd_shim`（2026-09-16 起）**：两个垫片以载荷形式打进守护进程自己的 HNP 包 `<pkg>/shim/`（`shim/shim.js`、`shim/musllib-shim.so`），由系统安装与替换；运行时守护进程从自身 `/proc/self/exe` 推出的包根取用（`hicodeerd/src/shim.rs`），并**进程级导出** `NODE_OPTIONS=--require <pkg>/shim/shim.js` 与 `LD_PRELOAD=<pkg>/shim/musllib-shim.so`，供所有后续子进程继承。一个不可用的垫片只会被**报告并跳过**（日志 warn），绝不把无效条目塞进变量——因为加载器会拒绝打不开的 `LD_PRELOAD`、Node 会拒绝指向缺失文件的 `--require`，那比垫片本身要修的缺陷更糟。数据根是用户可见、可删的目录，垫片放那里既可能被误删，也让守护进程依赖对它的写权限；包内载荷由系统整体安装，无运行时写入。因此原 `node/shim/osuser-shim.js` 与 `passwd_shim.rs` 均已不存在。
21. **guest-init 变更即重建 golden**：`bundle-ohos` 用 mtime 比较 `guest-init/` 与 `golden.qcow2`，前者更新则自动重建母盘（实测 14 ms/次），避免"改了引导脚本却装了个带旧脚本的母盘"。
22. **引擎不裁剪 HiSH 的启动线**：`-M`/`-cpu`/`-rtc`/`-overcommit`/磁盘与网卡选型整体取自 HiSH 的参考启动线，只补充本工程必需项（sandbox/customer_data share、8 个 pcie-root-port、RNG 熵源），并在常量旁逐条记录"为什么保留/为什么不照搬"。
23. **文件共享一律使用 virtio-fs，不用 virtio-9p**：9p 在 guest 工作目录上性能极差（stat 单次往返约 5.7 ms，慢 224 倍），已全量替换为 virtio-fs（vhost-user + FUSE）。不做任何基于 9p 的调优，只换方案（见 4.6）。

---

## 三、目标架构

```
HiCodeer 进程（HarmonyOS App, el2 sandbox, base_path=/data/storage/el2/base/haps/entry/files）
  launch_app (launch-zed/src/launch_app.rs::start_daemon_client)
    ├─ 关 QEMU：register_ohos_backend → cmd-client Endpoint::ohos_default() → OHOS hicodeerd 127.0.0.1:4022/4023
    └─ 开 QEMU（#[cfg(feature = "qemu-agent")]）：
          1) qemu_runtime::start_command_backend → provision_guest_files：resfile → base_path/qemu/{bin/hicodeerd, guest-conf/, ports/, disk.qcow2}
             disk.qcow2 = golden.qcow2 的副本（Valid 保留 / Missing/损坏/SizeMismatch 重拷）
             guest-conf/customer_data_path = 用户数据根绝对路径（仅当它在 sandbox 之外）
          2) qemu_manager::start_with_restart(paths, cfg, hook)  [主线程起，qemu-machine 引擎线程跑 guest]
          3) cmd-client Endpoint::qemu_guest() → host 127.0.0.1:4122(cmd)/4123(mgmt)
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

代码落位：

- `crates/gpui_ohos/depend/launch-zed/` — `launch_app.rs`（应用入口与后端分流）、`qemu_runtime.rs`（provision / 端点 / 重启钩子 / 时间同步）
- `crates/gpui_ohos/depend/qemu-mngt/qemuctrl/` — crate **`qemu-manager`**（`lib.rs` / `engine.rs` / `virtiofs.rs` / `mount.rs` / `qmp.rs`）
- `crates/gpui_ohos/depend/qemu-mngt/guest-init/` — 盘内引导脚本（`init`、`rcS`、`S00mount`、`S10sandbox`、`S12data`、`S30cmd-daemon`）
- `crates/gpui_ohos/depend/qemu-mngt/goldendisk/build-golden.sh`、`images/build-images.sh`
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/` — 命令客户端（`bootstrap` / `command` / `endpoint` / `executor` / `pool` / `protocol` / `pty` / `types`）
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/` — 守护进程（`main` / `sshd` / `peers` / `management` / `protocol` / `exec` / `pty` / `session_tmp` / `shim` / `keygen` / `sign_elf` / `logger`）

---

## 四、分模块设计

### 4.1 设置数据模型与启动读取

QEMU 档位是**纯 `settings.json` 数据模型**，当前**没有 Settings UI 页**，也**没有 `crates/settings/src/settings.rs` 读取器**，`assets/settings/*.json` 里也**没有** qemu 默认值。三处说法须以代码为准：

- **类型定义**：`crates/settings_content/src/qemu.rs`——`QemuCpuCores(1..=10)`（`#[default] Cpu1`）、`QemuMemGb(4/6/8/10/12)`（`#[default] Mem4`）、`QemuDiskGb(64/96/128/256/512)`（`#[default] Disk128`）。每个变体持久化为 snake_case token（`cpu4` / `mem8` / `disk128`）。该模块由 crate 根按 `target_env="ohos"` 条件编译，仅 HarmonyOS 构建存在这些类型。
- **字段挂载**：`crates/settings_content/src/settings_content.rs:266-293`——`qemu_enabled: Option<bool>`（`Default: false`）、`qemu_cpu_cores`、`qemu_mem_gb`、`qemu_disk_gb`，均带 `#[cfg(target_env = "ohos")]`。
- **启动读取器**：`launch-zed/src/qemu_runtime.rs`——不依赖 `SettingsStore`（启动早于其初始化），自己按候选路径找到 `settings.json`（`find_settings_json`）后以 `find_bool` / `find_number` 取值，落到 `LaunchQemuSettings`。`LaunchQemuSettings::default()` = `{ enabled: false, cores: 1, mem_gb: 4, disk_gb: 128 }`。

> 有效默认值以**两处**为准：settings 模型缺省（Cpu1 / Mem4 / Disk128）与 `LaunchQemuSettings::default()`（1 / 4 / 128）。二者必须一致，否则"代码默认单核、用户设置 4 核"这类漂移会让人误判性能数据出处。

### 4.2 cmd-client 命令客户端

`crates/gpui_ohos/depend/cmd-agent/cmd-client/`（模块 `bootstrap` / `command` / `endpoint` / `executor` / `pool` / `protocol` / `pty` / `types`）：

- **端点参数化**（`endpoint.rs`）：`CommandEndpoint{ mgmt_host, mgmt_port, command_host, command_port }`；`ohos_default()` = 127.0.0.1 **4023/4022**（OHOS 模式），`qemu_guest()` = 127.0.0.1 **4123/4122**（QEMU 模式）。注意 4122/4123 是 **host 侧**端口（hostfwd 暴露），guest 内部守护进程仍听 4022/4023。
- **协议常量**（`protocol.rs`）：`COMMAND_PORT=4022`、`MANAGEMENT_PORT=4023`、`BOOTSTRAP_COMMAND="hicodeerd-bootstrap"`、`SESSION_ID_PREFIX="__hicodeerd_sid__"`、`SIGNAL_PREFIX="__hicodeerd_signal__"`、`CLIENT_ID_PREFIX="hk-"`。这些串与守护进程 `protocol.rs` **必须 byte-for-byte 一致**（注释即如此要求）。
- **bootstrap**（`bootstrap.rs`）：持有一条 mgmt 连接**活到进程结束**（该连接的存活即 guest 侧进程树回收的凭据，见 4.3）；周期 `BOOTSTRAP_HEARTBEAT_INTERVAL=2s` 心跳，命令到来即刻 poke；`MGMT_TIMEOUT=15s`、`MAX_SSH_INFO_BYTES=64KiB`。首连成功即拿到 `SshInfo{command_port, command_host_key_pem, client_private_key_pem}`。
- **连接池**（`pool.rs`）：`MIN_IDLE=5` / `MAX_IDLE=16`；`CONNECT_TIMEOUT=15s`、`RECONNECT_BUDGET=3s`、`POLL_INTERVAL=100ms`、`ALLOCATE_RETRY=20ms`、`CONNECT_BACKOFF=1s`；keepalive 30 s、`REKEY_BYTE_LIMIT=1<<30`、`REKEY_TIME_LIMIT=365 天`。host key 不符即拒绝并触发重新 bootstrap。SSH 客户端为 russh。
- **executor**（`executor.rs`）：`SshCommandExecutor` 实现 `RemoteCommandExecutor`（`spawn` / `signal` / `try_exit` / `wait_exit_async` / `open_shell_pty`）；`IO_CHUNK_SIZE=8192`；每会话一条 `watch` 通道传 exit 状态；`spawn` 时起 `cmd-client-bootstrap` 后台线程。
- **pty**（`pty.rs`）：`TERM_TYPE="xterm-256color"`、`PTY_CHUNK=8192`；`RemotePty{stdin, stdout, resize_tx}` + `ResizeHandle`（`window_change`）。
- **自带的执行契约**（`types.rs`）：`FdMode`、`Signal{SigInterrupt=2,SigTerm=15,SigKill=9}`、`ExecSpec`、`RemoteChild`、`ExitFuture`、`ShellPtyFuture`、`trait RemoteCommandExecutor` 由 `cmd-client` **自带一份**（原 `command-executor` 叶子 crate 已不存在）。
- **命令拼装**（`command.rs`）：`build_command` 产出 `mkdir -p <cwd> && cd <cwd> && [ENV=..] exec '<binary>' '<args>' [redirs]`，再由 `sid_payload` 包上 session id 前缀。

### 4.3 hicodeerd 守护进程（OHOS 与 guest 双目标）

`crates/gpui_ohos/depend/cmd-agent/hicodeerd/`（源码 12 文件：`main` / `sshd` / `peers` / `management` / `protocol` / `exec` / `pty` / `session_tmp` / `shim` / `keygen` / `sign_elf` / `logger`；另有载荷目录 `shim/`）。同一份源码编两个目标，靠环境变量而非编译期 cfg 区分行为。

- **双目标编译**（`bundle-ohos`）：
  - OHOS 版（`aarch64-unknown-linux-ohos`）→ 装进 `hicodeerd.hnp`（public HNP），服务设备本机命令通道；包内**附带** `shim/shim.js`、`shim/musllib-shim.so`。
  - guest 版（`aarch64-unknown-linux-gnu` + `-C target-feature=+crt-static`）→ **静态链接**，因为 guest 用户态是 Alpine musl，没有 glibc 供动态二进制解析；产物无 `PT_INTERP`、无 `.dynamic`。
- **bind 与配置走环境变量**（`main.rs`）：`HICODEERD_BIND_ADDR`（guest 设 `0.0.0.0`，缺省 127.0.0.1）、`HICODEERD_CONF_DIR`（指向 sandbox 内 host staging 的 `guest-conf`）、`HICODEERD_DROP_CACHES`（`=1` 开启 15 s 周期 `drop_caches=2`）。代码无 ohos 专属 cfg 分支影响协议。
- **两个监听器**（`main.rs`）：动态密钥命令监听（4022）与**固定密钥**管理监听（4023）。管理连接是**长连接**，其生命周期承载身份语义（见下）。
- **双套 mgmt 钥**（`keygen.rs` + `bundle-ohos`）：OHOS 套与 guest 套各自 `ssh-keygen`（ed25519）。服务端半边（`mgmt_host_key` / `authorized_keys`）给对应守护进程；客户端半边（`mgmt-host.pub` / `mgmt-client-key`）给 cmd-client 读。两套都在每次构建时重生成，保证"设备上残留的旧钥永不匹配"。
- **身份与进程树回收**（`peers.rs`）：客户端 SSH 用户名带 `hk-` 前缀（`CLIENT_ID_PREFIX`）作为稳定身份。`Peer{groups, management_conns}`；只要该身份的**管理连接还活着**，其名下进程组就受保护；管理连接断开后经 `TERM_GRACE=1s` 宽限、`MIN_GROUP=2` 起才回收（`retire`）。停机时 `retire_all`。这样 guest 重启/客户端重连不会留下孤儿进程。
- **进程组信号**：子进程以 `process_group(0)` 起（pid == pgid），信号用 `kill(-pgid, sig)`；会话 id 与 pgid 存于 `exec.rs` 的静态 `SESSIONS` 表。退出码约定：`EXIT_SPAWN_FAILED=127`、`EXIT_UNKNOWN=128`、`EXIT_SIGNAL_UNKNOWN_SESSION=1`。
- **exec 路径**（`exec.rs`）：`spawn_command` 解析命令行并处理 npm 特例——`parse_which_programs` / `is_npm_token` / `NpmRewrite` 会在 `npm install` 后触发 `sign_elf` 签名（见下）；`IO_CHUNK_SIZE=8192`、`SLEEP_AFTER_EOF=10ms`、日志预览截断 `CMD_LOG_PREVIEW_CHARS=300`。含 `#[cfg(test)] mod tests`（10 例覆盖分词与 npm 重写）。
- **pty 路径**（`pty.rs`）：`PTY_SHELL="/bin/sh"`、`RELAY_POLL=20ms`、`PTY_CHUNK=8192`、`MAX_PTY_DIMENSION=u16::MAX`；slave 以 **O_RDWR** 打开（否则 shell 写 stdout EBADF），`setsid` + `TIOCSCTTY` 于 `pre_exec` 完成，master 双向 relay + `window_change` → `TIOCSWINSZ`；pty 进程同样登记进 `peers`。
- **per-client 临时目录**（`session_tmp.rs`）：把守护进程自身与所有子进程的 `TMPDIR` 指到 `<数据根>/tmp`（`TMPDIR_VAR="TMPDIR"`、`TMP_SUBDIR="tmp"`），每个客户端一份；`management.rs` 收到 bootstrap 命令时 `session_tmp::adopt`。
- **npm 后签名未签名 ELF**（`sign_elf.rs`）：`npm install` 会把预编译二进制落到 `node_modules`，OHOS 要求 ELF 带 `.codesign` 段。`sign_tree` 用 `binary-sign-tool`（`SIGNER`）对未签名 ELF 就地签名（`CODESIGN_SECTION=".codesign"`、`PACKAGE_DIR="node_modules"`、`SIGNED_SUFFIX=".signed"`、`SIGNED_MODE=0o775`、`SKIP_DIRS=[".git",".cache"]`、`MAX_FILES=20000`、`MAX_DEPTH=12`）。
- **垫片注入**（`shim.rs`，决策 20）：`install()` 从包根取 `<pkg>/shim/`，`append_env` 追加（已含则不重复）`NODE_OPTIONS=--require <pkg>/shim/shim.js` 与 `LD_PRELOAD=<pkg>/shim/musllib-shim.so`，并设 `HICODEERD_OSUSER`（缺省 `hicodeer`）供 Node 垫片上报用户名。不可用（缺失 / 含空白 / 非 UTF-8）一律 warn 并跳过。
- **日志**（`logger.rs`）：`mod ohos` 经 `OH_LOG_Print` 写 hilog（tag `Hicodeerd`）并同步 stdout；`--log` 启动时额外 `attach_file(<数据根>/logs/hicodeerd.log)`，形如 `MM-DD HH:MM:SS 级别 消息`（`TZ_OFFSET=8*3600`），并回放 `BACKLOG_LIMIT=256` 条启动前积压。非 ohos 目标走 stderr。
- **管理通道协议**（`management.rs`）：`exec_request` 解析 bootstrap 命令（`parse_bootstrap_command`）→ `session_tmp::adopt` + `logger::attach_file` → 回 `SshInfo` JSON；连接 `Drop` 时 `management_closed` 触发身份回收。`sshd.rs` 侧 `exec_request` 于通道已请求 pty 时改走 `pty::run_pty_shell`，否则 `exec::spawn_command`。

### 4.4 qemuctrl 引擎管理（crate `qemu-manager`）

`crates/gpui_ohos/depend/qemu-mngt/qemuctrl/`（Cargo 包名 `qemu-manager`，lib 名 `qemu_manager`）：

- **`lib.rs`**：常量 + `start` / `start_with_restart` / `prepare_reboot` / `build_argv`；类型 `QemuPaths{ kernel, initrd, port_dir, sandbox_mount, data_mount, disk_path }`、`QemuConfig{ cores, mem_gb, disk_gb }`；`static REGISTRY: Mutex<Option<Arc<MountRegistry>>>`。
- **类型与常量对齐 HiSH 参考启动线（逐条备注）**：
  - `MACHINE_TYPE="virt"`；`MACHINE_OPTIONS="memory-backend=mem,gic-version=max,iommu=none,usb=off,virtualization=off,compact-highmem=on,dump-guest-core=off,mem-merge=off,hmat=off"`。
  - `CPU_MODEL="max,pauth-impdef=on,sve=off,pmu=off"`；`SMP_SOCKETS=1`、`SMP_THREADS_PER_CORE=1`。
  - `TCG_TB_SIZE_MB=2048`（翻译缓存，地址空间而非常驻内存）；`RTC_OPTIONS="base=utc,clock=host"`；`OVERCOMMIT_OPTIONS="cpu-pm=off"`。
  - `DISK_DRIVE_OPTIONS="cache=writeback,aio=threads,discard=unmap"`；`DISK_IOTHREAD_ID="iothread0"`（专用块 IO 线程）。
  - `RNG_BACKEND_OPTION="rng-random,id=rng0,filename=/dev/urandom"` + `RNG_DEVICE_OPTION="virtio-rng-pci-non-transitional,rng=rng0"`，让 guest 的 CRNG 尽早播种（首个消费者是 TLS 握手）。
  - 端口常量 `GUEST_{COMMAND,MANAGEMENT}_PORT=4022/4023`、`HOST_{COMMAND,MANAGEMENT}_PORT=4122/4123`。
  - `MOUNT_TAG_SANDBOX="sandbox"`、`MOUNT_TAG_DATA="customer_data"`、`FS_SOCKET_{SANDBOX,DATA}`（`fs_sandbox.sock` / `fs_data.sock`）、`FS_WORK_PREFIX="fs_work"`、`QMP_SOCKET="qmp.sock"`、`WORKDIR_MOUNT_SLOTS=8`。
  - `QEMU_LIB_NAME="libqemu-system-aarch64.so"`、`QEMU_ENTRY_SYMBOL=b"main\0"`。
- **cmdline**（`GUEST_CMDLINE`）：`console=ttyAMA0,115200 root=/dev/vda rw init=/usr/lib/qemu-init/init mitigations=off TERM=xterm`——无 initramfs（无 `-initrd`）、无 switch_root。相对 HiSH 启动线去掉 `kpti=off`（本内核 `UNMAP_KERNEL_AT_EL0=n`，参数未注册）与 `init_on_alloc=1`（本内核默认关，传了反而开启零化、拖慢 guest）。**无 host-RAM 启动门槛**（guest RAM 是 lazy memfd，不需要整块空闲）。
- **`engine.rs`**：dlopen/dlsym(`main`) 专用线程（线程名 `qemu-machine`）跑 guest；**不 dlclose**（决策 17）；`QEMU_RUNNING: AtomicBool` + `is_running()` 供幂等判断；串口转发仅在 `qemu_debug_assertions` feature 下开。
- **`virtiofs.rs`**：静态 `sandbox` backend + 按需 `customer_data` backend + 动态 workdir backend；每个 share 一个 `virtiofsd-<tag>` 线程，走 `virtiofsd` crate 库内调用（无 CLI、无 seccomp），缓存策略 `CachePolicy::Auto`。
- **`mount.rs`**：动态 workdir 三段式（backend → QMP `device_add` → guest `mkdir` + `mount -t virtiofs`，`GUEST_MOUNT_RETRIES=10` × `GUEST_MOUNT_RETRY_DELAY="0.3"` s）；`MountRegistry` 去重 + 槽位记录；失败即"烧槽"；guest 重启时 `reset()` 清空。
- **`qmp.rs`**：仅 `device_add`（热插拔）用；hostfwd **不经 QMP 管理**（烤进 argv）。`GREETING_TIMEOUT=10s`。
- 文件共享 = 同名路径：host `files/` → guest 同名 `/data/storage/el2/base/haps/entry/files`，cmd-client 的 host 绝对 cwd 在 guest 无需路径翻译。

#### 4.4.1 引擎退出感知与自动重启（事件驱动）

- 崩溃根因（dlclose）与"不 dlclose"见决策 17。
- 重启 = 事件驱动：`engine::spawn_with_restart` 的引擎线程跑完一轮 `main` 后调注入的 `RestartHook`（`FnMut() -> Option<Vec<Vec<CString>>>`）：`Some(new_argv)` → 记 `relaunching guest after exit` → 立即 relaunch；`None` → 线程退出。
- `make_restart_hook(base_path)`（`qemu_runtime.rs`）每轮重启都重跑 `provision_guest_files`（盘校验/从母盘恢复）+ `prepare_reboot`（重启 virtiofs backend + 重置 mounts 注册表 + 重出 argv），实现"每次重启都自愈"。
- **崩溃循环熔断**：`FAST_EXIT_WINDOW=60s` 窗口内累计 `MAX_FAST_EXITS=3` 次快速退出即 `giving up relaunch`；每轮固定 `BACKOFF=3s` 退避；重启前重新读 settings，若用户已关 QEMU 则不再重启。
- 边界：guest 主动 poweroff 亦走同一路径（视作显式重启语义）。

### 4.5 guest 镜像（引擎 + 内核 + 根文件系统 + 引导脚本）

构建期在 host（aarch64 openEuler VM）完成，产物落入 HAP resfile/libs，由 `bundle-ohos` 打包。仓库内 `qemu-mngt/images/` 存有 `Image`、`alpine-rootfs.qcow2`、`libqemu-system-aarch64.so`、`libslirp.so.0`、`libz.so`、基线 config 与补丁。

#### 4.5.1 引擎（`images/libqemu-system-aarch64.so`）

- 来源：HiSH release。**QEMU 10.2.0**，约 52.5 MB；`DT_NEEDED` 只剩 `libslirp.so.0`、`libz.so`、`libc.so`——glib / pixman / pcre2 / intl 已内联，不再需要外挂 5 个 so。
- 导出入口：`engine.rs` 只需 `main`（`QEMU_ENTRY_SYMBOL=b"main\0"`）。
- `libz.so` 的处理：引擎要的是**无版本后缀**的 `libz.so`，而设备 `/system/lib64` 下没有 zlib。做法是把既有 `libz.so.1` 的 `DT_SONAME` 重写为 `libz.so` 另存（`images/build-images.sh` 内的 `patch_soname` python 段）；`libhicodeer.so` 不依赖 zlib，故删掉旧 `libz.so.1` 安全。

#### 4.5.2 内核（`images/Image`，linux 6.12.60）

- 基线 = HiSH 的 **`arm64_virt`** 配置（归档为 `images/arm64_virt.base.config`）+ HiSH 的 `KCFLAGS` + `images/n_tty_resize.patch`（补 `n_tty_resize`）。工具链 clang 17.0.6 + LLVM。完整复现步骤见 `images/kernel-build.md`。
- **能力增量 5 项（缺一不可）**：`FUSE_FS`、`VIRTIO_FS`（virtio-fs 是 FUSE 协议）、`PCIEPORTBUS`、`HOTPLUG_PCI`、`HOTPLUG_PCI_PCIE`（`pcie-root-port` 热插拔）。
- **性能增量 6 项（HiSH 为小内存 guest 做的牺牲，在 TCG 下是纯损失）**：`SLUB_TINY=n`、`ARM64_HW_AFDBM=y`、`ARM64_TLB_RANGE=y`、`TRANSPARENT_HUGEPAGE(_ALWAYS)=y`、`HIGH_RES_TIMERS=y`、`NR_CPUS=32`。
- `KCFLAGS='-march=armv8.5-a+crc+crypto+lse+rcpc+rng+sm4+sha3+dotprod+fp16 …'`：让 clang 直接发扩展指令，避免内核 `alternative` 打补丁路径——`+lse` 在 TCG 下把 `ldxr/stxr` LL/SC 重试循环换成单条原子指令。
- `images/build-images.sh` 以 pin 死的上游包（linux-config arm64_virt 基座、QEMU `hish-20260110` libs.zip、rootfs `20260117`）与 `MUST_Y` / `MUST_NOT` 列表驱动，带 `--check` / `--force` / `--only` / `--clean`。

#### 4.5.3 根文件系统与 golden 母盘（`images/alpine-rootfs.qcow2` + `goldendisk/build-golden.sh`）

- 基底：HiSH 的 `rootfs_aarch64.qcow2`，即 **Alpine Linux 3.22**（musl + OpenRC + busybox，128 GiB virtual）。
- **机制**：母盘 = 已完整落好系统层的 qcow2，构建期一次性生成，随 HAP resfile 分发（`resfile/qemu-guest/golden.qcow2`，qcow2 v3 / 64 KiB cluster / 128 GiB virtual），运行期只读；每次启动复制为工作盘。（早先实测压缩体积 18.2 MB（基底）与 23.5 MB（母盘），**本次未复核实测**——当前 `ENABLE_QEMU_AGENT=false`，无 golden 产物可量。）
- **生成步骤**（`build-golden.sh <OUT_QCOW2> <CACHE_ROOT>`，全流程每步失败即 abort）：
  1. `qemu-img convert` 把 Alpine 基底展开为稀疏 raw（`<CACHE_ROOT>/golden.raw`）；
  2. `sudo mount -o loop` 挂载；**先 `rm -rf` 镜像内 `/usr/lib/qemu-init/` 再注入**（复用 raw 时旧脚本会残留），注入清单**由 `guest-init/` 目录驱动**而非硬编码列表（新增脚本必须自动落盘，否则"构建成功但引导缺脚本"）；
  3. **补 glibc 运行时**：`ld-linux-aarch64.so.1` + `libc.so.6` / `libm.so.6` / `libpthread.so.0` / `libgcc_s.so.1` 等 17 个库 + `libstdc++.so.6`。原因：host 侧预编译的 language server 并非都认识 musl——`rust-analyzer` 运行时探测 libc 类型（`crates/languages/src/rust.rs`）会正确选 musl 版，而 `json` / `python` 适配器**硬编码** `unknown-linux-gnu`（glibc）资源。glibc 与 musl 可共存（loader 与 libc 的 soname 不同），故既保住那两个 server，又保住 musl 用户态的速度；
  4. **注入 tzdata**（Asia/Shanghai）：Alpine 基底不带时区库，缺了则运行时 `ln -sf … /etc/localtime` 是悬空链接、guest 打印 UTC；
  5. 卸载后 `qemu-img convert -c` 压成 qcow2 并安装到目标路径。
- 端到端实测 **5.3 s**；缓存目录 `<RUST_TARGET_ROOT>/<hash>-golden/`（`golden.raw` 悬留即跳过解包）。
- **运行期 provision（`qemu_runtime.rs`）**：读 `golden_virtual_size`（qcow2 头 offset 24）→ `inspect_qcow2(disk, size)` 判工作盘：`Valid` 保留 / `Missing` / `Corrupt` / `SizeMismatch` → `remove_file(disk)`（**容忍 NotFound**，全新首启的正常态）+ `copy golden → disk`。`Qcow2State` 除头部字段外还校验**覆盖头部簇的 refcount**——旧版曾出现"4 字节条目写在 qcow2 v2 的 2 字节位上"导致头簇被标为空闲、QEMU 在其上分配 L2 表的砖化案例。
- **待办（未固化）**：golden 尚无**内容指纹**（如 `sha256sum`），工作盘 `Valid` 判据只比虚拟容量，故"换母盘但容量相同"仍会保留旧系统。目标：在 golden 旁生成指纹，使判据升级为"指纹与当前 golden 一致才保留"。

#### 4.5.4 引导与盘内结构

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
- **守护进程不在盘内烧录，经 sandbox 从 host staging 提供**：host `base_path/qemu/bin/hicodeerd`（resfile 拷入）→ sandbox virtiofs → guest 同名路径执行（`S30cmd-daemon` 的 `DAEMON_BIN`）。改守护进程只需重拷 staging，不必重建母盘。
- **`init=` 覆盖 Alpine 自己的 `/init`**：OpenRC、getty、以及 `/etc/fstab` 里那条 `hostshare /mnt/share 9p` 都不会执行——既避免沙箱内 getty 反复失败，也使 9p 挂载需求消失。
- **`S30cmd-daemon` 的监督自愈（决策 18）**：脚本用 `( while [ ! -f /run/daemon.stop ]; do … "$DAEMON_BIN"; sleep 1; done ) &` 拉起守护进程（`RESPAWN_DELAY=1`），并在每次启动/退出打印 `guest daemon starting` / `guest daemon exited; restarting in 1s`（守护进程以**前台**方式跑在监督循环里，这些行直接落 guest 串口 ttyAMA0、再进 hilog；不带 `--log`、不写日志文件，避免污染被 watch 的工作区）。
  - **脚本名不含守护进程名**：否则 `pkill -f hicodeerd` 会同时匹配到监督壳 `/bin/sh /usr/lib/qemu-init/S30cmd-daemon start`，把监督者一起杀掉，守护进程永不再起（这正是 2026-09-12 之前"kill 后不自动重启"的根因）。
  - 守护进程环境：`HICODEERD_BIND_ADDR=0.0.0.0`、`HICODEERD_CONF_DIR=<sandbox>/qemu/guest-conf`、`HICODEERD_DROP_CACHES=1`、`HOME=/home`（并 `mkdir -p`，agent 会在此写状态）、`SHELL=/bin/sh`；`SSL_CERT_FILE` 按候选路径**存在才 export**（Alpine 用 `/etc/ssl/cert.pem`；指向不存在的路径会让所有 TLS 客户端失败）。
- **自愈的边界**：自愈只是安全网，不是根治。若根因（如 `/dev/shm` 被写满、内存耗尽）不变，守护进程重启后**会再次被杀**，形成"起了又杀、杀了又起"的拉锯；`drop_caches` 周期任务在停机期间也不执行。

### 4.6 宿主-访客文件共享（virtio-fs）

宿主（OHOS）与 guest 之间通过 QEMU 的共享文件系统交换数据，**一律使用 virtio-fs**（vhost-user + FUSE），不再使用 virtio-9p。

**约束**（与 9p 时代相同）：

- 不做任何基于 9p 的"调优"（cache 参数、msize 等），只换方案。
- OHOS 沙箱禁止 spawn 外部进程，virtiofsd 必须**嵌入 HiCodeer 进程内**（与 QEMU 同用 dlopen 方式）。
- 不做路径翻译：guest 看到的路径必须与宿主路径一致，避免 LSP 索引缓存因路径变化失效。

组成与数据流：

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

#### 4.6.1 三处 share 与路径一致性

共享点（share → tag → 宿主来源 → guest 挂载路径 → 后端 socket → 建立时机）：

- 应用沙箱：tag `sandbox`；宿主应用 `files/` 目录；guest 同宿主绝对路径；socket `fs_sandbox.sock`；启动参数（静态）。
- 用户数据根：tag `customer_data`；`<数据根>`（仅当它不在沙箱内）；guest 同宿主绝对路径；socket `fs_data.sock`；启动参数（静态，按需）。
- 工作目录：tag `work<N>`；用户打开的目录；guest 同宿主绝对路径；socket `fs_work<N>.sock`；QMP 热插拔（动态）。

**路径一致性**（§3.3 结论）——三个 share 一律挂到**与宿主完全相同的绝对路径**：

- 工作目录 → 如 `/storage/Users/currentUser/workspace/...`
- 应用沙箱 → `/data/storage/el2/base/haps/entry/files`
- 用户数据根 → 如 `/storage/Users/currentUser/HiCodeer`

原因有二：一是 **LSP 缓存跨重启有效**（clangd 等按路径建索引，路径一变缓存全废）；二是**宿主下发的命令无需翻译**——宿主把 `<数据根>/node/node-vXX-linux-arm64/bin/node` 交给 guest 执行时，该路径在 guest 里**就是**同样的字符串。9p 时代"宿主路径 → guest 挂载点"的翻译层因此整体消失，`PATH`、语言服务器注册表、debug adapter 路径都不必再带映射规则。

> **历史对照**：9p 时代 sandbox → `/sandbox`、tools → `/tools`（只读）、`ztag<N>` → 工作目录。tools 这一层现已不存在——宿主编译的 linux-arm64 二进制都放在**用户数据根**下（`node/`、`languages/`、`debug_adapters/`），由 `customer_data` share 挂入。

#### 4.6.2 静态挂载（`sandbox`、`customer_data`）

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
- `-object memory-backend-memfd` 是 vhost-user 的前提（guest RAM 共享给 virtiofsd）；**memfd_create 在 OHOS 沙箱可用是本方案成败的第一判定点**（实测通过，见 4.6.5）。
- `-chardev socket,path=...,id=fs_*` 为 client 模式，连接 virtiofsd 的监听 socket。
- 常量：`MOUNT_TAG_SANDBOX="sandbox"`、`MOUNT_TAG_DATA="customer_data"`、`FS_SOCKET_SANDBOX="fs_sandbox.sock"`、`FS_SOCKET_DATA="fs_data.sock"`。

virtiofsd 后端（`qemuctrl/src/virtiofs.rs`）：`start(&QemuPaths)` 在 QEMU 启动前为 `sandbox` 起一个后端线程；`paths.data_mount` 为 `Some` 时再为 `customer_data` 起一个。每个后端用 `PassthroughFs` + `VhostUserFsBackendBuilder` + `VhostUserDaemon` 监听对应 socket，等 QEMU 的 chardev 连接并完成 vhost-user 握手。缓存策略为 `CachePolicy::Auto`（`virtiofs.rs::run_backend`）。

**guest 侧由谁挂载**：引导脚本 `guest-init/S10sandbox`、`guest-init/S12data`：

- `S10sandbox`：`mkdir -p /data/storage/el2/base/haps/entry/files` 后 `mount -t virtiofs sandbox <同名路径>`，并在其下建 `home`、`qemu/logs`、`qemu/guest-conf`。
- `S12data`：先读 `.../qemu/guest-conf/customer_data_path`（宿主在启动前写入的数据根路径）。**文件不存在就直接退出**——没有自定义数据根时本次不建这个 share。有值则 `mkdir -p <该路径>` 后 `mount -t virtiofs customer_data <该路径>`，并在串口回显一行 `[qemu-init] data root mounted at <路径>: <顶层目录列表>`（这是 guest 侧证明"挂上了且内容是真的"的唯一凭据）。

**为什么数据根要单独占一个 share**：用户数据根可以选在应用沙箱之外（`<数据根>/node`、`<数据根>/languages`、`<数据根>/debug_adapters` 全是宿主预置的 linux-arm64 二进制），而 `sandbox` share 只覆盖沙箱内的 `files/`。这些二进制必须由 guest 在**原本的宿主绝对路径**上执行，所以需要第二个 share 把它们原样呈现进 guest。

#### 4.6.3 动态工作目录挂载

工作目录路径只在用户打开文件夹时才知道，无法在 QEMU 启动参数里静态声明，因此运行时热插拔。

宿主侧编排在 `qemuctrl/src/mount.rs` 的 `MountRegistry::mount_workdir`（三段式，add-only、从不卸载）：

1. **启动 virtiofsd 后端**：`virtiofs::spawn_workdir(port_dir, slot, shared_dir)` 创建后端线程，监听 `<port_dir>/fs_work<slot>.sock`，返回 socket 路径。线程起不来就返回 `Err`——绝不把 QEMU 指向一个没人服务的 socket。
2. **QMP 热插拔**（`qemuctrl/src/qmp.rs::create_workdir_vhost_fs`）：
   - `chardev-add`：新增一个 client socket chardev（id `fs_work<slot>`），指向 virtiofsd 的 `fs_work<slot>.sock`。
   - `device_add`：`vhost-user-fs-pci`，绑定该 chardev，`id=workdev<slot>`、`tag=work<slot>`，挂到启动时预建的根端口 `rp<slot>`。
3. **通知访客挂载**：guest 侧命令为
   ```sh
   mkdir -p '<宿主路径>' && for i in 1..10; do mount -t virtiofs 'work<slot>' '<宿主路径>' && exit 0; sleep 0.3 2>/dev/null || sleep 1; done; exit 1
   ```
   设备刚热插拔完，guest 需要先枚举 pcie 端口、再绑定 virtiofs 驱动，tag 才会出现，所以第一个 `mount` 常常太早——这就是 10 × 0.3 s 重试窗口的来历（`GUEST_MOUNT_RETRIES` / `GUEST_MOUNT_RETRY_DELAY`）。该命令**不自己讲 SSH**：它经 `GuestShell` trait 由调用方注入（cmd-client 的 `run_shell` 打到 guest 守护进程），`qemuctrl` 因此不依赖 cmd-client。

**槽位分配**（与 9p 时代的自增计数器不同）：启动时按 `WORKDIR_MOUNT_SLOTS=8` 预建 8 个 `pcie-root-port`（`rp0..rp7`）。挂载时取第一个空槽 `slots.iter().position(Option::is_none)`，编号即槽号：

- 后端 socket → `fs_work<N>.sock`
- chardev id → `fs_work<N>`
- device id → `workdev<N>`
- mount tag → `work<N>`
- pcie 端口 → `rp<N>`

**失败即烧槽**（`mount.rs` 的既定语义）：任何一步失败都把该槽标记为已占用。理由写在代码注释里——后端线程已把 socket 绑住（同槽重试会叠第二个听众），`device_add` 成功后 QEMU 也保留 chardev/device id，所以失败后让下一次重试换新槽，而不是在同一槽上叠设备。8 槽用尽时报 `all 8 workdir slots are occupied`。

**幂等**：`mounted: HashSet<PathBuf>` 按 canonicalize 后的路径去重，已挂的直接返回 `Ok`。

**guest 重启**：`MountRegistry::reset()` 清空去重集合与全部槽位。新 guest 什么都不挂，旧实例的"已挂载"记录与烧掉的槽位都不能留——否则调用方会被告知某个路径还挂着，而新 guest 里其实空空如也。

#### 4.6.4 依赖软件补丁（virtiofsd / cap-ng / vmm-sys-util）

virtiofsd 依赖链中有 3 个 crate 在 OHOS 上无法直接编译/运行，均在仓库根 `patches/` 下做本地补丁，全部用 `cfg(target_env = "ohos")` 包裹，**其他平台编译与上游一致**。

引入方式（`Cargo.toml`）：`[patch.crates-io]` 把 `virtiofsd` / `capng` / `vmm-sys-util` 指到 `patches/<name>`；`qemuctrl/Cargo.toml` 只在 `[target.'cfg(target_env = "ohos")'.dependencies]` 下依赖 `virtiofsd = { workspace = true }`，并把 `vhost`(0.16) / `vhost-user-backend`(0.22) / `vm-memory`(0.17.1) 钉到与 virtiofsd 解析结果一致的版本，避免 cargo 编出第二份。

- **virtiofsd（`patches/virtiofsd`）**：`PassthroughFs::new()` 内部 `OsFacts::new()` 用 `openat2` syscall 探测内核能力（Linux 5.6+ 优化），而 OHOS 沙箱的 seccomp 策略禁 `openat2`（SYS_openat2），直接 SIGSYS 杀死整个进程。修改（`src/oslib.rs`）：OHOS 下 `let has_openat2 = false;`，非 OHOS 保留原探测。`has_openat2=false` 时 PassthroughFs 回退用 `openat`（沙箱允许）；`openat2` 只是性能优化，不影响功能。
- **cap-ng（`patches/cap-ng`）**：`build.rs` **无条件**声明 `rustc-link-lib=dylib=cap-ng`，OHOS 无 libcap-ng 库 → 链接失败；`bindings.rs` 的 `#[link(name = "cap-ng")]` 引用 libcap-ng 符号；OHOS 沙箱无 capability 语义，virtiofsd 的 capng 调用路径在非 root 下从不执行。修改：`src/bindings.rs` 的 extern 块加 `#[cfg(not(target_env = "ohos"))]`，OHOS 下提供 20 个 `capng_*` no-op stub；`build.rs` 在 OHOS 下生成空 `libcap-ng.a`（`!<arch>\n`）并 `rustc-link-search` + `rustc-link-lib=static=cap-ng`。
- **vmm-sys-util（`patches/vmm-sys-util`）**：OHOS libc 为 bionic 风格，与 crate 假设的 glibc 布局不同——`sock_ctrl_msg.rs` 的 `libc::msghdr` 有私有字段（`__pad1`/`__pad2`）、`msg_iovlen` 为 `i32`、`msg_controllen` 为 `u32`；`ioctl.rs` 的 `IoctlRequest` 应为 `c_int`；`seek_hole.rs` 的 `lseek64` / `SEEK_DATA` / `SEEK_HOLE` 未从 libc 导入。修改（3 处，均加 `cfg(target_env = "ohos")`）：`sock_ctrl_msg.rs` 走 zeroed 构造分支；`ioctl.rs` 的 `IoctlRequest = c_int`；`seek_hole.rs` 补 `use libc::{lseek64, ENXIO, SEEK_DATA, SEEK_HOLE};`。

#### 4.6.5 与 9p 的性能对照

工作目录（LSP/git/编译主战场）实测（TCG 模拟 + OHOS 设备，**2026-09 数据**，本次未重测）：

- stat_hot（stat 同文件 500 次）：9p 2865 ms → virtio-fs 14 ms（204 倍）
- scan500（创建+stat 500 文件）：9p 45197 ms → virtio-fs 5627 ms（8 倍）
- read1M（读 1 MiB）：9p 368 ms → virtio-fs 10 ms（37 倍）
- write1M（写 1 MiB）：9p 268 ms → virtio-fs 103 ms（2.6 倍）

stat 单次往返从约 5.7 ms 降到约 0.03 ms，**仅比 guest 本地 tmpfs 慢 1.4 倍**。该数据至今未重测：virtio-fs 的数据通路（memfd + vhost-user + FUSE passthrough + `CachePolicy::Auto`）自替换起未变，换引擎/换发行版改的是别的层。

设备实测全链路通过：memfd_create 沙箱可用（guest 正常 boot）；静态 share `sandbox`、`customer_data` 与动态工作目录均挂载成功；guest 串口回显 `[qemu-init] sandbox mounted at ...` 与 `[qemu-init] data root mounted at <数据根>`；访客守护进程心跳/命令执行正常；多工作目录并发挂载正常；9p 已从挂载路径、QEMU 参数、访客挂载脚本中完全移除。

### 4.7 launch 分流 + qemu_runtime + 动态挂载 + 终端 pty

- `launch-zed/src/launch_app.rs::start_daemon_client`：编译期按 `#[cfg(feature = "qemu-agent")]` 分流——启用时调用 `qemu_runtime::start_command_backend`，否则 `register_ohos_backend`（读 `<resfile>/hicodeerd-mgmt/{mgmt-host.pub,mgmt-client-key}`）。`start_zed_main` 另设 `ZED_EXPERIMENTAL_A11Y=1` 并 `paths::set_custom_data_dir`。
- `launch-zed/src/qemu_runtime.rs::start_command_backend`：读 settings.json → enabled 则 `provision_guest_files` + `qemu_manager::start_with_restart`；按开关选 Endpoint+keys 构造 executor。若 guest 通道备不齐，回退（fallback chain）到 `register_ohos_backend`。
- **staging**：resfile → `base_path/qemu/{bin/hicodeerd, guest-conf/…, ports/}`；工作盘 disk.qcow2 = golden 副本。staging 含**强制覆盖**语义：guest 守护进程 bin 与 guest mgmt 密钥每次 `std::fs::copy` 覆盖（ed25519 文件定长，按大小跳过会留下旧钥 → `Unknown server key`）。
- **数据根 share（决策 19）**：`read_data_root(base_path)` 读 `<base_path>/custom_data_dir`（`HOME_DIRECTORY_RECORD_FILE`）；若路径不以 `base_path` 开头则填 `QemuPaths.data_mount` 并把路径写入 `guest-conf/customer_data_path`（`CUSTOMER_DATA_PATH_FILE`）；否则**删除**可能残留的记录文件（否则 guest 会去找一个本次并未共享的目录）。`covered_roots = [sandbox_mount] + data_mount`，两者覆盖的目录一律不走懒挂载（不烧热插拔槽）。
- **动态挂载**：`WorkdirAwareExecutor`（QEMU 模式外层）在 spawn 与 `open_shell_pty` 时按 cwd 懒挂（静态覆盖根内跳过；否则向上找最近的含 `.git` 的祖先作挂载根；backend + QMP + guest mount 三件套；只挂不卸）。另有 `GuestShellAdapter` 承载 `GuestShell` 注入。
- **终端 pty**：守护进程 pty-req/openpty（**slave O_RDWR**）；新终端探测式连接、失败回退本机。
- **时间同步**：独立 `qemu-time-sync` 线程（`start_time_sync`），最多 60 次 × 2 s。
- **设置读取**：`read_launch_qemu_settings` / `find_settings_json` / `find_bool` / `find_number`（见 4.1）。

### 4.8 构建与打包（bundle-ohos / HAP / 权限）

`script/bundle-ohos`：

- **编译期开关**：`ENABLE_QEMU_AGENT=false`（`script/bundle-ohos:49`，**无 `--qemu` 等 CLI 参数，须改常量**）。为 true 时给 launch-zed 加 `--features qemu-agent`。开关关闭时，QEMU 客户机相关的一切（guest 二进制、golden、内核、引擎 so、guest 密钥）**都不参与构建与打包**；脚本另有一条 `else` 分支从 HAP 树里**删除**可能残留的 qemu 资源（`:589` 起为 QEMU 段，`:731` 打印 `QEMU guest backend disabled`）。
- **开启时**：
  - 编译 OHOS 版守护进程 → `hnpcli pack` 出 `hicodeerd.hnp`（public，`conf/` 内含服务端密钥，含 `<pkg>/shim/` 载荷），签名前注入 HAP；
  - 编译 guest 版守护进程（gnu + `crt-static`）→ `resfile/qemu-guest/hicodeerd`；
  - guest mgmt 钥每次构建重生成 → `resfile/hicodeerd-mgmt-guest/{mgmt-host.pub, mgmt-client-key, mgmt_host_key, authorized_keys}`（OHOS 套 → `resfile/hicodeerd-mgmt/`）；
  - 内核 `images/Image` → `resfile/Image`；引擎 so 三件（`libqemu-system-aarch64.so`、`libslirp.so.0`、`libz.so`）→ `hap/entry/libs/arm64-v8a/`，入包前 `strip`（unstrip 副本留 `qemu-engine-symbols/` 供 addr2line）；
  - **golden 依赖检测（决策 21）**：`find guest-init -type f -newer golden.qcow2` 非空即自动重建，`--rebuild-golden` 强制重建，构建失败立即 abort（绝不发出旧母盘）。
- `libhicodeer.so` 入 HAP 前 strip（保留 unstrip 副本供 addr2line 符号化）；`TMPDIR` 指到 `target/tmp`。
- **HAP 清单（`hap/entry/src/main/module.json5`，实测）**：
  - `hnpPackages`（`:150-159`）只声明两项——`git.hnp`（private）与 `hicodeerd.hnp`（public）。**当前没有 openssh.hnp / curl.hnp**（原文档所列已不符；这些二进制能力的落地见 `2026-09-07-ohos-hnp-git-ssh-curl-local-exec-design.md`）。
  - `requestPermissions`（`:125-149`）只声明三项——`ohos.permission.FILE_ACCESS_PERSIST`、`ohos.permission.INTERNET`、`ohos.permission.kernel.ALLOW_WRITABLE_CODE_MEMORY`（TCG 要 JIT 生成可执行内存）。加载引擎 `.so` 另需 `LOAD_INDEPENDENT_LIBRARY` / `IGNORE_LIBRARY_VALIDATION` 一类系统级放行（见移植向导第 17 章）。
- **HAP 体积**：签后约 580 MiB（引擎 50 MiB + golden ~23.5 MiB + `libhicodeer.so` 451 MiB），开发期可用 `--hap-only` / hdc 预推替代整包重装。

### 4.9 实现约束（工程红线）

- 非 ohos 文件禁止自行修改；全路径不含 `ohos` 的改动一律 `#[cfg(target_env="ohos")]` 包裹、大段新代码封装成函数/独立文件。
- 不裁剪功能、不屏蔽/删减代码解决编译/运行问题；禁空函数/桩函数。
- 代码注释只英文；禁魔鬼数字；新增代码带分级日志（异常分支必有 error）。
- 禁止 clean / 删除目录 / 卸载程序；构建仅走 `script/bundle-ohos`。

---

## 五、Terminal → guest 切换（验收主线）分层设计

验收标准：新建 Terminal 连上 QEMU(guest) 的 shell，而不是 OHOS 的 /bin/sh。

1. **服务器 pty（hicodeerd）**：`pty.rs`（openpty；**slave 以 O_RDWR 打开**——O_RDONLY 会让 shell 写 stdout EBADF → 会话成功却零回显）spawn `/bin/sh -c <exec>` 挂 tty（setsid + TIOCSCTTY），master 双向 relay + `window_change` → `TIOCSWINSZ`。`sshd.rs`：`pty_request` 存尺寸、`exec_request` 当通道请求过 pty 时改走 `run_pty_shell`、`data()` 先查 pty master、`window_change_request` → resize。
2. **cmd-client RemotePty**：`open_shell_pty(cols,rows)`——allocate 连接 → `pty-req` → exec shell → 暴露读/写/`resize`。
3. **terminal 适配（crates/terminal，ohos cfg）**：新建 Terminal 先探测 `open_remote_shell`；成功用"本地 pty 作 master 给 alacritty + 后台线程桥接 guest RemotePty"（本地窗口 resize 同时下发 guest）；失败回退本机 /bin/sh。
4. **前置**：guest 能起（工作盘 provision + 引导 + 守护进程连上）。

---

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
10. **数据根挂载**：`customer_data` 第二 share + `customer_data_path` + `covered_roots`。✅（hilog `data root mounted at …`）
11. **守护进程自愈**：`S30cmd-daemon` 监督循环 + 1 s 重试。✅（宿主侧实验：pkill 后监督者存活、守护进程 1 s 内以新 PID 复活）
12. **golden 依赖检测**：guest-init 比 golden 新即自动重建。✅（实测 14 ms）
13. **垫片随包分发**（2026-09-16）：`<pkg>/shim/` + `NODE_OPTIONS`/`LD_PRELOAD` 进程级导出。✅
14. **golden 生成内容指纹**。（待办，见 4.5.3）

验证观察点（hilog）：`[qemu-boot]` provision 阶段、`[qemu-console]` guest 串口、`[qemu-init]` sandbox mounted / data root mounted / guest daemon starting|exited、`qemu_manager::start` 引擎幂等与丢弃旧注册表、`relaunching guest after exit`、drop_caches 周期、cmd-client bootstrap `reconfiguring pool to 127.0.0.1:4122`、`guest clock synced`、`guest shell ready` / `open_guest_pty`、`pty: interactive shell started`。

---

## 七、guest 性能实测与约束

### 7.1 结论先行

- **头号瓶颈是进程创建/exec，不是文件 IO**。guest 内每次 exec 约 37~64 ms，真机同负载约 1 ms。
- **加核不是灵药**：引擎是 MTTCG，而 guest 侧工作流串行，多 vCPU 只付同步税（屏障翻译、TLB shootdown/IPI、BQL 与 virtio 队列争用、TB 缓存按 vCPU 分片，再叠加宿主抢核）。默认档位因此定为**单核**。
- **换代 HiSH 栈（Alpine/musl + 6.12.60 内核）是当前最大的一次性能收益**：同设备同负载 B1 由 45.23 s → 27.96 s（1.62×），收益主要来自 sys 段。

### 7.2 负载定义（脚本 `qemu_perf_release.sh`）

- B1 exec 密集：`2000 × md5sum`（debug 基线读真文件 `/bin/sh`，release 基线读 `/dev/null`，两者不可直接相减归因）
- B2 io/copy 密集：先建 800 个小文件（每个文件额外 exec 一次 `head`），再 `cp -a` 该目录
- B3 cpu 密集：200 万次 shell 整数加法（≈纯 TCG 热循环翻译，除启动外无 syscall）
- 计时用 bash 内建 `time` / `TIMEFORMAT`，不依赖 guest 的 `date +%N`

### 7.3 HiSH 换代前后对照（同设备、同 release 构建、4 核）

- B1（2000 × `md5sum /dev/null`）：旧（openEuler + 6.18.7）45.232 s（user 15.726 / sys 35.533）→ 新（Alpine + 6.12.60）**27.96 s**（**1.62×**）
- B2a（建 800 个小文件）：未测（旧）→ 12.81 s（新）
- B2b（`cp -a` 该目录）：未测（旧）→ 0.19 s（新）
- B3（200 万次 shell 加法）：未测（旧）→ 74.25 s（新）

B1 复测一次 27.63 s（相差 <1.5%，噪声内）。旧基线 **78% 时间在 sys**，正是 exec 的 syscall 开销——这是换 musl/busybox 后收益最大的那一块。

### 7.4 通用模型：慢在 TCG 翻译 syscall/内核路径

- 顺序大块吞吐正常（guest 内 `/dev/vda` 读 284 / 写 130、virtiofs 写 179 MB/s）→ 磁盘参数（qcow2/压缩/cache）、线程、压缩盘**不是顺序 IO 瓶颈**。
- exec 密集 `user+sys ≈ real`（满载，非 IO 等待），且 `sys > user`：每次 fork/exec 走 guest 内核 clone/execve/mmap 装载，这些内核路径在 TCG 下逐条翻译执行，被放大约 **70×**。
- **判定口诀**：guest 快慢取决于「每单位工作的 syscall/翻译密度」，而非磁盘吞吐。dd 每 1 MiB 一次 syscall → 摊薄而快；小文件/进程/exec 密集负载（git、apk、cp 大目录、编译）每文件几十次 syscall → 全部翻译 → 慢。guest 内 `top` 的 CPU% 是**虚拟 CPU 时间**，看不到 host 侧 TCG 翻译的真实墙钟。
- drop_caches（决策 13）每 15 s 清 guest dcache，令小文件元数据周期重读（叠加干扰，次要）。
- 工作盘继承 golden 压缩属性（`convert -c`）：随机小写命中既有压缩簇触发读回/解压/COW，为写盘慢的次因（顺序写新簇不受影响）。

### 7.5 核数对照（替换前 release 基线；结论对 TCG 通用）

同一台设备（host `nproc=16`、24 GiB RAM），仅改 settings 核数（内存固定 8G）：

- B1（exec）：4 vCPU 127.049 s → 10 vCPU 74.204 s（1.71×，exec 密集是唯一吃得动多核的负载）
- B2 建文件：4 vCPU 19.628 s → 10 vCPU 31.873 s（10 核反而更慢，疑为重启后 guest 后台任务干扰，不作为"加核反噬"证据）
- B2 `cp -a`：4 vCPU 0.512 s → 10 vCPU 0.582 s（单进程完成、无 exec → 与核数无关）
- B3（CPU）：4 vCPU 227.584 s → 10 vCPU 188.268 s（1.21×，单 shell 循环只占一个 vCPU，加核近乎无用）

结论：核数是唯一有效杠杆但**收益亚线性**（guest 工作流串行，多核只帮宿主侧辅助线程铺开），且不应超过宿主物理核。默认单核；确有 exec 密集且存在真并行度的场景可上调。

### 7.6 硬件加速不可得（2026-09-10 调研）

- 设备无 `/dev/kvm`、无 vhost 节点/内核模块。
- OpenHarmony ARM64 官方 defconfig 默认关 `CONFIG_VIRTUALIZATION` / `CONFIG_KVM`（不作 hypervisor host、减攻击面）。
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

---

## 八、风险与未决

- **【已解决】磁盘系统第二段 init 二次挂 sandbox 失败**：随 switch_root/initramfs 一起废弃——现单盘直启，sandbox 只在盘内 init 挂一次。
- **【已解决】guest 守护进程被 kill 后不再起**：根因是监督壳命令行含守护进程名、`pkill -f` 连坐；改名 `S30cmd-daemon` 解决（决策 18）。
- **【已解决】数据根未挂进 guest**：node/LSP/DAP 在 guest 内不可用；新增 `customer_data` 静态 share（决策 19）。
- **母盘生成无内容指纹**：换 golden 后若虚拟容量相同，旧工作盘仍会被保留（4.5.3 待办）。
- **`mount.rs` 失败即烧槽**：同一目录挂载失败后 `is_mounted()` 仍为 false，重试会占新槽（共 8 个）。启动早期的预热挂载在守护进程就绪前失败，会连烧 `work0..work3`；槽位耗尽后工作区永远挂不上。**属既有缺陷，未修**（候选修法：失败不标 occupied，改为允许对同一目录幂等重试）。
- **终端 probe 时序**：启动早期（guest 未就绪）打开的面板会 fallback `/bin/sh` 且不重试；后续新开面板即连 guest。可作为体验优化。
- **`bundle-ohos` 第 5 步注释失效**：`script/bundle-ohos:724-725` 注释仍称"空 qcow2 在运行期由 `qemu_runtime.rs::write_empty_qcow2` 生成"，该函数已不存在（现由 resfile 的 golden 复制为工作盘）。**属注释残留，未修**。
- **`QemuPaths.initrd` 字段残留**：`qemuctrl/src/lib.rs:218` 仍有该字段，`qemu_runtime.rs:330` 仍把它指向不存在的 `rootfs.cpio.zst`（无 initramfs 后已无用途）。**属字段残留，未修**。
- **无 Settings UI 页**：QEMU 档位只能在 `settings.json` 手工填写；原文档所述的设置页控件并未落地。若需图形化入口，需另立工作项。
- **`qemu_enabled` 改动需重启应用生效**。

---

## 九、演化史（已废弃方案存档）

本节记录曾被写入设计文档、现已从代码中移除的方案，避免后人凭旧文档误判架构。原分散文档（`2026-08-26`、`2026-09-01`、`2026-09-02`、`2026-09-03`、`2026-09-12`）已删除，要点在此留痕。

1. **2026-08-26 沙箱→VM 文件同步引擎（FileSync）**：为让 VM 侧 spawn 的文件已就绪，设计过"沙箱→VM 单向镜像同步"——`zcoder` 内独立后台线程监听下载目录（`languages/`、`extensions/`、`external_agents/`、`copilot/`、`prettier/`、`node/`、`debug_adapters/`），把变更经 `cmd-agent-protocol` 新增的 FileSync 消息推送到 VM 写盘（`FileSyncStart` / `FileBegin`（裸字节流） / `FileRename`（`.ing`→原子落地） / `FileDelete` / `FileCreateDir` / `FileSyncEnd`），binary 路径经 Rule A 映射到 VM `/home/user/cmd-agent/zed/...`。**已整体废弃**：现由 virtio-fs 静态/动态 share 直接同名呈现宿主目录，无需同步、无需路径映射。
2. **2026-09-01 QEMU ssh-agent 设计**：早期把命令通道做成 guest 内 `ssh-agentd` + 宿主 `cmd-agent`（`cmd-agent-linker` / `cmd-agent-protocol`），控制面走 **virtio-serial**（`zcoder.ssh.mgmt` 一类的串口设备名），主机密钥校验为容忍式的 Accept-All，dropbear 作为 sshd。**已整体废弃**：virtio-serial 通道、`ohos-qemu-agent/ssh-agent/` 目录、`cmd-agent*` 系列 crate 均已不存在；现控制面走 hostfwd 暴露的 TCP 4022/4023（→ host 4122/4123），SSH 为 russh，主机密钥每次构建重生成并严格校验。
3. **2026-09-02 virtio-fs 替换 virtio-9p**：把 9p 全量换成 virtio-fs。其**有效内容已并入本文 §4.6**（三处 share、路径一致性、动态 workdir 三段式、三个依赖补丁、9p/virtio-fs 性能对照、tools share 的消失）。
4. **2026-09-03 ohos-qemu-DESIGN**：更早的一版 QEMU 集成总设计，描述 `libqemu-system-aarch64.so` + `Image` + `rootfs.cpio.zst` + `tools.tar.zst` 四件套、`/sandbox` 与 `/tools` 两个翻译挂载点、`SshCommandExecutor::mount_folder`、`command-executor` 叶子 crate、`bundle-ohos --qemu` 参数等。**已全部被本文取代**：无 initramfs（无 `rootfs.cpio.zst`）、无 tools share、无 `command-executor`（契约并入 `cmd-client`）、`--qemu` 参数不存在（改为 `ENABLE_QEMU_AGENT` 常量）。
5. **2026-09-12 zcoderd 自动重启 + 改名评估**：一份动手前的评估稿。其**决策已全部落地**——guest 侧采用"方案 A（`S30cmd-daemon` 内看护子 shell）"，`zcoderd` → `hicodeerd` 改名完成（含环境变量、HNP 包名、resfile 子目录、hilog tag），显示名 `zcoder` → `HiCodeer`（`AppScope/.../string.json` 与 `entry/.../string.json`）。未采纳项：`bundleName` 保持 `com.zcoder.studio`（改名等于装新应用、数据不迁移）；`libzcoder.so` 内部库名未改。OHOS 本机侧守护进程的自启问题仍无自动化（`hicodeerd.hnp` 为 public，应用拉不起），属独立未决项。

---

## 十、相关文档索引

- 数据根**位置**决策：`2026-09-10-ohos-data-home-directory-design.md`
- 数据根**内部布局**：`2026-09-14-ohos-data-root-layout.md`
- HNP 本地执行（git/ssh/curl）：`2026-09-07-ohos-hnp-git-ssh-curl-local-exec-design.md`
- 换代方案：`qemu-mngt/QEMU-HiSH-替换方案.md`；内核重建：`qemu-mngt/images/kernel-build.md`
- 相关 bugfix：`2026-09-09-qemu-provision-missing-disk-abort.md`、`2026-09-09-zcoderd-pty-slave-readonly.md`、`2026-09-09-ohos-qcow2-disk-corruption-self-heal.md`、`2026-09-13-ohos-language-server-unavailable.md`
- 历史（启动加速，不可当现架构说明）：`bugfix/2026-08-30-qemu-linux-boot-speedup.md`
