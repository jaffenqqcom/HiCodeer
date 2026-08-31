# OHOS 上复制粘贴（剪贴板）不可用

## 问题描述 (Problem Description)

zcoder（Zed 移植到 HarmonyOS NEXT / OpenHarmony）在设备上运行时，编辑器内复制/粘贴文本完全无效——复制无反应、粘贴无内容，系统剪贴板无法读写。GPUI 平台的剪贴板接口（`Platform::read_from_clipboard` / `write_to_clipboard`）在 OHOS 平台层未实现，导致编辑器无法与系统剪贴板交互。

## 问题表现 (Symptoms)

- 在 zcoder 编辑器选中文本按复制快捷键（Ctrl+C），无任何效果
- 粘贴（Ctrl+V）无内容插入
- 设备系统剪贴板中的内容无法读取，也无法写入
- 与桌面端（macOS/Linux）行为不一致，桌面端复制粘贴正常

## 问题原因 (Root Cause)

- GPUI 的 `OhosPlatform` 没有实现剪贴板接口：`read_from_clipboard` 返回 `None`、`write_to_clipboard` 为空操作，编辑器的复制/粘贴动作走平台层时落空。
- OHOS 的 ArkTS 剪贴板 API（`@ohos.pasteboard.getData()` / `setData()`）要求 `ohos.permission.READ_PASTEBOARD` 权限，普通三方应用拿不到该权限，无法直接读写剪贴板。
- 参考工程 warp-ohos 已验证：**NDK 层 C API `OH_Pasteboard_GetData`（`libpasteboard.so`）可以绕过 ArkTS 的权限校验**，无需 `READ_PASTEBOARD` 权限即可读写剪贴板。这为 Rust FFI 直调提供了可行路径。

## 解决方案 (Solution)

采用 Rust FFI 直调 NDK C API 的方式，新建**通用剪贴板模块**，零 GPUI/Zed 依赖，可移植到其他 OHOS 软件：

1. **新建 `openharmony-ability/crates/ability/src/clipboard.rs`**：
   - `#[link(name = "pasteboard")]` 声明 `OH_Pasteboard_Create/Destroy/GetData/SetData`
   - `#[link(name = "udmf")]` 声明 UDMF API（`OH_UdsPlainText_*` / `OH_UdmfRecord_*` / `OH_UdmfData_*`）
   - **写**：`OH_Pasteboard_Create` → `OH_UdsPlainText_Create` + `SetContent` → `OH_UdmfRecord_Create` + `AddPlainText` → `OH_UdmfData_Create` + `AddRecord` → `OH_Pasteboard_SetData`，逐级销毁
   - **读**：`OH_Pasteboard_GetData` → `OH_UdmfData_GetPrimaryPlainText` → `OH_UdsPlainText_GetContent` → CStr 转 String
   - 异常分支均有 `log::error!`；模块纯 `std` + `log`，无任何框架依赖

2. **`lib.rs` 注册**：`mod clipboard;` + `pub use clipboard::*;`

3. **`OhosPlatform` 接线**（`gpui_ohos/src/ohos/platform.rs`）：
   - `read_from_clipboard` → `openharmony_ability::read_text().map(ClipboardItem::new_string)`
   - `write_to_clipboard` → 提取文本后 `openharmony_ability::write_text(&text)`

**关键设计**：用户明确要求剪贴板 ability 必须**通用**（`clipboard.rs` 用 `String` 而非 GPUI `ClipboardItem`），移植到其他软件可直接复用 `openharmony_ability::read_text/write_text`，不与 zed 接口绑定。GPUI 适配只在 `OhosPlatform` 层。

## 修改文件 (Modified Files)

- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/clipboard.rs` — 新建，Rust FFI 剪贴板读写（OH_Pasteboard + UDMF），免 READ_PASTEBOARD 权限，通用模块
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lib.rs` — 注册 clipboard 模块
- `crates/gpui_ohos/src/ohos/platform.rs` — 实现 `read_from_clipboard` / `write_to_clipboard`，接线到 `openharmony_ability::read_text/write_text`

参考：[[ohos-debug-lessons]]
