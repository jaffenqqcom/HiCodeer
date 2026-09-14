# OHOS 后台 dispatch 线程复用改造（新增通用 worker-pool）

## 问题描述

`OhosDispatcher::dispatch()`（`crates/gpui_ohos/src/ohos/dispatcher.rs`）实现为每次调用
`std::thread::spawn`：每个后台任务新建一个 OS 线程、跑完即销毁，**线程不复用**，且是
`gpui_ohos/src` 内唯一的 per-task spawn 点。桌面 Linux 用固定常驻 worker 池
（`crates/gpui_linux/src/linux/dispatcher.rs:28-52`）消费 `PriorityQueueReceiver`，macOS 委托 GCD
系统池，线程均复用。OHOS 移植采用了最简实现（每任务一线程），没有随桌面模型池化。

## 问题表现

- `dispatcher.rs` `dispatch()` 每任务 `std::thread::spawn(move || runnable.run())`，无线程复用。
- `dispatch()` 承载 executor 调度唤醒等高频轻任务，每任务一次内核线程创建/销毁（量级几十~上百微秒），
  高频下形成线程 churn；诊断时线程名不可追踪、一批短命线程反复出现。
- `dispatch()` 的 `_priority` 参数被忽略（每任务独立线程、无排队，优先级本无意义）——顺带丢失了与桌面
  对齐的 Priority 语义。

## 问题原因

平台 dispatcher 各自实现后台执行后端，三者模型不同：

- Linux：`new()` 里按 `available_parallelism` 建 N 个常驻 `Worker-N`，`for runnable in receiver.iter()`
  阻塞消费共享的 `PriorityQueueReceiver`（带 Priority 加权随机出队，High/Med/Low 权重 60/30/10）；
  `dispatch()` 只是 `send`。线程复用。
- macOS：`dispatch()` 委托 `DispatchQueue::global_queue()`（GCD 系统线程池），线程由系统复用。
- OHOS：`dispatch()` 直接每次 `std::thread::spawn`——移植早期取最简实现，未做池化；这也是唯一不复用的一路。
  `dispatch_on_main` 走 main_sender+waker、`dispatch_after` 走 FFRT timer（均复用系统能力），无需改。

根因不是单个逻辑错误，而是**实现选择**：把一个本应按队列+常驻 worker 建模的后台执行，写成了每任务 spawn。

## 解决方案

**核心思路**：参照 LinuxDispatcher 的 worker 池结构，抽出一个平台无关、纯 std 的通用线程池能力库
`worker-pool`，再把 `OhosDispatcher::dispatch()` 的后台执行后端改为投递到该池的一个独立实例。

### 新增通用能力库（路线甲：独立纯 std 子 crate）

- 位置：`crates/gpui_ohos/depend/openharmony-ability/crates/worker-pool/`（openharmony-ability workspace
  内新增成员）。做成独立纯 std crate 而非放进核心 `ability` crate：核心 ability 强依赖 OHOS
  （napi/arkui/ffrt `#[link]`），host 无法 `cargo test`；独立 crate 仅依赖 `log`，可在 host 单测、可被
  任何 OHOS 适配复用（为将来 `util::command` 等预留）。
- `WorkerPool`：可实例化组件类（每业务各建池，不做全局单例）；`PoolConfig{thread_name_prefix, worker_count}`；
  `dispatch(job)` 默认 Medium、`dispatch_with_priority(Priority, job)`；Drop 优雅关闭（先关发送侧、再 join，
  已入队/在跑任务不抛弃）；任务 `catch_unwind`（panic 不杀 worker）；线程命名 `{prefix}-{index}`。
- 三档优先队列对齐 Linux：三个 `VecDeque`（high/medium/low），出队用 loaded-die 加权随机
  （gpui `queue.rs` 同款算法），纯 std `Mutex`+`Condvar`；随机源为每 worker 一个自含 xorshift64*
  （固定种子、无 rand 第三方依赖）。权重常量 60/30/10 注释注明取自 `scheduler::Priority::weight`。
