# OHOS 适配点清点与垫片方案评估

清点 `crates/gpui_ohos/`、`hap/` 之外，全部侵入上游 Zed 代码的 OHOS 适配点，按原因分类，并评估"垫片（shim / LD_PRELOAD 符号劫持）"能否作为更集中、更抗上游升级的适配手段。

**范围不含两类**：`patches/`（第三方依赖的本地副本补丁，`[patch.crates-io]` 机制），以及已由 cmd-agent 统一接管的进程执行类适配。两者都与"仍需在上游代码里保留的适配点"不同，单独处理。

清点基于当前工作区代码（2026-09-16 首版；2026-09-17 按 `main.rs` 入口链收敛与鉴权清理后的状态修订），不依赖历史文档——历史文档写于 08-16，早于 cmd-agent / QEMU / hicodeerd 的引入，已被本文件取代。

---

## 0 结论摘要

**结论一：剩余适配点 80 个（逻辑口径），分布在 42 个上游文件、175 处 cfg 标记里。**

"剩余"指已剔除两类不必再想办法的部分：第三方依赖的 `patches/` 补丁、以及已由 cmd-agent 统一接管的进程执行类。另有 **10 个 Cargo.toml、22 个 `[target.*ohos*]` 段头**（`175` 是 `.rs` 口径，不含 Cargo.toml；两者合计 197 处标记、52 个文件）、`.cargo/config.toml` 两段 rustflags、`script/` 四项。

**结论二：按原因分八类，其中四类合计 56 处、占 7 成：平台后端接入（16）、构建系统（15）、功能无需支持（15）、文件沙箱与授权（10）。**

**结论三：垫片这条路，本项目已经走通并且是主力手段——但它只能覆盖"运行时函数行为"，覆盖不了另外两件事。**

已经落地的四套垫片样本（见第 4 节）：`musllib-shim.so`（libc TLS 符号劫持）、`shim.js`（Node 运行时 605 行补丁）、`ohos-meta-shim.c`（交叉工具链 EPERM 放行）、`launch-zed/src/lib.rs` 的 `#[no_mangle]` 符号覆盖（Rust cdylib 内顶掉 libc 缺失符号）。

**结论四：真正能降低"Zed 升级后二次适配成本"的动作，不是加垫片，而是（a）把"就地插入式"cfg 收敛成"接缝式"单点，（b）把不可收敛的侵入登记成可机械重放的清单。**

当前残余的 cfg 里，有很大一部分是"在每个调用点就近打补丁"，这是升级冲突的主要来源。项目里已经有正确样板（`audio.rs` 的模块替换、`crashes.rs` 的 `#[path]` 拆分），应该把其余散点按这个模式改造。

---

## 1 清点范围与方法

- **范围**：仓库根下全部文件，**排除** `crates/gpui_ohos/**`、`hap/**`、`patches/**`、`target/**`、`.git/**`。
- **方法**：`grep -rn 'target_env = "ohos"'`、`grep -rn 'ohos' --include=Cargo.toml`、`find -name '*_ohos*.rs'`，逐点读上下文判定原因类别。
- **计数口径**：以 `target_env = "ohos"` / `not(target_env = "ohos")` 的**出现次数**为"cfg 标记数"（机械可重复）；以"一个逻辑适配意图"为"适配点数"（跨文件互斥分支只算一处，例如非 OHOS 分支排除 + OHOS 分支新增算一处）。下文两个口径都给出。

---

## 2 清点结果

### 2.1 总量

- **42 个 .rs 文件，175 处 cfg 标记**（均在 `crates/` 下，不含 gpui_ohos）
  - 这是机械计数，含已由 cmd-agent 接管的进程执行类；剔除后的"剩余逻辑适配点"为 80 个，见第 3 节
  - **不含 Cargo.toml**：Cargo.toml 的标记是 `[target.'cfg(...)']` 段头，共 22 处，单列如下
- `crates/*/Cargo.toml` 含 ohos：**10 个文件、22 个 `[target.*ohos*]` 段头**（16 个 `not(ohos)` 排除段 + 6 个正向 ohos 段），段头行 22 处
  - `.rs` 与 Cargo.toml 合计 **52 个文件、197 处标记**
- 根 `Cargo.toml`：4 个 OHOS workspace 成员项、3 个 workspace 依赖项
- `.cargo/config.toml`：2 段 OHOS rustflags（`--cfg gles`、`+fp16`）
- `script/`：`bundle-ohos`、`clippy`、`ohos-tls-shim.c`、`read-sign-pwd.js` 四项
- 新增（非侵入）的 `*_ohos.rs` 文件：4 个
  - `crates/audio/src/audio_pipeline_ohos.rs`
  - `crates/terminal/src/ohos_shell.rs`
  - `crates/util/src/command/ohos.rs`
  - `crates/zlog/src/ohos.rs`

### 2.2 侵入密度 TOP（前 15，含已由 cmd-agent 接管的部分）

- `crates/terminal/src/terminal.rs` — 12
- `crates/settings_ui/src/settings_ui.rs` — 12
- `crates/livekit_client/src/lib.rs` — 11
- `crates/node_runtime/src/node_runtime.rs` — 10
- `crates/livekit_client/src/record.rs` — 10
- `crates/util/src/process.rs` — 9
- `crates/zed/src/main.rs` — 7
- `crates/util/src/command.rs` — 7
- `crates/agent_servers/src/acp.rs` — 7
- `crates/settings_content/src/settings_content.rs` — 6
- `crates/project/src/git_store.rs` — 5
- `crates/util/src/archive.rs` — 5
- `crates/onboarding/src/basics_page.rs` — 5
- `crates/zed/build.rs` — 4
- `crates/zlog/src/zlog.rs` — 4

密度最高的 6 个文件（`terminal.rs`…`process.rs`，合计 64 处）承担了近 1/3 的标记量，是升级冲突的第一现场。

注意：本表是**机械计数**（cfg 标记出现次数），不区分是否已接管。其中 `util/process.rs`、`util/command.rs` 及 `terminal.rs` 的一部分属于已由 cmd-agent 接管的进程执行类；`node_runtime.rs`、`languages/*` 中也含少量此类派生点。剔除这些后，密度最高的是 `settings_ui.rs`（12）、`livekit_client/{lib,record}.rs`（21）。`zed/src/main.rs` 经 2026-09-17 的入口链收敛已从 13 降到 7，其中 2 处是本次新增的条件编译壳（见第 3 节 F 类）。

---

## 3 按原因分类

**进程执行这一类（沙箱禁止 `fork`/`exec`，子进程必须转发到 VM）已由 cmd-agent 统一接管**，其路由出口收敛在 `crates/util/src/command/ohos.rs:373` 的 `Command::spawn` 一处——命中设备本地 HNP 工具（git/ssh/curl）就本地 fork，其余全部交 VM executor；`crates/util/src/process.rs:11-14` 再用 `pub use crate::command::{Child, Stdio}` 把上游对 `std::process` 的使用整体替换过来。属于已被机制消化的一类，不再计入下面的统计。

