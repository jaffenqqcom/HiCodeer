# OHOS 上 Node / npm 工具链问题汇总（最终方案 + 已废弃方案留痕）

> 2026-09-14 合并重写。本篇取代以下 7 篇，原文件已删除（备份在
> `.workbuddy/tmp/backup-20260914-085943-node-npm-md/`）：
>
> - `2026-08-25-ohos-lsp-dir-relocate.md`
> - `2026-08-25-ohos-lsp-npm-install-path.md`
> - `2026-08-25-ohos-lsp-watchdog-pid.md`
> - `2026-09-10-ohos-managed-node-npm-entrypoint.md`
> - `2026-09-10-ohos-tar-link-materialize.md`
> - `2026-09-11-ohos-node-userinfo-passwd-shim.md`
> - `2026-09-11-ohos-npm-bin-links-denied.md`
>
> 并收录 2026-09-13～09-14 新解决的三个问题（`--ignore-scripts` 导致的依赖缺失、
> 安装期未签名 ELF、GitHub 下载建连超时）。
>
> 体例：**正文只写当前仍在生效的方案**；被取代的历史方案一律挪到
> §11「已废弃方案留痕」，写明废弃原因，不做删除——留痕是为了防止重犯。

## 0. 结论速览

| # | 问题 | 当前方案 | 关键位置 |
|---|---|---|---|
| 一 | tar 含链接条目 → 整包解压失败 | 解压错误路径里恢复链接并物化为真实副本 | `util::archive::unpack_tar_ohos`，被 `github_download.rs:307`、`node_runtime.rs:728` 共用 |
| 二 | 托管 Node 的 npm 入口 `MODULE_NOT_FOUND ../lib/cli.js` | npm 入口直指 `npm-cli.js` 真文件，绕开被物化的 `bin/npm` | `node_runtime.rs:615-623` |
| 三 | npm 建 `node_modules/.bin` 链接被拒 | 垫片里的 symlink 替身：先试真链接，只有该目录根本不支持链接才介入 | `hicodeerd/shim/shim.js` |
| 四 | `os.userInfo()` 必炸（uid 不在 `/etc/passwd`） | 预载垫片兜底，仅 `ERR_SYSTEM_ERROR` 才合成记录 | `shim.js` + `shim.rs:61/151` |
| 五 | `--ignore-scripts` 导致子工程依赖缺失 | 去掉该参数，改由垫片在 spawn 前补签名 | `hicodeerd/src/exec.rs` |
| 六 | 安装期执行未签名 ELF 报 EACCES | 垫片在 spawn 前按需签名，判据是"签名工具是否在 PATH 可解析" | `shim.js:567` |
| 七 | GitHub 下载建连 10 秒超时 | 放宽到 30 秒 | `reqwest_client.rs:36` |
| 八 | eslint 报 `Unrecognized method eslint/noLibrary` | **未修**（诊断已完成，方案待定） | 见 §9 |

## 1. 三条平台硬事实（后面所有问题都由它们派生）

1. **沙箱禁止 `symlink(2)` / `hard_link(2)`** —— 但**不是所有目录都禁**：数据根所在的
   文件系统实测**支持**真符号链接（装机后实测 33 个真链接 / 10256 个普通文件）。
   因此任何"链接一定会失败"的假设都是错的，**必须先探测再兜底**。
2. **未签名 ELF 不可 exec** —— 内核对无 `.codesign` 段的 ELF 报 EACCES。签名命令
   `binary-sign-tool sign -inFile X -outFile X.signed -selfSign 1`，签完 `chmod 0775`。
3. **只能 exec 白名单程序** —— 所有外部进程统一走 `util::command`：设备上已快照的 HNP
   工具本地 fork，其余发 `ExecSpec` 给守护进程。详见
   [[2026-09-11-ohos-process-spawn-via-command.md]]（该篇通用性大于 npm 范畴，保留未合并）。

