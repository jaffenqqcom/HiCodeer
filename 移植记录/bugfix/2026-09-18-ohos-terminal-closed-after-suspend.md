# OHOS 休眠后终端面板自动关闭（把子进程回收判据从心跳超时改为管理连接存活）

## 问题描述

在 OHOS 上使用终端时，设备休眠（或被系统冻结）之后再回来，终端会话会消失，表现为终端面板/item 自动关闭。

排查前排除了"代码里有主动断开"这个方向：daemon 侧不存在任何按空闲时间主动关会话的逻辑，PTY 与 shell 也没有超时退出路径。真正来源是 **hicodeerd 的客户端存活判据**：旧实现以"心跳是否按时到达"判断客户端实例是否还在，而设备休眠/进程被系统冻结期间客户端无法发出心跳，于是 daemon 判定实例已死，按"实例结束"回收该实例启动的全部进程组 —— 终端 shell 连同其子进程被 SIGTERM/SIGKILL，会话由此终结，面板随之关闭。

一句话概括根因：**判据建立在"用户态能按时跑起来"之上，而休眠冻结的正是用户态。**

## 问题表现

- 设备休眠（或应用被系统冻结）后回到前台，原先开着的终端会话消失，终端面板自动关闭
- 与用户是否在终端里操作无关；只要冻结时长超过判据窗口就必现
- 不休眠时一切正常；不冻结用户态任务的操作（如窗口失焦）也不触发
- 旧实现会把这次误判写进 daemon 日志，形态为：
  `conn: client <client-id> stopped heartbeating; took down N group(s)`
- 现象具有"整组消失"的特征：不只当前 shell，该实例启动的全部子进程一起没了（因为回收是按进程组做的）

## 问题原因

三条成因叠加，前两条是本次问题的来源，第三条是修复时才会暴露的隐藏坑。

### 1. 判据本身是"用户态心跳"（代码确证）

改动前的 `peers.rs`：

```rust
/// How long an instance may go unheard from before its session is treated as
/// over. A client polls the management listener every ten seconds, so this
/// tolerates three missed rounds.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the sweep looks for instances that have gone quiet.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
```

客户端每 10 秒对管理口（4023）发起一条**短连接**：认证 → `exec` bootstrap 命令 → 断开。每次认证都会走 `peers::touch()` 刷新 `last_seen`；daemon 侧 `spawn_sweeper()` 每 5 秒跑一次 `sweep()`，把 `now.duration_since(last_seen) >= IDLE_TIMEOUT` 的实例 `retire("stopped heartbeating")`。

也就是说，"实例还活着"这个判断完全依赖**客户端进程能按期执行到发心跳那一步**。

### 2. 休眠会冻住"发心跳的人"，而 socket 在内核里不变（机理层）

OHOS 在系统休眠时用 `nap-background` cgroup 冻结应用的用户态任务：客户端进程一行代码都跑不了，30 秒的窗口注定被突破。关键事实是 —— **socket 留在内核，冻结不产生 FIN**。所以从内核视角看"这条连接其实还在"，而旧判据根本没看连接，看的是用户态能不能按时爬起来发心跳。

于是判据把"活着但暂时不能说话"的实例判成了死。

### 3. 误杀的代价是整个进程组（代码确证）

`retire()` 对该实例记录的所有进程组先 `SIGTERM`，宽限 `TERM_GRACE = 1s` 后再 `SIGKILL`（`peers.rs` 的 `signal_groups` / `signal_now`）。终端 shell 及其全部子进程都在这个集合里 —— 这正是"终端会话消失、面板关闭"的直接原因。

即：**用户态被冻结这件事，经由 daemon 的误判，变成了"杀掉该用户的终端"。**

### 4. 换判据后才会暴露的坑：SSH 层会自己把连接断掉

改成"看连接在不在"之后还差一步。管理连接若因 SSH **rekey** 超时而断开，同样会被新判据读成"实例没了"，误杀照旧。russh 默认 `rekey_time_limit = 3600s`、`rekey_write/read_limit = 1 << 30`，而 rekey 需要双方应答 —— 冻结中的客户端答不了，连接就被自己这边断掉。

