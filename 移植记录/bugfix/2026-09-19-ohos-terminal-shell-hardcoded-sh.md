# OHOS terminal 连 hicodeerd 时的交互 shell 被 cmd-agent 固化：从硬编码 `/usr/bin/zsh` 到 program/args 透传

> **状态：最终方案已落地、交叉编译通过、设备装包实测通过**（2026-09-19）。
> 最终改动量：7 个文件 —— cmd-agent 侧（pty.rs / types.rs / executor.rs）+ launch-zed 1 处 + util 1 处 + terminal 2 处。
> 本记录合并了同一天的两轮迭代：**第一轮**把固化值从 `/bin/sh` 改成 `/usr/bin/zsh`（功能可用但分层不对，用户判定返工），**第二轮**按用户要求把 shell 决定权还给 terminal，改为 program/args 全链透传。

## 问题描述

终端连上设备侧的常驻守护进程（hicodeerd，旧称 zcoderd）之后，**交互 shell 不由 terminal 决定，而被 cmd-agent 固化**。

第一轮的表象是"shell 一直是 `/bin/sh`（设备上是 mksh），用户要求换成 `/usr/bin/zsh`"。按这个诉求改完后发现只解决了表象：改动是把固化值从 `sh` 换成了 `INTERACTIVE_SHELL = "/usr/bin/zsh"`，**仍然是 cmd-agent 里的硬编码**，用户在 terminal 设置里指定的 shell 照样无效。

用户的最终判定是分层问题：**"使用哪个 shell 应该由 terminal 决定，不应该是 cmd-agent 硬编码"**，并明确要求"cmd-agent 运行 shell 指令时，按照命令发送方指定的程序和参数运行，不要自行替换"。

## 问题表现

- **第一轮的形态**：带 daemon 后端的 terminal 里交互 shell 是 sh 而非 zsh；在 terminal 设置里指定 shell（`Shell::Program`）**不生效**——只要 daemon 后端在，设置值就被丢弃
- **第二轮（硬编码 zsh 之后）暴露的形态**：影响面远超"终端里 shell 不对"——
  - 在 codebuddy 里执行 shell 命令，**只返回 zsh 提示符**，命令本身没有任何输出
  - `Grep` / `Glob` 报 `ripgrep exited with code null`（同一条通道被接管）
  - ACP 任务的 terminal：`create_terminal_entity` 明明构造了 `command` + `args`（形如 `sh -i -c '<script>'`），终端里却只跑起一个纯 zsh 交互 shell，任务命令凭空消失
- 无 daemon 后端时终端回退到本地 `/bin/sh`，这是沙箱限制（`ohos_shell.rs` 注明沙箱只允许 exec `/bin/sh`），与本问题无关

## 问题原因

### 先厘清分层：terminal 只决定"走不走 daemon"，payload 由 cmd-agent 生成

`crates/terminal/` 里与 shell 相关的写入点有 3 处，**都不是**远端交互 shell：

- `crates/terminal/src/terminal.rs:1083` —— `env.insert("SHELL", "/bin/sh")`，只设环境变量，不决定实际跑的程序
- `crates/terminal/src/terminal.rs:1134` —— `Shell::System` 分支固定 `/bin/sh`（本地 pty 的 child）
- `crates/terminal/src/ohos_shell.rs:37` —— `const HOLD_SHELL: &str = "/bin/sh"`（guest 模式下本地 pty 的**驻留** child）

第 2 条为什么够不着远端，关键在 `crates/terminal/src/terminal.rs:1249-1252`：

```rust
let alacritty_shell = match guest_shell.as_ref() {
    Some(guest) => Some(guest.local_shell_argv()),
    None => alacritty_shell,
};
```

只要 `ohos_shell::probe` 探到 daemon 后端，`alacritty_shell` 就被 `guest.local_shell_argv()` **整体覆盖**，而它返回的是 `hold_command()`（`exec 0<fifo 1>/dev/null 2>/dev/null; read x`）——一个被 FIFO 挂住的**占位进程**，不是交互 shell。所以连 daemon 时，terminal 侧的 shell 设置（含 `Shell::Program`）全部失效。

