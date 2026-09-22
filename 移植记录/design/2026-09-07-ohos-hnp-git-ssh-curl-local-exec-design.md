# HiCodeer git / ssh / curl 本地 HNP 执行设计（去掉 VM 转发）

> **现状核对（2026-09-22）**：本文的**路由机制仍然有效**——`util::command::init_local_tools()`
> 在启动时快照 `/data/app/bin`，命中快照的程序本地 fork+exec（`crates/util/src/command/ohos.rs:80`、
> `:116`），其余交给命令后端执行；`apply_local_tool_env` 也仍然**不注入** `LD_LIBRARY_PATH`
> （`ohos.rs:440-447` 的注释明写理由）。
>
> **已变化的部分**：随 HAP 分发的 HNP 载荷现在只有两个——`hap/entry/src/main/module.json5:150-159`
> 声明 `git.hnp`(private) + `hicodeerd.hnp`(public)，`hap/entry/hnp/arm64-v8a/` 下也只有
> `git.hnp` 与 `hicodeerd.hnp` 两个文件，**没有 openssh.hnp / curl.hnp**。因此下文关于
> ssh / curl 两个包的装配、resfile/curl 残留、以及"产物含 3 个 hnp"的描述属于**当时的设计记录**，
> 不再对应现状。命令执行后端也已从"cmd-agent → OpenEuler VM"换成宿主侧的 `hicodeerd`
> 守护进程（`crates/gpui_ohos/depend/cmd-agent/hicodeerd`），详见
> `2026-09-08-ohos-qemu-runtime-design.md`。

## 1. 背景与目标

HiCodeer 在 OHOS 上因沙箱禁止 exec 外部 ELF，所有子进程命令（LSP / 终端等）原本经
`util::command` → 远程 VM 执行。这引入 20s 阻塞握手、VM 依赖、路径映射等一堆代价。

**目标**：把 **git、ssh、curl** 做成 **private 类型 HNP**（HarmonyOS Native Package）内嵌 HAP，安装后 `util::command` 在 OHOS 上**直接本地 fork+exec** 本机 hnp 二进制，不再转发 VM。

**驱动决策**：
- 载荷（git.hnp / openssh.hnp / curl.hnp）从 warp-oh / 验证可装的 github zcoder 拷贝，不自建。
- 本地工具路由从「编译期常量列表」最终演化为「**启动扫描 /data/app/bin 快照自动检测**」：`/data/app/bin` 里存在的程序一律本地，本机没有的自动走 VM。加新 hnp（如 busybox）**零代码改动**。
- ssh/curl 自带动态库的加载**使用官方 RUNPATH 机制**，不设进程级 `LD_LIBRARY_PATH`（该变量优先级高于 RUNPATH，反而会截胡包内库）。

## 2. 关键事实核验（承重事实）

### 2.1 HNP 内部结构与 ELF 动态段（readelf 实测）

解压 warp-oh 的 hnp 后 `readelf -d`：

- **openssh.hnp**：`bin/{ssh,scp,sftp,ssh-keygen}` + `lib/{libcrypto.so.3, libz.so.1}`。所有可执行 `DT_RUNPATH=$ORIGIN/../lib`；libcrypto/libz 自身 `DT_RUNPATH=$ORIGIN`（兄弟库互定位）。musl 链接（`/lib/ld-musl-aarch64.so.1`）。
- **curl.hnp**：`bin/curl` + `lib/` 全套传递闭包（libcurl.so.4 / libssl.so.3 / libcrypto.so.3 / libnghttp2… ~23 个）。curl 可执行 `DT_RUNPATH=$ORIGIN/../lib`；每个库自身 `DT_RUNPATH=$ORIGIN`。musl 链接。
- **git.hnp**：`bin/git` + `libexec/git-core` + `share`，**无 lib/ 目录**。git 主程序只 `NEEDED libc.so`（自包含），RUNPATH 无关紧要。git 的子命令经 `GIT_EXEC_PATH` 定位（libexec/git-core）。

musl 动态链接器对 `$ORIGIN` 的解析经 `/proc/self/exe`（内核 execve 跟随符号链接后的**真实路径**），所以即使通过 `/data/app/bin/ssh` 软链执行，`$ORIGIN` 仍展开到真实 hnp bin 目录，`../lib` 命中自带库。

### 2.2 官方规范一致

OpenHarmony HNP 官方要求 native 可执行自带 .so 时编译加 `-Wl,-rpath=$ORIGIN/../lib -Wl,--disable-new-dtags`（生成 DT_RUNPATH），`.so` 随包放 `lib/`。**warp-oh 拷来的包已经满足官方规范，无需重打**。官方不提供跨包"公共库"查找——公共依赖要么同包自带，要么是 musl libc 这类系统库。

### 2.3 本地路由快照机制