以下八类为**剩余**适配点，逻辑计数 80（175 处 cfg 标记按语义合并后的口径）。

### B 文件沙箱与授权（约 12 处）

判定：路径只能落沙箱 `filesDir`/`cacheDir`/`tempDir`；访问沙箱外目录需 picker 授权（且授权会随进程重启失效，需重新激活）；`symlink`/`hard_link` 被拒；可执行位语义差异。

代表点位：
- `crates/paths/src/paths.rs:196-199` — `temp_dir()` 返回 `data_dir().join("cache")`
- `crates/util/src/fs.rs:105` — OHOS 版 `make_file_executable` 只做本地 chmod，**不**路由命令守护（守护进程换了 uid，看不见调用方沙箱）
- `crates/util/src/archive.rs:159-315` — `unpack_tar_ohos` 自行遍历 tar，把被拒的 link 条目物化成真实拷贝（`materialize_link` / `copy_recursively`）
- `crates/node_runtime/src/node_runtime.rs:723`、`crates/http_client/src/github_download.rs:306` — 解包改走 `unpack_tar_ohos`
- `crates/node_runtime/src/node_runtime.rs:618` — `NPM_PATH` 直指 `lib/node_modules/npm/bin/npm-cli.js`（无符号链接，`bin/npm` 拷成普通文件后相对 require 断链）
- ~~`crates/zed/src/main.rs:254-278` — `resolve_home_directory` 调 `ohos_file_geturi::ensure_root_authorized` 复活 picker 授权~~ **已清理（2026-09-17），清除手法 7.1（整链外迁）叠加"外迁时删除该步"，详见第 7 节与附二清除项 4**：该函数连同其调用整体移入 `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`（路径含 ohos，不再是上游侵入点），且移出时**激活鉴权那一步被删除**，现只剩 `std::fs::create_dir_all`。同一条链上的固化鉴权（`write_home_directory_record` 把解析出的数据根写进 `<base_path>/custom_data_dir`）也随本批移入同一文件，功能保留、位置不再侵入上游
- ~~`crates/project/src/worktree_store.rs:38,941` — 打开 worktree 前 `ensure_root_authorized`~~ **已清理（2026-09-17），清除手法 7.4（依赖退场的消费者侧），详见附二清除项 5**：`create_local_worktree` 里的鉴权分支整体删除，`crates/project/` 的 cfg 标记归零（该文件已退出本表）
- **激活鉴权整体退场（清除手法 7.4 依赖退场，详见附二清除项 6）**：`crates/gpui_ohos/depend/ohos-file-geturi/` 整个 crate 已删除，`workspace`/`project`/`zed` 三处 `[target.'cfg(target_env = "ohos")'.dependencies]` 段同步移除。`ensure_root_authorized` 的定义仍在 `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs:101`（路径含 ohos，属新增而非侵入），但**已无任何上游消费者**
- `crates/project/src/git_store.rs:119,2626,2668,2689` — 额外 fs 句柄监听 `.git/config`（家目录 `.gitconfig` 在沙箱内 Permission denied）

### C 原生服务缺失（约 9 处）

判定：OHOS 无该能力的对应系统服务，且不是 libc 面能补的。

代表点位：
- `crates/zed/src/main.rs:167-202` — 启动失败通知：桌面走 `ashpd`（dbus）桌面通知，OHOS 直接 `process::exit(1)`
- `crates/title_bar/src/title_bar.rs:450-453` — 无系统菜单栏，强制自绘 `ApplicationMenu`
- `crates/terminal/src/terminal.rs:1734,1751,1773,2691` — `write_to_primary` / 中键 `read_from_primary` 排除（无 X11/Wayland 主选区）
- `crates/audio/src/audio_settings.rs:3`、`crates/livekit_client/src/lib.rs:3` — `DeviceId` 由 `cpal::DeviceId` 换成 `String`（无音频设备枚举）
- `crates/agent_servers/src/acp.rs:11,763,4746-4806`、`crates/agent_ui/src/conversation_view.rs:2030` — 无浏览器/回环回调通道，登录 URL 转成标准 elicitation 卡片兜底

### D libc / 内核接口差异（0 处）

判定：musl 变体 libc 的结构体布局、常量、内核允许列表与标准 Linux 不同。

**这一类在本清点范围内为 0**——libc 差异没有向上游 Zed 代码渗透，全部被挡在 `patches/`（第三方依赖本地副本）里，属另一个问题域。这是本项目做得好的一点。

唯一的边缘案例在 `crates/gpui_ohos/depend/launch-zed/src/lib.rs:12-25`：用 `#[unsafe(no_mangle)]` 导出 `pthread_mutexattr_setrobust` / `pthread_mutex_consistent` 的 no-op 实现，顶掉 libc 缺失符号——但路径含 ohos，属新增而非侵入。

### E 日志与崩溃通道（约 7 处）

判定：无 syslog/journald，需重定向 hilog；crash handler 无 socket 上报链路。

代表点位：
- `crates/zlog/src/zlog.rs:7-8,34-40,105-109` — `pub mod ohos`；OHOS 下 `log()` 直投 hilog 后 return，跳过 sink；默认级别 debug 版 Debug / release 版 Warn
- `crates/zlog/src/ohos.rs:35-80` — FFI 直连 `OH_LOG_Print`
- ~~`crates/zed/src/main.rs:375-395` — 跳过文件/stdout 日志初始化（`stdout_is_a_pty` 判断在 OHOS 无意义）~~ **已清理（2026-09-17），清除手法 7.3（先证"白做"再删冗余分支），详见附二清除项 3**：该分支已删除，`main.rs` 的 zlog 初始化恢复上游原样（pty 判定 + 文件日志 + stdout 兜底）。OHOS 下仍由 `zlog.rs:105-109` 的"直投 hilog 后 return"接管实际输出，文件初始化属白做一次、不构成故障
- `crates/crashes/src/crashes.rs:1-87` — 整个 `ohos` 模块顶替 `crashes_desktop.rs`；`crash_server` 只打一行"不可用"，`panic_hook` 直接 `abort()`
- `crates/lsp/src/lsp.rs:732` — LSP stderr 抬到 info 以便进 hilog（设备端无 stderr 可读通道）

### F 平台后端接入（约 18 处）

判定：GPUI `Platform` trait 挂 OHOS 实现、NAPI/ability 入口、单 XComponent 带来的窗口模型差异。**这一类不是"适配点"，是"平台接入本体"，不可消除。**

