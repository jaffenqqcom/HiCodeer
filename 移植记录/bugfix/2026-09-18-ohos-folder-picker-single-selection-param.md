# OHOS 菜单「Open Folder…」目录选择器与启动页不一致（调用方参数未对齐，非插件问题）

## 问题描述

菜单 File → Open Folder… 打开的目录选择器，与启动页（`hap/entry/src/main/ets/entryability/Setup.ets`）打开的目录选择器行为不一致：两者用的是同一个系统选择器，但菜单侧允许选择多个目录，启动页固定单选一个目录。需求是让菜单侧的目录选择与启动页保持一致（单选目录）。

## 问题表现

- 启动页选目录：一次只能选一个目录（`maxSelectNumber = 1` + `DocumentSelectMode.FOLDER`）
- 菜单 Open Folder：可以多选，选择数量上限为 500，且在 API 26+ 上还会开启 `allowsMulFolderSelection` 多目录选择
- 由于启动页与菜单走的是同一个系统组件（`picker.DocumentViewPicker`），差异必然只来自传入参数

## 问题原因

`crates/gpui_ohos/src/ohos/platform.rs` 的 `prompt_for_paths` 把 `PathPromptOptions.multiple` 原样映射成 `FileDialogOptions::allow_many`，**对"选目录"也照样传下去**：

```rust
let dialog_options = FileDialogOptions::new(dialog_type).allow_many(options.multiple);
```

ArkTS 插件据此设置选择器（`crates/gpui_ohos/depend/openharmony-ability/plugins/files/src/main/ets/FilesPlugin.ets:130`、`:136`）：

```ts
selectOptions.maxSelectNumber = options.allowMany ? 500 : 1;
...
if (options.dialogType === "open-folder") {
  selectOptions.selectMode = picker.DocumentSelectMode.FOLDER;
  selectOptions.maxSelectNumber = options.allowMany ? 500 : 1;
  if (deviceInfo.sdkApiVersion >= 26 && options.allowMany) {
    (selectOptions as DocumentSelectOptionsWithMulFolder).allowsMulFolderSelection = true;
  }
}
```

而启动页 `Setup.ets` 的等价逻辑是 `maxSelectNumber = 1` + `DocumentSelectMode.FOLDER`，不带任何多选参数。也就是说：插件本身把两种模式都正确实现了，菜单侧多选是因为调用方（Rust）传了 `allow_many = true`。

**走过的弯路（重要，勿重走）**：最初的改法是直接改 ArkTS 插件，让 `open-folder` 分支强制单选。这条路被否决并整体回退，理由是：ArkTS 插件是可复用的通用组件，它的职责是"调用方传什么就忠实执行什么"；在插件里写死某个调用方的特例，会让不同调用方互相影响，还会掩盖真正的问题（调用方把参数传错了）。最终 `FilesPlugin.ets` 一行都没有改（`git diff` 该文件为空）。

## 解决方案

按"参数问题在调用方修"的原则改 Rust 侧传参（`crates/gpui_ohos/src/ohos/platform.rs`）：

```rust
// `allow_many` mirrors `multiple` for files only: folder dialogs always ask for a single
// directory, matching the setup page's picker (the ArkTS plugin honours the flags it gets).
let dialog_type = if options.directories {
    dialog_type::OPEN_FOLDER
} else {
    dialog_type::OPEN_FILE
};
let allow_many = options.multiple && !options.directories;
let dialog_options = FileDialogOptions::new(dialog_type).allow_many(allow_many);
```

目录对话框由此始终以单选下发，与启动页行为对齐；文件对话框仍按 `multiple` 决定是否多选，不受影响。

## 修改文件

- `crates/gpui_ohos/src/ohos/platform.rs` — `prompt_for_paths` 中目录对话框不再透传 `allow_many`（第 610 行）
- `crates/gpui_ohos/depend/openharmony-ability/plugins/files/src/main/ets/FilesPlugin.ets` — 未改动（弯路阶段改过，已完整回退）

[[ohos-debug-lessons]]
