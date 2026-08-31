# ohos-qemu-agent 方案：命令执行与工作目录挂载（设计与检视清单）

> 本文档记录用 QEMU 替代 OpenEuler VM 后，命令执行通道、工作目录挂载、
> 路径映射等技术决策。实现与代码检视时逐条对照。
>
> 状态标记：`[已定]` 已确认的决策；`[待定]` 尚未拍板；`[待验证]` 需要实测/调研确认。

## 术语与命名规则

- **cmd-agentd**：跑在 QEMU 里的 agent（执行命令、端口回收、路径映射）。
  部署在 HAP 资源文件（resfile）中，`chmod +x` 保留可执行属性。
- **cmd-agent**：zcoder 侧的命令客户端（executor）。
- 不使用 "host / guest" 表述（易混淆）。目录、代码命名遵循 cmd-agentd / cmd-agent 规则：
  - 二进制/程序名 = **cmd-agentd**，**目录名亦用 cmd-agentd**。

## 1. 命令执行通道 [已定]

- **放弃 TCP/SLIRP hostfwd 作为命令通道**。
  - 原因：hostfwd 端口可能对整个网络可见，且 cmd-agent 协议明文无鉴权；
    命令通道承载"在设备上执行任意命令"的能力，按最高敏感度处理。
- **改用 virtio-serial 通道**：
  - zcoder 侧为 unix socket（应用沙箱内），QEMU 侧为 `/dev/vport*` 字符设备。
  - 不经过网络栈，天然不被网络可见。
- **不参考 OHcode 的 hostcmd 模型**（Guest→Host 的 popen 文本桥：无并发、
  无信号、无 stdin 注入、stdout/stderr 合并，只够一次性 shell 命令）。

## 2. virtio-serial 端口池 [已定]

- **每命令 2 端口 + 全局 1 管理端口**：
  - 数据端口：承载 stdin/stdout（双向流）。
  - stderr 端口：child 的 fd2 独立走此端口。
  - 管理端口：全局一条，承载 ExecResult / Signal / 心跳 / 映射同步。
- 并发 N 条命令 = **2N + 1** 个端口。
- **QEMU 启动时预创建端口池**（静态 chardev socket + virtserialport），不用 QMP 热插拔。
- 每个端口 = 一条双向字节流，语义等价 cmd-agent 的一条连接；
  **上层协议复用，只换传输层**。
- 待验证：
  - QEMU 侧关闭端口 fd 后，zcoder 侧能否读到 EOF（回收信号的物理基础）。
  - 端口断开后能否可靠复用：zcoder 重新 connect + QEMU 侧重新 open。

## 3. 命令回收协议 [已定]

- cmd-agentd 发 `ExecResult`（管理端口）→ cmd-agent 回 `OK` → 转发给
  util::command 调用者 → 关闭该 session 的 fd、移出 poll set。
- cmd-agentd 收到 `OK` → 回收端口；**2 秒超时未收到 OK → 强制回收**（防端口泄漏）。
- **关键顺序**：`OK` 必须在数据/err 端口都 EOF（输出 drain 完）之后再回，
  否则 socket close 会丢弃未读尾字节。

## 4. crate / 目录结构 [已定]

目录 **ohos-qemu-agent** 对照 **ohos-openeuler-agent** 拆为四个 crate，
**与 openeuler 完全独立**（openeuler 目录将来会删除；package 名带 `qemu-`
前缀避免与 openeuler 同名冲突，目录名仍对照）：

- **cmd-agent**（zcoder 侧，package `qemu-cmd-agent`，lib `qemu_cmd_agent`）：
  QEMU 启动（dlopen libqemu-system-aarch64.so、build_argv、virtio-serial
  端口池创建/分配）、`QemuCommandExecutor`（实现 qemu-cmd-agent-linker 的
  `RemoteCommandExecutor` trait）、挂载协调器（经 `MountFolder2QEMU` 挂载沙箱根）。
