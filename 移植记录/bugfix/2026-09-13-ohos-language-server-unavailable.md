# OHOS 上语言服务不可用：两条独立成因链（rust-analyzer 五层叠加 / TS Server 平台名）

> 本篇合并了原三份报告：`2026-09-12-rust-lsp-toolchain-path.md`（PATH 覆盖）、
> `2026-09-13-rust-lsp-startup-failure.md`（四层叠加）、
> `2026-09-13-ohos-tsserver-unsupported-platform.md`（TS Server 平台名）。
> 内容已全部收入本篇，原文件不再单独存在。行文只给**最终状态**，不保留中间试改的反复。
>
> 两条链**互不相关** —— 不同语言、不同进程、不同根因 —— 合在一篇只因它们共用一个排查入口
> 「OHOS 上语言服务起不来」，也共用同一条支撑通路「应用 → 守护进程 → 子进程 + 握手下发」。

## 问题描述

zcoder（Zed 的 OpenHarmony 移植版）在 OHOS 设备上语言服务大面积不可用。两个方向各有独立故障，
需要分开看。

### A. Rust 项目：rust-analyzer 不可用（五层叠加）

打开以 Rust 为工作区的项目时，Rust 语言服务不可用：没有跳转、没有补全、没有诊断。
过程中还伴随 `cargo metadata` 联网失败降级、临时文件乱落家目录等现象。

这**不是单一 bug，而是五层原因叠加**，任何一层没通，LSP 都到不了可用状态：

| 层 | 症状 | 根因 |
|---|---|---|
| ① 二进制不存在 | Zed 找不到 rust-analyzer | 1.97.1 工具链未装 `rust-analyzer` 组件 |
| ② 二进制不能执行 | `Permission denied (os error 13)` | 新落盘 ELF 无 `.codesign` 段，OHOS 内核拒绝 exec |
| ③ 环境串味 | `SSL error` / `Operation timed out` / `os error 2` | 调用方的沙箱内环境变量被转发给守护进程 |
| ④ 依赖缓存不全 | `cargo metadata` 报缺包并降级 | registry 缓存缺 316 个 crate |
| ⑤ 临时文件乱落 | 家目录攒一堆 `proc-macro-srv*` | `/tmp` 只读，且该工具链的 `temp_dir()` 只认 `TMPDIR` |

### B. TypeScript / JavaScript 项目：TS Server 启动即崩（单点）

打开以 TypeScript / JavaScript 为工作区的项目时，TS Server 起不来：TypeScript 语言服务在启动瞬间
自行退出，编辑器里没有补全、没有诊断、没有跳转，日志只留一行 `TSServer exited. Code: 1. Signal: null`。

与「找不到 tsserver」不同 —— **tsserver 找到了、也拉起来了，但它一读到平台名就主动自杀**。
问题不在 vtsls、也不在 zcoder 的 LSP 管理，而在 TypeScript 本体对 `process.platform` 的判断，
以及 Node 在 OHOS 上报告的那个平台名。根因是**单点**。

### 两部分对照

| | A. rust-analyzer | B. TS Server |
|---|---|---|
| 语言 / 进程 | Rust；`rust-analyzer` | TypeScript·JavaScript；`tsserver`（由 vtsls fork） |
| 引入方 | Zed 原生 Rust 适配器 | `@vtsls/language-server`（HiCodeer 的 vtsls 语言包） |
| 成因形态 | **五层叠加**，缺一不可 | **单点**，一处 `Debug.fail` |
| 病灶位置 | 宿主与工具链之间的五处落差 | 工具对平台名的白名单 |
| 共同通路 | 都走「应用 → 守护进程 → 子进程」；数据根 / 垫片均经握手送达 | ← 同左 |

## 问题表现

### A. rust-analyzer

**A① 语言服务压根不启动**

- 打开 Rust 项目后无任何 LSP 活动，设备 `ps -ef` 里没有 `rust-analyzer` 进程。
- 手工对照验证：命令行里 `rust-analyzer --help` 正常（同一台设备、同一个 PATH）。

**A② 组件装上后改为「起不来」而非「找不到」**

```
Permission denied (os error 13)
```

前后只差一件事：`rust-analyzer` 文件是否带 `.codesign` 段。

**A③ 环境串味有两种形态，都出现在「应用 → 守护进程 → 远端命令」这条链路上（纯命令行无此问题）**

形态一：证书路径指错 —— 语言服务启动后联网即崩，Zed 只能降级：

```
WARN `cargo metadata` failed and returning succeeded result with `--no-deps`
warning: spurious network error: SSL error: syscall failure: ; class=Os (2)
error: failed to load source for dependency `async-process`
Caused by: unable to open https://github.com/zed-industries/async-process.git?rev=0b6d671…
```

隔离实验（在守护进程侧分别注入/不注入调用方环境）复现出证书错误的真身：

```
the SSL certificate is invalid; class=Ssl (16)
```

形态二：PATH 被覆盖 —— rust-analyzer 自身 stderr（它自己的 logger 格式）：

```
ERROR failed fetching cargo workspace root e=… "cargo" "locate-project" --workspace … failed
ERROR Failed to load workspace: … No such file or directory (os error 2)
```

判据：四条全是 `os error 2`，且**没有任何一句 cargo 自身的输出** ⇒ 失败发生在 **exec 阶段**，
不是 cargo 跑起来之后报错。形态二与形态一同源：都是「应用环境被当成远端命令的环境」。

**A④ 缓存缺口精确可指认**

完整版 `cargo metadata` 一上来就要 `accesskit_consumer-0.37.0`，而设备侧只有 0.35.0
（`Cargo.lock` 里两版本共存）⇒ 必然报缺包 → Zed 退到 `--no-deps`。

**A⑤ 临时文件乱落**

每次语言服务会话在家目录 `~` 下留 2~4 个 `proc-macro-srv<hex>-<n>/`（空目录）与
`rust-analyzer<hex>-<n>/`（各含一个 550 KB 的 `Cargo.lock`）。被强杀或异常退出时**不会被回收**，
攒成硬残留。

