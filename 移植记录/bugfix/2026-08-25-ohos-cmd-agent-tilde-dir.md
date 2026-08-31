# cmd-agent 在 VM 上创建 `~` 字面目录（sh_quote 阻止波浪号展开）

## 问题描述 (Problem Description)

cmd-agent daemon 通过 SSH 部署 server 到 VM 时，在 `/home/user` 下创建了一个**名为 `~` 的字面目录**（`/home/user/~/cmd-agent/`），而不是期望的 `$HOME` 展开目录 `/home/user/cmd-agent/`。server 二进制与 server.log 都被部署进了错误目录。

## 问题表现 (Symptoms)

- VM 上出现异常目录：`/home/user/~/cmd-agent/cmd-agent-server`、`/home/user/~/cmd-agent/server.log`。
- `~` 目录由 `user` 用户所有，内容与正确部署的 cmd-agent 目录雷同，容易混淆。
- 服务器实际从 `/home/user/~/cmd-agent/cmd-agent-server` 运行（ps 可见），而非预期的 `/home/user/cmd-agent/`。

## 问题原因 (Root Cause)

部署代码用 `sh_quote()` 给远程路径加单引号，**单引号内的 `~` 不会被 shell 展开**：

```rust
// cmd-agent-client/src/deploy.rs
async fn create_remote_dir(session: &mut SshSession, dir: &str) -> Result<()> {
    let command = format!("mkdir -p {}", sh_quote(dir));  // sh_quote("~/cmd-agent") → '~/cmd-agent'
    run_simple(session, command, "mkdir").await
}
```

`sh_quote("~/cmd-agent")` 生成 `'~/cmd-agent'`，SSH 上执行的完整命令是 `mkdir -p '~/cmd-agent'`。bash 收到单引号包裹的字符串时，`~` 不再做波浪号展开，于是 `mkdir` 把 `~/cmd-agent` 当作**相对路径**，在当前目录（`/home/user`）下创建了字面目录 `~` 及子目录 `cmd-agent`。

`start_server` 同样用 `sh_quote(remote_dir)` 拼 server.log 路径（`> '~/cmd-agent'/server.log`），server.log 也写进了字面目录。

## 解决方案 (Solution)

把远程目录从 `~/cmd-agent` 改为**绝对路径** `/home/user/cmd-agent`，`sh_quote` 对绝对路径加引号后无需展开，语义完全正确：

```rust
// crates/gpui_ohos/depend/launch-zed/src/launch_app.rs
const REMOTE_DIR: &str = "/home/user/cmd-agent";  // 原为 "~/cmd-agent"
```

`cmd-agent-client/src/daemon.rs` 的 `parse_args` 命令行默认值同步改为绝对路径，避免独立命令行入口触发同一问题。

**遗留**：旧的 `/home/user/~/` 字面目录会残留，功能不受影响，需人工确认后清理（不自动删除）。

**验证**：重启 zcoder 后 server 正确部署到 `/home/user/cmd-agent/cmd-agent-server`，`ls -d /home/user/~/` 确认不再创建新字面目录。

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — `REMOTE_DIR` 由 `"~/cmd-agent"` 改为绝对路径 `/home/user/cmd-agent`
- `crates/gpui_ohos/depend/ohos-openeuler-agent/cmd-agent-client/src/daemon.rs` — `parse_args` 默认 `remote_dir` 同步改为绝对路径

参考：[[ohos-debug-lessons]]