- **cmd-agent-protocol**（zcoder 侧，package `qemu-cmd-agent-protocol`，
  lib `qemu_cmd_agent_protocol`）：独立协议 crate（messages + frame），被
  cmd-agent 与 cmd-agentd 共用；与 openeuler 协议互不干扰。
- **cmd-agent-linker**（zcoder 侧，package `qemu-cmd-agent-linker`，
  lib `qemu_cmd_agent_linker`）：executor 接口 trait，`util::command` 通过它
  调用 executor。
- **cmd-agentd**（QEMU 侧，package `qemu-cmd-agentd`，lib `qemu_cmd_agentd` +
  bin `cmd-agentd`）：独立 aarch64 二进制，执行命令、回收端口、做路径映射。
  - 部署在 HAP resfile，`chmod +x`；版本更新 = 替换 resfile 中的二进制。
  - 兜底：启动时打印 resourceDir 与 base_path 一条日志，确认包含关系。
- **ohos-openeuler-agent 不再被引用**（openeuler-vm 特性保留但将来删除；
  bundle-ohos 默认 qemu 编译）。
- **util::command 删除路径映射功能**（见第 6 节）。
- 9p 挂载根为**沙箱根** `/data/storage/el2/base/`（见第 7 节），不是 base_path。

## 5. 协议 [已定]

- **独立 cmd-agent-protocol crate**，复制 cmd-agent-protocol 的 wire 类型，
  互不干扰；不复用、不修改 ohos-openeuler-agent 目录下的 cmd-agent-protocol。
- 消息帧：长度前缀 JSON（frame.rs），同步读写。
- 握手 **`Hello` 只交换版本，不带映射表**（映射表不经握手同步，见第 6 节）。
- 消息全集（独立协议内定义）：
  - `Hello { version }` / `HelloOk`——握手。
  - `Spawn { session_id, spec }` / `SpawnOk` / `SpawnStderr { session_id }`——
    命令执行：数据端口承载 stdin/stdout，err 端口承载 stderr。
  - `ExecResult` / `ExecResultAck`——退出结果两段回收握手（见第 3 节）。
  - `Signal { session_id, signal }`——信号。
  - `MountFolder2QEMU { uri, mount_tag, guest_path }` / `MountOk`——
    挂载工作目录（**含沙箱根**）并登记映射表。
  - `UnmountFolder2QEMU { uri }`——撤销映射（保留不主动调用）。
  - `Error` / `Query` / `Shutdown`。

## 6. 路径映射 [已定]

- **映射逻辑在 cmd-agentd 侧做**（guest 侧 path_map.rs；openeuler 的
  cmd-agent 同理）。
- **util::command 删除路径映射功能**：移除 `ROOT_MAP` / `path_arg_indices`
  相关代码，build_spec 不再标记路径参数。
- **对照表，以 zcoder 侧 URI 或路径为索引**：
  - 每条目 = `host_root`（zcoder 侧 URI 或路径，即索引 key）→ `guest_root`
    （QEMU 内挂载路径）。
  - 对命令的 `binary` / `args` / `cwd_path`，**按 key 索引替换**：key 命中
    （key 本身、或 key 作为路径段前缀）即替换成 guest_root；**匹配不到直接透传**。
  - **禁止"最长前缀匹配"这类启发式选择**：替换只按对照表的 key 精确索引。
  - `--flag=<path>` 等号内联参数只映射 `/` 开头的 value。
- **映射表维护只在挂载/卸载函数里，不分散**：
  - `MountFolder2QEMU` 成功 → `path_map.add(host_root, guest_root)`。
  - `UnmountFolder2QEMU` → `path_map.remove(host_root)`。
  - **协议握手（Hello）不带映射表**，也没有其他同步映射表的机制；
    表的状态永远等于当前已挂载集合。
- **固定条目走 MountFolder2QEMU 登记**：沙箱根挂载成功后登记
  `沙箱根（/data/storage/el2/base）→ /sandbox`，命令里 base_path 下的路径
  由此自动映射到 `/sandbox/haps/entry/files/...`。