### B. TS Server

**B① TS Server 立刻退出（报错落在编辑器日志里）**

```
Starting TS Server
Using tsserver from: /storage/Users/currentUser/HiCodeer/languages/vtsls/node_modules/typescript/lib/tsserver.js
<syntax> Falling back to legacy node.js based file watching because of user settings.
<syntax> Forking...
<syntax> Starting...
<semantic> Falling back to legacy node.js based file watching because of user settings.
<semantic> Forking...
<semantic> Starting...
TSServer exited. Code: 1. Signal: null
```

退出码 1、无信号、无栈 —— 编辑器侧只拿到「进程没了」这一条信息，看不出原因。

**B② 手动跑同一份 tsserver，真实异常现身**

编辑器不转发子进程 stderr，所以直接手工拉起同一个文件（**这一步是整个排查的转折点**）：

```sh
node /storage/Users/currentUser/HiCodeer/languages/vtsls/node_modules/typescript/lib/tsserver.js --stdio < /dev/null
```

```
Error: Debug Failure. unsupported platform 'openharmony'
    at getGlobalTypingsCacheLocation (/storage/Users/currentUser/HiCodeer/languages/vtsls/node_modules/typescript/lib/_tsserver.js:561:41)
```

`Debug Failure. unsupported platform` 是 TypeScript 内部 `Debug.fail()` 的固定话术 —— 它**不是运行时错误，
而是「这个分支我不接受」的主动中止**。

**B③ 干扰项**

日志里那两行 `<syntax>/<semantic> Falling back to legacy node.js based file watching because of user settings.`
来自用户设置开启的文件监听回退，**与崩溃无关**，但极易被误当成病因（见「走错的弯路」）。

**B④ 复现条件**

- 环境：OHOS 设备（本例 HarmonyOS 6.1），Node v24.13.0，TypeScript 5.9.3
- 触发：拉起任何用到 `getGlobalTypingsCacheLocation()` 的 TS Server（vtsls 内部 fork 的 tsserver 首当其冲）
- 频率：**必现**，与项目内容无关

## 问题原因

### A. rust-analyzer：五层叠加

**层①：Zed 只认 `rustup which rust-analyzer` 的返回值，而该组件没装**

`crates/languages/src/rust.rs:203-240` 的查找协议是写死的：

```rust
let rustup = delegate.which("rustup").await?;   // 只找 rustup
    .args(["which", "rust-analyzer"])           // 向 rustup 要路径
```

前置条件是 worktree 里有 `rust-toolchain.toml`（本项目有，钉 `1.97.1`）。安装工具链时用的
`profile="minimal"`，**不含 `rust-analyzer` 组件** ⇒ `rustup which rust-analyzer` 失败 ⇒ Zed 返回 None，
语言服务不会被启动。

⚠️ **更正**：旧记录称「这里没有 fallback 到 `which rust-analyzer`」—— **不准确**。当前代码
`rust.rs:781` 确有 PATH 兜底，但那条路要求目标通过 `--help` 探针（`rust.rs:786-800`），而 brew 里
那个独立 formula 装的 RA 过不了探针，于是最终仍走「查缓存 → 联网下载」超时。两条路都返回 None 才是实情。

**层②：新落盘的 ELF 必须签名，而 rustup 的签名钩子不覆盖事后补装的组件**

OHOS 内核**拒绝 exec 任何不带 `.codesign` 段的 ELF**（`Permission denied`）。判据看段表：

| 文件 | 签名相关段 |
|---|---|
| `rustup component add` 刚落盘的 rust-analyzer | 只有 `.note.ohos.ident` ← **没签** |
| 自签名覆盖后的 rust-analyzer | `.note.ohos.ident` + **`.codesign`** ← 能跑 |

harmonybrew 的 rustup formula 带 `0003-ohos-post-install` 钩子，把 `rustc` / `cargo.real` / `rust-lld`
逐个自签名（日志原文 `info: ohos-post-install: signed <... rustc / cargo.real / rust-lld ...>`）。
但该钩子挂在「**下载工具链**」这个动作上，**事后 `component add` 补装的组件不走它** ——
这就是「工具链本身能跑、补装的 rust-analyzer 不能跑」的准确原因。

另外，签名产物默认权限 `0o660`，**不带执行位**，光签名还不够，必须再补 `chmod`。

**层③：调用方的环境变量不该跨越 uid 边界**

应用（uid 20020220）与守护进程（uid 20020117）运行在不同 uid、不同挂载视角下。应用进程环境里
有一批值**只在它自己的沙箱内可解析**：

| 变量 | 应用侧的值 | 守护进程侧 |
|---|---|---|
| `SSL_CERT_FILE` / `CURL_CA_BUNDLE` | HAP 内私有路径的 `ca-bundle.crt` | **不可见** ⇒ 证书校验失败（`class=Ssl (16)`） |
| `PATH` | 系统 PATH + el2 base + el1 resource，**不含工具链目录** | 本已含工具链（守护进程是命令行拉起的） |
| `HOME` | 用户选的数据根 | 与实际环境不一致 |

**传递链**（问题形态）：

```
应用 ensure_shell_env() 拼出短 PATH（launch_app.rs:78-122）
  → shell_env::capture() 采到的就是应用进程 env（util/src/shell_env.rs:48-55 capture_ohos）
    → lsp_store.rs 把整份 shell_env 设为 LanguageServerBinary::env（:858-871 / :736-741）
      → build_spec() 放进 ExecSpec.env（util/src/command/ohos.rs:402）
        → 客户端渲染成 `PATH='…' SSL_CERT_FILE='…' exec <程序>` 前缀（cmd-client/src/command.rs）
          → 守护进程 sh（uid 20020117）按前缀执行 ⇒ 工具链 PATH 被顶掉、证书文件打不开
```

**现场铁证**（守护进程 `--log` 实抓，2026-09-12 22:12）：

