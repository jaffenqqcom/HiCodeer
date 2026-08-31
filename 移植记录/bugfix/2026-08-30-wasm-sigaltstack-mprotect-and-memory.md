# wasmtime 在 OHOS 崩溃：sigaltstack mprotect ENOMEM + 内存释放时机（patch wasmtime 方案）

## 问题描述

zcoder 打开 C++ 项目触发 tree-sitter wasm grammar 解析时 app 崩溃。根因链：OHOS 应用沙箱拒绝 `mmap(PROT_NONE)` → `mprotect(RW)` 的权限升级（返回 ENOMEM），而 wasmtime 恰用这条路径分配每线程的 sigaltstack（信号备用栈），首次进入 wasm 即 panic。尝试用"预注册 altstack"绕过（方案 B）后，又引入**内存所有权/释放时机错误**（use-after-free）和**每线程 512KB 泄漏**（推高沙箱压力致 `pthread_create mprotect` 失败）。最终方案 A（patch wasmtime，让 wasmtime 自己分配并释放）彻底解决。

## 问题表现

三个阶段的崩溃（随修复推进暴露）：

1. **wasmtime sigaltstack（最初）**：`ts_parser_reset` → wasmtime C API `Assertion failed: !trap`；hilog `[qemu-stderr]` 有 `mprotect to configure memory for sigaltstack failed: Os { code: 12, kind: OutOfMemory }`（wasmtime `signals.rs` 的 `.expect`）→ SIGABRT。
2. **pthread_create mprotect（方案 B 泄漏触发）**：`MUSL: pthread_create: mprotect failed, err:Out of memory` → `std::thread::spawn` 的 `.expect("failed to spawn thread")` panic，或 git blame 任务 `Option::expect` panic。
3. **use-after-free（方案 B munmap 触发）**：SIGSEGV，崩溃栈 `[u8;4]::try_from`（ttf_parser 解析字体数据）→ `fontdb::load_fonts_dir`，读**已 munmap 的 altstack 内存**。

## 问题原因

### 第一层：OHOS 沙箱对 mprotect 的特殊限制（一切根源）

OHOS 应用沙箱（XPM LSM `xpm_mprotect_check`）对 mprotect 权限升级有控制，实测规律：
- `mmap(RW)` → `mprotect(RX)`（JIT 编译）：**成功**
- `mmap(PROT_NONE)` → `mprotect(RW)`：**失败 ENOMEM**（PROT_NONE 页不 commit，mprotect 改 RW 时内核要 commit 页/页表，沙箱在此返回 ENOMEM）
- 是**条件性**的（与虚拟内存/VMA 压力相关），不是每次必现。

### 第二层：wasmtime 和 musl 都用这条被限路径

- **wasmtime `allocate_sigaltstack`**（`signals.rs`）：`mmap(PROT_NONE, guard+256KB)` → `mprotect(RW)` → `sigaltstack`。每线程**首次进入 wasm**（`lazy_per_thread_init`）触发，`.expect` panic → abort。
- **musl `pthread_create`**：线程栈创建也是 `mmap(PROT_NONE)` → `mprotect(RW)`，同一条被限路径，条件性失败 → 线程创建失败。

### 第三层：方案 B（预注册 altstack）的设计错误（本报告核心教训）

为绕过 wasmtime 的失败分配，方案 B 在 zcoder 侧**预注册 512KB altstack**（`mmap(RW)` 直接），让 wasmtime 检测到已有 ≥256KB altstack 就**复用**（返回 `None` 不持有）。三个错误：

1. **内存所有权错位**：wasmtime 复用后不持有这块内存（`STACK = None`），内存归 zcoder 管；而 wasmtime 的 `SA_ONSTACK` 信号处理器是**进程级常驻**的，内核的 sigaltstack 注册持续生效到线程完全退出。zcoder 在线程退出（Rust TLS 析构）时 `munmap`，**时机错位** → 信号处理器读已释放内存 → use-after-free。
2. **覆盖范围过度**：注册挂在 `OhosDispatcher::dispatch` 通用入口，**所有异步任务线程**（UI/LSP/git 等不跑 wasm 的）都被强制 `mmap` 512KB + `sigaltstack`，浪费内存且改变线程信号行为。
3. **泄漏**：`dispatch` 每任务 spawn 一线程，每线程 512KB 不释放 → 打开项目时 9074 线程 ≈ **4.6GB VMA** → 推高沙箱压力 → `pthread_create` 的 mprotect 也失败。