- **动态条目** `URI_i → /xyz_i`：工作目录，MountFolder2QEMU 时登记。

## 7. 挂载触发：MountFolder2QEMU [已定]

- **沙箱根挂载也走 MountFolder2QEMU**：静态 fsdev（QEMU 启动参数）提供
  `sandbox` tag → 沙箱根（`/data/storage/el2/base/`）；guest 侧 S40sandbox
  开机 `mount -t 9p sandbox /sandbox`。host 侧 cmd-agent 在 QEMU 就绪后发
  MountFolder2QEMU，cmd-agentd 检测 `/sandbox` 已挂载则跳过 mount、直接登记
  映射表（`沙箱根 → /sandbox`）。
- **映射表维护收拢在 mount/unmount**：`mount_folder` 内 add、unmount 内
  remove（见第 6 节）。
- **只在"打开文件夹"入口触发**，不需要对 URI 做目录/文件类型判断
  （入口天然区分）。
- **单体文件不挂载**：单文件授权只覆盖该文件，无目录访问权，不能作为
  9p fsdev path；编辑走授权 fd，QEMU 不可见。
- **幂等**：已挂载的 URI 不重复挂载（zcoder 侧维护已挂载集合）。
- **等待机制**：等 QEMU 就绪 + 挂载成功才返回（挂载协调器 + cmd-agentd 完成确认）。
- **单工作目录**：一次只打开一个工作目录。
- **`UnmountFolder2QEMU` 保留但不主动调用**：防止 LSP 正在该目录扫描时
  umount 导致错误。
- **打开工作目录的入口要找齐**，至少三个：
  - 首次使用界面（欢迎页）的"打开文件夹"。
  - 菜单里的"打开文件夹"。
  - 最近项目列表里的"切换工作目录"。
  - 实现前需调研 zcoder（Zed）打开工作区的统一代码路径，挂 OHOS 侧触发；
    注意 Zed 核心代码（路径不含 ohos 的受保护文件）不能改。

## 8. 9p 路径访问方式 [已确认]

- QEMU 访问 guest 挂载路径（如 /xyz/...）时，9p 用 **fsdev path（zcoder 侧
  真实路径）+ guest 相对路径** 拼接出 OHOS 真实路径来 open / read / write。
- **结论：以真实路径访问工作目录必然可行**（授权范围内的目录，路径 open
  可用；QEMU 不会把路径转成 URI，直接以真实路径访问）。此点不再作为风险项，
  无需专门实验。
- 含义：工作目录的 fsdev path 必须指向其真实路径
  （/storage/Users/currentUser/<folder>），动态挂载要能做到运行时指定它。

## 9. 动态挂载实现路线 [已定：QEMU 补丁 fsdev_add]

- QEMU 9p fsdev 启动时静态，无 `fsdev_add`，官方不支持运行时新增 export。
- **已排除的路线**：
  - 镜像同步 / 复制：做不到真同步（URI 授权下监听不可靠），只能退化成
    复制，git 仓库等大目录不现实。
  - 预分配槽位 + 符号链接：fsdev 的 root fd 在 QEMU 初始化时打开并缓存，
    运行期改符号链接不生效；沙箱内无 bind mount 权限；guest 内核不认
    host 侧符号链接。本质矛盾：fsdev path 启动时固化 vs 工作目录运行时
    才确定。
  - 文件代理（zcoder 进程内做 URI 感知文件访问）：被否决。
- **唯一路线：QEMU 补丁 `fsdev_add`**——运行时动态创建 fsdev，path 直接
  指向工作目录真实路径；guest 侧挂载新 tag，QEMU 以真实路径访问
  （见第 8 节，路径 open 必然可行）。
