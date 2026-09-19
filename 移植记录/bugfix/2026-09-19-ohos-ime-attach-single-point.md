# OHOS 输入法调不出来（触发点散落 → 收敛为单点决策）

> **状态：已解决**（2026-09-19 第三轮方案，用户实机确认问题解决）。
> 本报告合并并存档了原 `2026-09-19-ohos-ime-terminal-keyboard-not-showing.md`（已删除），保留其中仍有价值的根因推导。
> 最终改动只落在 Ohos 适配层 `crates/gpui_ohos/src/ohos/window.rs`，`crates/gpui`（通用代码）一行未动。

## 问题描述

在 terminal 里用输入法时，**IME 经常调不出来**；非 terminal 场景（编辑器等）概率低得多。

用户提出的核心不满是一处**行为割裂**：不点击输入框时，按物理键盘也能把字符直接打进输入框；点一下输入框之后，键盘才弹出、此后走 IME 通道。"没点输入框能用 keyevent 输入、点了才用 IME"这种二义性无法向用户解释。因此要求：**IME 的 attach 应以窗口/GPUI 的输入状态为准，而不是以"用户点了哪里"为准**，并把散落的触发点收敛。

## 问题表现

- terminal 下 IME 经常不出来；复现后的恢复手段（都无需切 app、无需重启）：
  - 把 terminal panel 关掉再打开
  - 点一下菜单栏、再点一下 terminal 界面
- 行为割裂（本报告要解决的核心）：
  - 不点击输入框时，物理键盘按键仍能输入字符
  - 点击输入框后键盘才弹出，之后按键走 IME
- 既有行为（不得破坏）：点工具栏/非输入区域时键盘会消失

涉及的既有链路（已由代码确证）：

- 只有被聚焦的元素才能注册输入处理器 —— `crates/gpui/src/window.rs` 中 `focus_handle.is_focused(self)` 才 push `PlatformInputHandler`
- 光标变化是"编辑框已聚焦"的代理信号：`invalidate_character_coordinates` → `update_ime_position`

## 问题原因

根因分三层，前两层由第二轮定位，第三层是第三轮才认清的**真根因**。

### 第一层：Rust 缓存了一个会撒谎的值

ArkTS 侧 `ImePlugin.ets` 的 `bindWithRetries` 里，`accepted` 的真实含义是**"`attachWithUIContext` 没抛异常"**，而不是"键盘已显示"：

```ts
controller.attachWithUIContext(context.getUIContext(), TEXT_CONFIG);   // 绑定会话
this.attached = true;
try {
  controller.showTextInput();                                          // 真正弹键盘的动作
} catch (error) {
  console.error(`ohos.ime: showTextInput failed: ...`);                // 只打日志
}
return true;                                                           // 仍然报成功
```

紧接着的 `showTextInput()` 失败时只打日志、仍 `return true`（注释自认 "best-effort"）。而 Rust 侧把这个 ack 直接写进了一个"已绑定"缓存（原 `ime_attached`），并用它做 `show_keyboard_if_needed` 的守卫。

**于是出现了这种组合：键盘从未显示，但缓存说已绑定。** 此后所有 show 请求都被守卫挡住，永久空转。

清掉这个缓存只有两条路，而它们都需要一次真实的状态变化：

1. 系统/用户收起键盘的上报 —— **键盘从未显示时，这条上报永远不会发生**
2. `completed_frame` 时 `input_handler` 为空、或窗口 `LostFocus`

用户的现象正好是这条分析的反证：**恢复需要"焦点离开再回来"**。

### 第二层：触发信号的粒度决定了守卫为什么存在

- `Event::GainedFocus` 的来源是 `StageEventType::Active`，是**窗口级**生命周期事件。**窗口获焦 ≠ 编辑框获焦。**
- ArkTS 侧没有任何**组件级**焦点事件上报。
- 所以 Rust 侧只能用"光标位置更新"当"编辑框已聚焦"的代理信号，而那是**高频**路径——必须有东西防重复请求，那个"会撒谎的缓存"就是这么被引进来的。

链条：缺组件级焦点 → attach 时机不准 → 拿高频信号兜底 → 需要守卫 → 守卫缓存了乐观值 → 失同步即永久卡死。

