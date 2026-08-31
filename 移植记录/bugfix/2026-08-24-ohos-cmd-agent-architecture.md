# OHOS cmd-agent 远程命令执行整体方案

## 问题描述 (Problem Description)

zcoder（Zed 移植到 HarmonyOS NEXT，代码库即 Zed）在 OHOS 设备上无法像 Linux/macOS 那样直接 `spawn` 子进程执行命令（git、LSP、shell 等）：应用沙箱禁止创建子进程、管控 `/proc`、限制路径权限。为让 git 面板、LSP、终端等所有依赖子进程的功能正常工作，需要把命令执行转发到虚拟机（OpenEuler）上的独立进程执行，再把子进程的 stdio 实时回传。

本方案即 `ohos-openeuler-agent` 三 crate（`cmd-agent-client` / `cmd-agent-server` / `cmd-agent-protocol` / `cmd-agent-linker`）的整体功能设计。本文着重代码框架：线程分配与职责、client 与 server 的分工、要点与注意事项。

## 整体架构总览

三段式：**业务代码（zcoder 进程内）→ cmd-agent daemon（zcoder 进程内线程）→ cmd-agent-server（VM 独立进程）**。设备与 VM 通过目录挂载共享同一磁盘（设备路径 `/storage/Users/currentUser/` ↔ VM 路径 `/mnt/linux_share`），所以路径参数可以双向映射。

```
┌────────────── OHOS 设备：zcoder 进程（单进程） ──────────────┐
│                                                              │
│  业务代码 (git_ui / language / terminal / shell_env)         │
│    │  通过 cmd_agent_linker::RemoteCommandExecutor trait     │
│    │  (spawn / signal / try_exit / wait_exit_async)          │
│    ▼                                                         │
│  Client  (零线程，纯函数式)                                   │
│    │  spawn_req channel / signal channel / SharedControl 表   │
│    │  ←────── 进程内共享（Mutex + channel）──────────────→   │
│  cmd-agent daemon（3 线程，本方案核心）                       │
│    │ cmd-agent-bus  ×1  业务：accept / 握手 / VM 管理         │
│    │ cmd-agent-rel0 ×1  转发：VM↔业务 socket 字节复制          │
│    │ cmd-agent-rel1 ×1  转发：同 rel0，负载均衡               │
│    │ (+ async-io ×1，crate 固有全局 reactor，不可避免)        │
└──────┬──────────────────────────────────────────────────────┘
       │ TCP (127.0.0.1:<port>) —— 实际是设备↔VM 网络连接
       ▼
┌────────────── VM：cmd-agent-server 独立进程 ──────────────┐
│  每连接一个 smol::spawn 异步任务                            │
│  数据面：握手后 socket dup2 给子进程作 stdio（纯字节）       │
│  控制面：management 连接收 heartbeat、发 exit code           │
│  spawn_direct → wait_child_exit_shared (pidfd 事件驱动)     │
└─────────────────────────────────────────────────────────────┘
```

**为什么 daemon 跑在 zcoder 进程内而不是独立进程**：早期版本 daemon 是独立进程（unix socket 通信），后改为 zcoder 进程内线程。好处是 spawn 确认和 exit 结果通过进程内共享表直达 client，省掉两条进程间 socket 与两个 reader/writer 线程；坏处是 daemon 必须与其他 zcoder 线程共存，因此线程划分要格外小心（见下）。

## 线程分配与职责

### 总线程数

```
cmd-agent-bus   ×1   业务平面：accept loop、spawn 握手、VM management、连接池维护
cmd-agent-rel0  ×1   数据平面：VM ↔ 业务 socket 字节复制（relay_duplex）
cmd-agent-rel1  ×1   数据平面：同 rel0，分担转发负载
─────────────────────────────────────────────────────────────
async-io        ×1   smol/async-io crate 进程全局 reactor（首次用 I/O 即创建，
                     驱动所有 socket/Timer 事件；cmd-agent 不额外创建）
```

