# zcoder / Zed 问题定位手段（速查手册）

> **目的**：出了问题先查这里，不用每次翻代码。
> **用法**：先看 §0 速查表定位到对应小节，照抄命令/入口即可。
> **维护**：每发现一个可复用的定位手段，就往对应小节加一条，并在 §0 登记一行。
> **建文**：2026-09-11（由 codebuddy 登录/refusal 排查过程沉淀）。

---

## 0. 速查表：现象 → 手段

| 现象 | 首选手段 | 去 |
|---|---|---|
| agent（codebuddy / claude-acp）拒绝回答、不弹登录、行为诡异 | **ACP 报文面板** | §1 |
| 面板导出里"少了"某条报文（如没有 `session/prompt`） | 先做**快照自检**（尾部是不是停在发消息之前 / 连接选错） | §1 |
| zcoder UI 行为异常 / 功能静默失效 | 应用日志（hilog） | §2 |
| 子进程起不来、命令不执行、stdin 打不进去 | zcoderd 日志 | §3 |
| 不确定进程活没活、端口通不通 | 进程 / 端口探针 | §4 |
| 改的代码到底进没进设备 | 产物级字符串校验 | §5 |
| 找不到调用点、"搜不到" | 代码考古（`psrch.py`） | §6 |
| 要单独观察 agent 自身行为（登录态、原始协议帧） | ACP 直连探测 | §7 |
| 编译 / 装机 | `bundle-ohos` + skills | §8 |
| 想知道某功能代码在哪 | 代码坐标速查 | §9 |
| Node 程序（agent 等）报 `uv_os_get_passwd` / `os.userInfo` 失败，或 agent 莫名"拒绝回答" | OHOS 应用 uid 不在 `/etc/passwd` | §10 |
| 诡异现象先排除环境因素 | 环境与路径陷阱 | §10 |

---

## 1. ACP 报文面板（排查 agent 类问题第一手段）

### 是什么

zcoder 内置的"协议黑匣子"：记录 agent ↔ 客户端**全部进出 JSON-RPC 报文**（含错误响应），以 markdown 渲染完整 JSON。**不用改码、不用猜、不用抓日志**，是查 agent 行为最直接的证据来源。

### 打开方法

1. 菜单 **Go → Command Palette**（命令面板）
2. 输入 `Acp Logs`，回车

`dev` 命名空间**没有被任何 `hide_namespace` 过滤**（全仓只隐藏了 agent / agents / assistant / copilot / edit_prediction / editor / vim / repl），所以搜得到。

代码位置：
- 动作注册：`crates/acp_tools/src/acp_tools.rs:27` `actions!(dev, [OpenAcpLogs])`；`init()`（:29）在每个 Workspace 注册
- 报文采集：`crates/agent_servers/src/acp.rs:113-168` `AcpDebugLog`（含 error 响应的解析）

### 报文条目格式

每条自带三个描述字段：

| 字段 | 值 | 含义 |
|---|---|---|
| `_direction` | `outgoing` / `incoming` | 谁发的 |
| `_type` | `request` / `response` / `notification` | 消息类别（通知的 `id` 为 null） |
| `id` / `method` / `params` | — | 原样协议字段 |

### 怎么完整导出（关键：别导出成"半个快照"）

面板顶部工具栏（`acp_tools.rs:707-757`）从左到右四件事：

| 控件 | 名称 | 作用 |
|---|---|---|
| 下拉框 `acp-connection-selector` | 连接选择器 | **一次只显示一个连接**的报文；多 agent 时先在这里选对（如 `codebuddy-code`） |
| ↻ `restart_connection` | Restart Connection | 重启该连接（报文流会重来） |
| 复制 `copy-all-messages` | **Copy All Messages** | 导出**当前选中连接**的全部报文（pretty JSON）——就是我们要的证据 |
| 🗑 `clear_messages` | Clear Messages | **清空报文列表**，别手滑 |

导出实现 `serialize_observed_messages()`（`acp_tools.rs:344-371`）：把 `connection.messages` 逐条序列化成 `_direction/_type/id/method/params`。即**导出是忠实的、完整的**（对该连接而言）。

✅ **自检：这份导出是不是"发完消息之后"的快照？**
- 尾部必须是**你那条消息的 `session/prompt`（outgoing request）+ 紧跟的 response**；
- 若尾部仍停在 `session/new` response / `session/update` 通知上 ⇒ 这份快照**早于你发消息**（或选错连接），不能用它分析"发消息后的行为"。

⚠️ 历史回放有上限：面板自身列表无上限，但"打开面板时的历史回放"来自环形缓冲，上限 `MAX_DEBUG_BACKLOG_MESSAGES = 2000`（`acp.rs:54`），超出**丢最旧的**（`pop_front`，`acp.rs:215`）。⇒ **先开面板、再复现**，最稳。

✅ **导出被"截断"时：用 Clear 造一份最小快照（2026-09-11 实战踩坑）**

