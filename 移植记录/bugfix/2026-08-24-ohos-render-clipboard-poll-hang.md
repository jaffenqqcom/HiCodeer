# OHOS 上 commit 视图渲染每帧读剪贴板导致主线程卡死

## 问题描述 (Problem Description)

zcoder（Zed 移植到 HarmonyOS NEXT / OpenHarmony）在 git 面板打开 commit 视图后，主界面周期性卡死：点击 diff/commit 条目后界面要等 5~6 秒才有反应，严重时完全冻结。排查发现主线程（TID = PID）以约 250ms 一次的频率持续调用 `OH_Pasteboard_GetData`（剪贴板读取），每次都返回 `status=201`（无数据），主线程 CPU 飙到 159%~250% 忙跑，界面无法响应。最终确认该高频读取来自 commit 视图的**渲染函数**（`CommitView::render_header`）每帧都调用 `cx.read_from_clipboard()`。

## 问题表现 (Symptoms)

- git 面板打开 commit/diff 视图后，主界面卡顿/冻结，点击条目响应延迟 5~6 秒
- `top` 显示 `com.zcoder.studio` CPU 159% ~ 250%，主线程状态为 `R`（running 忙跑）
- hilog 中主线程（PID=TID）反复打印 `clipboard::read_text: OH_Pasteboard_GetData status=201`，间隔约 250ms，持续几十秒
- `llvm-addr2line` 进程反复出现（系统 watchdog 在解析主线程卡死栈）
- `XCollie: StartProfileMainThread durationTime: 267ms`、`Vsync: recv vsync timeout`、`ProcessJank: jank >= threshold` 等卡死特征日志
- cmd-agent 远程命令执行（git/LSP spawn）完全正常，无任何错误——**问题不在 cmd-agent**

## 问题原因 (Root Cause)

**根因：`CommitView::render_header` 在渲染路径里同步读剪贴板。**

`crates/git_ui/src/commit_view.rs` 的 `render_header`（渲染 commit 头部的函数）里有这样一段逻辑：

```rust
let clipboard_has_sha = cx
    .read_from_clipboard()               // 每次渲染都同步读剪贴板
    .and_then(|entry| entry.text())
    .map_or(false, |clipboard_text| {
        clipboard_text.trim() == commit_sha.as_ref()   // 判断剪贴板是否就是当前 SHA
    });
let (copy_icon, copy_icon_color) = if clipboard_has_sha {
    (IconName::Check, Color::Success)   // 是 → 按钮显示"✓ 已复制"
} else {
    (IconName::Copy, Color::Muted)      // 否 → 按钮显示"复制"
};
```

- **用途**：commit 头部有一个"复制 Commit SHA"按钮。Zed 想做贴心反馈——如果剪贴板内容恰好等于当前 commit 的 SHA，就把按钮图标换成绿色对勾，提示"你已经复制过了"。这是纯 UI 反馈，不承担任何功能逻辑。
- **问题**：`render_header` 被 `render` 每帧调用，所以**每次 commit 视图重绘都会同步调一次 `read_from_clipboard`**。
- **OHOS 放大效应**：在 Linux/macOS 上 `read_from_clipboard` 是轻量本地调用，开销可忽略；但 OHOS 的 `OH_Pasteboard_GetData` 是**同步 IPC 调用**，且当前设备上剪贴板无内容时返回 `status=201`，调用慢且频繁。commit 视图一旦有持续重绘（内容加载、光标、布局变化），就变成每 250ms 一次的阻塞轮询，把主线程拖死。

**链式因果**：commit 视图渲染 → `render_header` → `cx.read_from_clipboard()` → `OhosPlatform::read_from_clipboard` → `openharmony_ability::read_text()` → `OH_Pasteboard_GetData`（慢速 IPC，status=201）→ 主线程被反复阻塞 → 界面卡死。

> 注意：本次排查最初误判为"剪贴板高频轮询本身是根因"。实际**剪贴板读取是症状不是根因**——真正导致忙跑的是渲染路径中这个同步读剪贴板的调用，只要 commit 视图在重绘就会触发。后续在修复验证时还发现独立的 LSP spawn 失败（`package-version-server` 二进制缺失），但那属于另一个问题，不在本文范围。

## 解决方案 (Solution)

**方案 A（采用）：把"读剪贴板"从渲染路径移到点击动作，用状态标志代替每帧读剪贴板。**

关键洞察：按钮要表达的是"这个 SHA 刚被复制过"，而不是"剪贴板当前内容是否等于 SHA"。前者是**一次性动作反馈**，可以用一个布尔状态记录，完全不需要每帧读剪贴板。

1. **`CommitView` 结构体新增 `copied_sha: bool` 字段**（commit_view.rs:92），初始化为 `false`（两处构造点：`new` 与重新建编辑器处）。
2. **`render_header` 删除 `cx.read_from_clipboard()`**（commit_view.rs:615），改为直接读状态标志：

```rust
// Before: 每次渲染读剪贴板（OHOS 同步 IPC，卡主线程）
let clipboard_has_sha = cx.read_from_clipboard()...;
let (copy_icon, copy_icon_color) = if clipboard_has_sha { Check } else { Copy };

// After: 用点击动作设置的状态标志
let (copy_icon, copy_icon_color) = if self.copied_sha { Check } else { Copy };
```

3. **"复制 SHA"按钮的 `on_click`**：写剪贴板后置 `copied_sha = true` + `cx.notify()`，并用 `cx.background_executor().timer(2s)` 延时后复位 `copied_sha = false`（commit_view.rs:723-731）：

```rust
.on_click(cx.listener(move |this, _, window, cx| {
    cx.stop_propagation();
    cx.write_to_clipboard(ClipboardItem::new_string(commit_sha.to_string()));
    this.copied_sha = true;
    cx.notify();
    let delay = cx.background_executor().timer(Duration::from_secs(2));
    cx.spawn_in(window, async move |this, cx| {
        delay.await;
        this.update(cx, |this, cx| { this.copied_sha = false; cx.notify(); }).ok();
    }).detach();
}))
```

**为什么不选方案 B（异步读 + 缓存）**：OHOS 剪贴板没有可靠的"变化事件"，且改动面大。方案 A 改动最小、完全保留用户可见行为（复制后仍显示"✓"），读剪贴板次数从每帧一次降为**零**。

## 修改文件 (Modified Files)

- `crates/git_ui/src/commit_view.rs` — 修复 commit 视图渲染读剪贴板导致的卡死：
  - `CommitView` 结构体新增 `copied_sha: bool` 状态字段
  - 两处 `Self { ... }` 构造点初始化 `copied_sha`
  - `render_header` 删除 `cx.read_from_clipboard()`，改用 `self.copied_sha` 决定图标
  - "复制 SHA"按钮 `on_click` 写剪贴板后置 `copied_sha = true` + 定时器 2s 复位

> 说明：`commit_view.rs` 属于路径不含 "ohos" 的受保护文件，本次修改经用户明确授权（方案 A）。

参考：[[ohos-debug-lessons]]
