# ohos-qemu-ssh-agent：用 SSH 替换 virtio-serial 命令执行通道（设计）

> 现状：`crates/gpui_ohos/depend/ohos-qemu-agent/` 的 cmd-agent/cmd-agentd 用 virtio-serial
> 端口池（14 数据 + 14 err + 1 mgmt）在 QEMU guest 里执行命令，连接管理复杂且不稳定
> （virtio-serial 端口 host 断开重连必失败，必须 host 长连接复用）。文件共享已由
> **virtio-fs 替换 9p**（见 2026-09-02-virtio-fs-replace-9p-design.md，stat 快 204 倍）。
> 本设计改为：guest 内 **ssh-agentd 内嵌 russh server**（全 Rust SSH server，tokio 驱动），
> host 侧 cmd-agent 用 SSH 连接池执行命令；virtio-serial 只保留一条**管理串口**（引导 +
> 监督更新，host 长连接、**永不关闭**）。
>
> 状态标记沿用现有 DESIGN.md 约定：`[已定]` 已确认决策；`[待验证]` 需实测确认。
>
> 核心约束：**util::command 零改动**；**旧代码（ohos-qemu-agent 现有 cmd-agent*）不破坏**，
> 新实现全部放新目录 `ohos-qemu-agent/ssh-agent/`。

## 术语

- **cmd-agent**（host，zcoder 进程内）：新 SSH 版命令执行器，package `qemu-ssh-agent`。
- **cmd-agentd**（guest，QEMU 内）：新引导代理，package `qemu-ssh-agentd`，bin 名 `ssh-agentd`。
- **russh server**：guest 侧 SSH server 用 russh 的 server 模式实现（`russh::server`，tokio
  驱动），内嵌在 ssh-agentd 进程内（与 virtiofsd 内嵌同模式），全 Rust、无 C 依赖。
- **russh-keys**：russh 的密钥子库（`russh::keys::ssh_key`），生成/序列化 host key 与 client
  ed25519 密钥对。
- 目录命名遵循现有 cmd-agent / cmd-agentd 规则（与 openeuler 目录区分：ssh-agent 子目录）。

## 1. 总体架构 [已定]

```
 util::command (ohos.rs，零改动)
      │  RemoteCommandExecutor / FolderMounter（复用现有 cmd-agent-linker trait）
      ▼
 ┌──────────────────────────────────────────────┐
 │ cmd-agent (qemu-ssh-agent)  zcoder 进程内       │
 │  ├ qemu-machine 线程：dlopen 引擎跑 QEMU         │
 │  ├ 主流程：串口引导 → hostfwd → 启动连接池 → 就绪  │
 │  ├ virtiofsd 线程（静态 /sandbox /tools，进程内）  │
 │  ├ 连接池线程：共享 tokio runtime，维护 N 条就绪   │
 │  │   SSH 连接(127.0.0.1:H)，跑每命令 wait() 泵   │
 │  └ executor：allocate 连接 → 开 channel →        │
 │       管道接 util::command 流                    │
 └──────┬───────────────────────────────┬────────┘
        │ SSH (N 条连接池)                │ QMP (device_add vhost-user-fs / hostfwd)
        ▼                                │
   ssh-agentd 内嵌 russh server (guest 0.0.0.0:P, 密钥认证)   │
        ▲                                │
        └── 管理串口(仅引导+监督更新)：SshInfo{port, client私钥, host key}
```

数据流：util::command 流 ↔ OS 管道 ↔ wait() 泵任务 ↔ russh channel ↔ dropbear ↔ 进程。
泵任务是纯字节中继，无协议翻译；退出码来自 SSH 原生 exit-status。

QEMU 启动参数（精简后）：保留 kernel/initrd、**virtio-fs tools+sandbox 静态挂载**、
slirp netdev（供 hostfwd）、QMP、pcie-root-ports（动态挂载热插拔用）；**去掉 14 数据 +
14 err + 1 mgmt 端口池**，只留 1 条管理串口 `zcoder.ssh.mgmt`。