面板按**时序**追加，`session/prompt` 永远在**最末尾**。把整份导出贴进聊天时，**尾部先被字数上限砍掉** ⇒ 现象就是"明明复现了 refusal，导出里却只有 `session/new` + 一堆 `session/update`，没有 prompt"，极容易误判成"客户端没发 prompt"。

零改码解法——**先清、后发**：

1. 面板里用连接选择器选对连接（如 `codebuddy-code`）→ 点 🗑 `Clear Messages`
   - 它**只清面板镜像** `connection.messages`（`acp_tools.rs:373-382`），**不动**后台 `AcpDebugLog` 的 backlog，安全
2. 切回 agent 面板发一条消息，等出结果（如 refusal 文案浮现）
3. 切回面板 → 此时列表**只含本轮新报文**（`session/prompt` outgoing request + 紧跟的 response，外加少量 `session/update`）→ `Copy All Messages`

这样导出极短，不会被截断。（不走 Clear 的话，导出后**只贴 `session/prompt` 的 request + response 两条**也够用。）

### 实战价值（本手册就是它挣出来的）

- 看 `initialize` 返回的 `authMethods` / `agentCapabilities`
- 看 `session/new` 是**成功**还是 `{"code":-32000,"message":"Authentication required","data":{"category":"auth"}}` → **直接判定 agent 的登录态**
- 看 `session/prompt` 响应的 **`stopReason`** 与 **`_meta`** → agent 把真因藏在 `_meta` 时，这是唯一取证口
- 看 agent 发来的**私有反向往通知**（例：`_codebuddy.ai/command`、`_codebuddy.ai/authUrl`）→ 判断某个通知"到底发没发"
- 看**报文时序**（例：`session/update` 可能早于它所属的 `session/new` response 到达 → 解释 `unknown session` 类警告）

### 坑

- 面板在工具栏注册但默认 `Hidden`，**别从工具栏找，用命令面板**
- 只记 ACP 报文，**不含 zcoderd 侧的 exec/pty 日志**（那看 §3）
- 会话很长时注意滚动/过滤，关键报文可能在底部
- 导出**只覆盖一个连接**；连接被重建（重启/换 agent）时面板会切到新连接，旧连接报文不再出现在新导出里
- agent 的 stderr 会以 `_direction: "stderr"` 混在里面（`acp.rs:932-947`）；codebuddy 在 ACP 模式下**不写 stderr**，别指望它

### 反向用法：用"有没有某条报文"来证明"没发生"

`session/prompt` 在不在，是判断"客户端到底把消息发出去没有"的硬证据。同类判据：

| 想证明 | 看什么 |
|---|---|
| agent 有没有推某个私有通知（如 `_codebuddy.ai/authUrl`） | 面板里有没有该 method 的条目 |
| 客户端有没有把 prompt 发出去 | 有没有 outgoing `session/prompt` |
| agent 认为自己"已登录"吗 | `session/new` 是成功（带 models）还是 `{"code":-32000,"message":"Authentication required","data":{"category":"auth"}}` |

⚠️ zcoder 的 refusal 文案（"…refused to respond to this prompt…"）**只有唯一产生路径**：`PromptResponse.stop_reason == "refusal"`（`acp_thread.rs:3832` → `AcpThreadEvent::Refusal` → `conversation_view.rs:1721` → `thread_view.rs:1873` / 函数体 `:11137`）。所以：**看到那段文案 ⇒ 必然发生过一次成功的 prompt 往返**；若面板里找不到 prompt，那是**快照/连接选错**，不是"没发生"。

---

## 2. 应用日志（hilog）

### 通道

`log::info!` / `warn!` / `error!` → zlog logger → `OH_LOG_Print`，tag = `A00001/com.zcoder.studio/Zcoder`。

代码：`crates/zlog/src/zlog.rs:22` `log::set_logger(&ZLOG)`。

⚠️ OHOS 上 zcoder **故意禁用了文件日志**（`crates/zed/src/main.rs` 的 OHOS 分支只发一行 boot 确认，不走 `init_output_file`）⇒ 应用日志**只有 hilog 一条路**（历史上曾尝试落盘，改动已回退）。

### 抓法（顺序不能错）

1. `hilog` 缓冲**很小，约 40 秒就滚掉** ⇒ 必须「**清缓冲 → 立刻复现 → 立刻抓**」
2. 用现成脚本（会自动清缓冲）：

```bash
python3 ~/.workbuddy/skills/zcoder-logs/scripts/grab_via_vm.py <抓取秒数> <时间偏移>
# 例：抓 180 秒
python3 ~/.workbuddy/skills/zcoder-logs/scripts/grab_via_vm.py 180 0
```

3. **⛔ 抓取期间绝对不能跑 `hdc kill`** —— 它会掐断正在跑的 `hdc shell hilog` 流，抓取文件被提前截断（曾因此废掉一次 600s 抓取，只剩系统 tag、零 `Zcoder:` 行）。要并发查进程就用另一条不带 kill 的 hdc 命令，或等抓完。

### 注意