- 实施（已调研 /mnt/linux_share/workspace/qemu-inhap 源码，改动很小）：
  - `qemu_fsdev_add(QemuOpts*, Error**)` 已存在（fsdev/qemu-fsdev.c:110），
    从 QemuOpts 创建 fsdev 入全局链表；xen-9p-backend.c 已有运行时创建先例。
  - fsdev 的 root fd **惰性打开**：v9fs_device_realize_common（9p.c:4407）
    只把 fse->path 存入 ctx.fs_root（不 open）；guest attach（mount -t 9p）
    时才 open 根目录（9p.c v9fs_attach）。
  - virtio-9p 类定义未禁 hotplug（virtio_9p_class_init），device_add
    virtio-9p-pci 默认可热插拔；realize 时按 fsdev_id 查全局链表。
  - QMP 参考 qmp_netdev_add（net/net.c:1488）：QAPI 参数 → QemuOpts → 动态创建。
  - **改动点**：新增 QMP 命令 `fsdev-add`（复用 qemu_fsdev_add），交叉重编
    libqemu-system-aarch64.so。
- **完整流程**：zcoder 经 QMP `fsdev-add`（path=工作目录真实路径）→
  `device-add` virtio-9p-pci（fsdev=<id>, mount_tag=<tag>）→ cmd-agentd 在
  guest 里 mount -t 9p <tag> <挂载点> → attach 时 open 工作目录真实路径
  （第 8 节已确认可行）→ mount 成功回执给 zcoder。
- 与"不 umount + 单工作目录"叠加：切换工作目录时旧挂载点保留、新目录
  挂新点（fsdev 累积不删）。

## 9.1 cmd-agent 侧挂载/卸载接口 [已定]（2026-08-28 实施）

**对外接口分两侧，名字一一对应**：
- **cmd-agent（zcoder 进程内）**：`mount_folder(path)` / `unmount_folder(path)`
  两个公共接口，供主进程打开工作目录时调用。所有事情封装其中，包括 QMP 调用。
- **cmd-agentd（QEMU 内）**：同名 `mount_folder` / `unmount_folder` 是协议消息
  处理（实际 mount/unmount + 映射登记/撤销，见第 7 节）。
- 职责划分：cmd-agent 管"发起 + QMP + 编号 + 幂等"；cmd-agentd 管"执行挂载 +
  路径替换"。

**mount_folder 内部流程**：
1. 幂等检查：已挂载集合（`mounted: HashSet<String>`）命中即直接返回。
2. 分配编号 `sequence`：fsdev_id=`fsdev{n}`（fsdev0 是静态 sandbox）、
   device_id=`virtio9p{n}`、mount_tag=`ztag{n}`、guest_path=`/ws/{n}`。
3. QMP 一次性会话（`<port_dir>/qmp.sock`，server=on 单连接，低频 open-folder
   路径每次 连接→命令→断开）：`fsdev-add`（id, path=真实路径,
   security-model=passthrough）→ `device_add`（virtio-9p-pci, fsdev, mount_tag）。
4. 经 mgmt 队列（`MgmtCommand::Mount`，复用 signal 的单连接队列模式，见第 2 节）
   发 `MountFolder2QEMU {uri, mount_tag, guest_path}`，同步等 `MountOk`（或 `Error`）。
5. 成功 → 记录已挂载集合。

**unmount_folder 内部流程**：
1. 经 mgmt 队列发 `UnmountFolder2QEMU {uri}`（fire-and-forget，协议无 ack）。
2. 移除已挂载集合。
3. 不真 umount、不 QMP 清理 fsdev（DESIGN：fsdev 累积不删；LSP 可能正在扫描）。

**路径映射职责**（第 6 节延伸）：映射表只在 cmd-agentd（path_map.rs）。cmd-agent
不维护映射，传给命令的 binary/args/cwd 是原始 zcoder 路径，替换全在 cmd-agentd。

