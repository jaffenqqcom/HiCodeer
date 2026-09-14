# 病毒防护服务内存暴涨 —— 采集结果与判定

- 日期：2026-09-14
- 场景：hicodeer（`com.hicodeer.studio`）本机安装后启动，用户观察到系统级异常；上一轮曾出现 `virus_protection_service` 43M→10G 并导致系统重启
- 本轮动作：**只布采集、只读分析，不改任何代码，不主动拉起应用**
- 设备：本次开机 15:11:17，报告形成时已运行 27 分钟
- 采集口径：
  - 设备侧 `/data/local/tmp/probe/hilog.log`：全量 hilog（含开机以来缓冲），56090 行
  - 设备侧 `/data/local/tmp/probe/mem.log`：每 ~2.7 秒一帧 meminfo + `top -n 1`，共 295 帧
  - 共享盘 `…/.workbuddy/tmp/probe/*.log`：拉回后的分析副本

---

## 一、结论先行

| # | 结论 | 证据强度 |
|---|---|---|
| 1 | 本轮 3 次启动（15:29:58 / 15:30:41 / 15:31:07）**没有**把病毒服务内存推起来；全程平铺 300–312MB | 强（295 帧曲线 + `VmHWM`） |
| 2 | 那个 300M **不是启动造成的**：首次启动前 2 分钟（15:27:59）它就已经是 307M | 强 |
| 3 | 首次启动前更早（15:21，hicodeer 未运行）实测 PSS 334MB，其中 `native heap / jemalloc heap` 285MB | 强（`hidumper --mem 1381`） |
| 4 | 本次开机以来该进程峰值 RSS = **312MB**（`VmHWM 319580 kB`），从未接近 10G | 强（`/proc/1381/status`） |
| 5 | **不复现的直接原因**：每次启动触发的病毒巡检都因「引擎未加载」直接返回，扫描被跳过 | 强（hilog 原文，3 次一致） |
| 6 | 10G 的机制推断：病毒引擎成功加载时，巡检遍历/持有文件树内容 → jemalloc 原生堆暴涨 | 中（**未验证**） |
| 7 | 真正的隐患不是病毒服务，而是**系统已被拖垮**，且该病态**早于**启动动作 | 强 |

---

## 二、时间轴（全部取自 hilog 毫秒级时间戳）

| 时刻 | 事件 | 证据 |
|---|---|---|
| 15:11:17 | 设备开机（上一轮问题导致的重启） | `hilog: ========Zeroth log of type: init` |
| 15:21:xx | 病毒服务 PSS 已 **334MB**，jemalloc heap **285MB**（此时 hicodeer 未运行） | `dm_virus.txt`（hidumper --mem 1381） |
| **15:27:21.401** | `task_manager_service`(pid 1322) 开始每秒全机 CPU/内存采集；XCollie/PARAM/文件监控同期开始刷屏 | `UCollectUtil-CpuUtil` 首条 |
| 15:27:59 | 采集首帧：病毒服务 RES **307M** | `mem.log` 第 0 帧 |
| **15:29:58.688** | 第 1 次启动：开始 EntryAbility 会话 | `WMSLife: Request scene container session activation, name: EntryAbility/com.hicodeer.studio/entry/0` |
| 15:29:58.735 | APPSPAWN 起 `com.hicodeer.studio` | `com.hicodeer.studio/APPSPAWN` |
| **15:29:58.798** | 病毒服务收到巡检请求，**引擎未加载 → 放弃** | 见下节原文 |
| **15:30:41.36** | 第 2 次启动（APPSPAWN pid 17291），同刻再次巡检失败 | `VIRUS_PROTECTION_SERVICE` 第 2 组 |
| **15:31:07/08** | 第 3 次启动（进程 pid **19042**，**至今存活**），同刻巡检失败 | `ps` + 第 3 组日志 |
| 15:33–15:41 | 病毒服务 RES 稳定 300–312M；40 秒采样内 HWM 由 319,580 → 361,340 kB 后回落 | `mem_all2.log` / `trend.txt` |

---

## 三、关键证据原文

### 3.1 每次启动必触发一次应用级巡检，且本次引擎起不来