- **release 构建下 `debug` 级日志不出现**；要留证据必须用 `info` / `warn` / `error`
- **抓取产物不要写进工作区**（fs 事件 → git 刷新 → 更多日志，CPU 风暴约 8-12 次/秒）；写到 `.workbuddy/tmp/` 之外或 VM `/tmp`
- `agent_servers/src/acp.rs` 原本 0 处 `log::xxx!`（该 crate 惯用别的通道），在里面加日志是有效但"非常规"的手段

---

## 3. zcoderd 日志

### 前提：必须带 `--log`

`zcoderd/src/logger.rs`：只有 `init(true)`（即带 `--log`）才安装 logger。**不带 `--log` 时所有 `log::xxx!` 是空操作，日志全丢。**

带 `--log` 时 OHOS 走 `OH_LOG_Print`（domain `0x0001`、tag `Zcoderd`）。

⚠️ **但实测经 hdc 抓 hilog 抓不到 `Zcoderd` tag**（试过 `hilog -x` / `-D 0x0001` / `-T Zcoderd`，匹配数均为 0）⇒ 拿 zcoderd 日志的**可靠办法是把它 stdout 重定向到共享盘**。

### 启动方式（在能读 HNP 的设备侧终端里，uid 20020201）

```sh
killall zcoderd
setsid /data/service/hnp/bin/zcoderd --log </dev/null \
  >/storage/Users/currentUser/zcoderd-dbg.log 2>&1 &
```

- **⛔ 别用 `pkill -f zcoderd`** —— 会匹配到自己的命令行而自杀。用 `killall zcoderd` 或 `pkill -x zcoderd`
- **⛔ 日志别写进工作区**（会自激，见 §10）
- `/storage/Users/currentUser/` 与 VM `/mnt/linux_share/` 同盘，写这里本机/VM 都能直接读

### exec 链路三级判据（stdin 打不进去时）

| 日志行 | 含义 |
|---|---|
| `exec: forwarded N bytes stdin to channel=X` | stdin 已到达 zcoderd（问题在下游） |
| `exec: no live child for stdin channel=X` | children 表里没有这个 channel（键不匹配） |
| 完全没有 `forward_stdin` 记录 | zcoder 侧没写 stdin / SSH data 没投递 |

配套行：`exec: spawned session=N pid=…` / `exec: registered session=N pgid=…` / `exec: unregistered session=N`。

⚠️ 若 `registered session` 晚于 `data … bytes`，那批 stdin 会被静默丢弃（`exec.rs` 约 :110）。

---

## 4. 进程与端口探针

### 看进程

```sh
hdc shell ps -ef | grep -E "zcoder|codebuddy|zcoderd"
```

hdc shell 是 uid 2000：**能看到（跨 uid）但杀不掉**，也读不了 HNP 文件。

### uid 三分（读日志/判归属必备）

| uid | 归属 |
|---|---|
| `20020201` | WorkBuddy 桌面应用；也是**能读 HNP 的设备侧 shell** |
| `20020117` | zcoderd + 它拉起的 node / codebuddy |
| `20020217` | zcoder 应用本体 |

⚠️ 同一路径字符串在不同 uid 的 mount namespace 里指向**不同物理目录**（`/data/storage/el2/...` 是 per-uid bind mount）⇒ 读日志务必先确认归属。**本项目踩过**：把 WorkBuddy 的日志当成 codebuddy 的，得出错误结论。

### 端口探针（本机就能判 zcoderd 活没活）

```python
import socket
for port in (4022, 4023):
    sock = socket.socket(); sock.settimeout(2)
    print(port, sock.connect_ex(("127.0.0.1", port)))
```

- `4022` = 命令口，`4023` = 管理口；连上后 banner = `SSH-2.0-russh_0.55.0`
- 手起第二个 zcoderd 实例会报 `zcoderd fatal: Address in use (os error 98)`

### 清理孤儿进程（安全写法）

```sh
ps -ef | grep '[c]odebuddy --acp' | awk '{print $2}' | xargs -r kill
```

---

## 5. 产物级字符串校验（改动到底进没进设备）

"编译成功" ≠ "新代码进了设备上跑的那份 so"。装完包后做一次校验：

```sh
cd <工程根>
unzip -o -q hap/entry/build/default/outputs/default/entry-default-signed.hap \
  'libs/arm64-v8a/*' -d /tmp/hapx
strings /tmp/hapx/libs/arm64-v8a/libzcoder.so | grep -E '<你新加的字符串>'
```

**实战用例**：验证"删掉旧硬编码 + 加新规则表"两者都进了包 —— 新串命中、旧串计数 0。

**适用**：新增的日志文案、错误消息、常量字符串。
**失效**：只改逻辑、没有独有字符串时（改用 §1 / §2）。

---

## 6. 代码考古（⛔ 本仓 grep 会骗你）

### 最重要的一条

本仓 `grep -rn "<pat>" crates …` **会静默失效** —— 同一个文件 `grep` 无命中，而 `sed` / Read 明明有内容（在 `acp.rs`、`elicitation.rs` 多次复现）。Grep 工具（ripgrep）对 `crates/gpui_ohos/depend/**` 与 `crates/agent_servers/src/acp.rs` 也返回空。

