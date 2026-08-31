# OHOS 设置界面按 Esc 挂死（keymap 绑定 workspace::CloseWindow）

## 问题描述

OHOS 移植版（zcoder）设置 tab 打开后按 Esc 键界面挂死无响应。桌面端 keymap（`default-linux.json`）在 `SettingsWindow` 键盘上下文里绑定了 `"escape": "workspace::CloseWindow"`，该绑定在 OHOS 单窗口模型下触发异常关闭流程导致界面挂死。

## 问题表现

- 设置 tab 打开后按 Esc 界面挂死，应用无响应
- 其他 tab 按 Esc 无影响（其他 tab 没有 CloseWindow 绑定）
- 挂死只发生在 `SettingsWindow` 上下文（它有 `key_context("SettingsWindow")`）

## 问题原因

- `default-linux.json` 的 `SettingsWindow` context 绑定 `"escape": "workspace::CloseWindow"`（另有 `"ctrl-w"` 同样绑定 CloseWindow）
- 桌面端按 Esc 走 CloseWindow 是正常的窗口关闭；但 OHOS 上 `SettingsWindow` 是主窗口的一个 **tab**（不是独立窗口），`CloseWindow` 的关闭流程与 OHOS 单窗口 XComponent 模型不兼容，触发后关闭流程卡死，界面失去响应
- 全局 `ctrl-shift-w`（第 25 行）以及 `SkillCreator`、`SkillCreator > Editor` 的 `ctrl-w`（1603、1612 行）同样绑定 CloseWindow，在 OHOS 上都是潜在挂死源

## 解决方案

**新增 OHOS 专用 keymap，删除全部 5 处 `workspace::CloseWindow` 绑定**：

1. 新建 `assets/keymaps/default-ohos.json`（复制 `default-linux.json`，删除 5 处 CloseWindow）：
   - 全局 `ctrl-shift-w` → CloseWindow（第 25 行）
   - `SettingsWindow` context：`ctrl-w`、`escape` → CloseWindow（1447-1448 行）
   - `SkillCreator` `ctrl-w` → CloseWindow（1603 行）
   - `SkillCreator > Editor` `ctrl-w` → CloseWindow（1612 行）

   其余绑定与 `default-linux.json` 保持一致，不裁剪其他功能。

2. `crates/settings/src/settings.rs` 的 `DEFAULT_KEYMAP_PATH` 加 OHOS 分支指向 `default-ohos.json`，其余平台保持原样：

   ```rust
   #[cfg(target_env = "ohos")]
   pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-ohos.json";
   #[cfg(all(target_os = "macos", not(target_env = "ohos")))]
   pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-macos.json";
   // ... windows / linux 分支补 not(target_env = "ohos")
   ```

3. `SettingsAssets` 已 `#[include = "keymaps/*"]`，自动打包 `default-ohos.json`，无需额外配置。

## 修改文件

- `assets/keymaps/default-ohos.json` — 新建：复制 default-linux.json，删除全部 5 处 `workspace::CloseWindow` 绑定
- `crates/settings/src/settings.rs` — `DEFAULT_KEYMAP_PATH` 加 `#[cfg(target_env = "ohos")]` 分支，其余分支补 `not(target_env = "ohos")` 互斥

[[ohos-debug-lessons]]
