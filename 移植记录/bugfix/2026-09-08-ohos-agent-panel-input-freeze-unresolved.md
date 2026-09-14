# OHOS 上 agent 面板输入文字时冻屏（主线程 prepaint/绘制忙循环）【未解决】

## 问题描述 (Problem Description)

zcoder（Zed 移植到 HarmonyOS NEXT / OpenHarmony）在 **agent 面板里输入文字与 agent 沟通** 时频繁冻屏（THREAD_BLOCK_6S），极易复现（同日 00:23、00:30 两次 AppFreeze）。主线程长时间卡在 `uvLoopTask`（NAPI uv loop 事件），期间持续在 CPU 上忙跑。macOS/Linux 桌面同代码无此问题。**当前未解决。**

本问题与既往两个 unresolved 相关但**不是同一个 bug**（区别见下文），值得单独跟踪。

## 问题表现 (Symptoms)

- 冻结发生在 agent 面板**输入框敲字 / 与 agent 沟通**时，界面完全无响应，易复现。
- `appfreeze` 原因 `THREAD_BLOCK_6S`；主 handler 事件队列里全是输入法事件：`IMF_updateCursor`、`GetLeftTextOfCursorV2`、`ArkUIAceContainerNonPointerEvent`、大量 `MMITask`；主 handler 正在跑 `uvLoopTask`。
- 主线程状态 `R`（running，忙跑非睡眠），第二份 dump `utime=6208` ticks（clk=100，≈62 秒用户态 CPU 累积）。
- 进程 Rss≈603MB、设备 Available≈7.4GB（无内存压力）。

### 两次采样证据（appfreeze-…-002345141 / appfreeze-…-003006824）

1. **00:23 freeze 主线程栈**：`draw_roots → request_layout` 递归爆炸，同一批元素反复 request_layout（此前已分析）。
2. **00:30 freeze 第一份栈**（00:29:55 采样，255 帧截断仍未到底）：
   - 栈顶 `EditorElement::prepaint` → `resolve_selections_without_display_round_trip`（选区解析）→ `MultiBufferCursor::seek_forward` → `sum_tree::Cursor::seek_internal`。
   - 向下 255 帧被同一批 `Drawable<Div>::prepaint` / `Interactivity::prepaint` / `Stateful<Div>::prepaint` 帧**周期性填满**——同一批 UI 元素在反复 prepaint。
3. **00:30 freeze 第二份栈**（00:30:03 采样）：主线程已转到绘制阶段 `Scene::replay → insert_primitive → BoundsTree::find_max_ordering`，`state=R`。

## 问题原因 (Root Cause)

**未定位。** 目前可确认的线索与排除项：

- 核心现象：主线程在 8 秒窗口内从 prepaint 走到 paint，**是一轮又一轮「整树重新 prepaint + 绘制」的忙循环**，不是单次计算超时。
- 触发场景收敛：agent 面板**编辑器输入框**每次输入（光标/选区变化）触发其所在区域的过深 prepaint 递归——一批元素反复重排 250+ 层仍不收敛。
- 栈顶是**编辑器选区/游标解析**（`resolve_selections_without_display_round_trip` + `sum_tree::Cursor::seek_internal`），不是单点计算慢。
- **与既往 unresolved 的区别**：
  - vs `2026-08-24-ohos-ui-freeze-layout-prepaint-unresolved`（编辑器 prepaint 每帧 ~22ms 固定成本，缓解后 ~25fps 掉帧不冻屏）：本次回到 6s 级冻屏。
  - vs `2026-08-24-ohos-font-layout-hang-unresolved`（diff 视图字体 fallback 卡死）：本次栈里**没有** `cosmic_text`/`read_fonts` 字体整形帧，不是字体排版。
- 已排除：死锁（其余线程基本 S）、内存压力、dispatch 线程池改造（4 个 `gpui-ohos-bg` worker 均为 S，本次与池无关）。

## 解决方案 (Solution)

**未解决。** 候选方向（需先讨论授权再实施）：

### 方向 A（首选）：运行时打点指认递归元素
- 给编辑器 prepaint / selection resolve 加 `[diag]` 打点（沿用 freeze-trace 打法）：打 `resolve_selections` 入口、prepaint 嵌套深度、命中的 element/view 名。
- 复现一次抓 hilog，直接指认「递归的是哪个 view/element、深度多少」。
- 涉及 `crates/editor/src/element.rs`（editor prepaint）等，属诊断代码。

### 方向 B：对照桌面
- 同一操作在 Linux/macOS 桌面版是否也深 prepaint：若仅 OHOS 复现 → 重点查 OHOS 特有差异；若桌面也深 → 更可能是 agent 面板数据/结构触发的通用问题。

### 方向 C：采样外部识别 view
- 在主线程 freeze 的 appfreeze 栈中 EditorElement 外层 Div 帧无法给出 view 名，可在 `Window::draw` 的 prepaint 入口打印当前绘制根 view 身份，缩小到 agent 面板具体子树。

## 修改文件 (Modified Files)

- 暂无（纯诊断，未落地修复）。
- 相关文件（供后续参考）：
  - `crates/editor/src/element.rs` — `EditorElement::prepaint`、`resolve_selections_without_display_round_trip` 所在路径
  - `crates/editor/src/selections_collection.rs` — `resolve_selections_without_display_round_trip`
  - `crates/multi_buffer.rs` / `crates/sum_tree` — `MultiBufferCursor::seek_forward`、`Cursor::seek_internal`（Excerpt 游标遍历）
  - `crates/gpui/src/window.rs` — `draw` / prepaint / paint 分段（freeze-trace 打点位置）

关联：[[2026-08-24-ohos-ui-freeze-layout-prepaint-unresolved]]、[[2026-08-24-ohos-font-layout-hang-unresolved]]（同属 OHOS UI 冻结族，各有差异）
