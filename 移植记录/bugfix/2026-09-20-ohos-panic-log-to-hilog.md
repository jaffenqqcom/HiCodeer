# OHOS Rust panic 只有信号没有原因——panic 消息接通 hilog

## 问题描述

OHOS 移植版里，每一次 Rust panic 都只留下一个 `SIGABRT` 信号和系统侧的 `LastFatalMessage`，**没有 panic 的消息与源码位置**。原因是 OHOS 分支的 panic hook 收了 `message`/`location` 却直接丢弃后 `abort`，且它根本没有被安装。本次把 panic 消息接到 `log::error!`，从而经 zlog 落到 hilog，使 `hilog | grep -i panicked` 能看到线程名、位置与消息。

## 问题表现

- cppcrash 日志（`/data/log/faultlog/faultlogger/cppcrash-com.hicodeer.studio-<...>.log`）只有：
  - `Reason:Signal:SIGABRT(SI_TKILL)`
  - `Fault thread: Tid <n>, Name hicodeer.studio` → 崩溃的是主线程本身
  - `LastFatalMessage:[NativeXComponentDispatchMouseEvent] crash occured on callback`
  - 只有系统侧文案，没有 Rust 侧原因。
- `hdc shell hilog | grep -i panicked` 零命中。
- 每次定位崩溃都只能靠调用栈反推；而栈符号化又受 `so` 版本失配制约（本地一旦重新 `bundle-ohos`，设备上的 `so` 与本地不再一致，符号化会解出跨 crate 的杂乱符号），等于双重受阻。

## 问题原因

链条有三段，缺一不可：

**第一段：OHOS 上 `crashes::init` 永不被调用，实际可达的只有 `force_backtrace`。**

```rust
// crates/zed/src/main.rs:405-440
let should_install_crash_handler =
    client::telemetry::should_install_crash_handler(*release_channel::RELEASE_CHANNEL);

let crash_handler = if should_install_crash_handler {
    Some(app.background_executor().spawn(crashes::init(/* ... */)))
} else {
    crashes::force_backtrace();   // ← OHOS 走这里
    None
};
```

```rust
// crates/client/src/telemetry.rs:98-103
pub fn should_install_crash_handler(channel: ReleaseChannel) -> bool {
    matches!(
        env::var("ZED_GENERATE_MINIDUMPS").as_deref(),
        Ok("true" | "1")
    ) || (channel != ReleaseChannel::Dev && MINIDUMP_ENDPOINT.is_some())
}
```

注意这个函数里**没有任何 OHOS 专门分支**，它是通用条件在 OHOS 上自然为 `false`：OHOS 构建是 dev 渠道（`crates/zed/RELEASE_CHANNEL` = `dev`），且未注入 `ZED_MINIDUMP_ENDPOINT`，两个析取项都不成立。

**第二段：OHOS 的 `force_backtrace()` 是空函数，没装 hook。**

```rust
// 改动前，crates/crashes/src/crashes.rs
pub fn force_backtrace() {}
```

于是 panic 走的是 Rust **默认 hook**——默认 hook 把消息打到 **stderr**。OHOS 应用的 stdout/stderr 无人可读，输出直接丢失。这也是"为什么别的日志能进 hilog 而 panic 不能"的答案：普通日志走 `log` 门面宏，panic hook 打印**不经过 `log` 门面**，因此绕开了重定向。

**第三段：即使装了 hook，`panic_hook` 本身也丢弃信息。**

```rust
// 改动前
pub fn panic_hook(_crash_client: Arc<Client>, _message: &str, _location: Option<&Location>) {
    std::process::abort();
}
```

参数带下划线前缀即被丢弃，只 abort。

**附：普通日志之所以能到 hilog**，是因为 zlog 在编译期做了一个单点分流：

```rust
// crates/zlog/src/zlog.rs:84-118
#[cfg(target_env = "ohos")]
{
    ohos::submit_to_hilog(record);
    return;                       // ← 提前返回，永不落到 sink::submit
}
sink::submit(/* ... */);
```

