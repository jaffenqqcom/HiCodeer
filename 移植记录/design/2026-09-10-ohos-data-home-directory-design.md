# OHOS 数据主目录（custom_data_dir）迁到用户可见目录设计

> 正式归档：2026-09-10。本文取代此前「Onboarding 里选 LSP 安装路径」那一套方案：
> LSP 不再单独选路径，改为整个数据根（`data_dir`）落到用户目录，LSP 顺带解决。
>
> **现状核对（2026-09-22）**：本文的设计**已全部落地**，但实现位置与部分机制与下文所写不同，
> 读时以下列现状为准：
>
> 1. **选目录页是 ArkTS 的 `Setup` 页**，不是下文的 `pages/HomeDirectoryPicker.ets`：
>    `hap/entry/src/main/ets/entryability/Setup.ets`（`RouteName = 'Setup'`，:11）。
>    界面 = 启动图标的 `Image` + 按钮 + 提示文案（文案走字符串资源
>    `app.string.set_home_directory_button` / `set_home_directory_hint`，:170-176），
>    不再有"直接在 ets 里调 picker"之外的逻辑——picker / persist / 建目录 / 写记录文件
>    都收在 `Setup.ets` 的 `HomeDirectory` 类里（`read` :44、`write` :58、
>    `resolveExisting` :73、`accept` :107）。
> 2. **授权不再有 Rust 侧 crate**：`crates/gpui_ohos/depend/ohos-file-geturi/` 已整体退场，
>    `ensure_root_authorized` 全仓无引用。固化/激活改由 ArkTS 直接调
>    `@ohos.fileshare` 的 `persistPermission` / `activatePermission`；Rust 侧只剩
>    `path_from_uri`（来自外部依赖 `openharmony-ability`）与 `std::fs::create_dir_all`。
> 3. **传参字段名是 `home_directory`**，不是 `dataHomePath`：
>    `launch_app` 读 `app.home_directory()`（`launch_app.rs:27`、`:42`），
>    定义在**外部依赖** `openharmony-ability`（fork `jaffenqqcom/openharmony-ability-zed`
>    tag `v1.0.0-beta.1-zed.1`，`crates/gpui_ohos/Cargo.toml:41`）的
>    `crates/ability/src/app.rs:449`，本仓库内没有该 crate 源码。
> 4. **门闩 API 叫 `holdNativeLoad()` / `releaseNativeLoad()`**，基类实现，不是下文所述的
>    私有 `nativeLoadGate` + `releaseNativeLoad()` 自建字段；
>    入口在 `EntryAbility.onWindowStageCreate` → `prepareHomeDirectory`
>    （`hap/entry/src/main/ets/entryability/EntryAbility.ets:48`、`:68-101`）。
> 5. **Rust 侧数据根解析在 `launch_app.rs`**，不在 `crates/zed/src/main.rs`：
>    `launch_app.rs:54` 的 `start_zed_main` → `:62` `resolve_home_directory` →
>    `:64` `paths::set_custom_data_dir`；兜底目录已由 `zcoder` 改为 `hicodeer`
>    （`launch_app.rs:76`）。`zed` 侧入口现为 `zed::hicodeer_main()`（`launch_app.rs:83`）。
> 6. **§5.3 的删除项已全部执行**：`crates/zed/src/lsp_install_ohos.rs` 不存在，
>    `lsp_install_path` 设置项已删（`settings_content.rs` 该位置现为 `qemu_enabled`），
>    onboarding 的 `render_data_dir_section` / `pick_data_folder` 与
>    `language_registry.rs` 里的悬空注释均已清掉。
> 7. 下文 §3「授权机制」「现有 LSP 特化（本次要删）」「工作区中存在的半成品重构」
>    三小节描述的是**改造前的状态**，保留作背景。
> 8. 数据根内部布局见 `2026-09-14-ohos-data-root-layout.md`；QEMU 侧见
>    `2026-09-08-ohos-qemu-runtime-design.md`。

## 1. 背景与问题

