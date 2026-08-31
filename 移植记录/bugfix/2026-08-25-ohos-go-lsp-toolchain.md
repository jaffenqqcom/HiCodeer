# Go LSP（gopls）无法安装与运行（VM 缺 Go 工具链与网络）

## 问题描述 (Problem Description)

zcoder 打开 `.go` 文件时，Go LSP（gopls）无法启动。zcoder 报错 `Could not install the Go language server 'gopls', because 'go' was not found`。原因是 VM（OpenEuler）上既没有 `go` 命令（gopls 需 `go install` 安装），也没有 gopls 二进制。进一步，即使补装 go，默认的 `go install gopls@latest` 还会因版本不兼容与网络问题再次失败。

## 问题表现 (Symptoms)

- zcoder 界面报：`Language server gopls: Could not install the Go language server 'gopls', because 'go' was not found.`
- 设备日志：`Failed to start language server "gopls": Could not install the Go language server 'gopls', because 'go' was not found.`
- `which go` / `which gopls` 在 VM 上均无结果。
- VM 存在 `java`（OpenJDK 17）但无 `javac`，仅 JRE。

## 问题原因 (Root Cause)

### 缺陷 1：VM 缺 Go 工具链

`crates/languages/src/go.rs` 的 `GoLspAdapter`：
- `fetch_latest_server_version` 先 `delegate.which("go")`，找不到直接 `bail!("Could not install the Go language server 'gopls', because 'go' was not found.")`。
- `fetch_server_binary` 用 `go install golang.org/x/tools/gopls@latest` 安装 gopls。

VM 从未安装 golang（`dnf` 包未装），`go` 不在 PATH，gopls 无从安装。

### 缺陷 2：`gopls@latest` 要求过新 Go 版本

补装 go 1.21.4 后，`go install gopls@latest`（v0.23.0）报 `requires go >= 1.26.0`，Go 尝试自动下载 toolchain `go1.26.7`，但 `GOSUMDB=off` 时下载校验失败。

### 缺陷 3：proxy.golang.org 不可达

Go 模块默认从 `https://proxy.golang.org` 下载，VM 上 curl 探测超时不可达。

## 解决方案 (Solution)

### 步骤 1：VM 安装 Go 工具链

```bash
sudo dnf install -y golang        # go 1.21.4 aarch64
```

### 步骤 2：配置可用的 GOPROXY

`proxy.golang.org` 不可达，但 **goproxy.cn**（HTTP 200）可达：

```bash
go env -w GOPROXY=https://goproxy.cn,direct GOSUMDB=off
```

### 步骤 3：安装与 Go 1.21 兼容的 gopls 版本

`gopls@latest`（v0.23.0）需 go ≥ 1.26，改装兼容版本（查 `https://goproxy.cn/golang.org/x/tools/gopls/@v/v0.16.2.mod` 确认要求 go 1.19）：

```bash
go install golang.org/x/tools/gopls@v0.16.2
```

### 步骤 4：gopls 软链到 PATH

```bash
sudo ln -sf /home/user/go/bin/gopls /usr/local/bin/gopls
```

zcoder 的 `GoLspAdapter::check_if_user_installed` 用 `delegate.which("gopls")`，PATH 里找到即直接使用，不再走 `go install`。

**验证**：重启 zcoder 后日志 `found user-installed language server for gopls. path: "/usr/local/bin/gopls"`，gopls 进程持续存活（`-mode=stdio`），无错误。

## 修改文件 (Modified Files)

本次为 **VM 环境配置**，无 zcoder 代码修改：
- VM `dnf install golang`（go 1.21.4）
- `go env -w GOPROXY=https://goproxy.cn,direct GOSUMDB=off`
- `go install golang.org/x/tools/gopls@v0.16.2`
- `ln -sf /home/user/go/bin/gopls /usr/local/bin/gopls`

参考：[[ohos-debug-lessons]]
