# OHOS 上 diff 视图打开时字体排版卡死（主线程 APP_INPUT_BLOCK）【未解决】

## 问题描述 (Problem Description)

zcoder（Zed 移植到 HarmonyOS NEXT / OpenHarmony）在 git 面板打开 diff 视图时主线程周期性卡死：点击 diff/commit 条目后界面无响应，`appfreeze` 报告原因 `APP_INPUT_BLOCK`（输入事件阻塞超过 8000ms）。系统 watchdog 抓取到主线程完整调用栈，最终定位到 **cosmic_text 文本排版（字体回退）** 路径。本问题与已修复的「commit 视图渲染读剪贴板」是**两个独立问题**——剪贴板读取已修复并验证归零，但卡死依旧，说明这才是真正的主线程忙跑根源。**当前未解决，暂缓处理。**

## 问题表现 (Symptoms)

- git 面板打开 diff/commit 视图后，主界面冻结，点击条目响应延迟 5~6 秒或完全无响应
- `top` 显示 `com.zcoder.studio` CPU 159%~250%，主线程状态 `R`（running 忙跑）
- `appfreeze` 报告 `Reason: APP_INPUT_BLOCK`，`Wait Event to be marked exceed 8000ms`，`THREAD_BLOCK_3S`
- 系统反复出现 `llvm-addr2line`（解析卡死栈）、`XCollie StartProfileMainThread`、`Vsync recv timeout`、`ProcessJank jank >= threshold`
- 主线程卡死的调用栈（appfreeze）完整指向 cosmic_text 排版路径
- 已确认 cmd-agent 远程命令执行完全正常；已确认 commit 视图剪贴板读取（另一问题）修复后仍卡死

## 问题原因 (Root Cause)

**根因：OHOS 的 `layout_line` 缺少桌面版的「字符覆盖检查 + 字体 fallback 链规划」，导致 cosmic_text 对缺字字符做昂贵的内建 fallback（遍历字体 + 解析 GPOS 表），在编辑器换行宽度计算场景下卡死主线程。**

### 完整调用链（appfreeze 主线程栈，Tid=48429）

```
DiffMultibuffer::refresh                      打开 diff 视图，刷新 diff
  → register_buffer                          注册 diff buffer
    → conflict_view::conflicts_updated / buffer_ranges_updated
      → Editor::remove_blocks
        → DisplayMap::remove_blocks
          → sync_through_wrap                 换行同步
            → WrapSnapshot::update
              → LineFragmentBuilder::push_fragments
                → replacement_width            对每个字符计算宽度
                  → TextSystem::layout_width
                    → OhosTextSystem::layout_line   OHOS 文本排版
                      → cosmic_text shape_fallback   字体回退
                        → harfrust GPOS script_list  解析字体布局表
                          → read_fonts::from_be_bytes 读字体字节
```

### 关键机制差异（桌面 vs OHOS）

**桌面版**（`crates/gpui_wgpu/src/cosmic_text_system.rs`，`layout_line`）：
- 先为每个 run 构建 `fallback_chain`（fallback 字体列表）
- 用 `compute_run_spans`（cosmic_text_system.rs:841）遍历 grapheme，通过 `charmap_covers`（用 swash charmap 检查字符是否被字体覆盖）把**缺字字符提前分配到 fallback 字体的 span**
- 每个 span 用对应字体一次排完，避免 cosmic_text 内建昂贵的逐字符 fallback

**OHOS 版**（`crates/gpui_ohos/src/ohos/text_system.rs`，`layout_line` 501-591）：
- 只把所有字符用 primary font 排进 `attrs_list`（text_system.rs:504-525）
- **没有 fallback_chain、没有 compute_run_spans、没有字符覆盖检查**
- 缺字字符（diff 内容中的特殊符号/emoji/非 ASCII）触发 cosmic_text 内建 fallback：shape 时逐字遍历所有系统字体找字形，每个字体可能解析 GPOS 表 → 极慢

### 为什么在 diff 视图场景卡死

`WrapSnapshot::update` 里的 `replacement_width`（`<editor::display_map::wrap_map::LineFragmentBuilder>::replacement_width`）对**每个字符**单独调 `layout_width`（gpui/src/text_system.rs:208，把单字符编码后调 `platform_text_system.layout_line`）。diff 内容有大量字符，若存在缺字字符，**每个字符都触发一次完整的 cosmic_text 高级排版（Shaping::Advanced + GPOS 解析 + 内建 fallback）** → 大量字符 × 每次昂贵排版 = 主线程卡死 8 秒+。appfreeze 显示 **23 个线程**的栈都涉及 `cosmic_text`/`read_fonts`/`harfrust`，是进程级排版卡住。

### 已排除的方向

- ❌ cmd-agent 通道（共享表改造后命令执行全正常）
- ❌ commit 视图渲染读剪贴板（方案 A 已修复并验证归零，但卡死依旧）——这印证剪贴板读取是症状不是根因
- ⚠️ 桌面版同样用 `Shaping::Advanced`，说明排版模式本身不是差异；差异在 fallback 链规划

## 解决方案 (Solution)

**未解决，暂缓处理。** 候选修复方向（需先讨论授权，涉及 `crates/gpui_ohos/src/ohos/text_system.rs`，含 ohos 可改）：

### 方案 1（对齐桌面，治本，候选）
把 `gpui_wgpu` 的 fallback 逻辑移植到 OHOS `layout_line`：
- 为 run 构建跨族 `fallback_chain`（当前 OHOS 的 `loaded_font_ids` 只返回同一族的多字重，需补跨族 fallback：HarmonyOS Sans → Noto Sans CJK → 其他系统字体）
- 移植 `charmap_covers` / `compute_run_spans` / `RunSpan` / `pick_covering_slot` / `slot_font_id`（纯函数，可直接复用）
- 可行性已确认：OHOS `LoadedFont` 有 `font.as_swash()`，`charmap_covers` 逻辑可复用（text_system.rs:385 已用 `as_swash().charmap()`）

### 方案 2（临时缓解）
对短文本（单字符，如 `replacement_width` 场景）在 `layout_line` 用 `Shaping::Basic` 替代 `Advanced`，避免 GPOS 解析。会牺牲排版质量（连接字/脚本渲染）。

### 待验证的假设
卡死主因到底是「缺字字符触发内建 fallback」还是「`replacement_width` 对大量 ASCII 字符逐个排版本身过重」——这决定走方案 1（补 fallback 链）还是优化替换宽度计算。需进一步用 appfreeze 栈或加日志确认。

## 修改文件 (Modified Files)

- 暂无（未修复）。相关文件（供后续参考）：
- `crates/gpui_ohos/src/ohos/text_system.rs` — OHOS 文本排版，`layout_line` 缺少 fallback 链规划（候选修改点）
- `crates/gpui_wgpu/src/cosmic_text_system.rs` — 桌面版参考实现（charmap_covers / compute_run_spans / fallback_chain）
- `crates/gpui/src/text_system.rs` — `layout_width` 对每个字符调用 layout_line（问题放大点）
- `crates/git_ui/src/diff_multibuffer.rs` — 打开 diff 触发 refresh → 换行重排的入口

参考：[[ohos-debug-lessons]]