## 2. 目录与模块划分 [已定]

新目录 `crates/gpui_ohos/depend/ohos-qemu-agent/ssh-agent/`，含两个 crate。依赖已有
`cmd-agent-linker`（trait）与 `cmd-agent-protocol`（ExecSpec/Signal 类型），只读不改。
**无独立协议 crate**：串口引导消息两侧各放 ~30 行重复定义。

**cmd-agent（host，package `qemu-ssh-agent`）**

- `lib.rs` — crate 根；QEMU 启动：dlopen 引擎、qemu-machine 线程、精简 argv。
- `bootstrap.rs` — 串口引导客户端：连串口→收 SshInfo→QMP hostfwd_add→更新连接池配置；
  持续监听，收到更新（dropbear 重启）时重配连接池。
- `pool.rs` — 连接池（合并补池）：一个线程 + 共享 tokio runtime；维护就绪连接（弹出即删）、
  `allocate()` 接口、暴露 runtime handle 供 executor spawn 泵任务。**单一持有方向 executor→pool，
  无互相持有句柄**（泵定义在 executor.rs）。
- `executor.rs` — `RemoteCommandExecutor` 实现：spawn/signal/try_exit/wait_exit_async；
  极简 session 表（退出码 + pid 文件路径，命令结束即删）；**含每命令 wait() 泵任务**。
- `command.rs` — ExecSpec → shell 命令串（第 6 节详述）。
- `mounter.rs` — `FolderMounter` 实现：QMP chardev-add + device_add vhost-user-fs-pci
  + SSH `mount -t virtiofs`（同路径挂载，第 7 节）。
- `qmp.rs` — QMP 客户端：chardev-add / device_add / netdev_add / netdev_del /
  human-monitor-command（hostfwd_add/remove）。
- `virtiofs.rs` — 进程内 virtiofsd 后端（静态 + 动态，从现有 cmd-agent 迁移复用）。

**cmd-agentd（guest，package `qemu-ssh-agentd`，bin `ssh-agentd`）**

- `main.rs` — 引导：生成密钥（russh-keys）→ 启动 russh server → 推 SshInfo；tokio runtime 驱动。
- `serial.rs` — 管理串口引导服务。
- `keygen.rs` — 生成 host key + 一次性 client ed25519 密钥对（russh-keys）。
- `server.rs` — russh server（随机端口 P、绑定 guest 0.0.0.0、纯密钥认证、禁密码）。
- `exec.rs` — exec_request 处理：spawn 命令（显式 process_group）→ 桥接 stdio/退出码。

## 3. 线程划分 [已定]

**host（线程组）**
1. `qemu-machine` — 跑 QEMU 引擎。
2. 主流程 — 启动：串口引导 → hostfwd → 启动连接池线程 → 就绪。就绪后 executor 的方法由
   util::command 在调用线程上直接进入（executor 是线程安全共享对象）。
3. `pool` 线程 — 持共享 tokio runtime：后台申请 SSH 连接维持池子（<5 时补满 16）；
   `allocate()` 从池弹出；为每条命令跑 wait() 泵任务。**新连接建立永不在调用线程做**。
4. `mount-deferred` — guest 未就绪时延迟挂载（沿用现有模式）。
5. `virtiofsd` 后端线程 — 每个共享目录一个（/sandbox、/tools 启动即起；工作目录挂载时动态起），
   从现有 cmd-agent 的 virtiofs.rs 迁移复用。

**guest**：ssh-agentd 单进程（tokio runtime），russh server 异步处理各 SSH 连接，exec 命令
经 exec_request spawn 子进程。

## 4. 管理串口引导协议（极简，无独立 crate）[已定]

复用一条 virtio-serial 端口（`zcoder.ssh.mgmt`），长度前缀 JSON 帧。消息两侧重复定义：

- server→client：`HelloOk { version }`、`SshInfo { port: u16, client_private_key_pem: String, pid_dir: String, sshd_pid: u32 }`
- client→server：`Hello { version }`、`SshInfoAck { sshd_pid: u32 }`
- 错误：`Error { message }`

