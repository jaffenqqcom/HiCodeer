# OHOS IME 文本上屏比物理键盘多绕一圈主线程任务队列

## 问题描述

OHOS 移植版里，输入法走**候选提交/文本上屏**（`TextInputEvent`）时的落字速度明显滞后于物理键盘。物理键盘在 NDK 回调里同步直达输入处理，而 IME 文本上屏被 `foreground_executor.spawn` 排进主线程任务队列，等于把"已经在主线程上的工作"又排到队尾，多绕了一整圈事件循环。本次把这一跳去掉，让 IME 文本上屏的最后一跳与物理键盘完全同构。

## 问题表现

- 用输入法（软键盘/中文候选）上屏时手感"慢半拍"，物理键盘则跟手。
- 两者共用同一个 `input_handler`，操作也相同（`replace_text_in_range` + `unmark_text`），唯一差别是 IME 那条多了一次任务队列往返。
- 无报错、无崩溃、无丢字，纯粹是延迟差异，因此不会被任何异常日志暴露。

## 问题原因

两条输入通道的线程归属其实都在 UI 主线程，慢的不是"线程优先级"——**前台任务根本不参与优先级排序**：

```rust
// crates/gpui/src/executor.rs:344-354
// Priority is ignored for foreground tasks - they run in order on the main thread
```

`ForegroundExecutor::spawn` 是无优先级 FIFO，只能排在队列里已有任务之后。

**物理键盘（改动前就已是同步）**：

```
NDK key_event C 回调（native_callbacks.rs:258）
  → h(Event::Input(KeyEvent)) → handle_ohos_event → handle_input_event
  → dispatch_input → replace_text_in_range        // 同一栈内完成
```

**IME 文本上屏（改动前，多两个环节）**：

```
ArkTS IME 回调
  → invokeNativeSync（同步桥）→ push_input
  → h(Event::Input(ImeEvent)) → handle_ohos_event → handle_input_event
  → foreground_executor.spawn                     ← 环节 A：重新排队
  → main_sender 入队 + waker.wake() → TSFN → Event::UserEvent
  → run_foreground_tasks                          ← 环节 B：下一轮事件循环才执行
  → replace_text_in_range
```

环节 A 与 B 是同一跳的两半，去掉 `spawn` 后两者一起消失。

**这个 `spawn` 的必要性已经消失**：它是 NDK 时代的遗留——当时 IME 回调不在主线程，必须 spawn 切回主线程。插件化之后回调已经由 ArkTS 侧 `invokeNativeSync` 同步落在主线程（`crates/gpui_ohos/depend/openharmony-ability/crates/plugin-ime/src/lib.rs:107-112` 注释明写 "Runs on the ArkTS/N-API main thread"），这一跳只剩"把已经在该线程的工作重新排到队尾"。

**同文件已有先例**：`handle_ime_backspace` / `handle_ime_delete_forward` / `handle_ime_enter` 在 2026-09-03 就已经统一改成同步处理（见 `2026-09-03-ohos-ime-editing-keys-forwarding.md`），且它们做的操作与 `TextInputEvent` 分支一模一样，只是文本不同。也就是说"抽出异步闭包改同步"这条路当时只抽了编辑键，漏了文本上屏。

## 解决方案

把 `ImeEvent` 的文本/状态分支从异步闭包改为**同步借用调用**，形态对齐同文件既有的 `handle_ime_enter`。

改动前：

```rust
let handler_ref = self.input_handler.clone();
let ime_event = ime_event.clone();
let executor = self.foreground_executor.clone();

executor
    .spawn(async move {
        let mut handler_guard = handler_ref.borrow_mut();
        let Some(handler) = handler_guard.as_mut() else {
            return;
        };
        match ime_event {
            ImeEvent::TextInputEvent(data) => {
                handler.replace_text_in_range(None, &data.text);
                handler.unmark_text();
            }
            // ...
        }
    })
    .detach();
```

改动后：

```rust
// Deliver synchronously. The ArkTS IME callback already runs on the
// main thread (the bridge is `invokeNativeSync`), so re-queueing the
// text on the foreground executor only adds a main-thread task
// round-trip before the character reaches the input handler. The
// editing keys above (backspace/delete/enter) take the same
// synchronous route.
let mut handler_guard = self.input_handler.borrow_mut();
let Some(handler) = handler_guard.as_mut() else {
    return;
};

match ime_event {
    ImeEvent::TextInputEvent(data) => {
        handler.replace_text_in_range(None, &data.text);
        handler.unmark_text();
    }
    // ...
}
```

**为什么选这个方案**：

- 最小改动，且与同文件既有 3 处同步路径逐一对应，不引入新的借用策略。
- 不动 ArkTS 插件（符合既有的桥接层分工：行为不对改 Rust 侧，不改 ArkTS 插件）。
- 附带两个正向变化：一是**消除乱序可能**——原异步版会把文本处理落到任务队列，若中间夹进一个走同步路径的物理键盘事件，处理顺序会与实际输入顺序不符；二是**少一次 `ime_event.clone()`**（原来是为了 move 进 async block）。

**排除的替代方案**：把 `spawn` 保留但改成更高优先级——不可行，前台任务忽略优先级（见上）。

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — `handle_input_event`（:1783）内 `ImeEvent::TextInputEvent` / `ImeEvent::ImeStatusEvent(Hide)` 分支由 `foreground_executor.spawn(...).detach()` 异步入队改为同步借用调用（:1816-1847）。

## 验证与遗留

- 编译：`=== HAP BUILD SUCCESSFUL ===`，签名通过，4 个 hnp（openssh/git/curl/hicodeerd）齐全。
- 已装设备；**主观跟手度需人工实测**（这是唯一必须由人判断的一项）。
- 静态复核（借用安全）：
  - 借用模式与同文件 `handle_ime_backspace:1673`、`handle_ime_delete_forward:1723`、`handle_ime_enter:1754` 完全同构，那三处自 2026-09-03 起一直在生产路径上跑。
  - arm 内只调 `replace_text_in_range` + `unmark_text`，**不调 `dispatch_input`**，所以 guard 持有到 arm 结束无重入风险。
  - `handler` 为 `None` 时 `return`，此时 1786-1794 的 Hide 通知**已经执行完**，与原来"闭包内 return、不影响外层"等效。
  - `input_handler: Rc<RefCell<Option<...>>>`（:66）非 `Sync`，`handle_input_event(&self, ...)` 只能从持有窗口的线程调用，线程安全由类型系统兜底。
- 观察点（非阻塞）：同文件里存在两套借用策略——`dispatch_input:2394` 用的是 `take()` + 显式 `drop` 的保守写法，而 IME 路径是持 guard。若将来出现 `already mutably borrowed` 类 panic，先看这里。
- 同批剩余未处理的 `spawn` 点（非每键、不在本次范围）：`show_keyboard_if_needed:1040`、`hide_keyboard_if_needed`、`push_ime_cursor_rect:3014`。

[[ohos-debug-lessons]]
