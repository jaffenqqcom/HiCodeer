# 点击折叠箭头触发的 selections 断言 abort（disjoint selection start is not resolvable）

## 问题描述

在 OHOS 平台上，点击多缓冲区编辑器 buffer header 上的**折叠箭头**会触发进程级 abort（SIGABRT）。崩溃点不是折叠逻辑本身，而是 `SelectionsCollection::change_with` 内部的一条 debug 断言——它检查每条选区能否在当前 display snapshot 上解析，而崩溃时快照里躺着一条**指向已被移出 multi-buffer 的 buffer** 的陈旧选区。折叠动作只是第一个走到 `change_with` 的操作，把早就存在的状态不一致照了出来。

该断言包在 `if cfg!(debug_assertions)` 里，因此**只有 debug 构建会 abort**。三例崩溃都发生在 debug 版上。

## 问题表现

- `Reason:Signal:SIGABRT(SI_TKILL)`，主线程 tid == pid（`Fault thread info: Tid:12238, Name:hicodeer.studio`）
- `LastFatalMessage` 有两种形态：早期两例是 `[NativeXComponentDispatchMouseEvent] crash occured on callback`（崩在 XComponent 鼠标回调内），第三例是 `[NAPI] Crash occurred on ProcessAsyncHandle`
- hilog 中 panic 文本（经 OHOS panic hook 打到 `HiCodeer` tag）：

```
thread '<unnamed>' panicked at crates/editor/src/selections_collection.rs:583:17:
disjoint selection start is not resolvable for the given snapshot:
Selection { id: 4, start: ExcerptAnchor { text_anchor: Anchor { timestamp: Lamport {<local>: 1}, offset: 1326, bias: Left, buffer_id: BufferId(12884902200) }, path: PathKeyIndex(1), diff_base_anchor: None }, end: <同 start>, reversed: false, goal: None }
```

- 触发动作（符号化栈自下而上）：鼠标抬起 → XComponent 回调 → `Window::on_mouse_event::<MouseUpEvent>` → `ButtonLike::render` → `render_buffer_header` 点击闭包（`crates/editor/src/element/header.rs`）→ `Editor::fold_buffer` → `Editor::fold_buffers` → `SelectionsCollection::change_with` → 断言
- **不稳定复现**（用户原话"有时能出现"）。已掌握的复现路径：让某文件先进入多文件 diff / 多缓冲区视图 → 在其上放过光标 → 让该文件从视图消失（buffer 被移出 multi-buffer）→ 点**任意另一个** buffer 的折叠箭头
- 三次现场：`cppcrash-...-20260920224717825.log`（22:47）、`cppcrash-...-20260920234234782.log`（23:42）、`cppcrash-...-20260921002900255.log`（00:29）

## 问题原因

### 根因链

1. 某文件（实测为 `.gitignore`）曾在 multi-buffer 里有 buffer/excerpt，并在其中留下过光标
2. 该 buffer 被移出 multi-buffer（`buffers` 表里消失），但 `path_keys` 表**保留了**它的 path —— `get_or_create_path_key_index` 只增不减
3. `multi_buffer::Event::BuffersRemoved` 的处理（`crates/editor/src/editor.rs`）清掉了 inlay hints、`registered_buffers`、runnables、semantic tokens、LSP symbols/links、折叠范围等一批 per-buffer 状态，**唯独没有清理 `self.selections`**
4. 陈旧的选区留在 `SelectionsCollection` 里
5. 用户点折叠箭头 → `fold_buffers` → `change_with` → 断言对这条陈旧选区调用 `can_resolve` → 返回 false → abort

### 实测证据（打点输出，决定性）

崩溃前插入的 `[diag]` 打点给出四行：

```
[diag] change_with: rejected selection: ... buffer_id: BufferId(21474836871) ... path: PathKeyIndex(0) ...
[diag] change_with: disjoint_count=1, path_keys=[11 项，第 0 项 = ".gitignore"]
[diag] change_with: buffers=[10 项，不含 ".gitignore"，也不含 BufferId(21474836871)]
[diag] change_with: selection_buffer_id=Some(BufferId(21474836871)), buffer_present=false, buffer_path=None, companion=false
```