## 2. 问题一：tar 含链接条目时整个解压失败

**表现**：下载阶段正常，卡在解压；托管 Node 装完内容残缺，GitHub 下的 LSP/agent 包同样装不上。
官方 Node 包里 `bin/npm`、`bin/npx`、`bin/corepack` 全是软链。

**根因**：`async_tar` 默认 `Archive::unpack` 遇到链接条目被沙箱拒绝后**整包失败**，
不是跳过该条目。且两个下载入口表现不一致——`github_download.rs` 早有 OHOS 兜底，
托管 Node 的 TarGz 分支直接调 `unpack`，没有任何兜底。

**现状**：两个入口统一走 `util::archive::unpack_tar_ohos`，在 unpack 的错误路径里把链接
条目恢复成真实副本：

- `crates/http_client/src/github_download.rs:307`
- `crates/node_runtime/src/node_runtime.rs:728`

## 3. 问题二：托管 Node 的 npm 入口 MODULE_NOT_FOUND

**表现**：`Error: Cannot find module '../lib/cli.js'`，报错落在
`<数据根>/node/node-v24.11.0-linux-arm64/bin/npm`；每次必然失败，还没进安装逻辑就挂。
换成 `npm-cli.js` 手动跑就正常——这是定位分水岭。

**根因**：官方发行包里 `bin/npm` 本身是软链（指向 `../lib/node_modules/npm/bin/npm-cli.js`）。
经问题一的物化兜底后它变成真文件副本，于是 `npm-cli.js` 里的相对
`require('../lib/cli.js')` 指向了错误位置。

**现状**：`node_runtime.rs:615-623` 直接用 `NPM_PATH` 常量指向 `npm-cli.js` 真文件
（Linux 为 `lib/node_modules/npm/bin/npm-cli.js`，Windows 为 `node_modules/npm/bin/npm-cli.js`），
不再走 `bin/npm` 这个壳。

## 4. 问题三：npm 建 `.bin` 链接失败

**表现**：npm 安装收尾阶段为每个包的 `bin` 字段建链接，`node_modules/.bin/` 整目录都是软链，
沙箱下这步失败 → 安装中断，agent 装不上。

**现状（与旧方案不同，见 §11.5）**：垫片接管 `symlink` 三件套，**先调真实 `symlink`**；
只有当链接被拒**且**目标目录经探测确实不支持链接时才启用替身——在 `.bin` 下写一个
JS launcher（`#!/usr/bin/env node` + 绝对路径 spawn，再 `chmod 0775`），否则递归拷贝。
真实错误（`EEXIST`、`ENOENT` 等）原样抛回调用方。

实测数据根支持真链接，所以这条**全程未介入**——它是保险，不是主干路径。

## 5. 问题四：`os.userInfo()` 必炸 → 预载垫片

**表现**：给 agent 发任何消息都回 `refusal`；`session/prompt` 的
`_meta["codebuddy.ai/errorMessage"]` 是 `uv_os_get_passwd returned ENOENT`。
**与内容审查无关**——agent 把 prompt 期任何错误统一 catch 成 `stopReason:"refusal"`，
真因只在 `_meta` 里，而编辑器不读 meta，于是显示为误导性的"违反内容政策"。

**根因**：应用跑在自己的 uid（如 20020201 / 20020220），`/etc/passwd` 里只有系统账号，
libuv 的 `uv_os_get_passwd` 返回 ENOENT → Node 抛 `ERR_SYSTEM_ERROR`。agent 的
`getTeamUserId()` 四级回退（env → teamMemory.userId → git user.name → `os.userInfo().username`）
在设备前三级全空，第四级裸调；而 `getMemoryDir()` 无条件先算它，导致**每个 prompt 必踩**。

**方案**：`os.userInfo()` 走 libuv 原生调用，不读环境变量、没有配置项可改；唯一能插进去的
时机是 Node 进程启动瞬间预载代码（`NODE_OPTIONS=--require <垫片>`），而这个时机掌握在
**父进程守护程序**手里——所以是自动注入，用户侧零操作。