### 两个 smol::Executor（daemon.rs）

daemon 创建两个独立 `smol::Executor`，由不同线程驱动，把"业务"与"转发"彻底隔离：

- **business executor**（由 `cmd-agent-bus` 驱动）：accept loop、spawn_request_loop、vm_manager、连接池 maintain、handle_conn 握手。**控制消息永不被大数据转发拖累**。
- **relay executor**（由 `cmd-agent-rel0/rel1` 驱动）：proxy_data / proxy_stderr 提交的 `relay_duplex`（双向字节复制）、连接池 `rebuild_one` 预建。**重载只落在转发线程**。

同一 executor 由多线程驱动时靠 async-executor work-stealing 均衡：谁空闲谁取任务，一个忙的 relay 不会饿死另一个（日志验证：rel0/rel1 轮转承担 to_vm/to_uni）。

### 业务平面（cmd-agent-bus）承载的任务

```
business executor（cmd-agent-bus 驱动）
├─ accept loop            接受业务连接 → handle_conn（accept 错误 100ms 重试）
├─ spawn_request_loop     消费 shared.spawn_req_rx，smol::spawn 握手任务（catch_unwind）
├─ vm_manager             VM management 连接：heartbeat / control / events / signal 四任务
├─ pool maintain          连接池预建 + 心跳（等 vm_ready 后启动）
└─ handle_conn            握手：Hello → HelloOk → 路由 Spawn/SpawnStderr/Manage
```

### 数据平面（cmd-agent-rel0/rel1）承载的任务

```
relay executor（rel0/rel1 驱动，work stealing）
├─ relay_duplex         双向复制：to_vm（业务→VM 子进程 stdin）、to_uni（VM stdout→业务）
└─ pool.rebuild_one     预建连接池补充（connect + Hello 握手）
```

### Client：零线程

Client 不建管理连接、不启动任何 reader/writer 线程。spawn 确认与 exit 结果都从进程内 `SharedControl` 表读取，signal 走共享 channel。见下文分工。

### VM server：每连接一个异步任务（单线程事件循环 + work stealing）

server 跑在 VM 上，`smol::block_on` 单事件循环，每个连接 `smol::spawn` 一个异步任务，由 smol 内部 work-stealing 分配。**数据面不经过 server 进程**（socket 直接 dup2 给子进程），所以 server 的 CPU 压力只来自控制面（握手、wait、signal），天然轻量，无需多线程。

## 通信链路与协议

### 三种连接（Client ↔ Daemon，unix socket；Daemon ↔ Server，TCP）

```
业务侧                                    daemon 内部                VM 侧
──────────────────────────────────────────────────────────────────────────
[client] 无管理连接 ── SharedControl ──→ [daemon vm_manager] ── TCP ─→ [server]
                                          (管理连接：heartbeat/exit/signal)
[client] spawn 时开 unix socket ───────→ [daemon proxy_data] ── TCP ─→ [server]
                                          (数据连接：握手后纯字节)
[client] spawn 时开 unix socket ───────→ [daemon proxy_stderr] ──TCP──→ [server]
                                          (stderr 连接：子进程 fd2)
```

### 帧协议（cmd-agent-protocol/frame.rs）

```
[4-byte little-endian length][JSON payload]
帧上限 MAX_FRAME_SIZE = 256MB（防无界读）
消息带 type 判别字段（serde tag）
```

### 消息类型

