# zcoderd 设计文档

> 独立于《计划与遗留核对文档》。本文只描述系统设计与方案结论；开发后逐项核对清单见计划文档。

## 1. 目标与背景

zcoder(编辑器，移植 HarmonyOS NEXT)的应用沙箱禁止 exec 外部程序(git/LSP/编译等)，因此需要把命令执行放到"能 spawn 的进程"里。仓库原有两套后端：`cmd-agent`(外部 OpenEuler VM) 与 `qemu-ssh-agent`(进程内 QEMU guest + `ssh-agentd`)。

本设计统一为**单一自包含 agent**：

- **zcoderd**：服务端。独立二进制可执行程序，以 HNP(public) 集成进 zcoder HAP，作为鸿蒙本机常驻、可 spawn 命令、对外提供加密命令执行的原生服务。替代两套老后端里"命令执行"的角色。
- **cmd-client**：客户端库。打进 zcoder 应用进程，被 `util::command`(ohos) 与 launch-zed 依赖，负责连接 zcoderd 并执行远程命令。

其它 agent 代码一律保留但不再编译/调用。cmd-client / zcoderd 自包含，不依赖 gpui_ohos 下其它任何 crate。

## 2. 构建归属（两个 crate 分开，不合并成一个 workspace）

- `zcoderd/`：**独立二进制 crate，自持 `Cargo.lock`，不进仓库顶层 cargo workspace**（与 `cmd-agentd`/`qemu-ssh-agentd` 一致）。由 `script/bundle-ohos` 单独交叉编译，产物放 HNP resfile。
- `cmd-client/`：**独立 lib crate，不列顶层 member**；被 `util`(cfg ohos) 与 `launch-zed` 用 path 依赖引用，cargo 自动纳入解析(同 `cmd-agent` client 的先例)。
- 两端共享的"保留命令字 / SshInfo 结构 / 端口号"是**文本级协议约定**，在各 crate 内各自持有一份常量定义，注释标注"与对端必须保持一致"，以此保持零 crate 依赖。

```
crates/gpui_ohos/depend/cmd-agent/
  DESIGN.md
  zcoderd/                 独立 bin，独立 Cargo.lock
  cmd-client/              独立 lib
```

## 3. 运行形态与网络模型

两个角色、同一台鸿蒙设备、经 TCP loopback 通信，均使用 russh(SSH)，加密、无账号密码：

| 通道 | 端口 | 承载 | 钥匙 |
|---|---|---|---|
| SSH 命令口 | 127.0.0.1:4022 | exec(命令) | **动态**：每次启动现生成 host key + client 钥匙 |
| 管理口 | 127.0.0.1:4023 | bootstrap(发钥匙) | **固定**：构建期生成，分两份 resfile |

两套钥匙语义分开：

- **管理口固定钥匙**：管理口 server host key + client 鉴权公钥，构建期一次性生成，zcoderd 与 cmd-client 的 resfile 各存所需半边。固定 = 换一次包才轮换。
- **命令口动态钥匙**：zcoderd 每次启动现生成 host key + client keypair，**每次启动不同**；host key 供 cmd-client 校验"连的是真 zcoderd"(防中间人)，动态 client 私钥供命令口鉴权。均不落盘，由管理口在下述 SshInfo 中下发。

### 3.1 引导时序

1. zcoderd 启动：读固定管理钥匙 → 现生成动态命令钥匙 → bind 4023(管理) 与 4022(命令) → 各自 accept。
2. cmd-client 想用命令口：先连 **4023**，用固定 client 钥匙鉴权 + 校验固定 host key；然后发保留命令 `zcoderd-bootstrap`。
3. zcoderd 管理口收到该保留命令，返回本次 **SshInfo**：动态命令 host key + 动态 client 私钥(OpenSSH PEM) + 命令端口 4022。
4. cmd-client 断开管理口，用 SshInfo 连 **4022**：校验动态 host key、用动态 client 私钥鉴权，开始跑命令。
5. zcoderd 重启 → 动态钥匙变化 → 命令口旧连接失效 → cmd-client 回到第 2 步重新取钥(见 §8 重试)。

## 4. 命令通道功能（F1~F10、F17）

- F1 执行：每条 exec 请求在 zcoderd 端 `sh -c <命令>` 执行。
- F2 三路流：子进程 stdin 由 channel data 前送；stdout → SSH data；stderr → SSH extended_data(stderr 类)，独立。
- F3 长驻 LSP：双向桥**只有子进程真正退出才结束**；stdout 瞬时 EOF 只发 channel eof、继续 poll(LSP 可恢复输出)，不提前关任何方向。
- F4 结果：正常退出返回 exit-status；被信号杀死返回 exit-signal(信号名)；异常给兜底 128；spawn 失败 127。
- F5 中止：杀**整个进程组**。
- F6 并发：多 exec/多连接并发，各会话互不干扰。
- F7/F8/F9：加密、公钥鉴权、host key 校验(不 AcceptAll)。
- F10：服务端不设空闲超时踢连接，保证连接池的长连接不被回收。
- F17：pid 不落文件，**记内存**。

## 5. 会话与信号（pid 内存化）

zcoderd 端维护全局会话表 `Mutex<HashMap<u64 /*session_id*/, i32 /*pgid*/>>`：

- 每条 exec 由 cmd-client 分配自增 `session_id` 并编入命令串首行保留标记(见 §6)，zcoderd 的 exec 处理器解析该 id。
- spawn 用 `process_group(0)` 使子进程 pid==pgid；成功即写表 `(session_id → pgid)`。
- 中止走**保留命令字** `__zcoderd_signal__ <session_id> <signal>`：zcoderd 拦截(不跑 shell)，查表 `kill(-pgid, signal)`，返回退出码。不再有 `kill -$(cat pidfile)`。
- 清理：子进程退出 / channel 关闭 / 连接断开时删除对应表项；连接断开清掉该连接产生的所有会话。

