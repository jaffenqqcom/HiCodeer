# OHOS 数据根与 HOME 落地：用户选定目录贯通设置路径与进程环境

## 问题描述

zcoder 的数据根与进程环境 `HOME` 一度都落在应用沙箱
（`/data/storage/el2/base/haps/entry/files`），而用户自己的目录
（如 `/storage/Users/currentUser/HiCodeer`）用不起来。表现是一连串"看起来互不相干"的故障：

- 设置里改了 QEMU 开关，重启不生效；
- agent 建不了 `~/.codebuddy`；
- 终端里 `$HOME` 指向沙箱。

根因是同一件事：**数据根已经由"用户选定目录"决定，但几个消费方还在各自算自己的根**。

## 问题表现

- 用户设置文件写在 `<用户目录>/HiCodeer/config/settings.json`，启动流程却完全读不到
  （QEMU 照旧启动）。hilog（`qemu-boot` 标签）：

  ```
  qemu-boot: start_command_backend: base_path=/data/storage/el2/base/haps/entry/files
  qemu-boot: read_launch_qemu_settings: no settings.json; defaults (enabled=true)
  ```

- 终端与 agent 的 `HOME` 是沙箱路径，工具按 `$HOME` 找配置/建目录全部落到应用私有目录，
  重装即丢。

## 问题原因

### 一、ETS 早就把选定目录传下来了，Rust 侧只接了一半

`NativeAbility.ets` 一直把用户选的 custom dir 作为 `homeDirectory` 传给 native 侧，
`start_zed_main(base_path, home_directory)` 也早就收了这个参数并用于 zed 的 paths —— 但
**进程环境 `HOME` 仍写死 `base_path`**。这是链上唯一没接上的一环，也是最容易被忽略的一环：
数据根改对了，环境变量还在旧位置。

### 二、设置路径由启动期自己扫沙箱

命令后端的选型发生在 `SettingsStore` 初始化之前，所以
`launch-zed/src/qemu_runtime.rs` 自己实现了一份"找 settings.json"。这份实现只扫沙箱
（`base_path.ancestors().nth(3)`），而 `<用户目录>/HiCodeer` 与
`/data/app/el2/100/base/<bundle>` 是两个互不包含的挂载点，**扫描永远不可能命中**，
且失败是静默的（返回默认值）。

### 三、加载时机与数据根冲突

`EntryAbility` 原先 `defaultPage=true`，native 模块在用户选目录**之前**就加载了，而
`module.init → launch_app → start_zed_main` 这条链必须在启动时就知道数据根。

## 解决方案

### 关键点

数据根只解析一次、写进一个记录文件（`<filesDir>/custom_data_dir`），所有消费方都读它，
不再各自推导。

### 修改

**一、ETS 侧：先定目录，再加载 native**

- **新增 `hap/entry/src/main/ets/entryability/Setup.ets`**
  - `HomeDirectory.read/write`（`:44`/`:58`）：读写 `<filesDir>/custom_data_dir` 记录；
  - `HomeDirectory.accept`（`:107`）：用 `DocumentViewPicker`（FOLDER 模式）选根目录后，
    `mkdir <root>/HiCodeer`、`fileShare.persistPermission` 持久化授权、写记录，返回目录；
  - `HomeDirectory.resolveExisting`（`:73`）：读记录 → 若在用户公共目录前缀下则
    `fileShare.activatePermission` 重激活授权 → `statSync` 校验仍是可访问目录；
    目录被删或不可达时返回空，让设置页重新出现，而不是沿用坏路径；
  - `Setup` 页面（`:127`）给用户一个明确的选目录入口（含启动画面同款图标与提示文案）。
- **`hap/entry/src/main/ets/entryability/EntryAbility.ets`**
  - `defaultPage = false`（原来是 `true`）：主页面改由自己显式加载；
  - `onCreate` 里先 `this.holdNativeLoad()`：延后 `libzcoder.so` 的加载，因为
    `module.init → launch_app → start_zed_main` 必须已经知道数据根；
  - `onWindowStageCreate` → `prepareHomeDirectory`（`:76`）：有可用记录就直接用；
    否则 `loadContentByName(Setup.RouteName)` 让用户选，选到后
    `loadContentByName(MainPageRouteName)` 再 `releaseNativeLoad()`；失败路径打
    `hilog.error` 并照常释放，保证应用还能起来。

**二、Rust 侧：数据根只解析一处**

- **`crates/zed/src/main.rs`**
  - `start_zed_main(base_path, home_directory)` 增加第二个参数（`:216`）；
  - `resolve_home_directory`（`:250`）：`ohos_file_geturi::ensure_root_authorized` 重激活
    picker 授权（ETS 侧落盘的是授权，Rust 侧每次启动要重新激活）→
    `create_dir_all` 确保存在 → 返回可用目录；
  - `paths::set_custom_data_dir(home)`，并用 `write_home_directory_record`（`:270`）
    把结果幂等写回 `<base_path>/custom_data_dir`（ETS 侧写的是同一个值）；
  - 目录不可用时打 `log::error` 并**回落沙箱**，避免让 `set_custom_data_dir` 对不可达路径
    panic；
  - 顺带删掉 `lsp_install_ohos` 旁路：数据根统一后，LSP 下载目录回到
    `paths::languages_dir()`（`:622`），不再需要单独推导。

