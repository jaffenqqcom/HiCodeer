# OHOS 重启后文件 tab 不恢复（worktree 扫描器重启吞掉强制刷新结果）

## 问题描述

关闭应用前打开着文件 tab，重启后该 tab 不会自动恢复。
左侧/底部等 **panel 全部正常恢复**，唯独中心区的**文件 tab 不恢复**。

恢复机制本身是工作的：`restore_on_startup` 默认 `LastSession`，日志显示工程
`/storage/Users/currentUser/workspace/zcoder` 确实被自动打开，数据库里也存着
待恢复的条目（`items` 表的 `kind='Editor'` 行）。失败发生在"把条目重新打开成编辑器"
这一步：worktree 里查不到该文件的条目，条目被丢弃。

重要的是：这是**概率性**故障。同一台设备、同一个构建，有时恢复成功、有时失败。

## 问题表现

- 重启后 panel 回来了，文件 tab 没回来
- hilog 报错（每个待恢复文件一条）：

```
E com.zcoder.studio/Zcoder: Failed to open path in project:
    Could not find entry in worktree for "crates/acp_thread/src/acp_thread.rs" after refresh
```

- 磁盘上文件**存在**（`ls` 可确认）；数据库 `items` 表里该条目**存在**
- 时好时坏：连续重启若干次，失败若干次
- 手工从项目面板点开同一个文件**正常**（说明不是权限、不是文件缺失）

## 问题原因

### 因果链

恢复流程：`restore_or_create_workspace` → `Workspace::load_workspace`
（`crates/workspace/src/workspace.rs`）对每个 `kind="Editor"` 的 item 调
`Editor::deserialize`（`crates/editor/src/items.rs`）→
`Project::open_path` → `Worktree::refresh_entry`（`crates/worktree/src/worktree.rs`）
→ 发一个带 barrier 的"按路径强制刷新"请求 → 等 barrier → 读一次前台快照
→ 读不到就报 `Could not find entry ... after refresh`。

同时，启动阶段还有另一条链路在跑：

```
工作区创建（seeded_entries=1）
   ↓ 约 0.73 秒
WorktreeSettings 变化（只有 file_scan_exclusions 由默认值变为真实值，其余字段全部未变）
   ↓
restart_background_scanners
   ↓
新扫描器以前台当前快照为初始状态（worktree.rs: `let snapshot = self.snapshot();`）
   ↓
前台快照被"回退"
```

两条链路撞在一起时：

1. 强制刷新已经查到条目并插入旧扫描器，条目随之进入待投递的快照
   （日志 `pre-send check: present=true`）
2. 紧接着扫描器重启，新扫描器用**前台那一刻的旧快照**播种，于是前台被回退到
   重启前的状态 —— 刚插入的条目没了
3. 恰好此时，等待方（`refresh_entry`）等的 barrier 被释放
4. `refresh_entry` 只读一次快照，读不到就直接报错；**该函数没有任何重试**

### 关键证据（两轮故障日志，模式完全一致）

第二轮（复现于 00:54）：

```
16.676  start_background_scanner: seeded_entries=1
17.272  refresh_entry issued: path="crates/acp_thread/src/acp_thread.rs"
17.303  pre-send check: present=true changed=85      ← 条目已插入后台
17.311  deliver result: sent=true entries=2781       ← 投递的快照含该条目
17.404  WorktreeSettings changed: ... exclusions_same=false ...
17.404  restart_background_scanners: fg_seed_entries=2407   ← 用前台旧快照播种
17.404  start_background_scanner: seeded_entries=2407
17.414  fg applied update: entries=2407              ← 前台被回退，条目没了
17.415  refresh barrier resolved
17.415  refresh_entry miss
17.558  Failed to open path
```

第一轮（复现于 00:54 之前）同样是"投递 3502 条 → 被 2918 条覆盖回退"，
其中 **2918 正好等于请求发出前前台持有的条目数** —— 这与"新扫描器以前台快照播种"
完全吻合，是定位根因的关键数字。

### 为什么时好时坏

窗口只有约 0.1 秒（设置加载完成 → 扫描器重启）。恢复请求正好落在这个窗口里才会中招。

### 为什么以前没有

当时没有确认到历史证据，不做推断。可以确定的是触发条件：
**恢复请求落在"设置异步加载完成 → 扫描器重启"这个窗口内**。工程越大、初次扫描越久，
窗口的相对位置越容易撞上恢复时机。

## 解决方案

### 关键洞察

扫描器重启**只是回退状态，新扫描器本身完全能正常响应同一个刷新请求**
（日志里后续的刷新调用都是成功的）。所以对"强制刷新"做**有界重试**即可稳定命中，
不需要去改动扫描器重启这个上游机制。

### 修改（`Worktree::refresh_entry`，唯一改动的函数）