**连接生命周期 [已定]**：管理串口是 host 侧 bootstrap 持有的**永久长连接**，QEMU 生命周期内
**永不关闭**（virtio-serial 端口 host 断开重连必失败，见 virtio-serial 只服务首次连接的已知坑）。
bootstrap 在串口 socket 出现即开始连接并重试（socket 在 cmd-agentd 就绪前就存在）。因串口永不
关闭，不存在运行中断线重连问题；若串口异常断开视为致命错误，走整体重启 QEMU 路径。

流程：
1. **时序保证（无消息丢失）**：guest 侧 cmd-agentd 把当前 `SshInfo` 保存在内存，**收到
   `Hello` 即回带**，幂等，不依赖推送时机。
2. ssh-agentd 用 russh-keys 生成 client ed25519 密钥对（公钥注入 authorized_keys）与 host key，
   用 russh server 在随机端口 P 监听（绑定 guest 0.0.0.0）。
3. cmd-agent 连上串口 → `Hello` → 收到 `HelloOk` + 当前 `SshInfo` → 回 `SshInfoAck`。
4. cmd-agent 按 SshInfo → QMP hostfwd_add `tcp:127.0.0.1:H-:P`（**H 固定**，冲突换 H 重试）→
   连接池按 {H, 私钥} 连接。
5. **russh server 崩溃重启（H 固定，端口变 P'）**：ssh-agentd 重新生成密钥+随机端口 P' 并主动推新
   `SshInfo`。cmd-agent 先 `hostfwd_remove tcp:127.0.0.1:H` 再 `hostfwd_add tcp:127.0.0.1:H-:P'`
   （remove 目标不存在则忽略；add 失败短重试），随后清空连接池重配（在途命令失败语义见第 5 节）。

安全：client 私钥走管理串口不经网络（virtio-serial 天然不被网络可见）；hostfwd 只绑 host
127.0.0.1；russh server 禁密码登录、纯密钥认证。hostfwd 的 guest 侧绑定 0.0.0.0（slirp 只能把
hostfwd 连接送到 guest 网络地址，送不到 guest 127.0.0.1 回环，后者会被 guest 内核当火星包丢弃）。
管理串口 guest 侧设备节点 `/dev/vport*` 的读写权限由 ssh-agentd 独占（仅 root），防沙箱内
其他进程截获私钥。

### 4.1 密钥与 host key 校验 [已定]

- **host key**：ssh-agentd 用 **russh-keys**（`russh::keys::ssh_key::PrivateKey::random`，
  ed25519）生成，序列化为 OpenSSH 私钥文本，传给 russh server 的 `Config::keys`。host key
  存 guest 可写路径（initramfs 只读时落 tmpfs 或 /sandbox 下目录）。
- **client key**：ssh-agentd 用 russh-keys 生成一次性 ed25519 client 密钥对；公钥序列化为
  OpenSSH authorized_keys 行，注入 russh server 的认证检查；私钥文本随
  `SshInfo.client_private_key_pem` 经管理串口下发 host 侧连接池。
- **host key 校验 [已定]**：连接池的 russh `ClientHandler::check_server_key` 返回 `Ok(true)`
  （AcceptAll），沿用 openeuler-agent `cmd-agent/src/deploy.rs` 的 `AcceptAllHandler` 先例。
  理由：hostfwd 只绑 host 127.0.0.1（非公开网络）、client 私钥经 virtio-serial 一次性下发、
  场景内没有中间人攻击面。russh server 重启时 host key 重新生成，AcceptAll 使其不受影响。
- **依赖**：russh-keys 随 russh 依赖一并引入，无外部软件依赖。

## 5. 连接池与数据桥 [已定]

**连接池（轻量化，warp-ohos 风格）**
- 池 = `Mutex<VecDeque<Conn>>`，只存就绪连接；`allocate()` = 弹出（无 flag、无跟踪）。
- 命令结束 → channel 关闭 → 连接随泵任务 drop（系统自动回收，不回收不返回）。
- pool 线程后台补池：**池 <5 条时申请补满 16 条**（每次新连接带重试）——长驻进程（LSP）
  占用连接由补池机制覆盖，不会出现"池被占光"；唯一代价是并发 SSH 连接数上限 16，
  多工作区多 LSP 时 spawn 可能短等（≤1s）而非立刻取到。
