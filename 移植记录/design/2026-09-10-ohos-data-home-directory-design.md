# OHOS 数据主目录（custom_data_dir）迁到用户可见目录设计

> 正式归档：2026-09-10。本文取代此前「Onboarding 里选 LSP 安装路径」那一套方案：
> LSP 不再单独选路径，改为整个数据根（`data_dir`）落到用户目录，LSP 顺带解决。

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

启动链路：

```
ArkTS EntryAbility (NativeAbility)
  └ onCreate → initializeSession → NativeModuleLoader.load("zcoder") → module.init(...)
       └ #[ability] launch_app(app)                        crates/gpui_ohos/depend/launch-zed/src/launch_app.rs:7
            └ zed::start_zed_main(app.base_path())         launch_app.rs:31
                 └ paths::set_custom_data_dir(base_path + "/zcoder")   crates/zed/src/main.rs:218-230
  └ onWindowStageCreate → loadWindowStageContent → loadContentByName("NativeAbility")
```

要点：

- `launch_app` 发生在 **`onCreate` 阶段**，早于 `onWindowStageCreate`。因此「在 XComponent
  被挟持之前」拦截，实质是**推迟 `initializeSession`**（它会加载 libzcoder 并跑
  `module.init`）。
- `NativeAbility` 的生命周期操作走 `SerialTaskQueue`（严格 FIFO 串行，
  `native_ability/src/main/ets/runtime/SerialTaskQueue.ets:16-22`）。**在该队列里等待一个由
 窗口阶段才解锁的门闩会死锁**，拦截逻辑必须放在队列之外。

`paths` 语义（`crates/paths/src/paths.rs`）：

- `set_custom_data_dir`（:103-119）：`OnceLock`；若 `data_dir`/`config_dir` 已被初始化**直接
  panic**（:104-106）；内部 `create_dir_all` + `canonicalize`。
- `data_dir()`（:144-167）：优先 `CUSTOM_DATA_DIR`。
- `config_dir()`（:122-141）：`CUSTOM_DATA_DIR/config`。
- `languages_dir()`（:445-448）：`data_dir()/languages`。

授权机制（`crates/gpui_ohos/depend/ohos-file-geturi/src/ohos_file_geturi.rs`）：

- `path_from_uri(uri)`（:69-72）= `persist_permission(uri)` + `uri_to_local_path(uri)`。
  **这就是「固化存储」** —— NDK `OH_FileShare_PersistPermission`，跨重启有效。
- `ensure_root_authorized(path)`（:106-129）= 路径 → `path_to_uri` → `activate_permission`
  → 本地路径。重启后重新激活；幂等；只处理 `/storage/Users/currentUser` 前缀（:99, :110）。
- 两者互补：**选目录时 persist 一次，每次启动 activate 一次**。

现有 LSP 特化（本次要删）：

- `crates/zed/src/lsp_install_ohos.rs`（整文件）：读 `settings::lsp_install_path` →
  `ensure_root_authorized` → `<root>/zcoder/languages`。
- `crates/zed/src/main.rs:8`（`mod lsp_install_ohos;`）、`:577-582`（cfg 分支）。
- `crates/settings_content/src/settings_content.rs:266-269`（`lsp_install_path` 字段）。
- `crates/settings/src/settings.rs:205-216`（`lsp_install_path` 访问器）。

工作区中存在的半成品重构（本次一并按新设计整理，不回退）：

- `crates/onboarding/src/basics_page.rs:554-649`（`render_data_dir_section` /
  `pick_data_folder`）、`crates/onboarding/src/onboarding.rs:276-290`（`on_finish` 守卫）
  引用了三个**仓库中不存在**的符号：`settings::custom_data_dir`、
  `content.custom_data_dir`、`paths::ohos_redirect::redirect_to`。
- `crates/language/src/language_registry.rs:817` 注释引用了不存在的 `paths::redirect_data_root`。

QEMU 挂载（`crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs`）：

- 只**静态挂载**沙箱 `files/`（`sandbox_mount = base_path`，:337-343）。
- 其它目录靠 `WorkdirAwareExecutor::ensure_mounted`（:471-497）按 **cwd** 惰性挂载。
  LSP 二进制是以 argv 绝对路径传的，不是 cwd —— 所以 `data_dir` 挪出沙箱后 guest 很可能
  看不见，这正是本次要补的环节。

## 4. 决定（已与用户确认）

1. 拦截点放在 **hap 的 ArkTS 代码**里，在 libzcoder 被加载之前；界面是 ArkTS 页面。
2. 界面背景用**程序启动画面的背景**：`$r('app.color.start_window_background')`
   （`hap/entry/src/main/resources/base/element/color.json` 与 `dark/element/color.json`
   各一份，深浅色自带，无需额外判断）。
