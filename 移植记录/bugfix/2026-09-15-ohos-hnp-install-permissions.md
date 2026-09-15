# OHOS HNP 装机可执行位丢失：打包侧被 hmdfs 吞掉 other 位

> 2026-09-15。根因不在 HNP 安装器，也不在脚本的 chmod 参数，而在**打包机 `/storage` 的
> hmdfs 挂载会改写 chmod 结果**：凡 owner 有 x 一律落成 0770，owner 无 x 一律落成 0660，
> other 位恒为 0。hnpcli 用 `stat()` 如实记录这个被改写过的权限，设备端安装器又只看
> other 执行位来决定解压后给 0755 还是 0744，于是本机打出的可执行文件装到设备上恒为 0744。

## 问题描述

终端直接执行命令守护进程，被 shell 拒绝：

```
localhost ~ % hicodeerd
zsh: permission denied: hicodeerd
```

目标文件是随 HAP 安装的 **public HNP**：

```
/data/service/hnp/bin/hicodeerd -> ../hicodeerd.org/hicodeerd_0.1.0/bin/hicodeerd
-rwxr--r-- 1 installs installs 4585344 hicodeerd        # 0744
```

属主是 `installs`，而终端 uid 为 `20020117`，落入 other 类；other 无 x 位，故 exec 被拒。
`chmod o+x` 救急也走不通（非属主且非 root，报 `Operation not permitted`）。

## 根因链（逐环可复现）

1. `script/bundle-ohos:437` 写了 `chmod 755 "$STAGE_DIR/hicodeerd/bin/hicodeerd"`，参数本身正确。
2. 但 staging 落在 `/storage/Users/currentUser/...`，该路径是 **hmdfs** 挂载。实测 chmod 结果：
   `700→760`、`750→770`、`755→770`、`770→770`、`775→770`、`777→770`、`555→770`、
   `644→660`、`664→660`、`666→660`、`400→660`。即 `mode = owner_x ? 0770 : 0660`，
   **other 位恒为 0，chmod 777 也不例外**。
3. 于是 staging 里 `bin/hicodeerd` 实际是 0770，`conf/mgmt_host_key` 实际是 0660
   （第 440 行本意 0600，同样被改写）。
4. `hnpcli pack` 用 `stat()` 把实际权限写进 zip 条目
   （`service/hnp/base/hnp_zip.c` 的 `ZipAddFile`：`fileInfo.external_fa = (buffer.st_mode & 0xFFFF) << 16`）。
   产出的 `hicodeerd.hnp` 内 `hicodeerd/bin/hicodeerd` = `100770`。
5. 对照正常包：`curl.hnp` 内 `curl/bin/curl` = `100755`（有 other 执行位），所以 curl 能跑而本包不能。
6. 设备端安装器解压时（同文件 `HnpUnZipForFile`）判据只有 other 执行位：

   ```c
   mode_t mode = (fileInfo.external_fa >> ZIP_EXTERNAL_FA_OFFSET) & 0xFFFF;
   /* 如果其他人有可执行权限，那么将解压后的权限设置成755，否则为744 */
   if ((mode & S_IXOTH) != 0) {
       chmod(filePath, S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH);   /* 0755 */
   } else {
       chmod(filePath, S_IRWXU | S_IRGRP | S_IROTH);                        /* 0744 */
   }
   ```

   0770 的 `S_IXOTH` 位为 0 → 走 else → **0744**，与设备实测逐位吻合。
7. 同一文件的目录分支硬编码 `mkdir(..., S_IRWXU | S_IRWXG | S_IROTH | S_IXOTH)` = 0775，
   与设备上 `drwxrwxr-x` 也逐位吻合。

> 第 6、7 环的源码取自本机 `startup_appspawn-OpenHarmony-v6.0.0.1-Release.tar.gz` 的
> `service/hnp/`；设备固件版本未核对，且设备实际安装根是 `/data/service/hnp/`、与源码里的
> `<root>/<uid>/hnppublic` 布局不同，此两环为**强推断**（数字全吻合）。

