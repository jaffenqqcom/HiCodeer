# OHOS 上打开文件夹/文件（文件选择器）不可用

## 问题描述 (Problem Description)

zcoder 移植到 OHOS 后，打开文件、打开目录、保存文件的系统对话框全部不可用。GPUI 的 `prompt_for_paths` / `prompt_for_new_path` 在 OHOS 平台未接通，接入 OHOS 文件选择器（`DocumentViewPicker`）后又遇到插件集成失败、目录选择被误拦截、权限未持久化等一系列问题。该问题是文件选择器能力在 OHOS 上的完整落地，涉及 Rust 插件桥、ArkTS 实现、HAP 打包配置、设备能力适配四个层面。

## 问题表现 (Symptoms)

- 打开目录报错：`file dialog failed: GenericFailure, Error: This device does not support folder selection (FolderSelection capability missing)`
- 初次实现时打开目录报错：`open-folder dialog does not support allow_many`
- 按 Ctrl+O 弹出文件选择器但只能选文件、不能选目录（`workspace::OpenFiles` 传 `directories=false`）
- 新增插件 HAR 后 hvigor 打包报 `00309001 Cannot import files from an external module using relative paths` + WARN `library will not be merged`
- 打开/保存后 URI 授权未持久化，应用退出后台后再访问失效
- 打开文件、保存文件在中间状态一度正常（picker 可弹），但打开目录始终失败
- 二次开启（重启）后目录树只剩打开过的文件，未访问的子目录/子文件全部消失；即使出现在目录树里双击也打不开（std::fs `Operation not permitted` EPERM）

## 问题原因 (Root Cause)

多环节叠加，按发现顺序：

1. **DocumentViewPicker 仅 ArkTS**：OHOS 的文件选择器不能 Rust 直调，必须走 openharmony-ability 的 **plugin 机制**（Rust facade `FilesBridgePlugin` + ArkTS `FilesPlugin` + HAP 接线）。

2. **vendored 插件对目录多选的错误假设**：
   - `plugin-files` Rust `validate()` 和 ArkTS `parseDialogOptions` 都主动拒绝 `open-folder + allow_many`；
   - ArkTS 侧 folder 分支硬编码 `maxSelectNumber = 1`；
   - `supportsFolderSelection()` 硬编码 `deviceType === "2in1"`（用户设备是 tablet）。
   而官方 API 事实：目录多选靠 `maxSelectNumber`（API 23 起取消目录数量限制）+ `allowsMulFolderSelection`（API 26+），`selectMode=FOLDER` 支持目录选择。

3. **设备能力检测被用错**：`DocumentSelectMode` 的 `@syscap` 是 `SystemCapability.FileManagement.UserFileService.FolderSelection`，官方标注 `selectMode` **"Only 2-in-1 devices are supported"**。用户设备（MatePad Edge，devicetype=tablet，API 24）`canIUse(FolderSelection)` 返回 false，vendored 插件据此抛错。但参考工程 warp-ohos 的 work directory 实现证明：**直接调用 `DocumentViewPicker.select({selectMode: 1})`，不做 FolderSelection 预检，让系统决定**——warp 只检查基础能力 `SystemCapability.FileManagement.UserFileService`。这是正确的用法，预检反而误拦截了实际可用的调用。

4. **HAP 集成失败（00309001）**：本地 HAR 被 hvigor 当作外部模块时，`file:` 目录引用跨模块触发 00309001。根因是**该 HAR 缺 `src/main/module.json`**（hvigor 按 `module.json` 判定模块有效性，vendored 插件只带 `module.json5`），且**未在工程级 `hap/build-profile.json5` 的 `modules` 数组声明**（对照 native_ability 的做法）。

5. **插件注册位置错误**：`app.register_plugin(FilesBridgePlugin)` 最初放在 `zed/src/main.rs` 的 `run_with_ability_entry`（应用启动入口），把平台能力耦合进 zed 应用层，职责错位。