```
mkdir -p '<工作区>' && cd '<工作区>' && … \
PATH='/data/app/bin:/data/service/hnp/bin:…:/data/storage/el2/base/haps/entry/files:/data/storage/el1/bundle/entry/resources/resfile' \
… exec '/storage/Users/currentUser/.harmonybrew/bin/rust-analyzer' '--help'
```

三个关键事实：① 前缀确实生效（该次调用 `exit=0`，证明 `sh` 按前缀执行了）；② 被注入的 PATH 是
**应用进程的**（末尾两项正是 `launch_app.rs` 追加的，环境里还有 `__LIBACE_ENTRY_POINT` 等应用专有变量）；
③ **自相矛盾的组合** —— RA 的路径来自「经守护进程 shell 的 `which`」（那条链路 PATH 含工具链），
可它**自己进程的 PATH 里却没有那个目录**。

补充互证：日志中**完全没有 `cargo`/`rustc` 的 exec 记录** —— cargo 不是守护进程启动的，而是 RA 自己
fork 的，只继承 RA 那份被覆盖的 PATH，父进程直接 `execve` 失败，不经过守护进程、也不留痕。
这与「四条 ENOENT、无一句 cargo 自身输出」吻合。

**实测判决**（同一系统、复刻原命令）：

| 条件 | 结果 |
|---|---|
| PATH = 应用式（无工具链） | `sh: cargo: inaccessible or not found` ← 与日志 ENOENT 同形 |
| PATH 前插工具链目录 + 有效 HOME | `cargo locate-project …` → 正常输出 |
| 外环境 PATH 正确，但命令前拼应用式短 PATH 前缀 | `sh: cargo: inaccessible or not found` ← **决定性证据：覆盖即失因** |

⇒ 结论：**问题不是「设备连不上网」也不是「没人告诉它 cargo 在哪」，而是调用方环境本就不该跨越 uid 边界**。

**层④：registry 缓存缺 316 个 crate**

`Cargo.lock` 有 2007 个 package（registry 源 1644、git 源 76）。设备侧起初只有 1328 个 `.crate`，
缺 316 个。层③把联网路径堵死后，cargo 也无法自行补齐 —— 两者互为因果。
（git 源 32 个 repo **一个不缺**；日志里 `async-process revision not found` 是**时序假象**。）

**层⑤：临时目录的落点机制**

- 设备 `/tmp` 是 **`erofs ro` 只读镜像**（`/dev/block/dm-0 /tmp erofs ro`），**永久写不了**；
  `/var/tmp`、`/data/local/tmp` 不存在。
- 这份 Rust 1.97.1 的 `std::env::temp_dir()` ＝ **`TMPDIR` 优先，否则平台默认（linux-ohos 落 `/tmp`）**，
  **没有 HOME 回退**（`…/lib/rustlib/src/rust/library/std/src/sys/paths/unix.rs:407-415`）。
- RA 用 `tempfile` 建临时目录，正常退出靠 `Drop` 清理；被强杀就不清理 ⇒ 层⑤的症状。

**附带发现：`linker 'cc' not found`（本次未修，见「遗留」）**

`cargo check` 会全挂，因为它必须为 build script 生成**并运行**可执行文件、为 proc-macro 生成 `.so`，
两者都绕不开链接。而 rustc 在所有 `*-linux-*` 目标上默认拿裸名字 `cc` 当 linker 驱动，OHOS 上游 llvm 包
**从来没有提供过这个名字**（只有 `clang` / `clang++` / `ld.lld`）。不影响跳转与补全。

### B. TS Server：单点 —— Node 报的平台名工具不认

**因果链**

```
Node 在 OHOS 上   process.platform === 'openharmony'
        │
        ▼
TypeScript 5.9.3  getGlobalTypingsCacheLocation() 用 switch (process.platform) 分支取全局类型缓存目录
        │
        ├─ case "win32"                       → OK
        ├─ case "openbsd"/"freebsd"/"netbsd"
        │  / "darwin"/"linux"/"android"       → OK（连 android 都认）
        └─ default:                           → Debug.fail(`unsupported platform '${process.platform}'`)
                                                    ↳ 抛异常 → tsserver 进程退出码 1
```

**关键代码** —— `typescript@5.9.3` 的 `lib/_tsserver.js:545-563`：

```js
function getGlobalTypingsCacheLocation() {
    switch (process.platform) {
      case "win32": { /* … */ }
      case "openbsd":
      case "freebsd":
      case "netbsd":
      case "darwin":
      case "linux":
      case "android": {
        const cacheLocation = getNonWindowsCacheLocation(process.platform === "darwin");
        return combinePaths(combinePaths(cacheLocation, "typescript"), versionMajorMinor);
      }
      default:
        return Debug.fail(`unsupported platform '${process.platform}'`);   // ← 561 行
    }
}
```

**要点**：这份清单里连 `android` 都有，唯独没有 `openharmony`。而 `process.platform` 在 OHOS 上
返回的正是 `'openharmony'`（本机 node 实测），于是一头撞进 `default`。

**为什么是「主动中止」而不是「功能降级」**

`Debug.fail()` 的设计意图是「这个分支理论上不可达，真到了就是代码有 bug」。TypeScript 作者显然
没把 `openharmony` 列进「理论上可达」的平台集合，所以这里没有兜底路径、也没有环境变量逃生口 ——
**必须让平台名落进已知分支，或者拦在它读平台名之前**。

**为什么选链路下层的 preload，而不是改 TS**

核心事实是：这个应用早就有一条给 Node 子进程打宿主补丁的现成通道 —— 守护进程（`hicodeerd`）在 spawn
每个子进程时都会注入 `NODE_OPTIONS=--require <shim.js>`，而 vtsls fork tsserver 时**原样继承
`NODE_OPTIONS`**，补丁能一路传到底，无需碰任何第三方文件。

## 解决方案

### A. rust-analyzer

**层① + 层②：补组件 → 自签名 → 补执行位**

