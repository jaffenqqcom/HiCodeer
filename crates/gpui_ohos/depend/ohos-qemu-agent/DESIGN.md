# ohos-qemu-agent：现状设计

> 本文档只描述**当前实现**（2026-09）。早期路线（virtio-serial 端口池承载命令、
> cmd-agent/cmd-agentd 协议、9p / QMP fsdev_add 挂载）已被替换并随代码删除，
> 不做历史记录。
>
> 定位：zcoder 在 OHOS 上可从两个命令后端中选一个（`openeuler-agent` /
> `qemu-agent`，编译特性互斥，默认 `openeuler-agent`）。本文件描述
> `qemu-agent`：在 zcoder 进程内启动一个 Linux guest，把 git / LSP / 终端等
> 命令送进 guest 执行。

## 一、方案形态

OHOS 沙箱禁止 `exec` 外部程序，命令需要一个真实 Linux 环境来跑。qemu-agent：

- 进程内 `dlopen("libqemu-system-aarch64.so")`，在独立线程上跑一个 Linux guest
  （TCG 软件模拟，无硬件虚拟化加速）。
- guest 内嵌 SSH server（`ssh-agentd`，russh）；host 用 SSH 连接池把命令送进
  guest 执行（命令输出/退出码经 SSH channel 返回）。
- 文件共享用 **virtio-fs**（host 进程内 virtiofsd + guest `mount -t virtiofs`），
  不用 9p。

## 二、命令执行链路

```
util::command            （后端无关，只依赖 command_executor）
  -> command_executor::executor()         （全局注册的后端 executor）
  -> SshCommandExecutor                   （qemu-ssh-agent，russh SSH client）
  -> slirp hostfwd 127.0.0.1:2222
  -> guest 内 ssh-agentd                   （russh server，执行命令）
```

- QEMU 用 `-netdev user`（slirp）+ DHCP；hostfwd 只映射到回环，命令通道不外露。
- 一条管理 virtio-serial（`zcoder.ssh.mgmt`）只用于**引导**：guest 的
  `ssh-agentd` 经它上报 `SshInfo{port, client_private_key_pem, pid_dir}`；
  host `bootstrap` 读到后经 QMP `hostfwd_add` 建立 `127.0.0.1:2222 → guest ssh
  端口`，喂给连接池。之后命令全部走 SSH，virtio-serial 不再承载命令。
- 每条命令 = 一条 SSH exec session：`command.rs` 把 binary/args/env/cwd/modes
  拼成 POSIX sh 命令串（stdin/stdout/stderr 走 SSH channel）；退出码经 SSH
  exit-status 返回。
- 信号：guest 侧把进程组 leader pid 记入 pid 文件，host `kill -KILL -<pgid>`
  （`util::command` 只发 SigKill）。
- SSH 连接长连接复用（连接池），避免每次命令握手成本。

## 三、文件共享（virtio-fs）

- 静态两挂载（QEMU 启动时建好）：
  - `sandbox`：设备应用沙箱根 `/data/storage/el2/base` → guest `/sandbox`
    （进程内 virtiofsd，PassthroughFs）。
  - `tools`：HAP resfile（只读）→ guest `/tools`（PassthroughFsRo），承载预装
    工具（见第六节约束）。
- 动态工作目录（打开文件夹时）：`SshCommandExecutor::mount_folder`
  1. 幂等检查（已挂载集合去重）；
  2. 进程内 virtiofsd（`fs_work*` 后端 socket）+ QMP 热插 `vhost-user-fs-pci`
     （预置 8 个 pcie-root-port 槽位）；
  3. SSH 内 `mkdir -p` + `mount -t virtiofs <tag>` 到 guest 上**与设备路径同名**
     的挂载点 —— 保证 LSP 缓存里保存的路径跨重启稳定。
- `unmount_folder` 只摘已挂载集合，不真 umount（防止 LSP 正在扫描时目录被卸）。
- 单文件不挂载：单文件授权不足以为 fsdev 提供目录根；编辑走授权 fd。

## 四、路径映射

- host 侧 `command.rs` 的 `map_guest_path` / `map_guest_arg`：只把
  `/data/storage/el2/base/...` 前缀改写为 `/sandbox/...`；`--flag=<path>` 内联
  参数同样改写。`/storage/Users/currentUser/...` 工作区在 guest 挂载同名路径，
  直接透传。
- `util::command` **无**路径映射代码（无 `ROOT_MAP` / `path_arg_indices`）。