- `path_keys` 有 11 项且 `PathKeyIndex(0)` = `.gitignore` ⇒ **索引合法、并非越界**
- `buffers` 有 10 项，既无该 path 也无该 buffer_id ⇒ **buffer 已被移出**
- `disjoint_count=1` ⇒ selections 里就剩这一条陈旧的，断言必炸

旁证：当时 `buffers` 表的 10 个 path 与 `git status` 的已修改清单一一对应，说明出事的是 **Git 面板的多文件 diff 视图**。

### 为什么 anchor 解析不了

`MultiBufferSnapshot::can_resolve` 经 `ExcerptAnchor::try_seek_target` 做双重校验：

```rust
let path_key = snapshot.try_path_for_anchor(*self)?;          // path_keys.get_index(path.0)

let Some(state) = snapshot
    .buffers
    .get(&self.buffer_id())
    .filter(|state| &state.path_key == path_key)
else {
    return Some(AnchorSeekTarget::Missing { path_key });       // 注意：不是 None
};
```

两条失败出口：`try_path_for_anchor` 返回 `None`（索引越界），或返回 `Some(Missing)`（buffer 不存在 / path_key 不匹配）。`can_resolve` 里 `cursor.seek(&Missing)` 后 `cursor.item()` 落空，返回 false。本例走的是第二条。

`PathKeyIndex(u64)` 是 path 在 `Arc<IndexSet<PathKey>>` 里的**位置**而非稳定身份，锚点只在同一条快照谱系内有效；`path_keys` 只增不减，因此"索引仍合法但 buffer 已走"是可达状态。

### 为什么既有的清理逻辑清不掉它

`remove_selections_from_buffer` 用 `anchor_to_buffer_anchor` 判定，而后者**只查 buffer_id 是否存在、完全不校验 path_key**：

```rust
Anchor::Excerpt(excerpt_anchor) => {
    let buffer = self.buffer_for_id(excerpt_anchor.buffer_id())?;
    Some((excerpt_anchor.text_anchor, buffer))
}
```

buffer 已不在时它返回 `None`，而该分支的处理是 `else { true }`——**保留**。另有一层：`fold_buffers` 只对"被点的那一个" buffer 传 id，而陈旧的选区属于 `.gitignore`（已不在表里、header 不显示，用户点的是别的 buffer），连"buffer_id 相等"这条都命不中。

### 一处曾被误判的地方

排查中途一度认为"清理逻辑没机会执行，因为断言在 closure 之前"。**这是错的**：`change_with` 里 `let result = change(&mut mutable_collection)` 在闭包调用处执行，断言在其后。清理逻辑确实跑到了，问题纯粹是**判定口径太松**。

## 解决方案

### 关键认识

症状出现在 `change_with` 的断言上，但断言只是哨兵。要修的不是断言，而是**让陈旧的选区在该消失的时候消失**——这需要同时改"判定口径"与"清理时机"两件事。

### A：收紧判定口径

`crates/editor/src/selections_collection.rs` 的 `remove_selections_from_buffer`，在 filter 开头加一道以 `can_resolve`（与断言同一判定）为准的检查，不可解析者直接丢弃：

```rust
.filter(|selection| {
    #[cfg(target_env = "ohos")]
    // On OHOS a buffer can leave the multi-buffer while selections
    // made in it linger. Those can never be resolved again and would
    // make the resolve assertion in `change_with` fail on the next
    // edit, so drop them here. `anchor_to_buffer_anchor` alone is not
    // strict enough: it only checks that the buffer id is known,
    // ignoring whether the anchor's path key still matches.
    if !self.snapshot.can_resolve(&selection.start) {
        changed = true;
        return false;
    }

    if let Some((selection_buffer_anchor, _)) =
        self.snapshot.anchor_to_buffer_anchor(selection.start)
    { /* ...原逻辑不变... */ }
})
```

原 `else { true }` 分支保留：`Anchor::Min` / `Anchor::Max` 在 excerpts 为空时会走到它，那类锚点本就总能解析，丢弃没有意义。

### B：补上清理时机

`crates/editor/src/editor.rs` 的 `BuffersRemoved` 分支，在 `unfold_buffers` 之后补一次针对被移除 buffer 的选区清理：