zcoder 启动时 `start_zed_main` 把 `data_dir` 设为 HAP 私有沙箱
`<base_path>/zcoder`（`base_path` = OHOS UIAbilityContext 的 `filesDir`，即
`/data/storage/el2/base/haps/entry/files`）。所有下载物都落在这个私有目录下：
`languages/`（LSP）、`extensions/`、`external_agents/`、`node/`、`debug_adapters/`、
`config/`、agent 数据库。

问题：该目录**外部程序访问不到**。OHOS 沙箱禁止 exec 外部程序，LSP 等命令要交给
zcoderd（HNP 进程 / QEMU guest）执行，它们够不着 el2 私有目录，LSP 因此不可用。

## 2. 目标

1. `data_dir` 落到一个**用户选定、双方都能访问**的目录：用户在启动时选一个目录 `A`，
   `data_dir` = `A/HiCodeer`。
2. 这个选择在首次启动时用专门的界面问一次，之后每次启动自动复用。
3. 删除 Onboarding 里的 LSP / 数据目录设置，以及 LSP 下载路径的 OHOS 特化
   （`data_dir` 已经外部可见，特化没有存在意义）。
4. QEMU guest 里也能看见 `A/HiCodeer`（补上一个此前缺失的挂载环节）。

## 3. 现状（关键代码定位）

启动链路（**现状**）：

```
ArkTS EntryAbility（继承 @ohos-rs/ability 的 NativeAbility）
  └ onCreate → holdNativeLoad() → super.onCreate        EntryAbility.ets:48-49
       （native 模块加载被门闩挡住，libhicodeer 尚未加载）
  └ onWindowStageCreate → prepareHomeDirectory()        EntryAbility.ets:68-69
       ├ resolveExisting 命中（读 filesDir/custom_data_dir → 激活授权 → stat 可用）
       │    → 记为 homeDirectory
       └ 未命中 → loadContentByName('Setup') → awaitSelection() → 用户选目录
            → loadContentByName(MainPageRouteName) → releaseNativeLoad()  :99-100
  └ 门闩放行 → 原生模块加载 → #[ability] launch_app(app)   launch_app.rs:18-19
       └ zed::start_zed_main(app.base_path(), app.home_directory())   launch_app.rs:42
            └ paths::set_custom_data_dir(<A>/HiCodeer)    launch_app.rs:64
                 └ zed::hicodeer_main()                   launch_app.rs:83
```

要点：

- `launch_app` 现在发生在**门闩放行之后**（即 `onWindowStageCreate` 期间），不再像改造前那样在
  `onCreate` 阶段。拦截的实质仍是**推迟原生模块加载**——加载即跑 `module.init` → `launch_app`
  → `start_zed_main`，而那条链必须已经知道用户的数据根。
- `NativeAbility` 的生命周期操作走严格 FIFO 的串行队列（`SerialTaskQueue`）。该实现现随
  `@ohos-rs/ability` 分发（`hap/entry/oh-package.json5:9` 指向
  `target/ohos-arkts/openharmony-ability/native_ability`），本仓库内无源码。
  **在该队列里等待一个由窗口阶段才解锁的门闩会死锁**，所以 `releaseNativeLoad()` 必须在
  `super.onWindowStageCreate` 之前调用（`EntryAbility.ets:99-100`），此时窗口阶段操作尚未入队。

`paths` 语义（`crates/paths/src/paths.rs`）：

- `set_custom_data_dir`（:103-119）：`OnceLock`；若 `data_dir`/`config_dir` 已被初始化**直接
  panic**（:104-106）；内部 `create_dir_all` + `canonicalize`。
- `data_dir()`（:144-167）：优先 `CUSTOM_DATA_DIR`。
- `config_dir()`（:122-141）：`CUSTOM_DATA_DIR/config`。
- `languages_dir()`（:445-448）：`data_dir()/languages`。

授权机制（**改造前状态；该 crate 已整体退场**，现状见文首第 2 条。原位置
`crates/gpui_ohos/depend/ohos-file-geturi/src/ohos_file_geturi.rs`）：

