# OHOS 主题模式不跟随系统：三处缺口叠加，`Mode = System` 恒为浅色

## 问题描述

在 HiCodeer（Zed 移植到 HarmonyOS NEXT）中，`设置 → Appearance → Theme → Theme Mode → Mode → System` 这一档完全失效：不论系统处于深色还是浅色，应用始终渲染浅色主题。该问题覆盖两个方向——① 应用启动时不去读系统当前的主题；② 应用运行中切换系统深色/浅色时，应用收不到通知。两个方向都断了，所以 `System` 档与 `Light` 档在行为上没有任何区别。

`crates/settings_content/src/theme.rs:415` 定义的 `ThemeAppearanceMode::System` 取值为「跟随操作系统外观」，其判定依据是 GPUI 的 `SystemAppearance`。而 `SystemAppearance` 的取值来源只有两个入口：启动时的 `Platform::window_appearance()`，以及运行中的 `PlatformWindow` 外观变更回调。OHOS 后端这两条路径都没有接上，因此 `System` 档退化成常量浅色。

## 问题表现

- 系统设为深色后启动 HiCodeer，界面依旧全浅色；重启应用无效
- 应用运行中从系统控制中心切换深色/浅色，界面配色无任何变化，不闪不刷新
- `Theme Mode` 的 `Light` / `Dark` 两档正常，只有 `System` 档表现与 `Light` 完全一致
- 派生表现：`Icon Theme` 里的 `Mode = System` 同样失效（两者共用同一条 `SystemAppearance` 通路，见下）
- 无崩溃、无报错日志。应用侧在正常路径不打日志，所以从应用自身 hilog 里看不到任何异常

复现步骤：

1. 系统设置切到深色模式
2. 完全退出并重新启动 HiCodeer
3. 设置里把 `Theme Mode` 设为 `System`
4. 观察：界面仍为浅色；再切换系统主题，界面仍不变

## 问题原因

### 平台事实：`colorMode` 只在配置变化时上报

OHOS 的 `Configuration.colorMode` 取值语义为 `-1` 未设置 / `0` 深色 / `1` 浅色，由 `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/configuration/color_mode.rs:2` 定义为枚举：

```rust
pub enum ColorMode {
    NoSet = -1,
    Dark = 0,
    Light = 1,
}
```

关键的一点：`UIAbility` 只在**配置发生变更**时把 `Configuration` 交给应用（`onConfigurationUpdate`），**启动时不会上报一次**。也就是说，如果一个应用什么都不做，那么它读到的 `config.colorMode` 永远是 `-1`（`NoSet`）——它没有被声明为「跟随系统」。

要让系统开始向应用下发真实的 colorMode，必须在 Ability 侧显式声明跟随系统：

```
this.context.setColorMode(ConfigurationConstant.ColorMode.COLOR_MODE_NOT_SET);
```

这是所有缺口的共同前提。原代码从未调用它，因此整条链路从源头就是断的：即使后面把解析和 UI 都接好，读到的也永远是 `NoSet`。

### 缺口一：启动初值从未被读取

`Configuration` 里本来就有 `color_mode` 字段，`Event::ConfigChanged` 分支也确实会解析它（`crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/lifecycle.rs:136`）：

```rust
let color_mode = configuration.get_named_property::<i32>("colorMode")?;
```

但 `Configuration::default()` 把初值写死成 `NoSet`（`crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/configuration/config.rs:21`），而 Ability 初始化时传入的 `AbilityInitContext` 里根本没有 colorMode 这一项。于是窗口创建时拿到的必然是 `NoSet`，只能回落浅色。

### 缺口二：`window_appearance()` / `appearance()` 硬编码 `Light`

这是问题最直观的落点。OHOS 后端两个实现都把返回值写死：

```rust
// crates/gpui_ohos/src/ohos/platform.rs（修复前）
fn window_appearance(&self) -> WindowAppearance {
    WindowAppearance::Light
}
```

```rust
// crates/gpui_ohos/src/ohos/window.rs（修复前）
fn appearance(&self) -> WindowAppearance {
    WindowAppearance::Light
}
```

`Platform::window_appearance()` 是 `SystemAppearance` 的启动初值来源（见 `crates/theme/src/theme.rs:196` 的 `SystemAppearance::init`），`PlatformWindow::appearance()` 则是渲染时逐帧读取的权威值。两处都是常量，等于把「系统外观」这件事在 OHOS 上彻底屏蔽掉了。

### 缺口三：`Event::ConfigChanged` 分支不通知外观回调

即便前两个缺口补上，运行中切换系统主题仍然无效。`OhosWindow::handle_event` 的 `Event::ConfigChanged` 分支只做了尺寸/IME 相关的处理，处理完就结束：