- host 单测 8 项全过：任务全执行、单 worker 同档 FIFO、三档均被执行、多 worker 并发、panic 不杀 worker、
  Drop 优雅退出、50k 高频稳定、线程命名。

### 接入 OhosDispatcher

- `dispatcher.rs`：新增字段 `_background_pool: Arc<WorkerPool>`（Arc 保活池的共享状态到 dispatcher 生命周期）；
  文件级常量 `BACKGROUND_THREAD_NAME_PREFIX="gpui-ohos-bg"`、`MIN/MAX_BACKGROUND_WORKERS=1/4` +
  `background_worker_count()`（`available_parallelism().clamp(1..=4)`）。
- `dispatch()` 改为：`match priority` 把 `Priority::High/Medium/Low` 映射到 `PoolPriority`，投递
  `self._background_pool.dispatch_with_priority(p, || runnable.run())`（丢弃 `Runnable::run` 返回的 bool）；
  `Priority::RealtimeAudio` 理论不达（executor 已拦走 `spawn_realtime`），防御性走独立 `thread::spawn`
  并 `log::error!`，绝不静默丢任务。
- `dispatch_on_main_thread` / `dispatch_after` / `spawn_realtime` / `is_main_thread` / `now` 全部不动。

### 方案取舍记录（含被否路线）

- 位置曾考虑放 openharmony-ability 核心 `ability` crate → 否：会背上 OHOS 依赖、host 无法单测，
  且 pool 是通用件非 OHOS 系统能力。
- 方案 B：用 FFRT `ffrt_submit_*` 交系统共享 worker 池 → 否：协程模式下单 QoS 并发硬上限 16、用户态无 API
  可调高；协程非抢占，dispatch 内长阻塞会占 worker；Priority 映射不完整；新增 FFI 依赖本机 SDK。
- 复用 Linux 整类（gpui_ohos 依赖 gpui_linux）→ 否：拖入 calloop 与整套 Linux 平台依赖，main 唤醒/
  定时机制与 OHOS 不同。
- 给 `util::command` 也套同一池 → 评估后否：OHOS util::command 已是纯 async（`smol::process` 本地 +
  cmd-agent executor 远程），无 per-command 线程，套池会把 async 阻塞化。池保留为可复用能力，仅用于
  dispatch 这一处。

### 设备验证结果

- 真机日志 `WorkerPool started: name_prefix="gpui-ohos-bg", workers=4` 恰好出现一次。
- `/proc/<pid>/task/*/comm` 中 `gpui-ohos-bg-0..3` 四个常驻线程，等待/操作 20s 后数量仍为 4（无复建）。
- 无 worker panic、无 `dispatch received RealtimeAudio`、无 shut down、无 Fatal；UI 操作后调度正常无回归。

## 修改文件

- `crates/gpui_ohos/depend/openharmony-ability/crates/worker-pool/Cargo.toml` — 新增 crate 元数据：
  name=worker-pool, version=0.1.0，依赖仅 `log = "0.4"`（禁 workspace=true，因被 zcoder 外层 workspace 消费）。
- `crates/gpui_ohos/depend/openharmony-ability/crates/worker-pool/src/lib.rs` — 新增 `WorkerPool`/
  `PoolConfig`/`PoolPriority` 完整实现（三档加权随机队列、自含 PRNG、关闭/panic 语义）及 8 项 host 单测。
- `crates/gpui_ohos/Cargo.toml` — 在 `[target.'cfg(target_env = "ohos")'.dependencies]` 追加
  `worker-pool = { path = "depend/openharmony-ability/crates/worker-pool" }`。
- `crates/gpui_ohos/src/ohos/dispatcher.rs` — `dispatch()` 后台执行由每任务 `std::thread::spawn` 改为投递到
  常驻 `WorkerPool`（`gpui-ohos-bg`）；新增 `_background_pool` 字段、池线程数常量与 helper、Priority→
  PoolPriority 映射、RealtimeAudio 防御分支。

[[ohos-debug-lessons]]