- spawn 调用线程只做弹出 O(1) + 有界等待（池空等 ≤1s 或失败）。
- **池重配语义（russh server 重启，见第 4 节）**：收到新 SshInfo 时清空池内全部就绪连接（已失效）；
  正在执行的命令，其泵任务检测到 channel 断开 → 关闭管道写端（stdout/stderr EOF）并记录退出码
  `None` → 调用方 `wait_exit_async` 得 None → 128，命令以失败结束（明确失败，不静默挂起）。

**数据桥（诚实结论：数据移动不可避免，但做得很轻）**
- russh 0.55 是 tokio 硬编码（channel 流为 `tokio::io::AsyncRead/AsyncWrite`），
  util::command 要 `smol::io::AsyncRead/AsyncWrite`，两 trait 不兼容。
- 退出码、stderr 来自 channel 的 `wait()` 事件流（Data/ExtendedData/ExitStatus/Eof/Close），
  必须有一个 wait() 循环消费。
- 因此每命令一个 **wait() 泵任务**（tokio，跑在 pool 线程 runtime），对应 warp-ohos
  ExecCmdMain 的 `libssh2_channel_read → write_all(pipe)` 循环的异步版，约 50 行纯字节中继。

spawn（调用线程，同步，有界 2s 超时）：
1. `pool.allocate()` 弹出一条连接。
2. 该连接上开 channel（russh `channel_open_session` + `exec`，经 runtime handle 有界驱动）。
3. 建 3 对管道（stdin/stdout/stderr），返回 `RemoteChild`（smol `Async<pipe>`）。
4. 泵任务 wait() 循环：

```
loop {
  select! {
    msg = channel.wait() => {
      Data{d}              → 写 stdout 管道
      ExtendedData{stderr} → 写 stderr 管道
      ExitStatus{code}     → 记录退出码 Some(code)
      ExitSignal{..}       → 记录退出码 None（信号死，见第 8 节）
      Eof | Close | None   → 退出循环
    }
    管道 stdin 有数据 → channel.data()；EOF → channel.eof()
  }
}
// 关闭管道写端，通知 wait_exit_async
```

管道天然提供背压（util::command 不读 → 管道满 → 泵停读 channel → SSH 流控停发）。
**跨 runtime 注意**：泵任务跑在 pool 线程的 tokio runtime，但管道是 `smol::io::Async<pipe>`。
对管道的写入用**阻塞写**（`spawn_blocking` 或裸 fd write），不在 tokio 上下文 await smol future，
避免跨 runtime 的 reactor 注册/唤醒兼容问题。

## 6. 指令翻译与环境变量 [已定]（成败关键）

ExecSpec → shell 命令串（POSIX sh）：

```
mkdir -p <cwd> && cd <cwd> && PATH=/tools/bin:/usr/bin:/bin LD_LIBRARY_PATH=/tools/lib64 \
HOME=<沙箱内持久目录> VAR1='...' VAR2='...' exec <binary> <args...> [</dev/null] [>/dev/null] [2>/dev/null]
```

- **路径：工作区同路径原样透传，沙箱路径例外**（`/data/storage/el2/base` 前缀须改写为
  `/sandbox`，含 `--flag=<path>` 内联；见 7.1 修正一）。
- **cwd 自动创建**：`mkdir -p`（对应现有 create_dir_all）。
- **PATH / LD_LIBRARY_PATH 必须显式注入**：SSH 会话环境为空，现状是命令继承 cmd-agentd 环境
  （rcS 设了 `PATH=/tools/bin:...` 和 `LD_LIBRARY_PATH=/tools/lib64`，clangd/python3 依赖后者）。