```rust
// crates/gpui_ohos/src/ohos/window.rs（修复前，Event::ConfigChanged 分支尾部）
self.refresh_ime_cursor();
// ← 到此为止，没有任何外观通知
```

而 GPUI 侧的订阅早已就位（`crates/gpui/src/window.rs:1663` 注册的 `platform_window.on_appearance_changed(...)`），只是从来没人调用它。回调链的下一环是 `Window::appearance_changed`（`crates/gpui/src/window.rs:2432`），它会重新读 `platform_window.appearance()` 并保留所有外观观察者；再往上是 workspace 里唯一的订阅者（`crates/workspace/src/workspace.rs:1802-1808`）：

```rust
cx.observe_window_appearance(window, |_, window, cx| {
    let window_appearance = window.appearance();
    *SystemAppearance::global_mut(cx) = SystemAppearance(window_appearance.into());
    theme_settings::reload_theme(cx);
    theme_settings::reload_icon_theme(cx);
}),
```

即：`SystemAppearance` 在这里被刷新，随后 `reload_theme` 与 `reload_icon_theme` 一起重载。`ThemeAppearanceMode::System` 与 `Icon Theme` 的 `Mode` 都依赖这个值，所以缺口三一旦补上，两者会同时恢复。

### 完整链路（修复后）

启动方向：

```
EntryAbility.onCreate
  └─ setColorMode(COLOR_MODE_NOT_SET)          声明跟随系统，此后系统才会下发真实色值
NativeAbility.createInitContext
  └─ colorMode: context?.config?.colorMode ?? -1
AbilityInitContext.color_mode                    app.rs:35 / from_object: app.rs:52
OpenHarmonyAppInner::set_init_context
  └─ self.configuration.color_mode = ...         app.rs:302-308
OhosWindow::new → 缓存 color_mode 字段            window.rs:525-528
OhosWindow::appearance()                         window.rs:2820
OhosPlatform::window_appearance()                platform.rs:553-560
SystemAppearance::init                           crates/theme/src/theme.rs:196
```

运行中切换方向：

```
系统切换深色/浅色
  └─ NativeAbility.onConfigurationUpdate        NativeAbility.ets:594
     └─ onConfigurationUpdated(newConfig)        NativeAbility.ets:600
        └─ 解析 colorMode                        lifecycle.rs:136,149
           └─ 替换 app.inner.configuration       lifecycle.rs:159-163
              └─ 派发 Event::ConfigChanged       lifecycle.rs:164+
                 └─ OhosWindow 比对缓存并回调     window.rs:1605-1626
                    └─ Window::appearance_changed crates/gpui/src/window.rs:2432
                       └─ SystemAppearance 刷新
                          + reload_theme          crates/workspace/src/workspace.rs:1802-1808
                          + reload_icon_theme
```

### 为什么不用 openharmony-ability 的 plugins 模式

最初的设问是「openharmony-ability 有没有现成能力，没有的话用 plugins 模式实现可不可以」。结论是：**没有专用能力，但也不需要新插件**，因为通用配置变更通道已经端到端打通并且已经携带 colorMode，缺的只是两端接线。用 plugins 反而会引入一个真实问题——

`Platform::window_appearance()` 是**同步** trait 方法，而插件侧的桥是 `AsyncBridge`（异步）。`theme::init` 会在启动早期调用它取初值，异步桥在这个时点拿不到结果，必然产生竞态。为绕开竞态要么加阻塞等待，要么改 GPUI 的平台 trait 契约，代价远高于直接复用已有的同步 `Configuration`。

若将来确实需要绕过 `Configuration`（例如需要在非 Ability 上下文中读系统资源），退路是 `plugin-resource` + `ohos-resource-manager-sys 0.3.2` 的 `OH_ResourceManager_GetResourceConfiguration`，那是另一条独立路径。

## 排查过程中试错与废弃的改动

- **`ApplicationContext.onSystemConfigurationUpdated`**：查文档确认该接口 `@since 24`，而本工程 `targetSdkVersion` 为 `6.1.0(23)`（见 `hap/entry/build/default/intermediates/res/default/ark_module.json` 的 `targetAPIVersion: 60100023`）。不可用，放弃。
- **用应用自身 hilog 验证 colorMode**：想在修复后 grep 应用打印的 colorMode 来确认读到了正确值，但正常路径按日志规范不打日志，grep 不到属于预期。改用系统侧 hilog 交叉确认，找到 `SystemUIColorVm: updateColorMode, colorMode: 0`（sceneboard 进程），据此确认系统当时处于深色 `mode 0`，与应用的深色渲染一致。
- **新增函数初名违反命名铁律**：最初的命名为 `window_appearance_from_color_mode`，含 4 个下划线，违反项目「标识符下划线 ≤ 2」的硬性约定。已改为 `appearance_from_mode`（2 个下划线），并同步更新 `platform.rs` 与 `window.rs` 中的 3 处引用，随后重新编译确认。
- **不引入插件桥**：如上所述，`AsyncBridge` 与同步 trait 的竞态使其不划算，放弃。