### 真正的落点：cmd-agent 生成的 exec payload

交互 shell 名从 terminal 到 daemon 的整条接口链上**从来没有作为参数传递过**：

- `crates/terminal/src/terminal.rs:1226` → `crate::ohos_shell::probe(cwd)`
- `crates/terminal/src/ohos_shell.rs:103` → `open_remote_shell(INITIAL_COLS, INITIAL_ROWS, cwd)`
- `crates/util/src/command/ohos.rs:746-750` → `open_remote_shell(cols, rows, cwd)` —— 签名里没有 shell
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs:180` → `let command = crate::pty::shell_command(cwd);` —— **payload 在这里生成**
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs` → `shell_command(cwd)` 里拼出 `exec <硬编码 shell>`
- `pty.rs` → `channel.exec(true, command)` —— 发给 daemon
- daemon 侧 `hicodeerd/src/pty.rs:236-237` → 以 `PTY_SHELL`(`/bin/sh`) 加 `-c` 承载这个 payload，payload 里的 `exec` 再用目标程序替换掉外层 shell

改前的 `shell_command`：

```rust
const INTERACTIVE_SHELL: &str = "/usr/bin/zsh";

pub(crate) fn shell_command(cwd: Option<&str>) -> String {
    match cwd.filter(|dir| !dir.is_empty()) {
        Some(dir) => format!(
            "cd {} 2>/dev/null; exec {INTERACTIVE_SHELL}",
            crate::command::sh_quote(dir)
        ),
        None => format!("exec {INTERACTIVE_SHELL}"),
    }
}
```

**它不是"忽略了" program/args，而是压根没有参数入口。** 所以最终跑的是 payload 里那个固化值，既不是 daemon 的 `PTY_SHELL`，更不是 terminal 的设置。

### 排查中走过的弯路（dead ends）

- **先怀疑 daemon 的 `PTY_SHELL`**（`hicodeerd/src/pty.rs:33`）。它是仓库里最显眼的 shell 常量，名字就叫 `SHELL`。但它是 `-c` 的承载者，会被 payload 里的 `exec` 覆盖——改它不产生任何效果。
- **再怀疑 terminal 侧**。按范围内查完上文 3 处，确认都被 `local_shell_argv()` 的覆盖逻辑挡在远端之外。
- **第一轮的修法是"改固化值"**（加 `INTERACTIVE_SHELL = "/usr/bin/zsh"`）。功能上立刻可用，但用户当场判定**分层不对**——这正是第二轮返工的原因：错误修法是"把常量指向改成想要的"，正确修法是"把决定权还给调用方"。
- **本机无法编译验证**：`cargo check -p cmd-client` 在 host 目标下卡在依赖 `rustix`（`&libc::rlimit64` 与 `*const libc::rlimit` 类型不匹配，musl/OHOS 下的已知不兼容），与本次改动无关。验证只能靠 `./script/bundle-ohos` 交叉编译。
- **工具通道自身的故障一度阻塞排查**：Bash 只回提示符、Grep 报 `exited with code null`，导致"改代码 → 验证"无法进行；这正是被修的问题本身造成的，通道恢复后才完成编译与实测。

## 解决方案

### 第一轮（临时，已被第二轮取代）

在 `cmd-client/src/pty.rs` 新增常量 `INTERACTIVE_SHELL = "/usr/bin/zsh"`，payload 由 `exec sh` 改为 `exec {INTERACTIVE_SHELL}`。

选绝对路径而非 `exec zsh` 是因为 `/bin/zsh` 在设备上不存在，只有 `/usr/bin/zsh`（实测 `-rwxr-xr-x`，1332616 字节）。daemon 不动。

这一版让终端里的 shell 变成了 zsh，但 `INTERACTIVE_SHELL` 成了不可配置的硬编码值——用户指出"用哪个 shell 该由 terminal 决定"，遂返工。

### 第二轮（最终方案）