- `path_from_uri(uri)`（:69-72）= `persist_permission(uri)` + `uri_to_local_path(uri)`。
  **这就是「固化存储」** —— NDK `OH_FileShare_PersistPermission`，跨重启有效。
- `ensure_root_authorized(path)`（:106-129）= 路径 → `path_to_uri` → `activate_permission`
  → 本地路径。重启后重新激活；幂等；只处理 `/storage/Users/currentUser` 前缀（:99, :110）。
- 两者互补：**选目录时 persist 一次，每次启动 activate 一次**。

现有 LSP 特化（**改造前状态；下文所列的删除项现已全部删除**，见文首第 6 条）：

- `crates/zed/src/lsp_install_ohos.rs`（整文件）：读 `settings::lsp_install_path` →
  `ensure_root_authorized` → `<root>/zcoder/languages`。
- `crates/zed/src/main.rs:8`（`mod lsp_install_ohos;`）、`:577-582`（cfg 分支）。
- `crates/settings_content/src/settings_content.rs:266-269`（`lsp_install_path` 字段）。
- `crates/settings/src/settings.rs:205-216`（`lsp_install_path` 访问器）。

工作区中存在的半成品重构（**改造前状态；现已清理**，`basics_page.rs` / `onboarding.rs` /
`language_registry.rs` 均已搜不到下述符号）：

- `crates/onboarding/src/basics_page.rs:554-649`（`render_data_dir_section` /
  `pick_data_folder`）、`crates/onboarding/src/onboarding.rs:276-290`（`on_finish` 守卫）
  引用了三个**仓库中不存在**的符号：`settings::custom_data_dir`、
  `content.custom_data_dir`、`paths::ohos_redirect::redirect_to`。
- `crates/language/src/language_registry.rs:817` 注释引用了不存在的 `paths::redirect_data_root`。

QEMU 挂载（`crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs`）：

- **静态挂载两处**：沙箱 `files/`（`sandbox_mount: base_path`，:332）与用户数据根
  （`data_mount`，:333；来源是 `read_data_root(base_path)`，:303/:726）——后者正是本次要补的
  环节，现已落地。
- 其它目录靠 `WorkdirAwareExecutor::ensure_mounted`（:464）按 **cwd** 惰性挂载。
  LSP 二进制是以 argv 绝对路径传的，不是 cwd，所以惰性挂载覆盖不到——这正是数据根必须做
  **静态**挂载的原因。

## 4. 决定（已与用户确认）

1. 拦截点放在 **hap 的 ArkTS 代码**里，在 libhicodeer.so 被加载之前；界面是 ArkTS 页面。
2. 界面背景用**程序启动画面的背景**：`$r('app.color.start_window_background')`
   （`hap/entry/src/main/resources/base/element/color.json` 与 `dark/element/color.json`
   各一份，深浅色自带，无需额外判断）。
3. 界面内容：启动图标（`$r('app.media.foreground')`）+ 一个按钮 + 一行提示文字
   （文案走字符串资源 `app.string.set_home_directory_button` / `app.string.set_home_directory_hint`；
   现状见 `Setup.ets:163-182`）。
4. 用户选定路径 `A` 后：在 `A` 下建子目录 `HiCodeer`，`data_dir` = `A/HiCodeer`。
5. **新建目录名用 `HiCodeer`；既有路径与既有代码里的 `zcoder` 字样保留不改。**
6. 路径经**与现有 LSP 相同的接口**传给 Rust：扩充 `AbilityInitContext`，形状同 `basePath`。
7. 授权在 `ensure_root_authorized` **之前**先固化：选目录时 persist（与 `path_from_uri` 同源
   的 ArkTS 接口），Rust 启动时再 `ensure_root_authorized` 重新激活。
8. 记录文件：HAP 程序内部目录下（`filesDir`）一个名为 `custom_data_dir` 的文件，内容为路径
   `A/HiCodeer`。
9. 旧数据不迁移（会重新安装程序）。
10. `A/HiCodeer` 要**静态挂载**进 QEMU guest；挂载点需做归一化计算。
11. 不含 "ohos" 的文件改动一律 `#[cfg(target_env = "ohos")]` 包裹，桌面行为与 schema 不变。

