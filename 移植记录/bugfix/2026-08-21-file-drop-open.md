# OHOS 拖放文件进 zcoder 打不开（FileDrop 全链路 + paths 载荷刷新修复）

## 问题描述

OHOS 移植版（zcoder）从文件管理器拖文件到 zcoder 窗口上方，界面能正确显示"将文件放上/下/左/右"的 split 预览（说明拖拽状态和 hover 命中都正常），但**松开鼠标后文件始终打不开**。

## 问题表现

- 拖动文件到 zcoder 上方：split 方向预览正常显示（`group_drag_over` 生效）
- 鼠标释放（onDrop）：文件不打开，无报错提示
- 双击/普通点击等其他输入正常，仅拖放打不开

## 问题原因

根因在 GPUI 核心的 `FileDropEvent::Entered` 处理逻辑（`crates/gpui/src/window.rs`）：

```rust
if !cx.restore_platform_drag(source_window) && cx.active_drag.is_none() {
    cx.active_drag = Some(AnyDrag { value: paths, ... });
}
```

`Entered` **只在 `active_drag.is_none()` 时建立拖拽状态**。而 OHOS 实现为了让 split 预览尽早显示，在拖拽进入窗口（`onDragEnter`）时先用**空 paths** 的 `Entered` 建立了 `active_drag`；等到真正释放（`onDrop`）时，携带真实文件路径的第二次 `Entered` 因 `active_drag.is_some()` 被直接跳过——**`active_drag` 里的 paths 始终是空的**，`on_drop` 触发后 `handle_external_paths_drop` 拿到的 `ExternalPaths` 为空，`open_paths` 打开空列表，所以文件打不开。

桌面 X11 不触发此问题：它先发 `Pending` 再发带真实 paths 的 `Entered`（首次即建立 active_drag），路径不会在建立后被跳过。

## 解决方案

两部分：

1. **GPUI 核心修复（`crates/gpui/src/window.rs`）**：`Entered` 遇到已存在的 `active_drag` 时，如果其载荷是 `ExternalPaths`（外部文件拖拽），用新 paths **刷新载荷**（value / view / cursor_offset）。仅刷新外部文件拖拽，不影响 macOS 平台拖拽恢复（`restore_platform_drag` 分支）和窗口内部拖拽（非 `ExternalPaths` 不更新）。

```rust
FileDropEvent::Entered { position, paths } => {
    self.mouse_position = position;
    let source_window = self.handle.window_id();
    if !cx.restore_platform_drag(source_window) && cx.active_drag.is_none() {
        cx.active_drag = Some(AnyDrag { value: Arc::new(paths.clone()), ... });
    } else {
        // Refresh the payload of an in-flight external file drag so the drop
        // listener receives the actual paths.
        let is_external = cx.active_drag.as_ref().is_some_and(|drag| {
            drag.value.downcast_ref::<ExternalPaths>().is_some()
        });
        if is_external {
            let view: AnyView = cx.new(|_| paths.clone()).into();
            let refreshed = Arc::new(paths);
            if let Some(drag) = &mut cx.active_drag {
                drag.value = refreshed;
                drag.view = view;
                drag.cursor_offset = position;
            }
        }
    }
    PlatformInput::MouseMove(/* left-button MouseMove keeps the hover chain */)
}
```

2. **OHOS 拖放链路（plugin 架构，事件驱动）**：
   - ArkTS `FileDropPlugin.ets` 透明 overlay 绑定 `onDragEnter / onDragMove / onDrop`，把拖拽事件经 `invokeNativeSync` 推到 Rust
   - Rust `plugin-filedrop` 在主线程解析事件，构造 `FileDropEventData`（Enter / Move / Drop）回调 gpui_ohos
   - gpui_ohos 分三段映射到 GPUI：`Enter` → 空 paths `FileDrop::Entered`（建立 drag 态、让 split 预览可见）；`Move` → `MouseMove` + `FileDrop::Pending`（保持 hover 链）；`Drop` → 带真实 paths 的 `Entered` + `Submit`（刷新载荷并同步触发 on_drop）

验证链路（真机两次拖放）：`on_drop TRIGGERED` → `handle_external_paths_drop paths=1` → `open_paths resolved ok=1 err=0`，文件成功打开。

## 修改文件

- `crates/gpui/src/window.rs` — `FileDropEvent::Entered` 载荷刷新修复（唯一一处 gpui 核心修改，必需）
- `crates/gpui_ohos/depend/openharmony-ability/plugins/filedrop/src/main/ets/FileDropPlugin.ets` — 新增透明拖放 overlay 插件
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-filedrop/src/lib.rs` — 新增 filedrop 桥接插件
- `crates/gpui_ohos/src/ohos/window.rs` — `dispatch_filedrop_enter / dispatch_filedrop_move / dispatch_drop_files`（含 `path_from_uri` 授权解析）、`register_platform_event_handlers`
- `crates/gpui_ohos/src/ohos/platform.rs` — `OhosPlatform::register_plugins` 注册 `FileDropBridgePlugin`
- `hap/` 相关配置 — 插件模块声明与依赖

[[ohos-debug-lessons]]