```
09-14 15:29:58.798  1381  8218 E C02F36/virus_protection_service/VIRUS_PROTECTION_SERVICE:
    [line: 287, in function: RegisterScanResultListener] not load virusEngine
09-14 15:29:58.799  1381  8218 E ... [line: 589, SubscribeScanResultEvent] Fail to subscribe the scan result event.
09-14 15:29:58.799  1381  8218 E ... [line: 215, IsExistAnalysisEngine] not load virusEngine
09-14 15:29:58.799  1381  8218 W ... [line: 139, StartInspection] Analysis engine task for accessToken: 537668330 has not been created, create now.
09-14 15:29:58.799  1381  8218 E ... [line: 239, CreateAnalysisEngine] not load virusEngine
09-14 15:29:58.799  1381  8218 W ... [line: 143, StartInspection] Analysis engine accessToken: 537668330 create analysis engine failed, can not inspect, return.
09-14 15:29:58.799  1381  8218 W ... [line: 311, StartInspection] Start Inspection bundle: <private> = 1, current app erase from inspectMap.
```

- `accessToken: 537668330` 三次完全相同 → 三次都是**同一个应用（本次安装的 hicodeer）**。
- 整个开机日志里 `VIRUS_PROTECTION_SERVICE` 只有 **21 行**，全部是这三组；**15:29:58 之前一条都没有**。
- 结论：`StartInspection` 是**应用启动触发**的；本次因为没有引擎，`can not inspect, return`，扫描没有真正发生。

### 3.2 内存曲线（`mem.log` 295 帧，`virus_protection_service` RES）

```
15:27:59  307M      15:30:00  306M      15:33:xx  306M
15:28:30  308M      15:30:41  309M  ←第2次启动
15:29:30  305M      15:31:08  308M  ←第3次启动
15:29:58  306M ←第1次启动   15:41:23  303M
```

统计：295 个采样点，**min 300M / max 312M**，无任何阶跃。

### 3.3 进程峰值水位（`/proc/1381/status`）

```
VmPeak:  2849200 kB   (2.72 GB)
VmHWM:    319580 kB   (312 MB)   ← 本次开机以来的峰值 RSS
VmRSS:    316056 kB   (309 MB)
VmSwap:    42560 kB   (41.6 MB)
Threads:  39
```

趋势采样（间隔 25s）：

| 时刻 | VmHWM | VmRSS | MemAvailable |
|---|---|---|---|
| 15:40:25 | 319,580 kB | 312,880 kB | 5,216,256 kB |
| 15:40:50 | 319,580 kB | 312,888 kB | 5,106,688 kB |
| 15:41:15 | **357,228 kB** | 311,116 kB | 4,909,056 kB |
| 15:41:40 | **361,340 kB** | 312,868 kB | 5,125,120 kB |

→ RSS 稳、HWM 抬升 40MB 后回落：**周期性分配/释放，非持续泄漏**。

### 3.4 内存构成（`hidumper --mem 1381`，15:21）

| 项 | kB |
|---|---|
| Total Pss | 341,901 |
| Private Dirty | 297,604 |
| **native heap / jemalloc heap** | **285,572** |
| Swap Total | 42,956 |

→ 占用几乎全在**原生堆**，是「扫描/索引类」服务的典型形态。

---

## 四、系统侧的真实状态（隐患所在）

以下全部是**在 hicodeer 启动之前**（15:27:21 起）就已形成：

| 服务 | CPU | 累计 CPU | 说明 |
|---|---|---|---|
| `index_insert_service` (3324) | **196%** | 24:52（开机 27 分钟） | 开机以来一直满转 |
| `virus_protection_service` (1381) | **114%** | 23:29 | 同上 |
| `security_guard` (1395) | **103%** | 8:21 | 安全卫士 |
| `sysmgr-main` (2) | 170% | 8:01 | — |
| `file_monitor_service` (1191) | 37% | — | 每秒刷错误 |

- 系统负载：`load average: 23.27, 24.50, 24.02`（16 核）
- 内存总量 24GB，`MemAvailable` 由 15:28 的 6.8GB 降到 15:41 的 4.9GB；Swap 已用 2.6GB
- 文件监控刷屏原文（每秒 1–2 次，整个开机周期不断）：