```rust
// 新增具名常量
const REFRESH_ENTRY_MAX_ATTEMPTS: usize = 3;
const REFRESH_ENTRY_RETRY_DELAY: Duration = Duration::from_millis(50);

cx.spawn(async move |this, cx| {
    for attempt in 0..REFRESH_ENTRY_MAX_ATTEMPTS {
        if attempt > 0 {
            // 上一次刷新可能被"即将被替换的扫描器"应答，其结果随后被回退；
            // 针对当前在位的扫描器重新发起请求。
            cx.background_executor().timer(REFRESH_ENTRY_RETRY_DELAY).await;
            let retry = this.read_with(cx, |this, _| {
                this.as_local()
                    .map(|local| local.refresh_entries_for_paths(vec![path.clone()]))
            })?;
            match retry { Some(receiver) => refresh = receiver, None => break }
        }
        refresh.recv().await;
        let found = this.read_with(cx, |this, _| this.entry_for_path(&path).cloned())?;
        if let Some(entry) = found {
            return Ok(Some(entry));
        }
    }
    // 三次都失败才报原来的错误（错误文案不变）
    let new_entry = this.read_with(cx, |this, _| {
        this.entry_for_path(&path).cloned().with_context(|| {
            format!("Could not find entry in worktree for {path:?} after refresh")
        })
    })??;
    Ok(Some(new_entry))
})
```

上游原行为是"发一次请求 → 等 barrier → 读一次 → 查不到直接报错"，本次改动只增加了
重试循环，成功路径与失败文案都保持不变。

### 为什么选这个方案（备选方案对比）

- **恢复侧等工作区初次扫描完成**（`Project::wait_for_initial_scan`）：能一次性消除
  这一类竞态，但要改 `crates/zed` / `crates/workspace`（非 ohos 文件需 cfg 包裹），
  且 tab 要等扫描完才出现，代价大
- **让新扫描器继承旧扫描器的状态**：这才是"重启接替未完成事务"的正确做法，
  根治性更好；但旧扫描器的 state 被封在旧任务里（`_background_scanner_tasks` 一换就被
  drop），要取出得改成 `Arc<Mutex<BackgroundScannerState>>` 存在 `LocalWorktree` 上，
  改动面明显大于重试，风险更高
- **让扫描器等设置加载完成再启动**：可从根上消掉这次重启，但需要确定"设置就绪"的判定方式

结论：先用重试解决问题（必要且充分），根治性方案另立任务。

### 验证

修复版跑了一轮：启动时"设置变化 + 扫描器重启"**照常发生**
（`restart_background_scanners: fg_seed_entries=2564`），但不再出现
`Failed to open path`，tab 恢复成功。

注意：这是概率性故障，单次通过不足以定论。已由用户**连续多次重启验证通过**，未再复现。

## 走过的弯路（重要，避免重复）

按时间顺序，以下假设都被证据推翻过：

1. **"序列化没写进去"** → 查数据库，`items` 表里那条 Editor 记录**存在** ✓
2. **"editors 表与 items 表不一致，说明序列化中断"** → 两者由不同流水线写入
   （editor 自身持久化 vs `save_workspace`），不能这样推断
3. **"权限/授权问题（dir to uri 没做）"** → 被日志否定：强制刷新时
   `fs.metadata` + `fs.canonicalize` 对相关路径**全部成功**
   （诊断输出 `results=[...=found]`）；如果是权限问题，这里会是 `missing`/`error`
4. **"`send_status_update` 的提前返回分支释放了 barrier"** → 确实是一个**真实存在的
   隐患**（有 barrier 待发时也会 return，barrier 被释放却没投递更新），
   修了之后**故障依旧**，说明它不是本次根因；本次改动已把该修改**回退**，
   保持改动面最小，作为独立问题另行记录
5. **"前台没收到投递"** → 诊断证明：投递 `sent=true`，且前台确实应用了更新，
   只是应用的是**回退后的快照**

导致这些弯路的方法论教训：**加日志一次要加全**，覆盖"发出 / 送达 / 应用"三段。
本次为此编译了 5 次才把链路打通，见 skill `debug-log-process`。

## 遗留问题（本次未修，建议独立任务）

- **a**：`send_status_update`（`crates/worktree/src/worktree.rs` 的提前返回分支）
  在有待发 barrier 时也会 `return`，会导致 barrier 被释放但没有任何更新投递 ——
  同类隐患
- **b**：`restart_background_scanners` 用**前台快照**给新扫描器播种，导致
  "只到达旧扫描器、尚未送到前台"的条目丢失（本次根因）。彻底修法是让新扫描器
  **继承旧扫描器的状态**
- **c**：`WorktreeSettings` 在启动后约 0.7 秒才加载完成（`file_scan_exclusions`
  由默认值变为真实值）才触发这次重启。若让扫描器**等设置就绪再启动**，
  可从根上消除该窗口

## 修改文件

- `crates/worktree/src/worktree.rs` — `Worktree::refresh_entry` 增加有界重试：
  读不到条目时针对当前在位的扫描器重新发起强制刷新，最多 `REFRESH_ENTRY_MAX_ATTEMPTS`
  次、间隔 `REFRESH_ENTRY_RETRY_DELAY`；新增这两个具名常量并附原因注释。
  成功路径与失败错误文案均保持原样。文件内其他改动（诊断日志）已全部清理

[[ohos-debug-lessons]]
