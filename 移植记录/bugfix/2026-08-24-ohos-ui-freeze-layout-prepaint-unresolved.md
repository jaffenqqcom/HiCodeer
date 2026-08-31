# OHOS 上 UI 冻结/持续卡顿（编辑器布局 prepaint 阶段每帧 ~22ms）【未解决】

## 问题描述 (Problem Description)

zcoder（Zed 移植到 HarmonyOS NEXT / OpenHarmony）在**普通编辑操作**（空 tab 长按 Enter、Ctrl+F 搜索并按住 Enter 遍历匹配）时出现两类现象：
1. **冻屏**（16:11、20:33 两次 AppFreeze）：主线程 `APP_INPUT_BLOCK`，输入事件阻塞 >8000ms；
2. **持续卡顿**（21:22 之后多轮复现）：界面明显掉帧（~25fps），但无 AppFreeze——**界面滞后、内部处理持续推进**（松手后界面跳变到实际位置）。

macOS/Linux 上同样的代码无此问题，**仅 OHOS 复现**。已排查并排除：死锁、内存压力、auto-height 测量反馈循环（已修复）、vim HelixJump overlay（未触发）、编辑同步/全量重排（数据不支撑）。

**当前状态：未解决。** 已通过逐层打点把病根收敛到 **`Window::draw` 的 prepaint（布局预处理）阶段每帧 ~22ms 固定成本**（空 tab 亦如此，与内容量无关），并进一步拆分为「编辑器自身 ~11ms + 窗口其他元素 ~11ms」，但**两个子块的内部构成尚未实测完**（`editor_snapshot` 细分打点已加，待下一轮数据），**最终修复未落地**。

## 问题表现 (Symptoms)

- 16:11：AppFreeze `APP_INPUT_BLOCK`，主线程状态 `R`（运行中烧 CPU），栈顶 `compute_auto_height_layout` 内 `drop(EditorSnapshot → HashMap<BufferId,(Arc<[SemanticTokenHighlight]>,…)>)`；进程 Rss≈1.05GB、设备余 5.3GB（无内存压力）；debug 构建
- 20:33（修复后）：冻屏仍在，主线程栈顶**转移到绘制**：`IterMut<NavigationOverlayPaint>` → `paint_navigation_overlays`
- 21:22 起：无 AppFreeze，但**每帧 CPU 布局+绘制 33~90ms**（帧率 10~30fps），空 tab 长按 Enter 界面卡在低行数、松手跳变到高行数（输入未丢、渲染滞后）
- `top`/hilog：主线程持续烧 CPU；hilog 中 `freeze-trace` 打点可稳定复现（见下文数据）

## 定位历程与打点数据（按时间线）

所有运行时打点统一前缀 `[freeze-trace]`，经 `log::warn!` 输出（OHOS 上由 zlog 转发到 hilog，tag=`Zcoder`；**eprintln! 在 OHOS 不进 hilog，是首批打点无输出的原因**）。

### 1. 16:11 原始 AppFreeze（`appfreeze-...161142517.log`）

- 触发类型 `APP_INPUT_BLOCK`（>8s）；主线程 Tid 31792 状态 `R`，其余 ~60 线程全 `S` → **排除死锁**
- 栈顶：`compute_auto_height_layout` 正在 `drop` 巨大的语义高亮 HashMap → 每帧对整份缓冲区建 `editor.snapshot()`（含完整 display map + 语义高亮表）后丢弃
- 结论：**auto-height 编辑器布局测量路径**；字体后端（OHOS cosmic_text）为平台差异点（mac=CoreText、linux=HarfBuzz）
- 已确认跨平台层 `gpui::TextSystem` 本身有完整缓存（`LineLayoutCache`，key 含 `wrap_width`、`shape_line_by_hash`），**不是靠各平台适配层补缓存**；OHOS 后端只补了光栅像素（SwashCache）与家族解析

### 2. 18:45 首批运行时打点（`measure` / `rewrap` / `line_cache`）

```
measure #13693 wrap_width=Some(1165.3px) changed=true changed_so_far=13292
rewrap start rows=1  →  rewrap done(sync) cost=161µs
measure #13694 wrap_width=Some(25.45px)  changed=true
measure #13695 wrap_width=Some(749.5px)  changed=true   ← 每 0.5ms 一次
```

- `changed_so_far=13292 / 13693` ≈ **97% 的测量都触发 rewrap**
- `wrap_width` 在 1165 / 25 / 749 / 25px 之间**来回震荡**（25px≈零宽约束）
- 但 `rewrap rows=1`、`cost≈160µs` → **单次重排不慢**；rows=1 是小编辑器（搜索条）
- 结论修正：不是"单次重排慢"，而是 **measure 被疯狂反复调用 + 每次改状态触发 rewrap 的测量-失效反馈循环**；大内容编辑器进入同循环时单次 `snapshot()` O(内容) → 8s 冻屏