代表点位：
- `crates/gpui_platform/src/gpui_platform.rs:71-84` — `current_platform` OHOS 分支转 `gpui_ohos_linker::current_platform`；`:117-127` 新增 `vm_platform()`
- `crates/gpui/src/app.rs:237-241` — `Application::run` 末尾 `Box::leak`（OHOS run loop 由宿主驱动，`Platform::run` 立即返回）
- `crates/zed/src/lib.rs:1-2` — `include!("main.rs")`（把 bin 变成 lib 供 NAPI/ability 调用）
- `crates/zed/src/main.rs:210-218` — `pub fn main()` 的条件编译壳：OHOS 下 `#[cfg(target_env = "ohos")] pub fn main()`、其余平台 `#[cfg(not(target_env = "ohos"))] fn main()`，两者都转发到 `fn zed_main()`（原函数体逐字未动）。**这是 2026-09-17 新增的侵入点（2 处 cfg），也是 `main.rs` 现存 7 处标记中的 2 处**：可见性无法由 `cfg_attr` 改写，而 `launch-zed` 需要跨 crate 调 `zed::main()`（构造手法见 7.5，登记见附二"新增的侵入点 1"）
- ~~`crates/zed/src/main.rs:87-101` — OHOS 版 `build_application()` 强制 `Application::with_platform`~~ **已清理（2026-09-17），清除手法 7.2（运行期开关替掉编译期分支），详见附二清除项 1**：OHOS 版函数整段删除，`main.rs:87-94` 只留一份无 cfg 的上游实现；OHOS 由 `launch-zed` 事先置 `ZED_EXPERIMENTAL_A11Y=1` 走进 `Application::with_platform` 分支，行为与原 OHOS 版等价
- ~~`crates/zed/src/main.rs:217-281` — `start_zed_main` / `run_with_ability_entry` 入口链~~ **已清理（2026-09-17），清除手法 7.1（整链外迁），详见附二清除项 2**：`start_zed_main`（含数据根解析与固化）整体移入 `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`，`main.rs` 只留上面那条条件编译壳
- `crates/settings_ui/src/settings_ui.rs` 12 处 — 单窗口无法开第二个窗口，设置页改用 tab 形态（`open_settings_editor_in_tab`、`initialize_as_tab`、`mod ohos_settings_tab_impls`）；多处 `cx.defer` 规避同一 window lease 冲突
- `crates/settings_ui/src/page_data.rs:1551-1576` — 同上，打开 keymap 走 defer 且不 `remove_window`
- `crates/zed/build.rs:5-20,214-250` — 跳过 rpath 与 `prepare_app_icon_x11`，改调 `napi_build_ohos::setup()`

### G 构建系统（约 15 处，其中 Cargo.toml 涉及 12 个文件）

判定：Cargo.toml 平台依赖分段、build.rs 平台分支、target 专属 rustflags。是"手段"而非"问题"，但本身也是升级冲突面——上游改这些 Cargo.toml 时几乎必冲突。

Cargo.toml 分段共 **10 个文件、22 个段头**（逐文件段头数：`zed` 5、`livekit_client` 5、`audio` 3、`gpui_platform` 2、`settings_ui` 2、`agent_servers`/`call`/`crashes`/`util`/`title_bar` 各 1）：

- **排除类（`not(target_env = "ohos")`）16 段**：`call`/`title_bar` 的 `gpui(screen-capture)`；`crashes` 的 `async-process`/`crash-handler`/`minidumper`/`parking_lot`/`zstd`；`agent_servers` 的 `libc`/`nix`；`audio`/`livekit_client`/`settings_ui` 的 `cpal` 与 workspace `rodio`；`livekit_client` 的 `gpui(x11/wayland)`/`tokio`/`webrtc-sys`/`scap`；`zed` 的 `gpui(wayland/x11)`/`ashpd`/`image`/`pkg-config` 及 2 段 build-dependencies
- **新增类（正向 `target_env = "ohos"`）6 段**（2026-09-17 由 8 段降至 6 段）：`audio`/`livekit_client`/`settings_ui` 改用 rodio git rev（`default-features = false`）；`util` 加 `cmd-client` + `async-tar`；`gpui_platform` 加 `gpui_ohos_linker`；`zed` 的 ohos 段保留 `gpui` + `gpui_platform`。**原 `workspace`/`project`/`zed` 的 `ohos-file-geturi` 依赖段已随鉴权清理删除，`crates/workspace` 与 `crates/project` 已退出本表**
- **拆分 feature**：`zed` 把 `gpui_platform` 特性拆成 ohos 版（仅 `font-kit`）与非 ohos 版（`screen-capture + font-kit + wayland + x11`）
- **其他**：`crates/zed/build.rs` 4 处平台分支；`.cargo/config.toml` 两段 OHOS rustflags

### H 功能无需支持（约 15 处）

判定：仅把某平台专属或 OHOS 上不需要的功能排除，无需替代实现。

代表点位：
- `crates/zed/src/zed.rs:379-388,414` — `APP_ICON`（X11 用）不在 OHOS 编译
- `crates/zed/src/zed/app_menus.rs:94` — `Install CLI` 菜单项隐藏
- `crates/onboarding/src/basics_page.rs:26,477-541,732` — 隐藏 "Import Settings (VS Code / Cursor)"（4 处）
- `crates/call/src/call_impl/diagnostics.rs:307,462` — `compute_remote_audio_stats` / `extract_metrics` 走空实现（无 libwebrtc）
- `crates/livekit_client/src/{lib,record}.rs` — `livekit_client` / `mock_client` 不编译；`CaptureInput::finish()` 直接返回"不支持"
- `crates/audio/src/audio.rs:13` — `audio_pipeline_ohos as audio_pipeline` 模块替换
- `crates/audio/src/audio_pipeline/echo_canceller.rs` — `fake_implementation` 顶替 `real_implementation`

### I 其他（约 9 处）

- `crates/settings/src/settings.rs:137-140,151-163,165-172` + `settings_store.rs:923-927` — OHOS 专属默认设置层（`settings/default-ohos.json`、`keymaps/default-ohos.json`）
- `crates/settings_content/src/settings_content.rs:10-28,271-295` — `mod qemu` + `qemu_enabled`/`qemu_cpu_cores`/`qemu_mem_gb`/`qemu_disk_gb` 设置项
- `crates/node_runtime/src/node_runtime.rs:888` — `MIN_VERSION` 由 22 降到 18（VM 发行版自带 node 20）
- `crates/git_ui/src/git_panel.rs:6995` — Trust Directory tooltip 文案随平台切换
- `crates/zed/src/main.rs:9-15` — 二进制名与 `APP_NAME` 的 `const` 断言在 OHOS 关闭（lib 形态无 `CARGO_BIN_NAME`）

### 分类计数

- B 文件沙箱与授权 — 10
- C 原生服务缺失 — 9
- D libc/内核差异 — 0
- E 日志与崩溃通道 — 6
- F 平台后端接入 — 16
- G 构建系统 — 15
- H 功能无需支持 — 15
- I 其他 — 9

合计 80 个逻辑适配点（已排除由 cmd-agent 接管的进程模型一类）。较 2026-09-16 首版减少 5 处，全部来自 `main.rs` 入口链收敛与鉴权清理，逐条见第 3 节各分类里的「已清理（2026-09-17）」标记。

---

## 4 垫片方案评估

### 4.1 本项目已有的四套垫片（都是活的，不是设想）

**（1）libc 符号劫持 — `musllib-shim.so`**