### 第三层（真根因）：四个触发点都在近似同一个事实，却用"用户动作"近似

第二轮把触发点铺成了四处，它们想表达的都是同一件事——"现在有一个可编辑的输入目标"：

- `SurfaceCreate` 近似"界面起来了"（太早，surface 未就绪时会被拒）
- `XComponentFocus(true)` 近似"输入面拿到焦点了"（`set_default_focus(true)` 导致首次可能根本不发）
- `GainedFocus` 近似"窗口回来了"（窗口获焦 ≠ 编辑框获焦）
- `MouseDown` 近似"用户想输入了"（唯一真正表达意图的）

而真正的事实只有一个：**GPUI 当前有没有一个被聚焦的、可输入的目标**——也就是 `input_handler` 是否存在。

**"能打字但没键盘"这个割裂就是这么来的**：GPUI 每帧 `draw` 结束时，只要有被聚焦的输入元素注册了处理器，就会调用平台窗口的 `set_input_handler`（`crates/gpui/src/window.rs` 的 `draw` 尾部；push 点在 `focus_handle.is_focused` 判断处）。也就是说：

- **有 GPUI 输入焦点 ⟺ `input_handler` 非空 ⟺ 物理键盘能打进字**（keyevent 回退路径在 `crates/gpui_ohos/src/ohos/window.rs` 的 `dispatch_input` 里，只要 `input_handler` 非空就把可打印字符直接塞进去）
- 而 **IME attach 挂在另外四个点上**

两件事不同源，于是必然出现"焦点在、能打字、键盘不出来"，直到某次点击撞上第四个点。

## 废弃方案（dead ends，记录以免重走）

### 第一轮（2026-09-18 夜，已废弃）

在 `window.rs` 纯新增 14 行，两处：

1. 键盘收起上报时复位 `ime_attached`
2. `dispatch_input` 里"按下且持有输入处理器时请求键盘"

**没有解决问题。** 改动一的前提是"收到键盘收起上报"，而"键盘从未显示"这个真实场景里根本没有这条上报；改动二走了 `show_keyboard_if_needed`，在缓存为假 true 时第一步就被守卫挡回。改动二方向对（按下是"用户想输入"的明确信号），被保留进后续方案。

### 第二轮（2026-09-19，已废弃但部分能力仍在）

思路：治本，删掉那个会撒谎的缓存，改用**组件焦点事件**驱动。

- 在 `openharmony-ability` 新增 `Event::XComponentFocus(bool)`，并在 XComponent 节点上挂 `on_focus` / `on_blur` 投递该事件
- Rust 侧据此 attach / hide；删除 `ime_attached`，引入纯本地记账 `ime_session_open`

实机验证通过（用户确认"ime 的问题解决了"），**但触发点散成四处**。而且那个"必须先点一下才能唤出键盘"的体验问题正是这一轮之后由用户提出来的——散点在用"用户动作"近似"有没有输入目标"，会漏。

> 后续清理（2026-09-19）：这一轮引入的 `Event::XComponentFocus` 变体与 `on_focus`/`on_blur` 投递已在单点方案落地后一并删除；该轮保留下来的只有纯本地记账 `ime_session_open`。

## 解决方案（第三轮，最终）：单点决策

把 attach/detach 收敛到**唯一一个决策函数**，判据直接取那个事实本身。

### 上游已有现成范式

**Wayland**（`crates/gpui_linux/src/linux/wayland/window.rs`）：

```rust
fn update_ime_enabled(&self) {
    let mut state = self.state.borrow_mut();
    if !state.active {                                    // 判据之一：窗口不活跃
        return;
    }
    let ime_enabled = state
        .input_handler
        .as_mut()
        .map(|input_handler| input_handler.query_accepts_text_input())   // 判据之二
        .unwrap_or(true);
    if Some(ime_enabled) == client.ime_enabled() {        // 边沿检测：没变就不动
        return;
    }
    if ime_enabled { client.enable_ime(); } else { client.disable_ime(); }
}
```

调用点是它的 `frame()`，**每帧末调一次**。