### 3. 19:25 复现日志（`This PC-1787570802-hilog.txt`，28678 行全为 freeze-trace）

- 每秒 measure 226~528 条、22 秒均匀分布，**无任何高峰**；相邻 measure 最大间隔仅 476ms；snapshot/rewrap 全 µs 级 → **该窗口无冻屏段**
- 但暴露常态：**每秒 ~450 次测量、wrap_width 持续震荡（749/25/93/817px）、几乎每次 changed=true** → "火药桶"一直存在，解释"时卡时不卡"

### 4. 19:52 定位收官（`This PC-1787572388-hilog.txt`）——测量循环元凶

- 打点加入 `target=`（buffer 标题）后直接指认：**`target=hello` 52 次、`target=diag` 46 次交替**
- 每秒 ~450 次、持续 20 秒；wrap_width 震荡：hello 25/1097px、diag 93/1165px；最大停顿 **453ms**（19:52:51，每秒密度 528→223）
- 已查证 `hello`/`diag` = **搜索条输入框 buffer**（`MultiBuffer::title()` 对无文件 buffer 返回内容前几字符，multi_buffer.rs:2169）
- **根因确认**：`compute_auto_height_layout`（element.rs:10725）的 measure 闭包有**副作用**——每次被 taffy 测量都 `set_wrap_width()` → 改状态 → rewrap → 布局失效 → 下一帧再测

### 5. 20:33 治本修复后复测（`appfreeze-...203331534.log` + `This PC-1787574827-hilog.txt`）

- **修复：measure 无副作用化**（element.rs 三处）：删除 measure 内 `set_wrap_width`，AutoHeight 的 wrap 提交改由 `prepaint` 每帧一次完成
- 效果：**rewrap 归零**（450/s → 0）、measure 骤降（450/s → ~237/s，采样间隔 4ms）、snapshot 24~95µs
- **但冻屏仍在**：新 AppFreeze 主线程栈从布局**转移到绘制**——`IterMut<NavigationOverlayPaint>` → `paint_navigation_overlays`（element.rs:5713）
- 静态排查穷尽：`navigation_overlays` 生产插入唯一来源 = vim HelixJump（vim.rs:1823），搜索的 SelectNextMatch/SelectAllMatches 均不建 overlay，vim/helix 默认关闭 → **与用户"非 vim 模式"矛盾，打点待验证**

### 6. 用户操作澄清（20:48）

- 非 vim 模式；普通 tab 按 Ctrl+F 搜索，输入 hello/diag，**按住 Enter 遍历匹配**（OHOS 按键重复 ~10 次/秒）
- 搜索条是 auto-height 编辑器 → 解释了 hilog 高频测量；但冻屏栈 `paint_navigation_overlays` 属于主编辑器（Full 模式）→ overlay 来源仍待实测

### 7. 关键诊断（20:58）：空 tab + 长按 Enter 也卡

- 现象：**界面卡住但内部处理持续推进**（卡在第 5 行，松手恢复后跳变到 20~30 行）→ 输入事件没丢，是**每帧渲染极慢（持续掉帧）**，非输入阻塞
- 代码侧：`rewrap()` 全量重排阈值 `WRAP_YIELD_ROW_INTERVAL=100`（<100 同步 `block_on` 全量 O(行数)）；但 `flush_edits` 单次编辑走**增量** update → 编辑同步大概率非主因
- 新增**整帧打点**：`Window::draw`（gpui/src/window.rs:2929）`frame #N cost=?`

### 8. 21:22 第四次复现（`hap/This PC-1787577768-hilog.txt`，空 tab+长按 Enter，19 秒）

- **frame 打点 195 条：50-100ms × 180 + ≥100ms × 13 → 每帧 CPU 布局+绘制 ~90ms**
- 帧间隔 50-150ms → **帧率 ~10fps**
- `measure=0`、`wrap_sync=3`、`rewrap=1`、overlay=0 → **编辑/测量/重排都不是大头，病根在绘制本身**（注：此时打点粒度还粗，后续细分发现是"布局"而非"绘制"）
- 新增 `draw_detail layout=? paint=?` 分段打点（window.rs:3120）

### 9. 21:48 静置对照（`This PC-1787579310-hilog.txt`，空 tab 放着 10 秒）——病根锁定布局