**现状**：

| 环节 | 位置 |
|---|---|
| 垫片源码（唯一一份，编译期 `include_str!`） | `crates/gpui_ohos/depend/cmd-agent/hicodeerd/shim/shim.js` |
| 落盘与注入 | `hicodeerd/src/shim.rs`：`adopt(root)`（`:61`）、`ensure_placed()`（`:151`） |
| 触发时机 | `management.rs:103-104`，握手分支里 `session_tmp::adopt(root)` 之后紧接 `shim::adopt(root)` |
| 注入出口 | `exec.rs:415`、`pty.rs:241` |
| 落点 | `<数据根>/node/shim/shim.js` |

- 兜底策略：**先调真实实现**，只有抛 `ERR_SYSTEM_ERROR` 才返回合成记录，其他错误原样抛。
- `safe_homedir()`：优先 `HOME`，再 `try os.homedir()`，最后 `/`。初版直接在 fallback 里调
  `os.homedir()`，`HOME` 缺失时它走同一个 libuv 接口二次抛错——兜底自己成了崩溃点。
- 数据根由握手带过来（与 `session_tmp` 同一条通路），不猜、不读环境。
- `ensure_placed()` 每次注入前 `stat` 一次，丢了就重写——托管 runtime 重装 Node 会擦掉那块目录。

## 6. 问题五（本次新增）：`--ignore-scripts` 导致子工程依赖缺失

**表现**：eslint 语言服务秒退：
`Cannot find module 'vscode-languageserver/node'`（MODULE_NOT_FOUND）。

**根因链条**：

1. 语言服务包（如 vscode-eslint）的根 `package.json` 里 `postinstall` 是
   `node ./build/bin/all.js install`，负责装 `client/`、`server/` 两个子工程。
2. 守护进程为避免 postinstall 执行未签名 ELF 报 EACCES，在每条 npm 命令里**自己插入**
   `--ignore-scripts`（`DEFER_SCRIPTS`），并在安装结束后改用 `npm rebuild` 补跑。
3. 但 `npm rebuild` **只重跑依赖包自己的脚本，不跑根工程的 postinstall** ——
   于是 `client/`、`server/` 始终没装，服务器一启动就找不到模块。

**现状**：`rewrite_npm_para` 改为**原样透传**客户端命令（不再插入任何参数），
安装期脚本照常执行；未签名 ELF 的问题交给问题六的垫片解决。安装退出码为 0 后只保留一次
`sign_elf::sign_tree(&install_root)`，作为应用侧直接 spawn 的入口（不经垫片）的兜底。

**验证**：eslint 服务器目录下的 `client/node_modules` 11 项、`server/node_modules` 10 项
（含 `vscode-languageserver`）、`server/out/eslintServer.js` 正常产出；npm 日志里
`esbuild@0.25.5 postinstall { code: 0 }`。

## 7. 问题六（本次新增）：安装期执行未签名 ELF

**方案**：垫片接管 `spawn / spawnSync / execFile / execFileSync`，派生前对可执行文件做一次检查：

- 非 ELF、或段表已含 `.codesign`、或路径不在关注范围内 → 直接放行；
- 否则用保存的原始 `spawnSync` 调签名工具 → rename → `chmod 0775`；
- 按路径缓存判定结果（每个路径只查一次），签名工具自身豁免（防递归）。

**关键取舍：签名判据不是平台名。** 早期版本按 `process.platform === 'openharmony'` 决定是否
签名，实测**同一台宿主上不同 Node 构建有报 `openharmony` 也有报 `linux`** 的，按平台名判断
既会漏签也会在纯 Linux 上误签。现在改为：

```js
const host_needs_signing = resolve_program('binary-sign-tool') !== null;
```

