# OHOS 沙箱→VM 文件同步引擎详细设计（FileSync）

## 背景与目标

zcoder 在 OHOS 上所有下载（GitHub 二进制、node 运行时、扩展、AI 插件、DAP 适配器）统一走 http 直连落到设备沙箱 `data_dir()`。`util::command` 经 cmd-agent 在 VM 上执行命令，binary 路径经 Rule A 映射到 VM `/home/user/cmd-agent/zed/...`，但文件只存在于设备沙箱，VM 上缺失。

本设计实现一个**沙箱→VM 单向镜像同步**：zcoder 内独立后台线程监听下载目录，把变更经 cmd-agent 协议新增 FileSync 消息推送到 VM 写盘，保证 VM 侧 spawn 时文件已就绪。

## 同步范围

**源**：设备沙箱 `data_dir()` 下 7 个目录（目标 VM 侧为 `/home/user/cmd-agent/zed/...`）：

- `languages/`（LSP 二进制 + node_modules）
- `extensions/`（扩展）
- `external_agents/`（第三方 AI 插件）
- `copilot/`、`prettier/`
- `node/`（Node.js 运行时）
- `debug_adapters/`（DAP）

**排除**：`cache/`、`logs/`、`db/`、`embeddings/` 等运行时数据（同步了会污染 VM）。

## 协议扩展（cmd-agent-protocol/src/messages.rs）

**控制命令走 JSON 帧，文件内容走裸字节流**（复用 Spawn 数据面"握手后纯字节流"的模式：无 base64 膨胀、无分块协调、顺序天然保证）。

### ClientMessage 新增

```rust
/// Begin a file-sync session on a dedicated connection.
FileSyncStart { sync_id: u64 },

/// Declare the start of `path`'s content stream: the next `len` raw bytes
/// (NOT framed) are appended to `path.ing`.
FileBegin { sync_id: u64, path: String, len: u64 },

/// Atomically rename `path.ing` -> `path`. Also serves as the content-stream
/// terminator: sent right after the last byte of a FileBegin body.
FileRename { sync_id: u64, path: String },

/// Delete `path` (file or dir, recursive) on the VM. Guarded by is_sync_path.
FileDelete { sync_id: u64, path: String },

/// Create a directory (parents included) on the VM.
FileCreateDir { sync_id: u64, path: String },

/// End of the file-sync session; the connection closes after this.
FileSyncEnd { sync_id: u64 },
```

### 流式传输序列（一次批量同步）

```
[Frame: FileSyncStart { sync_id }]
[Frame: FileBegin { path, len }]        <- 声明开始传 path
[raw bytes × len]                        <- 裸字节流，非帧
[Frame: FileRename { path }]             <- .ing -> path（流收尾，原子落地）
[Frame: FileDelete { path }]
[Frame: FileCreateDir { path }]
[Frame: FileSyncEnd { sync_id }]
```

**无逐 op 确认**：TCP 传输可靠，失败靠连接错误传播（写入报错即断连，client 侧 file_sync 返回 Err）。文件内容写完立即 FileRename，整个 session 结束发 FileSyncEnd 后 client 关连接，`file_sync` 成功返回即视为落盘完成。

**读侧保护**：server 读到 FileBegin 后记录 pending 状态，读完 len 字节裸流后，下一帧必须是 FileRename（否则协议错误，报错关连接），避免两端字节数不一致时帧错位。

## Server 侧（cmd-agent-server）

### main.rs

- **启动清理**：`main()` 在 accept loop 前调用 `cleanup_stale_ing_files()`——递归扫描 7 个同步目录的 VM 路径，删除所有 `*.ing` 残留（上次异常退出遗留，避免 spawn 见到残留 `.ing` 白等 30s）。
- **路由**：`handle_connection` 循环新增分支：收到 `FileSyncStart` 后 `return run_file_sync(stream, state, root_map)`。
- **`run_file_sync`**：独立 handler，循环处理 FileSync 系列消息：
  - `FileBegin { path, len }`：`map_path` → `create_dir_all` 父目录 → 打开 `path.ing`（截断）→ `read_exact(len)` 裸字节流写入 → 读下一帧（须为 `FileRename{path}`）→ `rename(path.ing, path)`（原子）
  - `FileDelete { path }`：`map_path` → 校验 `is_sync_path` → `remove_file`/`remove_dir_all`
  - `FileCreateDir { path }`：`map_path` → `create_dir_all`
  - `FileSyncEnd`：关闭连接

