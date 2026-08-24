# cmd-agent 使用说明

cmd-agent 是一个通用的远程命令执行桥。它把"在 HarmonyOS NEXT (OHOS) 设备上执行外部程序"这件事，搬到一台 VM（虚拟机，arm64 openEuler）上完成：业务进程（如 zcoder）在本机通过 client 发出指令，server 在 VM 上真正创建子进程执行，输出流式回传。

本说明覆盖：软件功能、运行架构、server 与 client 的运行方式与功能、client 接口与集成方法、代码编译、server 安装方式。

---

## 1. 软件功能

OHOS 应用沙箱禁止**执行外部程序**（`exec`），业务进程（如 zcoder 编辑器）无法直接创建子进程来运行 git、rust-analyzer、bash 等外部程序。cmd-agent 通过"本地 daemon + 远端 VM server"的架构绕过这一限制，提供：

- **执行任意二进制**：在 VM 上 spawn 指定的可执行程序，argv 数组传参（无需 shell 字符串拼接，参数特殊字符安全）。
- **短命令（一次性）**：执行后收集全部输出与退出码，适合 git status、git diff 等。
- **长驻服务（双向流）**：以裸字节流方式双向透传 stdin/stdout，适合 rust-analyzer、LSP server 等常驻进程。
- **路径映射**：业务侧（OHOS）路径根自动映射为 VM 路径根，业务代码无需感知两端路径差异。
- **多指令并行**：每条指令一条独立 TCP 连接，互不阻塞，天然支持并发（如 LSP 运行的同时执行多个 git 命令）。
- **数据零转发**：子进程直接持有连接 socket（`dup2`）作为其 stdin/stdout，数据在"子进程 ↔ 业务"之间直达，不经过 server 进程，无汇聚瓶颈。
- **进程组管理**：超时、断连、显式信号时按进程组清理整个子进程树，无僵尸/孤儿进程残留。
- **server 生命周期自动跟随**：client 退出（或心跳超时）时 server 自动退出，VM 上不留孤儿进程。
- **server 自动恢复**：client 检测到 VM server 不可达时，自动重新部署（按名杀旧进程 → 重传二进制 → 拉起 → 探活）。

## 2. 软件运行架构

三层进程、四条连接：

```
┌─ OHOS 设备 ──────────────────────────────┐        ┌─ VM (arm64 openEuler) ───────────────┐
│  业务进程 (zcoder)                        │        │  cmd-agent-server                    │
│    │ 通过 unix socket 与 client 通信      │        │   │ accept / 握手 / spawn / waitpid  │
│    ▼                                     │        │   │  (不碰数据)                        │
│  client (cmd-agent-client daemon 进程)    │        │   ▼  dup2(socket_fd, 0/1/2)          │
│    ├─ 管理连接（常驻）──心跳/退出码/信号──┼──TCP──►│  子进程 (git / rust-analyzer / bash)  │
│    └─ 数据连接（每条指令一条）─纯字节透传──┼──TCP──►└─────────────────────────────────────┘
└──────────────────────────────────────────┘
```

**进程角色**

- **业务进程（zcoder）**：OHOS 应用。spawn client daemon，通过 unix socket 与之通信。数据直达业务，不做中转。
- **client（daemon）**：OHOS 本地独立进程。业务与 VM server 之间的**协议代理 + 运维封装**：数据连接纯字节透传、管理连接中继（退出码/信号）、心跳维持 VM server、断线重连与自动重部署、父进程存活检测。
- **server**：VM 上的独立二进制。真正创建子进程，子进程直接持有连接 socket 作为 stdio，server 只做 spawn/wait/kill 与退出码上报。

**连接模型**

- **管理连接**：1 条，常驻。承载心跳（10s 间隔）、`SpawnOk`（spawn 确认）、`ExecResult`（退出码）、`Signal`（信号）。同时是 client 存活标记：心跳超时/断开时 server 退出。
- **数据连接**：每条指令 1 条。握手（`Hello`/`Spawn`）之后变成纯字节流，子进程 stdio 与业务直接互连。退出码等控制信息不走数据连接（子进程输出是任意字节，无法在其中划帧边界）。

