# OHOS Terminal panel 本地终端打通：pty 打开(EACCES) + shell=/bin/sh + 进程 env 本机化

## 问题描述

zcoder 在 HarmonyOS PC/2in1 打开 **Terminal panel** 报 `Failed to spawn terminal`，无法进入本地 `/bin/sh` 交互终端。问题分三层：pty 分配被沙箱拦、shell 被解析成 OpenEuler VM 的 `/bin/bash`、终端子进程环境被远端 VM login 环境整体污染。本记录只讲"本地 pty 终端适配"这一维度；终端里 backspace/delete/enter 无效属 IME 输入层问题，见另篇 `2026-09-03-ohos-ime-editing-keys-forwarding.md`。

## 问题表现

- **打开 Terminal panel**：`Failed to spawn terminal; Working directory: /storage/Users/currentUser/workspace/warp-oh; Shell command: <system defined shell>; IOError: Permission denied (os error 13)`。注意报错**没有** "Failed to spawn command '…'" 前缀 → 失败发生在 spawn(exec) 之前的 pty 分配阶段。
- 修好 pty 后：`Failed to spawn command '/bin/bash': No such file or directory (os error 2)` —— exec 的是 `/bin/bash`（设备上不存在；终端只放行 `/bin/sh`）。
- 终端能起 `/bin/sh` 后，环境全是 VM 的：`$HOME=/home/user`、`$PATH=/home/user/.nvm/...:/home/user/.cargo/bin:...`、`$SHELL=bash`；设备本地 shell 找不到 `ls` 等命令。
- 同样的裸 libc probe（`posix_openpt`+`fork`+`exec /bin/sh`）在 cmd-agent daemon 路径**成功**——出现"probe 通、面板不通"的矛盾。

## 问题原因

### 1. pty 打不开 → TIOCGPTPEER 被沙箱拒（EACCES 13）
alacritty 的 unix pty 打开走 `rustix_openpty::openpty()`。其 linux 分支 `open_user()`（rustix-openpty 0.2.0 `src/lib.rs`）**先尝试 `TIOCGPTPEER` ioctl 直开 slave**，失败只容忍 `EPERM/ENOSYS` 才回退 `ptsname+openat`；**其它 errno（含 EACCES）直接冒泡**。OHOS 沙箱对 TIOCGPTPEER 返回 `EACCES(13)` → openpty 整体失败 → 面板报裸 `Permission denied (os error 13)` 且无 spawn 前缀。
裸 libc probe 用 `posix_openpt + ptsname_r + open("/dev/pts/N")`，**从不调 TIOCGPTPEER**，故成功。这与 OHOS seccomp 拦新式 syscall（如 `openat2`）同族：库内的"新内核优化路径"在 OHOS 撞墙。

### 2. shell 解析成 /bin/bash
- `TerminalSettings.shell` 默认 `Shell::System` → zc `TerminalBuilder::new` 在非 windows 把 shell_params 置 `None` → alacritty `Options.shell=None` → 走 alacritty `ShellUser::from_env()`，读**进程 env `$SHELL`**（无则查 `/etc/passwd`）。
- zc 启动时 `zed/src/main.rs` 在 `!stdout_is_a_pty()` 分支调 `util::load_login_shell_environment()`：它用 `shell_env::capture` 经 cmd-agent **在 OpenEuler VM 上**跑 `/bin/sh -l -c env` 捕获 login 环境，把 VM 的 `SHELL=/bin/bash`、`HOME=/home/user`、VM 的 PATH **逐个 `set_var` 写进设备 zc 进程**。
- 于是 `from_env` 读到 `/bin/bash` → exec 失败。`/etc/passwd` 侧也无救：设备 passwd **无 app uid 条目**，且所有现有条目 `pw_shell` 一律 `/bin/false`。

### 3. 设备进程环境被 VM login 环境整体污染
`load_login_shell_environment()` 与开终端时的 `resolve_directory_environment()`（给 `TerminalBuilder` 提供 env）**共用同一个 `shell_env::capture_ohos()`**，都经 executor 在 VM 捕获 → 终端 `/bin/sh` 子进程继承 VM 的 HOME/PATH/SHELL，设备本地不可用。

