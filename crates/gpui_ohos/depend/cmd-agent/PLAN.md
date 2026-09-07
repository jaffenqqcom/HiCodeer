# zcoderd：把 qemu-ssh-agentd 移植为鸿蒙本机命令执行服务

> 本文件为《计划 + 遗留核对文档》。设计与结论依据见同目录独立文档：`crates/gpui_ohos/depend/cmd-agent/DESIGN.md`（架构细节以 DESIGN 为准）。

## Context（为什么做）

zcoder(编辑器，移植到 HarmonyOS NEXT) 因 OHOS 应用沙箱禁止 exec 外部程序(git/LSP/编译等)，一直依赖外部命令执行后端。仓库现有两套：`cmd-agent`(OpenEuler VM，默认 feature `openeuler-agent`) 与 `qemu-ssh-agent`(进程内 QEMU guest + guest 内 `ssh-agentd`，feature `qemu-agent`)。

用户决定：**统一为单一、自包含的新 agent** —— server 端 **zcoderd**（独立二进制可执行程序，以 HNP(public) 集成进 zcoder HAP，作为鸿蒙本机可 spawn 命令、常驻监听端口的原生服务）；client 端库名 **cmd-client**。zcoder 通过加密 SSH 连 zcoderd 执行命令。不再使用其它 agent（代码保留但不编译/调用）。

## 已确认决策（用户拍板汇总；架构细节见 DESIGN.md）

**功能点（F1~F17，逐项确认）**
- 要：F1 执行 shell 命令(sh -c) / F2 stdin/stdout/stderr 三路流 / F3 长驻进程(LSP，双向不关) / F4 退出码·信号·兜底 / F5 中止(杀整个进程组) / F6 多命令并发 / F7 连接加密 / F8 公钥鉴权(无账号密码) / F9 zcoder 校验 host key / F10 不踢闲置连接。
- F17：**pid 不落文件，记内存**（server 维护"会话→进程组"表；"杀"走保留命令字）。
- 删除项 = F12 时间同步 / F13 周期清缓存 / F14 改 profile·建家目录 / F15 路径翻译映射 —— guest 支撑 zcoderd 不需要。
- serial 载体删除：bootstrap 语义**保留**，只是从 virtio-serial 改为 socket 管理口(见下)。

**两通道 × 两套密钥**
- SSH 命令口 `127.0.0.1:4022`：**动态钥匙**，每次启动现生成 host key + client 钥匙；host key 供 cmd-client 校验(F9)、client 动态私钥供鉴权。
- 管理口 `127.0.0.1:4023`：**固定钥匙**，构建期静态预置(管理口 server host key + client 鉴权公钥)，两端 resfile 各存所需半边。cmd-client 连上发保留命令 `zcoderd-bootstrap`，server 回本次 SshInfo(命令口动态 host key + 动态 client 私钥 + 命令端口)，cmd-client 用它连 4022。
- 两路都 russh、加密、公钥鉴权、host key 校验，**不 AcceptAll**。端口 4022/4023 已确认。

**下载与工作目录（用户最初 5 点 #5 与 D3）**
- zcoderd 默认工作路径/数据根 = `/storage/Users/currentUser/.zcoder`。
- **只改 LSP 下载**：落到 `.zcoder/languages`；**不改** extensions 等其它下载。下载仍由 zcoder 侧发起落盘；启动 LSP 用 `languages_dir()` 拼绝对路径下发，zcoderd 按绝对路径 exec（不靠 PATH，cmd-client 不注入 PATH）。
- 与"命令/URI 不映射"是两件事：不映射 = 路径原样传递；改 LSP 下载路径 = 一处语言服务器安装目录常量调整。

## 目录与构建归属（按 DESIGN §2；zcoderd 不进顶层 cargo workspace）