⇒ **凡"零命中 / 不存在"这类结论，必须用本目录的 `psrch.py` 或 Read 复核。**
  本项目已因此误判过一次（"URL 形态在 zcoder 里根本不存在" → 实际全链路早就有了）。

### 用 psrch.py

```sh
cd <工程根>
# 基本用法
python3 移植记录/zed问题定位手段/psrch.py crates/agent_servers "authUrl|_codebuddy"

# 要搜 depend/ 子目录时加 --depend（默认跳过）
python3 移植记录/zed问题定位手段/psrch.py crates/gpui_ohos/depend/cmd-agent "forward_stdin"

# 限定后缀 / 限制输出
python3 移植记录/zed问题定位手段/psrch.py crates/agent_ui --ext rs,toml --max 300
```

- 默认跳过 `target/ node_modules/ .git/ build/ oh_modules/ .hvigor/` 与任何 `depend/` 目录
- 输出格式 `路径:行号: 内容`，末尾给命中总数
- ⚠️ 位置参数 **只接受一个 root**（多给会报 `unrecognized arguments`）；要搜多处就跑多次，或提高 root 层级
- 正则用 **ERE** 写法（`|` 交替、`\s` 等直接写，勿加 `\|`）

### 其他考古坑

- bash grep 在 toybox 下**必须用 `-E`**；用 BRE 的 `\|` 交替会静默返回空
- 别依赖"我记得搜过了"——一律 `psrch.py` 复核

---

## 7. ACP 直连探测（绕开 zcoder，直接问 agent）

skill：`codebuddy-acp-probe`（`~/.workbuddy/skills/codebuddy-acp-probe/`）

```sh
S=~/.workbuddy/skills/codebuddy-acp-probe/scripts/probe_acp.py
python3 $S init            # 只发 initialize，看 authMethods / capabilities（默认）
python3 $S new             # + session/new
python3 $S prompt 300      # + session/prompt("hello")，超时 300s

# 换 HOME 做 A/B 对照
PROBE_HOME=<空目录>            python3 $S prompt 90
PROBE_HOME=/storage/Users/currentUser python3 $S prompt 90
```

**它证明过什么**：codebuddy 的登录态完全由 `HOME` 决定 —— 空 HOME 时 `session/new` **立即**回 `-32000 Authentication required`；真实 HOME 时长时间不返回（在做认证握手）。

**附带能力**：会读 codebuddy 自己的日志 `$HOME/.codebuddy/logs/<date>/`。

**坑**：
- 收到 **SIGKILL 会丢缓冲日志**（脚本先 `terminate()` 再等）
- 已有登录态时 `session/new` 可能**长时间不返回**，属正常，不是卡死
- 用真实凭据（`CODEBUDDY_AUTH_TOKEN`）前**须经许可**

---

## 8. 编译与装机

### 编译（唯一合法方式）

```sh
cd <工程根> && ./script/bundle-ohos          # 默认 debug
./script/bundle-ohos --release               # release
```

- **禁止手拼 cargo / 手动 export 环境变量**（本机无 cargo/rustc）
- 本机与 VM `172.16.105.2` **共享磁盘**（`/storage/Users/currentUser/` == VM `/mnt/linux_share/`），编译实际在 VM 上跑
- skill：`zcoder-build`

### 装机

```sh
python3 ~/.workbuddy/skills/zcoder-install/scripts/install_via_vm.py
```

- 权威入口是**工程根 `install_run.sh`**（force-stop → `hdc install -r` → `aa start`），skill 只是连接设备并跑它；**不要手搓安装脚本**
- 产物：`hap/entry/build/default/outputs/default/entry-default-signed.hap`（约 626MB，推送耗时，放后台跑）
- skill：`zcoder-install`

### 装完必做

1. §5 产物字符串校验
2. 调试前按 §3 **重启 zcoderd（带 `--log`）**

---

## 9. 代码坐标速查

### agent / ACP

| 事项 | 位置 |
|---|---|
| 生产连接建立 | `crates/agent_servers/src/acp.rs` `connect_client_future`（约 :684） |
| 通知处理器注册（2 个） | `acp.rs:749-758` `handle_session_notification` / `handle_complete_elicitation` |
| 兜底通知（任意 method，OHOS） | `acp.rs` `handle_ext_notification` + `EXT_URL_ROUTES` 规则表 |
| 会话通知路由（**unknown session 丢弃点**） | `acp.rs:4938-4961` |
| `session/new` 错误映射 | `acp.rs:1636-1639` → `map_acp_error`（约 :2093） |
| URL elicitation 能力声明 | `acp.rs:807-811` |
| URL elicitation 接收 | `acp.rs:749` → 实现约 `acp.rs:4652` |
| elicitation 校验 / 存储 | `crates/acp_thread/src/acp_thread.rs:443-455`；`request_elicitation_with_id`（:593） |
| URL 卡片 UI | `crates/agent_ui/src/conversation_view/elicitation.rs:1876` `render_url_elicitation` |
| 打开 URL 出口 | `conversation_view.rs:2454`、`conversation_view/thread_view.rs:6654` |
| 认证状态 / 登录 UI | `conversation_view.rs` `handle_auth_required`（约 :1150）；登录按钮渲染 :2246-2341 |
| **Refusal → 误导文案** | `acp_thread.rs:3832`（唯一发出点，emit 在 :3851/3859/3863）→ `conversation_view.rs:1720-1721` → `thread_view.rs:11028`（分派）→ `render_refusal_error`（:11137，文案 :11140） |
| **AuthRequired → 登录按钮** | `thread_view.rs:11032`（分派）→ `render_authentication_required_error`（:11155，`authenticate_button` :11168） |
| ACP 报文面板 | `crates/acp_tools/src/acp_tools.rs:27` |
| 报文采集 | `acp.rs:113-168` `AcpDebugLog` |