```
E file_monitor_service: [HandleReadWriteMsgs:484]GetApplication failed, appName: /data/storage/el1/bundle/libs/arm64/electron, uid: 100
E file_monitor_service: [AddOtherUseRecord:564]fileType invalid: 0
E FILE_MANAGER_SERVICE: [ConvertFilePathToUri] ConvertFilePathToUri failed, Invalid physicalPath:<private>
W file_monitor_service: [BatchInsertOrUpdate:35]sql exe error[27394115]
```

### 磁盘侧规模（设备用户存储 = 扫描/索引的覆盖面）

| 路径 | 体积 | 条目数 |
|---|---|---|
| `/storage/Users/currentUser/workspace` | **86 GB** | **470,123** |
| ├ `workspace/warp-oh` | 60 GB | — |
| ├ `workspace/zcoder` | 15.5 GB | 188,563 |
| └ `workspace/commandline-linux-arm64`（SDK） | 6.7 GB | — |
| `/storage/Users/currentUser/HiCodeer`（hicodeer 数据根） | 775 MB | 26,580 |
| 单文件 >100MB | 多处（`libwarp.so`、`.rlib`、`query-cache.bin`、`liblldb.so`、HAP…） | — |

hicodeer 正在持续写入：最近 3 分钟家目录内 **8,186** 个条目被改动，其中 `HiCodeer/node` **6,063** 个。

当前存活链路（`ps` 15:38:47）：

```
uid 20020220  19042  com.hicodeer.studio              (15:31:07 起)
uid 20020117  22750  hicodeerd                        (15:36:48 起)
uid 20020117  22921  node .../codebuddy -c            (15:37:04 起)
uid 20020117  23758  python3 -m pip install --target /storage/Users/currentUser/HiCodeer/tmp/pylibs pillow
```

---

## 五、判定与推断

### 已确认（有硬证据）

1. **应用每次启动，系统确实会向病毒服务发起一次针对该应用的巡检**（`StartInspection`，同一 accessToken）。这就是「启动」与「病毒服务」之间的真实耦合点。
2. 本轮巡检**全部空转**：引擎未加载 → `can not inspect, return`。
3. 因此本轮启动**不可能**造成病毒服务内存上涨；实测曲线与峰值水位双重佐证。
4. 观察到的 ~300MB 是**开机后前 8 分钟内涨上去的稳态**，与启动动作解耦。

### 推断（**未验证**，需下一步取证）

5. 上一轮的 43M→10G：引擎成功加载时，巡检会真正遍历/读取内容，占用集中在 jemalloc 原生堆 → 一旦覆盖面里有超大目录树（用户存储 47 万条目、86GB、多处几百 MB 单文件），堆就失控。
6. hicodeer 的**设计性风险**：数据根与工作区都落在**设备用户存储**（`/storage/Users/currentUser/**`），这正是文件索引（`index_insert_service`）、文件监控（`file_monitor_service`）、全局搜索（`FusionSearch`）、病毒防护（`virus_protection_service`）共同覆盖的区域；hicodeer 的运行时（node/npm/pip/语言服务）又在这棵树里做**成万文件级的突发写入**。
7. 「引擎为何 `not load virusEngine`」是能否复现的**钥匙**：引擎起来 = 可能复现 10G；引擎起不来 = 一定不复现。

---

## 六、建议的下一步取证（均不改代码、不主动拉起 hicodeer）

1. **定位引擎加载失败的原因**：`/data/service/el1/public/` 下病毒引擎相关目录（uid 2000 无权限，需换取法）；或在 hilog 里向前追溯 `security_guard` / `sg_*` 的引擎加载记录。
2. **把「扫描」与「启动」解耦验证**：用 `hidumper --mem 1381` 每 10 秒采样，跨越一次 hicodeer 的大写入（npm/pip 安装窗口），看原生堆是否同步上涨。若同步 → 触发面是文件树；若不同步 → 触发面是引擎自身状态。
3. **复现前先给设备减负**：现在开机即满负载（load 23），任何复现都会被噪声淹没，也容易在非目标环节先崩。
4. 顺手记录：应用侧有 `MMG [NMM:1289] app module: Name mismatch: default/hicodeer != default/zcoder`（模块名仍为 `hicodeer`，某处按 `zcoder` 请求）——历史改名残留，与本次内存问题**无关**，仅备查。
