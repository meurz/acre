# Acre

用 Rust 和 GPUI 构建的 Windows 磁盘占用分析工具。通过目录树、扩展名汇总和交互式方块图找到大文件。

[English](README.md) · [下载](https://github.com/meurz/acre/releases) · [扫描核心](https://github.com/meurz/acre-core)

## 功能

- 管理员权限下直接读取 NTFS 文件表；普通权限下逐目录扫描。扫描过程显示进度并支持取消。
- 扫描整个磁盘或指定文件夹，按实际占用统计，硬链接只计算一次。
- 目录树与方块图联动选中，双击文件夹进入，按 Backspace 或鼠标后退键返回上一级。
- 在资源管理器中显示、复制路径、重新扫描，以及确认后删除。移入回收站和永久删除分别提供。
- 随时切换英文与简体中文，无需重启。首次启动跟随 Windows 界面语言，之后记住选择。

## 下载运行

从 [Releases](https://github.com/meurz/acre/releases) 下载
`acre-<版本>-windows-x86_64.zip`，解压后运行 `acre.exe`。压缩包包含程序和许可文件，无需安装，支持 Windows 10/11 x64。

管理员账号启动时可能看到 Windows 提权提示，以启用 NTFS 快速扫描。需要明确使用普通权限时运行：

```powershell
./acre.exe --no-elevate
```

发布页的 `SHA256SUMS` 提供压缩包和独立程序的校验值，可用以下命令核对下载文件：

```powershell
Get-FileHash ./acre-0.2.0-windows-x86_64.zip -Algorithm SHA256
```

语言选择保存在 `%APPDATA%\Acre\settings.json`。不支持的系统语言默认使用英文。Windows 文件对话框及系统错误详情使用操作系统的语言。

## 构建

安装 [Rust](https://rustup.rs/) 和 Visual Studio Build Tools 的“使用 C++ 的桌面开发”工作负载及 Windows SDK，在 Windows 终端中运行：

```powershell
git clone https://github.com/meurz/acre.git
cd acre
./build.ps1                 # 输出 dist/acre.exe
./build.ps1 -Test           # 数据层及本地化测试
./build.ps1 -Check          # 检查桌面程序
./build.ps1 -Bench          # 输出 dist/model-bench.exe
```

项目固定使用 Rust 1.98.1，从 `PATH` 查找 `cargo`，默认并行任务数为 2，构建缓存位于当前仓库的 `target/`。可通过 `CARGO_TARGET_DIR` 指定其他缓存目录，或用 `-Jobs` 调整并行数。

扫描核心是公开仓库，依赖固定到具体提交。如需使用本地核心源码：

```powershell
./build.ps1 -CorePath ../acre-core/crates/acre-core
```

本地覆盖可能改变 `Cargo.lock`，提交其他改动前请恢复锁文件。

架构与开发说明见 [英文 README](README.md)、[架构](docs/architecture.md) 和 [基准测试](docs/benchmarking.md)。

## 许可

Acre 原创代码可任选 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 许可。嵌入字体、GPUI 及其他第三方组件保留各自许可，详见 [第三方声明](THIRD_PARTY_NOTICES.md)。