## 5. 设计

### 5.1 ArkTS 启动拦截（现状）

**基类门闩（`NativeAbility`，现随 `@ohos-rs/ability` 分发）**

`NativeAbility` 提供一对方法，构成「原生加载门闩」：

- `holdNativeLoad()`：把原生模块加载挡在门闩后（`EntryAbility.ets:48` 调用）。
- `releaseNativeLoad()`：放行（`EntryAbility.ets:75`、`:100` 调用）。

默认行为：门闩初始即已放行 —— 不覆盖该钩子的使用方（demo、其它宿主）行为完全不变。

**`EntryAbility`（`hap/entry/src/main/ets/entryability/EntryAbility.ets`）**

- `defaultPage = false`（:31）：窗口内容由自己决定加载哪一页。
- `onCreate` 先 `holdNativeLoad()` 再 `super.onCreate`（:48-49）；门闩未放行，libhicodeer 不加载。
- 覆盖 `onWindowStageCreate(windowStage)`，**先完成主目录判定，再 `super`**（:68-78、:85-101）：

  ```
  prepareHomeDirectory(windowStage):
      1. resolveExisting(context)
           读 filesDir/custom_data_dir → activatePermission → statSync 验活
           可用 → this.homeDirectory = 记录值
      2. 否则（无记录 / 不可访问）：
           await windowStage.loadContentByName(Setup.RouteName)   // 设置主目录页
           picked = await Setup.awaitSelection()                  // 取消则停在 Setup 页
           this.homeDirectory = picked
      3. await windowStage.loadContentByName(MainPageRouteName)
      4. releaseNativeLoad()                 // 放行 onCreate 里等着的加载
  ```

  关键：**第 4 步必须在 `super.onWindowStageCreate` 之前**（`EntryAbility.ets:99-100` 位于
  `.then()` 内，`super` 在 `:71`）。此时窗口阶段操作还没入队，串行的 `SerialTaskQueue`
  不会被卡死；放行后原生初始化继续，窗口阶段操作随后入队并按序执行。

- `createInitContext(moduleName)` 带上 `homeDirectory`，与 `basePath` 同形状（字段定义在外部
  依赖 `openharmony-ability` 的 `crates/ability/src/app.rs:449`）。

**正常路径（主目录可用）**：主页面由 `EntryAbility` 在放行前显式加载
（`loadContentByName(MainPageRouteName)`，:99）。`MainPageRouteName` 从 `@ohos-rs/ability` 导入；
基类在 `defaultPage === false` 时不自己加载，故不会重复。

**设置主目录页（`hap/entry/src/main/ets/entryability/Setup.ets`）**

- 背景 `$r('app.color.start_window_background')`，居中：启动图标 + 按钮 + 提示文字（:163-182）。
- 点击 → `picker.DocumentViewPicker.select`（`DocumentSelectMode.FOLDER`，:132-139）→ 取第一个 URI。
- `HomeDirectory.accept(root, uri, context)`（:107-124）：`fs.mkdirSync(<root>/HiCodeer)`
  → `fileShare.persistPermission`（固化）→ `HomeDirectory.write` 写 `filesDir/custom_data_dir`
  → 回调通知 `EntryAbility` 继续。
- 取消 → 停留原页，不继续加载原生（:141-143、:145-147）。
- `HomeDirectory.resolveExisting`（:73-104）：每次启动先 `activatePermission` 再 `statSync` 验活，
  目录被删或不可访问则返回 `''` 让设置页重出；路径不在 `/storage/Users/currentUser` 前缀内时
  直接返回记录值（:78-81）。

### 5.2 Rust 侧（现状）

- `OpenHarmonyApp::home_directory()`（**外部依赖** `openharmony-ability` 的
  `crates/ability/src/app.rs:449`）提供该字段，ArkTS 侧同名字段经 NAPI 传入。