**三、进程环境：`HOME` 取用户选定目录**

- **`crates/gpui_ohos/depend/launch-zed/src/launch_app.rs`**
  - `ensure_terminal_shell_env(base_path)` → `ensure_shell_env(base_path, home_directory)`；
  - `HOME` 取 `home_directory`（非空），**只有它为空时**才回落 `base_path`（首次启动、
    用户尚未选目录）。注释写明判据：git config、`~/.codebuddy`、shell 历史这类状态需要一个
    用户拥有、能脱离 zcoder 访问、且重装后仍然存在的目录，沙箱目录三条都不满足；
  - `SHELL`/`USER`/`PATH`/CA 相关逻辑未动；
  - `launch_app()` 把 `app.home_directory()` 一并传给 `start_zed_main`（`:31`）。

**四、设置路径：读记录，扫描降级为兜底**

- **`crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs`**：`find_settings_json` 开头改为
  先按 `<base_path>/custom_data_dir` 记录的 home 解析 `<home>/config/settings.json`，
  沙箱扫描保留为"没有记录"时的兜底。详见 [[2026-09-11-ohos-qemu-setting-path-mismatch.md]]。

### 验证

- `./script/bundle-ohos` 全量编译通过（6m48s），装机成功（`install bundle successfully` +
  `APP_STARTED`）。
- 设置路径那一段有 hilog 证据链：`find_settings_json: home=… candidate=…` →
  `found …/settings.json` → `enabled=false` → 不再启动 QEMU，回落 OHOS 后端
  （见上引报告）。
- HOME 实际生效面的**边界**（重要）：zcoder 自身进程、本地 shell、FIFO 目录、本地工具 fork
  拿到的都是新 HOME。但 **codebuddy 继承的是 zcoderd 的环境**——`ps -ef` 实测
  zcoderd 由 hishell 的 zsh 手工拉起（`zcoderd ← zsh ← com.huawei.hmos.hishell`），
  而 `util::command::build_spec` 只把**显式指定的 env** 放进下发报文、pty 请求不带 env。
  因此本次改动**传导不到 codebuddy**；若要覆盖，需把 HOME 随每次命令下发（待定项）。

### 走过的弯路（供后来者避免）

- 一开始想把 home 目录作为参数从 `launch_app` 一路传下去，后来发现 `read_launch_qemu_settings`
  还有第二个调用点（QEMU 重启钩子）手上只有 `base_path` —— 读记录文件对两处都适用，
  于是改成"记录文件 + 读记录"。
- 一度为设置路径**新增**了一个 `find_launch_settings_json` 函数，被指出"路径变了而已，
  不该新增接口"；正确做法是直接修 `find_settings_json` 本身 —— 全局设置文件的路径解析
  只应该有一处。
- 差点漏掉进程环境 `HOME`：改完数据根后，设置能读到了、paths 也对了，但终端和工具仍按
  沙箱 `HOME` 办事。**"数据根"与"进程环境 HOME"是两件事，必须一起改。**

## 修改文件

- **新增** `hap/entry/src/main/ets/entryability/Setup.ets` — home 目录的选择、记录、
  授权持久化与复用
- `hap/entry/src/main/ets/entryability/EntryAbility.ets` — `defaultPage=false`、
  `holdNativeLoad`/`releaseNativeLoad` 时序、`prepareHomeDirectory`
- `hap/entry/src/main/ets/pages/Index.ets`、`resources/base/element/string.json`、
  `resources/base/profile/main_pages.json` — 设置页路由与文案
- `crates/zed/src/main.rs` — `start_zed_main` 增参、`resolve_home_directory`、
  `write_home_directory_record`、`HOME_DIRECTORY_RECORD_FILE`；移除 `lsp_install_ohos` 旁路
- `crates/gpui_ohos/depend/launch-zed/src/launch_app.rs` — `ensure_shell_env`，
  `HOME` 取用户选定目录
- `crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs` — `find_settings_json` 按记录
  解析设置文件

## 关联

- [[2026-09-11-ohos-qemu-setting-path-mismatch.md]] — 设置路径这一支的完整证据链
- `移植记录/design/2026-09-10-ohos-data-home-directory-design.md` — 数据根与 home 目录的设计
- **未决**：把 HOME 随命令下发（exec 写 `spec.env`、pty 请求补 env 字段），使 agent 与
  终端不再依赖 zcoderd 的启动环境

[[ohos-debug-lessons]]
