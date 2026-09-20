# OHOS AutoHeight 编辑器软换行：measure 提交被删、prepaint 提交没接上

## 问题描述

agent_ui 的输入框（`EditorMode::AutoHeight`）输入长文本时**不自动换行**，整行横向溢出；而把它换回上游原版（在 measure 回调里提交 wrap 宽度）又会大幅提高 AppFreeze 概率。两条路各坏一头，本文记录最终的"两全"方案：把 wrap 提交点从 measure 搬到 prepaint，并只在 OHOS 生效。

## 问题表现

- agent 输入框、Ctrl+F 搜索条等 AutoHeight 编辑器输入长文本**不换行**，文字横向溢出可视区。
- 高度也不随软换行增长（AutoHeight 的高度是由 wrap 宽度推导的，宽度不提交 → 高度也算不出来）。
- 换回上游原版后：换行恢复，但 **极易 AppFreeze**（长按 Enter 遍历匹配、输入长文本都会触发）。
- 对应的历史实测数据（2026-08-24）：搜索条输入时**每秒约 450 次 measure**，其中 **97% 触发 rewrap**，`wrap_width` 在 25/1097px（hello 标签）与 93/1165px（diag 标签）之间来回震荡，最大停顿 453ms。

## 问题原因

这是三层叠加，不是"改错一行"。

**第一层：软换行需要"模式 + 宽度"两件事，缺一不可。**

- `Editor::set_soft_wrap()` 只把**模式**置为 `SoftWrap::EditorWidth`（`crates/editor/src/config.rs:51`），**不提交宽度**。
- 真正执行换行要求宽度为 `Some`：`crates/editor/src/display_map/wrap_map.rs:276` 的 `if let Some(wrap_width) = self.wrap_width`。
- 提交换行宽度的地方只有两处：measure 回调 `compute_auto_height_layout`（`element.rs:10695`）与 prepaint（`EditorElement::prepaint`，本文改动区 `element.rs:8070-8102`）。

**第二层：上游把提交放在 measure，构成布局反馈回路。**

measure 是 taffy 的测量回调。在它里面 `set_wrap_width()` 会改编辑器状态 → 触发 rewrap → 布局失效 → 下一帧 taffy 再测 → 再改状态。这就是 450 次/秒、97% rewrap、宽度震荡的来源。放大倍数来自 `rewrap()`：当 `total_rows < WRAP_YIELD_ROW_INTERVAL (100)` 时它走**同步 `block_on` 全量重算**，大内容时单次成本是 O(内容)。

**第三层（本次的真正病根）：2026-08-24 的修复只做了一半。**

当年为压冻屏做了"measure 无副作用化"——删掉 measure 内的 `set_wrap_width`，并在原处留了一句**承诺注释**：

```rust
// The measure callback must stay side-effect free: wrap width is committed
// once per frame by EditorElement::prepaint, so calling set_wrap_width here
// would mutate editor state on every taffy measure pass, triggering a rewrap
// and a layout-invalidation feedback loop.
```

但"改由 prepaint 每帧一次完成"这半从未落地：prepaint 里 AutoHeight 与 Minimap 同处一个分支，**直接返回旧 snapshot，根本不调用 `set_wrap_width`**。

```rust
// 改动前（与 HEAD 逐字相同）
if matches!(
    editor.mode,
    EditorMode::AutoHeight { .. } | EditorMode::Minimap { .. }
) {
    snapshot                       // ← 直接返回，不提交 wrap
} else {
    let wrap_width = calculate_wrap_width(/* ... */);
    if editor.set_wrap_width(wrap_width, cx) { editor.snapshot(window, cx) } else { snapshot }
}
```

于是提交点被删了、新家没搬进去，AutoHeight 的换行彻底失效。附带一个证据缺口：当年文档（`2026-08-24-ohos-ui-freeze-layout-prepaint-unresolved.md` 第 59 行）自称"element.rs **三处**"已改，但当前文件里 prepaint 的 AutoHeight 分支根本没有提交动作——说明那部分从未真正落到这个文件里。

## 解决方案

采用 **A + B + C** 三步，全部用 `#[cfg(target_env = "ohos")]` 门控：

- **A. measure 保持纯函数**：`compute_auto_height_layout` 与 HEAD 逐字相同（维持 measure 无副作用），不再触碰。
- **B. prepaint 补上提交**：把 AutoHeight 从 `Minimap` 同臂里拆出来，并入 wrap 提交路径；`Minimap` 保持不提交（它是父编辑器的只读投影，本来就不该提交宽度）。
- **C. wrap 真变化后请求再排一次布局**：`cx.notify()`，且**只在 AutoHeight**——因为 AutoHeight 的高度由 wrap 宽度推导，宽度变了必须再排一次才能让元素改高；Full 模式高度不依赖 wrap，不需要。