- **HOME 固定注入**：指向沙箱挂载下的持久目录（/sandbox/home）。
- **git 时注入** `GIT_CONFIG_COUNT=1 / GIT_CONFIG_KEY_0=safe.directory / GIT_CONFIG_VALUE_0=*`
  （virtio-fs passthrough 所有权 → dubious ownership，与 9p 时同一问题，现有补丁保留）。
- **FdMode**：Null → `</dev/null`/`>/dev/null`/`2>/dev/null`；Piped → 交给 channel。
- **shell 转义**：binary/arg/cwd/env 值单引号包裹 + `'\''` 转义。
- **`exec` 前置**：二进制替换 shell，退出码即二进制退出码，信号语义干净。
- **pid 前缀（信号用，第 9 节）**：命令串固定带 `echo $$ > <pid_dir>/<session>.pid; exec ...`
  前缀（`exec` 保 pid，pid==进程组 id，供 `kill -KILL -<pid>` 信号组；`pid_dir` 由 cmd-agentd
  创建并随 SshInfo 下发）。
- **环境覆盖核对**：SSH 会话环境为空，任何遗漏都以难以察觉的方式失败。实现时**逐项对比旧
  cmd-agentd 继承的 guest rcS 环境**（PATH / LD_LIBRARY_PATH / HOME / TMPDIR / LANG 等）与注入
  列表，确认无遗漏（见第 11 节 #10）。

### 6.1 加密选择 [已定，none 待验证]

SSH 2.0 传输层默认强制加密。可选三档（按 TCG 软模拟下开销从低到高）：

1. **none cipher（不加密）**：最快。russh 的 client 与 server 均支持 `NONE`/`CLEAR` cipher
   （`clear.rs` 实现）。russh server 的 `Config` 把 cipher 列表设为只含 `none` 即只协商不加密。
   安全性：纯公钥认证 + hostfwd 只绑 host 127.0.0.1、密钥一次性下发、非公开网络，
   中间人攻击面可忽略。**russh server 侧 none cipher 协商待实测**（#12）。
2. **chacha20-poly1305@openssh.com**：双方默认支持，TCG 软模拟下无 AES 硬件加速，
   chacha20 是软件最快 AEAD。默认回退档。
3. **aes128/256-ctr**：无 AES-NI 时比 chacha20 慢，仅当协商失败时用。

**决定**：实现时优先测 none cipher；若 russh 无法协商，回退 chacha20-poly1305。

## 7. 挂载（基于 virtio-fs；工作区同路径、沙箱前缀映射）[已定]

文件共享沿用已落地的 virtio-fs 方案（见 2026-09-02-virtio-fs-replace-9p-design.md），
SSH 只替换命令通道，不替换文件系统。

- **工作区无 path_map，沙箱前缀映射**：guest 挂载点与 OHOS 工作区路径逐字符一致，工作区命令路径
  无需改写；仅沙箱路径 `/data/storage/el2/base` 例外（guest 挂 `/sandbox`，见 7.1 修正一）。
- **/sandbox、/tools 静态挂载保留**：QEMU 启动参数带 vhost-user-fs-pci，进程内 virtiofsd
  后端线程导出（/sandbox、/tools 挂载点与 virtio-fs 方案完全一致，不拆 /sandbox）。
- **挂载点与 virtio-fs 方案一致**：工作目录仍挂到设备路径同名 guest 路径，保证 LSP 索引
  缓存跨重启有效。
- `mount_folder(path)`：
  1. 进程内 virtiofsd 后端动态起一个（`spawn_workdir`，监听 fs_work<N>.sock）。
  2. QMP `chardev-add`（client socket → fs_work<N>.sock）+ `device_add vhost-user-fs-pci`
     （tag=ztag<N>，挂 rp<N> 根端口），沿用 virtio-fs 方案已实测的
     `create_workdir_vhost_fs`。
  3. SSH 执行 `mkdir -p <path> && mount -t virtiofs ztag<N> <path>`（带 5s 重试窗，
     对应现有 mount retry）。
  4. 成功登记已挂载集合（幂等）。
- `unmount_folder`：fire-and-forget，只删已挂载集合，不 umount（沿用现有决策）。

