# OHOS vtsls LSP 静默 exit 1（vscode-languageserver watchdog 跨机器 PID 失效）

## 问题描述 (Problem Description)

ets 语言支持依赖 vtsls（`@vtsls/language-server`，基于 vscode-languageserver 的 TypeScript 语言服务器）。vtsls 在 VM 上由 cmd-agent 启动后，能打印正常的启动日志（"Starting TS Server / Using tsserver from ... / Forking... / Starting..."），但**约 3-4 秒后主进程以退出码 1 静默退出**，zcoder 报 `via vtsls failed: server shut down` / `Server reset the connection`。clangd、rust-analyzer 等原生 LSP 正常，只有基于 vscode-languageserver 的 LSP（vtsls、tailwindcss-language-server）受影响。

## 问题表现 (Symptoms)

- zcoder 界面显示 TS Server 启动输出（fork 了 syntax/semantic 两个 tsserver 子进程），随后 ets 功能不可用。
- zcoder 日志：`Get code actions via vtsls failed: server shut down`、`Shutdown request failure, server vtsls (id 1): server shut down`。
- cmd-agent server.log：`session 83 done, exit_code=Some(1)`（vtsls.js spawn 后 4 秒退出，无 stderr）。
- 多次重启 zcoder 均复现：spawn → 3-4 秒 → exit 1，无任何 stderr 输出（静默退出）。
- tailwindcss-language-server（同样基于 vscode-languageserver）也 exit 1，问题范围一致。

## 问题原因 (Root Cause)

vscode-languageserver 内置 **watchdog（看门狗）** 机制，检查"父进程（LSP 客户端）是否还活着"：

```js
// node_modules/vscode-languageserver/lib/node/main.js
const watchDog = {
    initialize: (params) => {
        const processId = params.processId;          // zcoder 发来的设备进程 PID
        if (Is.number(processId) && exitTimer === undefined) {
            setInterval(() => {
                try {
                    process.kill(processId, 0);      // 每 3 秒探测父进程存活
                } catch (ex) {
                    process.exit(_shutdownReceived ? 0 : 1);  // 探测失败 → exit 1
                }
            }, 3000);
        }
    },
};
```

链路：zcoder 在 LSP `InitializeParams` 里发送 `process_id = std::process::id()`（设备上 zcoder 进程的 PID）→ vtsls 收到后 watchdog 每 3 秒 `process.kill(设备PID, 0)` 探测。但 **vtsls 运行在 VM 上，设备 PID 在 VM 的 PID 命名空间里不存在**，`process.kill` 抛 `ESRCH` → watchdog 误判"父进程已死"→ `process.exit(1)`。

关键证据：
- exit 时间稳定在 spawn 后 **3-4 秒**（watchdog 的 3 秒周期）。
- vtsls 有完整的启动输出（说明初始化走到 fork 子进程之后），但无 stderr（`process.exit` 直接退出）。
- clangd / rust-analyzer 是原生实现，不用 vscode-languageserver，**没有 watchdog**，因此不受影响。

## 解决方案 (Solution)

在 OHOS 上 LSP 的 `InitializeParams` 里**不发送 processId**（设为 `None`），vscode-languageserver 的 watchdog 只在 `processId` 是 number 时才启动定时器，因此直接禁用该 watchdog：

```rust
// crates/lsp/src/lsp.rs 的 InitializeParams 构造
InitializeParams {
    // OHOS: the language server runs on the VM while this process runs on the
    // device, so a device PID is meaningless there. vscode-languageserver's
    // watchdog polls `process.kill(processId, 0)` and would see the device PID as
    // dead on the VM, killing the server ~3s after initialize. Sending no PID
    // disables that watchdog.
    #[cfg(target_env = "ohos")]
    process_id: None,
    #[cfg(not(target_env = "ohos"))]
    process_id: Some(std::process::id()),
    ...
}
```

**为什么方案可行**：
- watchdog 只是"父进程崩溃时回收 LSP"的保险。vtsls 由 cmd-agent-server 管理进程组，连接断开时 server 会 `kill(-pid)` 整组回收，父进程（zcoder）崩溃后连接自然断开，VM 侧不会泄漏进程。所以禁用 watchdog 不引入资源泄漏。
- 这是全局修改（`process_id: None` 对 OHOS 上所有 LSP 生效），对原生 LSP（clangd/rust-analyzer）无副作用——它们不用 vscode-languageserver 的 watchdog。
- 另一个 watchdog 入口是命令行 `--clientProcessId` 参数（`setupExitTimer`），zcoder 启动参数里未传，无需处理。

**验证**：vtsls 主进程 + 两个 tsserver 子进程 + typingsInstaller 持续存活 10 分钟+，无 exit；tailwindcss-language-server 的 exit 1 一并解决。

## 修改文件 (Modified Files)

- `crates/lsp/src/lsp.rs` — `InitializeParams.process_id` 在 `#[cfg(target_env = "ohos")]` 下设为 `None`，禁用 vscode-languageserver watchdog

参考：[[ohos-debug-lessons]]
