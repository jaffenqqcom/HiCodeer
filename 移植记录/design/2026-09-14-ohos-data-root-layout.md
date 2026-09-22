# OHOS 数据根布局定义（`data_dir()` 下的数据文件）

> 归档：2026-09-14。本文定义**数据根下每一处文件/目录是什么、谁写的、能不能删**。
> 数据根**落在哪里**由 `2026-09-10-ohos-data-home-directory-design.md` 决定，本文不重复；
> 本文只描述其**内部布局**。

## 1. 数据根是什么

运行期数据根 = 用户选定的目录 `A` 下的 `HiCodeer`，即 `data_dir()` 的返回值
（`crates/paths/src/paths.rs:144`；OHOS 启动时经 `set_custom_data_dir` 定死，:103）。

设备上实测值：`/storage/Users/currentUser/HiCodeer`。

一个关键性质：**该目录对三方同时可见** —— 宿主应用（uid 20020220）、守护进程
（uid 20020117）、QEMU guest（静态挂载）。所以三方都往里写，本文按**写入方**分区。

## 2. 应用侧（`paths` 定义，`crates/paths/src/paths.rs`）

| 路径 | 定义处 | 内容 | 说明 |
|---|---|---|---|
| `config/` | `config_dir()` :122 | `settings.json`（用户设置）、`themes/` | 用户配置。按需还会生成 `keymap.json`、`tasks.json`、`debug.json`、`AGENTS.md`、`snippets/`、`settings_backup.json`、`keymap_backup.json` |
| `cache/` | `temp_dir()` :193 | 应用缓存 | OHOS 分支特判为 `data_dir()/cache`（非 macOS 走 XDG 缓存的那套在这里不适用） |
| `db/` | `database_dir()` :262 | SQLite 库 | `0-dev/`（项目本地库）、`0-global/`（全局库） |
| `threads/` | `agent/src/db.rs:438` | `threads.db` | agent 会话线程库 |
| `extensions/` | `extensions_dir()` :352 | 已装扩展 | `index.json` + `installed/`、`staging/`、`work/` |
| `languages/` | `languages_dir()` :445 | 内置语言服务 | 下载解包目录，每服务一个子目录（`vtsls/`、`eslint/`、`tailwindcss-language-server/` …） |
| `logs/` | `logs_dir()` :232 | 应用日志 | `telemetry.log`；`<AppName>.log` / `.log.old` 按需生成 |
| `prompts/` | `prompts_dir()` :390 | Assistant 提示库 | `prompts-library-db.0.mdb` |
| `external_agents/` | `external_agents_dir()` :461 | 外部 agent 服务 | 下载目录，含 `registry/` |
| `debug_adapters/` | `debug_adapters_dir()` :453 | DAP 适配器 | 下载目录 |
| `hang_traces/` | `hang_traces_dir()` :226 | 卡顿追踪 | 按需写入 |

按需创建、当前设备上尚未出现的（均为 `data_dir()` 下）：`embeddings/`（:431）、
`copilot/`（:467）、`prettier/`（:473）、`remote_extensions/`（:360）、
`remote_servers/`（:479）、`server_state/`（:244）、`devcontainer/`（:485）、
`prompt_overrides/`（:408）。

## 3. 受管 Node 工具链（应用建、三方用）

| 路径 | 定义处 | 内容 | 说明 |
|---|---|---|---|
| `node/` | `node_runtime.rs:643` | 受管 Node 容器 | 语言服务与 agent 都不用系统 node，用这份 |
| `node/<dist>/` | `node_runtime.rs:644` | Node 发行版解包目录 | 形如 `node-v24.11.0-linux-arm64/`，内含 `bin/node`、`bin/npm`、`lib/node_modules/npm/` |
| `node/cache/` | `node_runtime.rs:654` `:916` | npm 包缓存与日志 | `_cacache/`、`_logs/`、`_update-notifier-last-checked`；Node 重装时整目录删除重建 |

> `node/` 下的一切都会在 Node 重装时被整目录删除重建，所以守护进程写入的东西一律放在 `node/` **之外**（见下一节）。

## 4. 守护进程侧（`hicodeerd`，uid 20020117）