```bash
# 1) 装官方组件（中科大镜像，7s / 58 MB）
rustup component add rust-analyzer --toolchain 1.97.1-aarch64-unknown-linux-ohos

# 2) 自签名（原件先改名备份）
/data/storage/el1/bundle/libs/arm64/binary-sign-tool sign \
    -inFile <RA> -outFile <RA>.signed -selfSign 1
mv <RA>.signed <RA>

# 3) 补执行位（sign 产物默认 0o660，没有 x 位）
chmod 0o775 <RA>
```

验证：`rust-analyzer --version` → `rust-analyzer 1.97.1 (8bab26f4 2026-07-14)`。
**`8bab26f4` 与项目 rustc 的 commit 完全一致**，证明它与项目编译器同源 ——
这正是「必须走 `rustup which` 而不是随便一个 RA」的价值。

**层③：远端命令不带任何环境变量（核心代码改动）**

`crates/util/src/command/ohos.rs` 的 `build_spec()` 由「转发调用方的显式覆盖」改为「**什么都不带**」——
远端命令只从守护进程自身被拉起时的环境开始：

```rust
// 改动后：不写 spec.env，远端命令继承守护进程自身环境
// A remote command carries no environment: it starts from whatever the
// daemon itself was launched with. The two run under different uids, so the
// caller's own environment can name roots that only resolve inside its
// sandbox, and forwarding those would hand the child a path it cannot open.
// Explicit overrides are honoured only where the child runs on this device
// -- see `spawn_local`.
```

**为什么选这个方案**：曾考虑的三条补 PATH 备选（改数据根 `settings.json` 的
`lsp.rust-analyzer.binary.env` / 在 `launch_app.rs` 补齐应用 PATH / 在 `capture_ohos` 补），**均未采用** ——
它们要么只治 PATH 不治证书，要么在「采集应用 env」的地方打补丁。最终判定：调用方环境本来就不该跨越
uid 边界，**从契约上不转发**才是治本，且附带收益是远端命令行为可预测（只取决于守护进程怎么被拉起的）。

**保留项**：`Command::env()/envs()/env_remove()/env_clear()` 不能删 —— 本地 HNP 分支 `spawn_local()`
要用，Zed 上层（如 `crates/git/src/repository.rs` 注入 `GIT_INDEX_FILE`）也在调。
**只有远端那条路不带 env。**

**层③清理：删掉成死代码的二次处理**

`build_spec()` 不再下发 env 后，cmd-agent 里两处处理变成空转，一并删除（保持整洁）：

1. **守护进程的 PATH 合并**（`hicodeerd/src/exec.rs`）：`PATH_ENV` 常量、`merged_path()`、
   `path_assignment_index()`、`merge_path_assignment()`、同文件那份只服务它的 `sh_quote()`、
   `spawn_command` 里的调用点、3 个相关测试、模块注释段。
   *行为等价依据*：被删的 `merge_path_assignment()` 自己写着 ——「没有 `PATH` 赋值就原样返回，
   子女继承守护进程自己的 PATH，那本来就是完整的」，即无输入时是 no-op。
2. **客户端的 git 注入**（`cmd-client/src/command.rs`）：`GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_0` /
   `GIT_CONFIG_VALUE_0` 三常量与 `if spec.source_program == "git"` 块。
   *为何是死代码*：`local_exec_matches()` **只按 basename 匹配** HNP 名单，而 `git` 已作为 HNP 打入应用
   ⇒ 任何叫 `git` 的命令都被强制走**本地** fork，远端那条路永远执行不到。

**必须保留的共用件**：`split_words()` / `Word` / `is_env_assignment()` / `is_separator()`
（被 `rewrite_npm_para()` 使用，删了编译不过）；`cmd-client/command.rs` 的 `sh_quote()`
（`build_command` 5 处调用，是整个命令串的地基 —— 与上面删掉的那个**同名但独立**）。

**层④：补齐 registry 缓存**

```bash
cargo fetch        # 本机执行即可，设备侧 cargo home 就是同一个 ~/.cargo
```

- 结果：`.crate` 从 1328 → **1644**，正好等于 `Cargo.lock` 的 registry 需求数，一个不缺。
- 验证：**离线**跑完整 `cargo metadata` → **3.7 秒，exit=0，11 MB JSON**；完整版不再降级。
- 关键前提：应用链路的 cargo home 就是真实家 `~/.cargo`（4.0 G 完整）；数据根下那个 `.cargo`
  （22 M、连 `registry/` 都没有）是废弃残留。

**层⑤：临时目录经握手落到数据根**

守护进程拿到数据根后，把它的临时目录指到 `<数据根>/tmp` 并设 `TMPDIR`，其派生程序自然继承。
数据根**不猜**，由客户端每轮握手带过来（与垫片同一条通路）：

- `hicodeerd/src/session_tmp.rs`（新增）：`adopt(root)` —— `OnceLock` 守卫（只做一次）+
  `create_dir_all(<root>/tmp)` + `set_var("TMPDIR", …)`；任一步失败只 `log::warn!` 且**不动** `TMPDIR`。
- `hicodeerd/src/protocol.rs` / `cmd-client/src/protocol.rs`：新增 `parse_bootstrap_command()` /
  `bootstrap_command()`，握手命令 `hicodeerd-bootstrap <$HOME>` 的编解码。
- `cmd-client/src/bootstrap.rs`：每轮握手读 `HOME` 带给守护进程。
- `hicodeerd/src/management.rs:103-104`：握手分支里调用 `adopt(root)`。

验证：家目录 `proc-macro-srv*` **21 → 0**、`rust-analyzer*` **2 → 0**；`<数据根>/tmp` 建立并被实际使用。

**配置：语言服务内存优化（`<数据根>/config/settings.json`）**

新增 `lsp.rust-analyzer.initialization_options`：

```json
"lsp": {
  "rust-analyzer": {
    "initialization_options": {
      "check": { "workspace": false },
      "cachePriming": { "enable": false }
    }
  }
}
```

