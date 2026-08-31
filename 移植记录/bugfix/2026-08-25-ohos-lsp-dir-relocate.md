# LSP 安装目录迁移到 cmd-agent 数据目录下（路径映射 Rule A 变更）

## 问题描述 (Problem Description)

zcoder 在 OHOS 上通过 cmd-agent 把设备应用沙箱的 LSP 安装目录（`/data/storage/el2/base/haps/entry/files/zed/...`）映射到 VM 时，最初映射到登录目录 `$HOME/zed`（即 `/home/user/zed`）。用户提出：LSP 安装目录应放在 cmd-agent 自己的数据目录下（`/home/user/cmd-agent/zed`），与 cmd-agent 的部署位置保持一致，更符合"cmd-agent 统一管理 VM 上 zcoder 数据"的语义。

## 问题表现 (Symptoms)

- LSP 二进制、npm 包、node cache 分布在 `/home/user/zed/`（约 121M），与 cmd-agent 的 `/home/user/cmd-agent/` 分离。
- 目录归属不清晰：`/home/user/zed` 与 `/home/user/~/cmd-agent`（早期 bug 残留）并存，运维与排查时容易混淆。

## 问题原因 (Root Cause)

`cmd-agent-server/src/spawn.rs` 的路径映射 Rule A 把设备沙箱文件根硬编码映射到 `$HOME`：

```rust
// 修复前
const DEVICE_APP_FILES_ROOT: &str = "/data/storage/el2/base/haps/entry/files";
// Rule A: <DEVICE_APP_FILES_ROOT>/... -> $HOME/...
if let Some(rest) = path.strip_prefix(DEVICE_APP_FILES_ROOT) {
    if rest.is_empty() || rest.starts_with('/') {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        return format!("{}{}", home, rest);
    }
}
```

设备 `/data/storage/el2/base/haps/entry/files/zed/...` 被映射到 `/home/user/zed/...`。该目录由登录 shell 的 `HOME` 决定，与 cmd-agent 部署目录（`/home/user/cmd-agent`）无关联。

## 解决方案 (Solution)

将 Rule A 的目标根改为固定的 cmd-agent 数据目录 `/home/user/cmd-agent`：

```rust
// 修复后
const DEVICE_APP_FILES_ROOT: &str = "/data/storage/el2/base/haps/entry/files";
const VM_AGENT_ROOT: &str = "/home/user/cmd-agent";
// Rule A: <DEVICE_APP_FILES_ROOT>/... -> <VM_AGENT_ROOT>/...
if let Some(rest) = path.strip_prefix(DEVICE_APP_FILES_ROOT) {
    if rest.is_empty() || rest.starts_with('/') {
        return format!("{VM_AGENT_ROOT}{rest}");
    }
}
```

- 设备 `.../files/zed/languages/rust-analyzer` → VM `/home/user/cmd-agent/zed/languages/rust-analyzer`。
- 不再依赖 `$HOME` 环境变量，路径确定性更强，与 server 部署目录（`/home/user/cmd-agent`）统一。
- Rule B（`/storage/Users/currentUser` ↔ `/mnt/linux_share` 共享盘）保持不变。

**迁移**：把旧目录 `mv /home/user/zed /home/user/cmd-agent/zed` 保留 LSP 缓存，避免重新下载；若缓存已丢失则 zcoder 自动在新路径重建（rust-analyzer GitHub 下载、vtsls/tailwind npm install、gopls 走 `which` 在 PATH）。

**验证**：重启 zcoder 后 `/home/user/cmd-agent/zed/languages/` 重建，vtsls/tailwind/rust-analyzer 均从新路径运行，gopls 通过 PATH 检测直接启动，全部稳定。

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/ohos-openeuler-agent/cmd-agent-server/src/spawn.rs` — 新增 `VM_AGENT_ROOT` 常量；Rule A 目标由 `$HOME` 改为 `/home/user/cmd-agent`；更新常量注释

参考：[[ohos-debug-lessons]]