### 7.1 落地修正（2026-09-03）

**修正一：并非完全"无 path_map"——沙箱路径需前缀映射。** 对**工作目录**"同路径挂载、无需改写"
成立；但 LSP / 下载的二进制落在**设备沙箱** `/data/storage/el2/base/...`（guest 侧该前缀被挂到
`/sandbox`，设备上不存在 `/data/storage`），因此命令的 binary/args/cwd 若以该前缀开头，必须改写为
`/sandbox`（`command.rs` 的 `map_guest_path` / `map_guest_arg`，后者处理 `--flag=<path>` 等号内联）。
曾因未映射导致 rust-analyzer 下载产物在 guest 侧 `exec ...: not found`。工作区路径（
`/storage/Users/currentUser/...`）与 `/tools` 路径仍原样透传。

**修正二：进程内 virtiofsd 的 fd 生命周期必须靠 guest 周期回收 dcache。**
落地后出现周期性崩溃（SIGABRT），表层 wgpu `Out of Memory` / `Invalid surface`，实为进程 fd 耗尽
（顶满 OHOS 的 32768）：virtiofsd 为 guest 每次 lookup 的 inode 开一个 `O_PATH` fd 存无界 inode
缓存，释放只靠 guest 发 `FUSE_FORGET`；而 guest 内存充足时 dcache 从不 shrink，guest git 遍历
海量未 gitignore 文件（如 cargo `build/`）后 fd 只增不减。file-handles 方案（`--inode-file-handles`）
被 OHOS seccomp 以 SIGSYS 硬杀不可用。**解法**：在 guest 侧执行代理（`qemu-ssh-agentd`）加后台任务
每 15s 写 `/proc/sys/vm/drop_caches = "2"`，强制 guest 回收 dcache → 触发 FORGET → virtiofsd 释放 fd。
详细排查链见 bugfix 报告与 ohos-dev-guild。**任何在进程内内嵌 virtiofsd 的共享目录方案都必须带这个
guest 侧周期回收，否则大目录遍历迟早把进程 fd 吃光。**

### 7.2 落地经验：russh server 的 channel 生命周期（qemu-ssh-agentd）

- **`channel_eof` 必须实现**：client 侧发 EOF（关闭 stdin）时，须把运行子进程表里该 channel 的
  stdin 写端移除（drop `ChildStdin`）。否则行式/批式子进程（`git cat-file --batch-check`）读不到
  EOF 永不退出 → exec channel 永不关 → 等待完整输出的调用方（git panel 的 `compute_snapshot`
  `revparse_batch`）永久挂起。曾实测 git panel 空白即因此。
- **`channel_close` + `kill_child` + `ChildHandle.pid` 不可加**：曾补齐这三个（在 channel_close 时按
  pid 强杀子进程）后，`util::command` 全线异常——因为在子进程**真正退出前**不应提前关闭其 stdin 写端
  （长驻 LSP 如 clangd 读 stdin 遇 EOF 会报 transport error），且信号统一走第 9 节"pid 文件 +
  `kill -KILL -<pid>`"机制，不需要也不应依赖 `ChildHandle.pid` 额外信号路径。正确形态：保留
  `channel_eof` 只删表项；`ChildHandle` 只持 stdin，不持 pid；子进程退出由 stdio 桥持续到
  `child.wait()` 完成。
- **每连接私有运行表**：russh `ChannelId` 是每连接复用的小整数，跨连接共享运行表会互相覆盖
  （后一命令插入同 channel id 时 drop 掉前一个 LSP 的 stdin 写端 → transport error）。每连接一个
  私有 `children: HashMap<ChannelId, ChildHandle>`，exec 前建连接、用完即断（连接池补池）。

## 8. 退出码 [已定]（对应问题 8）

1. `exec` 后二进制即 dropbear 子进程，退出码经 SSH `exit-status` 发出。
2. 泵任务从 wait() 事件流取退出结果，映射：
   - `ExitStatus{code}` → `Some(code)`；
   - `ExitSignal{...}`（子进程被信号杀）→ `None`（与现有 `status.code()` 为 None 语义一致）；
   - channel 直接 `Close` 且无 exit-status（连接断开等）→ `None`。
