# zcoderd 自动重启 + 改名评估（动手前方案）

日期：2026-09-12
状态：**评估稿，未动任何代码**
范围：guest 侧与 OHOS 本机侧的 zcoderd 存活策略；`zcoderd` → `hicodeerd`；`zcoder` 显示名 → `HiCodeer`

---

## 0. 结论速览

| 事项 | 可行性 | 改动量 | 建议 |
|---|---|---|---|
| guest zcoderd 被杀后自动拉起 | 可行，机制简单 | `guest-init` 1~2 个文件 | **建议做** |
| OHOS 本机 zcoderd 自动拉起 | 受阻：应用无法 spawn public HNP | 需先改 HNP 类型或另找宿主 | 待主人定方向 |
| `zcoderd` → `hicodeerd` | 可行 | **40 文件 / 285 处**，其中 8 组硬契约 | 建议做，须一次构建+装机验证 |
| `zcoder` 显示名 → `HiCodeer` | 可行 | **2 行** | 建议做，零风险 |
| `bundleName` 改 `com.hicodeer.*` | 可行但**破坏数据** | 2 文件 + 装机 | **不建议**，除非主人明确要 |

---

## 1. guest zcoderd 自动重启

### 1.1 现状：启动后无人过问

- `guest-init/rcS:30-33` 遍历 `/usr/lib/qemu-init/S??*` 依次执行 `start`
- `guest-init/S30zcoderd:35` 以 `"$ZCODERD" &` 后台启动，此后**没有任何进程观察它的存活**
- `guest-init/init:15-17` 跑完 rcS 即 `while :; do sleep 3600; done` 空转（PID 1 只为"不退出"）

结论：zcoderd 一旦退出（OOM、panic、被 kill），**永久消失**，直到下次冷启动。

### 1.2 两套方案

**方案 A：S30zcoderd 内加看护子 shell（改动最局部）**

替换 `S30zcoderd:35` 的单行后台启动：

```sh
(
  # 让看护者比 zcoderd 更晚被 OOM 选中（zcoderd 是内存大户，本就更靠前）
  echo -1000 > /proc/self/oom_score_adj 2>/dev/null
  delay=1
  while [ ! -e /run/zcoderd.stopped ]; do
    "$ZCODERD"
    [ -e /run/zcoderd.stopped ] && break
    echo "[qemu-init] zcoderd exited, restart in ${delay}s"
    sleep "$delay"
    delay=$((delay * 2))
    [ "$delay" -gt 30 ] && delay=30
  done
) &
```

stop 分支前置哨兵，否则 `pkill` 之后立刻被拉起来：

```sh
stop) touch /run/zcoderd.stopped; pkill -f /qemu/bin/zcoderd 2>/dev/null ;;
```

- 退避 1→2→4→8→16→30s 封顶，避免配置错误时疯狂重启刷屏
- 改动：`S30zcoderd` 一个文件，约 +12 行、改 1 行
- 代价：多一个 busybox sh 常驻（RSS 极小）
- `/proc/self/oom_score_adj` 依赖 busybox `echo` 是内建（不 fork，`/proc/self` 才指向看护者本身）——**未验证**，故带 `2>/dev/null` 容错

**方案 B：init(PID 1) 亲自看护（最健壮）**

`init` 跑完 rcS 后不再空转，改为收养 zcoderd：

```sh
while :; do
  [ -e /run/zcoderd.stopped ] && break
  if [ -x "$ZCODERD" ]; then "$ZCODERD"; fi
  sleep 1
done
```

同时把 `S30zcoderd` 的 `start` 分支删空（保留 `stop`），避免 rcS 与 init 双重启动。

- 优点：无额外常驻进程；PID 1 极难被 OOM 选中；语义上最正统
- 缺点：改 2 个文件（`init` + `S30zcoderd`），分层被打破（启动脚本不再自足）
- 恢复延迟取决于 `sleep`，设 0.2~1s 均可

**推荐**：先做方案 A（局部、易验证）；若实测发现看护者也被 OOM 带走，再升级到方案 B。

### 1.3 边界：这是"止血"，不是"没有风险"

主人的原判断是"zcoderd 能自动重启，就没有 OOM 风险了"。**不成立**，准确表述如下：