即"工具可用就签，不可用就不签"。同一份垫片因此在标准 Linux 上也是安全的：无签名工具时
spawn 完全不被接管、链接是真链接、真实错误原样抛出、探测无残留。这条要求已写进 `shim.js`
文件头：**新增补丁要么有条件，要么先探测能力，绝不能靠"这是被测宿主"来触发**。

**验证**：trace 命中 `signed .../@esbuild/linux-arm64/bin/esbuild`，该 ELF 段表含 `.codesign`；
全树 33 个符号链接 / 10256 个普通文件，仅 1 次签名事件。

> **记录口径已于同日收紧**（主人要求：正常路径不留日志，只保留异常路径）。`shim.js` 的
> `trace()` 现在**只写签名失败**，加载行与 `signed <path>` 成功行均已删除——上面的验证记录
> 是收紧前的取证快照，按现在的版本重跑不会再出现 `signed` 行。想确认签名是否发生，改看
> 文件本身：被签的 ELF 段表里会出现 `.codesign`。

## 8. 问题七（本次新增）：GitHub 下载建连 10 秒超时

**表现**：`downloading release from https://github.com/... : error sending request for url (...):
client error (Connect): operation timed out`，语言服务包反复下载失败。

**定位**：reqwest 错误链三层正好对上——顶层 `error sending request for url` → kind
`client error (Connect)` → 底层 io `operation timed out`，即**建连超时**，不是下载慢。

- 取值：`crates/reqwest_client/src/reqwest_client.rs:36`，原 10 秒，**已改为 30 秒**
  （含 DNS + TCP + TLS，直连 GitHub 时 10 秒偏紧）。所有客户端构造都走这个 `builder()`，
  应用入口在 `crates/zed/src/main.rs:608`。
- 另一处 10 秒是 `crates/http_client/src/github.rs:10` `GITHUB_RELEASE_REQUEST_TIMEOUT`，
  只作用于 `api.github.com` 的**元数据**请求，与 tarball 下载无关。
- 下载体**没有**整体超时：`read_timeout=None`，且 `github_download.rs:57/91` 的
  `get(url, Default::default(), true)` 未挂 `RequestTimeout`。
- **没有重试**：`crates/languages/src/eslint.rs:119` 下载失败直接 `?` 上抛，
  `crates/language` 下无 retry/backoff。

## 9. 问题八（本次新增，未修）：eslint 报 `Unrecognized method eslint/noLibrary`

不是崩溃，是**服务端在抱怨"这个工程里找不到 ESLint 库"**，只是编辑器端没接住这句话。

| 环节 | 位置 | 行为 |
|---|---|---|
| 服务端发请求 | `server/out/eslint.js:943` | `resolveSettings` 失败且 `!settings.silent` 时发 `eslint/noLibrary` |
| 本端无此 handler | `crates/lsp/src/lsp.rs:539` | 回 `-32601 Unrecognized method \`eslint/noLibrary\`` |
| 服务端收到错误响应 | `.../vscode-jsonrpc/lib/common/connection.js:606` | promise 被 reject，且该 `sendRequest` 前加了 `void`，无人 catch |
| 兜底打印 | `server/out/eslintServer.js:103/125` | `process.on('uncaughtException')` 打印，**进程不退出** |

官方客户端在 `client/out/client.js:215` 接这个请求，弹提示 "Failed to load the ESLint library
for the document …" 后 `return {}`。

**里层根因**：工程根 `node_modules` 为空、且无 `eslint.config.*` / `.eslintrc*` ——
ESLint 库是**工程依赖**，语言服务不负责装，VS Code 同理（见 §10）。

**下一个坑**：即便装上库，没有配置文件会接着走 `eslint/noConfig`（同为自定义请求，
官方客户端 `client/out/client.js:203-214` 处理），同样抛 Unrecognized method。

**候选方案**（未实施，待定）：