| 指标 | 静置 | 编辑（21:22） |
|---|---|---|
| frame cost | 16-33ms × 25（无 ≥50ms） | 50-100ms × 180 |
| **layout** | **~21ms/帧**（25/26 超 16ms 预算） | 放大到 80ms+ |
| paint | **~4.5ms**（全部 4-8ms） | 稳定，非瓶颈 |

- **paint 不是瓶颈；layout 是病根**——静置时每帧布局就要 ~21ms（超帧预算），编辑把它放大到 80ms+
- 静置也在 60fps 全速重绘（光标闪烁/平台驱动）→ 21ms 基线每帧都在付
- 新增布局细分：`draw_detail layout=? (taffy=? prepaint=?) paint=?`（request_layout vs prepaint_as_root 分段）

### 10. 21:53 编辑版（`This PC-1787579618-hilog.txt`）——旧编译，无 taffy 细分

- frame 33-50ms × 382（主体）；layout 16-33ms × 345 + 33-50ms × 63 + ≥50ms × 14（含 **166ms 尖峰**）；paint 4-8ms × 361 稳定
- 代码确认：`EditorElement::prepaint`（element.rs:8024）**每帧无条件 `editor.snapshot()`** → `display_map.snapshot()`（editor.rs:3010）＝布局段固定成本候选
- 本日志为旧编译（draw_detail 无 taffy/prepaint 括号），需重编重跑

### 11. 22:03 新编译（`This PC-1787580242-hilog.txt`）——prepaint 坐实为病根

| 阶段 | 典型值 | 分档（252 帧） |
|---|---|---|
| **taffy**（整树 flex 布局） | **~6ms** | 4-8ms × 249 → 正常 |
| **prepaint**（元素预处理） | **~21ms** | 16-33ms × 249，首帧 55ms 尖峰 → **绝对大头** |
| paint | ~10ms | 8-16ms 主体 → 次要 |
| 整帧 | **~35-40ms** | ~25-30fps，卡顿明显、不冻屏 |

- **空 tab（0-1 行）prepaint 也稳定 ~21ms → 固定成本，与内容量无关**；不随行数增长
- measure/wrap_sync/rewrap/overlay 全 0 → 之前各假说全排除
- 新增 `editor_prepaint` RAII 计时（element.rs:8005）：编辑器自身 prepaint 耗时

### 12. 22:15 编辑器细分（`This PC-1787580958-hilog.txt`，123 帧）——prepaint 一分为二

| 秒 | draw_detail prepaint 段 | editor_prepaint（最大） | 差值＝窗口其他元素 |
|---|---|---|---|
| 22:15:46 | 21.8ms | 11.4ms | **10.4ms** |
| 22:15:47 | 21.8ms | 10.0ms | **11.8ms** |
| 22:15:49 | 24.7ms | 10.6ms | **14.2ms** |
| 22:15:52 | 24.6ms | 17.7ms | **6.9ms** |

- `editor_prepaint` 8-16ms × 112（主体 ~10-11ms）；`draw_detail prepaint` 20-30ms × 99
- **prepaint 段 ~22ms = 编辑器自身 ~11ms + 窗口其他元素 ~11ms（各占一半）**，均为空 tab 固定成本
- 整帧 33-50ms × 95 + ≥50ms × 27 → ~22-28fps
- 已新增 `editor_snapshot` 计时（element.rs:8041）：拆编辑器 prepaint 内部（snapshot vs 其余 gutter/minimap/样式计算）
- 注意：hilog 行前缀很长，`cut -c1-120` 会截掉 `cost=` 值，分析需看完整行

## 当前结论 (Current Conclusion)

1. **冻屏已缓解为持续掉帧**：measure 无副作用化修复后 rewrap 归零，8s 级 APP_INPUT_BLOCK 不再出现；但**每帧 ~35-45ms（taffy 6ms + prepaint 22ms + paint 10ms）→ ~22-28fps 的卡顿仍稳定存在**；
2. **病根 = prepaint 阶段每帧 ~22ms 固定成本**（空 tab 亦如此），taffy 布局与 paint 均非主因；
3. prepaint 段 = **编辑器自身 ~11ms + 窗口其他元素 ~11ms**，两者内部构成未实测完；
4. 平台特异性：macOS/Linux 同代码不卡 → OHOS 特有的固定开销（候选：`editor.snapshot()` 每帧重建、OHOS `cosmic_text` 文本后端的固定整形/字体解析开销、窗口其他 UI 元素每帧 prepaint）。

## 解决方案 (Solution)

**未解决。** 待下一轮实测数据（`editor_snapshot` 打点）分叉后实施：