- **ClientMessage**（client→server）：`Hello{version, root_map}` / `Manage` / `Heartbeat` / `Spawn{session_id, spec}` / `SpawnStderr{session_id}` / `Signal{session_id, signal}` / `Query` / `Shutdown`
- **ServerMessage**（server→client）：`HelloOk{server_version}` / `SpawnOk{session_id}` / `ExecResult{session_id, exit_code, timed_out}` / `Error{session_id, message}`
- **ExecSpec**（一次命令的全部描述）：`source_program` / `binary` / `args` / `path_arg_indices`（标记哪些参数是路径，需映射）/ `cwd_path` / `env` / `stdin`（可选预写负载）/ `timeout_ms` / `stdin_mode/stdout_mode/stderr_mode`（`FdMode::Piped|Null`）
- **协议版本** `PROTOCOL_VERSION = 1`：Hello 时协商，mismatch 直接回 Error 关闭

### 管理连接 vs 数据连接

- **管理连接**：低频率控制面。carries heartbeat（保活）、exit code、signal。**是 server 的生命周期标记**（见容错节）。
- **数据连接**：高频率字节面。握手（Hello/Spawn/SpawnOk）后**立即变成纯字节流**，client 的 stdin/stdout 直连子进程 stdio，**不再有任何帧**。SpawnOk/ExecResult 因此必须走管理连接，否则帧会泄漏进子进程 stdout 干扰输出解析。

## Client / Daemon / Server 三方分工

### Client（cmd-agent-client/src/client.rs，zcoder 进程内，零线程）

职责：把业务代码的一次 spawn/signal/wait 翻译成对 daemon 的请求，等待结果。

```
业务代码                      Client 内部
spawn(spec)
  │  shared.spawn_req_tx.try_send(SpawnRequest{client:Arc, spec, reply})
  │  阻塞 rx.recv_timeout(20s)          ← 握手实际在 daemon executor 跑
  ▼  reply 返回 Session{stdin, stdout, stderr}
spawn 得到可用 Session
```

- `spawn`（同步）：把请求**入共享 channel**，调用线程只等一个 `std::sync::mpsc` 回复。**绝不在调用线程跑 `smol::block_on`**——否则在 GPUI 主线程（如 LSP 前台任务）会嵌套 async-io reactor 死锁。
- `spawn_async`（异步，由 daemon executor 调用）：真正握手——先开 stderr 连接（`SpawnStderr` 必须先于 `Spawn` 到达），再开主连接 `Hello`→`HelloOk`→`Spawn`→`wait_spawn_ok`（读 shared 表）。
- `try_exit`：读 `shared.exec_results` 表；`wait_exit_async`：在 `shared.exec_waiters` 注册事件驱动等待（daemon 一收到 ExecResult 即通知），避免轮询。
- `signal`：请求入 `shared.signal_tx` channel，阻塞等回复（同样是 channel，不嵌套 reactor）。
- 实现 `cmd_agent_linker::RemoteCommandExecutor` trait，注册为进程唯一执行器，`util` 等业务 crate 只依赖 linker 抽象，不直接依赖 client。

### Daemon（cmd-agent-client/src/daemon.rs，zcoder 进程内 3 线程）

职责：业务 client 的进程内代理，桥接 VM server；维持连接池与 VM 存活。

```
SharedControl（进程内共享表，client 与 daemon 共享同一 Arc）
├─ spawn_oks     spawn 确认表（daemon 从 VM 事件填充，client wait_spawn_ok 读）
├─ exec_results  exit 结果表（daemon 填充，client try_exit/wait_exit 读）
├─ exec_waiters  exit 等待者（client 注册，daemon 收到 ExecResult 即通知）
├─ signal_tx/rx  signal 请求 channel（client 写，daemon VM 写者消费）
└─ spawn_req_tx/rx spawn 握手请求 channel（client 写，daemon 握手循环消费）

Ctx（连接处理器共享上下文）
├─ pool         VmConnectionPool（预建连接池，POOL_SIZE=16）
├─ control_tx   VM 控制消息发送 channel（signal 转发）
├─ events_rx    VM 事件接收（SpawnOk/ExecResult）
├─ spawn_oks    daemon 内部 spawn 确认表（proxy_data 用）
├─ vm_ready      AtomicBool + tokio::sync::Notify（VM 就绪事件）
├─ active_management  当前业务管理连接（单 reader 防事件分叉）
├─ shared        与 client 共享的 SharedControl
├─ executor      business executor
└─ relay_executor relay executor
```