- `launch_app.rs:42`：`zed::start_zed_main(app.base_path(), app.home_directory())`。
- `start_zed_main` 位于 **`launch_app.rs:54`**（不在 `crates/zed/src/main.rs`）：

  ```
  match resolve_home_directory(home_directory):        // create_dir_all 试可用性，:90
      Some(d) -> paths::set_custom_data_dir(d)         // :64
                 写 <base_path>/custom_data_dir = d    // 与 ArkTS 写同一个值；幂等，:103
      None    -> paths::set_custom_data_dir(<base_path>/hicodeer)   // :76 兜底，不崩
  zed::hicodeer_main()                                  // :83
  ```

- **不再有 Rust 侧的授权激活步骤**：`ensure_root_authorized` 已随 `ohos-file-geturi` crate
  整体退场，激活改由 ArkTS 的 `activatePermission` 在 `resolveExisting` 里完成（见 §5.1）。
- `set_custom_data_dir` 在首次调用后置位，晚于任何 `data_dir()` / `config_dir()` 调用会 panic
  （`paths.rs:103-106`）；因此主目录解析必须发生在 `zed::hicodeer_main()` 之前。
- 目录创建由 `set_custom_data_dir` 内部的 `create_dir_all` 完成（`paths.rs:109`）；ArkTS 侧
  也建一次（`Setup.ets:110`），两边都是幂等的 `create_dir_all`。
- 兜底路径已由 `zcoder` 改为 `hicodeer`（`launch_app.rs:76`）。

### 5.3 删除项

- `crates/zed/src/lsp_install_ohos.rs`：整文件删除。
- `crates/zed/src/main.rs:8`：删 `mod lsp_install_ohos;`。
- `crates/zed/src/main.rs:577-582`：cfg 分支恢复为单句
  `let lsp_download_dir = paths::languages_dir().clone();`。
- `crates/settings_content/src/settings_content.rs:266-269`：删 `lsp_install_path` 字段。
- `crates/settings/src/settings.rs:205-216`：删 `lsp_install_path` 访问器。
- `crates/onboarding/src/basics_page.rs`：删 `render_data_dir_section`（:554-586）与
  `pick_data_folder`（:593-649），`render_settings_section` 恢复为无条件走
  `render_import_settings_section`（即删掉 :546-549 的 OHOS 分派）。
- `crates/onboarding/src/onboarding.rs:276-290`：删 `on_finish` 的 OHOS 守卫。
- 清理指向不存在符号的注释：`basics_page.rs` 里的 `paths::ohos_redirect`、
  `language_registry.rs:817` 的 `paths::redirect_data_root`。
- 半成品残留的 `settings::custom_data_dir` / `content.custom_data_dir` 引用随上述删除一并消失；
  不再新增这两个符号（新的持久化落在 ArkTS 读、Rust 写的 `filesDir/custom_data_dir` 文件上）。

### 5.4 QEMU guest 挂载

在 `qemu_runtime.rs` 现有静态挂载处（`:332` 的 `sandbox_mount = base_path`）旁边追加
`A/HiCodeer` 的静态挂载（`:333` 的 `data_mount`）。此时 `A` 已经由 ArkTS 决定并经 init
context 传入，`launch_app` 一开始就知道。

**挂载点归一化**（guest Linux 允许嵌套挂载，但「先挂子目录、后挂父目录」会把子目录遮蔽，
所以必须计算）：

- 新增候选 `R`：若已有挂载点 `M` 是 `R` 的祖先 → 跳过（已被覆盖）；
- 若 `R` 是某些已有挂载点 `M` 的祖先 → 这些 `M` 已被 `R` 覆盖，移除它们、改挂 `R`；
- 否则独立挂载 `R`；
- 下发顺序保证祖先先于后代。

宿主与 guest 使用**相同路径**（与现有 `sandbox_mount` 一致），不做路径映射。

## 6. 时序（首次启动，现状）

