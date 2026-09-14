# OHOS 终端单独按 Alt 回显 "lt"：修饰键自身被当 KeyDown 派发，终端把键名当字符编码送进 pty

## 问题描述

OHOS（HarmonyOS NEXT，PC/2in1）ZCoder 的 Terminal panel 中，单独按一下 **Alt** 功能键，命令行会新出现 `lt` 两个可打印字符（由 `/bin/sh` 回显）。Alt 是修饰键，不应产生任何可显示字符。该现象只出现在终端（pty）里，代码区/输入框等非 pty 场景按 Alt 正常。

## 问题表现

- 终端里单独按 Alt（左右均可），光标处新增 `lt`，像输入了普通字符；Alt 应当只是修饰键。
- 日志事件流非常"干净"：按 Alt 只有一次 `KeyDown(key="alt", key_char=None)`，**没有**任何 IME `insertText` 文本事件、**没有** `l`/`t` 的按键事件。GPUI 的字符输入 fallback（仅限 `key_char` 且只允许 shift 修饰）也不会插入文本。
- 因此从窗口输入管道看"不可能出字"，但终端屏幕确实回显 `lt` —— 说明有字节写进了 pty。
- 与之前的 Enter/Backspace 问题（IME 截获物理编辑键）**不是同类**，不能按那套思路修。

## 问题原因

两层叠加导致：

1. **平台层偏离桌面约定**：桌面 GPUI 从不把修饰键自身按下当作 KeyDown 派发，只发 `ModifiersChanged`。证据：
   - X11：`crates/gpui_linux/src/linux/x11/client.rs` 中 `if keysym.is_modifier_key() { return Some(()); }`
   - Wayland：`crates/gpui_linux/src/linux/wayland/client.rs` 中 `KeyState::Pressed if !keysym.is_modifier_key() => { /* 构造 KeyDown */ }`
   - OHOS 平台（`crates/gpui_ohos/src/ohos/window.rs`）却把 `AltLeft` 的按下也构造成 KeyDown 派发，keystroke 为 `key="alt"`、`key_char=None`。

2. **终端键码编码把"键名"当"字符"**：`crates/terminal/src/mappings/keys.rs` 的 `to_esc_str` 有一段 Linux 分支：

   ```
   is_alt_lowercase_ascii = modifiers == Alt && keystroke.key.is_ascii()
   → 命中后返回 format!("\x1b{}", key)
   ```

   `key` 是 `"alt"`（ASCII），于是编码出 `\x1b a l t` 四个字节写入 pty。`/bin/sh` 收到 `ESC` 后当作 meta/转义前缀，消耗掉紧随的一个字符（`a`），剩下的 `l`、`t` 成为普通字符被 shell 回显 —— 屏幕上就是 `lt`。

因果链：

```
AltDown(code=AltLeft)
  → KeyDown(key="alt", key_char=None)      // OHOS 派发了修饰键自身
  → to_esc_str: ESC + "alt"                // "alt" 被当成 Alt+字母
  → pty 字节: \x1b 'a' 'l' 't'
  → /bin/sh: ESC+a 被当 meta 消耗, 回显 l t
  → 界面出现 "lt"
```

## 解决方案

平台层对齐桌面：**修饰键自身按下/抬起只更新修饰状态（`ModifiersChanged`），不派发 `KeyDown`**。

- 在 `window.rs` 的 `Action::Down` 分支：`ModifiersChanged` 照常发送；用 `if !Self::is_modifier_key(key_event.code)` 包住 KeyDown 的构造/派发与 auto-repeat。
- `is_modifier_key` 已覆盖 Ctrl/Shift/Alt/Meta 的左右键 + CapsLock + Fn（AltLeft/AltRight 都包含）。
- 影响面：单独按 Alt/Ctrl/Shift 等不再作为"按键"进入应用，与桌面一致；**Alt+字母等组合不受影响**——组合键是字母键按下时携带 alt 修饰符走 KeyDown，终端按 meta 编码 `ESC+letter`，行为保持桌面语义。

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — `Action::Down` 对修饰键（含 AltLeft/AltRight）跳过 KeyDown 派发，仅保留 ModifiersChanged；使 OHOS 与桌面（x11/wayland）一致。

诊断参照（未改动）：
- `crates/terminal/src/mappings/keys.rs` — `to_esc_str` 的 alt+ascii 分支是触发点。
- `crates/gpui_linux/src/linux/x11/client.rs`、`crates/gpui_linux/src/linux/wayland/client.rs` — 桌面"修饰键自身不发 KeyDown"的范例。

## 验证

设备实机确认：单独按 Alt 不再出现 `lt`；`Alt+字母`组合键仍按 meta 工作；代码区行为不受影响。修复前曾尝试"按 IME 截获思路查 insertText"是走不通的方向（该问题无 IME 文本产生）。

[[ohos-debug-lessons]]