- A：`crates/languages/src/eslint.rs:253` 的 `"validate": "on"` 改 `"probe"`。
  `server/out/eslint.js:753` 里 `silent` 只在 `validate === probe` 时为真，缺库/缺配置即静默
  （注意不能靠配置项直接关，`684` 行会强制重置 `silent:false`）。
- B：在 lsp 层为 `eslint/noLibrary` 注册 `on_custom_request` 回 null
  （`crates/lsp/src/lsp.rs:1206` 有该接口，注册点需到 `lsp_store` 里找）。

## 10. ESLint 库为什么不自动装

两个东西都叫 eslint，但不是一回事：

| | 是什么 | 谁装 |
|---|---|---|
| `vscode-eslint` | 语言服务器，只做协议翻译 | 编辑器自动下载安装（已装好 3.0.24） |
| `eslint` | 规则引擎本体 | **使用者自己在工程里装** |

服务器不含规则，启动时 `require('eslint')` 去工程里找包。工程用什么版本、什么配置由工程
自己决定，编辑器替塞一个进 `node_modules` 是越界。

要装的话（本机 Node v26.8.1 / npm 11.19.0）：

```
cd /storage/Users/currentUser/workspace/zcoder
/storage/Users/currentUser/.harmonybrew/bin/npm install --save-dev eslint
```

并补一份 `eslint.config.js`。**当前工程的判断是不装**：全工程仅 39 个 JS/TS 文件
（16 个 `.js` 在 `patches/wgpu/` 下，23 个 `.ts` 多为 `Index.d.ts` 与 `hvigorfile.ts`），
为它们在 Rust 工程根塞 `node_modules` 会让工作区索引从 8791 条暴涨，收益接近零。

## 11. 已废弃方案留痕

### 11.1 旧 VM 架构三件套（**整类作废**）

以下三篇的方案都属于"OpenEuler VM 后端"（命令转发到 VM 执行、路径在设备与 VM 间映射）。
后端已切到 QEMU guest + 本地执行，路径语义、进程归属全变了，这些修法**不再适用**，
留档仅供理解历史：

| 原报告 | 原方案 | 废弃原因 |
|---|---|---|
| `2026-08-25-ohos-lsp-dir-relocate.md` | 把 LSP 安装目录从 `$HOME/zed` 迁到 `$HOME/cmd-agent/zed`，改 `spawn.rs` 的 Rule A 硬编码映射 | 数据根不再靠路径映射推导，改由握手下发，统一为 `<数据根>`；VM 侧目录约定作废 |
| `2026-08-25-ohos-lsp-npm-install-path.md` | 补 `--flag=<path>` 等号内联形式的路径映射；给带 `cwd` 的 npm 命令补工作目录 | 同上，路径映射整层随 VM 后端退役 |
| `2026-08-25-ohos-lsp-watchdog-pid.md` | 处理 vscode-languageserver watchdog 的跨机器 PID 失效（客户端 PID 在 VM 上不存在 → 误判父进程已死 → exit 1） | 语言服务现在跑在 guest / 本地，PID 命名空间与客户端一致，watchdog 不再误判 |

### 11.2 `osuser-shim.js` / `passwd_shim.rs` 命名（**已改名**）

垫片最初只为 `os.userInfo()` 而写，因此叫 `osuser-shim.js`、模块叫 `passwd_shim.rs`。
现在它承载了四节补丁（userInfo 兜底、平台名改写、symlink 替身、spawn 前签名），
名字已经名不副实。2026-09-13 统一改名为 `shim.js` / `shim.rs`。

**文件头已写死一条约定**：这是**唯一**垫片，新补丁一律加进这个文件，禁止再拆第二个文件。
理由：一个 `--require` 才好落地、才好同步到设备、才好排查。

### 11.3 启动期写 HNP `conf/`（**已废**）