## 解决方案

核心判断：**通用配置变更通道已存在且已携带 colorMode，问题不是「缺能力」，而是「源头没声明 + 两端没接线」。** 因此修复分四步，全部是接线，不新增通道、不改 GPUI 契约。

### 1. Ability 声明跟随系统

`hap/entry/src/main/ets/entryability/EntryAbility.ets` 的 `onCreate` 中，在 `super.onCreate` 之后调用 `setColorMode(COLOR_MODE_NOT_SET)`。这是让系统开始下发真实 colorMode 的前提。用 `try/catch` 包住并以 `hilog.error` 记录失败，因为该接口在旧版本上可能抛 `16000011`。

时机上是安全的：原生模块的加载被 `nativeLoadGate` 挡住，这次调用会先于 init context 被读取。

### 2. 把启动色值带进 init context

三处配合，把 Ability 启动时读到的色值一路带到 Rust：

```rust
// crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs
pub struct AbilityInitContext {
    ...
    /// OHOS `Configuration.colorMode` at init time: -1 not set, 0 dark, 1 light.
    pub color_mode: Option<i32>,
```

```rust
// 同文件 set_init_context
if let Some(color_mode) = context.color_mode.map(crate::ColorMode::from) {
    if matches!(color_mode, crate::ColorMode::NoSet) {
        log::warn!(
            "set_init_context: colorMode not set; system appearance unknown, falling back to light"
        );
    }
    self.configuration.color_mode = color_mode;
}
```

`NoSet` 时打 `warn` 并按浅色回落——这是有意的保守选择：读不到系统色值时保持历史行为（浅色），而不是猜一个深色。这条 `warn` 也是排查时唯一可用的信号。

### 3. 抽出映射函数，替换两处硬编码

```rust
// crates/gpui_ohos/src/ohos/window.rs
/// Maps the OHOS configuration color mode to the GPUI window appearance. `NoSet` means the
/// platform reported no color mode; the historical light appearance is kept rather than
/// guessing.
pub(crate) fn appearance_from_mode(color_mode: ColorMode) -> WindowAppearance {
    match color_mode {
        ColorMode::Dark => WindowAppearance::Dark,
        ColorMode::Light | ColorMode::NoSet => WindowAppearance::Light,
    }
}
```

两处硬编码改为调用它：

- `crates/gpui_ohos/src/ohos/platform.rs:553-560` — `window_appearance()` 读 `app.config().color_mode` 后映射
- `crates/gpui_ohos/src/ohos/window.rs:2820` — `appearance()` 读缓存的 `color_mode` 后映射

### 4. `ConfigChanged` 时通知外观回调

`OhosWindow` 增加一个缓存字段：

```rust
/// Last color mode observed in the platform configuration. Compared on
/// `Event::ConfigChanged` so only an actual dark/light switch reports an appearance change.
/// Cached rather than re-read from the app: `appearance()` is queried while rendering, and
/// `OpenHarmonyApp::config()` clones the whole configuration.
color_mode: RefCell<ColorMode>,
```

之所以缓存而不是每次从 app 读：`appearance()` 在渲染路径上被频繁调用，而 `OpenHarmonyApp::config()` 会克隆整个 `Configuration`，不能放在热路径上。

`Event::ConfigChanged` 分支尾部新增比对与通知：

```rust
let new_color_mode = self
    .app
    .borrow()
    .as_ref()
    .map(|a| a.config().color_mode)
    .unwrap_or(ColorMode::NoSet);
if *self.color_mode.borrow() != new_color_mode {
    *self.color_mode.borrow_mut() = new_color_mode;
    // Taken out before the call so the callback cannot observe a live borrow of
    // the callback table (mirrors the should_close path below).
    let mut appearance_callback =
        self.callbacks.borrow_mut().appearance_changed.take();
    if let Some(callback) = appearance_callback.as_mut() {
        callback();
    }
    if appearance_callback.is_some() {
        self.callbacks.borrow_mut().appearance_changed = appearance_callback;
    }
}
```

两个细节值得注意：

- **先比对再通知**：只在色值真正变化时触发。配置变更事件不只由主题引发（屏幕密度、语言、方向都会触发），不比对会造成无意义的重载。
- **先 `take()` 后调用**：回调内部可能重新进入并借用 `callbacks`，若持有 `borrow_mut()` 就会 panic。这里把回调取出表外再调用、调用后放回，与本文件既有的 `should_close` 路径写法一致。

