# OHOS 启动时读不到用户设置，导致 qemu_enabled=false 不生效

## 问题描述

在设置界面把 QEMU 关闭（`qemu_enabled: false`），重启应用后 QEMU 仍然被启动。
用户设置文件 `<数据根>/config/settings.json` 里明确写的是 `"qemu_enabled": false`，
但启动流程完全没有读到这个文件。

启动期的命令后端选择发生在 `SettingsStore` 初始化之前，因此
`crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs` 自己实现了一个
"去找 settings.json" 的逻辑，而这个逻辑与真实的数据根已经脱节。

## 问题表现

- 设置里关闭 QEMU → 重启 → QEMU 依旧启动，走 QEMU guest 后端
- 设备 hilog（`qemu-boot` 标签，直连 hilog 的 boot trace）每次都打印：

```
qemu-boot: start_command_backend: base_path=/data/storage/el2/base/haps/entry/files
qemu-boot: read_launch_qemu_settings: no settings.json; defaults (enabled=true)
qemu-boot: start_command_backend: settings enabled=true cores=4 mem=8G disk=128G
qemu-boot: provision_guest_files OK; starting qemu
qemu-boot: QEMU guest backend ACTIVE
```

- 设备上确认沙箱内根本没有 settings.json：

```
$ hdc shell "find <沙箱> -name settings.json | wc -l"
0
```

- 用户实际修改的文件在别处：

```
/storage/Users/currentUser/HiCodeer/config/settings.json   ← 内容 qemu_enabled: false
```

复现条件：只要数据根是用户选择的 home 目录（不是沙箱默认目录），**必然发生**，与 QEMU 是否可用无关。

## 问题原因

### 数据根已经搬走，但扫描根没跟着改

应用的数据根由 `paths::set_custom_data_dir` 指定（`crates/zed/src/main.rs`），
OHOS 上取自用户在设置页选择的 home 目录，记录在 `<base_path>/custom_data_dir`：

```
$ hdc shell "cat <沙箱>/hap/entry/files/custom_data_dir"
/storage/Users/currentUser/HiCodeer
```

于是：

- 配置目录 = `CUSTOM_DATA_DIR/config`（`crates/paths/src/paths.rs`）
- 用户设置文件 = `/storage/Users/currentUser/HiCodeer/config/settings.json`

而启动期读设置的 `find_settings_json` 只扫描沙箱：

```rust
// 扫描根：base_path.ancestors().nth(3) = /data/storage/el2/base
let root = base_path.ancestors().nth(3).unwrap_or(base_path).to_owned();
```

`/storage/Users/currentUser/HiCodeer` 与 `/data/app/el2/100/base/<bundle>` 是两个
互不包含的挂载点，扫描**永远不可能**命中。

### 连锁后果

扫不到文件 → `read_launch_qemu_settings` 直接返回默认值 → 而默认值是"开启 QEMU"：

```rust
impl Default for LaunchQemuSettings {
    fn default() -> Self {
        Self { enabled: true, cores: 4, mem_gb: 8, disk_gb: 128 }
    }
}
```

### 为什么以前是好的

以前数据根落在沙箱内（`<base_path>/zcoder`），扫描能命中；改用"用户选择的 home
目录"作为数据根后，扫描逻辑没有同步修改，于是静默失效 —— 没有任何报错，
只是"设置不生效"。

## 解决方案

### 关键点

数据根的位置**已经由 `custom_data_dir` 这个记录文件明确写下来了**，不需要扫描，
直接读它即可。ArkTS 侧（`Setup.ets`）与 Rust 侧（`main.rs`）都写这个文件，内容就是 home 目录。

### 修改（保持单函数，不新增接口）

把 home 目录解析直接放进 `find_settings_json` 的开头，扫描退化为兜底：

```rust
fn find_settings_json(base_path: &Path) -> Option<PathBuf> {
    let record = base_path.join(HOME_DIRECTORY_RECORD_FILE);
    match std::fs::read_to_string(&record) {
        Ok(home) if !home.trim().is_empty() => {
            let file = Path::new(home.trim())
                .join(HOME_CONFIG_SUBDIR)      // "config"
                .join(SETTINGS_FILE_NAME);     // "settings.json"
            if file.is_file() {
                return Some(file);
            }
        }
        Ok(_) => {}
        Err(err) => {
            log::warn!("qemu_runtime: read home record {} failed: {err}; ...", record.display());
        }
    }

    // 原有沙箱扫描保持不变，仅作为"没有记录 home 目录"时的兜底
    const MAX_DEPTH: usize = 5;
    ...
}
```

### 验证

重新编译打包安装后，hilog 显示：

```
qemu-boot: find_settings_json: home=/storage/Users/currentUser/HiCodeer candidate=/storage/Users/currentUser/HiCodeer/config/settings.json
qemu-boot: read_launch_qemu_settings: found /storage/Users/currentUser/HiCodeer/config/settings.json
qemu-boot: read_launch_qemu_settings: enabled=false cores=4 mem=8G disk=128G
qemu-boot: start_command_backend: settings enabled=false
qemu-boot: register_ohos_backend entered      ← 不再启动 QEMU，回落 OHOS 后端
```

### 走过的弯路（供后来者避免）

- 一开始误以为是"沙箱内还有一个旧的 settings.json 被优先命中"，为此在设备上
  反复 `find` 确认沙箱内 settings.json 数量为 **0**，才转向"扫描根不对"
- 曾把修复实现成**新增一个 `find_launch_settings_json` 函数**，被指出"路径变了而已，
  不该新增接口"；正确的做法是直接修 `find_settings_json` 本身的解析逻辑 ——
  全局设置文件的路径解析只应该有一处
- 尝试过把 home 目录作为参数从 `launch_app` 传下来，但 `read_launch_qemu_settings`
  还有一个调用点是 QEMU 重启钩子（只有 `base_path`），读记录文件对两处都适用，故弃用

## 修改文件

- `crates/gpui_ohos/depend/launch-zed/src/qemu_runtime.rs` — `find_settings_json` 改为先按
  `<base_path>/custom_data_dir` 记录的 home 目录解析 `<home>/config/settings.json`，
  沙箱扫描降级为兜底；新增 `HOME_DIRECTORY_RECORD_FILE` / `HOME_CONFIG_SUBDIR` /
  `SETTINGS_FILE_NAME` 三个具名常量；该文件无"已存在的魔数/硬编码路径"改动

[[ohos-debug-lessons]]
