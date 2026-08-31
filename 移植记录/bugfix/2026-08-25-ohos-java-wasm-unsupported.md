# Java LSP 无法加载（OHOS 沙箱禁止 wasmtime JIT 可执行内存）

## 问题描述 (Problem Description)

zcoder 打开 `.java` 文件时，尝试通过 Zed 扩展系统加载 java 语言扩展（wasm 格式的扩展包），加载失败：`unable to make memory executable: Invalid argument (os error 22)`。java 扩展无法实例化，Java 语言没有语法高亮、没有 LSP。同类问题还影响基于 wasm 的 tree-sitter grammar（已有 `ohos-treesitter-wasm-crash` 记录）。

## 问题表现 (Symptoms)

- zcoder 日志：`Failed to load extension: java, Loading extension from ".../zed/extensions/installed/java": loading wasm extension: java: failed to compile wasm component: unable to make memory executable: failed to make memory executable: Invalid argument (os error 22)`。
- `.java` 文件被当作纯文本，无补全/诊断/跳转。
- Java 语言服务不可用，且不限于 java——所有 wasm 扩展在同一原因下都无法加载。

## 问题原因 (Root Cause)

zcoder 用 **wasmtime** 作为 wasm 运行时执行扩展（`crates/extension_host/src/wasm_host.rs`）。wasmtime 用 Cranelift JIT 把 wasm 编译成机器码，需要把编译产物写入**可执行内存**（内部执行 `mmap(PROT_READ|PROT_WRITE)` 后 `mprotect(PROT_EXEC)`）。

HarmonyOS NEXT 的应用沙箱**禁止应用创建可执行内存**（安全加固，防 ROP/注入），`mprotect(PROT_EXEC)` 返回 `EINVAL`（os error 22），wasmtime 报 `make memory executable` 失败。这是操作系统级限制，不是 wasmtime 配置能绕过的——系统 WebView 内置的 JIT 有系统级豁免，但应用自建 JIT（wasmtime/V8 独立嵌入）没有。

相关影响：
- **java 扩展**（wasm 格式）→ 无法加载。
- **tree-sitter wasm grammar** → 同样受限（已记录 `ohos-treesitter-wasm-crash`，降级方案已认可）。

## 解决方案 (Solution)

wasmtime JIT 在 OHOS 沙箱无法运行，wasm 扩展这条路走不通。Java 的出路是**改用不经 wasm 的独立 LSP——jdtls（Eclipse JDT Language Server）**：

- jdtls 是独立 Java 程序，**不依赖 wasm**，在 **VM** 上运行（通过 cmd-agent，不在设备沙箱内），不受设备 JIT 限制。
- VM 已具备 `java 17.0.15`（jdtls 要求 JDK 17+），但**缺 `javac`**（当前是 JRE），需 `dnf install java-17-openjdk-devel` 补完整 JDK 才能做编译诊断。
- zcoder 侧需功能开发：新增 java 语言注册（`crates/languages/src/java.rs` + tree-sitter-java grammar）、`JavaLspAdapter`（下载 jdtls → VM 启动），工作量较大，jdtls 启动重、内存占用高。

**当前结论**：Java 在 OHOS 上暂不支持，等待 jdtls 集成方案决策；不影响 cpp/rust/ets/go 等已调通的 LSP。

## 修改文件 (Modified Files)

本次为**调研结论**，无代码修改。相关代码位置：
- `crates/extension_host/src/wasm_host.rs` — `build_wasi_ctx`（`inherit_stdio`）、`wasm_engine`（wasmtime `Config`），wasm 扩展执行入口
- `crates/languages/src/lib.rs` — 语言与 LSP adapter 注册表（java 尚未注册）

参考：[[ohos-debug-lessons]]
