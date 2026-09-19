# OHOS terminal 连 hicodeerd 时的交互 shell 被写死为 `/bin/sh`，改为 `/usr/bin/zsh`

> **状态：改动已落地、编译通过、用户确认问题已解决**（2026-09-19）。
> 改动量：`crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs` 3 行代码 + 1 行注释。
> **已知不足：落点分层不对** —— 详见文末"遗留不足"一节，用户明确指出"用哪个 shell 应该由 terminal 决定，不该在 cmd-agent 硬编码"。

## 问题描述

在 terminal 里连上设备侧的常驻守护进程（hicodeerd，旧称 zcoderd）之后，终端里跑起来的交互 shell 一直是 `/bin/sh`（设备上是 mksh）。用户要求把它换成 `/usr/bin/zsh`。

用户最初的判断是"现在是把 `/bin/sh` 作为 shell"，方向正确，但**具体是哪一个 `/bin/sh` 需要先查清**，因为链路里同时存在多个候选（见"问题原因"里的四个候选点）。

## 问题表现

- 带 daemon 后端的 terminal 里，交互 shell 是 sh，不是 zsh
- 在 terminal 设置里指定 shell（`Shell::Program`）**也不生效** —— 只要 daemon 后端在，设置值会被丢弃
- 无 daemon 后端时终端回退到本地 `/bin/sh`，这是沙箱限制，与本次问题无关

## 问题原因

### 先厘清分层：terminal 只决定"走不走 daemon"，payload 由 cmd-agent 生成

`crates/terminal/` 里与 shell 相关的写入点有 3 处，**都不是**远端交互 shell：

- `crates/terminal/src/terminal.rs:1083` —— `env.insert("SHELL", "/bin/sh")`，只设环境变量，不决定实际跑的程序
- `crates/terminal/src/terminal.rs:1134` —— `Shell::System` 分支固定 `/bin/sh`（本地 pty 的 child）
- `crates/terminal/src/ohos_shell.rs:37` —— `const HOLD_SHELL: &str = "/bin/sh"`（guest 模式下本地 pty 的**驻留** child）

第 2 条为什么够不着远端，关键在 `crates/terminal/src/terminal.rs:1239-1242`：

```rust
let alacritty_shell = match guest_shell.as_ref() {
    Some(guest) => Some(guest.local_shell_argv()),
    None => alacritty_shell,
};
```

只要 `ohos_shell::probe`（`crates/terminal/src/ohos_shell.rs:102`）探到 daemon 后端，`alacritty_shell` 就被 `guest.local_shell_argv()` **整体覆盖**。而 `local_shell_argv()`（`ohos_shell.rs:62`）返回的是：

```rust
(HOLD_SHELL.to_string(), vec!["-c".to_string(), self.hold.hold_command()])
```

`hold_command()`（`ohos_shell.rs:200-205`）是 `exec 0<fifo 1>/dev/null 2>/dev/null; read x` —— 一个被 FIFO 挂住的**占位进程**，不是交互 shell。所以连 daemon 时，terminal 侧的 shell 设置（含 `Shell::Program`）全部失效。

### 真正的落点：cmd-agent 生成的 exec payload

交互 shell 名从 terminal 到 daemon 的整条接口链上**从来没有作为参数传递过**：

- `crates/terminal/src/terminal.rs:1226` → `crate::ohos_shell::probe(cwd)`
- `crates/terminal/src/ohos_shell.rs:103` → `open_remote_shell(INITIAL_COLS, INITIAL_ROWS, cwd)`
- `crates/util/src/command/ohos.rs:746-750` → `open_remote_shell(cols: u32, rows: u32, cwd: Option<&str>)` —— 签名里没有 shell
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs:180` → `let command = crate::pty::shell_command(cwd);` —— **payload 在这里生成**
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs:70-78` → `shell_command()` 里拼出 `exec sh`
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs:96` → `channel.exec(true, command)` —— 发给 daemon
- daemon 侧 `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/pty.rs:236-237` → 以 `PTY_SHELL`（`/bin/sh`）加 `-c` 承载这个 payload，payload 里的 `exec sh` 再用 `exec` 替换掉外层 shell

所以最终跑的是 payload 里的那个 shell，**不是** daemon 的 `PTY_SHELL`。

### 排查中走过的弯路（dead ends）

- **先怀疑 daemon 的 `PTY_SHELL`**（`hicodeerd/src/pty.rs:33`）。它是仓库里最显眼的 shell 常量，且名字就叫 `SHELL`。但它是 `-c` 的承载者，payload 里的 `exec sh` 会覆盖它 —— 改它不产生任何效果。
- **再怀疑 terminal 侧**。按用户"只看 terminal 代码"的范围查完 3 处（见上），确认都被 `terminal.rs:1239-1242` 的覆盖逻辑挡在远端之外。
- 结论：要改必须落在 cmd-agent 生成 payload 的那一处。

## 解决方案（已实施的修改）

`crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs`

改前：

```rust
        Some(dir) => format!(
            "cd {} 2>/dev/null; exec sh",
            crate::command::sh_quote(dir)
        ),
        None => "exec sh".to_string(),