### 进程 / 命令

| 事项 | 位置 |
|---|---|
| 进程创建唯一合法路径 | `crates/util/src/command/ohos.rs` `Command::spawn`（本地 HNP 用 fork，其余发 ExecSpec 给 zcoderd） |
| `Child::spawn`（stdio 入口） | `crates/util/src/command/ohos.rs` |
| ACP 子进程 spawn 点 | `crates/agent_servers/src/acp.rs:850-870` |
| agent 环境构造 | `crates/project/src/agent_server_store.rs:1379-1398` |
| OHOS 环境捕获（= 进程自身 env） | `crates/util/src/shell_env.rs:47-54` |
| **子进程 HOME 被改写** | `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs:44-54` |
| zcoderd pty 表（键 `(conn_id, ChannelId)`） | `crates/gpui_ohos/depend/cmd-agent/zcoderd/src/pty.rs:45` |
| zcoderd exec stdin 转发 | 同上 `sshd.rs:127`、`exec.rs` 约 :104 |

### 日志 / 数据目录

| 事项 | 位置 |
|---|---|
| zcoder 应用 logger | `crates/zlog/src/zlog.rs:22` |
| OHOS 日志初始化（禁文件日志） | `crates/zed/src/main.rs` 的 OHOS 分支 |
| zcoderd logger（须 `--log`） | `crates/gpui_ohos/depend/cmd-agent/zcoderd/src/logger.rs` |
| data_dir 设置 | `crates/paths/src/paths.rs:103-119` |

---

## 10. 环境与路径陷阱

- **本机 ↔ VM 同盘**：`/storage/Users/currentUser/` == VM `/mnt/linux_share/`，改动实时可见，无需同步。VM 登录 `ssh user@172.16.105.2`；OHOS 无 sshpass ⇒ 用 pty 驱动 ssh 的脚本（如 `.workbuddy/tmp/vm_run.py`）
- **共享盘上的 `.codebuddy/` 与 `.workbuddy/` 都归 WorkBuddy**，不是设备侧 codebuddy 的 ⇒ 拿它们推断设备侧登录态会得出**错误结论**（本项目踩过）。设备侧子进程的 `$HOME/.codebuddy` 在它自己的 mount namespace 里，外部读不到
- **`/data/storage/el2/...` 是 per-uid bind mount**：同一字符串不同 uid 指向不同目录
- **抓取产物 / 日志不要写进工作区**：fs 事件 → git 刷新 → 更多日志 → CPU 风暴（实测约 8-12 次 git/秒）。写到 `.workbuddy/tmp/` 之外或 VM `/tmp`
- **设备时间与本机可能有偏差**（曾观测到快约 9 分钟）⇒ 对时间轴前先核对
- **HNP 二进制在 `/data/service/hnp/bin/`**，只在能读 HNP 的 uid 命名空间可见；hdc（uid 2000）会报 `No such file`，是**假象**
- **`rm` 被拦** ⇒ `python3 -c "import os; os.remove(p)"`
- git 在 `/storage/Users/currentUser/.harmonybrew/bin/git`（**设备侧 agent 进程的 PATH 里没有 git**，`execSync("git ...")` 一律失败 —— 这一点会改变某些程序的分支走向，见下条）
- ⚠️ **OHOS 上任何 Node 程序的 `os.userInfo()` 必炸**（2026-09-11 实锤）：应用/服务 uid **不在 `/etc/passwd`** 里（该文件只有 root/bin/system/file_manager… 等系统账号），`id <appuid>` 直接报 `bad uid`；libuv 的 `uv_os_get_passwd`（即 `getpwuid_r`）拿不到条目 → Node 抛 `ERR_SYSTEM_ERROR: A system error occurred: uv_os_get_passwd returned ENOENT`。
  - **本机一行复现**：`<node> -e "require('os').userInfo()"`（本机 uid 20020201 同样中招）
  - `os.homedir()` **通常不受影响**（优先读 `HOME` 环境变量）；但 **`HOME` 缺失时它会掉进同一个 `uv_os_homedir` → ENOENT**（本机 `env -i` 实测），所以任何兜底代码自己也不能裸调它 —— 别混淆
