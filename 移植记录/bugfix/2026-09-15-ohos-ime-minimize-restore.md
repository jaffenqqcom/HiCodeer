# OHOS 最小化恢复后输入法不激活：获焦时刻的重新绑定被 attached 缓存短路

> 2026-09-15。根因**不在**输入法插件监听的窗口事件上——实测最小化/恢复时 `windowStageEvent`
> 一切正常（`INACTIVE`→`HIDDEN`，恢复 `SHOWN`→`ACTIVE`），`windowVisibilityChange` 也会发。
> 问题出在**窗口恢复的时序**：整条恢复链里唯一一次 `attach` 发生在 `SHOWN`（窗口尚未可见、
> 尚未获焦），真正获焦的 `ACTIVE` 那一刻反而被 `ime_attached` 缓存短路，于是系统侧会话始终
> 没在「已获焦」状态下重建，键盘再也不弹候选框。

## 问题描述

HiCodeer（Zed 的 HarmonyOS NEXT 移植）在平板上把窗口**最小化后恢复**，系统输入法有时不再
激活：敲拼音没有候选框，字符被当作物理按键直接送进输入框。**再切到别的应用、切回
HiCodeer，输入法就恢复了**，且最小化→恢复可反复稳定复现（本次实测连续 4 个周期全部复现）。

相关代码：`crates/gpui_ohos/depend/openharmony-ability/plugins/ime/`。

## 问题表现

- 最小化→恢复后，敲拼音**不出现选词框**，文字经 keyevent 通道直接进入输入框。
- 用「切到别的应用 → 切回 HiCodeer」可立刻恢复正常（用户长期使用的手工绕行办法）。
- 复现步骤：
  1. 点进编辑器让光标出现，确认中文候选框正常（基线）。
  2. 点窗口标题栏「最小化」按钮。
  3. 从 Dock／任务栏点回 HiCodeer。
  4. 敲 `nihao` —— 候选框不出现。
- 设备：`deviceType=tablet`、型号 `QXS-W10`、`const.ohos.apiversion=26`。
- 触发条件是「最小化（真进后台）」，不是「失焦」：不带 `HIDDEN` 的单纯失焦/回来那条路径
  （即切走切回）反而能自行恢复。

## 问题原因

### 平台事实：最小化/恢复的 windowStageEvent 实测序列

`@ohos.window.d.ts` 定义 `WindowStageEventType` 为 `SHOWN=1 / ACTIVE=2 / INACTIVE=3 / HIDDEN=4`。
本机 2026-09-15 21:47 与 21:53 两轮实测（`A00001/com.hicodeer.studio/HiCodeer` 侧把
`event_type` 原始整数打成日志）：

```
最小化：  raw=3 (INACTIVE)  →  raw=4 (HIDDEN)
恢复：    raw=1 (SHOWN)     →  raw=2 (ACTIVE)

时间差（第 2 周期）：INACTIVE 39.168 → HIDDEN 39.243 →（visibility false 39.921）
                     SHOWN 40.300 →（visibility true 40.331）→ ACTIVE 40.400
```

两个结论：

1. 最小化**确实会发** `windowStageEvent`，`INACTIVE` 与 `HIDDEN` 都到；`handler_installed=true`、
   `acceptingLifecycle=true`，没有任何事件被守卫丢掉。「平板最小化不发 windowStageEvent」的
   猜测被证伪。
2. `windowVisibilityChange` 也发，但**比对应的 stage 事件晚** 30～700ms：恢复时
   `SHOWN`(40.300) 早于 `visible=true`(40.331) 早于 `ACTIVE`(40.400)。也就是说
   **`SHOWN` 时刻窗口既不可见、也未获焦**。

事件映射在 `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lifecycle.rs:182-191`：

```rust
StageEventType::Shown    => Event::Start,        // 恢复
StageEventType::Active   => Event::GainedFocus,  // 获焦
StageEventType::Inactive => Event::LostFocus,
StageEventType::Hidden   => Event::Stop,         // 最小化
```

ArkTS 侧注册在 `native_ability/src/main/ets/ability/NativeAbility.ets:468`（`windowStageEvent`）
与 `:473`（`windowVisibilityChange`）。