**Windows**（`crates/gpui_windows/src/events.rs`）几乎逐字相同，`unwrap_or(false)`。

这直接回答了"每帧调用会不会形成请求风暴"：**每帧评估不是问题，缺边沿检测才是问题**。函数第一件事就是"值没变就 return"，每帧成本只是一次比较，真正的系统调用只在状态翻转时发生一次。

（macOS 参考不了：它只有 `makeFirstResponder_`，系统输入法跟随第一响应者，不存在"启停"决策。另注：上游 `crates/gpui/src/platform.rs` 里预留了 `show_soft_keyboard` / `hide_soft_keyboard` / `text_input_state_changed` 和 `TextInputStateChange::{FocusGained,FocusLost,…}`，语义正是"可编辑元素获焦/失焦"，但**全仓零调用、零实现**，是从未接线的预留钩子。）

### 落点

**唯一决策点**：`crates/gpui_ohos/src/ohos/window.rs` 新增 `update_ime_enabled()`

```rust
fn update_ime_enabled(&self) {
    let wants_ime = self.active.get() && self.input_handler.borrow().is_some();
    if self.ime_enabled.get() == Some(wants_ime) {
        return;
    }
    self.ime_enabled.set(Some(wants_ime));
    if wants_ime {
        self.show_keyboard_if_needed();
    } else {
        self.hide_keyboard_if_needed();
    }
}
```

**唯一调用点**：`OhosWindowHandle` 的 `completed_frame`（每帧末），对应 Wayland 在 `frame()` 里的位置。

**唯一意图入口**：`dispatch_input` 里 `MouseDown` 且 `input_handler` 非空且键盘当前不可见时，把 `ime_enabled.set(None)`，让下一帧重新评估。它不再直接 attach。

**新增状态**：`ime_enabled: Rc<Cell<Option<bool>>>` —— 记录"上次推给 ArkTS 的值"，只记本侧发过什么，不读系统回报，所以不撒谎。

**删除的散点**：`SurfaceCreate` 的 attach、`XComponentFocus` 整个分支、`GainedFocus` 的 attach、`LostFocus` 的 hide。

结果：`show_keyboard_if_needed` / `hide_keyboard_if_needed` 的唯一调用者就是 `update_ime_enabled`。

### 为什么判据用 `input_handler.is_some()` 而不是 `query_accepts_text_input()`

上游用的 `PlatformInputHandler::query_accepts_text_input()` 内部会 `cx.update(...)`。而 `completed_frame` 由 core 在 `Window::complete_frame` 里调用，那时窗口正被 `handle.update(&mut cx, …)` 可变借用——再 update 会重入。所以判据退化为"有没有输入处理器"。OHOS 上 GPUI 只为可编辑且聚焦的元素注册处理器，这个判据足够。

### 为什么 `MouseDown` 删不掉

"用户此刻想不想弹键盘"**无法从任何客观事实推出来**：焦点在输入框、窗口也活跃，但用户可能刚把键盘收起来。此时"有输入目标"为真，若不拦，键盘会立刻自己弹回来，跟用户对着干。所以必须留一个用户动作入口，但它只负责"记下用户想输入"，决策仍在同一个点。

### 为什么 `query_accepts_text_input` 之外不能恢复四处散点

第三轮的判断标准是：**四个点里没有一个是"事实本身"**。凡是能同时改变"有没有输入处理器"或"窗口是否活跃"的事件（启动、surface 获焦、窗口获焦、点击），都会被每帧的决策自动看到，不需要各自去调 attach。这也是"散"的解法：不是找更好的触发点，而是把判据换成不再需要触发点的那个事实。

## 修改文件