最初由打包脚本把垫片拷进 HNP 包的 `conf/`、守护进程启动时 `install(conf)`。
失败原因：守护进程启动时**拿不到数据根**（环境变量无人设、HNP 内 conf 与应用记录文件各在一处、
兜底路径层级算错），落点永远失败。改为**复用已有的握手通路**：数据根随 bootstrap 请求送达，
与 `session_tmp` 同源。

走过的另外两层弯路：想用 `HOME` 兜底取目录（那是猜的，且没验证守护进程的 `HOME` 是否等于
数据根，`/proc/<pid>/environ` 被 SELinux 挡住读不了）；想在启动期读 `/etc/passwd` 做 uid 判定
（见 11.6）。

### 11.4 `--ignore-scripts` + `npm rebuild` 后置链（**已废**）

见 §6。废弃原因：跳过脚本的代价是根工程 postinstall 不执行，而 `npm rebuild` 补不回来，
属于"用一个更大的洞去堵一个小洞"。正确做法是不插参数、让脚本照常跑，签名问题在 spawn 前解决。

### 11.5 命令行补 `--bin-links=false`（**已废**）

早期方案是在命令通道给 npm 补 `--bin-links=false`，让 npm 别去建链接。废弃原因：
它把"链接能力"这个**文件系统属性**当成全局常量处理了——实测数据根所在文件系统是支持真链接的，
一刀切禁用会白丢功能。现状改为垫片先试真链接、失败再探测目录能力（见 §4）。
当前 `exec.rs` 里已无任何 `bin-links` 相关代码。

### 11.6 运行期 uid 判定 `needs_preload()`（**已废**）

旧版读 `/etc/passwd` 比对当前 uid，缺失才注入垫片。废弃原因：垫片自己先调真实实现、
正常主机天然 no-op，判定是多余的；无条件注入还省掉了"判定时机与实际注入时机不一致"的隐患。

### 11.7 按平台名决定是否签名（**已废**）

见 §7。判据改为签名工具是否在 PATH 可解析。

## 12. 当前生效的文件清单

| 文件 | 作用 |
|---|---|
| `crates/gpui_ohos/depend/cmd-agent/hicodeerd/shim/shim.js` | 唯一垫片，四节补丁 |
| `crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/shim.rs` | 落盘（`adopt`）、自愈（`ensure_placed`）、注入（`apply`） |
| `.../src/management.rs:103-104` | 握手期触发，与 `session_tmp` 同源 |
| `.../src/exec.rs:415`、`.../src/pty.rs:241` | 两个子进程出口各一行注入 |
| `.../src/exec.rs` | npm 命令原样透传 + 安装后一次 `sign_tree` |
| `crates/http_client/src/github_download.rs:307` | 下载解压的 OHOS 链接兜底 |
| `crates/node_runtime/src/node_runtime.rs:615-623, 728` | npm 入口直指 `npm-cli.js`；解压兜底 |
| `crates/reqwest_client/src/reqwest_client.rs:36` | 建连超时 30 秒 |

## 13. 未决事项

- §9 的 `eslint/noLibrary` 修法未定（倾向方案 A：改 `validate: "probe"`）。
- 下载仍无重试；下载体无整体超时（只放宽了建连）。
- 语言服务缺库/缺配置时，本端统一回 `-32601`，但服务端对该响应的处理是未捕获异常——
  是否要在 lsp 层给未知请求做统一降级（回 `null` 而非 error），属独立议题，尚未评估。

## 关联

- [[2026-09-11-ohos-process-spawn-via-command.md]] —— 进程路由总纲（未合并，通用性更强）
- [[2026-09-13-ohos-language-server-unavailable.md]] —— 语言服务另一条成因链
  （rust-analyzer 五层叠加；TS Server 平台名那段现在由 `shim.js` 的平台名改写统一承担）
- [[2026-09-11-ohos-data-home-directory.md]] —— 数据根与家目录的约定
- 排查手册：`移植记录/zed问题定位手段/Zed提供的问题定位手段.md`