- 源码 `crates/gpui_ohos/depend/cmd-agent/hicodeerd/shim/musllib-shim.c`（上游镜像 `script/ohos-tls-shim.c`）
- 做什么：OHOS libc 每进程只给 128 个 pthread key，而 `aarch64-unknown-linux-ohos` target spec 未启用原生 TLS（`tls-model: emulated`），Rust std 的 `thread_local!` 就落在 pthread key 上。垫片接管 `pthread_key_create`/`getspecific`/`setspecific`/`key_delete` 四个符号，发放 65536 个"虚拟 key"，用一把真实 key 锚定按线程的堆数组。
- 怎么加载：`hicodeerd/src/shim.rs:install()` 在 daemon 启动时设置 `LD_PRELOAD=<pkg>/shim/musllib-shim.so`，注入它 spawn 的每个子进程；`.so` 随 HNP 载荷进设备。
- **这是"劫持 libc 让失败调用正常返回"的完整落地样本，已验证（200/2000 个 thread-local 正常、4×200 Drop 恰好 800 次析构）。**

**（2）Node 运行时补丁 — `shim.js`（605 行）**

- 同目录 `shim.js`，经 `NODE_OPTIONS=--require` 注入
- 已修：`os.userInfo()` 在 uid 不在 `/etc/passwd` 时抛 `ERR_SYSTEM_ERROR`（OHOS 应用 uid 不在 passwd 里，`uv_os_get_passwd` 返回 ENOENT）
- **它的文件头写下了一条纪律，正是用户想要的"集中不分散"**：

> Keep this the ONLY preload file. Every host workaround belongs in here, in its own labelled section below -- do not add a second shim, and do not split these patches across files.

并要求每个补丁"先试真实实现，只在宿主确有该缺陷时才替代"，且必须用运行时探测确立必要性（平台名或能力探针），不能靠"这是我们正在测的宿主"来断言。

**（3）交叉工具链垫片 — `ohos-meta-shim.c`**

- 位于 `$TOOLCHAIN/tool/shim/`，放行 virtiofs 共享盘的 EPERM

**（4）Rust cdylib 内的符号覆盖 — `launch-zed/src/lib.rs:12-25`**

- `#[unsafe(no_mangle)] pub unsafe extern "C" fn pthread_mutexattr_setrobust(...) -> c_int { 0 }`
- 作用：OHOS libc 缺 robust mutex 符号，而 std 引用它们；在最终 cdylib 里导出同名符号，满足链接期未定义符号
- **这一条证明：不改 libc、不用 `LD_PRELOAD` 环境变量，直接在 Rust 产物里定义同名符号就能顶掉 libc 的实现。** 这是主进程内做垫片的最短路径。

### 4.2 垫片的能力边界：它只能改"运行时函数行为"

垫片（无论是 `LD_PRELOAD` 还是 cdylib 内 `no_mangle`）能做的事，是**在函数调用点改变行为**。有三件事它做不到：

- **改不了编译期的类型与内存布局。** OHOS libc 的 `cmsg_len` 是 u32 而非 usize、`msghdr` 的字段宽度、`IoctlRequest` 是 `c_int` 而非 `c_ulong`——这些在 Rust 编译时就要定死。垫片改的是运行时返回值，改不了 Rust 认为的结构体大小与字段偏移。这类差异只能靠条件编译在源码层解决。
- **改不了常量是否存在。** OHOS libc 未导出的常量（`XFS_SUPER_MAGIC`、`ST_RELATIME`、`O_FSYNC` 之类）在编译期就找不到符号。垫片无法凭空造出编译期常量——除非在 Rust 侧再定义一份，但那叫兼容层，不叫垫片。
- **改不了内核是否允许这个系统调用。** `openat2` 被 seccomp 允许列表挡住会直接 SIGSYS 杀进程，`mprotect(PROT_NONE→RW)` 返回 ENOMEM，`TIOCGPTPEER` 返回 EACCES——垫片可以**改调用路径**（先试、失败改走别的），但不能改内核的答复。

### 4.3 逐类"可垫片性"判定

（进程执行类已由 cmd-agent 接管，不在判定范围内；顺带一提，`execve` 虽是可劫持的 libc 符号，但劫持后无法"正常返回"——那等于把一次 `fork+exec` 改写成跨机器 RPC 并伪装成本地进程，属重写 `std::process` 语义，不是垫片能承担的事。）

- **B 文件沙箱（10）— 部分可垫片，收益有限。**
  `archive.rs` 的 `symlink`/`hard_link`（沙箱拒绝→改成拷贝）曾是最被看好的一处垫片面：`LD_PRELOAD` 拦 `symlink`/`link` 即可让 `unpack_tar_ohos`/`materialize_link`/`copy_recursively` 三段整体删除。**该方向已评估并否决（2026-09-17）：解压这一块保持现状不动，今后不再重复评估。** 否决理由是语义无法等价——垫片无从得知"解压已结束、可以收尾"，且前向 symlink（目标尚未出现）的拷贝会失败，而 `symlink(2)` 允许悬空目标是 POSIX 合法状态。其余 B 类都绑在编译期 `cfg!`（如 `paths.rs` 的 `temp_dir`），垫片够不到；OHOS 专有授权 API（`ensure_root_authorized`）已随 2026-09-17 的鉴权清理退场。
- **C 原生服务缺失（9）— 不可垫片。**
  沙箱内没有 dbus session bus、没有 X11 主选区、没有音频设备枚举、没有 crash socket。垫片造不出一个不存在的服务。剪贴板主选区（`terminal.rs` 4 处）稍特殊——剪贴板已被抽象成能力接口，补实现即可，但那是在 `gpui_ohos` 里补方法，不是垫片。
- **E 日志与崩溃通道（7）— 不可垫片，但可收敛。**
  hilog 重定向是"换个 sink"，不是"劫持符号"。`crashes.rs` 的降级是对 `crash_server` 的语义替换。
- **F 平台后端接入（18）— 不可垫片，且不该被视为适配点。**
  这是"把 GPUI 挂到 OHOS 上"，不是"绕开 OHOS 的限制"。评估垫片时应整体排除在目标之外。
- **G 构建系统（15）— 不是运行期问题，垫片无关。**
- **H 功能无需支持（15）— 无需垫片（本来就不需要实现）。**
- **I 其他（9）— 产品决策，垫片无关。**

### 4.4 关于"劫持 libc 让失败的调用正常返回"的整体判断

方向对，适用范围比预期窄，而且**本项目在它能覆盖的范围内已经用了**：

- 已经垫过：libc TLS key 上限（`musllib-shim.so`）、libc 缺失符号（`launch-zed` 的 `no_mangle`）、Node 运行时缺陷（`shim.js`）、工具链 EPERM（`ohos-meta-shim.c`）
- 曾认为可垫、但**已否决（2026-09-17）**：`archive.rs` 的 `symlink`/`hard_link`（1 处意图 / 3 个函数）。收益是删掉约 150 行 Rust 代码，但语义无法等价（前向链接 + 完工时刻不可知），**解压保持现状，不再重复评估**
- 垫不了的：C（9）、F（16）——合计 25 处，占剩余 80 个逻辑适配点的约 31%