**数据路径（零转发）**

```
业务 ──unix socket──► client ──TCP──► server ──dup2──► 子进程 stdin
子进程 stdout ──dup2──► server 的 socket ──TCP──► client ──unix socket──► 业务
```

## 3. Server 运行方式及功能

server 是部署在 VM 上的独立可执行程序 `cmd-agent-server`。

### 运行方式

```
cmd-agent-server [--listen ADDR]
```

- `--listen ADDR`：监听地址，默认 `0.0.0.0:4040`。

### 功能

- **监听数据连接**：每连接一个会话。握手（`Hello` 校验协议版本、接收路径映射）→ 收到 `Spawn` → 用 `std::process::Command` 创建子进程，`pre_exec` 中把连接 socket `dup2` 到子进程 0/1/2 并清除 `O_NONBLOCK`（子进程获得阻塞 stdio）→ 经管理连接发 `SpawnOk` → 丢弃自身 socket 副本（client 读到 EOF 的时点 = 子进程退出）→ `try_wait` 轮询等待子进程退出 → 经管理连接发 `ExecResult`。
- **stderr 处置**：debug 构建重定向到 VM 本地日志 `logs/session-<id>.stderr.log`；release 构建重定向到 `/dev/null`。
- **管理连接**：读心跳维持 client 存活；收到 `Signal` 时查 session 注册表，对目标进程组发信号（SIGINT/SIGTERM/SIGKILL）；把各会话的 `SpawnOk`/`ExecResult` 转发给 client。
- **进程组管理**：子进程是新进程组组长（`process_group(0)`），超时、断连、信号时 `kill(-pid)` 清理整个进程树。
- **生命周期**：管理连接断开或心跳超时（30s）→ server 退出并清理全部子进程。
- **超时**：`ExecSpec.timeout_ms` 指定时，超时 kill 整个进程组。

### 日志

server 使用 `env_logger`，默认 info 级。可通过 `RUST_LOG=debug` 开详细日志。

## 4. Client 运行方式及功能

client 是运行在 OHOS 设备上的独立进程 `cmd-agent-client`（daemon），由业务进程 spawn 并作为其子进程。

### 运行方式

```
cmd-agent-client \
    --unix-socket /data/<app>/cmd-agent.sock \
    --vm-addr <vm-ip>:4040 \
    [--ssh-host <vm-ip> --ssh-user <user> --ssh-pass <pass> --remote-dir ~/cmd-agent] \
    [--server-binary <本地 server 二进制路径>] \
    [--agent-port 4040]
```

- `--unix-socket PATH`：本地 unix socket 路径，默认 `/tmp/cmd-agent.sock`。
- `--vm-addr HOST:PORT`：VM server 地址。
- `--ssh-*`：SSH 部署凭据（自动恢复/安装 server 用，可选）。
- `--server-binary PATH`：本机可访问的 server 二进制路径（自动部署用，可选）。
- `--agent-port PORT`：server 监听端口（部署用，默认 4040）。

### 功能

- **监听 unix socket**：接收业务连接。管理连接与数据连接按首帧（`Manage`/`Hello`）区分。
- **数据连接透传**：连上 VM server 对应数据连接，握手后把 unix socket 与 VM TCP 之间做**双向字节拷贝**（不解析、不缓存）。任一侧 EOF 即关闭另一侧写方向。
- **管理连接中继**：VM 的 `SpawnOk`/`ExecResult`/`Error` 转发给业务；业务的 `Signal` 下发给 VM。`SpawnOk` 按 session 路由，避免与子进程输出竞态。
- **心跳维持**：每 10s 向 VM server 管理连接发心跳。
- **自动恢复**：检测 VM server 不可达 → 先重连，失败且配了 SSH 时自动部署：`pkill -x cmd-agent-server` 按名杀旧进程 → 分块上传二进制 → `setsid nohup` 拉起 → TCP 探活。
- **父进程存活检测**：启动时记录父进程 PID 并设置 `PR_SET_PDEATHSIG`，周期检查 `getppid()`；父进程（业务）死亡或管理连接断开 → client 自动退出。client 退出 → VM server 管理连接断 → server 也退出。