### 失败时间线（修复前，2026-09-15 21:47:39–40，逐条日志）

```
21:47:39.168  INACTIVE → LostFocus: attached=true
21:47:39.168  hide_keyboard_if_needed: enter attached=true  → sending detach
21:47:39.282  [ArkTS] stopInputSession: attached(before)=true     ← 两侧缓存都复位为 false
21:47:39.243  HIDDEN   → Stop → hide_keyboard_if_needed: 已被上一步置 false → 空转
21:47:39.921  windowVisibilityChange visible=false
21:47:40.300  SHOWN    → 发起 attach
21:47:40.301  [ArkTS] attach attached(before)=false → attachWithUIContext
21:47:40.302  showTextInput returned without throwing; attach ack accepted=true   ← 窗口尚不可见
21:47:40.331  windowVisibilityChange visible=true
21:47:40.400  ACTIVE   → GainedFocus: attached=true → show_keyboard_if_needed 短路   ★失败点
```

★ 是全链路的断点：**恢复流程里唯一一次 `attach` 落在 `SHOWN`（窗口不可见、未获焦）**；
等真正获焦的 `ACTIVE` 到达时，`ime_attached` 已被上一步的 attach 置为 `true`，
`crates/gpui_ohos/src/ohos/window.rs:1004` 的守卫直接返回，**从未在「已获焦」状态下重建会话**。
API 全程没抛异常、`accepted=true`，但系统侧的输入会话没有起来，所以字符只能走 keyevent。

### 两层短路

`attached` 状态被缓存了两份，任一份陈旧都会让恢复变成空转：

- **Rust 侧** `OhosWindow::ime_attached`（`window.rs:72`）：
  `show_keyboard_if_needed`（`window.rs:1003`）第一行 `if self.ime_attached.get() || self.ime_attach_in_flight.get() { return; }`。
- **ArkTS 侧** `ImePlugin.attached`（`ImePlugin.ets:114`）：修复前 `attach()`
  （原 `ImePlugin.ets:174`）第一行 `if (this.attached) { return Promise.resolve(true); }`，
  此时**不会**再执行 `attachWithUIContext`（`:199`）与 `showTextInput()`（`:216`），
  却向 Rust 回报 `accepted=true`，反过来把 Rust 的 `ime_attached` 重新置真。

### 为什么「切走再切回」能救回来

那条路径**没有 `SHOWN`**（窗口没被隐藏，只是失焦）：

```
切走： INACTIVE → LostFocus → hide → detach（两侧缓存复位 false）
切回： ACTIVE   → GainedFocus → ime_attached 为 false → 真的 attach + showTextInput（已获焦）✔
```

对比最小化恢复：多出来的 `SHOWN` 抢先做了一次「不可见、未获焦」的 attach，把缓存占成 `true`，
把后面那次唯一正确的 attach 挡掉了。这把「同一条恢复链路，切走切回有效、最小化恢复无效」
的差异解释干净。

## 排查过程中试错与废弃的改动

以下 4 类改动都试过并**已全部回退**，记录在此避免重走：

1. **假设「平板最小化不发 windowStageEvent」**（一开始的判断，源于 `lifecycle.rs:338-356` 一段
   针对 **2in1** 的注释：「titlebar minimize/hide on 2in1 does NOT fire windowStageEvent」）。
   实测证伪——tablet 上 `raw=3`/`raw=4` 都到。该注释不适用于 tablet。
2. **在 `Event::VisibilityChanged` 上直接 show/hide**（第 1 轮）。无效：`windowVisibilityChange`
   比 stage 事件晚 30～700ms，且同样被上述两层缓存短路。
3. **改在 ArkTS 侧监听 `windowVisibilityChange`**（第 2 轮）。已按「只走会触发的方案」的要求回退——
   它只是换了个更晚的触发源，不解决短路。