3. `wait_exit_async` → `Option<i32>` → util::command `status_from_code` 原样处理
   （`Some(code)→code<<8`，`None→128`）。
4. 语义差异记录：若 dropbear 对信号杀死的子进程回传 `exit-status=128+n`（而非 ExitSignal），
   返回码会是 137 之类而非 128，与现有 virtio-serial 行为（None→128）不同——实测确认
   dropbear 行为后在实现时对齐（见第 11 节 #5）。
5. 无自定义协议。

## 9. 信号 [已定：patch dropbear + kill 命令单一机制]

- util::command 只发 `SigKill`。
- **单一机制，不探测 dropbear 能力（二选一后固定）**：dropbear 对 SSH "signal" 请求的支持长期有限，
  直接采用 kill 命令——主路径即兜底，两条路径不再并存。
- **进程组保证（代码层，不依赖外部行为）**：russh server 是自研代码，exec_request spawn 命令时
  用 `std::os::unix::process::CommandExt::process_group(0)`（等价 libc::setpgid 使 pid==pgid）
  或 pre_exec 里 setsid，进程组语义由代码保证，不存在 C dropbear 的 setsid 回归风险。
- 命令串固定带 `echo $$ > <pid_dir>/<session>.pid; exec ...` 前缀（第 6 节）：
  `exec` 保 pid；spawn 时已显式建进程组，pid==pgid，`kill -KILL -<pid>` 即信号整个
  进程组（复刻现有 process_group(0) 语义）。
- signal 实现：从连接池取一条连接（空则短等 ≤1s 或返回失败，best-effort），开 channel 执行
  `kill -KILL -$(cat <pid_dir>/<session>.pid)`，用完即弃（drop，由补池线程补回）。
- 进程尚未 spawned（pid 文件不存在）时 kill 失败 → 返回错误，调用方可忽略
  （对应现有"命令尚未 spawned 时跳过信号"）。
- 兜底验证：实测 patch 后 child 的 pid==pgid（见第 11 节 #6）。

## 10. 修改点（授权范围）[已定]

- 新目录 `ohos-qemu-agent/ssh-agent/` 两个 crate：新建，不动旧代码。
- 主 workspace Cargo.toml 追加 2 个成员（qemu-ssh-agent / qemu-ssh-agentd）：仅追加
  （路径不含 ohos，需人工授权）。
- `launch_app.rs` 注册点由 `QemuCommandExecutor` 换成 `SshCommandExecutor`
  （该文件路径含 ohos，属可改范围）：唯一改动现有文件。
- guest 镜像：新增 ssh-agentd 二进制、init 脚本；旧二进制保留不动。（无 C dropbear，
  无需额外 C 软件。）
- russh 依赖：host/guest 两侧共享同一依赖配置
  `russh = { version = "0.55", default-features = false, features = ["ring", "flate2", "rsa"] }`
  + `tokio`（rt/net/time/sync）+ `russh-keys`（russh::keys），已有 openeuler-agent 先例可编 OHOS。
- **hostfwd 的 QMP 调用 [已定，两案实测后选]**：
  - 原生 QMP：`netdev_add`（`user` 后端支持 `hostfwd` 数组参数）+ `netdev_del`。重建 netdev 会
    中断该 netdev 上所有已有 SSH 连接，对 dropbear 重启路径（仅改 guest 端口）代价大。
  - HMP 兼容层：`human-monitor-command hostfwd_add/remove`，可单条增删 hostfwd、不动 netdev，
    需指定 netdev id（`hostfwd_add <netdev_id> tcp:127.0.0.1:H-:P`）。
  - 决定：**验证后优先原生 QMP**（QMP 是第一等公民、类型化参数）；若实测 netdev 重建导致
    连接中断不可接受（dropbear 重启路径频繁），回退 HMP 单条增删。两案均列待验证（#3）。

