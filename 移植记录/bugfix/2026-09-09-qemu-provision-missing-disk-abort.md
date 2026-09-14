# QEMU 全新 provision 被 remove_file NotFound 误判中止：guest 静默回退 OHOS，terminal 连不上 guest

## 问题描述

QEMU guest 集成中，目标 terminal 连上 guest zcoderd。清空应用沙箱（或首次启动）后重启 zcoder，QEMU 应重建工作盘并启动 guest。实际表现：终端一直连本机 `/bin/sh`，cmd-client 报连接失败，QEMU 看似"没起来"。日志显示 provision 在 `Missing` 分支走到 remove_file 报 `NotFound` 后被当成致命错误 `return None`，QEMU **根本没启动**，静默回退 OHOS backend。

## 问题表现

- 清空 `/data/storage/el2/base/haps/entry/files` 后重启，日志：
  - `provision: working disk Missing; copying golden`
  - `provision: remove stale working disk failed: No such file or directory (os error 2)`
  - `provision_guest_files returned None; OHOS fallback`
  - `register_ohos_backend entered`（此后 cmd-client 连的是 `127.0.0.1:4023` = OHOS 本机端口，`Connection refused`）
- 终端 open 面板 probe 落到 `using the local /bin/sh`，永远连不上 QEMU。
- 此前若干次启动日志出现 `QEMU guest backend ACTIVE` 但 cmd-client 连 `4123` **timed out**——那些是旧工作盘恰好 `Valid (kept)` 绕开了恢复分支，掩盖了本条 bug（现象被误导到"网络不通"方向排查）。

## 问题原因

`launch-zed/src/qemu_runtime.rs` 的 `provision_guest_files` 恢复路径：

```rust
if !healthy {
    // Missing / Corrupt / SizeMismatch 都走到这里
    if let Err(err) = std::fs::remove_file(&disk) {   // disk 不存在 → NotFound
        boot_trace(...);
        log::error!(...);
        return None;    // ← Missing 分支必然在此返回，copy golden 永不执行
    }
    if let Err(err) = std::fs::copy(&golden, &disk) { ... }
}
```

工作盘判 `Missing` 时（全新 provision / 沙箱被清），`remove_file` 对一个**本就不存在**的文件报 `NotFound`，被当成致命错误返回 `None` → provision 中止、guest 永不启动。只有旧遗留盘存在且 `Valid` 时才走健康分支能启动，所以此前"看起来启动成功"实为侥幸绕开。

## 解决方案

`remove_file` 的 `NotFound` 即"无可删之物"（fresh provision 的正常状态），不应中止 provisioning。仅当错误非 `NotFound` 才视为致命返回；否则继续 copy golden。

```rust
if let Err(err) = std::fs::remove_file(&disk) {
    if err.kind() != std::io::ErrorKind::NotFound {
        boot_trace(&format!("provision: remove stale working disk failed: {err}"));
        log::error!("qemu_runtime: remove stale working disk: {err}");
        return None;
    }
}
```

修复后冷启动日志：`Missing → copying golden → restored working disk from golden → QEMU guest backend ACTIVE → cmd-client bootstrap 成功 → guest clock synced`，guest 真正启动。

**排查经验（端口判后端）**：cmd-client 报错端口可快速区分状态——
- 连 `4023`/`4022` refused = OHOS fallback（QEMU 未启动）→ 查 provision / QEMU 启动链
- 连 `4123` **timed out**（非 refused）= slirp hostfwd 在监听但 guest 侧网络不通（如 eth0 无 IP）→ 查 guest 网络
- `guest clock synced` 出现 = cmd-client → guest zcoderd 命令链路已通

## 修改文件

- `crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs` — provision 恢复路径容忍 `remove_file` 的 `NotFound`（Missing 盘无需删除），仅非 NotFound 错误才中止 provision

另见 [[2026-09-09-ohos-qcow2-disk-corruption-self-heal.md]]（qcow2 母盘机制）；与 [[ohos-debug-lessons]] 排查经验相关。