```rust
// crates/editor/src/element.rs:8070-8102
#[cfg(target_env = "ohos")]
let skip_wrap_width = matches!(editor.mode, EditorMode::Minimap { .. });
#[cfg(not(target_env = "ohos"))]
let skip_wrap_width = matches!(
    editor.mode,
    EditorMode::AutoHeight { .. } | EditorMode::Minimap { .. }
);

if skip_wrap_width {
    snapshot
} else {
    let wrap_width = calculate_wrap_width(editor.soft_wrap_mode(cx), editor_width, em_layout_width);

    if editor.set_wrap_width(wrap_width, cx) {
        #[cfg(target_env = "ohos")]
        if matches!(editor.mode, EditorMode::AutoHeight { .. }) {
            cx.notify();
        }
        editor.snapshot(window, cx)
    } else {
        snapshot
    }
}
```

**为什么这样能两全**：把提交点从"每次测量都触发"的 measure 换成"每帧一次"的 prepaint，既保住了 08-24 压冻屏的收益（rewrap 归零、measure 骤降），又让 AutoHeight 拿回了它独有的一脚提交。`set_wrap_width` 在宽度未变时是 `Pixels` 精确相等比较后直接 `return false`（`wrap_map.rs:261-263`），所以稳态下 B 的额外成本是几个浮点运算；C 也只在宽度真变化时多触发一帧，收敛后消失。

**为什么加 `#[cfg]` 门控而不是直接改**：这段代码位于上游三平台共用的布局逻辑里。按项目规则（CODEBUDDY.md 第 9 节：不能改变软件在其他操作系统的编译和功能），必须把差异限制在 OHOS。加了 `cfg` 之后，其他平台拿到的 `skip_wrap_width` 与 notify 行为与上游**逐字一致**。

**被否决的方案与走过的弯路（记录以避免重复）**：

1. **完整 revert 到上游**（把 measure 里的 `set_wrap_width` 按上游恢复）——已实测：换行确实恢复，但"极易 freeze"，且不是偶发。方向被否决。
2. **A+B+C 的第一版没有加 `cfg`**，且 `cx.notify()` 写在**共用分支**上——这样 Full 模式（主编辑器）也会被 notify，而它的高度不依赖 wrap，等于改变了主编辑器的行为。已修正为 `cfg` 门控 + 仅 AutoHeight 触发。
3. **保留 `spawn`/改优先级之类的旁路**不适用——崩因在布局反馈回路，不在调度。

## 修改文件

- `crates/editor/src/element.rs` — prepaint 内 AutoHeight 的 wrap 提交流程（`:8070-8102`，20 增 2 删）：
  - `:8076-8082` 用 `#[cfg(target_env = "ohos")]` / `#[cfg(not(...))]` 双绑定把平台的跳过集合分开：OHOS 只跳过 Minimap，其他平台维持上游的"AutoHeight + Minimap 都跳过"。
  - `:8093-8100` wrap 真的变化且模式为 AutoHeight 时（仅 OHOS）`cx.notify()`，请求补一次布局。

未改动 `compute_auto_height_layout`（`element.rs:10695`），保持 measure 无副作用。

## 验证与遗留

- 编译：`EXIT=0`、`=== HAP BUILD SUCCESSFUL ===`、0 error。
- 装包：`install bundle successfully` + `start ability successfully`。
- **收敛性实测（风险最大的一项）**：打开 Ctrl+F 搜索条（即 AutoHeight 编辑器，确实在屏）后静置，连续 5 次采样 CPU 稳定在 **~23%**；无 AutoHeight 编辑器时基线 ~20%。若 notify 不收敛（每帧 notify → 每帧全窗口重排），这里会持续高位——实测不漂移，说明 notify 收敛。
  - 过程中出现过一次 112% 的异常读数，经查是当时后台在跑 `Checking for updates to rust-analyzer...`，与本次改动无关。
- **换行已由人工确认恢复**（这是本问题的核心功能判据）。
- 遗留风险：`cx.notify()` 的重绘循环风险是这类改动固有的（若 AutoHeight 的 wrap 宽度每次布局都算出不同值，就会 notify → 布局 → notify 自激）。正常会收敛，因为 `set_wrap_width` 只在宽度真变化时返回 `true`。加 `cfg` 不改变这个性质。

## 附：可复用经验

- **"删掉一个有副作用的调用"和"把提交点搬到新家"是两件事**。移植改动只做前半，会留下编译通过、语义完整的**功能空洞**。识别信号就是那句**承诺注释**（"committed once per frame by X"）——见到就去核对 X 到底有没有做。本问题是"承诺了 prepaint，prepaint 里没做"。
- **measure/布局回调里改状态必然构成反馈回路**，判据是"测量频率 × rewrap 比例"。本次的历史数据（450 次/秒、97% rewrap、宽度震荡）就是该回路的指纹。
- **同一维度的功能，要在"提交点"这一层判断归属**：AutoHeight 需要提交（它从 wrap 推导高度），Minimap 不需要（只读投影）。两者被写在同一个 `matches!` 里是历史巧合，不是设计——拆分时要按各自语义而非按"当前分支长得像"。
- **平台差异集中在 `#[cfg]` 里**。同样是"让换行恢复"，去掉 `cfg` 就直接改了上游三平台的行为；加上 `cfg` 后改动面被限制在 OHOS，才是可接受的移植改动。

[[ohos-debug-lessons]]