```
crates/gpui_ohos/depend/cmd-agent/          # 两个独立 crate，无外层共享 workspace
  DESIGN.md
  zcoderd/                       # 独立二进制 crate：自持 Cargo.lock，不进仓库顶层 workspace，
                                 # 由 script/bundle-ohos 单独交叉编译 → HNP(public) resfile
    src/main.rs                  # 入口：读固定管理钥匙→生成动态ssh钥匙→bind 4022/4023→accept
    src/management.rs            # 管理口 russh(固定钥匙)：zcoderd-bootstrap 返回 SshInfo
    src/sshd.rs                  # 命令口 Handler(动态钥匙 publickey, exec/data/eof) ← qemu-ssh-agentd/server.rs
    src/exec.rs                  # sh -c + process_group + 双向桥 + exit ← exec.rs；pid 内存表；保留命令拦截
    src/protocol.rs              # 保留命令字(__zcoderd_sid__/__zcoderd_signal__/zcoderd-bootstrap) + SshInfo(server 侧一份)
    src/keygen.rs                # ed25519 生成/序列化 ← qemu-ssh-agentd/keygen.rs
    src/logger.rs                # cfg(ohos): hilog；非 ohos: stderr(本机联调)
    Cargo.toml
  cmd-client/                    # 独立 lib crate：不列顶层 member；被 util/launch-zed path 依赖自动入图
    src/types.rs                 # executor 契约 ExecSpec/FdMode/Signal/RemoteChild/trait + 全局 init_executor/executor
    src/protocol.rs              # 与 server 同步的保留命令字 + SshInfo 反序列化(cmd-client 一份，须与 server 保持一致)
    src/bootstrap.rs             # 连 4023(固定钥匙)+exec zcoderd-bootstrap 读 SshInfo；后台线程每10s重试，不阻塞调用线程
    src/pool.rs                  # 连接池 ← qemu-ssh-agent/pool.rs；去 AcceptAll→host key 校验
    src/executor.rs              # spawn/signal/try_exit/wait_exit + pump ← qemu-ssh-agent/executor.rs；signal→__zcoderd_signal__
    src/command.rs               # ExecSpec→sh -c 串；无任何路径/目录映射；首行带 __zcoderd_sid__ ← command.rs
    Cargo.toml
```