所以必须同时做到两件事：把 rekey 上限调到近似"永不"，并且**不给管理连接设 keepalive**。少了任何一条，新判据都会退化成同样的误杀。

## 解决方案

核心 insight：**换一个有"内核持久性"的存活信号。** 让客户端把管理连接**一直持有**（不再每轮新建短连接），只在这条连接上反复发 bootstrap 请求。于是"连接还在"等价于"进程还在" —— 因为这是一条 loopback 连接、双方都不会在实例活着时主动关它，唯一能结束它的就是**内核在进程消失时回收文件描述符（发 FIN）**。冻结不改变这一点，冻结因此不再被判死。

改动前（每轮一条短连接，靠轮询当心跳）：

```
client ──connect──▶ mgmt ──auth──▶ touch(last_seen)  ──exec──▶ 断开
       ... 10s 后重复 ...
daemon: sweep() 每 5s 检查 last_seen，超 30s ⇒ retire ⇒ SIGTERM/SIGKILL 整组
```

改动后（一条长连接，连接存在即存活）：

```
client ──connect──▶ mgmt ──auth──▶ management_opened(client_id, token)
       │  在同一条连接上反复 exec bootstrap（不再重连）
       └─ 进程消失 ⇒ 内核回收 fd ⇒ 连接结束 ⇒ handler drop
daemon: management_closed(client_id, token) ⇒ 最后一个 token 消失才 retire 整组
```

要点：

1. **状态表改造**：`Peer { last_seen, groups }` → `Peer { groups, management_conns: BTreeSet<u64> }`；删掉 `IDLE_TIMEOUT` / `SWEEP_INTERVAL` / `spawn_sweeper` / `sweep` 与 `last_seen`。实例存活 ⟺ `management_conns` 非空，**不再有任何计时器**。
2. **连接的挂点**：`ManagementHandler` 增 `client_id: Option<String>` 与 `conn_token: u64`，公钥认证通过时 `management_opened(user, token)`。连接的结束用 `Drop` 捕获 —— russh 0.55 的 server `Handler` trait 没有 `disconnected` 回调，handler 由 `russh_server::run_stream` 按值持有、连接结束即 drop，这是唯一可靠挂点。最后一个连接 drop 时 `retire(client_id, "management connection closed")`。
3. **多客户端隔离**：`conn_token`（来自 `NEXT_MANAGEMENT_CONN`）区分同一实例的多条连接，避免"重连过程中旧连接关闭"误 retire 一个仍被新连接担保的实例；同时删掉旧 `touch()` 里"新身份顶掉全部旧身份"的行为，改为只在表里缺失时插入，不再触碰其它身份。这是"多个客户端并存"的必要前提：某客户端退出只回收它自己启动的进程组，不影响其它客户端。
4. **rekey / keepalive**：两端（daemon 的两个 listener 与 client 侧）统一 `russh::Limits::new(REKEY_BYTE_LIMIT, REKEY_BYTE_LIMIT, REKEY_TIME_LIMIT)`，时间上限取 `365 * 24 * 3600` 秒（"近似永不"，且不用 `Duration::MAX`，留余量并保持 `Debug` 可读）；**管理连接不设 keepalive**。池化的命令连接保留 30s keepalive —— 丢一条命令连接本身不代表实例没了，也就不会回收任何东西。
5. **daemon 退出时清场**：新增 `retire_all("daemon exiting")`，由 `SIGTERM`/`SIGINT`/`SIGHUP` 或两条 accept 循环结束触发。"子进程不得比它的 owner 活得久"这条约束在多客户端模型下更需要显式成立。
6. **补齐多客户端所需的每客户端状态**：`TMPDIR` 从全局 `OnceLock` 一份改为按 client_id 各一份（`session_tmp::adopt(client_id, root)` / `tmpdir(client_id)`），并在 spawn 点按**发起该命令的客户端**注入（`exec.rs` 的 `spawn_command`、`pty.rs` 的 `run_pty_shell`），而不是写进 daemon 自己的进程环境。清点结论：cmd-client 经管理 bootstrap 传递的客户端级参数**只有 `HOME`（→ data_root → `TMPDIR`）这一个**；`cwd` 走每命令的 `ExecSpec.cwd_path`，`TERM`/`cols`/`rows` 走每连接的 pty 请求，`PATH` 与 `HOME` 是刻意不转发的。