## 解决方案

三组修复，全部只影响 OHOS（`cfg(target_env="ohos")` 或仅 OHOS 路径），linux/macOS/windows 行为不变：

### 1. pty：patch rustix-openpty 跳过 TIOCGPTPEER
仿 zc 既有 `cap-primitives`/`nix`/`wgpu` 先例，把 rustix-openpty 0.2.0 patch 成本地 `patches/rustix-openpty/`，`open_user` 的 TIOCGPTPEER 优化分支改 `#[cfg(not(any(target_os = "android", target_env = "ohos")))]`，OHOS 直接走 `ptsname + openat("/dev/pts/N")`（与裸 libc probe 一致、已验证成功的路径）。根 Cargo.toml `[patch.crates-io]` 加 `rustix-openpty = { path = "patches/rustix-openpty" }`。

### 2. shell：固定 /bin/sh，绕开 from_env 的进程 SHELL/passwd
`crates/terminal/src/terminal.rs` `TerminalBuilder::new` 里 `Shell::System` 分支加 `else if cfg!(target_env = "ohos")` → 解析成 `ShellParams::new("/bin/sh")`（即 `Options.shell=Some(/bin/sh)`，直接 `execve(/bin/sh)`），并把 child env 的 `SHELL` 覆盖为 `/bin/sh`。

### 3. env：capture 本机化 + 启动环境固化
- `crates/util/src/shell_env.rs`：`capture_ohos` 不再去 VM 跑 `/bin/sh -l -c env`，直接返回**本机进程 env**（`std::env::vars()`）——两个上游调用者（load_login_shell_environment 的进程注入、resolve_directory_environment 的终端 env）一起落到设备环境。
- `launch-zed/src/launch_app.rs` 新增 `ensure_terminal_shell_env()`（launch_app 早期调用）：
  - `SHELL=/bin/sh`（写死，适配 exec 白名单）；`USER` 缺省补 `app`；`HOME` 无条件 = `app.base_path()`（hap el2 files，设备私有可写）；
  - `PATH` 尾部追加 el1 resfile（`application_resource_dir("entry")`）与 el2（base_path）及 `…/curl`；`LD_LIBRARY_PATH` 前缀 `…/curl`（curl 的动态库同目录）。

## 修改文件

- `Cargo.toml` — `[patch.crates-io]` 增 `rustix-openpty = { path = "patches/rustix-openpty" }`。
- `patches/rustix-openpty/src/lib.rs` — `open_user` 的 TIOCGPTPEER 优化分支加 `not(target_env = "ohos")`，OHOS 走 ptsname+openat。
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — 新增 `ensure_terminal_shell_env`：SHELL=/bin/sh、USER/HOME(el2 files)、PATH(el1+el2+curl)、LD_LIBRARY_PATH(curl)。
- `crates/terminal/src/terminal.rs` — `Shell::System` 在 ohos 显式 `/bin/sh`；child env `SHELL=/bin/sh`。
- `crates/util/src/shell_env.rs` — `capture_ohos` 改为返回本机进程 env（不再 VM 捕获）。

## 附：可复用经验

- **"probe 通、库调用不通" → 优先怀疑库内的"新内核优化路径"**：TIOCGPTPEER 与 openat2 同族，OHOS seccomp/沙箱对这类新式 ioctl/syscall 返回特定 errno（EACCES/EPERM），而第三方库往往只对少数 errno 才 fallback。定位时把 probe 与库调用逐字节对齐即可命中。
- **设备 `/etc/passwd` 无 app uid 条目、且 pw_shell 全为 `/bin/false`** → alacritty 的 passwd fallback 在 OHOS 不可依赖；shell 只能走进程 `$SHELL` 或显式 `Options.shell`。
- **Zed 的 login-env 捕获会污染设备进程 env**：OHOS 本地子进程（终端 /bin/sh）需要的是设备环境，绝不能把远端 VM login env 灌进来。

[[ohos-debug-lessons]]