### spawn.rs

- 新增常量与函数：

```rust
/// Relative names of the sync-mirrored directories under the VM agent root.
const SYNC_DIR_RELATIVES: &[&str] = &[
    "zed/languages", "zed/extensions", "zed/external_agents",
    "zed/copilot", "zed/prettier", "zed/node", "zed/debug_adapters",
];

/// True when `path` falls under one of the sync directories, so the spawn
/// wait (and FileDelete) only touch device-mirrored content.
fn is_sync_path(path: &str) -> bool
```

- **spawn 等待逻辑**（async，放 `run_spawn` 里、`spawn_direct` 之前，用 `smol::Timer` 轮询，不能阻塞 executor）：

```rust
/// Resolve `mapped_binary`; if it is missing but inside a sync dir, wait for
/// the sync engine to deliver it. `.ing` present -> wait up to 30s for the
/// rename; never saw `.ing` -> wait up to 1000ms; both poll at 50ms.
async fn resolve_binary_wait(mapped_binary: &str) -> Result<String>
```

判定：
1. `binary` 存在 → 直接返回
2. 缺失且 `is_sync_path(binary)` → 50ms 轮询：出现 `binary` 即返回；出现过 `binary.ing` 则等待 `.ing`→`binary` 最多 **30s**；从未出现 `.ing` 则最多 **1000ms**；超时返回 `Error`
3. 缺失且非同步目录 → 返回错误（不等待）

`run_spawn` 等待得到 binary 后传入 `spawn_direct`（保留其原有三级回退逻辑作为兜底）。

## Client / Daemon 侧（cmd-agent-client）

### 传输 op（带设备路径，内容由 daemon 流式读，同步引擎不碰文件内容）

```rust
/// One file-sync operation, addressed by its device-side path; the daemon
/// maps it to the VM side and (for WriteContent) streams the file body.
pub enum FileSyncOp {
    /// Stream `device_path`'s content to the VM as `.ing` then rename it.
    WriteContent { device_path: String },
    /// Mirror a device-side rename: `.ing` -> final name on the VM.
    Rename { device_path: String },
    /// Mirror a device-side delete.
    Delete { device_path: String },
    /// Mirror a device-side directory creation.
    CreateDir { device_path: String },
}
```

### client.rs

- `Client` 新增同步 API `file_sync(sync_id, ops) -> io::Result<()>`：与 `spawn` 同模式——请求入 `shared.file_sync_req_tx`，调用线程 `mpsc` 等待回复（零线程，不嵌套 reactor block_on）。

### daemon.rs

- `SharedControl` 新增 `file_sync_req_tx/rx`。
- `spawn_daemon` 新增 `file_sync_loop`（业务 executor）：消费 `FileSyncRequest`，执行流式传输，经 mpsc 回结果：
  1. 建立独立 VM 连接（`connect_vm_handshake`，Hello→HelloOk）
  2. 发 `FileSyncStart{sync_id}`
  3. 逐个 op：
     - `WriteContent`：`map_path` → 取设备文件 len → 发 `FileBegin{vm_path, len}` → **用 `smol::unblock` 流式读设备文件写 VM 连接**（大文件不整读内存，也不阻塞 executor）→ 发 `FileRename{vm_path}`
     - `Rename`/`Delete`/`CreateDir`：发对应帧
  4. 发 `FileSyncEnd{sync_id}` → 关连接 → mpsc 回 `Ok`

FileSync 走**独立 VM 连接**，不走 `VmConnectionPool`（池连接是给 Spawn 的短生命周期 data connection；FileSync 需长时间独占传输大文件）。

## 同步引擎（新 crate `cmd-agent-sync`）