## 五、后端独立与代码结构

`command-executor` 叶子 crate（`crates/gpui_ohos/depend/command-executor`）承载
后端无关抽象：`ExecSpec`/`FdMode`/`Signal`/`RemoteChild`/`ExitFuture` +
`RemoteCommandExecutor`/`FolderMounter` + 全局 `OnceLock` 注册
（`init_executor/executor/init_mounter/mounter`）。`util` 与 `workspace` 只依赖
它，故两个后端在 `util` 面前无差异、可单独开/关编译。两后端各自的
linker 只负责把具体 executor 注册进 `command_executor`。

qemu 侧 crate（`crates/gpui_ohos/depend/ohos-qemu-agent/`）：
- `ssh-agent/qemu-ssh-agent`（host）：启动 guest、bootstrap、`SshCommandExecutor`
  （实现 executor 与 folder mounter）、连接池、QMP、进程内 virtiofsd。
- `ssh-agent/qemu-ssh-agentd`（guest）：`ssh-agentd` 二进制（russh server）。
- `ssh-agent-linker`：把 `SshCommandExecutor` 桥到 `command_executor` 注册。
- `images/`：`libqemu-system-aarch64.so` + 依赖 so、`Image`、`rootfs.cpio.zst`、
  `tools.tar.zst`。

编译开关：`launch-zed` 特性 `qemu-agent`（构建用 `--no-default-features
--features qemu-agent`），即 `script/bundle-ohos --qemu`。

## 六、构建与产物

`bundle-ohos --qemu`：编译带 `qemu-agent` 特性的 libzcoder.so → 编译 guest 的
`ssh-agentd` → 把 qemu 专用资源打进 hap：

- `libs/arm64-v8a/`：`libqemu-system-aarch64.so` + 6 个运行依赖 so（slirp /
  zlib / pixman / glib / intl / pcre2）。
- resfile：`Image`、`rootfs.cpio.zst`、`ssh-agentd`、`tools` 工具树
  （`bin/`、`lib64/` 等展平，guest 以只读 `/tools` 挂载）。

默认 `bundle-ohos`（openeuler-agent）会走清理分支移除上述全部 qemu 资源，
保证默认 hap 不含 qemu 产物。

## 七、方案的硬约束（成本 / 风险）

### 7.1 额外一个 Linux guest：CPU 占用偏高、耗电高

qemu-agent 在设备上多跑一个完整 Linux 系统（当前配置 4 vCPU / 8G 内存的 TCG
软件模拟）。代价：

- **闲时**：QEMU/guest 常驻，即使 guest 以 `idle=halt` 压低空转，模拟器自身的
  线程轮询与虚拟化开销仍使整机**闲时 CPU 占用明显偏高**。
- **忙时**：TCG 翻译执行（非原生）放大 guest 内命令 / LSP 的 CPU 开销。
- **结果**：整机 CPU 占用升高、耗电变大，对移动 / 嵌入式 OHOS 设备影响明显。

这是"在设备内再造一个 Linux"的本质代价，无法用配置完全消除。

### 7.2 LSP 生态工具链必须预装进 guest：体积巨大、很难更新

要让 guest 支撑真实 LSP 负载，必须在 guest 里预装配套工具链：

- 语言服务依赖的编译器 / 工具：rust-analyzer 需要 `cargo`/`rustc`；
  clangd 需要标准库头文件（libc / C++ 头等）；gopls 需要 go 编译器；还有
  node / python / 扩展二进制等。
- **体积**：单个工具链即数百 MB 起步（rust 工具链、go、各类头文件累计更大），
  整体会非常庞大；HAP / resfile 体积与安装 / 解压预算都吃紧。
- **更新难**：guest 内没有包管理器；工具升级要手工重建 rootfs / tools 归档再
  随版本发布，无法像 openeuler 后端那样在现成 Linux 上按需安装 / 升级。

对比 `openeuler-agent` 后端：直接复用现成 Linux 虚拟机（有软件源，可安装、可
更新），这是 openeuler 的相对优势；qemu-agent 的自包含换来的是上述自维护成本。

## 八、结论

qemu-agent 提供进程内、免外部 VM 依赖的自包含执行环境，guest 固定可控；但
第七节两项约束（CPU/耗电、工具链体积与维护）决定了它代价较高。是否需要扩展
支持的 LSP 语言时，应权衡是否继续扩充 guest 工具集，或改用 openeuler 后端。