**更关键的一点**：垫片本身不减少"与上游代码的耦合"这个问题。垫片的价值在于"把改动从 Rust 源码挪到独立的 `.c`/`.js` 文件"，从而让上游 Rust 代码保持干净。但残余的 cfg 里，真正能靠垫片消掉的上游 cfg 只有个位数。

---

## 5 建议（按性价比排序）

### 5.1 立即可做、零风险：清理纯残留侵入

这些是历史改造后剩下的死代码，清掉即可减少冲突面：

- `crates/workspace/src/workspace.rs:1931-1939` + `:10466-10477` — `mount_opened_dirs` 已是空实现（`Ok(())`），调用点整条可删
- `crates/zed/src/main.rs:199-202` — OHOS 的 `process::exit(1)` 分支重复出现在 linux 分支之后
- `crates/onboarding/src/basics_page.rs` 的 4 处 cfg — 合并为一次运行时判定

### 5.2 中收益：把"编译期 cfg"改成"运行时判定"

上游代码里 `cfg` 越少，升级时冲突越少。有些点本质是运行时决策，却写成了编译期分支：

- `crates/zlog/src/zlog.rs:34-40` — 默认日志级别随平台不同
- `crates/paths/src/paths.rs:196-199` — `temp_dir()` 的 `if cfg!(target_env = "ohos")`
- 这类改法的代价是失去编译期检查，需逐个权衡，不适合一刀切

### 5.3 高收益：把散点收敛成"接缝式"单点

项目里已经有正确样板，应推广到其余文件：

- **好样板**
  - `crates/audio/src/audio.rs:13` — `audio_pipeline_ohos as audio_pipeline` 模块替换，调用方一行不用改
  - `crates/crashes/src/crashes.rs` — `#[path = "crashes_desktop.rs"]` 把原文件整体搬走，只留一个分发器
- **应改造的散点**
  - `crates/settings_ui/src/settings_ui.rs`（12 处 F 类）— tab 化逻辑应收进一个 `ohos_settings_tab` 模块，主文件只留 1 处 `mod` + 少量转发
  - `crates/livekit_client/src/{lib,record}.rs`（21 处）— 已有 `mock_client` 概念，应做成"OHOS 整体替身模块"，参照 `audio_pipeline_ohos` 的做法
  - ~~`crates/zed/src/main.rs`（13 处，F/C 类为主）— 入口链可整体搬进一个 ohos 模块，`main.rs` 只留调用~~ **已做（2026-09-17）**：入口链（`start_zed_main` + 数据根解析 + 固化 + 鉴权）已整体搬进 `launch-zed`，`main.rs` 从 13 处降到 7 处，且剩下的 2 处是本轮新增的 `pub fn main()` 条件编译壳；其余 5 处（二进制名断言、ashpd/通知、单实例检查）因深埋 `fn main()` 控制流或依赖上游 `cfg` 互斥而搬不走，见第 6 节
  - `crates/onboarding/src/basics_page.rs`（5 处 H 类）— 同一条件的 4 处 cfg 合并为 1 次运行时判定

### 5.4 高收益：把不可收敛的侵入做成"可机械重放"的清单

这是直接回答"Zed 升级后二次适配工作量大"的动作。既然侵入无法全部消除，就让它**可重复施加**：

- 把剩余适配点逐个登记（文件、行、原因类别、上游版本、设计意图），形成机器可读清单；机械口径下就是那 175 处 cfg 标记
- 目前 42 个上游文件的改动是"手改"，升级时靠人肉比对。**建议改为按逻辑单元保存为有序补丁系列**（`git format-patch` 或登记表 + 重放脚本），使 zed 升级后能自动重放、只在真正冲突处人工介入
- 与 `移植记录/bugfix/` 的既有流水记录互补：那份记录的是"为什么这么改"，清单记录的是"改了哪一处"

### 5.5 若确实要新增垫片，遵循 `shim.js` 已立的纪律

- **一个垫片文件**，所有宿主 workaround 分节放进去，不新增第二个
- 每个补丁**先试真实实现**，只在宿主确有缺陷时才替代
- **必须用运行时探测**确立必要性（平台名或能力探针），不能靠"这是我们正在测的宿主"断言
- 补丁要写明它修的缺陷，以及**在正常 Linux 上为何是 no-op**

### 5.6 曾建议新增、现已否决的垫片面

`crates/util/src/archive.rs` 的 `unpack_tar_ohos` / `materialize_link` / `copy_recursively`（约 150 行）：曾建议改由 `musllib-shim.so` 中新增一节拦截 `symlink`/`link`/`linkat` 为实现拷贝。

**已评估并否决（2026-09-17）：解压这一块保持现状不动，今后不再重复评估。** 否决理由：垫片无从得知解压何时结束，且前向 symlink 的拷贝会失败，语义与 tar 原意不等价。

---

## 6 待验证的开放问题

- **主进程（`libhicodeer.so`）内的符号覆盖范围**：`launch-zed` 的 `no_mangle` 技巧已验证能补**缺失**符号。但"覆盖一个 libc **已有**的符号（如 `execve`）"是否同样生效，取决于 ELF 符号查找顺序与链接器参数（`-Bsymbolic` 等），**未验证**。若日后想用符号覆盖替代某些条件编译，必须先做这个验证。
- **`.cargo/config.toml` 两段 OHOS rustflags** 叠加是否造成 `--cfg gles` 重复声明（重复值本身无害，但注释声称的语义与实际不符），**未验证**。
- **`crates/zlog/src/ohos.rs` 混有中文注释**，与项目"代码注释一律英文"的规则冲突。

---

## 7 适配点清除手法（可复用）

第 2、3 节回答"还剩什么"，本节回答"**下一个怎么清、清完怎么证明清干净了**"。下面七个手法全部取自 2026-09-17 那批清理的实操，每个都给出适用场景、步骤、判据、陷阱与本项目实例，目的是让清侵入点这件事从个人经验变成可重复的流程。

### 7.1 手法一：整链外迁（把 OHOS 专属逻辑搬到路径含 `ohos` 的 crate）

**适用**：上游文件里成段的 OHOS 专属逻辑（一个函数或一整条调用链），且它需要的信息上游已经通过参数/入口提供。

**步骤**：

1. **先判依赖方向**。本项目只允许 `launch-zed → zed`，不能反向——`launch-zed` 提供不了 `zed` 内部要调用的东西。方向不允许的，直接归入 7.6 的"搬不走"。
2. 在 `crates/gpui_ohos/depend/launch-zed/` 下重建该函数，**连注释、日志一起搬**。注释解释的是"为什么这么写"，丢掉就等于丢掉经验。
3. 上游侧整段删除，**连包裹它的 `#[cfg(target_env = "ohos")]` 一起删**。只删函数体、留下 cfg 是最常见的半成品。
4. 补 `launch-zed/Cargo.toml` 依赖。本次补了 `paths`，因为外迁的代码要调 `paths::set_custom_data_dir`；`Cargo.lock` 随之更新。
5. 跨 crate 的调用点改名：外迁后的入口在自己的函数体末尾走 `zed::main()`。

**判据**：