- `spawn_daemon`：绑 unix socket → 建双 executor → 提交 accept loop / vm_manager / pool maintain / spawn_request_loop → 起 1 业务线程 + 2 转发线程。
- `handle_conn`：读首帧，`Hello`→版本校验→`HelloOk`→设连接池 root_map→读次帧路由 `Spawn`（→proxy_data）/ `SpawnStderr`（→proxy_stderr）/ `Manage`（→manage_connection）。
- `proxy_data`：等 vm_ready（Notify 事件驱动）→ `pool.take()`（取预建连接）→ 写 `Spawn` → `wait_spawn_ok`（读 daemon 内部表）→ **把 relay_duplex 提交到 relay executor**（catch_unwind 保护）→ 立即返回。
- `relay_duplex`：在 relay executor 上双向复制字节。**子进程 EOF（to_uni 完成）即 tear down 两条连接、cancel to_vm**，VM 连接立刻释放（防 CLOSE-WAIT）。
- `vm_manager`：连 VM → `Manage` → 置 vm_ready=true + `notify_waiters()` → `pool.invalidate()`（旧连接作废）→ 并发跑 heartbeat / control 转发 / events 读 / signal 写（共享 write_lock 串行化）→ 任一失败则 recover（探活→SSH 重部署）→ 1s 后重连。
- `manage_connection`：新管理连接替换旧的（关闭旧连接），保证 `events_rx` 单 reader，SpawnOk/ExecResult 不被分叉到两条陈旧连接。
- 连接池 `VmConnectionPool`：预建 POOL_SIZE 条已握手连接；`generation` 计数器——root_map 变化或 VM 重连时 invalidate，握手期间状态变更则丢弃该连接，**stale 连接绝不交给 spawn**；`take` 消耗后 `rebuild_one` 立即补一条。

### Server（cmd-agent-server/src/main.rs + spawn.rs，VM 独立进程）

职责：在 VM 上真正 fork/exec 子进程，把 socket 交成子进程 stdio，汇报退出码。

```
accept loop（smol::block_on + accept_or_shutdown）
  └─ handle_connection：Hello→HelloOk，路由 Manage/Spawn/SpawnStderr
       ├─ run_management：heartbeat_loop（读心跳+派发 signal）+ forward_results（克隆 socket 发 exit）
       ├─ run_spawn：取 stderr 连接(2s 内) → spawn_direct → 发 SpawnOk → drop(socket) → wait → 发 ExecResult
       └─ run_stderr_attach：pending_stderr 注册 + 30s 孤儿回收
```

- `spawn_direct`（spawn.rs）：**把连接 socket `dup2` 成子进程的 0/1/2**，子进程直接与 client 通信，数据不经过 server 进程（零中间拷贝）。`process_group(0)` 建新进程组，便于 `kill(-pid)` 整组消灭。`pre_exec` 清 `O_NONBLOCK`（子进程期望阻塞 stdio）。binary 解析三级回退：设备路径映射后存在 → VM 绝对路径存在 → 按 PATH 名字查找。
- `wait_child_exit_shared`：**pidfd 事件驱动**（`pidfd_open` + `smol::Async::readable`），不阻塞 executor；超时则 kill 整进程组并显式 reap，防 zombie/孤儿孙进程。无 pidfd 内核时轮询兜底。
- 失败快速上报：spawn 失败走 `results_tx.send(SpawnError)` 经管理连接立即通知 client，**避免 client 干等 15s 超时**。

## 一次命令执行的完整生命周期

以 `git status` 为例（timeout 场景简化）：