## 6. 协议：SSH 承载 + 保留命令字

保留命令字(两端各自定义，保持一致)：

- `zcoderd-bootstrap`（管理口）：管理 handler 收到即返回 SshInfo(§3.1)。
- `__zcoderd_signal__ <session_id> <signal>`（命令口）：拦截并查会话表杀进程组(§5)。
- spawn 会话关联：命令口每条 exec 的命令串首行为保留标记行 `__zcoderd_sid__ <session_id>`，其后才是真正的 `sh -c` 内容；zcoderd exec 处理器解析首行后把该 exec 关联到该 session_id 再 spawn。

SshInfo 字段（管理口返回，JSON）：动态命令 host key(公钥) 、动态 client 私钥(OpenSSH PEM) 、命令端口 4022 、会话表相关无 pid_dir。

说明：管理口"返回数据"与命令口"保留命令拦截"均复用一个轻量 handler，分别绑定固定的/动态的 authorized 与 host key(见 §7)。

## 7. 钥匙装配与鉴权要点

- 管理口 russh server：authorized = 固定 client 公钥；host keys = 固定 host key；cmd-client 侧校验固定 host 公钥。
- 命令口 russh server：authorized = 本次动态 client 公钥；host keys = 本次动态 host key；cmd-client 侧用 SshInfo 里的动态 host 公钥校验(不自持、不 AcceptAll)。
- 命令口与连接池的 keepalive 由 client 侧每 30s 维持；服务端 `inactivity_timeout = None`。

## 8. cmd-client 架构与线程模型

- 一个**后台连接线程/任务**(自带 tokio 或 async runtime)负责：连管理口取钥 → 用 SshInfo 建/维持命令口连接池；管理口不可达时**每 10s 重试**，**不阻塞调用线程**(绝不在 GPUI/调用线程 block_on，沿用 cmd-agent 的双 reactor 教训)。
- 连接池：维护 ≥ 空闲连接；命令到达时从池取一条，池空则有限等待或显式失败。
- 重启恢复：zcoderd 重启换钥后旧连接失效 → 检测到失效 → 清池 → 重新取钥建池。
- executor 接口：cmd-client 内置契约类型 `ExecSpec / FdMode / Signal / RemoteChild / RemoteCommandExecutor / ExitFuture` + 全局 `init_executor/executor`；`spawn/signal/try_exit/wait_exit_async` 语义对齐 `util::command::ohos` 现有 Child API。
- 每条命令的 stdio 在 cmd-client 内用 socketpair + pump 任务把 SSH channel 数据桥到调用方可读写的流，保持与 util 的 smol 流接口兼容(移植自 qemu-ssh executor 的 pump)。

## 9. 命令拼装（ExecSpec → sh -c）

由 cmd-client 侧把 `ExecSpec` 拼成 `sh -c` 命令串(移植 qemu-ssh `command.rs`)：

- 首行写 `__zcoderd_sid__ <session_id>`(会话关联，§5/§6)。
- **不做任何路径/目录映射**：binary/args/cwd 原样传递(以后目录都不映射；zcoderd 与 zcoder 同机看同一路径)。
- env：显式 env 以 `KEY=value` 前缀注入；HOME/PATH 等是否需要强制写死按实际 uid 环境核对(见计划遗留#12，尤其 git safe.directory)。
- fd：Null 模式重定向 `/dev/null`。
- cwd 不存在先 `mkdir -p`。
- 保留 `sh_quote` 转义；去掉原 guest 的 `/tools`、`/sandbox` 相关注入。

## 10. 下载与工作目录

- zcoderd 默认工作路径/数据根 = `/storage/Users/currentUser/.zcoder`。
- **只改 LSP 下载**：LSP 下载到 `/storage/Users/currentUser/.zcoder/languages`；extensions 等其它下载路径不变。下载仍由 zcoder 侧发起并落盘；启动 LSP 时 zed 用 `languages_dir()` 拼出**绝对路径**下发，zcoderd 按绝对路径 exec（不靠 PATH：`which` 只查 PATH 目录本身、不递归子目录，cmd-client 不再注入 PATH）。
- 与"命令/URI 不映射"区别：不映射 = 路径原样；改 LSP 下载路径 = 一处语言服务器安装目录常量调整。

## 11. 日志

- zcoderd：`cfg(target_env="ohos")` 下经 `OH_LOG_Print` 写 hilog(domain 0x0001，tag 定名后统一)；非 ohos(本机联调) 写 stderr。
- cmd-client：位于 zcoder 进程内，沿用 zcoder 既有日志通道。

## 12. 服务端健壮性

- 4022/4023 被占或旧实例未退：bind 失败记 error 后退出，靠外部(鸿蒙进程管理/用户侧)重启；cmd-client 持续每 10s 重试取钥，进程恢复后自动连上。
- 单连接 handler 保持每连接独立会话状态(移植 qemu server.rs 的教训：SSH channel id 从 2 起、跨连接会冲突，会话表必须隔离)。

## 13. 老后端移除边界(简述)

- launch-zed 不再启用 `openeuler-agent`/`qemu-agent` 及对应 linker；`util` 不再依赖 `command-executor`(契约收进 cmd-client)。
- cmd-agent / qemu-ssh-agent / command-executor / 两个 linker 文件全保留，摘除引用后不再被编译。
- zed/language 的 URI 映射与 LSP 下载路径相关分支按计划遗留清单逐项定位移除/调整(受保护文件会先给 diff)。