- 上游文件的 `grep -n 'target_env'` 里不再出现被清那段的 cfg
- `grep -rn '<被外迁的函数名>' --include=*.rs .` 只在 `crates/gpui_ohos/**` 命中
- 编译通过（`script/bundle-ohos` 出 HAP）

**陷阱**：

- 外迁时顺手"简化"原逻辑，行为会悄悄漂移。本次是**逐字搬**（连注释里关于 QEMU 沙箱挂载的说明一起），只在外迁完成后**单独**做了一次删除（鉴权那一步）。
- 外迁后若只剩本 crate 调用，可见性要从 `pub` 降回私有，否则留下无意义的 `pub`。

**实例**：`start_zed_main` + `resolve_home_directory` + `write_home_directory_record` + `HOME_DIRECTORY_RECORD_FILE` 整链，从 `crates/zed/src/main.rs` 迁入 `launch-zed/src/launch_app.rs`。

### 7.2 手法二：用运行期开关替掉编译期分支

**适用**：上游**本来就有一份可配置的实现**，OHOS 只是需要选中另一支——即"OHOS 专属分支"与"上游某个既有分支"逻辑等价。

**步骤**：

1. 在上游代码里找现成的运行期开关。本次是 `ZED_EXPERIMENTAL_A11Y`。
2. 在 OHOS 侧入口、**且必须在上游读取该开关之前**把它设好（`launch_app.rs` 的 `std::env::set_var("ZED_EXPERIMENTAL_A11Y", "1")` 位于 `zed::main()` 调用之前）。
3. 删掉 OHOS 那份重复实现，上游只留一份。

**判据**：删之前把两份函数体逐支比对，确认语义等价。本次：原 OHOS 版是无条件 `Application::with_platform(platform)`；上游版在开关为 `"1"` 时走同一支，故等价。

**陷阱**：**时序**。开关必须在被读取之前设好，否则静默走到另一支——症状是"行为不对但不报错"，很难查。外迁后的入口（7.1 的产物）是设置这个开关的唯一合适位置。

**实例**：`main.rs` 的 OHOS 版 `build_application()` 删除。

### 7.3 手法三：删除冗余分支（先证"白做"，再删）

**适用**：该 OHOS 分支的存在理由已被**更底层的机制**覆盖，分支即使执行也不改变结果。

**步骤**：

1. **先找出真正接管该行为的机制**。本次是 `crates/zlog/src/zlog.rs:105-109`——OHOS 下每条日志直投 hilog 后立即 `return`，根本不经过 sink。
2. 论证被删分支即使执行也无害。本次：文件日志初始化做了也不会有人去读。
3. 删除分支，恢复上游原样。

**判据**：能**指名道姓**说出"谁在接管"。指不出接管者就不许删——那会造出"以为有兜底、其实没有"的空洞，比留着分支更危险。

**陷阱**：这类分支的注释常写着"在 OHOS 上无意义/会失败"，而**注释本身就是未经验证的历史断言**。本次删掉的那条写着"`stdout_is_a_pty` 判断在 OHOS 无意义"，但真正原因是输出已被 hilog 接管，与 pty 判断是否成立无关。删之前要重新验证注释的断言，不要被注释说服。

**实例**：`main.rs` 的 OHOS 日志初始化分支删除。

### 7.4 手法四：依赖退场（消费者优先，最后删 crate）

**适用**：某个 OHOS 专属 crate 已无存在必要。

**步骤（顺序是硬要求）**：

1. **先清空消费者**：本次是 `main.rs` 的鉴权调用与 `worktree_store.rs` 的鉴权分支。
2. 删各 `Cargo.toml` 的 `[target.'cfg(target_env = "ohos")'.dependencies]` 段（本次 3 处：`workspace`/`project`/`zed`）。
3. 删 crate 目录，并删根 `Cargo.toml` 的 `members` 项。
4. **清外围引用**——最容易漏的一步。本次漏了 `script/clippy` 的 `--exclude ohos-file-geturi`，事后单独补删。

**判据**：`grep -rn '<crate_name>' 与 '<crate_name_underscored>'` 在 `*.rs`/`*.toml`/`*.sh`/`script/*` 里零命中（`移植记录/`、记忆类文档不算）。

**陷阱**：

- **顺序颠倒会造成编译断裂窗口**。本次 `ohos-file-geturi` 的依赖在 23:14 就删了，消费者到 01:19 才走完，中间 OHOS 目标上 `crates/zed` 是编译不过的。若这两批是两次独立提交，中间那次提交是断的——**删依赖要么跟在删消费者之后，要么同批完成**。
- crate 名与目录名可能不同形（目录 `ohos-file-geturi`，Rust 标识符 `ohos_file_geturi`），两个都要搜。
- crate 删掉后，它的**实现可能仍在别处**。本次 `ensure_root_authorized` 的定义本来就在 `openharmony-ability/crates/ability/src/file_uri.rs:101`，`ohos-file-geturi` 只是包装层——这类残留要么一并删，要么作为"已无消费者的死代码"明确登记（见第 6 节）。

**实例**：`ohos-file-geturi` 整体退场。

### 7.5 手法五：可见性条件编译壳

**适用**：跨 crate 调用要求某个项的**可见性**与上游不同，而**可见性无法用 `cfg_attr` 改写**（Rust 的可见性是语法位置的一部分，不是 attribute）。这是本手法存在的唯一理由。

**步骤**：

1. 把原函数**只改名字**：`pub fn main() {` → `fn zed_main() {`。**函数体一行不动**，缩进也不动（仍在同一层级）。
2. 在其上方加两份薄壳，各自只做一次转发：

```rust
#[cfg(target_env = "ohos")]
pub fn main() {
    zed_main();
}

#[cfg(not(target_env = "ohos"))]
fn main() {
    zed_main();
}
```

**判据**：非 OHOS 下该入口的可见性与签名与上游一致；OHOS 下可被跨 crate 调用。

**代价（要如实登记）**：非 OHOS 侧比上游多一层转发。这是**为 OHOS 真实引入的侵入点**，不该假装它不存在——本次它已作为 2 处 cfg 登记进 F 类。

**陷阱**：不要试图用宏生成可见性（宏不能出现在可见性位置）；也不要为了"看起来优雅"把函数体拆成两份，那会让两份实现日后的漂移无人发现。

**实例**：`main.rs:210-218`。

### 7.6 手法六：判明"搬不走"，不做徒劳尝试

清点后最容易浪费时间的事，是反复尝试搬那些**结构上搬不走**的点。动手前先跑下面三条判定，命中任一条就停手并在文档里登记原因：

- **A 深埋 `fn main()` 控制流**：搬走它就得把 `main` 的流程切开，等于重写入口。本次的二进制名断言（`main.rs:9-15`）与单实例检查（`main.rs:370`、`:386-388`）属此类。
- **B 与上游 `cfg` 互斥耦合**：OHOS 分支的去留会影响**同一条 `all(...)` 里其他条件的取值**。本次 ashpd 那条写作 `cfg(all(any(target_os = "linux", target_os = "freebsd"), not(target_env = "ohos")))`——`not(ohos)` 的作用正是给 OHOS 排除 linux 分支，动它等于让 linux 分支在 OHOS 上被激活。
- **C 依赖方向不允许**：`launch-zed → zed` 单向，`launch-zed` 提供不了 `zed` 内部要调的东西。