```
业务代码                          daemon (bus线程)          daemon (rel线程)        VM server
   │   ExecSpec{git,status...}         │                         │                      │
   │──spawn→ spawn_req channel ────────→│                         │                      │
   │                                    │ spawn_request_loop 起握手任务                 │
   │                                    │ client.spawn_async:                             │
   │                                    │  开 stderr 连接(SpawnStderr) ────────────────→ │
   │                                    │  开主连接(Hello→HelloOk→Spawn) ──────────────→ │
   │                                    │                    （走连接池预建连接）         │
   │                                    │ wait_spawn_ok(daemon表)                         │
   │                                    │   ←SpawnOk 从管理连接回到 events 任务           │
   │  ←reply: Session{stdin,stdout}      │                         │                      │
   │──读 stdout───────────→ [proxy_data 把 relay_duplex 提交 relay executor]              │
   │                                  （rel0/rel1 复制字节：stdout→client，stdin→child）   │
   │                                    │                         │     子进程执行完退出   │
   │                                    │   ←ExecResult 从管理连接回到 events 任务        │
   │  ←exit_code 写入 shared.exec_results，通知 exec_waiters                              │
   │  业务 wait_exit_async 拿到退出码    │                         │                      │
```

## 路径映射机制

两条固定映射规则 + 一次 RootMap 协商：

- 规则 A：`/data/storage/el2/base/haps/entry/files/...`（设备应用沙箱）→ `$HOME/...`（VM 登录目录）
- 规则 B：`/storage/Users/currentUser/...`（设备 IDE 工作区）→ `/mnt/linux_share/...`（VM 挂载盘）

应用在：每个 `args` 里 `path_arg_indices` 标出的参数、`cwd_path`、binary。前缀匹配是**component-bounded**：`<root>X/...` 这种只共享前缀的路径不误匹配。RootMap 由 client 在 Hello 时携带，连接池必须以相同 root_map 握手，否则路径映射错乱。

## 下载文件落盘目录（OHOS 适配）

zcoder 在 OHOS 设备上无法像桌面端那样直接用 `http_client` 拉取 GitHub 二进制（设备沙箱 GitHub 访问不可靠，常落地失败 → ENOENT）。因此 `crates/http_client/src/github_download.rs` 与 `github.rs` 在 `#[cfg(target_env = "ohos")]` 分支改用 `util::command` 把下载转发到 VM 执行（curl / sha256sum / tar / gunzip / mv），文件实际落盘在 VM 侧。

**无论下载走 http 还是 `util::command`，落盘目录都相同**——目录由调用方传入的 `destination_path` 决定，与下载实现方式无关。所有目录均以 `paths::data_dir()` 为根（`paths.rs`：OHOS 上经路径映射落到 VM 的 `$HOME/zed`，即 `~zed/...`）。

| 下载内容 | 下载方式（代码内） | 落盘目录（基于 `data_dir()`） | 代码依据 |
|---|---|---|---|
| 内置语言服务器二进制（rust-analyzer、c/c++、pyright 等） | http（非 OHOS）/ `util::command`+curl（OHOS） | `data_dir()/languages/<server>` | `languages_dir()` `paths.rs:445`；`github_download.rs` |
| Debug Adapters (DAP) | 同上 | `data_dir()/debug_adapters/<name>` | `debug_adapters_dir()` `paths.rs:453` |
| External agent servers | 同上 | `data_dir()/external_agents/<name>` | `external_agents_dir()` `paths.rs:461`；`agent_server_store.rs` |
| Node.js 运行时二进制 | **http（`http.get`），未改成 command** | `data_dir()/node/<folder>/`（node=`node_dir/bin/node`，npm=`node_dir/node_modules/npm/bin/npm-cli.js`） | `node_runtime.rs:636-638` |
| npm 包（JS 依赖，如 eslint / prettier 的 node 版） | 运行 `npm install`（经 `util::command`，OHOS 走 VM） | `<server_dir>/node_modules`（即 `data_dir()/languages/<server>/node_modules` 等） | `node_runtime.rs` `install_npm_packages`；`local_package_directory.join("node_modules")` `:847/:1040` |
| Copilot / Prettier 等插件 | http | `data_dir()/copilot`、`data_dir()/prettier` | `paths.rs:467/473` |