- **形状铁律**：必须**嵌套对象**。RA 的 `get_field_json`（`crates/rust-analyzer/src/config.rs:3621-3640`）
  把字段名下划线换成 JSON 指针（`check_workspace` → `/check/workspace`）；写扁平点分键
  `"check.workspace"` 会去找字面键 ⇒ **静默失效**。
- 通道：`lsp_store.rs:437` 取 `initialization_options` → `:570-591` 合并后下发；Rust 适配器未实现
  `initialization_options`（trait 默认 `None`）⇒ 用户值**原样成** `initialize` 参数。
  `lsp.rust-analyzer.settings` 才是死字段（只有实现了 `workspace_configuration` 的适配器会读，
  全仓 13 处调用无一在 `rust.rs`）。
- 生效方式：命令面板 `editor: restart language server`，或重启应用。
- 代价：`check.workspace=false` 后非当前包不再出诊断；`cachePriming` 关闭后首次查询略慢。

**A 的最终验证**

| 检查 | 结果 |
|---|---|
| 设备进程 | `rust-analyzer`（CPU 341%，累计 16:48）+ **4 个** `rust-analyzer-proc-macro-srv` |
| 说明 | proc-macro server 只有在**拿到完整项目模型**后才会拉起，`--no-deps` 降级态给不出这个 |
| 原报错 | hilog 中 `FetchWorkspaceError` / `failed to load workspace` 命中数 **0**；LSP stderr 中 `os error 2` 命中数 **0** |
| 临时文件 | 家目录残留 **21+2 → 0** |
| 编译 | `./script/bundle-ohos` → `BUILD_RC=0`，仅 1 条无关告警（wasmtime 的 clang 参数） |

### B. TS Server

**突破性洞察**

**不要修 TypeScript，要修「TypeScript 看到的平台名」，而且要挂在已有的那条 preload 通道上。**

三层证据确认这条通道能把补丁送到 tsserver：

| 环节 | 证据 |
|---|---|
| 守护进程给所有子进程注入 preload | `hicodeerd/src/exec.rs:368` 的 `crate::shim::apply(&mut cmd)`，作用于 `sh -c <命令>` 通用 exec 通道；`pty.rs:241` 同理 |
| vtsls 被我方守护进程拉起，继承 `NODE_OPTIONS` | `hicodeerd → node …/vtsls.js --stdio`，父子链在设备 `ps -ef` 中可见 |
| vtsls fork tsserver 时不丢 `NODE_OPTIONS` | `@vtsls/language-service/dist/index.js:15404` 的 `fork(tsserver.js, args, { env: generatePatchedEnv(process.env) })`；该函数（同文件 `:15194`）是 `Object.assign({}, env)` 后**只**改 `ELECTRON_RUN_AS_NODE`/`NODE_PATH`/`PATH`，**不触碰 `NODE_OPTIONS`** |

于是链路闭合：

```
hicodeerd ──NODE_OPTIONS=--require <数据根>/node/shim/shim.js──▶ sh -c "node …/vtsls.js"
    └─ vtsls（已 preload shim）
         └─ fork(tsserver.js, env=generatePatchedEnv(process.env))   ← NODE_OPTIONS 原样继承
              └─ tsserver（已 preload shim）→ platform 已归一为 'linux' → 正常启动
```

**关键改动**

*引入（`shim/shim.js`）* —— 新增一个「平台名归一」段落，带守卫、非 OHOS 平台为 no-op：

```js
if (process.platform === 'openharmony') {
  try {
    Object.defineProperty(process, 'platform', { value: 'linux' });
  } catch (error) {
    // Left as reported; a caller that cares surfaces its own error.
  }
}
```

之所以能用 `defineProperty`：本机 node v24.13.0 上 `process.platform` 的属性描述符是
`configurable: true`（实测），可被改写，且 `os.platform()` 同源、会一并变成 `linux`。
之所以选 `'linux'`：运行时本就是 Linux 用户态，`getNonWindowsCacheLocation()` 为 linux 挑的路径
（`$XDG_CACHE_HOME` 或 `$HOME/.cache`）在 OHOS 上是可用路径。

*改名与合并（同一改动的一部分）* —— 原文件叫 `osuser-shim.js`，只修一件事：`os.userInfo()` 在 OHOS 上
抛 `ERR_SYSTEM_ERROR`。现在它要修第二件事，**名字已经名不副实**，于是：

| 旧 | 新 | 理由 |
|---|---|---|
| `shim/osuser-shim.js` | `shim/shim.js` | 按「它是什么」（垫片）命名，而非按「它最先修的那个 bug」命名 |
| `src/passwd_shim.rs` | `src/shim.rs` | 同上；模块不再只服务 passwd/userinfo 一件事 |

并把「**只允许一个 preload 文件**」写成约定，落进 `shim/shim.js` 文件头注释（原文）：

> Keep this the ONLY preload file. Every host workaround belongs in here, in its own labelled section
> below -- do not add a second shim, and do not split these patches across files. One file means one
> `--require` to materialise, keep in sync on the device, and reason about when a Node process
> misbehaves; a second file makes all three worse for no gain. **The file is named for what it is,
> a shim, rather than for the first bug it happened to fix.**

`shim.rs` 的模块文档同步写明同一条约束（「Add new host fixes inside that script instead of adding a
second shim… Do not reintroduce a per-bug file here or in `shim/`」）。

*路径拼装自动跟随改名* —— 「把 shim 传给 node 的地方」在 `src/shim.rs`，路径由常量拼出，
所以改名后无需再改逻辑：

```rust
const SHIM_FILE: &str = "shim.js";                        // :34
const SHIM_SUBDIR: &str = "node/shim";                    // :36
let path = root.join(SHIM_SUBDIR).join(SHIM_FILE);        // :65 → <数据根>/node/shim/shim.js
value.push_str("--require "); value.push_str(shim);       // :118-119 → NODE_OPTIONS="--require <该路径>"
```