**经验**：先查这三条，比"清完发现编译不过再回滚"省事得多。搬不走的点必须**写明原因**留在文档里，避免下一个人（或下一轮的自己）重复尝试。

### 7.7 手法七：把散点收敛成接缝（历史已验证的样板）

不属于"清除"，但同属降低侵入的手段，且本项目已有成功样板可直接照搬：

- **模块整体替换**：`crates/audio/src/audio.rs:13` — `audio_pipeline_ohos as audio_pipeline`，调用方一行不改
- **文件整体替换**：`crates/crashes/src/crashes.rs` — `#[path = "crashes_desktop.rs"]` 把原文件整个搬走，只留分发器
- **出口收敛到单点**：进程执行类由 cmd-agent 接管，路由出口收敛在 `crates/util/src/command/ohos.rs:373` 的 `Command::spawn` 一处
- **入口外迁**：`crates/zed/src/lib.rs` 原有的 NAPI launch 入口已搬到 `launch-zed`（见 `launch_app.rs:17` 的注释）

**共同点**：上游文件里只留"1 处分发 + 少量转发"，其余逻辑全在路径含 `ohos` 的地方。这与 7.1 是同一思路在不同粒度上的落实。

---

## 附：本次清点的可复现命令

```sh
# cfg 标记总量（排除 gpui_ohos / hap / patches / target）
grep -rn 'target_env = "ohos"' . --include=*.rs 2>/dev/null \
  | grep -v '^./target/' | grep -v '^./hap/' | grep -v '^./patches/' \
  | grep -v '^./crates/gpui_ohos/' | wc -l

# 逐文件密度
grep -rc 'target_env = "ohos"' . --include=*.rs 2>/dev/null \
  | grep -v '^./target/' | grep -v '^./hap/' | grep -v '^./patches/' \
  | grep -v '^./crates/gpui_ohos/' | grep -v ':0$' | sort -t: -k2 -rn

# 含 ohos 的 Cargo.toml
grep -rln 'ohos' --include=Cargo.toml crates 2>/dev/null | grep -v '^crates/gpui_ohos/'
```

注意：本仓库必须用 `grep` 命令搜索，ripgrep 类工具搜不全。

---

## 附二：2026-09-17 清除记录（main.rs 入口链收敛 + 鉴权清理）

本次只按代码现状修订第 2、3、4、5 节，未扩大清点范围。下面**逐条记录每个被清除的适配点及其清除方案**——手法编号对应第 7 节（"下次怎么复用"），本节是"这一批具体怎么做到的"。

### 清除项 1：OHOS 版 `build_application()`（原 `main.rs:87-101`，F 类）

- **手法**：7.2 用运行期开关替掉编译期分支
- **清除方案**：上游那份 `build_application` 本来就带运行期开关 `if std::env::var("ZED_EXPERIMENTAL_A11Y").as_deref() == Ok("1")`，OHOS 需要的正是 `Application::with_platform` 那一支。于是把"置开关"这件事移到 OHOS 侧入口——`launch-zed/src/launch_app.rs` 的 `start_zed_main` 在调用 `zed::main()` **之前**执行 `std::env::set_var("ZED_EXPERIMENTAL_A11Y", "1")`——然后把 OHOS 版函数整段删除，同时删掉它上方的 `#[cfg(not(target_env = "ohos"))]` 包装。
- **判据**：两份函数体逐支比对等价（OHOS 版是无条件 `with_platform`；上游版在开关为 `"1"` 时走同一支，而 OHOS 上该开关恒为 `"1"`）；非 OHOS 侧 `main.rs:87-94` 与上游逐字一致。
- **同步改动**：无。`Cargo.toml` 无需变动。

### 清除项 2：`start_zed_main` 入口链（原 `main.rs:217-281`，F 类）

- **手法**：7.1 整链外迁
- **清除方案**：把 `start_zed_main` / `resolve_home_directory` / `write_home_directory_record` / `HOME_DIRECTORY_RECORD_FILE` 四项**逐字**搬入 `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`（含原注释与日志文案），上游侧整段删除。依赖方向满足 `launch-zed → zed`，外迁后的函数末尾通过 `zed::main()` 进入上游入口。
- **判据**：`main.rs` 的 `grep -n 'target_env'` 里不再有这几段的 cfg（由 13 处降至 7 处）；四个标识符只在 `crates/gpui_ohos/**` 命中；`script/bundle-ohos` 通过。
- **同步改动**：`launch-zed/Cargo.toml` 新增 `paths = { path = "../../../paths" }`（外迁代码要调 `paths::set_custom_data_dir`），`Cargo.lock` 随之更新。
- **搬迁中的取舍**：外迁后函数在本 crate 内被调用，可见性从 `pub` 降回私有；注释一并带走，不留在上游。

### 清除项 3：OHOS 日志初始化分支（原 `main.rs:375-395`，E 类）

- **手法**：7.3 删除冗余分支（先证"白做"）
- **清除方案**：先定位真正接管日志输出的机制——`crates/zlog/src/zlog.rs:105-109`，OHOS 下每条记录直投 hilog 后立即 `return`，根本不经过 sink，因此 `main.rs` 里跳过"文件/stdout 输出初始化"这一分支**即使执行也不改变结果**。据此删除 OHOS 分支与其 `#[cfg(not(target_env = "ohos"))]` 包装，恢复上游原样（pty 判定 + 文件日志 + stdout 兜底）。
- **判据**：能指名接管者（`zlog.rs` 的直投 hilog 路径）；OHOS 上文件初始化属"白做一次"，不构成故障。
- **注意**：被删注释原写"`stdout_is_a_pty` 判断在 OHOS 无意义"，但真正原因是输出已被 hilog 接管，与该判断是否成立无关——**删前重新验证了注释的断言**，没有被注释说服。
- **同步改动**：无。

### 清除项 4：激活鉴权（`main.rs` 那处，原 `main.rs:254-278`，B 类）

- **手法**：7.1 整链外迁 + 外迁时删除该步
- **清除方案**：`resolve_home_directory` 随整链外迁，但**外迁的同时删掉了 `ohos_file_geturi::ensure_root_authorized` 调用**，外迁后的实现只剩 `std::fs::create_dir_all(home_directory)`。这是"外迁"与"删除"叠加：位置搬到 ohos 侧，行为本身也不再需要。
- **判据**：`main.rs` 内 `ohos_file_geturi` 零引用；上游该处 cfg 消失。
- **同步改动**：见清除项 6（crate 退场）。

### 清除项 5：激活鉴权（`worktree_store.rs` 那处，原 `:38,:941`，B 类）