模型前提（决定本方案成立与否，务必保留）：**该等价关系依赖两端都在同一台机器的 loopback 上**。若将来 daemon 与客户端分处两台机器，连接可能因与进程无关的原因断开，就必须重新引入宽限期，不能再用"连接断 ⟺ 进程没了"。

兼容性（装包时要注意）：**旧 app + 新 daemon 是危险组合** —— 旧客户端的短连接会让 daemon 每轮都触发一次 `management_closed` → retire，把子进程杀光；反向的"新 app + 旧 daemon"是安全的（旧 daemon 只是在同一条连接上处理 exec，把短连接换成保持住的长连接不改变它的行为）。两者的实现同装在一个 HAP 内，正常装包不会产生危险组合。

验证：

- `cargo check`（`aarch64-unknown-linux-ohos`，需带 `script/bundle-ohos` 导出的 `RUSTFLAGS`，否则 `.cargo/config.toml` 的 `--cfg rustix_use_libc` 失效会产生假报错）通过，两个 crate 无新增告警。
- `script/bundle-ohos` → `=== HAP BUILD SUCCESSFUL ===`。
- `./install-local.sh --reinstall` 卸载 / 安装 / 启动均成功。
- **生效前提（容易踩）**：hicodeerd 不在 HAP 内，它是设备侧**由交互式 shell 手动拉起的常驻进程**（本机实测父进程为 `/usr/bin/zsh`，非系统托管），因此 `--reinstall` 只换磁盘上的 `.hnp`，**不会**重启已在跑的进程。实测重装后磁盘二进制已换新（`/data/service/hnp/hicodeerd.org/hicodeerd_0.1.0/bin/hicodeerd`，mtime `2026-09-18 23:15:50`，大小与本地 stage 产物一致），但当时运行中的进程仍是旧文件（可执行映射 inode `2750446` ≠ 磁盘 `1255434`）。**必须显式重启 hicodeerd 或重启设备，新判据才生效。**
- 生效判据（比 `ps` 的时间列可靠）：读 `/proc/<pid>/maps` 里可执行文件的映射 inode，与磁盘文件 inode 比对。重启后复核：运行中的 hicodeerd（pid 21924）映射 inode 为 `1255434`，与磁盘一致 —— 新判据已生效。
- 实机"休眠 → 回来"的复现：用户判定该问题不再出现。

## 修改文件

- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/peers.rs` — 重写存活模型：删 `IDLE_TIMEOUT` / `SWEEP_INTERVAL` / `spawn_sweeper` / `sweep` / `last_seen`；`Peer` 改记 `management_conns`；新增 `new_management_token` / `management_opened` / `management_closed` / `retire_all`；`touch` 不再顶掉其它身份
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs` — `ManagementHandler` 增 `client_id` / `conn_token`；认证成功时登记 `management_opened`；新增 `Drop` 实现调 `management_closed`；`exec_request` 改为 `session_tmp::adopt(client_id, root)`
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/main.rs` — 新增 `REKEY_BYTE_LIMIT` / `REKEY_TIME_LIMIT` 并应用到两个 listener；删 `peers::spawn_sweeper()`；退出路径统一 `peers::retire_all`，新增 `ShutdownSignals`（SIGTERM/SIGINT/SIGHUP）
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/session_tmp.rs` — `TMPDIR` 从全局 `OnceLock` 改为按 client_id 的 `HashMap<String, PathBuf>`；`adopt(client_id, root)` / `tmpdir(client_id)`
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs` — `spawn_command` 按发起命令的 client_id 注入 `TMPDIR`
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/pty.rs` — `run_pty_shell` 同样注入 `TMPDIR`
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/Cargo.toml` — tokio 增加 `signal` feature
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/bootstrap.rs` — bootstrap 改为在同一条长连接上轮询（`session: Option<SshSession>`），失败即丢弃该连接；新增 `connect_management`（不设 keepalive、rekey 上限调大）
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pool.rs` — 新增 `REKEY_BYTE_LIMIT` / `REKEY_TIME_LIMIT`，命令连接应用之（keepalive 仍为 30s）

[[ohos-debug-lessons]]
