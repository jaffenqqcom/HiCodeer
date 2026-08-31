<div align="center">

<img src="AppScope/resources/base/media/app_icon.png" width="96" height="96" alt="OHcode">

# OHcode

**让 HarmonyOS PC 拥有更得心应手的代码编辑体验**

[![HarmonyOS](https://img.shields.io/badge/HarmonyOS-6.1.0-blue?logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIyNCIgaGVpZ2h0PSIyNCIgdmlld0JveD0iMCAwIDI0IDI0Ij48cGF0aCBmaWxsPSJ3aGl0ZSIgZD0iTTEyIDJMNiA3djEwbDYgNWw2LTVWN0wxMiAyem0wIDIuNEwxNiA4djhsLTQgMy4zTDggMTZWOGwwLTMuNnoiLz48L3N2Zz4=)](https://developer.huawei.com/consumer/cn/harmonyos/)
[![ArkTS](https://img.shields.io/badge/ArkTS-ETS-orange)](https://developer.huawei.com/consumer/cn/arkts/)
[![C++17](https://img.shields.io/badge/C++-17-blue)](https://isocpp.org/)
[![Git LFS](https://img.shields.io/badge/Git%20LFS-3.6+-green)](https://git-lfs.github.com/)
[![License](https://img.shields.io/badge/License-BSD--3--Clause-informational)](./LICENSE)

_将 Electron / VS Code 移植到鸿蒙桌面设备——不仅仅是「能跑」，而是「好用」。_

</div>

---

## 📖 目录

- [✨ 项目愿景](#-项目愿景)
- [🏗️ 架构总览](#️-架构总览)
- [🧩 模块详解](#-模块详解)
  - [web_engine — 引擎核心](#web_engine--引擎核心)
  - [electron — 应用入口](#electron--应用入口)
  - [Embedded Linux VM (QEMU)](#embedded-linux-vm-qemu)
- [🚀 快速开始](#-快速开始)
- [🛠️ 开发指南](#️-开发指南)
  - [项目结构](#项目结构)
  - [添加新的 Adapter](#添加新的-adapter)
  - [QEMU 虚拟机配置](#qemu-虚拟机配置)
  - [HNP 打包与注入](#hnp-打包与注入)
  - [文件关联与打开方式](#文件关联与打开方式)
  - [代码风格与约定](#代码风格与约定)
- [📡 通信机制](#-通信机制)
- [🔐 权限说明](#-权限说明)
- [⚠️ 已知限制与安全注意](#️-已知限制与安全注意)
- [🗺️ 路线图](#️-路线图)
- [🤝 参与贡献](#-参与贡献)

---

## ✨ 项目愿景

OHcode 的目标是让 HarmonyOS 2in1 / 平板设备上的开发者**无需离开鸿蒙生态**，就能获得完整的 VS Code 编辑体验：

| 🎯 目标 | 💡 实现方式 |
|---------|-----------|
| **原生编辑体验** | Chromium/Electron 引擎直接在 HarmonyOS 上渲染，XComponent 承载原生窗口 |
| **完整 Linux 工具链** | 嵌入式 QEMU 虚拟机提供 bash、node、rg 等开发工具 |
| **鸿蒙系统整合** | 37+ Adapter 桥接 Electron API → HarmonyOS 系统 API |
| **文件管理器联动** | 「用 OHcode 打开」直接关联 28+ 文件类型 |
| **扩展生态兼容** | 基于 VS Code 1.85.2，兼容 Open VSX 扩展市场 |

---

## 🏗️ 架构总览

```mermaid
graph TB
    subgraph HS["HarmonyOS System"]
        subgraph APP["OHcode Application"]
            subgraph HAP["electron — HAP 入口模块"]
                EA["EntryAbility"]
                BA["BrowserAbility"]
                SA["StatelessAbility"]
                SBE["StatusBarEntryAbility"]
                BEA["BrowserEmbeddedAbility"]
                IDX["Index · 启动加载页"]
                QR["QemuRunner · prepareQemuImages"]
            end

            subgraph HAR["web_engine — HAR 引擎库"]
                subgraph AB["Adapter 层 · 37 个系统适配器"]
                    A1["窗口管理"]
                    A2["对话框 / 文件选择"]
                    A3["蓝牙 / BLE"]
                    A4["输入法 / 手势"]
                    A5["通知 / 状态栏"]
                    A6["… 更多"]
                end
                subgraph JB["JSBindings · 37 个绑定"]
                    JBM["JsBindingMethod · 注册中心"]
                end
                subgraph CMP["UI 组件"]
                    WW["WebWindow"]
                    WSW["WebSubWindow"]
                    WEW["WebEmbeddedWindow"]
                    MB["MessageBox"]
                end
                subgraph INF["基础设施"]
                    DI["InjectModule · IoC 容器"]
                    AM["AbilityManager"]
                    BS["BaseAdapter"]
                    JBU["JsBindingUtils"]
                end
            end

            subgraph NAT["Native Bridge"]
                LA["libadapter.so<br/>Chromium / Electron"]
                LE["libentry.so<br/>NAPI 模块"]
            end

            subgraph QEMU["QEMU Linux Guest"]
                QG["ARM64 Linux<br/>bash · node · rg"]
                V9["virtio-9p 文件共享"]
                VS["virtio-serial 命令桥"]
                VN["virtio-net 网络转发"]
            end
        end
    end

    EA -->|extends| WW
    BA -->|extends| WW
    EA -->|boots| QR
    QR -->|dlopen + thread| LE
    LE -->|启动| QG
    JB -->|bindFunction| LA
    AB -->|调用| JBU
    CMP -->|XComponent| LA
    LE -->|HostCmd| VS
    QG <-->|文件| V9
    QG <-->|网络| VN

    style HAP fill:#1a1a2e,stroke:#e94560,color:#eee
    style HAR fill:#16213e,stroke:#0f3460,color:#eee
    style NAT fill:#0f3460,stroke:#533483,color:#eee
    style QEMU fill:#1a1a2e,stroke:#e94560,color:#eee
```

**核心思路**：`web_engine` 封装 Chromium 渲染 + 37 个系统适配器，作为 HAR 供 `electron` 入口模块引用；`electron` 额外启动 QEMU 虚拟机为 VS Code 的终端/工具链提供 Linux 用户态。

---

## 🧩 模块详解

### web_engine — 引擎核心

`web_engine` 是可复用的 HAR 库，包含在 HarmonyOS 上运行 Chromium/Electron 所需的全部适配逻辑。它是一个**自包含的引擎层**——理论上任何鸿蒙应用都可以引用它来获得基于 Chromium 的窗口渲染能力。

#### 对外 API

`web_engine` 通过 `Index.ets` 导出以下公共接口，应用层只需关心这些类型：

| 导出项 | 类型 | 说明 |
|--------|------|------|
| `WebAbilityStage` | Class | Ability 生命周期管理，最早初始化入口——在此处引导 NativeContext 和全部 JS 绑定 |
| `WebAbility` | Class | 主窗口 Ability 基类，处理窗口创建、参数传递、主题切换、权限申请 |
| `WebEmbeddedAbility` | Class | 嵌入式 UI Extension Ability 基类，用于被其他应用嵌入展示 |
| `WebWindow` | Component | XComponent 原生渲染窗口，核心 UI 组件，承载 Chromium 渲染表面 |
| `WebSubWindow` | Component | 子窗口组件，用于弹出的辅助窗口 |
| `WebEmbeddedWindow` | Component | 嵌入式窗口组件，运行在 UIExtensionContentSession 中 |
| `WebWindowNode` | Component | NodeController 变体，适用于需要节点级管理的场景 |
| `ILoginInfo` / `ILogin` | Interface | 登录相关类型 |

#### Ability 层级体系

`web_engine` 定义了两条平行的 Ability 继承链，它们都实现 `WebProxy` 接口，使得 `AbilityManager` 可以统一管理：

```mermaid
classDiagram
    class WebProxy {
        <<interface>>
        +getWindowStage()
        +getWindow()
        +getWindowId()
        +getOriginWindowId()
        +closeWindow()
        +setWindowTitle()
        +createSubWindow()
    }

    class WebBaseAbility {
        +xcomponentId: string
        +startUri: string
        +nativeContext: NativeContext
        +hideTitleBar: boolean
        +useDarkMode: boolean
        +electronRelaunchArgs: string[]
        +initParameters(want)
        +getContentPath(): string
    }

    class WebBaseExtensionAbility {
        +hostModuleId: string
        +session: UIExtensionContentSession
        +onSessionCreate(want, session)
    }

    class WebAbility {
        +onCreate()
        +onWindowStageCreate()
        +onConfigurationUpdate()
    }

    class WebEmbeddedAbility {
        +onSessionCreate()
        +onSessionDestroy()
    }

    WebProxy <|.. WebBaseAbility : implements
    WebProxy <|.. WebBaseExtensionAbility : implements
    WebBaseAbility <|-- WebAbility
    WebBaseExtensionAbility <|-- WebEmbeddedAbility
    WebAbility <|-- EntryAbility
    WebAbility <|-- BrowserAbility
    WebAbility <|-- StatelessAbility
    WebEmbeddedAbility <|-- BrowserEmbeddedAbility
```

> 💡 **为什么两条链？** 鸿蒙有两种窗口形态：`UIAbility`（独立窗口）和 `EmbeddedUIExtensionAbility`（嵌入宿主应用窗口）。两条继承链确保无论是独立运行还是被嵌入，都能获得一致的 Chromium 渲染能力。

#### 适配器全景

37 个 Adapter 是 `web_engine` 的核心价值——它们将 Electron API 逐一映射到 HarmonyOS 原生能力：

```mermaid
mindmap
  root((37 Adapters))
    🖥️ 窗口管理
      AppWindowAdapter · 主窗口创建/管理
      SubWindowAdapter · 子窗口池 (max 20)
      PopupWindowAdapter · 弹出窗口
      SystemFloatingWindowAdapter · 悬浮置顶窗口
      RunningLockAdapter · 防休眠
    💬 对话与交互
      DialogAdapter · 消息框/文件选择/照片选择
      FilePickerAdapter · 文件打开/保存 (MIME 过滤)
      MultiInputAdapter · 手势路由 (鼠标/触控/手写笔)
      IMFAdapter · 输入法框架 (软键盘/光标/文本插入)
    📡 连接与通信
      BluetoothAdapter · 经典蓝牙状态
      BluetoothLowEnergyAdapter · BLE 扫描/GATT 读写
      DeviceAdapter · USB 枚举与热插拔
      NotificationAdapter · 系统通知
      StatusBarManager · 状态栏托盘图标
    🔐 安全与权限
      CertManagerAdapter · 客户端证书安装/枚举/删除
      PermissionManagerAdapter · 运行时权限 (定位/相机/麦克风)
      BrowserPolicyAdapter · MDM 企业策略
      DeviceUserAuthAdapter · 生物识别 (指纹/面部/PIN)
    🎨 显示与外观
      DisplayAdapter · 多显示器/分辨率
      NativeThemeAdapter · 深色/浅色主题
      CursorAdapter · 光标样式/自定义光标
      FontAdapter · 系统字体枚举
    📸 媒体与视觉
      MediaAdapter · 摄像头预览
      ScreenshotAdapter · 全屏截图
      ShapeDetectionAdapter · 人脸检测 + OCR 文字识别
      SpeechAdapter · TTS 语音合成
    ⚙️ 应用基础
      ElectronAppAdapter · 应用信息/重启/区域设置
      AppLifecycleAdapter · 引擎生命周期
      ProcessAdapter · 子进程启动
      ContextAdapter · 上下文/活跃窗口持有
      ContextPathAdapter · 资源目录/系统设置/下载路径
    📁 文件与拖放
      FileManagerAdapter · 在文件管理器中显示
      DragDropAdapter · 跨窗口拖放 (UDC)
      MimeTypeAdapter · MIME ↔ 扩展名转换
      PrintAdapter · 系统打印对话框
    🌍 国际化
      I18nAdapter · 语言/区域 + 时区变化监听
      AccessibilityAdapter · 无障碍 (stub)
```

#### Native Bridge 流程

Chromium 引擎运行在 `libadapter.so` 中，它通过 `NativeContext` 对象与 ArkTS 层双向通信：

```mermaid
sequenceDiagram
    participant C as Chromium / libadapter.so
    participant NC as NativeContext
    participant JB as JSBindings
    participant AD as Adapters
    participant HO as HarmonyOS APIs

    Note over NC: 引擎启动时由 WebAbilityStage 初始化

    rect rgb(30, 30, 60)
        Note over C,HO: 正向调用 — ArkTS → Chromium
        WW->>NC: runBrowser(args)
        WW->>NC: ExecuteCommand(kNewWindow, {url})
        NC->>C: 执行 Chromium 命令
    end

    rect rgb(60, 30, 30)
        Note over C,HO: 反向回调 — Chromium → ArkTS
        C->>NC: 需要打开对话框
        NC->>JB: Dialog_ShowMessageBox(params)
        JB->>AD: DialogAdapter.showMessageBox()
        AD->>HO: AlertDialog.show()
        HO-->>AD: 用户点击
        AD-->>C: 返回结果
    end

    rect rgb(30, 60, 30)
        Note over C,HO: 绑定注册 — 启动时一次性
        JB->>NC: JSBind.bindFunction("Dialog_ShowMessageBox", fn)
        NC->>C: 注册回调句柄
    end
```

**关键对象 `NativeContext`** 提供的核心方法：

| 方法 | 方向 | 说明 |
|------|------|------|
| `runBrowser(args)` | ArkTS → C++ | 启动 Chromium 主循环，传入命令行参数 |
| `ExecuteCommand(type, params)` | ArkTS → C++ | 发送命令（新建窗口、打开 URL、退出等） |
| `JSBind.bindFunction(name, fn)` | ArkTS → C++ | 注册回调，让 C++ 侧可以调用 ArkTS 函数 |
| `OnWindowInitSize(rect, drawableRect)` | ArkTS → C++ | 通知窗口初始尺寸 |
| `OnWindowEvent(event)` | ArkTS → C++ | 通知窗口状态变化（最小化/最大化/关闭等） |
| `OnPanEventCB / OnPinchEventCB` | ArkTS → C++ | 转发触控手势事件 |

#### 依赖注入

`web_engine` 使用 Inversify IoC 容器管理所有 Adapter 的生命周期。这套机制保证了：

- **单例保证**：每个 Adapter 在整个应用中只存在一个实例
- **按需加载**：未使用的 Adapter 不会初始化（通过 `import lazy` 和 `getOrCreate` 实现）
- **松耦合**：组件间通过接口依赖，不直接引用具体实现

```mermaid
flowchart LR
    subgraph DI["InjectModule · IoC 容器"]
        AM["AdapterModule<br/>核心 Adapter 预注册"]
        GO["getOrCreate<br/>按需自动绑定"]
    end

    subgraph Core["核心流程"]
        WA["WebAbility"]
        WW["WebWindow"]
    end

    subgraph Adapters["Adapter 实例"]
        DA["DialogAdapter"]
        DDA["DragDropAdapter"]
        MIA["MultiInputAdapter"]
        NTA["NativeThemeAdapter"]
        PMA["PermissionManagerAdapter"]
        CA["ContextAdapter"]
        CTA["ContextPathAdapter"]
        DSP["DisplayAdapter"]
        DOT["… 其余 29 个"]
    end

    WA -->|Inject.get| DI
    WW -->|Inject.get| DI
    AM --> DA & DDA & MIA & NTA & PMA & CA & CTA & DSP
    GO --> DOT
```

> 💡 **核心 vs 按需**：`AdapterModule` 预注册了 WebAbility/WebWindow 直接需要的 8 个 Adapter；其余 29 个通过 `Inject.getOrCreate()` 在首次被 JSBinding 使用时自动绑定。这样既保证了核心流程的确定性，又避免了不必要的初始化开销。

### electron — 应用入口

`electron` 是薄应用层，继承 `web_engine` 基类，添加 QEMU 启动逻辑、文件关联处理和品牌加载页。

#### Ability 清单

| Ability | 基类 | 类型 | 说明 |
|---------|------|------|------|
| `EntryAbility` | `WebAbility` | UIAbility | 主入口。`onCreate` 启动 QEMU + 处理「用 OHcode 打开」；`onNewWant` 处理后续文件打开请求 |
| `BrowserAbility` | `WebAbility` | UIAbility | 浏览器窗口，使用 `pages/WindowNode`（NodeController 变体） |
| `StatelessAbility` | `WebAbility` | UIAbility | 无状态窗口，禁用系统窗口位置自动保存，使用 `pages/Index` |
| `StatusBarEntryAbility` | `StatusBarViewExtensionAbility` | ExtensionAbility | 状态栏扩展，加载 `pages/StatusBarPage`，独立于浏览器窗口体系 |
| `BrowserEmbeddedAbility` | `WebEmbeddedAbility` | ExtensionAbility | 嵌入式 UI 扩展，使 OHcode 可被其他应用嵌入展示 |

#### 启动流程

从用户点击图标到 VS Code 工作台就绪，整个流程如下：

```mermaid
sequenceDiagram
    actor User
    participant OS as HarmonyOS
    participant AS as WebAbilityStage
    participant EA as EntryAbility
    participant QR as QemuRunner
    participant IDX as Index Page
    participant WW as WebWindow
    participant CH as Chromium
    participant VS as VS Code

    User->>OS: 点击 OHcode 图标
    OS->>AS: onCreate()
    AS->>AS: initNativeContext(kMainProcess)
    AS->>AS: JsBindingMethod.bind() — 注册全部回调

    OS->>EA: onCreate(want)
    EA->>EA: handleOpenWant() — 处理文件打开意图
    EA->>QR: startQemu(context)
    QR->>QR: prepareQemuImages() — 首次拷贝内核/rootfs
    QR->>QR: ensureWorkspaceDir()
    QR->>QR: native.startQemu() — dlopen + 启动线程

    OS->>EA: onWindowStageCreate()
    EA->>IDX: 加载 pages/Index

    IDX->>IDX: 显示流光渐变加载页
    IDX->>IDX: 清除残留的就绪标记
    IDX->>IDX: 开始轮询 ohcode-ready.flag (200ms)

    IDX->>WW: WebWindow 组件渲染
    WW->>WW: buildArgs() — 读取标记文件
    WW->>CH: runBrowser(args)
    CH->>CH: 初始化 V8 / 加载 VS Code

    CH->>VS: VS Code 启动
    VS->>VS: onStartupFinished
    VS->>VS: 写入 ohcode-ready.flag

    IDX->>IDX: 轮询到标记 → hideLoading()
    IDX->>IDX: 加载页淡出 (500ms)
    User->>User: 🎉 VS Code 工作台就绪
```

#### 加载页

`Index` 页面包含一个精心设计的启动加载页，确保用户在 VS Code 工作台就绪前看到品牌画面而非空白窗口：

- **视觉**：多层渐变 + blur 融合的流光溢彩效果，中央放置 OHcode logo + 品牌标语
- **就绪检测**：轮询 `ohcode-ready.flag` 标记文件（由 VS Code 扩展 `ohcode-splash` 在 `onStartupFinished` 时写入），200ms 间隔
- **兜底超时**：20 秒后强制隐藏，防止标记写入失败导致加载页常驻
- **淡出动画**：500ms `EaseInOut` 透明度过渡 + 轻微缩放

### Embedded Linux VM (QEMU)

VS Code 的终端、构建工具需要 Linux 用户态。OHcode 在进程内启动 QEMU 虚拟机——不是子进程，而是通过 `dlopen` 加载 `libqemu-system-aarch64.so` 在独立线程中运行。

#### 启动链路

```mermaid
flowchart TB
    EA["EntryAbility.onCreate()"]
    QR["QemuRunner.startQemu(context)"]

    subgraph Prep["准备阶段"]
        PQI["prepareQemuImages()<br/>拷贝 Image + rootfs.cpio.gz"]
        EWD["ensureWorkspaceDir()<br/>创建 /data/.../workspace"]
    end

    subgraph Native["Native 层 · libentry.so"]
        SQ["startQemu() — NAPI"]
        DLO["dlopen libqemu-system-aarch64.so"]
        HCB["StartHostCommandBridge()<br/>连接 Unix Socket"]
        QET["qemu_system_entry()<br/>独立线程运行"]
    end

    subgraph Guest["QEMU Guest · ARM64 Linux"]
        INIT["/sbin/init"]
        OHPTYD["ohptyd — PTY 守护进程"]
        MOUNT["mount -t 9p usershare /share"]
        TOOLS["bash · node · rg · …"]
    end

    EA --> QR
    QR --> PQI --> EWD --> SQ
    SQ --> DLO --> HCB --> QET
    QET --> INIT
    INIT --> OHPTYD & MOUNT & TOOLS

    style Prep fill:#1a3a1a,stroke:#4caf50,color:#eee
    style Native fill:#1a1a3a,stroke:#5c6bc0,color:#eee
    style Guest fill:#3a1a1a,stroke:#ef5350,color:#eee
```

**关键设计决策**：

| 决策 | 原因 |
|------|------|
| QEMU in-process（非子进程） | 避免鸿蒙应用启动外部进程的权限问题；`dlopen` 更可控 |
| rootfs 使用 `overwriteIfChanged` | 更新 rootfs 后无需卸载应用清数据，下次启动自动重新部署 |
| kernel 使用 `if-absent` 拷贝 | 内核极少变化，跳过重复拷贝加速启动 |
| 串口输出到文件 | 调试日志持久化，不会因终端断开丢失 |

#### Guest ⇄ Host 通信通道

```mermaid
graph LR
    subgraph Guest["QEMU Guest"]
        G9["/share<br/>virtio-9p 挂载点"]
        GVS["ohcode.hostcmd<br/>virtio-serial 端口"]
        GCON["ttyAMA0<br/>串口控制台"]
        GNET["7681 端口<br/>ohptyd 监听"]
    end

    subgraph Host["HarmonyOS Host"]
        H9["/storage/Users/currentUser<br/>用户工作目录"]
        HSK["hostcmd.sock<br/>Unix Socket"]
        HLOG["console.log<br/>日志文件"]
        HPT["127.0.0.1:39001<br/>终端连接"]
    end

    G9 <-->|"双向文件共享<br/>编辑即保存"| H9
    GVS -->|"Guest 发命令<br/>Host popen 执行"| HSK
    GCON -->|"内核日志<br/>启动诊断"| HLOG
    HPT -->|"hostfwd TCP 转发<br/>VS Code 终端"| GNET

    style Guest fill:#3a1a1a,stroke:#ef5350,color:#eee
    style Host fill:#1a1a3a,stroke:#5c6bc0,color:#eee
```

| 通道 | 方向 | 传输层 | 用途 |
|------|------|--------|------|
| **virtio-9p** | 双向 | 9P 文件协议 | `/storage/Users/currentUser` ↔ guest `/share`——用户编辑的文件实时同步 |
| **virtio-serial** `ohcode.hostcmd` | Guest → Host | Unix Socket | Guest 通过 `popen()` 执行宿主机命令（⚠️ 当前无白名单） |
| **串口控制台** | Guest → Host | 文件写入 | 内核日志输出到 `console.log`，用于启动诊断 |
| **TCP 39001 → 7681** | Host → Guest | TCP hostfwd | VS Code 终端通过此端口连接 guest 中的 `ohptyd` |

#### 原生模块 (`libentry.so`)

| 文件 | 角色 |
|------|------|
| `qemu_runner.cpp` | NAPI 模块 `entry`。加载 QEMU（`dlopen`），组装 argv，暴露 `startQemu()` / `isQemuStarted()`。持有 QEMU 的 `-machine` / `-device` 配置 |
| `host_command_bridge.cpp` | 通过 Unix Socket 接收 guest 命令，在宿主机执行 `popen()`，以 `__OHCODE_HOSTCMD_END__` 帧定界返回结果 |
| `host_command_bridge.h` | 声明 `StartHostCommandBridge(socketPath)` |
| `CMakeLists.txt` | 构建 `entry` 共享库（C++17，链接 `libace_napi`、`libhilog_ndk`、`dl`） |

---

## 🚀 快速开始

### 环境要求

| 依赖 | 版本要求 | 说明 |
|------|---------|------|
| **DevEco Studio** | 5.0+ | HarmonyOS 官方 IDE |
| **HarmonyOS SDK** | Compatible 5.0.3(15), Target 6.1.0(23) | 通过 DevEco SDK Manager 安装 |
| **Git LFS** | 3.6+ | 管理大型二进制文件 |
| **PowerShell** | 5.1+ | HNP 重新打包脚本 |
| **Java** | 8+ | `app_packing_tool.jar` 运行时 |

### 克隆仓库

```bash
# 安装 Git LFS（如果尚未安装）
git lfs install

# 克隆——LFS 会自动拉取大型文件
git clone https://github.com/HanversionOvO/OHcode.git
cd OHcode
```

### 环境变量

```bash
# 必须设置：HNP 重打包脚本依赖此变量定位 app_packing_tool.jar
export DEVECO_SDK_HOME="C:\Users\<you>\AppData\Local\Huawei\Sdk"
```

### Electron资源

在构建项目之前，需要前往[release](https://github.com/HanversionOvO/OHcode/releases)下载Electron资源，并将其解压到目录`<OHcode>/web_engine/src/main/resources/resfile`文件夹下，若不存在`resfile`文件夹，请手动创建后再解压到此处

### build-profile文件
将项目下的`build-profile.json5.template`重命名为`build-profile.json5`后，再在项目结构中对项目签名

### 构建与运行

> ⚠️ 本项目**必须使用 DevEco Studio 构建**，暂无 CLI-only 构建流程。
> 若需要使用Linux的CLI进行创建编译，请重写repack脚本

1. 用 DevEco Studio 打开项目根目录
2. 等待 Hvigor 同步完成（`oh_modules` 安装）
3. **Build → Build Hap(s)/APP(s)**
4. 连接鸿蒙 2in1 设备或启动模拟器
5. **Run → Run 'electron'**

构建流水线自动执行：

```mermaid
flowchart LR
    A["编译 ArkTS / C++"] --> B["PackageHap<br/>打包 HAP"]
    B --> C["RepackHapWithHnp<br/>注入 HNP 包"]
    C --> D["SignHap<br/>签名"]
    D --> E["安装到设备"]
```

### 预构建资源

项目依赖若干不在 Git 仓库中的预构建产物：

| 资源 | 路径 | 说明 |
|------|------|------|
| `libelectron.so` | `electron/libs/arm64-v8a/` | Chromium/Electron 主库（~200MB） |
| `libadapter.so` | 同上 | Electron→ArkTS 桥接库 |
| `libqemu-system-aarch64.so` | 同上 | QEMU 引擎（`dlopen` 加载） |
| `libffmpeg.so` / `libextractor.so` | 同上 | 媒体解码/解压 |
| Node `.node` addons | 同上 | 原生扩展 |

> 这些文件需手动放入 `electron/libs/arm64-v8a/` 目录，已在 `.gitignore` 中排除。

---

## 🛠️ 开发指南

### 项目结构

```
ohcode-r/
├── 📂 AppScope/                    # 应用级配置与资源
│   ├── app.json5                   #   bundleName、版本号、图标
│   └── resources/base/media/       #   应用图标（layered image）
│
├── 📂 electron/                    # 📦 入口模块 (HAP)
│   ├── hnp/arm64-v8a/             #   HNP 包（node · bash · rg · electron）
│   ├── libs/arm64-v8a/            #   预构建 .so（gitignore）
│   ├── src/main/
│   │   ├── cpp/                   #   🧠 C++ NAPI 模块 → libentry.so
│   │   │   ├── qemu_runner.cpp    #     QEMU 加载/启动
│   │   │   ├── host_command_bridge.cpp  #  Guest→Host 命令桥
│   │   │   └── CMakeLists.txt     #     C++17, links napi/hilog/dl
│   │   ├── ets/
│   │   │   ├── entryability/      #   Ability 类（5 个）
│   │   │   ├── extensionAbility/  #   ExtensionAbility（1 个）
│   │   │   ├── pages/             #   UI 页面（7 个）
│   │   │   ├── qemu/              #   QEMU 启动逻辑（3 个文件）
│   │   │   ├── ohpty/             #   PTY 子进程（WIP）
│   │   │   └── process/           #   Child Process
│   │   ├── resources/
│   │   │   ├── base/              #   颜色、字符串等
│   │   │   └── rawfile/qemu/      #   内核 Image + rootfs（LFS）
│   │   └── module.json5           #   模块配置（权限、Ability、HNP）
│   ├── build-profile.json5
│   └── hvigorfile.ts              #   🔧 自定义 Hvigor 插件（HNP 重打包）
│
├── 📂 web_engine/                  # 📚 引擎库 (HAR)
│   ├── Index.ets                   #   公共 API 导出（8 个符号）
│   ├── childProcess.ets            #   Child Process 导出
│   ├── src/main/
│   │   ├── ets/
│   │   │   ├── ability/            #   Ability 基类（3 个）
│   │   │   ├── adapter/            #   🌉 37 个系统适配器
│   │   │   ├── jsbindings/         #   📎 37 个 JS 绑定 + 注册中心
│   │   │   ├── components/         #   🖼️ UI 组件（5 个）
│   │   │   ├── common/             #   🔧 DI 容器、基础类（13 个）
│   │   │   ├── interface/          #   📐 核心类型定义（3 个）
│   │   │   └── utils/              #   🔨 工具函数（8 个）
│   │   └── resources/
│   │       └── resfile/            #   Chromium 运行时 + app.asar (LFS)
│   ├── build-profile.json5
│   └── hvigorfile.ts
│
├── 📂 scripts/                     # 构建脚本
│   └── repack-hap-with-hnp.ps1     #   HAP + HNP 重打包
│
├── 📂 docs/                        # 参考文档（gitignore）
├── build-profile.json5             #   应用级构建配置
├── hvigorfile.ts                   #   根 Hvigor 配置
├── .gitattributes                  #   Git LFS 跟踪规则
└── .gitignore
```

### 添加新的 Adapter

遵循现有的 **Adapter → Bind → Register** 三步模式：

#### 第 1 步：创建 Adapter

`web_engine/src/main/ets/adapter/XxxAdapter.ets`

```typescript
import { BaseAdapter } from '../common/BaseAdapter';
import { injectable } from 'inversify';
import LogUtil from '../utils/LogUtil';

const TAG = 'XxxAdapter';

@injectable()
export class XxxAdapter extends BaseAdapter {
  doSomething(param: string): void {
    LogUtil.info(TAG, `doSomething: ${param}`);
    // 调用 HarmonyOS 系统 API
  }
}
```

#### 第 2 步：创建 Bind

`web_engine/src/main/ets/jsbindings/XxxAdapterBind.ets`

```typescript
import JsBindingUtils from '../utils/JsBindingUtils';
import { XxxAdapter } from '../adapter/XxxAdapter';
import Inject from '../common/InjectModule';

export function bind(): void {
  const adapter = Inject.getOrCreate(XxxAdapter);

  JsBindingUtils.bindFunction('Xxx_DoSomething', (param: string) => {
    adapter.doSomething(param);
  });
}
```

#### 第 3 步：注册绑定

`web_engine/src/main/ets/jsbindings/JsBindingMethod.ets`

```typescript
// 在 bindAll() 中添加一行
import { bind as bindXxx } from './XxxAdapterBind';

export function bindAll(): void {
  // …existing binds…
  bindXxx();
}
```

#### 第 4 步（可选）：注册到 DI 容器

如果该 Adapter 会被 `WebAbility` 或 `WebWindow` 直接使用，在 `AdapterModule.ets` 中注册；否则会通过 `Inject.getOrCreate()` 自动绑定。

> 💡 **经验法则**：核心窗口流程需要的 → `AdapterModule`；按需加载的 → `getOrCreate()` 自动绑定。

#### 完整调用链

当 Chromium 需要调用新 Adapter 时，数据流如下：

```mermaid
flowchart LR
    C["Chromium<br/>libadapter.so"] -->|"调用已注册的<br/>JSBind 回调"| B["XxxAdapterBind<br/>提取参数"]
    B -->|"委托"| A["XxxAdapter<br/>调用 HarmonyOS API"]
    A -->|"返回结果"| C
```

### QEMU 虚拟机配置

QEMU 参数在 `electron/src/main/cpp/qemu_runner.cpp` 的 `RunQemuThread()` 中配置：

```cpp
std::vector<std::string> args = {
    "qemu-system-aarch64",
    "-M", "virt",              // 机器类型
    "-cpu", "cortex-a57",      // CPU 模型
    "-m", "512M",              // 内存大小
    // …更多参数…
};
```

**常用调整**：

| 需求 | 修改 |
|------|------|
| 增加 guest 内存 | 改 `"-m"` 后的值，如 `"1024M"` |
| 添加新 virtio 设备 | 在 `args` 末尾追加 `"-device"` 参数 |
| 更换内核/rootfs | 替换 `rawfile/qemu/` 下的 `Image` 和 `rootfs.cpio.gz` |
| 串口输出改为 TCP | 将 `"file:" + CONSOLE_LOG_PATH` 改为 `"tcp:127.0.0.1:39001,server,nowait"` |

> ⚠️ 修改 C++ 后需在 DevEco Studio 中 **Clean → Rebuild** 才能生效。

### HNP 打包与注入

HNP（HarmonyOS Native Package）是鸿蒙原生工具链的分发格式。OHcode 通过 HNP 分发 `node`、`bash`、`rg`、`electron` 等运行时。

**构建流程**：

```mermaid
sequenceDiagram
    participant HV as Hvigor 构建系统
    participant PH as PackageHap 任务
    participant RH as RepackHapWithHnp 插件
    participant PS as PowerShell 脚本
    participant JT as app_packing_tool.jar
    participant SH as SignHap 任务

    HV->>PH: 打包 HAP
    PH-->>RH: unsigned HAP 就绪
    RH->>PS: execFileSync(powershell)
    PS->>PS: 检查 hnp/ 目录
    PS->>PS: 检查 DEVECO_SDK_HOME
    PS->>JT: java -jar app_packing_tool.jar<br/>--hnp-path hnp/
    JT-->>PS: 重新打包的 HAP（含 HNP）
    PS-->>RH: 完成
    RH->>SH: 签名
```

**添加新的 HNP 包**：只需将 `.hnp` 文件放入 `electron/hnp/arm64-v8a/`，构建脚本会自动 glob 所有 `*.hnp` 并注入。无需修改任何代码。

**当前 HNP 包**：

| 包 | 大小 | 说明 |
|----|------|------|
| `node.hnp` | 52 MB | Node.js 运行时 |
| `electron.hnp` | 10 MB | Electron 主进程 |
| `bash.hnp` | 1.2 MB | Bash shell |
| `rg.hnp` | 1.8 MB | ripgrep 搜索工具 |

> 🔑 前提：`DEVECO_SDK_HOME` 环境变量必须正确指向 SDK 路径，否则脚本无法找到 `app_packing_tool.jar`。

### 文件关联与打开方式

OHcode 注册了 28+ 种文件类型的「用 OHcode 打开」关联（`module.json5` 中 `scheme: "file"` + `linkFeature: "FileOpen"`）。

**处理流程**：

```mermaid
sequenceDiagram
    actor FM as 文件管理器
    participant EA as EntryAbility
    participant MF as 标记文件
    participant WW as WebWindow
    participant CH as Chromium

    FM->>EA: onNewWant(want)<br/>want.uri = "file:///data/.../main.ts"
    EA->>EA: handleOpenWant()
    EA->>EA: FileUri(uri).path → 真实路径
    EA->>EA: statSync(path).isDirectory()?
    EA->>MF: 写入 "file|file:///data/.../main.ts"
    Note over MF: ohcode-open-target.txt

    Note over WW: 下次 WebWindow 加载时
    WW->>WW: buildArgs()
    WW->>MF: 读取标记
    MF-->>WW: "file|file:///data/.../main.ts"
    WW->>MF: 删除标记（一次性消费）
    WW->>CH: --file-uri=file:///data/.../main.ts
    Note over CH: VS Code 打开该文件
```

**注册的文件类型**（覆盖主流开发语言）：

| 类别 | UTD 类型 |
|------|---------|
| 通用文本 | `general.text`, `general.plain-text`, `general.log` |
| 源代码 | `general.source-code`, `general.script`, `general.c-source` … |
| Web | `general.java-script`, `general.type-script`, `general.css`, `general.html` |
| 配置 | `general.json`, `general.yaml`, `general.xml`, `general.conf` |
| 文档 | `general.markdown`, `general.diff` |
| 数据 | `general.comma-separated-values-text`, `general.tab-separated-values-text` |
| 学术 | `org.tug.tex`, `org.tug.bib`, `org.tug.cls` |

**支持扩展**：在 `module.json5` 的 `uris` 数组中添加新的 UTD 即可支持更多文件类型，无需修改代码。

### 代码风格与约定

| 约定 | 说明 |
|------|------|
| **Adapter 模式** | 每个 Adapter 是 `@injectable()` 单例，封装 HarmonyOS API；Bind 文件只做接线（提取参数 → 委托 Adapter）|
| **`import lazy`** | `AdapterModule.ets` 中使用 lazy import 延迟加载 Adapter 类，减少启动开销 |
| **`reflect-metadata`** | 在 `BaseAdapter` 中导入，为 Inversify 装饰器提供运行时元数据 |
| **版权头** | 所有源文件保留华为 BSD 风格版权声明 |
| **日志** | 使用 `LogUtil` + `TAG` 常量；`LogDecorator.ts` 提供 `@LogMethod` 自动日志装饰器 |
| **窗口 ID** | 字符串类型（`'browser1'` 等），由 `AbilityManager` + `ConfigData` 管理，数字 ID 自动归一化为 `browserN` 格式 |
| **标记文件** | 跨组件通信使用一次性标记文件——写入方创建，消费方读取后立即删除，保证不残留 |

---

## 📡 通信机制

OHcode 中有多层通信机制，理解它们对开发至关重要：

```mermaid
graph TB
    subgraph Layer1["Layer 1 · Chromium ↔ ArkTS"]
        CH["Chromium / libadapter.so"]
        JB["JSBindings<br/>(36 个回调注册)"]
        NC["NativeContext<br/>runBrowser / ExecuteCommand"]
    end

    subgraph Layer2["Layer 2 · ArkTS ↔ HarmonyOS"]
        AD["Adapters<br/>(37 个系统封装)"]
        HO["HarmonyOS APIs"]
    end

    subgraph Layer3["Layer 3 · ArkTS 组件间"]
        EA["EntryAbility"]
        WW["WebWindow"]
        IDX["Index Page"]
        MF["标记文件<br/>(ohcode-open-target.txt 等)"]
    end

    subgraph Layer4["Layer 4 · Guest ↔ Host"]
        QG["QEMU Guest"]
        V9["virtio-9p"]
        VS["virtio-serial"]
        VN["virtio-net"]
        HP["Host Process"]
    end

    CH <-->|"JSBind 回调<br/>+ ExecuteCommand"| NC
    JB -->|bindFunction| CH
    AD -->|"调用"| HO
    JB -->|"委托"| AD
    EA -->|"写入"| MF
    WW -->|"读取+删除"| MF
    IDX -->|"轮询"| MF

    QG <-->|"文件"| V9 <--> HP
    QG -->|"命令"| VS --> HP
    HP -->|"终端"| VN --> QG
```

**标记文件协议**（Layer 3 详情）：

| 标记文件 | 写入方 | 消费方 | 格式 | 生命周期 |
|---------|--------|--------|------|---------|
| `ohcode-open-target.txt` | `EntryAbility` | `WebWindow.buildArgs` | `kind\|file://path` | 写入 → 读取 → 删除（一次性） |
| `ohcode-remote-target.txt` | Remote SSH 扩展 | `WebWindow.buildArgs` | `vscode-remote://...` | 写入 → 读取 → 删除（一次性） |
| `ohcode-ready.flag` | `ohcode-splash` 扩展 | `Index` 页面 | （空文件） | 写入 → 轮询到 → 删除 |

---

## 🔐 权限说明

OHcode 需要以下系统权限（`module.json5` 中声明）：

| 权限 | 级别 | 用途 |
|------|------|------|
| `LOAD_INDEPENDENT_LIBRARY` | kernel | 加载 QEMU .so（非应用自带库） |
| `IGNORE_LIBRARY_VALIDATION` | kernel | 跳过 .so 校验（预构建库无鸿蒙签名） |
| `ALLOW_WRITABLE_CODE_MEMORY` | kernel | QEMU TCG JIT 生成可执行代码页 |
| `ALLOW_DEBUG` | kernel | 调试支持 |
| `CUSTOM_SANDBOX` | system | 自定义沙箱路径（QEMU 文件布局） |
| `READ_WRITE_USER_FILE` | system | 读写用户文件（编辑器核心能力） |
| `INTERNET` / `GET_NETWORK_INFO` | normal | 网络访问（扩展市场、Remote SSH） |
| `ACCESS_CERT_MANAGER` | system | 证书管理（HTTPS 客户端证书） |
| `RUNNING_LOCK` | normal | 防止系统休眠（长时间构建/运行） |
| `PRINT` | system | 打印功能 |
| `PREPARE_APP_TERMINATE` | system | 应用退出前清理 |
| `ACCESS_BIOMETRIC` | normal | 生物识别（WebAuthn 认证） |
| `FILE_ACCESS_PERSIST` | system | 持久文件访问 |
| `PRIVACY_WINDOW` | system | 隐私窗口（防止截屏泄露敏感内容） |
| `WINDOW_TOPMOST` | system | 窗口置顶（调试面板等） |

---

## ⚠️ 已知限制与安全注意

### 🟡 功能 WIP

- **OhPty**：`electron/src/main/ets/ohpty/OhPtyChild.ets` 已引入但未调用，`libentry.so:Main` 入口未实现。最终目标是原生 PTY 子进程通过 TCP 39001 提供 VS Code 终端。
- **文件夹打开**：HarmonyOS 文件管理器不支持「用 OHcode 打开文件夹」，需从 OHcode 内部打开。
- **扩展兼容**：VS Code 版本固定在 1.85.2，较新的扩展可能需要通过「Install Specific Version」安装旧版本。

### 🟢 设计约束

- **仅 2in1 / 平板**：`deviceTypes` 为 `2in1` + `tablet`，不支持手机。
- **QEMU in-process**：虚拟机运行在应用进程内，不是独立进程；崩溃会影响整个应用。
- **arm64-only**：预构建库和 QEMU 均为 aarch64 架构。

---

## 🗺️ 路线图

- [ ] 🔒 Host Command Bridge 命令白名单
- [ ] 🖥️ OhPty 原生 PTY 子进程实现
- [ ] 📦 Extension Host 兼容性改进
- [ ] 🎨 主题/外观适配优化
- [ ] 🔄 Remote SSH 稳定性增强
- [ ] 📱 自适应布局（平板竖屏适配）

---

## 🤝 参与贡献

欢迎贡献代码！请遵循以下流程：

1. **Fork** 本仓库
2. 创建功能分支：`git checkout -b feat/your-feature`
3. 遵循[代码风格与约定](#代码风格与约定)
4. 确保在 DevEco Studio 中构建通过
5. 提交 PR，描述清楚变更内容和动机

**Commit 规范**：`类型: 简短描述`

| 类型 | 用途 |
|------|------|
| `feat` | 新功能 |
| `fix` | 修复 Bug |
| `refactor` | 重构 |
| `docs` | 文档 |
| `test` | 测试 |
| `chore` | 构建/工具变更 |

---

<div align="center">

**OHcode** — 让 HarmonyOS PC 有更得心应手的编辑体验 ✨

</div>