- **手法**：直接删除（无需外迁）
- **清除方案**：`crates/project/src/worktree_store.rs` 的 `create_local_worktree` 里那段"打开 worktree 前对根路径调 `ensure_root_authorized`、并在路径变化时重建 `SanitizedPath`"的 OHOS 分支整体删除，同时删掉文件顶部 `#[cfg(target_env = "ohos")] use ohos_file_geturi;` 引用。该文件 cfg 归零，退出本表。
- **判据**：`worktree_store.rs` 的 `grep -n 'target_env'` 零命中；`crates/project/` 退出密度表。
- **同步改动**：见清除项 6。

### 清除项 6：`ohos-file-geturi` crate 整体退场（G 类）

- **手法**：7.4 依赖退场（消费者优先，最后删 crate）
- **清除方案**：按"先清消费者 → 再删依赖段 → 最后删 crate"的顺序执行。消费者两处见清除项 4、5；依赖段三处（`crates/workspace`、`crates/project`、`crates/zed` 的 `[target.'cfg(target_env = "ohos")'.dependencies]`）；随后删 crate 目录与根 `Cargo.toml` 的 `members` 项（同时新增 `crates/gpui_ohos/depend/ohos-libc-shim`）。
- **判据**：`grep -rn 'ohos_file_geturi\|ohos-file-geturi'` 在 `*.rs`/`*.toml`/`*.sh`/`script/*` 里零命中。
- **教训（顺序）**：本次依赖段在 23:14 删除、消费者到 01:19 才走完，**中间窗口 OHOS 目标上 `crates/zed` 编译不过**。若这是两次独立提交，中间那次是断的——删依赖必须跟在删消费者之后或同批完成。
- **教训（外围引用）**：`script/clippy` 的 `--exclude ohos-file-geturi` 是本次漏掉的一处外围引用，事后单独补删；crate 名与目录名不同形（`ohos-file-geturi` / `ohos_file_geturi`），两个都要搜。
- **残留登记**：`ensure_root_authorized` 的定义仍在 `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs:101`（路径含 ohos，属新增而非侵入），`ohos-file-geturi` 只是它的包装层。**该定义现已无任何上游消费者**，属可删的死代码，已在第 6 节登记。

### 清除项 7：固化鉴权移出上游（原 `main.rs` 内，B 类）

- **手法**：7.1 整链外迁（功能保留）
- **清除方案**：写 `<base_path>/custom_data_dir` 记录的逻辑 `write_home_directory_record` 随清除项 2 同批搬入 `launch-zed/src/launch_app.rs`，**功能完整保留**（ets 侧 `Setup.ets` 仍读同一个记录文件，两侧约定不变），只是位置不再落在上游文件里。
- **判据**：`main.rs` 内 `HOME_DIRECTORY_RECORD_FILE`、`write_home_directory_record` 零命中；`launch-zed` 内两者均在。
- **与清除项 4 的差别**：同样是"搬"，鉴权那处搬完还删了行为，固化鉴权这处搬完行为完全不变——**外迁不等于删除，两者要分开判断**。

### 新增的侵入点 1：`pub fn main()` 条件编译壳（`main.rs:210-218`，F 类）

- **手法**：7.5 可见性条件编译壳
- **为何不能省**：`launch-zed` 需跨 crate 调用 `zed::main()`，而 OHOS 下 `main.rs` 是通过 `crates/zed/src/lib.rs` 的 `include!("main.rs")` 进 lib target 的，必须是 `pub`；非 OHOS 下它只作 `[[bin]]` 编译，上游形态是非 `pub`。Rust 的可见性无法由 `cfg_attr` 改写，只能按平台一分二。
- **具体做法**：原 `pub fn main() {` 一行改为 `fn zed_main() {`（**函数体一行未动、缩进未动**），其上方插入两份薄壳，各自只做 `zed_main();` 转发。
- **代价（如实登记）**：非 OHOS 侧比上游多一层转发函数，是为 OHOS 真实引入的侵入点，即 `main.rs` 现存 7 处 cfg 标记中的 2 处。
- **验证**：`script/bundle-ohos` 通过、HAP 正常产出，且构建日志中 `zed` crate 无新增 warning/error。

### 解压垫片方向结案

`archive.rs` 的 `symlink`/`hard_link` 垫片方案（原 5.6 节"唯一建议新增的垫片面"）**已否决**：解压保持现状，今后不再重复评估。理由见 4.3 / 4.4 / 5.6。

### 计数变动

- 机械口径：43 个文件 / 183 处 → **42 个文件 / 175 处**
- Cargo.toml：12 个文件 / 24 段头 → **10 个文件 / 22 段头**（正向 ohos 段 8 → 6）
- 逻辑口径：85 → **80**（B 12→10、E 7→6、F 18→16）

### 未验证项

`main.rs` 新加的条件编译壳与 `launch-zed` 的 `zed::main()` 调用链，**只在 OHOS 目标上编译验证过**（`script/bundle-ohos` 通过、HAP 正常产出）；非 OHOS 目标的编译未做实测——本机 `rustup target list --installed` 只有 `aarch64-unknown-linux-ohos` 一个 target。

### 本批清除项 → 手法索引

- 清除项 1 `build_application` → 手法 7.2（运行期开关）
- 清除项 2 `start_zed_main` 入口链 → 手法 7.1（整链外迁）
- 清除项 3 OHOS 日志初始化分支 → 手法 7.3（删冗余分支）
- 清除项 4 激活鉴权（`main.rs` 那处）→ 手法 7.1 + 外迁时删除该步
- 清除项 5 激活鉴权（`worktree_store.rs` 那处）→ 直接删除，无需外迁
- 清除项 6 `ohos-file-geturi` 退场 → 手法 7.4（依赖退场）
- 清除项 7 固化鉴权外迁 → 手法 7.1（仅搬位置，功能不动）
- 新增侵入点 1 `pub fn main()` 壳 → 手法 7.5（可见性壳）

### 以后遇到新适配点时的判定顺序

1. **先跑 7.6 的三条判定**（深埋 `fn main()` 控制流 / 与上游 `cfg` 互斥耦合 / 依赖方向不允许）。命中任一条就停手并写明原因，别硬搬——这是省时间最多的前置动作。
2. **再看能否改成运行期开关（7.2）**。成本最低，不新增侵入点，但要注意开关必须在上游读取之前设好。
3. **再看该分支是否已被更底层机制覆盖（7.3）**。删除的前提是能**指名**接管者，指不出就不许删。
4. **否则走整链外迁（7.1）**。逐字搬（含注释日志）、cfg 一起删、先确认依赖方向、外迁后可见性降回私有、`Cargo.toml` 补依赖。
5. **跨 crate 可见性差异用 7.5 处理**，并把因此新增的侵入点**如实登记**（不要假装非 OHOS 侧没变化）。
6. **伴生 crate 的退场按 7.4 的顺序走**：消费者 → 依赖段 → crate 目录 + members 项 → 外围引用（`script/` 最容易漏，`ohos-file-geturi` 就是这么漏的）。
7. **收尾给出判据**：上游文件对应 cfg 归零、被移走的符号只在 `crates/gpui_ohos/**` 命中、`script/bundle-ohos` 通过。非 OHOS 目标若本机没有 target 可编，要如实标注"未实测"，不要用"逻辑上等价"代替验证结论。