## 11. 待验证清单

1. hostfwd guest 侧绑定 0.0.0.0 才能被 slirp 送达（guest 127.0.0.1 会被内核当火星包丢弃）——
   设计已按此定，但需实测确认。
2. OHOS 沙箱内 QEMU slirp 能否在 host 绑定 `127.0.0.1:<host_port>`。
3. QMP hostfwd：原生 `netdev_add/remove` vs HMP `hostfwd_add/remove` 可用性；netdev 重建
   是否中断已有连接（决定两案取舍）。
4. ssh-agentd 在 initramfs 里的运行环境；authorized_keys / host key / client key 落在可写路径
   （initramfs 只读时用 tmpfs 或 /sandbox 下目录）。
5. russh server 对信号杀死子进程的退出上报：`exit-status=128+n` 还是无 exit-status/`ExitSignal`
   （决定第 8 节返回码对齐；russh server 由我们控制，预期走 ExitSignal）。
6. russh server exec spawn 用 process_group(0) 后 pid==pgid 实测（保证 `kill -KILL -<pid>` 信号组）。
7. 长驻进程（LSP）持有 channel 时连接池补池行为；daemonized 子进程持有 stdout 时
   channel 不关的边界情况（可加"退出码已到 + 输出超时后丢弃"兜底）。
8. 16 条并发 SSH 连接 + 每命令一连接，slirp 单连接吞吐下的压力测试。
9. hostfwd_remove 对不存在目标的幂等行为（配合第 4 节 dropbear 重启路径）。
10. 环境注入覆盖面：逐项对比旧 cmd-agentd 继承的 guest rcS 环境（PATH / LD_LIBRARY_PATH /
    HOME / TMPDIR / LANG 等）与注入列表，确认无遗漏。
11. 命令串长度（ARG_MAX）边界：多参命令是否超限。
12. russh 客户端能否协商 none cipher（决定第 6.1 节加密档位）。

## 12. 检视时对照

- 命名：cmd-agent / cmd-agentd，新目录 ssh-agent，package 带 ssh- 前缀。
- 命令通道：SSH（russh 连接池），无 virtio-serial 命令通道残留；virtio-serial 仅剩 1 条
  管理串口（永不关闭）。
- 串口：仅引导 + 监督更新，承载 {port, client 私钥}；幂等（Hello 回带当前 SshInfo）+ SshInfoAck，
  /dev/vport* 权限独占。
- 密钥：host key 与 client key 均用 russh-keys 生成（ed25519）；host key 校验 AcceptAll
  （沿用 openeuler-agent 先例）。
- 连接管理：弹出即删、不跟踪、pool 线程补池（<5 补满 16）；无心跳/无 reconcile/无端口回收；
  dropbear 重启时清空就绪连接 + 在途命令明确失败。
- 数据桥：每命令 wait() 泵 + 管道，纯字节中继（泵定义在 executor.rs，无 pool/bridge 互持句柄）；
  管道写用阻塞写，不在 tokio 上下文 await smol future。
- 路径：工作区同路径挂载、原样透传；沙箱路径前缀例外（/data/storage/el2/base → /sandbox，见 7.1
  修正一）；文件共享基于 virtio-fs（非 9p），进程内 virtiofsd 需 guest 周期回收 dcache（见 7.1 修正二）。
- 挂载：进程内 virtiofsd + QMP chardev-add/device_add + SSH `mount -t virtiofs`，幂等，
  不主动 umount；/sandbox、/tools 静态挂载保留。
- 指令翻译：PATH/LD_LIBRARY_PATH/HOME/git safe.directory 注入保留，exec 前置，shell 转义，
  pid 前缀固定（信号用）。
- 加密：优先 none cipher（dropbear 开 DROPBEAR_NONE_CIPHER），回退 chacha20-poly1305。
- 信号：russh server exec spawn 显式 process_group + kill 命令单一机制（pid 文件 +
  `kill -KILL -<pid>`）。
- 退出码：SSH exit-status（ExitSignal/Close→None）。
- util::command 与 linker trait：零改动。