- `crates/gpui_ohos/src/ohos/window.rs` — 第三轮主体：新增 `ime_enabled` 字段（结构体与初始化各一处）与 `update_ime_enabled()`；`completed_frame` 改为调用它；删除 `SurfaceCreate`/`XComponentFocus`/`GainedFocus`/`LostFocus` 四处 attach/hide 调用；`MouseDown` 由"直接 attach"降级为"重置 `ime_enabled`"；同步修正 `show_keyboard_if_needed` 与 `update_ime_position` 的文档注释（调用关系已变、`XComponentFocus` 已不存在）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/event.rs` — **第二轮遗留，已清理**：删除 `XComponentFocus(bool)` 变体与其 `as_str` 映射（第三轮后已无消费者，事件只会落进 `handle_event` 的 `_ => {}`）
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/render/xcomponent.rs` — **第二轮遗留，已清理**：删除 XComponent 的 `on_focus` / `on_blur` 投递块及其注释，并回退为它们而加的 `mut xcomponent_native`（该 `mut` 仅被这两个回调需要，其余 `background_color` / `set_focusable` / `set_default_focus` / `native_xcomponent` 均取 `&self`）与 `ArkUIEvent` 的 `use`（全仓唯一引用）

`crates/gpui`（通用代码）**未改动**。

## 验证结果

- 编译：`./script/bundle-ohos` 均为 `EXIT=0` + `=== HAP BUILD SUCCESSFUL ===`（单点方案落地后两次：2m33s / 2m15s；孤儿代码清理后再一次：2m31s），产物 `hap/entry/build/default/outputs/default/entry-default-signed.hap`
- 清理后 `openharmony-ability` 编译零警告，全仓 `XComponentFocus` / `ArkUIEvent` 代码零引用
- 实机：用户确认问题解决

## 遗留与观察点

1. **（已清理）`Event::XComponentFocus` 孤儿**：变体、`as_str` 映射、XComponent 的 `on_focus`/`on_blur` 投递块已全部删除；连带回退 `mut xcomponent_native` 与 `ArkUIEvent` 的 `use`，并修正 `update_ime_position` 中指向该事件的过时注释。清理后全仓 `XComponentFocus` 代码零引用（仅本报告作为历史记录保留其名）。
2. **`LostFocus` 的 hide 被删除**后，收键盘依赖后续帧触发 `completed_frame`。此点已由 core 代码确证：`on_active_status_change` 的回调里调了 `window.refresh()`（`crates/gpui/src/window.rs:1701`），窗口失焦与恢复都会因此产生一帧，单点决策随即执行。时序也成立——`Event::GainedFocus` / `LostFocus` 里先写本侧 `active`，再回调 core，帧在本侧状态之后才跑。
3. **启动早期的一次性窗口**：若 window 刚起时 `input_handler` 已非空（存在自动聚焦的输入元素），会在 surface 未就绪时尝试 attach。失败后 `ime_enabled` 已是 `Some(true)`，同一状态不会再翻回，因此重试只有两条路——ArkTS 内部重试（约 1 秒）与用户点击重置（`MouseDown` + 键盘不可见 ⇒ `ime_enabled.set(None)`）。实测未复现，但这是本轮把"直接 attach"换成"边沿决策"后必然带来的性质。
4. **`query_accepts_text_input` 的重入判断是静态推断**，未运行验证；判据因此用了 `input_handler.is_some()`。
5. **ArkTS 侧 `showTextInput()` 失败仍返回 success 这件事没修**（未动 ets）。自愈现在靠"键盘不可见时点击重置 `ime_enabled`"，另加 ArkTS 内部重试。
6. **`ime_session_open` 不能删**：它只记"本侧发过 attach、欠一次 detach"，纯本地事实，永远不挡后续 attach。也不能改用 `keyboard_visible` 代替——`notify_keyboard_hidden_by_user_if_needed` 要用 `keyboard_visible.replace(false)` 决定是否触发 `virtual_keyboard_hidden_by_user` 回调，共用一个位会抢位导致回调丢失。
7. **`keyboard_visible` 的准确性是自愈路径的前提**：点击重置的门槛是 `!keyboard_visible`。若系统收了键盘却没有上报（该位仍为 true），点击不会触发重置，只能等下次状态翻转。实测未复现。

## 附：一条跨语言教训

本次三层根因背后站着同一条规律：**同一个状态量在 Rust 与 ArkTS 各缓存一份时，一定会在某个时序上失去同步**。排障时优先怀疑这种跨语言重复缓存——第一层根因（`ime_attached`）正是它；最终方案干脆让双方都不再缓存"是否已绑定"，只保留"本侧发过什么"的本地记账。

[[ohos-debug-lessons]]