### 日志

client 使用 `env_logger`，默认 info 级。日志写自身 stderr（由业务进程重定向）。

## 5. Client 接口说明和集成方法

业务与 client 之间使用**同一套帧协议**：长度前缀（4 字节小端长度）+ JSON 消息。每帧一个消息，`type` 字段区分。

### 消息类型

**Client → daemon（业务发出）**

- `Hello { version, root_map }`：握手。`root_map = { ohos_root, vm_root }` 为路径映射（可选）。
- `Manage`：将当前连接标记为管理连接。
- `Heartbeat`：管理连接心跳。
- `Spawn { session_id, spec }`：发起一次执行。`session_id` 由业务分配（全局唯一），`spec` 见下。
- `Signal { session_id, signal }`：向指定会话的进程组发信号（`SigInterrupt`/`SigTerm`/`SigKill`），走管理连接。
- `Query` / `Shutdown`：查询 / 请求关闭。

**Daemon → 业务**

- `HelloOk { server_version }`：握手成功。
- `SpawnOk { session_id }`：spawn 成功，数据连接转为字节流。
- `ExecResult { session_id, exit_code, timed_out }`：执行结束（走管理连接）。`exit_code` 为 `None` 表示被信号终止。
- `Error { session_id, message }`：错误。

**ExecSpec（一条执行指令的完整描述）**

- `source_program`：指令来源标识（如 `"git"`、`"rust-analyzer"`），仅日志用。
- `binary`：要执行的程序（路径或 PATH 名）。
- `args`：完整 argv（不含 binary 本身）。
- `path_arg_indices`：`args` 中属于路径的下标，server 会做 OHOS 根 → VM 根映射。
- `cwd_path`：工作目录（同样做路径映射）。
- `env`：环境变量。
- `stdin`：可选，spawn 确认后由业务写入连接（注入给子进程 stdin）。
- `timeout_ms`：可选超时，超时 kill 整个进程组。

### 集成流程（业务侧）

1. **启动 daemon**：业务进程用 OHOS 的进程创建接口 spawn `cmd-agent-client`，并传入 unix socket 路径与 VM 地址。
2. **建立管理连接**：连接 unix socket，发 `Hello` + `Manage`。此后 daemon 会持续把 `SpawnOk`/`ExecResult`/`Error` 推过来，业务按 `session_id` 匹配。
3. **执行一条指令**：
   - 新建一条连接，发 `Hello`，再发 `Spawn { session_id, spec }`。
   - 等待该连接的 `SpawnOk`（经管理连接路由返回）或 `Error`。
   - 收到 `SpawnOk` 后：若有 `spec.stdin` 先写入连接；此后连接即为子进程 stdio 的裸字节流——业务直接读写。
   - 子进程退出 → 数据连接 EOF；退出码经管理连接 `ExecResult` 异步到达。
4. **结束会话**：需要主动终止时，经管理连接发 `Signal { session_id, SigKill }`。业务正常结束 = 关闭管理连接（daemon 随之退出，server 也随之退出）。

### 代码形态

- **daemon 可执行**：`cmd-agent-client` 二进制，业务 spawn 它。
- **client crate**：`cmd-agent-client` 库提供了 `deploy`（SSH 部署）、`daemon`（代理主逻辑）等模块；业务也可直接链接该 crate 复用协议与部署代码。
- **protocol crate**：`cmd-agent-protocol` 提供消息结构体与帧编解码（`frame::write_message`/`read_message`），两端共享。

## 6. 代码编译

三个 crate：`cmd-agent-protocol`、`cmd-agent-server`、`cmd-agent-client`，同属一个 Cargo workspace。

### Client（daemon）：与业务代码一起编译

client 运行在 OHOS 设备上，与业务进程（zcoder）使用同一套 OHOS 工具链编译：

```
# 在 zcoder 主工程（或 OHOS SDK 环境）中
cargo build -p cmd-agent-client --target aarch64-unknown-linux-ohos --release
```