```
系统 → EntryAbility.onCreate
         └ holdNativeLoad() → super.onCreate（原生加载排队等门闩）
系统 → EntryAbility.onWindowStageCreate → prepareHomeDirectory
         └ resolveExisting：读 filesDir/custom_data_dir → 无记录 → 返回 ''
用户 → loadContentByName('Setup') → 点按钮 → 选目录 A
         └ HomeDirectory.accept：mkdir A/HiCodeer → persistPermission(A 的 URI)
                                 → 写 filesDir/custom_data_dir
         └ this.homeDirectory = A/HiCodeer
         └ loadContentByName(MainPageRouteName)
         └ releaseNativeLoad()
         └ super.onWindowStageCreate → 窗口阶段操作入队
             └ 门闩放行 → 加载 libhicodeer → module.init → launch_app
                 └ start_zed_main(base_path, "A/HiCodeer")
                     └ resolve_home_directory → set_custom_data_dir
                     └ 写 <base_path>/custom_data_dir（幂等）
                     └ zed::hicodeer_main() → GPUI 接管
```

## 7. 风险与降级

- **授权被系统回收 / 路径失效**：ArkTS 判定为不可访问 → 重新显示设置主目录页，不静默退回沙箱。
- **授权激活只认 `/storage/Users/currentUser` 前缀**（`Setup.ets:17` 的
  `USER_PUBLIC_PATH_PREFIX`）。`resolveExisting` 对前缀外的记录值跳过 `activatePermission`
  直接返回（`Setup.ets:78-81`），这类目录只靠持久授权、激活不会重做。→ 需在设备上确认选择器
  实际可选范围；必要时在设计上把可选范围约束在该前缀内（见第 9 节）。
- **`set_custom_data_dir` 的 panic 约束**：ArkTS 侧判定路径可访问时**绝不能触碰** Rust 的
  `data_dir()`/`config_dir()`（历史坑：`start_zed_main` 之前调用会 panic）。判定全部在 ets 完成。
- **门闩死锁**：`releaseNativeLoad()` 必须发生在 `super.onWindowStageCreate` 之前（或在不处于
  `SerialTaskQueue` 中的上下文里），否则串行队列互等。
- **QEMU 未启用时**：不需要挂载，走设备侧 hicodeerd（HNP），`A/HiCodeer` 同机同路径，直接可用。
- **`config_dir` 跟着挪到 `A/HiCodeer/config`**：用户设置、keymap 等都随之迁移，这是本方案的
  既定语义（旧数据不迁移，重装即可）。

## 8. 验证

- 构建：`script/bundle-ohos`。
- 设备（首次）：清数据强制首启 → 出「设置主目录」页（背景与启动画面一致，深浅色各看一次）→
  点「设置主目录」按钮选目录 → 自动进入编辑器。
- 设备（再次）：杀进程重启 → **不出现**该页，直接进编辑器；`Setup` TAG（`hilog`）无报错。
- 目录落位：`A/HiCodeer/` 下出现 `config/`、`languages/`、`extensions/` 等；
  `filesDir/custom_data_dir` 内容为 `A/HiCodeer`。
- LSP 端到端：打开工程 → LSP 装到 `A/HiCodeer/languages/<server>` 并能启动。
- QEMU 模式：guest 内 `ls A/HiCodeer` 可见；LSP 从该路径启动成功。
- 边界：选择器取消 → 停留设置页；选到前缀之外的目录 → 记录现象（见第 9 节）。

## 9. 实施期需核实项

> 实现落地后的现状：第 1、2 条已确定，见 `Setup.ets`（`DocumentSelectMode.FOLDER` 于 `:135`，
> `persistPermission` 的调用与错误分支于 `:114-121`，前缀判定于 `:17`/`:78`）；
> 第 3、4 条仍按原设计未变。

1. ArkTS 侧目录选择 API 的确切用法（`@ohos.file.picker` 的 folder 模式；`DocumentSelectMode.FOLDER`
   的 API 版本要求）与 `@ohos.fileshare` persist 的返回码语义。
2. 选择器实际可选范围是否都落在 `/storage/Users/currentUser` 前缀内。
3. 「设置主目录」页在 2in1/tablet 上的窗口尺寸与安全区（`deviceTypes` 见
   `hap/entry/src/main/module.json5`）。
4. Multi-module 场景（当前 `moduleName` 单模块）下门闩的语义（本方案按单模块设计）。
