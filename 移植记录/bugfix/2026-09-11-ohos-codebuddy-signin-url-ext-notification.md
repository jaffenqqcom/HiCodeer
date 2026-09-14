# OHOS 上 codebuddy 没有登录界面：私有通知 `_codebuddy.ai/authUrl` 被 SDK 丢弃

## 问题描述

codebuddy 未登录时需要在浏览器完成登录，但 zcoder 界面上**不出现任何东西**——既没有
卡片，也没有可点的链接，用户无从下手。

ACP 本身已有把 URL 交给用户的标准方式（`elicitation/create` 的 URL 模式），zcoder 这条
链路是通的；问题在于 codebuddy 不走标准，而是把登录地址塞进一个厂商私有通知里，而 ACP
客户端 SDK 对**无法路由的通知静默丢弃**。

## 问题表现

- 触发登录后界面无变化，无报错、无提示。
- 抓 ACP 报文能看到 agent 发出的通知 `_codebuddy.ai/authUrl`，参数里带完整 URL；
  但客户端没有任何 handler 认领，SDK 直接吞掉。
- 与另一个表象相关：agent 自身调用 `openUrl` 也返回失败（`openSuccess=false`），
  所以"等它自己打开浏览器"这条路也不通。

## 问题原因

`agent_client_protocol` 只把通知派发给已注册的 handler，未注册的 method 不报错也不保留。
zcoder 注册的都是**标准**方法（`elicitation/create`、`elicitation/complete` 等），
没有厂商命名空间的兜底，于是这条 URL 在协议层就消失了。

值得注意的是：**这不是"缺一个功能"，而是"标准流程已具备、缺一层归一"**。标准 URL
elicitation 在 zcoder 里全链路都在位：

- 客户端能力声明 URL elicitation；
- 接收 `handle_create_elicitation`，URL 模式走 `ElicitationUrlMode`；
- 存储前校验（`crates/acp_thread/src/acp_thread.rs:443-455`：必须 `http`/`https` 且有 host）；
- UI 卡片 `render_url_elicitation`（`crates/agent_ui/src/conversation_view/elicitation.rs:1876`，
  含国际化域名钓鱼警告）；
- 出口调用 `cx.open_url`。

所以正确的做法是**不重造标准侧**，只加一条"私有通知 → 标准 URL 动作"的薄通道。

## 解决方案

### 关键点

把差异做成**数据**：一张路由表，一行 = 一个厂商 method + URL 字段名 + 卡片文案。
支持新厂商时只改表，不加代码路径。

### 修改

**`crates/agent_servers/src/acp.rs`**（`#[cfg(target_env = "ohos")]`，其他平台零影响）

- `ExtUrlRoute` 结构 + `EXT_URL_ROUTES` 表（`:4746-4761`）：

  ```rust
  const EXT_URL_ROUTES: &[ExtUrlRoute] = &[ExtUrlRoute {
      method: "_codebuddy.ai/authUrl",
      url_key: "authUrl",
      message: "Sign in to continue. Your browser will open the agent's sign-in page.",
  }];
  ```

- `handle_ext_notification`（`:4779`）：只处理表内 method，其余原样返回；参数里取不到
  字符串 URL 时打 `warn!`；命中则把 URL 归一为标准动作，并 `log::info!` 记一行
  （`:4804`）——设备上没有浏览器时，可以从日志取链接在另一台机器上完成登录。
- `request_url_elicitation`（`:4815`）：构造 `CreateElicitationRequest` +
  `ElicitationUrlMode` 后注入 `ctx.request_elicitations`。请求 id 用固定常量
  `EXT_URL_REQUEST_ID = "ext-notification"`（`:4666`），elicitation id 用
  `ext-url-<uuid>` 保证唯一；HTTP(S) 之外的 URL 由 `ElicitationStore` 自身的校验拒绝
  （`:4833` 打 warn），不在这里重复判断。
- 注册位置是关键（`:765`）：这个 catch-all 必须在**最后**注册，保证所有类型化 handler
  优先匹配，只有无人认领的通知才落到它这里。

**`crates/agent_ui/src/conversation_view.rs`**（`:2022-2030`）

卡片是客户端本地注入的，agent 永远不会为它发 `elicitation/complete`。视图在认证完成后
本来就不再渲染未认证态，这里再补一刀把 backing store 里的临时卡片清掉：

```rust
#[cfg(target_env = "ohos")]
this.cancel_request_elicitations(cx);
```

（`cancel_request_elicitations` 定义在 `conversation_view.rs:2661`。）

### 验证

- `./script/bundle-ohos` 全量编译通过，装机成功。
- 设备上实际出现登录卡片并成功点开的端到端验证**尚未做**，属未验证项。

### 走过的弯路（供后来者避免）

- 一度打算照着标准 `elicitation/create` 的接收路径再写一份厂商专用实现，被否：
  标准侧已经完整，重复实现只会多出一条要维护的分支。
- 也考虑过在 SDK 层做全局兜底，最终收敛到"表驱动 + 最后注册的 catch-all"，因为
  catch-all 的注册顺序本身就能保证不影响既有类型化 handler。

## 修改文件

- `crates/agent_servers/src/acp.rs` — 新增 `ExtUrlRoute` / `EXT_URL_ROUTES` /
  `EXT_URL_REQUEST_ID` / `handle_ext_notification` / `request_url_elicitation`，
  并在通知链末尾注册 catch-all
- `crates/agent_ui/src/conversation_view.rs` — 认证完成后取消本地注入的 URL 卡片

## 关联

- 排查手册：`移植记录/zed问题定位手段/Zed提供的问题定位手段.md` §10（ACP 扩展通道）
- 同一 agent 的另一处问题：[[2026-09-14-ohos-node-npm-toolchain.md]] §5
  （登录之外，发消息必失败的真因；原 userinfo 篇已并入该篇）

[[ohos-debug-lessons]]