**security-model 决策**：工作目录用 `passthrough`（不污染工作区；mapped-file
会在用户目录生成每文件 meta 文件，git 仓库内不可接受；sandbox 根仍用启动参数
mapped-file）。副作用：guest 以 root 跑 git，目录 owner 是 host app uid → git
报 dubious ownership，需 cmd-agentd 配 `safe.directory` 缓解。

**QMP 通道**：build_argv 加 `-qmp unix:<port_dir>/qmp.sock,server=on,wait=off`，
与 virtio-serial 端口池并列。

**挂接点**：workspace `open_paths`（统一汇聚点）对目录路径 cfg(ohos) 经
cmd-agent-linker `mounter()` 调 `mount_folder`；fire-and-forget，不阻塞打开流程；
单文件不挂载（第 7 节）。调用链：打开工作目录（对话框/最近项目/命令行）
→ `open_paths` → `mount_opened_dirs` → `mounter.mount_folder` → QMP +
`MountFolder2QEMU` → cmd-agentd `mount_folder` 执行。

## 10. 待验证 / 待定清单汇总

1. 协议：已定（独立 cmd-agent-protocol crate，互不干扰）。
2. 动态挂载路线：已定 —— QEMU 补丁 fsdev_add（镜像/槽位/文件代理已排除）。
3. virtio-serial 端口 EOF 语义 + 断开重连复用 —— 待实测。
4. resfile 中 cmd-agentd 的路径是否在 9p 挂载范围内 —— 沙箱根挂载（第 7 节）
   天然覆盖 base_path，启动日志兜底验证。
5. 9p 以真实路径访问工作目录 —— 已确认可行（第 8 节），不再验证。
6. 单文件 URI 授权是否只到文件级 —— 待实测（当前按"单文件不挂载"处理）。
7. Zed 打开工作区统一触发点 —— open_paths（workspace.rs），对话框/最近项目/
   命令行三个入口都汇聚于此；cfg(ohos) 挂接已实施（第 9.1 节）。
8. 路径映射表：沙箱根→/sandbox 固定条目 + URI→挂载点动态条目，仅由
   MountFolder2QEMU / UnmountFolder2QEMU 维护 —— 已定。
9. QEMU fsdev_add 补丁（qemu-inhap 源码）—— 已编译 libqemu-system-aarch64.so；
   zcoder 侧 QMP 调用与 mount_folder/unmount_folder 接口已实施（第 9.1 节），
   端到端实测待验证（task #22）。

## 11. 检视时对照

实现与 review 时，逐节核对上述决策是否被遵守：
- 命名：cmd-agent / cmd-agent-protocol / cmd-agent-linker / cmd-agentd 四
  crate（package 带 qemu- 前缀，与 openeuler 同名区分），无 host/guest 混用。
- 命令通道：virtio-serial，无 TCP hostfwd 命令通道残留。
- 端口回收：ExecResult + ExecResultAck 两段握手 + 2 秒兜底，OK 在 EOF 后回。
- 协议：独立 cmd-agent-protocol crate，不改 ohos-openeuler-agent。
- 路径映射：util::command 无映射代码；映射在 cmd-agentd 侧按对照表
  （以 zcoder 侧 URI/路径为索引）替换，无"最长前缀"启发式。
- 映射表维护：只在 MountFolder2QEMU / UnmountFolder2QEMU 内 add/remove，
  握手不带映射表。
- 挂载：沙箱根 + 打开文件夹入口都走 MountFolder2QEMU；幂等、等成功返回、
  单工作目录、不主动 umount。
- 单体文件不挂载。
- 工作目录挂载：运行时经 QEMU 补丁 fsdev_add 以真实路径挂载（第 8、9 节）。
- 对外接口：cmd-agent 侧 mount_folder/unmount_folder 封装全部（含 QMP）；
  cmd-agentd 侧同名函数只做协议处理；路径替换只在 cmd-agentd。
- 幂等与编号：已挂载集合去重；fsdev/device/mount_tag/guest_path 按序号分配。

## 12. QEMU 编译与依赖（特性对齐记录）

