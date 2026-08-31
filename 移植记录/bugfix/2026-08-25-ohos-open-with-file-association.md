# OHOS 双击 / 右键“打开方式”用 zcoder 打开文件失败（skill 声明 + on_open_urls 通道修复）

## 问题描述

在系统文件管理器里双击文件（或用右键“打开方式”）让 zcoder 打开目标文件，移植版 zcoder 表现出两个阶段的失败：

1. **右键“选择其他应用”列表里没有 zcoder**——应用没有被系统索引为候选“打开方式”；
2. 即便通过“打开方式”选中 zcoder，**应用能正常启动，但目标文件始终没有打开**。

## 问题表现

- `hap/entry/src/main/module.json5` 在 `EntryAbility.skills` 里声明了 `ohos.want.action.viewData` + uris（`text/plain` + pathRegex），但文件管理器“选择其他应用”列表里看不到 zcoder。
- 右键“打开方式”→ 选择 zcoder：zcoder 冷启动成功（说明 skill 匹配与 want 拉起链路是通的），但文件未打开。
- hilog 显示 ArkTS 侧完整收到 want（`want.uri=file://docs/storage/Users/currentUser/workspace/zcoder/docs/README.md`、`collected=1`），`invokeNativeSync` 无异常抛出。

## 问题原因

### 根因一：skill 文件打开能力声明不符合官方规范 → 系统不索引为“打开方式”候选

官方规范（拉起文件处理类应用）对“文件打开能力”的 uris 声明有三个硬性要求：

```json5
"uris": [
  {
    "scheme": "file",              // 必填
    "type": "general.plain-text",  // 必填：UTD 类型（不是 MIME！）
    "linkFeature": "FileOpen"      // 必填且大小写敏感：文件打开功能标志
  }
]
```

而 zcoder 之前的声明犯了两个错：`type` 写成了 MIME `"text/plain"`（应写 UTD `"general.plain-text"`）；**完全缺失 `linkFeature: "FileOpen"`**——这是系统把应用列入“打开方式”候选的关键标志，缺失即不索引。

### 根因二：打开链路用 FileDrop 模拟（Entered+Submit），架构上不可靠 → 文件打不开

gpui_ohos 不依赖 zed crate，无法直接调用 `zed::open_paths`，最初把“打开文件”实现为在窗口上回放 `FileDropEvent::Entered + Submit` 模拟一次拖放（`OhosWindow::open_external_paths`）。但追到 GPUI 源码后发现该路径**架构上不可靠**：

- GPUI 的拖放打开依赖**鼠标悬停位置上的元素 drop 目标**（`on_drop` 注册在具体元素上，workspace crate 里没有兜底 drop 处理）；模拟 drop 落在 `(0,0)`，该处没有 drop 目标，事件被拖放状态机消费但不会触发打开。
- 冷启动时 `flush_pending_open_with` 在窗口刚创建时同步执行，workspace 尚未就绪，时序更差。

附带发现（排查成本点）：`hap/entry/src/main/cpp/CMakeLists.txt` 是**空文件**，`hvigor assembleHap` 只打包 prebuilt `libzcoder.so`、**不编译 Rust**。改 Rust 代码后若只跑 hvigor，设备上仍是旧 .so（诊断日志不生效），必须用 `script/bundle-ohos` 全量构建。

## 解决方案

### 修复一：module.json5 按官方规范声明（ArkTS 侧，重打包 HAP 即可）

`EntryAbility.skills` 新增文件打开 skill：

- `actions` 补 `ohos.want.action.sendData`（保留 `viewData`、`sendMultipleData`）；
- `uris` 用 **UTD 类型列表**（全部带 `linkFeature: "FileOpen"`）：`general.plain-text`、`general.text`、`general.source-code`、`general.markdown`、`general.json`、`general.xml`、`general.html`、`general.css`、`general.type-script`、`general.java-script`、`general.python-script`、`general.shell-script`、`general.comma-separated-values-text`；
- 保留 1 条 `pathRegex` 兜底（`.rs/.ets/.toml/.go/.yml/.json5` 等无预置 UTD 的扩展名），同样带 `linkFeature`。