### 候选方向 A：编辑器 prepaint ~11ms（若 `editor_snapshot` ≈ 10ms）
- **`Editor::snapshot()` 记忆化/增量**：内容与显示参数未变时不重建完整 display map + 高亮表，复用已有快照（消除每帧 `snapshot()` 的 O(内容) 固定重建）
- 位置：`crates/editor/src/editor.rs` `fn snapshot` / `crates/editor/src/display_map`（`display_map.snapshot()` 增量复用）

### 候选方向 B：编辑器 prepaint ~11ms 但 snapshot 很小（若 `editor_snapshot` ≈ 1-2ms）
- 其余成本在 `gutter_dimensions` / `get_minimap_width` / 样式与字体度量计算（element.rs:8046-8079 每帧执行）→ 结果缓存（仅字体/样式/尺寸变化时重算）
- `resolve_font`/`em_width`/`em_advance` 每帧调用（element.rs:8047-8052）→ 若 OHOS 后端这些调用慢（`font_ids_by_family_cache` 未覆盖的路径），可缓存

### 候选方向 C：窗口其他元素 ~11ms
- 需定位是哪类元素：对照实验（收起左侧项目树/右侧面板复测，prepaint 段是否骤降）或开 gpui `profiler` feature 拿元素级耗时分布
- 若是文本整形 → OHOS `layout_line` 降级 `Shaping::Basic`（牺牲少量排版质量）或预整形缓存；若是特定面板元素 → 该元素 prepaint 瘦身

### 候选方向 D（呼应"字体适配"线索）
- 若窗口其他元素 ~11ms 与文本整形/字体解析强相关，则与 00:47 已记录的 diff 视图字体排版卡死（`2026-08-24-ohos-font-layout-hang-unresolved.md`）同源：**OHOS `layout_line` 缺 fallback 链规划 + 逐字符排版过重**，对齐桌面版 `cosmic_text_system.rs` 的 `fallback_chain`/`compute_run_spans` 是治本方向

## 已实施修改 (Implemented Changes)

1. **measure 无副作用化（已生效，治本一半）**：`crates/editor/src/element.rs`
   - `compute_auto_height_layout`：删除测量闭包内 `editor.set_wrap_width()` 副作用，measure 变纯函数
   - `prepaint`：AutoHeight 的 wrap_width 提交改由 prepaint 每帧一次完成（不再每帧被 taffy 多次触发 rewrap）
2. **运行时打点（临时，待清理）**：5 个文件、全部带 `[freeze-trace]` 前缀，`grep -v`/`git checkout` 可一键移除
   - `crates/gpui/src/window.rs` — `frame #N cost=?`（整帧）、`draw_detail layout=? (taffy=? prepaint=?) paint=?`（分段）
   - `crates/editor/src/element.rs` — `measure #N wrap_width=? changed=? buffer_rows=? target=?`（auto-height 测量）、`editor_snapshot cost=?`（snapshot 计时）、`editor_prepaint cost=?`（编辑器 prepaint 计时）、`nav_overlay_layout sets/total/target`（overlay 布局）
   - `crates/editor/src/display_map/wrap_map.rs` — `rewrap start/done rows=N cost=?`（全量重排）、`wrap_sync(incr) rows/cost`（编辑增量同步）
   - `crates/gpui/src/text_system/line_layout.rs` — `line_cache hits=? miss=?`（跨平台整形缓存命中）
   - `crates/vim/src/vim.rs` — `helix_jump overlays=N`（vim 跳转 overlay 计数）

## 下一步 (Next Steps)

1. 重编（确认 `window.rs` 含 `taffy=`、`element.rs` 含 `editor_snapshot`）→ 空 tab 静置 10s + 长按 Enter 10s 各抓一份 hilog
2. 判定 `editor_snapshot`：≈10ms → 走方向 A；≈1-2ms → 走方向 B
3. 对照实验定位窗口其他 ~11ms：收起侧边栏/右侧面板复测；或开 gpui `profiler` feature
4. 修复落地后清理全部 `[freeze-trace]` 打点，回归验证（空 tab 静置 frame < 16ms）

## 修改文件 (Modified Files)

- `crates/editor/src/element.rs` — 修复（measure 无副作用化）+ 打点
- `crates/gpui/src/window.rs` — 打点（整帧/分段）
- `crates/editor/src/display_map/wrap_map.rs` — 打点
- `crates/gpui/src/text_system/line_layout.rs` — 打点
- `crates/vim/src/vim.rs` — 打点
- 参考：`crates/gpui_ohos/src/ohos/text_system.rs`（OHOS 文本后端，候选修改点）
- 参考：`crates/gpui_wgpu/src/cosmic_text_system.rs`（桌面版 fallback 参考实现）

关联：[[2026-08-24-ohos-font-layout-hang-unresolved]]（diff 视图字体排版卡死，同源候选）