| 路径 | 定义处 | 内容 | 说明 |
|---|---|---|---|
| `tmp/` | `session_tmp.rs:38` | 子进程临时目录 | 守护进程按客户端身份把它记为 `TMPDIR`（`session_tmp.rs:60` 的 `adopt` 建档，spawn 点用 `tmpdir(client_id)` 取出后交给子进程；不写本进程自己的 env，避免多个客户端互相顶掉）。第三方工具的实际产物：`node-compile-cache/`（Node 编译缓存）、`vscode-typescript<N>/`（TS 服务临时文件）等 |
| `tmp/shim-trace.log` | `hicodeerd/shim/shim.js` | 垫片诊断轨迹 | 写在 `$TMPDIR` 下，即本目录。**仅**在"需要签名的宿主"上、且发生异常时写（`shim.js:376` 的 `trace`）；正常路径不落盘 |
| `logs/hicodeerd.log` | `logger.rs`（`attach_file`） | 守护进程日志镜像 | **仅 `--log` 启动时**创建。内容与 hilog 同源，多一列本地时间（`MM-DD HH:MM:SS 级别 消息`），便于事后读取 |

守护进程**不写**应用那些目录，只写 `tmp/`、`logs/` 两处。垫片不在此列：它们随
daemon 的 HNP 包分发，装在包内 `<pkg>/shim/`，见第 6 节。

## 5. 可清理性

| 删除对象 | 后果 |
|---|---|
| `tmp/` | 无副作用，下次启动重建；仅丢失在跑的临时文件 |
| `logs/hicodeerd.log` | 无副作用（`--log` 时重建） |
| `node/cache/` | 仅丢 npm 缓存，下次安装重新下载 |
| `node/<dist>/` | Node 运行时丢失，下次启动重新下载解包 |
| `languages/` | 语言服务丢失，打开对应文件时按需重下（`eslint.rs:116` 一类逻辑会整目录清空重下） |
| `cache/` | 无副作用 |
| `config/`、`db/`、`threads/`、`prompts/` | **用户数据**，删了设置/会话/提示库即丢失 |
| `extensions/`、`external_agents/`、`debug_adapters/` | 已装扩展与 agent 丢失，需重装 |

整体删除整个数据根 = 恢复出厂：设置、会话、扩展、语言服务、Node 运行时全部重来。

## 6. 与本文件的关系

- 数据根**位置**的决策：`2026-09-10-ohos-data-home-directory-design.md`
- 语言服务/Node 工具链问题的成因链：`../../bugfix/2026-09-14-ohos-node-npm-toolchain.md`
- 本文改动（2026-09-14）：新增 `logs/hicodeerd.log`；`tmp/shim-trace.log` 的写入条件收紧为
  「仅在需要签名的宿主上、异常路径」。
- 本文改动（2026-09-16）：垫片**不再落在数据根**。两个垫片（Node 的 `shim.js`、musl libc
  pthread key 的 `musllib-shim.so`）改为随 daemon 的 HNP 包分发：打包时进 `<pkg>/shim/`，
  由系统安装，运行时 daemon 从自身 `/proc/self/exe` 推出的包根取用，并进程级导出
  `NODE_OPTIONS` / `LD_PRELOAD` 供所有子进程继承（见 `hicodeerd/src/shim.rs`）。理由：
  数据根是用户可见、用户可删的目录，垫片放那里既可能被误删，也让 daemon 依赖对它的写权限；
  包内载荷由系统整体安装与替换，无运行时写入。因此上文原先记录的 `node/shim/` 已不存在。
- 本文改动（2026-09-16）：TLS 垫片改称 `musllib-shim`（它垫的是 musl libc 的 pthread key
  上限，`ohos-tls` 名不副实）。源码入库于 `hicodeerd/shim/musllib-shim.c`，
  `script/bundle-ohos` 用 OHOS SDK clang 编出同目录的 `musllib-shim.so`；改 `.c` 即自动重编
  并同步刷新该产物（脚本 SHIM 段）。`script/ohos-tls-shim.c` 保留为 warp-ohos 上游
  （`warp/script/ohos/ohos-tls-shim.c`）的逐字节镜像副本，仅供对比，不再参与编译。