即 OHOS 上日志**从不**进 `sink::submit`（也就从不写 `Zed.log`），而是转交 `crates/zlog/src/ohos.rs` 的 `submit_to_hilog`，hilog tag 固定为 `HiCodeer`（`crates/zlog/src/ohos.rs:31`）。这是整个日志框架里唯一的平台分叉点。

## 解决方案

把 hook 安装在 OHOS 上**真正可达**的那一个函数上，并在 abort 之前把消息经 `log` 门面输出。

```rust
// crates/crashes/src/crashes.rs:52-66
/// Install the panic hook used on OHOS.
///
/// The desktop variant only raises `RUST_BACKTRACE`, leaving the default
/// hook to print to stderr, which is readable there. Neither stdout nor
/// stderr is visible from an OHOS app, so the message has to be routed
/// through `log::error!` instead, which zlog forwards to hilog.
pub fn force_backtrace() {
    std::panic::set_hook(Box::new(|payload| {
        panic_hook(
            Arc::new(Client),
            payload.payload_as_str().unwrap_or("<non-string panic payload>"),
            payload.location(),
        )
    }));
}
```

```rust
// crates/crashes/src/crashes.rs:88-94
pub fn panic_hook(_crash_client: Arc<Client>, message: &str, location: Option<&Location>) {
    let current_thread = std::thread::current();
    let thread_name = current_thread.name().unwrap_or("<unnamed>");
    let location = location.map_or_else(|| "<unknown>".to_owned(), |location| location.to_string());
    log::error!("thread '{thread_name}' panicked at {location}:\n{message}");
    std::process::abort();
}
```

关键决策点：

- **hook 挂 `force_backtrace` 而不是 `init`**——这是本次唯一一个先判断错又纠正过来的点。最初的想法是"把日志补到 `init` 里"，但 OHOS 上 `init` 永不被调用（第一段已证），挂在那里等于没挂。实际可达路径只有 `force_backtrace`。
- **`init` 保持原样不动**，不制造"看起来更统一"但实际不可达的改动。
- 用 `log::error!` 而不是 `eprintln!`，因为只有 `log` 门面才会经过 zlog 的 hilog 重定向（第三段已证）。

## 修改文件

- `crates/crashes/src/crashes.rs` — OHOS 模块内 `force_backtrace()`（:52-66）由空函数改为安装 panic hook；`panic_hook()`（:88-94）在 `abort()` 前用 `log::error!` 输出线程名 + location + message。（20 增 2 删）

已提交：`213d33c3 rust的 panic 日志接通 hilog`。

## 验证与遗留

- 编译：`=== HAP BUILD SUCCESSFUL ===`，签名通过。
- `payload_as_str()` 在当前工具链（rustc 1.97.1）上可用，无编译错误。
- **尚未实测触发**：本轮启动没有发生 Rust panic，`hdc shell hilog | grep -i panicked` 为空。要验证只能等下一次真崩，或人为构造一次 panic。
- 覆盖边界：本改动只覆盖 **Rust panic → abort** 这条通道。若崩溃不经过 panic hook（例如 C/C++ 侧 abort、信号直接致死、double panic 在 hook 内再崩），仍只有系统信号。这类情况需要回到栈符号化，且必须保证设备上的 `so` 与本地构建一致。

## 附：可复用经验

- **判断"某类输出能否进 hilog"，看它是否经过 `log` 门面**。凡绕过 `log` 宏、自己写 stdout/stderr 的输出（默认 panic hook、`println!`、C 侧 printf）在 OHOS 上都会丢，必须改走 `log::*`。
- **"补日志/补 hook"之前先确认目标函数到底会不会被调用**。OHOS 上 `crashes::init` 这条路径是死的，特征是：调用点的条件来自通用逻辑（`should_install_crash_handler`）而非平台分支，它在 OHOS 上恰好恒假。
- **空函数是隐蔽的功能空洞**。`pub fn force_backtrace() {}` 与 `pub fn panic_hook(...) { abort() }` 编译无警告、语义上"有实现"，但实际什么都没做；排查"某能力在 OHOS 上不存在"时，优先怀疑这类带平台分支的空实现。

[[ohos-debug-lessons]]
