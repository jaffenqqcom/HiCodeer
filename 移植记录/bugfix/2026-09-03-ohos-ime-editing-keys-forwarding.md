# OHOS IME 截获物理编辑键：backspace/delete/enter 失效——统一在"无组合文本时转发真实按键"

## 问题描述

OHOS（HarmonyOS NEXT）把物理键盘的 **Backspace / Delete(ForwardDel) / Enter** 当作 **IME 编辑键回调**（`deleteLeft` / `deleteRight` / `sendFunctionKey`）上报给应用，而不是普通按键事件。zcoder 的窗口输入层把这些回调一律翻译成"IME 文本层删除/替换"，导致：终端里 backspace 大多无效、delete 完全无效、设置单行输入框按回车插入字面 `\n`。该问题属于**输入法(IME)/输入层**维度，凡是获得 IME 焦点的输入控件（editor、设置框、终端）都可能中招——只是最先在终端暴露。终端本地适配本身见另篇 `2026-09-03-ohos-terminal-local-pty-shell.md`。

## 问题表现

- **backspace**：终端里**极少数生效、大多无反应**，无规律。
- **Delete(ForwardDel)**：终端里完全无效。
- **Enter**：设置单行输入框按回车，显示字面 `\n`（回车没生效/提交）。
- **editor（代码 tab）backspace 正常**（形成对比，成为定位线索）。
- 日志对账（物理键 backspace）：按 19 次里 18 次走 `ime backspace event len=1`（IME 截走），只有 1 次走 `keydown backspace`（IME 恰好没截时有效）。

## 问题原因

- OHOS 输入法 attach 后，物理编辑键作为 IME 回调送达：`deleteLeft`→`BackspaceEvent`、`deleteRight`→（被丢弃）、`sendFunctionKey`→`EnterEvent`。
- zc 窗口层（`gpui_ohos/src/ohos/window.rs`）此前把所有编辑键都翻译成对 **IME 文本层**的操作（`replace_text_in_range`）：
  - **Backspace**：`selected_text_range` + `replace_text_in_range` 删除文本层。terminal 的 `InputHandler`（`terminal_element.rs`）`selected_text_range` 恒返回 `Some(0..0)`、`replace_text_in_range` 只 `commit_text` **从不删除**（shell 命令行在 pty 里，本地无文本）→ 对 `0..0` 删空串 = no-op。editor 正常正是因为它的文本在本地编辑缓存、replace 真删字。
  - **deleteRight**：在 `plugin-ime/src/lib.rs` 被直接丢弃（原注释"GPUI 无 forward-delete 事件"）→ Delete 事件到不了窗口。
  - **Enter**：被 `replace_text_in_range(None, "\n")` 当文本插入 → 单行设置输入框出现字面 `\n`。

## 解决方案

统一原则（与桌面语义对齐）：**编辑键在无 IME 组合(marked)文本时，作为真实 gpui `KeyDown` 派发给焦点视图**（各视图用自己的语义：editor 删字/换行、terminal 写 pty 删除字节或 `\r`、单行设置框完成输入）；有组合文本时才走 IME 文本层（删组合/提交）。

- `window.rs`：把 IME 编辑键事件从异步文本层闭包中抽出，同步处理，按有无 `marked_text_range()` 分流；无组合时构造 `Keystroke`（`"backspace"`/`"delete"`/`"enter"`）`dispatch_input(KeyDown(...))`。
  - `handle_ime_backspace(len)`：有组合 → 删组合文本；无 → 派发 backspace 键。
  - `handle_ime_delete_forward(len)`：forward 删除对组合无意义，无组合 → 派发 delete 键。
  - `handle_ime_enter()`：有组合 → 沿用"提交"行为；无 → 派发 enter 键（不再插入 `\n`）。
- `ability/.../input/mod.rs`：`ImeEvent` 增 `DeleteRightEvent(i32)` 变体 + Debug。
- `plugin-ime/src/lib.rs`：`DELETE_RIGHT_EVENT` 不再丢弃，上报 `DeleteRightEvent(length)`。

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — IME 编辑键统一处理：新增 `handle_ime_backspace/delete_forward/enter`，无组合时派发真实 backspace/delete/enter KeyDown；同时移除排查期 `[diag]` 日志。
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/input/mod.rs` — `ImeEvent` 增 `DeleteRightEvent(i32)` 变体与 Debug。
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-ime/src/lib.rs` — `deleteRight` 回调改为上报 `DeleteRightEvent`（此前被丢弃）。

## 附：可复用经验

- **OHOS 把物理编辑键作为 IME 回调上报是一整类问题，不是单键**：凡 OHOS 上某物理键"偶发/完全无效"，先确认它是否被 IME 截成 `deleteLeft/deleteRight/sendFunctionKey`（加对账日志数一次即可），再按"有组合走 IME 文本层、无组合派发真实按键"处理。
- **"某控件文本删除失效而另一个控件正常"→ 检查该控件的文本是否真在本地缓存**：IME `replace_text_in_range` 只能作用于本地文本层；文本在外部（pty/远端）的控件必须走真实按键路径。
- **不要在一个维度暴露的问题里，把输入层修复和终端适配写在同一篇**——它们影响面与归属层完全不同。

[[ohos-debug-lessons]]