## 平台硬事实：hmdfs 上的 chmod

- 判据：`df <path>` 看挂载类型。`/storage/Users/currentUser` 是 hmdfs；`/data/...` 是 hmfs。
- hmdfs 只保留 owner 位：owner-x 决定 0770，否则 0660；group 位被改写，**other 位恒 0**。
- 推论：**在这块盘上无法产出 other 可执行或 other 可读的文件**。任何依赖 chmod 结果的下游
  工具（hnpcli、tar/zip 打包、签名）都会拿到被吞掉的权限位。
- 需要精确权限时，不要靠 chmod：改为在目标格式里显式写入（如 zip 条目的 `external_attr`），
  或先把文件放到 hmfs 上再操作。

## 修复

在 `script/bundle-ohos` 的 `hnpcli pack`（`:465`）之后、产包校验（`:523`）之前，新增一段
python（`:467-521`）就地改写 `hicodeerd.hnp` 内 **central directory** 条目的 `external_attr`：

- 判据用 **staged 源文件的 owner 执行位**——hmdfs 保留了 owner 位，故它仍可靠地表达
  `chmod 755` / `chmod 600` 的差异；owner 有 x 的条目把记录值改写为 `0o100755`。
- 其余条目（含本意 0600 的 `conf/mgmt_host_key`）**保持原样**，不擅自降级或提升。
- 只动 central directory 偏移 38 处的 4 字节；local header、条目数据与 CRC 全部不碰。
  minizip 的 `unzGetCurrentFileInfo` 正是从 central directory 读 `external_fa`。
- 时机安全：改的是 `.hnp` 自身，而 HAP 签名（`:838` 的 `sign-app`）在其后，签名覆盖的已是
  修正后的包。

实测该改动是**原地**改写：`hicodeerd.hnp` 长度 1690964 字节前后不变，条目数 7 不变，
CRC 校验全干净。

## 验证状态

已在打包机对**真实包的副本**验证（用从脚本里提取的同一段 python，非复制品）：

- `hicodeerd/bin/hicodeerd`：`100770` → **`100755`**
- `hicodeerd/conf/mgmt_host_key`、`conf/authorized_keys`、`hicodeerd/hnp.json`：保持 `100660` 未动
- 目录条目：保持 `0000` 未动（安装器对目录用硬编码 0775，不看包内）
- 缺 staged 源文件时以非零码带明确信息退出，不静默放过（已用错误路径实测）

**未验**：设备端装机后是否真的变成 0755。需重打包 + 装一次 HAP 才能确认。

## 未修项：`conf/mgmt_host_key` 装机后全局可读

安装器的 else 分支硬编码 `S_IRWXU | S_IRGRP | S_IROTH` = **0744**，**无视包内记录值**。
所以服务端管理私钥 `conf/mgmt_host_key` 无论包内写 0600 还是 0644，装到设备上都是 0744，
即**对所有本地用户可读**。

这是安装器自身行为，与本次包的权限位无关，本次不处理，仅记录。日后再议时的方向：
不要依赖 HNP 分发密钥的权限，改为守护进程启动时以 0600 自行落盘到数据根。

## 同类受影响位置（同一原因，本次未改）

脚本内其余 chmod 调用在 hmdfs 上同样不产生预期权限位：

- `script/bundle-ohos:440` — staging 的 `conf/mgmt_host_key` 本意 0600
- `script/bundle-ohos:452` — resfile 的 `hicodeerd-mgmt/mgmt-client-key` 本意 0600
- `script/bundle-ohos:562` — resfile 的 guest `hicodeerd` 本意 0755
- `script/bundle-ohos:622` — resfile 的 guest 密钥四件套本意 0600
- `script/bundle-ohos:810` — 注入 HAP 时用 `os.stat(payload).st_mode` 取 .hnp 条目权限位，
  同样读到被 hmdfs 改写后的值（`0660`）

这些条目都不需要设备上的执行位或保密位（resfile 内容最终打进 HAP，权限由 HAP 决定），
故未一并改动；如需精确权限位，按上面的「平台硬事实」处理。