6. **权限未持久化**：picker 返回的 URI 只有临时权限，退出后台即失效，需要 `OH_FileShare_PersistPermission`（`libohfileshare.so`）固化。

7. **缺 `ohos.permission.FILE_ACCESS_PERSIST` 权限声明，持久化授权从未真正生效**（重启后目录树消失的直接根因）：
   - warp 在 `module.json5` 的 `requestPermissions` 声明了 `ohos.permission.FILE_ACCESS_PERSIST`，而 zcoder 的 `module.json5` **一个权限都没声明**；
   - 后果是 `OH_FileShare_PersistPermission` 因权限校验失败（err=201 `ERR_PERMISSION_ERROR`）**从未成功持久化任何 URI**——首次打开能工作只是靠选择器临时授权掩盖；
   - 重启后临时授权失效，`OH_FileShare_ActivatePermission` 找不到持久化记录返回 `err=13900011`（`ERR_ENOMEM`），`ensure_root_authorized` 降级返回原路径；
   - worktree 根路径 std::fs 访问被沙箱拒绝（`Operation not permitted` EPERM）→ `Worktree::local` 的 `fs.metadata` 失败 + `scan_dir` 的 `read_dir` 失败，目录树扫描近乎为空。

## 解决方案 (Solution)

分四块落地：

**A. 插件机制接线**：
- `crates/plugin-files/src/lib.rs`：`FilesBridgePlugin` facade + `FileDialogOptions`/`FileDialogResponse`（`impl_bridge_napi_type!` 命名桥接类型）+ `FilesExt::show_file_dialog`
- ArkTS `plugins/files/src/main/ets/FilesPlugin.ets`：`FilesPlugin extends AsyncPluginBase`，`DocumentViewPicker.select/save` 实现
- HAP 接线：`hap/entry/oh-package.json5` 加 HAR 依赖、`EntryAbility.ets` 加 `bridgePlugins = [new LazyPlugin(() => new FilesPlugin())]`（显式返回类型 `(): BridgePlugin => new FilesPlugin()`，ArkTS 禁隐式返回类型）

**B. 目录多选与设备适配（参考 warp work directory）**：
- 删除 plugin-files `validate()` 与 `parseDialogOptions` 对 `open-folder + allow_many` 的拒绝
- folder 分支 `maxSelectNumber = allowMany ? 500 : 1`；API 26+ 设 `allowsMulFolderSelection=true`（接口断言 `DocumentSelectOptionsWithMulFolder` 绕过 compatibleSdkVersion=23 静态检查，模式同现成 `getSelectedIndex` API 14 处理）
- `supportsFolderSelection()` 放宽为按 `canIUse(FolderSelection syscap)` 判断
- **关键修复**：绕过 canIUse 预检，直接 `documentPicker.select({selectMode: FOLDER})` + try/catch 捕获系统真实错误（对齐 warp 的 PickDirectory 做法）
- `GPUI PathPromptOptions → OHOS dialog` 映射：`directories=true → OPEN_FOLDER`，`multiple → allow_many`

**C. HAP 集成修复（00309001）**：
- 新建 `plugins/files/src/main/module.json`（内容同 module.json5，严格 JSON）
- `hap/build-profile.json5` 的 `modules` 数组加 `plugin_files` 声明（`srcPath` 指向 crates 目录，`name` 与 module.json 一致）
- `hap/AppScope/syscap.json` 声明 `FolderSelection` 能力（工程此前完全缺失）
- 修复 `plugin-files/Cargo.toml`：`workspace = true` 改显式 path/version 依赖（zcoder 主 workspace 无对应 workspace.dependencies）

**D. 插件注册归位 + 权限固化**：
- `register_plugin` 集中到 `gpui_ohos` 平台层 `OhosPlatform::register_plugins`（`set_app` 统一调用），删除 zed 应用层注册；`zed/Cargo.toml` 移除 plugin-files 依赖
- `file_uri.rs` 合并 `OH_FileShare_PersistPermission/ActivatePermission` FFI，`path_from_uri` 转换 URI 时**自动固化授权**（打开文件/目录/保存都走此函数），无需外部额外调用