- **编译环境**：/home/user/qemu-ohos（configure-ohos.sh 交叉配置 + build-ohos.sh
  = sync-src.sh 同步 + ninja 编译）。源码 = /mnt/linux_share/workspace/qemu-inhap
  （git 仓库，补丁改这里），经 sync-src.sh 同步到 qemu-src 本地副本编译。
- **特性对齐决策**：OHcode 的完整 configure 无权威记录（qemu-inhap 的
  `鸿蒙移植改造方案.md` 仅早期草案，且后来实际改用 --enable-slirp）。
  按用户决策：**先安装本地的**——用本地 configure-ohos.sh 配置编译，
  不再纠结 OHcode 精确对齐。
- **已发现的差异（记录，后续如需对齐再处理）**：
  - OHcode .so（52MB）NEEDED 仅 3 个：libslirp.so.0 / libz.so / libc.so
    —— glib/pixman **静态链接**进 .so。
  - 本地 .so（67MB）NEEDED 5 个：libslirp.so.0 / libz.so.1 /
    libpixman-1.so.0 / libglib-2.0.so.0 / libc.so —— glib/pixman **动态链接**。
  - zlib soname：OHcode 用 libz.so；本地用 libz.so.1。
  - slirp：一致（均 --enable-slirp，NEEDED 均有 libslirp.so.0）。
- **影响**：本地 QEMU 进 HAP 需额外带 libglib-2.0.so.0、libpixman-1.so.0、
  libz.so.1（连同 libslirp.so.0）。bundle-ohos 的 QEMU 资源打包需补全这些依赖。

## 12.1 实际部署记录（2026-08-28）

- **QEMU 入口符号**：新编译的 qemu-ohos 库导出 `main`（非 OHcode 预构建的
  `qemu_system_entry`）；cmd-agent load_engine 改 dlsym("main")。
- **新 .so**：/tmp/qemu-ohos/build/libqemu-system-aarch64-stripped.so →
  ohos-qemu-agent/libqemu-system-aarch64.so（15MB stripped）。NEEDED 动态链接
  glib/pixman。
- **HAP 依赖**（bundle-ohos 循环拷贝；OHOS 沙箱禁 symlink，拷实体文件；
  来源 ohos-qemu-agent 资源目录 + harmonybrew lib）：
  libslirp.so.0 / libz.so.1 / libpixman-1.so.0 / libglib-2.0.so.0 /
  libgobject-2.0.so.0 / libgmodule-2.0.so.0 / libgthread-2.0.so.0 /
  libintl.so.8 / libpcre2-8.so.0 / libffi.so.8。
- **cmd-agentd 端口池**：QEMU build_argv 生成 zcoder.mgmt + zcoder.cmd.0..13 +
  zcoder.err.0..13（每命令 2 端口 + 1 管理，PORT_POOL_SIZE=14）。
- **guest 启动**：rootfs 加 S42cmdagentd（从 /sandbox find cmd-agentd 启动），
  并清理残留 S40hostshare/S40usershare（fsdev 已移除）；cmd-agentd 由
  bundle-ohos 编为 aarch64-linux 二进制放 resfile。
- **host executor**：cmd-agent/src/executor.rs 的 QemuCommandExecutor 实现
  RemoteCommandExecutor，launch_app start_qemu 里注册；协议来自独立
  cmd-agent-protocol crate（cmd-agent / cmd-agent-protocol / cmd-agentd 均
  纳入 zcoder workspace）。
- **动态挂载（9.1 节落地）**：build_argv 加 `-qmp unix:<port_dir>/qmp.sock`；
  cmd-agent 新增 qmp.rs（QMP 客户端，fsdev-add + device_add 一次性会话）；
  executor.rs 加 mount_folder/unmount_folder（MgmtCommand 队列复用 mgmt 单连接，
  挂起请求回 MountOk/Error）；cmd-agent-linker 加 FolderMounter trait；
  workspace open_paths cfg(ohos) 挂接。security-model=passthrough。