*构建脚本不用动* —— `script/bundle-ohos:260` 的注释已写明「The Node preload is compiled into the
daemon and re-materialised at runtime」，即垫片由 `include_str!` 编进二进制、运行时落到数据根，
**包内不带副本**，所以文件名变化与打包脚本无关。

**方案对比（为何不选其它）**

| 方案 | 做法 | 结果 |
|---|---|---|
| **采用**：复用守护进程 preload 通道 | 在 `shim.js` 加平台段 + 重编译 daemon | ✅ 一处改动修掉**所有** Node 子进程的同类问题（tsserver / eslint / tailwind LS…），不动任何第三方文件，升级 TS 不失效 |
| ✗ 改 `_tsserver.js` 加 `case "openharmony"` | 在 `node_modules` 里加一行 | 改三方产物，重装/升级 `vtsls`/`typescript` 即丢失；只修 TS 一家 |
| ✗ 给 vtsls 的 spawn 挂 `--require` | 改 vtsls 的 dist 代码 | 同上，且要额外找 spawn 点，比现成通道更绕 |
| ✗ 环境变量绕过 | 设 `TYPESCRIPT_GLOBAL_TYPES_CACHE_LOCATION` | **实测无效** —— `default` 分支在读到环境变量之前就 `Debug.fail` 了 |
| ✗ 手改设备上的落盘垫片 | 直接编辑 `<数据根>/node/shim/osuser-shim.js` | **无效** —— `write_if_stale()` 按内容比对，daemon 下次 spawn 会把它重写回编译版。必须改源 + 重编译 |

**验证**

*① 补丁本体（本机静态 + 动态）*

| 检查 | 结果 |
|---|---|
| 源内旧名残留（`passwd_shim` / `osuser-shim`） | **0 处** |
| `node --check shim.js` | 通过 |
| 本机 node 事实 | `process.platform = "openharmony"`，属性 `configurable: true` |
| **不加 shim** 拉起 tsserver | `Debug Failure. unsupported platform 'openharmony'` → **退出码 1** |
| **加新 shim** 拉起 tsserver | 静默启动 → **退出码 0**（正常等 stdio） |
| 加 shim 后 `os.userInfo()` | 返回 `{uid:20020201, username:"hicodeer", homedir:"/storage/Users/currentUser", shell:"/bin/sh"}`（原先 `ERR_SYSTEM_ERROR`） |

*② 产物（拆 HNP 直接数二进制字符串）*

`hap/entry/hnp/arm64-v8a/hicodeerd.hnp` 内 `bin/hicodeerd`：

| 字符串 | 计数 |
|---|---|
| `shim.js` / `node/shim` | 1 / 1 ✅ |
| `--require` | 3 |
| **`osuser-shim`** | **0** ✅ |
| `openharmony` / `process.platform` | 3 / 3（内嵌 JS 里的守卫与判断） |

HNP 内清单只有 `hnp.json / bin/hicodeerd / conf/mgmt_host_key / conf/authorized_keys`
—— 包内不带垫片副本，与设计一致。

*③ 编译与装机*

- 编译：`./script/bundle-ohos`（经 VM 172.16.100.2），**5 分 05 秒，exit 0**。
  > 说明：该脚本没有「只编 daemon」档位；能跳 Rust 的 `--hap-only` 恰好会跳过 hicodeerd 重编。
  > 5 分钟里 daemon 本身只占几十秒，大头是重出并签名 596 MB 的整包 HAP。
- 装机：**29 秒**，`install bundle successfully` + `start ability successfully` + `APP_STARTED`
  （包名 `com.hicodeer.studio`）。

*④ 运行态（决定性）*

- 数据根 `<数据根>/node/shim/shim.js` 于装机后 13 秒由 daemon 写盘，**3940 B，md5 与源文件一致**
  → 新 daemon 在跑、且注入的是新路径。
- 进程树持续存活：`hicodeerd(61532) → node …/vtsls.js --stdio(62157) → tsserver×2(62189/62190) → typingsInstaller(62222)`。
- 其中 `typingsInstaller` 已带上
  `--globalTypingsCacheLocation /data/storage/el2/base/haps/entry/files/typescript/5.9`
  —— **这正是原先 `Debug.fail("unsupported platform")` 那条代码路径**，现在能正常算出缓存目录。
- hilog 里 `vtsls failed: server shut down` 全部落在 **装机前的旧实例**（18:11:47–51）；
  新 LS 于 18:13:09 拉起后**再无退出记录**。原 `TSServer exited. Code: 1` 消失。

## 走错的弯路（两部分合计，供后人避坑）

### A. rust-analyzer

| 弯路 | 为什么错 |
|---|---|
| 在 `cmd-client` 里加「过滤沙箱路径」的 `SANDBOX_ROOTS` 过滤器 | 落点错了：客户端只该渲染，不该判断环境语义；过滤法还要枚举沙箱根、随平台变化失效。**已完全回退**，改在 `ohos.rs` 的 `build_spec()` 从源头不转发 |
| 认为「应用链路的 `HOME` = 数据根」导致 cargo 去数据根找工具链 | 实测守护进程是**终端手工拉起**的（`hicodeerd ← zsh ← hishell`），`HOME` = 真实家，该因果链不成立 |
| 把手工执行时看到的「阿里云 404 → 超时」当作应用现象 | 那是手动 rustup 的输出；应用侧日志显示的是 ~200 ms 秒退，两回事 |
| 以为 `cargo metadata` 卡住了 | 首次要下 git 302 MB + registry 919 MB（62,594 文件）≈ 1.2 GB，8–10 分钟属正常；缓存后单跑 60 s |
| 把日志里 `async-process` 的 `revision not found` 当独立故障深挖 | 是**时序假象**：本地对象库里 `refs/commit/0b6d671…` 08:59 已补齐，`cat-file` 返回 commit。真堵点在 registry 侧 |
| 信了 `cmd-client` 的「No PATH is injected」注释 | 那句说的是本模块的意图，而上游 `shell_env` 早已把 PATH 塞进来 —— 注释与事实不符，是本处最容易误导人的地方 |
| 把设备侧 `/tmp` 当作可写 | 它是 `erofs ro` 只读镜像；`/dev/shm` 虽可写但 tmpfs 上限 1 GB 且 noexec，不适合放语言服务的临时文件 |
| 认为改 `TMPDIR` 要「每个心跳都设」或「连 HOME 一起设」 | `set_var` 一次即全链路继承（子进程默认继承父环境）；且 `temp_dir()` **不读 HOME**，设 HOME 是多余且有害的 |