**E. 权限声明 + 重启时激活授权根（权限固化存储闭环）**：
- `hap/entry/src/main/module.json5` 的 `requestPermissions` 声明 `ohos.permission.FILE_ACCESS_PERSIST`（+ `string.json` 的 `perm_auth_reason`，对照 warp 权限段写法）——`PersistPermission`/`ActivatePermission` 的必需权限
- `file_uri.rs` 新增 `ensure_root_authorized(path)`：对沙箱外根路径（`/storage/Users/currentUser/` 前缀）执行 `OH_FileUri_GetUriFromPath` → `OH_FileShare_ActivatePermission` → `OH_FileUri_GetPathFromUri`，**重启后重新激活持久化授权**；沙箱内路径/任何失败原样返回（幂等）；抽出 `uri_to_local_path` 纯转换，`path_from_uri` 复用（避免重复 Persist）
- `create_local_worktree`（`crates/project/src/worktree_store.rs`，受保护文件，已授权）在调 `Worktree::local` 前对根路径调用 `ensure_root_authorized`——首次打开（临时授权）与重启恢复（持久授权激活）统一走此入口，叶子文件仍用根+相对路径 std::fs 访问，零改动
- `crates/project/Cargo.toml` 加 ohos-only 依赖 `openharmony-ability`

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-files/src/lib.rs` — 删除 `open-folder+allow_many` 校验；更新单测断言；Cargo.toml workspace 依赖改显式
- `crates/gpui_ohos/depend/openharmony-ability/plugins/files/src/main/ets/FilesPlugin.ets` — 删 parse 拒绝；folder 分支支持多选（maxSelectNumber + allowsMulFolderSelection）；绕过 canIUse 直接 select；try/catch 捕获真实错误（ArkTS `throw` 限 Error 类型）
- `crates/gpui_ohos/depend/openharmony-ability/plugins/files/src/main/module.json` — 新建，hvigor 模块有效性配置（hvigor 找 `module.json` 非 json5）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs` — 合并 `OH_FileShare_*` FFI；`path_from_uri` 内部自动持久化授权
- `crates/gpui_ohos/src/ohos/platform.rs` — `prompt_for_paths`/`prompt_for_new_path` 实现 + GPUI→OHOS 参数映射；`register_plugins` 集中注册；`read/write_from_clipboard` 接线
- `crates/gpui_ohos/Cargo.toml` — 加 `openharmony-ability-plugin-files` 依赖
- `crates/zed/src/main.rs` — 移除 `register_plugin(FilesBridgePlugin)`（应用层不承接平台能力）
- `crates/zed/Cargo.toml` — 移除 `openharmony-ability-plugin-files` 依赖
- `hap/build-profile.json5` — `modules` 数组声明 `plugin_files` 模块（对照 native_ability）
- `hap/AppScope/syscap.json` — 新建，声明 FolderSelection 系统能力
- `hap/entry/oh-package.json5` — 加 `@ohos-rs/ability-plugin-files` HAR 依赖
- `hap/entry/src/main/ets/entryability/EntryAbility.ets` — `bridgePlugins` 注册 `FilesPlugin`（显式返回类型）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/file_uri.rs` — 加 `OH_FileUri_GetUriFromPath` FFI + `ensure_root_authorized`（重启激活授权根）+ 抽 `uri_to_local_path` 纯转换
- `crates/project/src/worktree_store.rs` — `create_local_worktree` 对根路径调 `ensure_root_authorized`（ohos 分支）
- `crates/project/Cargo.toml` — ohos-only 依赖 `openharmony-ability`
- `hap/entry/src/main/module.json5` — `requestPermissions` 声明 `ohos.permission.FILE_ACCESS_PERSIST`
- `hap/entry/src/main/resources/base/element/string.json` — 加 `perm_auth_reason` 字符串资源

参考：[[ohos-debug-lessons]]