两点说明：

- http 与 `util::command` 两路下载落盘到**同一目录**；区别仅在 OHOS 上 curl 经 cmd-agent 在 VM 执行，路径映射后落在 VM 的 `$HOME/zed/languages/<server>` 等。
- "npm 下载"并非 zcoder 直接拉取文件，而是调用 `npm`（经 `util::command` 运行，OHOS 转发到 VM）去拉包，落盘在对应 server 安装目录的 `node_modules` 子目录，无统一固定目录。

## 关键设计要点与注意事项

### Client 侧要点

- **零线程 + 绝不在调用线程 block_on**：spawn/signal 都走共享 channel + `std::sync::mpsc` 回复，调用线程从不驱动 async-io reactor。在 GPUI 主线程（LSP 前台任务、渲染回调）嵌套 `smol::block_on` 会嵌套 reactor 死锁。
- **必须消费 HelloOk**：数据连接读 HelloOk 不及时清走，dup2 后该帧会变成子进程 stdout 前缀，破坏 git 输出解析。
- **先开 stderr 连接，再开主连接**：协议保证 `SpawnStderr` 先于 `Spawn` 到达，否则子进程 fd2 无从接线。
- **StdinWriter drop 半关闭 unix socket**：模拟 `ChildStdin` 的 drop 关写端。缺失会让 `git cat-file --batch-check`、`git blame --contents` 等交互命令永远等不到 stdin EOF 而挂起。
- **wait_exit_async 事件驱动**：注册 `exec_waiters` 而非轮询，daemon 一落地 ExecResult 立即唤醒。

### Daemon 侧要点

- **业务/转发双 executor 隔离**：单业务 worker 保证控制消息（accept、握手、VM 管理）不被大转发拖累；2 转发 worker work-stealing 防"一个忙死一个饿死"。这是本方案线程模型的灵魂。
- **relay 必须 catch_unwind**：relay 任务 panic 若逃逸会杀死驱动 `block_on` 的转发 worker；握手任务同样 catch_unwind，panic 时回错误而非让 client 永久 hang。
- **连接池 generation 防 stale**：root_map 变化 / VM 重连都会 bump generation，握手期间状态变更的连接直接丢弃，绝不交给 spawn。
- **vm_ready 事件驱动**（`tokio::sync::Notify` + 100ms fallback 兜底），不在业务线程轮询；VM 管理器 establish 后广播一次。
- **relay 生命周期跟随子进程**：to_uni 完成（子进程退出）即 shutdown 双连接 + cancel to_vm，VM 连接立即释放，防 CLOSE-WAIT 堆积。
- **active_management 单 reader**：新管理连接必须关闭旧连接，否则 events_rx 被两个读者竞争，SpawnOk/ExecResult 分叉到陈旧连接。
- **vm_manager 四并发任务共享 write_lock**：heartbeat / control 转发 / events / signal 写者同一 socket，写必须串行化。

### Server 侧要点

- **数据面纯字节零拷贝**：socket dup2 给子进程，数据不经过 server 进程；代价是**握手完成后 socket 上不能再有残余帧**（SpawnOk 走管理连接）。
- **pidfd 事件驱动 wait**：不用 `try_wait` 轮询（会卡 executor）；`pidfd_open` + readable 事件 + Timer 超时。无 pidfd 内核轮询兜底。
- **process_group 整组管理**：子进程 `process_group(0)`，signal/超时/断连都 `kill(-pid)`，消灭整棵树防孤儿。
- **stderr 连接时序**：spawn 到达时 stderr 可能未注册（并发转发），等 2s；超时降级 `/dev/null`（但 daemon 的 stderr relay 会因此等不到 EOF）。孤儿 stderr 30s 回收。
- **服务器生命周期 = 管理连接**：heartbeat 超时 30s 或管理连接断开 → shutdown。否则 VM 上堆僵尸 agent。
- **失败快速上报**：spawn 失败走 `SpawnError` 管理连接立即通知，避免 client 等 15s 确认超时。
- **O_NONBLOCK 清除**：子进程期望阻塞 stdio，dup2 后要清非阻塞标志（dup2 共享 file description，server 侧也变阻塞，但 server 立即 drop socket 拷贝）。