产物 `cmd-agent-client` 可执行文件随应用打包，业务启动时 spawn。

> 说明：client 依赖 `russh`（SSH 部署）、`smol`（异步运行时）、`libc`。若目标工具链对某些依赖编译有差异，按实际工具链适配。

### Server：用 host 工具链编译，随 `bundle-ohos` 打进 resfile

server 运行在 VM（arm64 openEuler）上，用 host 工具链（本机 aarch64-linux）编译成独立可执行文件，不依赖 OHOS 环境。`zcoder/script/bundle-ohos` 已集成：

```
# 在 zcoder 主工程中执行 bundle-ohos：
#   OHOS 段编译后，用 host 工具链编译 server，产物拷贝到 hap/entry/src/main/resfile/cmd-agent-server
# 单独编译亦可：
cargo build -p cmd-agent-server --release
```

产物 `cmd-agent-server` 是独立的可执行文件，运行时经 SSH 部署到 VM，无需额外运行库。

> 提示：server 只依赖 `std`、`libc`、`smol`、`serde`，无 OHOS 特有依赖，可在普通 aarch64 Linux 环境交叉/本地编译。

## 7. Server 安装方式

server 二进制作为**资源文件**放进 HAP 的 resfile 目录（`hap/entry/src/main/resfile/cmd-agent-server`）。resfile 随安装**解压**到应用沙箱，有真实只读路径；业务侧经 native API 拿到路径，daemon 直接按路径读取部署——无 resourceManager、无 ArkTS。

**为什么 resfile 可行**

rawfile 打包后**不解压**，运行时无物理路径，只能经 resourceManager 接口读取，独立 daemon 进程拿不到。resfile 与之相反：随安装解压到沙箱（`context.resourceDir`），有真实只读路径，应用内任何同 UID 进程（含 daemon）都能按路径直接 `std::fs::read`，无需额外授权。

**路径获取（native，不经 ArkTS）**

1. `zcoder/script/bundle-ohos` 用 host 工具链（本机 aarch64-linux，环境变量显式声明）编译 server，拷贝到 `hap/entry/src/main/resfile/cmd-agent-server`（拷贝目标路径为脚本环境变量）。
2. 业务侧（Rust）调 `OH_AbilityRuntime_ApplicationContextGetResourceDir(module_name, ...)`（libability_runtime.so，API 20+，zcoder 目标 API 23 满足）拿 resfile 只读路径——封装为 `openharmony_ability::application_resource_dir(module_name)`。
3. 拼出 `<resourceDir>/cmd-agent-server`，经 daemon 的 `--server-binary` 参数传入（不硬编码）。

**安装步骤（程序启动后自动执行，lazy）**

1. daemon 用 `std::fs::read(server_binary_path)` 读字节，交给部署能力（SSH）：
   - 连接 VM（`--ssh-host/--ssh-user/--ssh-pass`）；
   - 创建远端目录（`--remote-dir`，默认 `~/cmd-agent`）；
   - `pkill -x cmd-agent-server` 杀掉残留旧进程（不存在则忽略）；
   - 分块上传二进制（SCP 语义，每块 4MB）；
   - `chmod +x` 使远端文件可执行，并用 `setsid nohup cmd-agent-server --listen 0.0.0.0:<port> &` 拉起；
   - TCP 探活确认 server 可连。
2. client 建立管理连接，进入正常服务。

**触发时机**

- 业务（zcoder）启动时 spawn daemon（创建子进程 ability）。
- daemon 启动即尝试连 VM server 建立管理连接；连不上 → 自动部署（通常第一次需要执行指令时触发，因此时 server 尚未安装）。
- 运行中：VM server 崩溃/断线，client 重连失败后自动重新安装（复用同一流程）。

**依赖参数**

- `--ssh-host` / `--ssh-port`（默认 22）/ `--ssh-user` / `--ssh-pass`：VM 的 SSH 凭据。
- `--remote-dir`：VM 上放置 server 的目录。
- `--server-binary`：业务侧把 resfile 只读路径传到这里。
- `--agent-port`：server 监听端口（默认 4040），与 `--vm-addr` 端口一致。