```

改后（新增常量 + 两处引用它）：

```rust
/// Interactive shell the daemon pty ends up running: the payload built below
/// execs it, replacing the shell that carries `-c`. The daemon runs outside the
/// app sandbox, so a shell the sandbox itself may not exec is usable here.
const INTERACTIVE_SHELL: &str = "/usr/bin/zsh";
```

```rust
        Some(dir) => format!(
            "cd {} 2>/dev/null; exec {INTERACTIVE_SHELL}",
            crate::command::sh_quote(dir)
        ),
        None => format!("exec {INTERACTIVE_SHELL}"),
```

选这个方案的理由：

- **用绝对路径**：`exec sh` 原本靠 PATH 查找；`/bin/zsh` 在设备上不存在，只有 `/usr/bin/zsh`（实测 `-rwxr-xr-x`，1332616 字节），写绝对值避免 PATH 依赖
- **抽常量**：同一字面量要在两个分支各写一次，抽出来后只有一个真值来源；`None` 分支也因此从 `"exec sh".to_string()` 变成 `format!`
- **daemon 一行不动**：`PTY_SHELL` 保持 `/bin/sh`。它是 `-c` 的承载者，用 mksh 跑一条 `exec` 是最稳的；改成 zsh 既不改变最终 shell，还会平白引入 zsh 的启动开销与非 POSIX 语义风险。副作用是**本次不需要重启 hicodeerd**（改的是 host 侧代码，随 HAP 重装生效）

编译验证：`./script/bundle-ohos` → `EXIT=0`、`=== HAP BUILD SUCCESSFUL ===`，耗时 4m31s（增量）。产物 `hap/entry/build/default/outputs/default/entry-default-signed.hap`。

## 修改文件

- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs` — `:25-28` 新增 `INTERACTIVE_SHELL` 常量；`:73` 与 `:76` 的 payload 由 `exec sh` 改为 `exec {INTERACTIVE_SHELL}`；`:81` 函数文档里的 `exec sh` 字样同步为 `exec /usr/bin/zsh`
- daemon 侧 `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/pty.rs` — **未改动**（`PTY_SHELL` 仍为 `/bin/sh`，理由见上）

## 遗留不足（用户指出，尚未处理）

用户对本次方案的判断原话：**"方案不够完美，使用哪个 shell，应该是有 terminal 决定，不应该是 cmd-agent 硬编码才对。"**

这条判断在分层上是对的：

- **用哪个 shell 是终端语义**，对应 Zed 自己的 `terminal.shell` 设置，属用户可配置项，归 `crates/terminal/`
- **cmd-agent 是传输层**，职责是把 payload 送到设备侧执行，不该内置"用哪个 shell"这类策略
- 现状后果：`INTERACTIVE_SHELL` 成了一个不可配置的硬编码值；用户在设置里改 shell 依然无效；换 shell 要改代码并重新编译

理想方案是把 shell 作为参数沿链路透传，需要动三个 crate：

- `crates/util/src/command/ohos.rs:746-750` —— `open_remote_shell` 增加 shell 参数
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/types.rs:114` 与 `executor.rs:177` —— 扩 `open_shell_pty` 的 trait 方法与 `ShellPtyFuture`
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs:70` —— `shell_command()` 接收 shell 名，不再硬编码
- `crates/terminal/src/terminal.rs:1231-1242` —— 把 `shell_params` 解析出的 shell 传下去（而不是被 `local_shell_argv()` 覆盖掉）

本次未做，因为用户当时的诉求是"先把 shell 换成 zsh"，且该重构跨 3 个 crate、涉及 trait 签名变更，属独立的接口改造。

## 验证状态与观察点

- 编译：`EXIT=0` + `HAP BUILD SUCCESSFUL`（已确认）
- 用户已确认问题解决；如需复核，在连 daemon 的 terminal 里跑：
  - `echo $0` —— 期望 `/usr/bin/zsh`
  - `ps -o comm= -p $$` —— 期望 `zsh`
- 若仍显示 sh，先确认该终端是否真的走了 daemon 后端：走本地回退时仍是 `/bin/sh`（`ohos_shell.rs:3` 注明沙箱只允许 exec `/bin/sh`），这属另一个问题
- 行为差异提醒：zsh 会读 `~/.zshrc`，若 HOME 下没有 zsh 配置，提示符会退回 zsh 默认样式，属预期现象

## 附：与既有记录的关系

- 同链路 pty 问题：[[2026-09-09-zcoderd-pty-slave-readonly]]（pty slave 只读致终端无回显）、[[2026-09-11-ohos-pty-channel-key-collision]]（channel id 跨连接串号）
- 本次与"休眠后 terminal panel 自动关闭"（[[2026-09-18-ohos-terminal-closed-after-suspend]]）无关，后者是 hicodeerd 存活判据的改造
- [[ohos-debug-lessons]]