### 协议层面

- 帧长度前缀 + JSON，`PROTOCOL_VERSION` 握手协商，mismatch 即 Error 关闭，杜绝跨版本错乱。
- `MAX_FRAME_SIZE` 上限防无界读。
- ExecSpec 自带 `path_arg_indices` 显式标记路径参数，映射规则才有的放矢。

## 容错与生命周期管理

- **VM 失联自动恢复**：`recover_vm` 先探活，失败则 SSH 重新部署（`deploy.rs`：`cat >` 分块上传 4MB/块、`setsid nohup` 后台启动、TCP probe 30×500ms 探活）。russh 用 ring 后端（纯 Rust 编译快）。部署是低频阻塞操作，跑在独立 tokio runtime，不占用 smol 协议路径。
- **worker 崩溃防护**：三个 daemon worker 全部 `catch_unwind`，panic 只记日志不杀 zcoder 进程。
- **server 关停不泄漏**：管理连接断 → shutdown；孤儿 stderr 30s 回收；超时子进程整组 kill + reap。
- **跨重启路径**：unix socket 残留文件先删再 bind。

## 已知边界与限制

- 数据面零拷贝的前提是握手后 socket 干净；任何在数据连接上误写帧（如重复 SpawnOk）都会破坏子进程输出。
- SSH 部署 `AcceptAllHandler` 接受任意主机密钥，仅适合内部桥接 VM，生产需钉 host key。
- 连接池固定 POOL_SIZE=16，超载时回退即时握手（`take` 空池兜底），但峰值并发超过池上限时每个 spawn 要多付一次 TCP+Hello。
- client 的 spawn/signal 是同步阻塞 API（依赖 daemon executor 及时消费），daemon executor 若被业务任务长期占用会放大调用时延——这正是拆分双 executor 的原因。

## 修改文件 (Modified Files)

本方案对应代码位于 `crates/gpui_ohos/depend/ohos-openeuler-agent/`：

- `cmd-agent-client/src/daemon.rs` — 核心：双 executor + 3 线程（bus/rel0/rel1）、SharedControl、VmConnectionPool、vm_manager、proxy_data/proxy_stderr/relay_duplex、recover_vm
- `cmd-agent-client/src/client.rs` — 零线程 client：spawn/spawn_async/signal/try_exit/wait_exit_async，实现 RemoteCommandExecutor
- `cmd-agent-client/src/deploy.rs` — SSH 部署 server：SCP 语义分块上传、setsid nohup 启动、TCP probe
- `cmd-agent-client/src/main.rs` — daemon 入口（`spawn_daemon` + park）
- `cmd-agent-server/src/main.rs` — VM 侧 accept loop、run_management/run_spawn/run_stderr_attach、dispatch_signal
- `cmd-agent-server/src/spawn.rs` — spawn_direct（dup2 数据面）、wait_child_exit_shared（pidfd）、map_path
- `cmd-agent-protocol/src/messages.rs` — 帧内消息类型（ClientMessage/ServerMessage/ExecSpec/RootMap/Signal）
- `cmd-agent-protocol/src/frame.rs` — 长度前缀帧传输
- `cmd-agent-linker/src/lib.rs` — `RemoteCommandExecutor` 抽象 trait，util 与 cmd-agent-client 解耦

参考：[[ohos-debug-lessons]]