**核心思路：让 payload 生成函数接收调用方的 program/args，逐项安全 quote 后原样 `exec`，不设默认值。**

改后（`cmd-client/src/pty.rs`）：

```rust
pub(crate) fn shell_command(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
) -> std::io::Result<String> {
    if program.is_empty() {
        log::error!("cmd-client pty: shell_command called with an empty program");
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pty shell: caller specified no program",
        ));
    }
    let mut command = vec![crate::command::sh_quote(program)];
    command.extend(args.iter().map(|arg| crate::command::sh_quote(arg)));
    let exec = format!("exec {}", command.join(" "));
    Ok(match cwd.filter(|dir| !dir.is_empty()) {
        Some(dir) => format!("cd {} 2>/dev/null; {exec}", crate::command::sh_quote(dir)),
        None => exec,
    })
}
```

关键取舍：

- **空 program 报错，不回落**。这是"不自行替换"的强约束：调用方没给 shell，就要被告知，而不是静默拿到一个。错误沿 `?` 一路上抛，`util::open_remote_shell` 记 `warn`，terminal 据此回退本地 `/bin/sh`（保持"连不上后端也能开终端"的既有语义）。
- **args 逐项 quote 后作为独立参数串联**，不是拼成一条命令字符串。`sh_quote`（`cmd-client/src/command.rs:55`）是标准 `'\''` 转义实现，因此 ACP 传进来的多行脚本（`exec </dev/null\n<script>`）在单引号内保持字面换行，由 `sh -c` 正确解析——与 `ShellBuilder` 的 Posix 分支设计一致。
- **抽参数而不是加常量**：删掉 `INTERACTIVE_SHELL`，shell 的选择权回到 `terminal` 的 `shell_params`，与 Zed 自己的 `terminal.shell` 设置语义对齐。
- **daemon 一行不动**：`PTY_SHELL` 保持 `/bin/sh`。它是 `-c` 的承载者，用 mksh 跑一条 `exec` 最稳；改成别的既不改变最终程序，还会引入额外启动开销与非 POSIX 语义风险。同理 `rewrite_npm_para`（`hicodeerd/src/exec.rs`）只在 **exec 路径**生效，pty 路径不涉及，不构成第二个替换点。

透传链路：

```
terminal.rs:1229  probe(cwd, &params.program, params.args.as_deref().unwrap_or(&[]))
  └─ ohos_shell::probe(cwd, program, args)
       └─ open_remote_shell(cols, rows, cwd, program, args)
            └─ executor.open_shell_pty(cols, rows, cwd, program, args)
                 └─ pty::shell_command(program, args, cwd)
```

**编译验证**：`./script/bundle-ohos` → `BUNDLE_EXIT=0` + `hvigor BUILD SUCCESSFUL` + `=== HAP BUILD SUCCESSFUL ===`（全量与补日志后的增量各一次）。`util` / `terminal` / `terminal_view` / `launch-zed` 均编译通过——这几处改动都在 `cfg(target_env = "ohos")` 下，必须交叉编译才能覆盖。产物 `hap/entry/build/default/outputs/default/entry-default-signed.hap`。

**设备实测**（装包后，由助手直接执行）：

- `echo probe-ok` / `uname -a` / `pwd` / `ls -1 crates | wc -l`（→`244`）/ `printf 'a\nb\nc\n' | sort -r | tr`（→`c b a`）全部正常，多级管道与多步命令可用
- `rg --version` → `ripgrep 15.2.0`；`rg --json -n` 正常吐 JSON
- `shell=$0` → `/bin/sh`（发送方指定，不再是硬编码 zsh）
- `zsh -c 'echo $ZSH_VERSION'` → `5.9`
- **反向验证**：故意指定 `bash` → 原样在 bash 里执行（`bash_ok=5.3.9(1)-release`，设备上位于 `/storage/Users/currentUser/.harmonybrew/bin/bash`）。修复前这会被无声替换成 zsh，这就是"不自行替换"的直接证据

## 修改文件

最终态（第二轮）共 7 个文件：

- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/pty.rs` — 删除 `INTERACTIVE_SHELL` 常量；`shell_command` 改签名为 `(program, args, cwd) -> io::Result<String>`，逐项 `sh_quote` 拼 `exec`，空 program 报 `InvalidInput` 并记 error 日志；更新函数文档
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/types.rs` — trait `RemoteCommandExecutor::open_shell_pty` 增加 `program: &'a str` 与 `args: &'a [String]`，默认实现仍返回 `Unsupported`
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/executor.rs` — `SshCommandExecutor` 实现透传，`shell_command(program, args, cwd)?`
- `crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs` — `WorkdirAwareExecutor` 实现透传（**第二轮排查新发现的遗漏点**：不改会因 trait 签名不匹配而编译失败）
- `crates/util/src/command/ohos.rs` — `open_remote_shell` 增加 `program`/`args`，入口 `info` 日志补上这两个字段
- `crates/terminal/src/ohos_shell.rs` — `probe` 增加 `program`/`args` 并透传；本地占位子进程仍是 `HOLD_SHELL`(`/bin/sh`)，与远端 program 解耦
- `crates/terminal/src/terminal.rs` — probe 调用点改传 `shell_params` 的 program/args；改动**全部位于 `#[cfg(target_env = "ohos")]` 块内**，对非 OHOS 编译与行为零影响

未改动（理由见"解决方案"）：

- daemon 侧 `crates/gpui_ohos/depend/cmd-agent/hicodeerd/` —— `PTY_SHELL` 仍为 `/bin/sh`，`rewrite_npm_para` 仍只在 exec 路径生效

第一轮的改动（`pty.rs` 新增 `INTERACTIVE_SHELL` 常量）已被第二轮覆盖并删除，最终态中不存在。

## 行为变化（需知悉）

修复让一批原本被"替换"掩盖的语义**真正生效**，属预期但有可感知差异：

- **ACP terminal 的 stdin 重定向生效**：`create_terminal_entity` 显式调用了 `redirect_stdin_to_dev_null()`，其 Posix 分支会插入 `exec </dev/null`。修复前这段 args 被丢弃，ACP 终端实际是可交互 zsh；修复后按 args 执行，该终端**无法输入**（这是任务终端的设计意图）
- **设置里配置的 shell 会真实生效或真实失败**：`Shell::Program(p)` 现在走 `exec 'p'`。之前无论配什么都被替换成 zsh，所以"总能打开"；现在若 p 在设备上不存在，终端会起不来——这是"不替换"的代价
- **默认 shell 由发送方决定**：当前发送方指定 `/bin/sh`。若要默认 zsh，需在发送方配置，而不是回到 cmd-agent 硬编码

## 验证状态与观察点

- 编译：`EXIT=0` + `HAP BUILD SUCCESSFUL`（已确认）
- 设备实测：已确认（见上）
- 未决的小问题：`Grep` 工具仍报 `ripgrep exited with code null`，而手动 `rg`（含 `--json`）完全正常——属工具调用层问题，与本链路无关，另行排查
- 引用点唯一性已核：`shell_command` 仅 `executor.rs:187` 调用，`ohos_shell::probe` 仅 `terminal.rs:1231`，`open_remote_shell` 仅 `ohos_shell.rs:104`；全仓 `impl RemoteCommandExecutor` 仅 2 处，`/usr/bin/zsh` 硬编码无残留
- 若终端仍显示 sh，先确认该终端是否真的走了 daemon 后端：走本地回退时仍是 `/bin/sh`，属沙箱限制

## 附：与既有记录的关系

- 同链路 pty 问题：[[2026-09-09-zcoderd-pty-slave-readonly]]（pty slave 只读致终端无回显）、[[2026-09-11-ohos-pty-channel-key-collision]]（channel id 跨连接串号）
- 本次与"休眠后 terminal panel 自动关闭"（[[2026-09-18-ohos-terminal-closed-after-suspend]]）无关，后者是 hicodeerd 存活判据的改造
- [[ohos-debug-lessons]]