4. **新增 `Event::Start`/`Event::Stop` 分支来驱动 IME**（第 3 轮）。**不仅无效，还有害**：
   - `Start` 把 attach 提前到 `SHOWN`，即窗口可见前 31ms、获焦前 100ms —— 正是失败点本身；
   - `Stop` 到达时 `ime_attached` 已被 `LostFocus` 置 false，日志逐条显示
     `hide_keyboard_if_needed: short-circuited, no detach sent`，是纯粹的空转。
   两者的日志证明见上文时间线，已删除。
5. **在 `window.rs` 的 `GainedFocus` 前加一行 `self.ime_attached.set(false);`**（第 4 轮，
   作为「清缓存强制重绑」的兜底）。复核日志发现它在本轮**未产生任何行为差异**：
   14 次 `GainedFocus` 打印的 `attached` 全为 `false`，而该日志位于 `set(false)` **之前**，
   说明没有这一行 `show_keyboard_if_needed()` 也不会短路。既然无证据支持，已按「无效即回退」
   删除，`window.rs` 回到仓库原状。

同样被回退的还有排查期插入的 26 行 `[diag]` 临时日志（Rust 与 ArkTS 两侧）以及为给 hilog
腾缓冲而临时关掉的 `DEBUG_CURSOR_POS`（`ImePlugin.ets:46`，已还原为 `true`）。

## 解决方案

**让 ArkTS 的「已绑定」判断不再吞掉重新绑定请求**：删掉 `attach()` 里基于 `this.attached`
的早退分支。回调注册的一次性保证由另一个独立的标志 `callbacksRegistered` 承担，并发去重由
`attachInFlight` 承担，因此去掉这个早退不会导致回调重复注册。

`crates/gpui_ohos/depend/openharmony-ability/plugins/ime/src/main/ets/ImePlugin.ets`

修改前（原 `:174`）：

```ts
  private attach(): Promise<boolean> {
    if (this.attached) {
      return Promise.resolve(true);
    }
    if (this.attachInFlight) {
      return this.attachInFlight;
    }
```

修改后（现 `:177`）：

```ts
  private attach(): Promise<boolean> {
    if (this.attachInFlight) {
      return this.attachInFlight;
    }
```

改动后恢复链路的实际走向（2026-09-15 21:53:33.5–34.6 实测）：

```
21:53:33.566  INACTIVE → LostFocus → detach
21:53:34.498  SHOWN    → （不再有任何提前 attach）
21:53:34.534  ACTIVE   → GainedFocus: attached=false → 发起 attach
21:53:34.647  [ArkTS] attachWithUIContext + showTextInput 成功，ack accepted=true   ← 已获焦
```

即「恢复后的唯一一次 attach 落在已获焦的 `ACTIVE`」，与一直可用的「切走再切回」路径一致。

为什么选这个方案而不是别的：

- 不去改窗口事件的监听源。事件本身没问题（实测齐全），换触发源（`windowVisibilityChange`）
  只会把 attach 推到更晚或更早的时刻，都不在「已获焦」这个正确窗口里。
- 不做「最小化时 detach、恢复时 attach」的新状态机。原有状态机已经正确地在
  `LostFocus` 时 detach 了（日志可证），缺的只是「获焦时刻那次重新绑定被缓存吞掉」。
- 修改面最小：只删 3 行早退分支，不动回调注册、不动并发去重、不动 Rust 侧。

## 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/plugins/ime/src/main/ets/ImePlugin.ets` —
  删除 `attach()` 中基于 `this.attached` 的早退分支（原 `:174-176`），并改写其文档注释说明
  「已标记 attached 的请求仍需重新绑定，因为窗口隐藏时系统会收走会话」。**这是本次唯一的源码改动**。

以下文件在本轮排查中被临时改动，均已逐行回退、`git diff` 为空，仅记录以免混淆：

- `crates/gpui_ohos/src/ohos/window.rs` — 曾加 `Event::Start`/`Event::Stop` 分支配 IME 处理，
  以及 `GainedFocus` 前的 `self.ime_attached.set(false);`，均已删除。