3. 界面内容：一个按钮（文案 `Pick up`）+ 一行字 `Set Home Directory of HiCodeer`。
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

### 5.1 ArkTS 启动拦截

**新增可覆盖钩子（`native_ability`，路径含 ohos，可直接改）**

`NativeAbility` 增加一个「原生加载门闩」：

- 私有 `nativeLoadGate: Promise<void>` + 其 resolver。
- `initializeSession` 所在的生命周期操作在真正加载模块前 `await` 该门闩。
- 新增 `protected releaseNativeLoad(): void` 放行。
- 默认行为：门闩初始即已放行 —— 不覆盖该钩子的使用方（demo、其它宿主）行为完全不变。

**`EntryAbility`（`hap/entry/src/main/ets/entryability/EntryAbility.ets`）**

- `defaultPage` 改为 `false`：窗口内容由自己决定加载哪一页。
- 覆盖 `onWindowStageCreate(windowStage)`，**先完成主目录判定，再 `super`**：

  ```
  onWindowStageCreate(windowStage):
      async
        1. 读 context.filesDir/custom_data_dir；读到路径 P
        2. 若 P 非空：P → URI → 试访问
             可访问 → 记住 P
        3. 否则（无记录 / 不可访问）：
             loadContent('pages/HomeDirectoryPicker')   // 设置主目录页
             等用户选目录 A（取消则停留该页）
             A → URI → persist 授权（固化存储）
             记住 A/HiCodeer
        4. releaseNativeLoad()                 // 放行 onCreate 里等着的加载
        5. 记为 this.dataHomePath
        6. await super.onWindowStageCreate(windowStage)
  ```

  关键：**第 4 步必须在 `super.onWindowStageCreate` 之前**。此时窗口阶段操作还没入队，
  串行的 `SerialTaskQueue` 不会被卡死；放行后 ability-create 操作继续，窗口阶段操作随后
  入队并按序执行。

- `createInitContext(moduleName)` 增加 `dataHomePath: this.dataHomePath ?? ""`，与 `basePath`
  同形状。

**正常路径（主目录可用）**：`loadContentByName("NativeAbility")` 由
`NativeAbility.loadWindowStageContent`（`defaultPage === false` 时不加载）之外的流程负责 ——
`EntryAbility` 在 `super.onWindowStageCreate` 之后调用 `windowStage.loadContentByName(Entry.RouteName)`，
或直接在判定为「可用」时先加载该页。二者等价，实现取一。（若采用后者，`defaultPage` 保持
`false`，不会重复加载。）

**设置主目录页（新文件 `hap/entry/src/main/ets/pages/HomeDirectoryPicker.ets`）**

- 背景 `$r('app.color.start_window_background')`；居中按钮 `Pick up` + 下方一行
  `Set Home Directory of HiCodeer`。
- 点击 → 目录选择器（ArkTS `@ohos.file.picker`，folder 模式）→ 取第一个 URI。
- 选完后：`@ohos.fileshare` 的 `persistPermission`（固化）→ 建 `A/HiCodeer` → 写
  `filesDir/custom_data_dir`（内容 `A/HiCodeer`）→ 回调通知 `EntryAbility` 继续。
- 取消 → 停留原页，不继续加载原生。

### 5.2 Rust 侧

- `AbilityInitContext` 加字段 `dataHomePath`（`crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs:38-49`
  的 `from_object` + 访问器），ArkTS 侧同名字段经 NAPI 传入。
- `launch_app.rs`：`zed::start_zed_main(app.base_path(), app.data_home_path())`。
- `crates/zed/src/main.rs` 的 `start_zed_main(base_path, data_home)`：

  ```
  if data_home 非空:
      p = ensure_root_authorized(data_home)
      set_custom_data_dir(p)
      写 base_path/custom_data_dir = p        // 与 ArkTS 写同一个值；幂等
  else:
      沿用现有 base_path + "/zcoder" 兜底      // 不崩，只是退回沙箱
  ```

- `ensure_root_authorized` 的调用发生在 `set_custom_data_dir` 之前、任何 `data_dir()` 之前，
  满足 `OnceLock` 的 panic 约束（`paths.rs:103-106`）。
- 目录创建由 `set_custom_data_dir` 内部的 `create_dir_all` 完成（`paths.rs:109`）；ArkTS 侧
  也建一次以便写记录文件前确认可写，两边都是幂等的 `create_dir_all`。

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

在 `qemu_runtime.rs` 现有静态挂载处（:337-343，`sandbox_mount = base_path`）追加 `A/HiCodeer`
的静态挂载。此时 `A` 已经由 ArkTS 决定并经 init context 传入，`launch_app` 一开始就知道。