| 项 | 自动重启之后 |
|---|---|
| 命令链路 | ✅ 能自愈。host 侧机制已完整，无需改：`pool.rs:54-60` 检出 host key 不符即拒绝并触发重新 bootstrap；`pool.rs:31-35` 有 5s `DOWN_COOLDOWN` + `poke` 立即重试；`bootstrap.rs:21` 周期 10s、`bootstrap.rs:50-58` 命令到来即刻 poke |
| fd 保护 | ⚠️ 停机期间不执行。15s 周期内停几秒，影响可忽略；停机久则 virtiofsd 的 O_PATH fd 继续累积 |
| **根因** | ❌ **完全不变**。若 `/dev/shm` 被写满，zcoderd 重启后**会再次被杀** → 变成"起了又杀、杀了又起"的拉锯 |
| 用户观感 | ⚠️ 由"彻底卡死"变成"偶发命令失败 / 终端卡顿"，但仍不稳定 |

连带副作用（既有行为，非本次引入）：zcoderd 被 SIGKILL 时，它已 spawn 的 shell/agent 不会被连带杀死（子进程各有自己的 process group，`exec.rs` 用 `process_group(0)`），会成为孤儿被 PID 1 收养。重启后可能出现"新旧两批进程并存"。

**结论**：自动重启是一张安全网，不是解药。真正消除只有两条路——限住 `/dev/shm`（主人已否决）或找出并限制吃内存的元凶。

### 1.4 OHOS 本机侧 zcoderd（另一回事，且更难）

现状比 guest 更差：**全仓 0 处代码启动它**，目前靠人工起。

- 应用具备本地 fork 能力：`crates/util/src/command/ohos.rs:497 spawn_local`（smol fork + exec）
- 但它只认 **private HNP**（`ohos.rs:66-67` 注释明确：只查私有安装目录，public 有意不作为回退）
- 而 `module.json5:163` 里 `zcoderd.hnp` 的 type 是 **public** ⇒ **应用拉不起它**

三条出路，需主人选：

1. 把 `zcoderd.hnp` 改 private → 应用可用 `spawn_local` 拉起并在应用内做健康探针（TCP 4022 探测）看护
2. 查 HNP 规范是否支持 `hnp.json` 的 `install` 段配自启（当前 `bundle-ohos:250` 是空 `{}`）
3. 保持人工启动

---

## 2. `zcoderd` → `hicodeerd`

### 2.1 规模

**285 处 / 40 个文件**。其中大量是注释与文档。

### 2.2 硬契约（必须双端原子改动，错一处即断链）

| # | 契约 | 一端 | 另一端 |
|---|---|---|---|
| 1 | Cargo 包名 + bin 名 | `zcoderd/Cargo.toml:2,14` | 构建产物路径 |
| 2 | guest 二进制名 | `bundle-ohos:307`（拷为 `qemu-guest/zcoderd`） | `qemu_runtime.rs:52 GUEST_BIN_FILE` ↔ `S30zcoderd:10` |
| 3 | HNP 包名 | `bundle-ohos:249-250` hnp.json `"name"` + `:267` 产物名 | `module.json5:163 "zcoderd.hnp"` |
| 4 | resfile 目录 | `bundle-ohos:254-257 / 340-348` | `qemu_runtime.rs:39 OHOS_KEY_SUBDIR` / `:41 GUEST_KEY_SUBDIR` |
| 5 | 协议保留字 ×3 | `zcoderd/src/protocol.rs:17,20,23` | `cmd-client/src/protocol.rs:17,20,23`（`zcoderd-bootstrap`、`__zcoderd_sid__`、`__zcoderd_signal__`；注释要求 **byte-for-byte 一致**） |
| 6 | 环境变量 ×4 | `zcoderd/src/main.rs:37,41,47` + `passwd_shim.rs:25` | `S30zcoderd:12-14`（注入侧） |
| 7 | guest 停止匹配串 | `S30zcoderd:47 pkill -f /qemu/bin/zcoderd` | — |
| 8 | 构建缓存目录名 | `bundle-ohos:216,229,237,298,334`（`${PROJECT_HASH}-zcoderd*`） | 无关功能，改后旧缓存自动重建 |

### 2.3 展示性（建议一并改，但不影响运行）

- hilog tag：`zcoderd/src/logger.rs:39 c"Zcoderd"`、`:73/:81/:125` 日志前缀
- 错误提示：`cmd-client/src/pool.rs:195,197`
- cargo feature 名：`launch-zed/Cargo.toml:17-18 zcoderd-agent`（改了要同步所有 `--features` 调用点，当前 `bundle-ohos:169` 提到它）
- 文档 31 处：`qemu-mngt/QEMU-HiSH-替换方案.md`

### 2.4 不该改

- `qemu-mngt (2)/`：替换前快照，历史存档
- `crates/gpui/**`：共享代码，禁改

### 2.5 风险