### B. TS Server

| 弯路 | 为什么错 |
|---|---|
| 把日志里的「文件监听回退」当成病因 | `<syntax>/<semantic> Falling back to legacy node.js based file watching because of user settings.` 是设置项导致的正常回退提示，与崩溃无关。**别顺着它查** —— 真正的信息在编辑器不转发的子进程 stderr 里 |
| 指望用环境变量绕过 | `TYPESCRIPT_GLOBAL_TYPES_CACHE_LOCATION` 看似是逃生口，实测**无效**：`default` 分支在读到环境变量之前就 `Debug.fail` 了 |
| 打算去改 TypeScript 的 `_tsserver.js` | 加一行 `case "openharmony":` 看似最直接，但落在 `node_modules` 里 —— **重装或升级 `vtsls`/`typescript` 就会丢**，而且只修 TS 一家。既然应用已有通用 preload 通道，没理由碰三方产物 |
| 以为可以直接手改设备上那份垫片 | 设备上 `<数据根>/node/shim/*.js` 是 daemon 从二进制里 `include_str!` 出来的副本，`write_if_stale()` 会按内容比对并**重写回编译版**。必须改源 + 重编译 |
| 以为要 root / `su` / `hdc smode` | 本机 hdc shell 是系统 `shell` 用户（uid=2000），写 `persist.*` 参数直接有权，无需提权；本次修复全程未用到 root。别一上来就往提权方向找路 |
| 以为「只编 hicodeerd」很快，不用等整包 | `bundle-ohos` 无 daemon-only 档位；改动只有 4.4 MB 的 daemon，但交付物是 596 MB 的签名 HAP。心里要有这个数量级预期 |

## 修改文件

### A. rust-analyzer —— 代码