**复用与改造要点（拷贝后在各自 crate 内独立改造，不引用源 crate）**
- exec.rs：spawn 成功把 `(session_id, pgid==child pid)` 插入内存 `Mutex<HashMap<u64,i32>>`；`__zcoderd_signal__ <sid> <sig>` → 查表 `kill(-pgid, sig)`；child 退出/连接关闭清表。SshInfo 不含 pid_dir。
- command.rs(cmd-client)：目录一律不映射(删 map_guest_path//tools env)；git safe.directory 按遗留#12 复核；命令串首行带 `__zcoderd_sid__` 使 server 内存关联；保留 sh_quote 与 fd redirection。
- 协议常量两端各一份、注释标注须与对端同步；管理口与命令口复用轻量 handler、authorized/host key 各自不同(固定 vs 动态)。
- 本机联调：zcoderd 以非 ohos target 编译即可在 linux VM 直接跑(日志 stderr)。

## 接线与依赖改动（受保护文件逐项列，实施前逐一说明 diff 面再改）

1. `crates/util/Cargo.toml`（受保护）：`[target.'cfg(target_env="ohos")'.dependencies]` `command-executor` → `cmd-client`(path 到 `depend/cmd-agent/cmd-client`)。空 marker `openeuler-agent`/`qemu-agent` 随废弃清理。
2. `crates/util/src/command/ohos.rs`（含 ohos，可改）：`use command_executor::{..}` → `use cmd_client::{..}`；`executor()`/`init()` 改读 `cmd_client::executor()`；其余类型名不变。
3. `crates/gpui_ohos/depend/launch-zed/Cargo.toml`（受保护）：移除 `openeuler-agent`/`qemu-agent` 及四个 agent/linker dep；新增 `cmd-client` path dep + 新 feature(如 `zcoderd-agent`，默认启用)。
4. `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`（含 ohos，可改）：`start_qemu`/`start_cmd_agent`(及 helper) 换 `start_zcoderd_client`：起 cmd-client 后台连接器，注册 `cmd_client::init_executor`；移除 VM/QEMU 逻辑。
5. 顶层 `Cargo.toml`（受保护）：**不需要新增 member**——zcoderd 独立编译不进 workspace；cmd-client 由 util/launch-zed path 依赖自动纳入解析(同 cmd-agent client 先例)。老 agent crates 保留 members；`default-members` 现状已 `["crates/zed"]`，核对是否还需收窄。
6. `script/bundle-ohos`（受保护）：移除 `--server-only`(cmd-agentd) 与 `--qemu`(QEMU 资源) 分支；新增：独立交叉编译 `depend/cmd-agent/zcoderd`(独立可执行, ohos target) 放入 HNP resfile；管理口固定钥匙生成并分别放入两份 resfile。
7. URI/目录映射整体移除：**以后目录都不映射**。`crates/zed/Cargo.toml` 的 `qemu-agent`、`crates/language/Cargo.toml` 对应分支、`crates/workspace/Cargo.toml` 的 openeuler-agent/qemu-agent 空 marker 一并清理；language 实际映射分支定位后按其 cfg 移除(受保护文件先给 diff)。
8. **LSP 下载目录改动**（受保护文件定位后给 diff）：定位语言服务器安装目录常量(现随 data_dir 的 languages/ 或对应逻辑)，使 LSP 落在 `.zcoder/languages`；只影响 LSP，extensions 等不变。

## 未确认 / 遗留待核清单（开发完成后逐项核对）

1. **zcoderd 进程如何被拉起并常驻**：zcoderd 是独立可执行；由 HNP(public) 以哪种方式启动为常驻进程(用户鸿蒙侧)。代码只交付 zcoderd 独立可执行入口。
2. **spawn 权限**：普通 HAP + 权限如何放行 exec git/LSP(exec 白名单外扩)，module.json5/签名由用户落地。
3. **URI/目录映射移除的精确 diff**：定位 language 与 zed 里映射/跳过逻辑所在文件与 cfg；目标 = 无任何目录映射。
4. **"不再编译老 agent"边界**：cmd-agentd/qemu-ssh-agentd 是 bundle-ohos 单编 bin；client/linker 经 launch-zed feature 编译。核对改完后无 crate 再引用 command-executor/两个 linker。
5. **`util::command::ohos::init()` 语义**：从"socket_path 存在性校验"改为"cmd_client executor 就绪校验"；launch-zed 传入参数变化。
6. **pid 内存化附带**：命令串 sid 首行、`__zcoderd_signal__`、内存表清理；与 util kill_on_drop/并发/重启兼容；SshInfo 无 pid_dir。
7. **管理口固定钥匙**：(a) 管理口 server host key + (b) 管理口 client 鉴权公钥，构建期生成分两份 resfile；命令口动态钥匙由管理口 SshInfo 下发、不落盘；不使用 AcceptAll。
8. **HNP 资源路径**：zcoderd bin + 管理口钥匙如何进 zcoder HAP 的 HNP resfile；cmd-client 与 zcoderd 各自读取哪些文件(用户打包侧，开发完核对实际安装路径)。
9. **日志**：zcoderd 打 hilog(domain 0x0001)；tag 定名后记此核对。
10. **端口冲突**：4022/4023 被占或旧实例未退处置策略(bind 失败重试/报错退出 + client 10s 重试)。
11. **F2+F3 回归**：stderr(extended_data)/stdout EOF 语义在内存会话表下正确性(长驻 LSP 双向、quick 命令 EOF 后仍等 exit status)。
12. **env/uid 细节**：去掉 guest /tools 注入后，git `safe.directory` 是否需保留(文件 owner ≠ zcoderd 运行 uid 时 dubious ownership)；保留范围按实际 uid 关系定。
13. **marker feature 取舍**：zed/language/workspace/util 老 marker 全清还是留空缩小 diff(接线点 1/7 已倾向清理)。
14. **端口值 4022/4023** 与设备现有服务是否冲突(用户侧确认后记结果)。
15. **LSP 下载目录改动精确位置**：语言服务器安装目录常量在哪个 crate/文件，如何以 ohos 条件覆盖到 `.zcoder/languages`，只影响 LSP(接线点 8)。
16. **zcoderd 以 `.zcoder` 为根落地**：cwd/数据根由 HNP 指定还是自 cd；`.zcoder/languages` 读写权限与 uid 关系(与 #2 相关)。

## 验证（每阶段独立可跑）

1. **编译独立**：分别 `cargo check` `depend/cmd-agent/zcoderd` 与 `depend/cmd-agent/cmd-client` —— 各自无本仓库内部 path 依赖(协议常量仅自身)。
2. **本机手测(linux VM 直接跑 zcoderd 独立可执行)**：起 zcoderd → 管理口(固定钥匙)取 SshInfo → 动态 key 连 4022 跑 `git status`、长驻 LSP(clangd)双向探针、并发多命令、杀进程组、退出码/信号；重启 zcoderd 验证换钥 + cmd-client 10s 重连取新钥。
3. **接线全链路**：util/launch-zed 改完后，zcoder `util::command::new_command("git")` 落到 zcoderd；参考 cmd-agent examples 思路做探针。
4. **打包**：bundle-ohos 产出含 zcoderd(HNP) 交付物与钥匙；配合用户侧 HNP 集成上设备联调。
5. **收尾核对**：对照遗留清单 #1~#16 逐项核对勾销。

## 实施顺序

1. 建两个独立 crate 骨架：`depend/cmd-agent/zcoderd`(bin, 自带 lock) 与 `depend/cmd-agent/cmd-client`(lib)，能各自编译；**不加顶层 members**。
2. 移植 server：sshd/exec/keygen/logger/main/protocol/management，删 guest 特化，pid 内存化，保留命令拦截。
3. 移植 client：types/protocol/pool/executor/command/bootstrap，契约收进 cmd-client，host key 校验。
4. 本机两进程端到端手测(验证2)。
5. 接 util + launch-zed(受保护 1/3、可改 2/4)。
6. bundle-ohos 与 HNP 集成(受保护 6 + 用户鸿蒙侧)。
7. 收尾：老 agent 摘除边界、URI 映射移除、LSP 下载目录改动(遗留#3/#4/#15)，对照清单逐项核对。
