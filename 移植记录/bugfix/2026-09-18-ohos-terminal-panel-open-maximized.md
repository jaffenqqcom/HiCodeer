# OHOS 终端面板打开即最大化（把 PanelEvent::ZoomIn 挂在 set_active 上）

> 说明：本文件记录的是**交互调整（功能变更）**，不是缺陷修复。放在 `bugfix/` 目录是按项目要求归档，阅读时请按"功能记录"对待。

## 问题描述

OHOS 上打开终端面板（dock 上的终端按钮、菜单里的 Terminal Panel、以及默认键位 ctrl 加反引号）时，面板以 dock 的默认高度出现在底部，需要再手动点一次面板上的 zoom 按钮才能最大化。需求是：**每次打开终端面板都直接最大化**（面板 zoom，铺满整个表面），仅在 OHOS 生效。

## 问题表现

- 打开终端面板 → 只占底部默认高度（`TerminalSettings.default_width / default_height`）
- 手动点面板的 zoom 按钮 → 面板铺满，说明 zoom 链路本身是好的，缺的只是"打开时自动触发"
- 关闭面板再打开 → 又回到默认高度

## 问题原因

在 Zed 里，面板"最大化"不是简单地改面板尺寸，而是 workspace 层的 zoom 状态：

1. 面板上的 zoom 按钮最终 emit `PanelEvent::ZoomIn`
2. dock 处理该事件（`crates/workspace/src/dock.rs:701`）：`set_panel_zoomed(true)` 并写入 `workspace.zoomed` / `workspace.zoomed_position`
3. workspace 渲染时只渲染 zoomed 的那个面板，从而铺满表面

所以"打开即最大化"的正确做法是在面板变为 active 时触发一次 `PanelEvent::ZoomIn`，复用既有链路。难点在触发时机：`TerminalPanel::set_active` 是**在 panel 正在被 update 的时候**由 dock 调用的（`crates/workspace/src/dock.rs:557` 的 `set_open`、`:866` 的 `activate_panel`），如果在这里同步回调 dock，就会重入同一实体的 update。

关键事实（决定了这个实现安全）：GPUI 的 `Context::emit` **不是即时回调**，它把 `Effect::Emit` 压进 `pending_effects` 队列（`crates/gpui/src/app/context.rs:765`），等到 effect flush 阶段才真正派发给订阅者。因此在 `set_active` 里 emit 事件不会被同步重入，dock 收到时 panel 的借用早已释放。

## 解决方案

在 `TerminalPanel::set_active` 中，当 active 由 false 变 true 时 emit 一次 `PanelEvent::ZoomIn`（`crates/terminal_view/src/terminal_panel.rs:1615`）：

```rust
fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
    let old_active = self.active;
    self.active = active;
    // [ohos] Opening the terminal panel also maximizes it, so it fills the
    // surface instead of sitting at the dock's default height.
    #[cfg(target_env = "ohos")]
    if active && !old_active {
        cx.emit(PanelEvent::ZoomIn);
    }
    if !active || old_active == active || !self.has_no_terminals(cx) {
        return;
    }
    ...
}
```

要点：

- 复用用户点 zoom 按钮的同一条链路，没有新增任何 zoom 逻辑
- `#[cfg(target_env = "ohos")]` 保证其它平台行为不变
- 判断放在 early return 之前，因为"已有终端"时下面会提前返回，但最大化仍需生效

副作用与一致性：面板关闭时 `workspace.zoomed` 会被清空、而面板自身保持 zoomed（这是上游既有语义，见 `crates/workspace/src/workspace.rs` 中 "Close the dock while it is zoomed" 的既有测试）；再次打开时本实现会重新触发最大化，正好符合"每次打开最大化"。

**未采纳的方案（记录以免重走）**：同一时期还实现过"双击状态栏 / 菜单栏空白处 = 点一下终端面板按钮"的入口，最终整体回退。原因是架构别扭：`terminal_panel::Toggle` 定义在 `terminal_view`，而状态栏（`crates/workspace`）与菜单栏（`crates/title_bar`）都不能反向依赖它，只能把新 action 声明在 `workspace` crate —— 一个本不认识终端的中立 crate 因此多出终端专用 action 名，且 `title_bar` 也要跨 crate 引用它。回退后相关文件均已还原（`git diff` 为空）。

若将来仍要这个入口，正解是照上游既有的面板 action 模式：把 action 定义在中立的 `zed_actions`（先例：`zed_actions::project_panel::{Toggle, ToggleFocus}`，`crates/zed_actions/src/lib.rs:411`；`debug_panel`、`git_panel` 同理），handler 仍注册在 panel 自己的 crate，任何需要的地方直接 dispatch（先例：`crates/vim/src/vim.rs:332`）。

## 修改文件

- `crates/terminal_view/src/terminal_panel.rs` — `set_active` 增加 OHOS 分支：active 由 false 变 true 时 emit `PanelEvent::ZoomIn`（新增 6 行）

[[ohos-debug-lessons]]