- `crates/util/src/command/ohos.rs` — `build_spec()` 不再写 `spec.env`（远端命令继承守护进程自身环境）；删除上一轮临时加的 `HOME_ENV` 常量；模块注释与 `env()` 方法注释同步说明「仅本地子进程受显式覆盖影响」
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/exec.rs` — 删除已成死代码的 PATH 合并全套（`PATH_ENV`、`merged_path()`、`path_assignment_index()`、`merge_path_assignment()`、本地 `sh_quote()`、调用点、3 个测试、相关注释段）；保留 `split_words()`/`is_env_assignment()`/`is_separator()` 供 `rewrite_npm_para()` 使用
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/command.rs` — 删除 git 的 `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0` 三常量与 `if spec.source_program == "git"` 注入块
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/session_tmp.rs` — **新增**：`adopt(root)` 建 `<数据根>/tmp` 并设 `TMPDIR`
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/protocol.rs` — **新增** `parse_bootstrap_command()`
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/protocol.rs` — **新增** `bootstrap_command()`
- `crates/gpui_ohos/depend/cmd-agent/cmd-client/src/bootstrap.rs` — 每轮握手读 `HOME` 带给守护进程
- `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/management.rs` — 握手分支调用 `session_tmp::adopt()` 与 `shim::adopt()`

### A. rust-analyzer —— 环境侧变更（非代码）

- `~/.rustup/toolchains/1.97.1-…/bin/rust-analyzer` — `rustup component add` 补装后自签名 + `chmod 0o775`；未签名原件备份为 `rust-analyzer.unsigned.bak`（58 MB，待清）
- `~/.rustup/toolchains/1.97.1-…` — 新增 `rust-src` 与 `rust-analyzer` 组件
- `~/.cargo/registry/cache/` — `cargo fetch` 补齐 316 个 `.crate`（1328 → 1644）
- `<数据根>/config/settings.json` — 新增 `lsp.rust-analyzer.initialization_options`（内存优化两项）

### B. TS Server —— 代码

全部位于 `crates/gpui_ohos/depend/cmd-agent/hicodeerd/`：

- `shim/osuser-shim.js` **→** `shim/shim.js` — 改名（按本名而非首个 bug 命名）；文件头重写为「**唯一 preload**」约定注释；新增 `process.platform === 'openharmony'` → `linux` 的归一补丁段；保留原 `os.userInfo()` 兜底段
- `src/passwd_shim.rs` **→** `src/shim.rs` — 改名；模块文档改写为「宿主兼容 preload」并复述单文件约定；`SHIM_FILE = "shim.js"`、`SHIM_SUBDIR = "node/shim"`；日志 tag `passwd shim:` → `shim:`
- `src/main.rs:19` — `mod passwd_shim;` → `mod shim;`（模块改名后的声明同步）
- `src/management.rs:11` — 模块文档注释里的 `passwd_shim` → `shim`
- `src/management.rs:104` — `crate::passwd_shim::adopt(root)` → `crate::shim::adopt(root)`（bootstrap 握手处落地垫片）
- `src/exec.rs:368` — `crate::passwd_shim::apply(&mut cmd)` → `crate::shim::apply(&mut cmd)`（通用 exec 通道注入 preload）
- `src/pty.rs:241` — `crate::passwd_shim::apply(&mut cmd)` → `crate::shim::apply(&mut cmd)`（pty 通道注入 preload）

未改动：`script/bundle-ohos` —— 其 `:260` 注释已说明 preload 编进二进制、运行时落到数据根，与文件名无关。

### 文档侧（随本次改名 / 合并同步）

- `移植记录/bugfix/2026-09-14-ohos-node-npm-toolchain.md` — 2026-09-14 把 Node/npm 系列报告合并成这一篇（原 userinfo 篇已随之并入，本处引用指针同步）
- `.workbuddy/memory/ARCH/05-platform.md`、`.workbuddy/memory/ARCH/09-pitfalls.md` — 更新指向本篇的路径与一句话描述

**未同步（有意为之，已在「遗留」列出）**：`移植记录/zed问题定位手段/Zed提供的问题定位手段.md` 与
`移植记录/design/2026-09-08-ohos-qemu-runtime-design.md` 中仍写着旧垫片名。它们**不只是名字陈旧** ——
还描述着已被握手机制取代的 `install(&conf)` 与「数据根三级定位」（现实现是 `adopt(root)`，
数据根由客户端每轮握手带来）。只改名字会留下一份「名字新、机制错」的看似权威的描述，
故不在本次改名里顺手改，留待单独一轮内容修订。

## 遗留（本次未修，已单列）

**`linker 'cc' not found`** —— `cargo check` 仍全挂（跳转/补全不受影响）。

- 定性：**名字不存在**，不是 PATH 缺目录。`rustc --print link-args` 证明 rustc 要执行的就是裸 `cc`；
  全盘（`~/.harmonybrew`、`/usr/bin`、`/bin`、`/system/bin`、`/vendor/bin`）**0 个名为 `cc` 的文件**，
  而 `which clang` 能解析。
- 已实测可行解：`brew install llvm-gcc-compat`（依赖已装的 `ohos-sdk`，在 `~/.harmonybrew/bin/`
  建 16 个 GCC 风格软链 `cc/gcc→clang`、`ld→ld.lld`、`ar→llvm-ar` 等）。
- **用户定下的方向**：不该让用户手工建软链，应由应用侧探测 `clang` 自建 `cc`（与 node 垫片同一套路）。
- 另：rust-analyzer **没有磁盘缓存**，分析结果只在进程内存，重启即全量重来。

**其他残留**

- `rust-analyzer.unsigned.bak`（58 MB 未签名原件，待清）
- 设备上旧的 `<数据根>/node/shim/osuser-shim.js`（1674 B）为**无害孤儿文件** —— 新 `NODE_OPTIONS`
  只指向 `shim.js`，不会被加载，可按需清理
- 设备在 09-13 08:14 重启过一次（原因未查）

**文档与旧垫片名脱节（需单独一轮修订）**

- `移植记录/zed问题定位手段/Zed提供的问题定位手段.md`（`:400`、`:401`、`:404`）
- `移植记录/design/2026-09-08-ohos-qemu-runtime-design.md`（`:35`、`:91`、`:94`、`:98`）

两处仍写着 `osuser-shim.js` / `passwd_shim.rs`，且**同时**保留着已被握手机制取代的描述 ——
`install(&conf)`、`HICODEERD_DATA_ROOT` → `customer_data_path` → `../../hicodeer` 的「数据根三级定位」、
「启动期写盘」。现实现是握手期 `adopt(root)`（数据根由客户端每轮带来）+ 每次派生 `ensure_placed` 自愈。
**这不是机械改名能修好的**，需要按当前实现重写相关段落，故单列。

`移植记录/bugfix/2026-09-11-ohos-node-userinfo-passwd-shim.md` 曾按**历史快照**保留旧名。
2026-09-14 该篇已连同另外 6 篇 Node/npm 报告合并进
`移植记录/bugfix/2026-09-14-ohos-node-npm-toolchain.md`，原文件删除
（备份在 `.workbuddy/tmp/backup-20260914-085943-node-npm-md/`）。

## 关联与前后接续

- `移植记录/bugfix/2026-09-11-ohos-process-spawn-via-command.md` — 外部进程统一经 `util::command` 路由，LSP 走的就是这条路。
- `移植记录/bugfix/2026-09-14-ohos-node-npm-toolchain.md` — 同一条握手通路支撑的 Node `os.userInfo()` 垫片（原 userinfo 篇已并入此篇）；**与 B 部分改的是同一个垫片文件**，但它修的是另一件事（`os.userInfo()` 的 `ERR_SYSTEM_ERROR`），与 `process.platform` 归一互不依赖。
- `移植记录/bugfix/2026-09-03-ohos-qemu-guest-time-sync.md` — 同日另一处 env / 通路问题。
- **历史沿革**：`2026-09-12-rust-lsp-toolchain-path.md`（PATH 覆盖）已被本篇合并吸收；其修法（守护进程侧合并 PATH）在本篇的「不转发 env」收口下成为死代码，已删除。两者结论不冲突 —— 09-12 治「PATH 被覆盖」，本篇治「根本不该转发任何 env」，后者是前者的收口，也是更小的契约面。
- **一处未完全对齐**：09-12 曾把 `class=Ssl (16)` 归为「cargo 内置 libgit2 不读 `CURL_CA_BUNDLE`」，并给出 `CARGO_HTTP_CAINFO` + `CARGO_NET_GIT_FETCH_WITH_CLI=true` 的绕过。本篇在应用链路上观察到的同类失败，触发点是调用方的 `SSL_CERT_FILE` 被转发（HAP 内私有 `ca-bundle.crt`，守护进程侧打不开）；移除 env 转发后应用侧完整 `cargo metadata` 直接通过，无需任何 `CARGO_*` 绕过。两条归因不互斥，指向同一个「CA 路径不对」的根；**本篇覆盖应用链路**，`CARGO_HTTP_CAINFO` 那招在别的环境（如守护进程自身也缺 CA）可能仍需保留，留给后续在纯净设备上复验。
- **A 与 B 的关系**：两条链**没有因果关系**，别因为都发生在同一天、都表现为「语言服务起不来」就混为一谈。唯一的重合点是支撑通路（应用 → 守护进程 → 子进程 / 握手下发数据根），以及 B 的实现落在 A 那次改动建立起来的握手通路上。

[[ohos-debug-lessons]]
