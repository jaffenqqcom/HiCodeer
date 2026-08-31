# OHOS npm 安装 LSP 包报 EACCES / ENOENT（路径映射与工作目录缺失）

## 问题描述 (Problem Description)

zcoder 在 OHOS 设备上通过 cmd-agent 把 `npm install` 转发到 VM（OpenEuler）执行，用于安装 typescript-language-server、tailwindcss-language-server、vtsls、prettier 等 npm 包。安装持续失败，先报 `EACCES`（在设备路径创建目录被拒），修复路径映射后又报 `ENOENT`（找不到 npm 二进制路径）。这导致 vtsls、tailwind 等基于 npm 的 LSP 无法安装，ets 与 tailwind 语言支持不可用。

## 问题表现 (Symptoms)

- zcoder 界面上 tailwindcss-language-server 报错：`mkdir '/data/storage/el2/base/haps/entry/files/zed'` 失败（EACCES）。
- cmd-agent server.log 记录 npm spawn 失败：
  - 修复前：`failed to spawn /usr/bin/npm: io error: No such file or directory (os error 2)`，且 `mapped_args` 里 `--cache` 参数仍是设备路径 `/data/storage/el2/base/haps/entry/files/zed/node/cache`。
  - 同一批失败中 session 66/67（无 `cwd` 的 `npm config`）成功，而 session 64/71/72（带 `cwd` 指向 prettier/vtsls/tailwind 目录）全部失败——强烈指向工作目录问题。
- 现象归纳：**带 `cwd` 的 npm 命令全挂，不带 `cwd` 的 npm 命令成功**。

## 问题原因 (Root Cause)

两个独立缺陷叠加，都位于 cmd-agent-server 的 `spawn.rs`：

### 缺陷 1：`--flag=<path>` 形式的参数未做路径映射

zcoder 的 npm 调用有两种传参形式：
- 空格分隔：`--cache /data/storage/...`（每个参数独立，`/data/...` 会被 `map_path` 处理）
- **等号内联**：`--cache=/data/storage/...`（npm 会把 `--cache=<dir>` 合并为一个参数）

对等号内联形式，`map_path` 只对"整个参数"调用，`--cache=/data/storage/...` 不以设备路径前缀开头，整体原样返回 → 设备路径泄漏到 VM 上的 npm → npm 尝试在设备路径写缓存目录 → EACCES。

### 缺陷 2：映射后的工作目录在 VM 上不存在

`Command::current_dir()` 对**不存在的目录**调用 `spawn` 会直接返回 `ENOENT`。zcoder 在设备侧先创建了 `/data/storage/.../zed/prettier` 等目录，但 map_path 把它们映射到 VM 的 `$HOME/zed/prettier`（即 `/home/user/zed/prettier`）——这个目录在 VM 上**从未存在**（npm 的 `--prefix` 会自己建目录，但 `current_dir` 必须先于 spawn 存在）。于是所有带 cwd 的 npm 命令都 ENOENT。无 cwd 的 `npm config`（session 66/67）恰好避开此路径，成功——这成为定位的突破口。

## 解决方案 (Solution)

### 修复 1：`map_path` 识别并映射 `--flag=<path>`

```rust
// 修复前：--cache=/data/... 整体不匹配前缀，原样返回
// 修复后：split_once('=') 拆出值部分，仅当值以 '/' 开头时映射值
pub fn map_path(path: &str) -> String {
    if path.starts_with('-') {
        if let Some((flag, value)) = path.split_once('=') {
            if !value.is_empty() && value.starts_with('/') {
                return format!("{flag}={}", map_path_value(value));
            }
        }
    }
    map_path_value(path)
}
```

只在参数以 `-` 开头且含 `=`、值以 `/` 开头时拆分映射，普通路径与脚本内容整段透传，不误伤。

### 修复 2：spawn 前自动创建映射后的 cwd

```rust
if let Some(cwd) = &cwd {
    if !Path::new(cwd).exists() {
        log::info!("spawn: creating mapped cwd {cwd:?}");
        if let Err(err) = std::fs::create_dir_all(cwd) {
            log::warn!("spawn: failed to create mapped cwd {cwd:?}: {err}");
        }
    }
    command.current_dir(cwd);
}
```

映射后 cwd 不存在即 `create_dir_all`（幂等，缺失才建）。创建失败仅 warn 不中断，让 spawn 自身暴露真实错误。

**为什么在 server 侧修复而非 zcoder 侧**：用户明确要求"cmd server 的代码里完成路径映射，不用 zcoder 这边改"。cmd-agent-server 是所有命令在 VM 上的统一入口，在这里做路径适配可覆盖任意参数形式，最彻底。

## 验证结果

- `@tailwindcss/language-server` 0.16.0、`@vtsls/language-server` 0.3.0 成功安装到 `/home/user/cmd-agent/zed/languages/`。
- npm cache 目录（`_cacache`、`_logs`）正常创建，无 EACCES/ENOENT 残留。
- server.log 的 `mapped_args` 显示 `--cache=/home/user/zed/node/cache`（空格与等号两种形式均正确映射）。
- tailwindcss-language-server 与 vtsls 随后稳定运行。

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/ohos-openeuler-agent/cmd-agent-server/src/spawn.rs` — `map_path` 增加 `--flag=<path>` 值映射；`spawn_direct` 映射后 cwd 不存在时自动 `create_dir_all`

参考：[[ohos-debug-lessons]]