放在 `crates/gpui_ohos/depend/ohos-openeuler-agent/cmd-agent-sync`，**由 cmd-agent-client 的 `spawn_daemon` 内部启动**，不对外暴露新入口——daemon 启动即绑定同步引擎生命周期。依赖 `notify`（已在依赖树）。

### 线程与事件流

```
notify watcher 线程 ──事件──▶ pending 队列 ──debounce(静默 500ms)──▶ 归并 ──▶ shared.file_sync_req_tx ──▶ daemon file_sync_loop
```

同步引擎线程持有 `Arc<SharedControl>`（daemon 进程内共享），构造 `FileSyncRequest{ops}` 发送，阻塞等 mpsc 回复（独立线程，不在 GPUI 主线程）。

### 事件映射（只处理"写入完成"）

- `EventKind::Access(Close(Write))`（文件写完并关闭）→ `FileSyncOp::WriteContent{device_path}`
- `EventKind::Modify(Name(Rename))`（`IN_MOVED_TO` 进目录）→ 兜底 `WriteContent`（读最终路径内容，不依赖临时文件是否同步过）+ `Rename`，保证最终路径必有内容
- `EventKind::Remove`（删除/移出）→ `FileSyncOp::Delete`
- `EventKind::Create(Dir)` → 递归 `watch` 新子目录（notify 不递归，需手动）
- 忽略 `Modify(Data)` 等写入中事件

**统一原则**：对任何"最终路径的写入完成事件"，一律 `WriteContent`（`.ing` → rename）原子落地，不区分 close_write 还是 rename。

### debounce

`npm install` 批量写 `node_modules` 会产生海量事件。channel + 定时器：事件入队，500ms 无新事件触发一次批量同步；合并去重（同一路径只同步最后一次状态）。

## 竞态与原子性总结

- VM 上 `binary` 要么不存在、要么完整（先 `.ing` 后 rename）
- spawn 等待逻辑覆盖三种时序：已同步（直接跑）/ 同步中（等 `.ing`→binary 30s）/ 未开始（等 1000ms）
- 无动态名单：`is_sync_path` 是静态路径前缀，文件系统状态（`.ing`）是唯一"同步进行中"信号
- server 启动清理残留 `.ing`

## 日志规范

按 CLAUDE.md：函数入口 `info`、异常 `error`、状态转换 `info`、批量循环 `debug`。新增代码全英文注释，重点逻辑（spawn 等待、事件归并、流式传输）加注释说明设计意图。

## 修改文件清单

注：crate 逻辑名与物理名已**彻底改名**——`cmd-agent-client` → **`cmd-agent`**（目录 `cmd-agent/`，lib `cmd_agent`）、`cmd-agent-server` → **`cmd-agentd`**（目录 `cmd-agentd/`，编译产物与 VM 部署程序名均 `cmd-agentd`）。同步引擎作为 **`cmd-agent` 内部模块**（`src/sync_engine.rs`，由 `spawn_daemon` 启动，不对外暴露，避免循环依赖）。

- `cmd-agent-protocol/src/messages.rs` — FileSync 消息类型
- `cmd-agentd/src/main.rs` — 启动清理 `.ing`、`run_file_sync`（流式）、路由分支
- `cmd-agentd/src/spawn.rs` — `SYNC_DIR_RELATIVES`、`is_sync_path`、`resolve_binary_wait`
- `cmd-agent/src/client.rs` — `file_sync` API
- `cmd-agent/src/daemon.rs` — `SharedControl` 扩展、`file_sync_loop`、`spawn_daemon` 内启动同步引擎
- `cmd-agent/src/sync_engine.rs`（新模块）— 同步引擎（notify 监听 + debounce + 事件归并）
- `cmd-agent/Cargo.toml` — 新增 `notify` 依赖
- `launch-zed/Cargo.toml` — 依赖 key 与路径改 `cmd-agent`；`launch_app.rs` 传 `sync_roots`（7 个 data_dir 子目录）
- `script/bundle-ohos` — server 段改名 `cmd-agentd`（crate/产物/resfile 名）

参考：[[ohos-cmd-agent-architecture]]、[[ohos-lsp-run-mechanism]]