**挂载点归一化**（guest Linux 允许嵌套挂载，但「先挂子目录、后挂父目录」会把子目录遮蔽，
所以必须计算）：

- 新增候选 `R`：若已有挂载点 `M` 是 `R` 的祖先 → 跳过（已被覆盖）；
- 若 `R` 是某些已有挂载点 `M` 的祖先 → 这些 `M` 已被 `R` 覆盖，移除它们、改挂 `R`；
- 否则独立挂载 `R`；
- 下发顺序保证祖先先于后代。

宿主与 guest 使用**相同路径**（与现有 `sandbox_mount` 一致），不做路径映射。

## 6. 时序（首次启动）

```
系统 → EntryAbility.onCreate
         └ enqueue ability-create：await nativeLoadGate → initializeSession
             （门闩未放行，libzcoder 尚未加载）
系统 → EntryAbility.onWindowStageCreate
         └ 读 filesDir/custom_data_dir → 无 → loadContent(设置主目录页)
用户 → 点 Pick up → 选 A
         └ persistPermission(A 的 URI) → 建 A/HiCodeer → 写 filesDir/custom_data_dir
         └ releaseNativeLoad()
         └ super.onWindowStageCreate → 窗口阶段操作入队
             └ ability-create 继续：加载 libzcoder → module.init → launch_app
                 └ start_zed_main(base_path, "A/HiCodeer")
                     └ ensure_root_authorized → set_custom_data_dir
             └ 窗口阶段操作：loadWindowStageContent（defaultPage=false，不加载）
             └ EntryAbility 加载 "NativeAbility" 页 → GPUI 接管
```

## 7. 风险与降级

- **授权被系统回收 / 路径失效**：ArkTS 判定为不可访问 → 重新显示设置主目录页，不静默退回沙箱。
- **`ensure_root_authorized` 只认 `/storage/Users/currentUser` 前缀**（`ohos_file_geturi.rs:99,110`）。
  用户若选到该前缀之外的目录，重新激活不会生效。→ 需在设备上确认选择器实际可选范围；必要时在
  设计上把可选范围约束在该前缀内（见第 9 节）。
- **`set_custom_data_dir` 的 panic 约束**：ArkTS 侧判定路径可访问时**绝不能触碰** Rust 的
  `data_dir()`/`config_dir()`（历史坑：`start_zed_main` 之前调用会 panic）。判定全部在 ets 完成。
- **门闩死锁**：`releaseNativeLoad()` 必须发生在 `super.onWindowStageCreate` 之前（或在不处于
  `SerialTaskQueue` 中的上下文里），否则串行队列互等。
- **QEMU 未启用时**：不需要挂载，走设备侧 zcoderd（HNP），`A/HiCodeer` 同机同路径，直接可用。
- **`config_dir` 跟着挪到 `A/HiCodeer/config`**：用户设置、keymap 等都随之迁移，这是本方案的
  既定语义（旧数据不迁移，重装即可）。

## 8. 验证

- 构建：`script/bundle-ohos`。
- 设备（首次）：清数据强制首启 → 出「设置主目录」页（背景与启动画面一致，深浅色各看一次）→
  点 `Pick up` 选目录 → 自动进入编辑器。
- 设备（再次）：杀进程重启 → **不出现**该页，直接进编辑器；hilog 可见 `ensure_root_authorized`
  的成功日志。
- 目录落位：`A/HiCodeer/` 下出现 `config/`、`languages/`、`extensions/` 等；
  `filesDir/custom_data_dir` 内容为 `A/HiCodeer`。
- LSP 端到端：打开工程 → LSP 装到 `A/HiCodeer/languages/<server>` 并能启动。
- QEMU 模式：guest 内 `ls A/HiCodeer` 可见；LSP 从该路径启动成功。
- 边界：选择器取消 → 停留设置页；选到前缀之外的目录 → 记录现象（见第 9 节）。

## 9. 实施期需核实项

1. ArkTS 侧目录选择 API 的确切用法（`@ohos.file.picker` 的 folder 模式；`DocumentSelectMode.FOLDER`
   的 API 版本要求）与 `@ohos.fileshare` persist 的返回码语义。
2. 选择器实际可选范围是否都落在 `/storage/Users/currentUser` 前缀内。
3. 「设置主目录」页在 2in1/tablet 上的窗口尺寸与安全区（`deviceTypes` 见
   `hap/entry/src/main/module.json5`）。
4. Multi-module 场景（当前 `moduleName` 单模块）下门闩的语义（本方案按单模块设计）。