- 排查 agent 类问题时，**看到 `uv_os_get_passwd` 就不要往"登录/鉴权/内容政策"方向想**，直接看调用方的兜底逻辑。已知实例（codebuddy `dist-server/codebuddy.js`）：
  - `getTeamUserId(){ let i=process.env.CODEBUDDY_USER_ID; if(i) return i; let n=await settings.get("memory"); if(n?.teamMemory?.userId) return n.teamMemory.userId; let c=GitUtils.getGitUserName(); return c||(await import(70857)).userInfo().username }` —— **第 ③ 条无 try**，且 `getMemoryDir()` **无条件 await** 它 ⇒ 每次请求构造上下文时必炸 ⇒ 被它自己的 catch 包成 `stopReason:"refusal"` + `_meta["codebuddy.ai/errorMessage"]` ⇒ zcoder 又只读 `stop_reason` ⇒ UI 显示成误导性的"违反内容政策"
  - 两个短路口子（让第 ③ 条永不执行）：**环境变量 `CODEBUDDY_USER_ID=<任意非空>`**，或**设置项 `memory.teamMemory.userId`**（`DEFAULT_SCOPES=[USER,PROJECT,PROJECT_LOCAL]`，项目级文件路径 = `<workDir>/.codebuddy/settings.json`）
  - **这个值填什么**：全仓只有 `getMemoryDir()` 一个消费者（`getTeamUserId()` 仅 1 处调用），且**仅当 team memory 启用**时才被拼成目录名 —— `getTeamMemoryDir(id) = join(getProjectHomeDir(), "memories", "@"+id)`（@5669973）。**它不是凭证、不上报、不参与鉴权**，填任意非空合法目录名即可（别含 `/`）。设备侧 team memory 默认未启用（`isTeamMemoryEnabled()` 要求显式 `memory.teamMemory.enabled=true`，env `CODEBUDDY_TEAM_MEMORY_ENABLED` 可覆盖）⇒ 该值实际只起"短路开关"作用，填 `zcoder` 这种标识即可。
  - **为什么"发 hello 必炸"（包内反查）**：`shouldInjectMemoryContext(input)` 里 `1===input.filter(m=>m.role==="user").length` —— 只要**只有一条 user 消息**就返回 true ⇒ **每个新会话的首条 prompt 都必然注入 memory 上下文** ⇒ 必然走 `getMemoryDir()` ⇒ 必然炸。是确定性，不是偶发。
  - ✅ **离线实证（本机复刻，不需要登录态，秒出结果）**：把 `getTeamUserId()` 的分支逻辑照抄成 Node 脚本跑 —— 无干预 ⇒ 抛 `ERR_SYSTEM_ERROR` / `A system error occurred: uv_os_get_passwd returned ENOENT (no such file or directory)`，**与设备 refusal 报文 `_meta["codebuddy.ai/errorMessage"]` 里的字符串逐字一致**；加 `CODEBUDDY_USER_ID=zcoder` ⇒ 命中第①条、不抛。（模板脚本曾放 `/storage/Users/currentUser/.workbuddy/tmp/chain-probe.js`，已随过程文件清理；照抄 `getTeamUserId()` 的三条分支即可重建，注意先用 `env -i` 隔离）
  - 另一条同源开关：**`CODEBUDDY_DISABLE_AUTO_MEMORY=1`** —— 从源头关掉 auto memory。**腾讯自家 WorkBuddy 就是这么绕的**：codebuddy 被当 SDK 内嵌时（`CODEBUDDY_LIBRARY_RUNTIME=1`）会自动 `??=` 赋上这组 `DISABLE_*` 兜底（`dist-server/codebuddy.js` @4018280），而 zcoder 的 `--acp` 启动**不走这个分支** —— 所以只有 zcoder 中招。取舍：`CODEBUDDY_USER_ID` 保留 memory 功能；`CODEBUDDY_DISABLE_AUTO_MEMORY` 更彻底但丢功能。
  - 🧯 **通用兜底（不限 codebuddy）已落盘（2026-09-12 改版）：守护进程在 spawn 子进程时注入 `NODE_OPTIONS="--require <数据根>/node/shim/osuser-shim.js"`**，shim 内 hook `os.userInfo()`（先调真实实现，仅在 `ERR_SYSTEM_ERROR` 时返回合成值）。**本机实证（2026-09-11）**：裸调 ⇒ `THROW ERR_SYSTEM_ERROR`；带 shim ⇒ `OK`，且 `homedir/platform/tmpdir` 等其它 API 不受影响；node v24.13.0 接受 NODE_OPTIONS 里的 `--require`。因为 zcoderd 是所有 agent 的父进程、其 spawn 走 `sh -c` 且**不清环境**，一处注入即覆盖全部 Node ACP agent。
    - **实现（2026-09-12 改版）**：`crates/gpui_ohos/depend/cmd-agent/hicodeerd/src/passwd_shim.rs`。垫片脚本用 `include_str!` **编进守护进程二进制**；`install(&conf)` 在启动时定位数据根、把脚本**对账写盘**到 `<数据根>/node/shim/osuser-shim.js`，`NODE_OPTIONS` 指向该路径。`script/bundle-ohos` **不再**把脚本拷进 HNP `conf/`。**落点为什么选数据根**：它是宿主应用与 guest **同名同址**可见的位置（数据根经第二个 virtiofs share 挂到同名路径），因此守护进程无论跑在宿主侧还是 guest 侧，都读得到同一份 —— 老实现把副本放进 HNP 包，guest 里那份压根不存在，属漏投。
    - **数据根三级定位**：`HICODEERD_DATA_ROOT` 环境变量 → `<conf>/customer_data_path` 记录（宿主在数据根位于沙箱外时写入，见 §10 数据根挂载条）→ `<conf>/../../hicodeer`（默认数据根）。都拿不到时退到 `<conf>/osuser-shim.js`；仍不可写则只 `log::warn!`、不注入。
    - ⚠️ **必须防"毒化"**：数据根同时是 Node 的 scratch 区，托管运行时重新下载 Node 时会 `remove_dir_all(<数据根>/node)` —— 放在 `node/shim/` 下的垫片会被**连坐删除**，而 `NODE_OPTIONS` 指向不存在的文件会让 Node **直接起不来**（比没有垫片严重）。故 `apply()` 每次派生**前多做一次 `stat`**，不在就立刻重写（只有 stat 的开销）。
    - **注入点 2 处**（各 1 行 `passwd_shim::apply(&mut cmd)`）：`hicodeerd/src/exec.rs`（exec 通道 —— 所有 ACP agent 走这里）、`hicodeerd/src/pty.rs`（交互终端）；`main.rs` 解析出 conf 目录后调 `passwd_shim::install(&conf)`
    - **无条件注入（2026-09-12 起）**：原"运行期判定"（`libc::getuid()` + 读 `/etc/passwd` 第 3 字段比对，uid==0 或查得到就不注入）已**整体删除** —— 垫片本身幂等、无副作用，判断只带来漏投风险。二进制级验证：`/etc/passwd` 出现 **0** 次、旧判定符号 **0** 个。
    - **安全栏**：shim 文件缺失 / 路径含空白 / 非 UTF-8 ⇒ `log::warn!` + 不注入（**绝不**把无效 `--require` 塞进 `NODE_OPTIONS` —— 那会让该会话所有 Node 进程都起不来，比原 bug 更糟）；既有 `NODE_OPTIONS` 是**追加**不是覆盖，并做幂等
    - **本机三组实测**（uid 20020201，2026-09-11；用户名缺省已随改名调整）：无 shim ⇒ `ERR_SYSTEM_ERROR / uv_os_get_passwd ENOENT`；有 shim + 无 `HOME` ⇒ `OK {uid:20020201, gid:20020201, username:"hicodeer", homedir:"/"}`；有 shim + 有 `HOME` ⇒ `OK {username: HICODEERD_OSUSER 或 "hicodeer", homedir:$HOME}`
    - `username` 可用 `HICODEERD_OSUSER` 覆盖（与 `NODE_OPTIONS` 一起注入，缺省 `hicodeer`）；`uid/gid` 取真实值（`process.getuid()`），避免"按用户名隔离"的逻辑撞车
    - ⚠️ 只覆盖 **Node 系** agent；Python/native agent 若要同样效果仍需 `LD_PRELOAD`（未验证，不建议）。shim 只在 `ERR_SYSTEM_ERROR` 时兜底，其余异常原样上抛
    - ⚠️ **无效做法**：设 `USER` / `LOGNAME` —— libuv 的 `uv_os_get_passwd` 走 `getpwuid_r(uid)`，**不读环境变量**，骗不过去。
    - 更底层的 `LD_PRELOAD` hook `getpwuid_r` 理论上更通用（连 Python/native agent 一起覆盖），但需交叉编译 .so、OHOS 沙箱是否放行**未验证**、且实现 `struct passwd` 缓冲容易踩内存坑 ⇒ **不建议**。
    - 伪造值建议用**真实 uid**（`process.getuid()`）而非固定字符串，避免不同 uid 的应用在"按用户名隔离"的逻辑里撞车。
    - 代价：`os.userInfo()` 从"必然抛错"变成"静默返回假值"，会少一个排查信号；建议 shim 保留一个 verbose 开关（如 `SHIM_VERBOSE=1` 时向 stderr 打一行）。
    - ✅ **自证"注入真的生效"的两条免用户路径（2026-09-11 补）**：
      - ① **本机等价复现**（最轻，随时可跑）：`env -i PATH=/usr/bin:/bin [NODE_OPTIONS="--require <shim>"] /bin/sh -c "<node> <probe.js>"` —— 与 zcoderd 内部 `Command::new("sh").arg("-c")` **逐字同构**。A 组（不注入）应复现 `uv_os_get_passwd ENOENT`；B 组应返回正常 userInfo ⇒ 证明 `NODE_OPTIONS` 能穿过 `sh -c` 抵达 node。
      - ② **集成级**（走守护进程自己的通道，全程无 UI）：管理口 **4023** 用固定 key（`<conf>/authorized_keys`，构建期生成）连入并发 `hicodeerd-bootstrap` ⇒ 拿到本次运行的**动态命令口私钥**（JSON `SshInfo`）⇒ 再用它连命令口 **4022** exec 一条 `node …`，即可证"守护进程确实注入了 `NODE_OPTIONS`"。**这条通路已封装成 skill `zcoder-guest-exec`**（`scripts/guest_exec.sh <脚本文件>`，脚本经 stdin 喂给 guest 的 `sh -s`），本次验证即用它取证。构建期私钥留在 VM `~/rust-target/<PROJECT_HASH>-hicodeerd-keys/`（`bundle-ohos` 每次重建；指纹与设备侧 `authorized_keys` 一致）。协议常量见 `hicodeerd/src/protocol.rs`（`COMMAND_PORT=4022` / `MANAGEMENT_PORT=4023` / `BOOTSTRAP_COMMAND`）。
      - ⛔ **验收原则**：以上都是**我方取证**，交付/发布流程里**用户侧零操作** —— 正确验收就是"打开 zcoder → 连 codebuddy → 发一句话"。把验证命令写成"请用户执行"是错的。
  - ✅ **注入 env 的官方口子（首选，零改码零重编）**：zcoder 设置 `agent_servers` 里给注册表 agent 传 env ——
    `crates/settings_content/src/agent.rs:737-740` `#[serde(tag="type", rename_all="snake_case")] enum CustomAgentServerSettings { Custom{command,args,env}, Registry{env,…} }`（`Registry.env` 注释即"Additional environment variables to pass to the agent"）；
    `crates/project/src/agent_server_store.rs:376-384` 用 **registry id** 作键查表，env 一路进 `AgentServerCommand.env` → spawn env。
    设备侧配置文件路径 = **`<CUSTOM_DATA_DIR>/config/settings.json`**（`paths.rs` `settings_file()=config_dir()/settings.json`，`config_dir()=CUSTOM_DATA_DIR/config`）；本机 picked root = 工作区 ⇒ `HiCodeer/config/settings.json`（共享盘，可直接改）。写法：`"agent_servers": { "<registry-id>": { "type": "registry", "env": { "KEY": "VAL" } } }`
  - 参考：`build_command`（`depend/cmd-agent/cmd-client/src/command.rs:21-65`）把 exec 拼成 `mkdir -p '<cwd>' && cd '<cwd>' && KEY=VAL exec '<bin>' '<arg>'` —— **cwd 会被 cd 进去，env 以 `KEY=VAL` 前缀注入，其余继承 zcoderd 环境**（故给 zcoderd 加环境变量同样生效）
  - 另有两个 `os.userInfo()` 调用点（OTel `ProcessDetectorSync`、`compressPath`）**都有 try/catch**，无害
