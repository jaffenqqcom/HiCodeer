# OHOS 菜单「Open File…」选完文件无反应（单窗口平台拒绝新建窗口 + 错误被吞）

## 问题描述

OHOS 移植版从菜单 File → Open File… 打开系统文件选择器，选中文件（单个、多个都一样）并确认后，HiCodeer 没有任何反应：文件不打开、界面无变化、也没有错误提示。同一批菜单里的 Open Folder… 正常，拖文件进窗口、文件管理器里右键用 HiCodeer 打开也都正常。

## 问题表现

- 复现步骤：菜单 File → Open File… → 在系统文件选择器里选中一个（或多个）文件 → 确认
- 结果：无任何反应，无错误对话框、无界面变化
- 单选、多选文件都失败
- 同一版本下这些路径全部正常：拖拽文件进窗口；文件管理器里"右键 → 用 HiCodeer 打开"；菜单 File → Open Folder…
- 当时设备上装的是 release 包（info/debug 级日志已被编译剔除），hilog 里也看不到相关线索，因此定位完全依赖代码路径推导

## 问题原因

一条完整的因果链，每一步都有代码落点：

1. `crates/zed/src/zed.rs:1081` 的 `workspace::OpenFiles` handler 把 `create_new_window` 参数**硬编码为 `true`**（第 1092 行）：

```rust
workspace::prompt_for_open_path_and_open(
    workspace,
    workspace.app_state().clone(),
    PathPromptOptions {
        files: true,
        directories,
        multiple: true,
        prompt: None,
    },
    true, // create_new_window
    window,
    cx,
);
```

2. 选完文件后，`crates/workspace/src/workspace.rs:3624` 的 `open_workspace_for_paths` 带着 `OpenMode::NewWindow` 一路走到 `cx.open_window(...)`

3. OHOS 平台只有一块 XComponent 表面（只支持单窗口），`crates/gpui_ohos/src/ohos/platform.rs:516` 的 `open_window` 检测到已有窗口后直接拒绝（第 527 行）：

```rust
anyhow::bail!("OHOS supports a single window; cannot open a second window");
```

4. 这个 `Err` 在上层调用处被 `.log_err()` 消费掉，没有上抛到 UI 层，于是对外表现为"点了没反应"

为什么对照组都正常，这正好是定位的关键分叉：

- **Open Folder 能用**：它走的是 `workspace::Open`（菜单项见 `crates/zed/src/zed/app_menus.rs:118`），`create_new_window` 取自设置 `default_open_behavior`，默认值是 `existing_window`（`assets/settings/default.json:181`）→ 在当前窗口打开 → 正常
- **拖放 / 右键打开能用**：它们本来就是"在当前窗口打开文件"的路径，完全不经过 `open_window`
- **只有 Open File 不能用**：它是极少数把 `create_new_window` 写死为 `true` 的入口

## 解决方案

在 `open_workspace_for_paths` 里加一段 OHOS 专属降级（`crates/workspace/src/workspace.rs:3642`）：

```rust
// [ohos] Rendering goes through a single XComponent surface and the platform refuses to
// open a second window, so a requested new window has to land in the current one.
#[cfg(target_env = "ohos")]
if open_mode == OpenMode::NewWindow {
    open_mode = OpenMode::Activate;
}
```

为什么选在这里而不是把 zed.rs 里那个硬编码的 `true` 改掉：Open File 只是其中一个入口。欢迎页、Open Recent，以及其它把 `create_new_window` 传成 `true` 的调用点，在 OHOS 上都会以完全相同的方式静默失败；收敛在 workspace 层一次覆盖全部入口。这段降级与紧邻的 `workspace_is_empty → OpenMode::Activate`（第 3637 行）是同一类处理，语义上并列。

用户复测结果：打开文件、打开目录均正常。

## 修改文件

- `crates/workspace/src/workspace.rs` — `open_workspace_for_paths` 增加 OHOS 分支，把 `OpenMode::NewWindow` 降级为 `OpenMode::Activate`（新增 7 行）

[[ohos-debug-lessons]]