- `crates/gpui_ohos/src/ohos/platform.rs` — 曾加 `handle_ohos_event` 入站事件名 `[diag]` 日志，已删除。
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lifecycle.rs` —
  曾加 `window_stage_event` 闭包的 `raw`/`handler_installed`/`mapped` `[diag]` 日志，已删除。
- `crates/gpui_ohos/depend/openharmony-ability/native_ability/src/main/ets/ability/NativeAbility.ets` —
  曾在 `onWindowStageEvent`/`onWindowVisibilityChange` 加 `[diag]` 日志，已删除。

## 验证状态

- **已验（日志级）**：事件序列、两层短路、失败时间线、修复后 attach 落在 `ACTIVE` 且
  `attachWithUIContext`/`showTextInput` 均无异常、`accepted=true` 共 15 次、无一失败
  （`accepted=false`、`12800xx`、`attach failed` 均为 0 次）。
- **已验（用户侧）**：用户在该修复构建上执行最小化→恢复并确认问题不再出现。
- **未验**：本次交付的最终构建只剩 ArkTS 一处改动（`window.rs` 已回退），**尚未由用户重新验证**。

## 遗留：一处尚未闭合的逻辑缺口

按日志，「ArkTS 早退分支」在验证通过的几轮里**从未被命中**——那 15 次 attach 的
`attached(before)` 全是 `false`（因为 `LostFocus` 的 detach 已把它复位），也就是说：
**去掉早退分支与不去掉，在这几轮的时序下行为相同**。

这与「用户在本次排查之前就报告该 bug」存在张力：若最小化/恢复与切走/切回在 App 侧状态机上
完全等价（差别只有 `SHOWN`，而 `SHOWN` 当时不处理任何事），那原始版本就不该失败。
因此**修复前那版失败的精确机制尚未完全闭合**，最可能是某个「获焦之前的抢先 attach」在
未被抓到的时序里占用了缓存（候选来源：`Event::SurfaceCreate` → `window.rs:1446`，
或 `update_ime_position` → `window.rs:3006`）。本次两轮日志都没抓到这类抢跑。

若该问题今后仍有间歇复现，下一步不要盲目改逻辑，先一次性打全这三处入口日志抓抢跑：
`Event::SurfaceCreate`、`update_ime_position`、`Event::GainedFocus`（含 `ime_attached` 值），
判定是否存在「获焦前 attach 占了缓存」的时序；若是，正确的修法是在获焦时刻无条件重建
（即上面第 5 条那个已被回退的 `set(false)`，届时它才有日志证据支持）。

## 附：可复用经验

- **OHOS 窗口恢复有 4 拍，顺序固定且间隔可达百毫秒**：`SHOWN` → `windowVisibilityChange(true)`
  → `ACTIVE`（最小化侧是 `INACTIVE` → `HIDDEN` → `windowVisibilityChange(false)`）。
  凡是「要在窗口重新可用后重建某系统资源」的逻辑，**必须挂在 `ACTIVE`（`GainedFocus`）**；
  挂 `SHOWN` 会落在「不可见、未获焦」的空窗里，系统会静默吞掉请求且**不报错**。
- **OHOS 这些 IME API 的「成功」不可信**：窗口不可见/未获焦时
  `attachWithUIContext` + `showTextInput()` 不抛异常、`ack` 也正常，但输入会话并未建立。
  别用「有没有抛异常」当健康判据——**只有「文字是否走 keyevent 通道」才是真判据**。
- **同一个布尔量在两处（Rust 与 ArkTS）各缓存一份时，一定会在某个时序上失去同步**：
  本例中最小化时是**系统**把会话收走的，App 侧没人通知 ArkTS 复位 `attached`，
  于是此后所有 attach 都成了「假装成功的空转」。排障时优先怀疑这种跨语言重复缓存。
- **别信注释里针对其它设备形态的结论**：`lifecycle.rs:338-356` 那段「2in1 最小化不发
  windowStageEvent」把排查第一次带偏。注释要带设备形态限定，读到时要先实测当前形态。
- **权威依据是设备返回的原始枚举整数**，不是文档语义描述：把 `windowStageEvent` 的
  `event_type` 原样打出来（`raw=3` / `raw=4`）比读 `@ohos.window.d.ts` 的措辞可靠得多。

[[ohos-debug-lessons]]