- ⚠️ **在本机做 codebuddy 实验前必须先隔离 WorkBuddy 注入的环境**：WorkBuddy 自己的 shell 里带着整套 `CODEBUDDY_*`，其中 `CODEBUDDY_DISABLE_AUTO_MEMORY=1`（**会提前掩盖本 bug**）、`CODEBUDDY_INTERNET_ENVIRONMENT=internal`（走内网网关 ⇒ `session/new` 卡死）、`CODEBUDDY_MCP_CONFIG=…`（去连 `127.0.0.1:34831` 的 connector-proxy）、`CODEBUDDY_CONFIG_DIR=/storage/Users/currentUser/.workbuddy` —— 照抄环境跑 probe = 自带补丁 + 自带卡死，**直接毁掉结论**。用 `env -i` 全清，或 `env -u` 精确摘掉。详见 skill `codebuddy-acp-probe`
- **本机没有可用的、独立的 codebuddy 登录态** ⇒ 本机到不了 `session/prompt`（`session/new` 直接回 `Authentication required`）；设备侧能到，是因为它沙箱里有登录态，而那份凭证在 per-uid mount namespace 里、外部读不到 ⇒ **端到端 A/B 只能在设备侧做**，本机只能做离线链路复现（见上条）
- `authenticate {"methodId":"internal"}` 实测**只回私有反向通知**（`_codebuddy.ai/authUrl`，URL = `https://www.codebuddy.ai/login?platform=CLI&state=…`），**不发标准 `elicitation/create`**；且它自己日志打 `[CliExternalUriOpener] UrlUtils.openUrl result: false` / `browser open failed, keep polling` ⇒ 在 OHOS 上它连自己的浏览器都打不开（这正是 zcoder 里"没有登录选择界面"的一半原因）
- 其它不确定的路径可先 `ls` 确认，别照文档里的行号硬信（代码会动）

---

## 附：本目录文件

| 文件 | 用途 |
|---|---|
| `Zed提供的问题定位手段.md` | 本手册 |
| `psrch.py` | 代码考古检索器（grep 的可靠替代，见 §6） |