`/data/app/bin` 是 private HNP 二进制软链目录，**进程存活期间不变** → 启动扫一次缓存，后续每次 spawn 零 stat。

## 3. 问题与根因（按出现顺序）

### 3.1 问题 A：ssh 报 `Error loading shared library libcrypto.so.3`

现象链：
1. 早期：`ssh user@…` → `Error loading shared library libcrypto.so.3: …`（cannot open）。
2. 改进程 env 注入 openssh lib 目录后：变成 `symbol not found OpenSSL_version`。
3. 用户终端 `echo $LD_LIBRARY_PATH` = `/data/storage/el1/bundle/entry/resources/resfile/curl`。

**根因（不是包的问题）**：
- 旧版本 HAP 的 resfile 里打包了 `curl/` 目录（含一套 so），启动代码把该目录塞进了进程级 `LD_LIBRARY_PATH`。
- 动态加载器查找顺序 **LD_LIBRARY_PATH 先于 DT_RUNPATH**。于是 ssh 自带正确 RUNPATH 也没用——环境变量先把它顶掉，从 resfile/curl 加载到旧/不完整 libcrypto → 先找不到、后符号缺失。
- **resfile 残留坑**：打包源 `src/main/resources/resfile/` 早已删掉 curl（只剩 ca-bundle.crt + cmd-agentd），但 hvigor 增量构建不清 `intermediates/res/default/resources/resfile/curl`，新 HAP **仍然把旧 curl 整目录打进去**（与仓库 CLAUDE.md 记载"旧缓存 server 打进 HAP"是同一类坑）。设备上旧安装的 resfile/curl 也一直存在。

### 3.2 问题 B：`util::command` 本地/远程路由的可维护性

最初按用户拍板实现为编译期常量 `LOCAL_EXEC_COMMANDS = ["git","ssh","curl"]`。用户随后要求**去掉列表**：凡 `/data/app/bin` 存在的程序都本地、不存在的走 VM；且**启动时扫描一次存快照**，避免每次 spawn 读目录。这样以后加 busybox.hnp / zsh.hnp 等零代码改动。

### 3.3 问题 C：签名链路三连坑

1. **hvigor 内建 SignHap 必失败**：`hap/build-profile.json5` 的 default product 挂 `signingConfig: default`，其 material.storeFile 指向 `/storage/Users/currentUser/Documents/ohos/config/default_zcoder…p12`（HarmonyOS IDE 侧路径，VM 上访问不到，且该目录实际没有这个 p12）→ `00303107 Invalid storeFile value`，assembleHap 走不到 HNP 注入段。
2. **keystore password incorrect**（`11014003`）：`hap/cerfile` 里的 p12/cer/p7b 与「验证可装的那套」（github zcoder clone，明文口令 mMahnPUzBCojAkQk）**sha 不一致**——cerfile 被 AGC/DevEco 换成了另一套新证书，密码对应关系不同。
3. 因此每次签名被迫手动传 env / 手动 sign-app，不够"固话"。

## 4. 解决方案

### 4.1 路由：删除常量列表，改启动快照自动检测

`crates/util/src/command/ohos.rs`：
- 删 `const LOCAL_EXEC_COMMANDS`。新增：

```rust
static LOCAL_TOOL_NAMES: OnceLock<Vec<String>> = OnceLock::new();

pub fn init_local_tools() {
    LOCAL_TOOL_NAMES.get_or_init(|| {
        // 扫 LOCAL_TOOL_BIN_DIRS（/data/app/bin），收集 is_executable_file 的名字
    });
}
```

- `local_exec_matches(program)` 改为查 `LOCAL_TOOL_NAMES` 快照：命中 → `spawn_local`（本地）；未命中 → 原远程 executor（VM 重定向）。快照为空（无 hnp 的 dev 构建）则全部走 VM。
- `local_tool_programs() -> &'static [String]` 返回快照，供启动诊断。
- 启动调用点：`launch_app()` 在 `ensure_terminal_shell_env` 后立即 `util::command::init_local_tools()`（幂等）。

### 4.2 ssh/curl 库加载：撤 LD_LIBRARY_PATH hack，走官方 DT_RUNPATH

- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` `ensure_terminal_shell_env`：删除**两处**进程级 `LD_LIBRARY_PATH` 注入——(a) resfile/curl 目录，(b) 早期为绕过加的 hnp `../lib` 前置。进程 env 不再含 `LD_LIBRARY_PATH`，子进程继承干净环境，靠包自带 RUNPATH 加载。
- `crates/util/src/command/ohos.rs` `apply_local_tool_env`：删除 LD_LIBRARY_PATH 段；**保留** PATH 前置（git 找子命令）与 git 的 `GIT_EXEC_PATH` / `GIT_TEMPLATE_DIR`。

### 4.3 resfile/curl 残留根治

删除 hvigor 中间产物残留目录 `hap/entry/build/default/intermediates/res/default/resources/resfile/curl`，重跑 assembleHap 后新 HAP 不再含 `resources/resfile/curl`。

### 4.4 HNP 打包装配固化进 bundle-ohos

`script/bundle-ohos`（默认执行，无需开关）：
1. hvigor assembleHap（product 不挂 signingConfig → 出 unsigned HAP）。
2. python 注入：读产物 `module.json` 的 `hnpPackages`，把 `hap/entry/hnp/arm64-v8a/<pkg>.hnp` 追加为 `hnp/arm64-v8a/<pkg>`（保留源文件 external_attr 执行位）。
3. `hap-sign-tool.jar sign-app` 用 `hap/cerfile` 的 default_zcoder* 三件套整体重签（**口令等默认值固话进脚本**，env 可覆盖），输出 `entry-default-signed.hap`（install 脚本指向的稳定路径）。

`hap/build-profile.json5`：default product **移除 `signingConfig: "default"`**（签名全部交脚本，不再用 build-profile material，避开 /storage 不存在路径与加密串密码问题）。`hap/cerfile` 的 p12/cer/p7b **用验证可装的那套覆盖**（github zcoder 同源，sha 一致）。

`hap/entry/src/main/module.json5`：`hnpPackages` 声明 `git.hnp`（private）。
（当时的方案是同时声明 `git.hnp` / `openssh.hnp` / `curl.hnp` 三个 private 包；现状见文首现状核对。）

## 5. 修改文件

- `crates/util/src/command/ohos.rs`（含 ohos）— 删 `LOCAL_EXEC_COMMANDS` 列表，新增 `LOCAL_TOOL_NAMES`(OnceLock)+`init_local_tools()`；`local_exec_matches` 查快照；`apply_local_tool_env` 删 LD_LIBRARY_PATH 注入，保留 PATH/git env；`local_tool_programs` 改返 `&[String]`；错误文案去掉列表引用。
- `crates/util/src/command.rs` — 导出 `init_local_tools`。
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`（含 ohos）— `ensure_terminal_shell_env` 删两处 `LD_LIBRARY_PATH` 注入；`launch_app()` 加 `util::command::init_local_tools()` 调用。
- `hap/entry/src/main/module.json5` — `hnpPackages` 增 `curl.hnp`。
- `script/bundle-ohos` — 签名默认值固化为 cerfile 三件套（P12/CER/P7B/ALIAS/PWD），去掉 workspace/debugkey.p12 默认。
- `hap/build-profile.json5` — default product 移除 `signingConfig`（含连带 JSON 结构修正）。
- `hap/entry/hnp/arm64-v8a/curl.hnp` — 新增，从 warp-oh 拷贝（sha256 对齐）。
- `hap/cerfile/default_zcoder…{p12,cer,p7b}` — 用验证可装的 github zcoder 套覆盖。
- 删除中间产物 `hap/entry/build/default/intermediates/res/default/resources/resfile/curl/`（一次性）。

## 6. 验证

- `./script/bundle-ohos` 无 env 单命令跑通：Rust 编译 → hvigor unsigned assembleHap → 注入 HNP → sign-app success。
- 产物 `entry-default-signed.hap` 含模块 `hnpPackages` 声明的那几个包（当时是 `{git,openssh,curl}`，
  现状是 `{git,hicodeerd}`），**不含** `resources/resfile/curl`。
- `hdc install -r` 成功（install bundle successfully）。
- 启动日志预期：`init_local_tools: 3 on-device tool(s)` + `[diag] local tool {git,ssh,curl} -> /data/app/bin/…`；终端 `ssh -V` / `curl -I https://…`（CA 走 `SSL_CERT_FILE=resfile/ca-bundle.crt`）/ `git --version` 均本地、无 LD_LIBRARY_PATH 注入。

## 7. 遗留与后续

- 设备实测（ssh/curl/git 本地）由用户在真机终端验证。
- 官方链接库机制已确认包合规；若 ssh 后续仍需不依赖任何环境的绝对可靠，正路是重打 openssh.hnp 时确认链接 `-Wl,-rpath=$ORIGIN/../lib -Wl,--disable-new-dtags`（当前包已满足，通常无需重打）。
- 未来加 hnp（busybox/zsh/…）：拷贝载荷 + module.json5 声明 + 重编即可，路由自动生效（快照检测），无 util 代码改动。

> 关联 OHOS 调试经验：见 [[移植记录/bugfix]] 下同类文件系统/签名/resfile 中间产物教训；resfile 中间产物残留属 hvigor 增量不清坑，与 CLAUDE.md「SERVER_RESFILE_DIR 放错会打进旧 server」同类。