```rust
#[cfg(target_env = "ohos")]
{
    let snapshot = self.display_snapshot(cx);
    self.selections.change_with(&snapshot, |selections| {
        for buffer_id in removed_buffer_ids {
            selections.remove_selections_from_buffer(*buffer_id);
        }
    });
}
```

**A 与 B 各治一半**：A 让清理**删得掉**，B 让它**在该删的时候被调用**。缺 A，B 调用后仍会保留；缺 B，陈旧选区要等下一次 `remove_selections_from_buffer` 恰好被调用才清掉，其间任何 `change_with`（例如单纯移动光标）都会 abort。

### 为什么用 `#[cfg(target_env = "ohos")]` 包裹

`crates/editor/src/` 是上游共享路径，改动会影响其他平台的编译与行为。本问题的产生条件（选区在 buffer 被移出 multi-buffer 后长期驻留）在 OHOS 上实测成立，但没有在其他平台上验证过，因此两处改动都收敛到 OHOS 之内，不触碰其他平台的既有行为。

需要强调的是：被包裹的**不是日志，而是实质逻辑** —— A 的 `can_resolve` 判定（丢弃无法解析的选区）与 B 的清理调用。少了它们修复就不成立；`#[cfg]` 在这里的职责只是"让这份修复不越出 OHOS"，而不是"屏蔽某种可有可无的输出"。

### 走不通 / 被否决的路

- **只想靠 release 绕过**：断言在 `cfg!(debug_assertions)` 内，release 确实不会在这里 abort。但这只是掩盖症状——状态不一致仍然存在。被明确否决，要求真修
- **第一版 B 直接调 `self.selections.remove_selections_from_buffer(..)`**：编译报 `error[E0599]`，该方法是 `MutableSelectionsCollection` 的方法，`SelectionsCollection` 上没有，只能经 `change_with` 进入
- **保留 `[diag]` 打点作为"异常处理分支"**：否决。打点里真正有价值的判定逻辑已被 A 完整吸收，剩下的只是日志输出；且它位于断言之前，每次 `change_with` 都要在 debug 版多扫一遍选区，修复生效后更退化为永不触发的死代码
- **删掉 `change_with` 里的断言**：否决。断言是发现 invariant 被破坏的哨兵，删掉会把真实 bug 掩盖成静默错误
- **去掉 `#[cfg(target_env = "ohos")]`、让所有平台一起修**：一度按此改过，随后回退。A 的 `can_resolve` 判定是修复的实质 —— 少了它，`remove_selections_from_buffer` 对"buffer 已移出"的选区仍走 `else { true }` 保留，B 即使被调用也删不掉。既然它是实打实的逻辑改动、落点又在上游共享路径，按 `CODEBUDDY.md` 第 9 条"不能改变软件在其他操作系统的编译和功能"，两处改动都必须收敛到 OHOS 之内

### 验证

- `./script/bundle-ohos`（debug）通过：`=== HAP BUILD SUCCESSFUL ===`，0 error
- 产物 `libhicodeer.so` 内 `[diag] change_with` 标记 4 处，装机后用户验证不再 abort
- 清理完成后再次编译，产物内打点标记归零

### 关于日志

判定分支里**没有加日志**。丢弃一条无法解析的选区是本修复的正常工作路径，加日志属于多余的留痕；真要复查时，`change_with` 的断言仍在原位，一旦还有别的路径制造陈旧选区，照样会以 panic 文本暴露出来。

## 修改文件

- `crates/editor/src/selections_collection.rs` — `remove_selections_from_buffer` 的 filter 增加 `#[cfg(target_env = "ohos")]` 包裹的 `can_resolve` 判定，不可解析的选区直接丢弃（口径修正）
- `crates/editor/src/editor.rs` — `multi_buffer::Event::BuffersRemoved` 分支增加 `#[cfg(target_env = "ohos")]` 包裹的清理块，经 `change_with` 对被移除 buffer 调用 `remove_selections_from_buffer`（时机补全）

诊断期间临时改动过、结案前已完全回退的文件：

- `crates/multi_buffer/src/multi_buffer.rs` — 曾新增 `pub fn path_keys()` 只读访问器以便打点输出有序 path 全表；打点撤除后该访问器已随之删除，此文件回到零 diff

参见 [[ohos-debug-lessons]]。