利用 UTD 的**归属链匹配**（声明父类型即匹配所有子类型）：`general.text` 覆盖全部文本类、`general.source-code` 覆盖全部源码类，13 条 UTD 足以覆盖约 40 种扩展名。

### 修复二：Rust 侧改走 zed 正规打开通道 `on_open_urls`

不再用 FileDrop 模拟，改为把文件路径注入 gpui 的 `on_open_urls`（macOS “打开方式”同款正规通道）：

```
openwith 事件（ArkTS want.uris）
  → Rust plugin-openwith on_main_thread_event（诊断日志）
  → platform.rs 回调：path_from_uri 解析路径
  → file_url_from_path：路径 → file:// URL（百分号编码，
     zed 的 OpenRequest::parse 对非 file:// 前缀打 "unhandled url" 丢弃）
  → 调 gpui on_open_urls 回调（App::on_open_urls 注册）
  → zed：OpenListener → workspace::open_paths（main.rs 有持续消费循环）
```

关键改动（`crates/gpui_ohos/src/ohos/platform.rs`）：

- `OhosPlatform` 新增字段 `open_urls_callback: Rc<RefCell<Option<Box<dyn FnMut(Vec<String>)>>>>`（`new()`/`Clone` 同步初始化）；
- `Platform::on_open_urls`（原为空实现）→ 存回调 + 立即 `flush_pending_open_with_urls()`；
- `register_openwith_handler` 回调：解析路径 → 有回调则转 `file://` URL 投递；回调未注册（冷启动早期）则缓冲进 `pending_open_with`；
- 新增 `flush_pending_open_with_urls()`（on_open_urls 注册时冲刷缓冲）与 `file_url_from_path()`（路径 → `file://` URL，仅保留 `A-Za-z0-9/-_.~`，其余字节 `%XX` 编码）；
- `open_external_paths`（FileDrop fallback）保留但降级，加日志。

`plugin-openwith` 补充全链路诊断日志（事件到达 / decode / 回调是否注册），并补 `Cargo.toml` 的 `log = "0.4"` 依赖（此前缺失会编译失败）。

## 验证

- `module.json5` 通过 JSON 合法性校验，uris 字段全部在 schema 白名单内（`scheme/type/pathRegex/linkFeature`）。
- Rust 侧括号平衡静态检查通过；跨语言字符串（`ohos.openwith` / `open-with` / 类型名）此前已核对一致。
- 需真实构建验证：`./script/bundle-ohos`（**全量**，含 Rust）→ **先卸载旧版再安装**（让系统重新索引 skills）→ 文件管理器右键“打开方式”列表应出现 zcoder → 选中后目标文件应在 zcoder 中打开。
- 抓日志定位：`hilog | grep -E "open-with|on_open_urls|open-external"`，逐跳确认 `on_open_urls: callback registered` → `on_main_thread_event` → `decoded N` → `callback_invoked=true` → `delivering N url(s) through on_open_urls`。

## 修改文件

- `hap/entry/src/main/module.json5` — skills 重写：UTD 类型列表 + `linkFeature: "FileOpen"` + `sendData` action（仅需重打包 HAP）
- `crates/gpui_ohos/src/ohos/platform.rs` — `on_open_urls` 实现、openwith 回调改道 on_open_urls、`file_url_from_path`、pending flush
- `crates/gpui_ohos/src/ohos/window.rs` — `open_external_paths` 加日志（保留为 fallback）
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-openwith/src/lib.rs` — 全链路诊断日志
- `crates/gpui_ohos/depend/openharmony-ability/crates/plugin-openwith/Cargo.toml` — 补 `log = "0.4"`
- 前置改动（本问题的一部分）：`hap/entry/src/main/ets/bridge/OpenWithPlugin.ets`（新增）、`hap/entry/src/main/ets/entryability/EntryAbility.ets`（onCreate/onNewWant 接线）、`crates/gpui_ohos/Cargo.toml`（依赖）

参考：[[ohos-debug-lessons]]