### 替代方案为何不选

- **改 `ThemeAppearanceMode::System` 的判定逻辑**：治标不治本，`SystemAppearance` 本身接错了，改上层只会让其他依赖它的地方（如 `Icon Theme` 的 System 档）继续错。
- **在 Rust 侧起一个定时轮询读系统配置**：无谓的 CPU 开销，且系统本来就提供了配置变更事件。
- **引入插件桥**：见上文竞态分析。

## 修改文件

- `hap/entry/src/main/ets/entryability/EntryAbility.ets` — `onCreate` 中新增 `setColorMode(COLOR_MODE_NOT_SET)` 声明跟随系统，并以 `hilog.error` 记录失败；import 增加 `ConfigurationConstant`
- `crates/gpui_ohos/depend/openharmony-ability/native_ability/src/main/ets/ability/type.ets` — `AbilityInitContext` 接口新增可选字段 `colorMode?: number`，附注释说明 `-1/0/1` 语义与「系统只在配置变更时上报」这一事实
- `crates/gpui_ohos/depend/openharmony-ability/native_ability/src/main/ets/ability/NativeAbility.ets` — `createInitContext` 新增一行 `colorMode: context?.config?.colorMode ?? -1`，把 Ability 启动时读到的色值交给原生侧
- `crates/gpui_ohos/depend/openharmony-ability/crates/ability/src/app.rs` — `AbilityInitContext` 新增 `color_mode` 字段并在 `from_object` 中解析；`set_init_context` 用它填充 `self.configuration.color_mode`，`NoSet` 时打 `warn` 并回落浅色
- `crates/gpui_ohos/src/ohos/window.rs` — 新增 `appearance_from_mode` 映射函数（`ColorMode` → `WindowAppearance`）；`OhosWindow` 新增 `color_mode: RefCell<ColorMode>` 缓存字段并在 `new()` 中初始化；`appearance()` 由硬编码 `Light` 改为读缓存映射；`Event::ConfigChanged` 分支尾部新增色值比对与外观回调通知
- `crates/gpui_ohos/src/ohos/platform.rs` — import 增加 `ColorMode` 与 `appearance_from_mode`；`window_appearance()` 由硬编码 `Light` 改为读 `app.config().color_mode` 后映射

## 验证状态

**已验证通过（真机）。**

- 编译：`./script/bundle-ohos`，判据 EXIT=0 且输出 `HAP BUILD SUCCESSFUL`；0 error，改动文件无告警
- 安装：`install-local.sh`，`install bundle successfully` + `start ability successfully`
- 功能：用户真机确认问题解决，启动即为深色（未出现 `colorMode not set` 告警，说明 `setColorMode` 生效、系统已下发真实色值）
- 代码检视：未使用桩函数/空实现；`ConfigChanged` 分支仅在实际变化时通知，无重复触发；回调 `take()` + 复原写法避免重入 panic；除新增行外未改动任何已有代码行；命名违规已修正为 `appearance_from_mode`

**副产品**：`Icon Theme` 的 `Mode = System` 与本问题共用同一条 `SystemAppearance` 通路（`crates/settings_ui/src/page_data.rs:740` 读 `SystemAppearance::global(app).is_light()`，`crates/theme_settings/src/theme_settings.rs:114` 读同名值），因此本次修复一并使其在 OHOS 上生效，无需额外改动。

## 附：可复用经验

- **OHOS 的 `Configuration.colorMode` 是「订阅制」而非「查询制」**：不调 `setColorMode(COLOR_MODE_NOT_SET)` 就永远是 `-1`，且启动时不会主动上报一次。凡是需要跟随系统主题的 OHOS 应用都必须显式声明，并自行把启动初值从 Ability 侧带进原生侧。
- **遇到「某个平台能力缺失」时，先分清是「通道缺失」还是「两端未接线」**。本例中通用配置变更通道早已携带 colorMode，`Event::ConfigChanged` 也早已派发，只是没有消费者。补插件是成本最高、也最容易引入新问题（如本处的同步/异步竞态）的选项，应当留到最后。
- **同步 trait 与异步桥不兼容**：给 GPUI 这类框架补平台能力前，先确认目标 trait 方法是同步还是异步。异步桥无法喂给同步取值点，除非能接受阻塞或改契约。
- **在热路径上读配置要缓存**：`app.config()` 这类「返回结构体克隆」的接口不适合在渲染路径反复调用，需要缓存 + 比较。
- **只上报真实变化**：配置变更事件的触发源远多于主题（密度、语言、方向都会触发），消费时必须自行比对，否则会造成无谓的主题重载。

---
*OHOS 移植专属问题，关联 [[ohos-debug-lessons]]。*