（补充：`AltStack::drop` 若只 `munmap` 不先 `sigaltstack(SS_DISABLE)`，正是 wasmtime 上游已知坑 [wasmtime#13857](https://jira.mongodb.org/si/jira.issueviews:issue-html/SERVER-131462/SERVER-131462.html)——悬垂注册，且 wasmtime 46 仍未修。）

## 解决方案（方案 A：patch wasmtime）

**核心洞察**：让 wasmtime **自己分配、自己释放** altstack——`lazy_per_thread_init` 只在该线程真正首次进入 wasm 时触发，天然精准（只有跑 wasm 的线程才分配，覆盖所有入口且不碰 zcoder 业务代码）；`Stack::drop` 在线程退出时释放，时机正确。

改 `patches/wasmtime/src/runtime/vm/sys/unix/signals.rs`（通过 zcoder 已有 `[patch.crates-io]` 机制挂载，`#[cfg(target_env="ohos")]` 不影响其他平台）：

1. **`allocate_sigaltstack` 分配**：OHOS 上 `mmap_anonymous(READ|WRITE)` 直接（跳过被拒的 `PROT_NONE→RW` mprotect）：
```rust
#[cfg(target_env = "ohos")]
let prot_flags = rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE;
#[cfg(not(target_env = "ohos"))]
let prot_flags = rustix::mm::ProtFlags::empty();
let ptr = mmap_anonymous(null_mut(), alloc_size, prot_flags, MapFlags::PRIVATE)
    .expect("failed to allocate memory for sigaltstack");
// OHOS: 区域已 RW，跳过下方 mprotect（非 OHOS 保持原 mprotect）
```

2. **`Stack::drop` 释放**：先 `sigaltstack(SS_DISABLE)` 再 `munmap`（wasmtime#13857 修复，防悬垂注册）：
```rust
impl Drop for Stack {
    fn drop(&mut self) {
        unsafe {
            let disabled = libc::stack_t {
                ss_sp: ptr::null_mut(),
                ss_flags: libc::SS_DISABLE,
                ss_size: 0,
            };
            libc::sigaltstack(&disabled, ptr::null_mut());
            let r = rustix::mm::munmap(self.mmap_ptr, self.mmap_size);
            debug_assert!(r.is_ok(), "munmap failed during thread shutdown");
        }
    }
}
```

3. 回退 zcoder 侧方案 B 的全部预注册代码（`dispatcher.rs` / `platform.rs` / `gpui_ohos Cargo.toml` 的 libc 依赖）。

**方案对比（为何选 A）**：wasmtime 在 zcoder 有多个使用面（tree-sitter wasm grammar、wasm 扩展宿主 `extension_host`、cli 工具），方案 B-精准需**枚举所有 wasm 入口**逐个打补丁（侵入大、未来新入口易漏）；方案 A 一处 patch 由 wasmtime 自行判断"谁在跑 wasm"，天然精准、覆盖所有入口、释放时机正确。代价是 patch 第三方（zcoder 已有 `patches/` 机制，且改动全被 `cfg(target_env="ohos")` 隔离）。

## 修改文件

- `patches/wasmtime/src/runtime/vm/sys/unix/signals.rs` — OHOS 上 `allocate_sigaltstack` 用 `mmap(READ|WRITE)` 直接（跳过被拒的 `PROT_NONE→RW` mprotect）；`Stack::drop` 先 `sigaltstack(SS_DISABLE)` 再 `munmap`（wasmtime#13857 修复）；全部 `#[cfg(target_env="ohos")]` 包裹。
- `Cargo.toml` — `[patch.crates-io]` 加 `wasmtime = { path = "patches/wasmtime" }`（含注释说明改动与原因）。
- `patches/wasmtime/` — 从 crates.io 拷贝的 wasmtime 36.0.12 源码树（4.4MB，供 patch）。
- 回退（方案 B 移除）：`crates/gpui_ohos/src/ohos/dispatcher.rs`（删除 `register_ohos_sigaltstack`，`dispatch`/`spawn_realtime`/`dispatch_after` 恢复原状）、`crates/gpui_ohos/src/ohos/platform.rs`（删除主线程调用）、`crates/gpui_ohos/Cargo.toml`（移除 libc 依赖）。

---
*OHOS 移植专属问题。wasm 运行前置障碍（JIT 权限 + openat2 SIGSYS）见 `2026-08-26-ohos-wasm-runtime-permission-and-openat2-crash.md`。关联 [[ohos-debug-lessons]]。*