1. **协议常量改名是高危**：只改一端 → 连接能建立但 bootstrap 命令不被识别 → 客户端拿不到 `SshInfo` → 命令链路全断，且报错现象是"连上了但不回话"，不易定位
2. **HNP 名三处必须一致**（hnp.json / hnpcli 产物 / module.json5），漏一处装机后找不到 zcoderd
3. **guest 二进制名三处必须一致**（构建拷贝 / 运行时查找 / 启动脚本），漏一处 guest 里没有 zcoderd
4. 改名需**重新构建 + 重装**才生效；HNP 有缓存，建议 clean 后构建

---

## 3. `zcoder` 显示名 → `HiCodeer`

### 3.1 现状（重要）

`HiCodeer` 这个名字**已经在用**，但只用在用户主目录上：

- `hap/entry/src/main/ets/entryability/Setup.ets:15` `HOME_DIR_NAME = 'HiCodeer'`
- 同文件 `:36/:70/:106` 注释同样口径

即：用户选定的根目录下会创建 `<root>/HiCodeer` 作为工作主目录。应用显示名却仍是 `zcoder`。

### 3.2 分三档

**第一档 · 用户可见显示名（必改，零风险）**

| 位置 | 现值 → 目标 |
|---|---|
| `hap/AppScope/resources/base/element/string.json:5` | `app_name: "zcoder"` → `"HiCodeer"` |
| `hap/entry/src/main/resources/base/element/string.json:13` | `EntryAbility_label: "zcoder"` → `"HiCodeer"` |

- 全仓只有 `base` 一个语言目录（无 en_US / zh_CN 变体），改这两处即可
- 效果：桌面图标名、任务卡标题
- 不影响数据、不影响包名、不影响任何逻辑

**第二档 · bundleName（高风险，不建议）**

`hap/AppScope/app.json5:3` `com.zcoder.studio` → `com.hicodeer.studio`

- OHOS 按 bundleName 分配沙箱数据目录，改名等于**装了一个全新应用**：
  - 应用内数据（settings、`custom_data_dir` 记录、QEMU 金盘与工作盘、guest 配置）**全部不迁移**
  - 旧应用与新应用**并存**，旧数据仍占空间
  - 用户显式选定的外部 home 目录（`<root>/HiCodeer`）**不受影响**
- 配套需改：`install_run.sh:10`（force-stop）、`:33`（aa start）
- 建议：**先不动**。显示名改了，用户看到的就是 HiCodeer

**第三档 · 内部库名 `libzcoder.so`（可选，改动面大）**

约 8 处，且改错**应用直接起不来**（`EntryAbility.ets:1 import 'libzcoder.so'` 找不到模块）：

`hap/entry/src/main/cpp/CMakeLists.txt:6`（project 名）、`hap/oh-package.json5:5`、`hap/oh-package-lock.json5:9-15`、`hap/entry/oh-package.json5:17`、`hap/entry/oh-package-lock.json5:17-92`、`hap/entry/src/main/cpp/types/libzcoder/`（目录名 + `oh-package.json5:2` + `Index.d.ts`）、`EntryAbility.ets:1,17`、`native_ability/oh-package.json5:16`、`bundle-ohos` 的拷贝目标

- 用户完全看不到这个名字
- 建议：**不在本轮**，或单独立项

---

## 4. 待主人决策

- [ ] guest 侧自动重启：选 **方案 A**（局部）还是 **方案 B**（PID 1 亲自看护）
- [ ] 是否接受"自动重启 ≠ 消除风险"这一事实（1.3），仍继续做
- [ ] OHOS 本机侧 zcoderd 是否要自启；若要做，选 1/2/3 哪条路
- [ ] `zcoderd` → `hicodeerd`：是否含"展示性"（日志、feature 名、文档）一起改
- [ ] 软件名：只改显示名（第一档），还是连 `libzcoder.so`（第三档）一起
- [ ] `bundleName` 是否保持 `com.zcoder.studio`（建议保持）

## 5. 改动文件清单（评估版）

**自动重启（方案 A）**：`crates/gpui_ohos/depend/qemu-mngt/guest-init/S30zcoderd`
**自动重启（方案 B）**：上述 + `guest-init/init`

**`zcoderd` → `hicodeerd`（硬契约）**：
`cmd-agent/zcoderd/Cargo.toml`、`cmd-agent/zcoderd/src/main.rs`、`cmd-agent/zcoderd/src/protocol.rs`、
`cmd-agent/cmd-agent/.../cmd-client/src/protocol.rs`、`launch-zed/src/qemu_runtime.rs`、
`script/bundle-ohos`、`hap/entry/src/main/module.json5`、`guest-init/S30zcoderd`

**显示名**：`hap/AppScope/resources/base/element/string.json`、`hap/entry/src/main/resources/base/element/string.json`
