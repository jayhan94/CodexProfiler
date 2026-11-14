# CodexProfiler

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Windows x64](https://img.shields.io/badge/platform-Windows_x64-yellow.svg)](https://github.com/jayhan94/CodexProfiler/releases)

轻量的本地 Codex 性能监控工具。按会话查看模型请求、工具调用、上下文压缩、token 速率及 API 等价成本，并查看 Codex 本机进程树的资源占用。

Rust + egui + SQLite；内置 JetBrains Mono。无需 API Key，不额外调用模型，监控数据保存在本机。

## 快速安装（Windows）

当前提供 **Windows 10/11 x64** 预编译发行包。安装不需要管理员权限、Python 或 Rust；需要 PowerShell 5.1 或更新版本。

先安装并启动 [Codex](https://learn.chatgpt.com/docs/)。完整请求计时需要 Codex 支持 `PreModelRequest` / `PostModelRequest` 等 hooks；本项目已验证的 Codex 版本为 `0.167.0-alpha.3.2`，不虚设未经验证的最低版本。旧版本如缺少相应事件，请先更新 Codex。

在 PowerShell 中运行：

```powershell
irm https://github.com/jayhan94/CodexProfiler/releases/latest/download/install.ps1 | iex
```

安装器会下载官方仓库的最新发行包，验证 SHA-256，将程序安装到当前用户目录，导入监控 hooks，并创建开始菜单快捷方式和启动程序。

> hook 的信任必须由你在 Codex **设置 → Hooks** 中完成（CLI：`/hooks`）。安装器不会写入信任记录或绕过此步骤。信任后继续一个真实聊天，即可开始采集请求起止和速率。若 Codex 提示需要重新加载配置，请重新打开聊天。

默认位置：

| 内容 | 路径 |
| --- | --- |
| 程序 | `%LOCALAPPDATA%\Programs\CodexProfiler` |
| 本地统计缓存 | `%LOCALAPPDATA%\CodexProfiler\monitor.sqlite` |
| Codex 主目录 | `$env:CODEX_HOME`，未设置时为 `%USERPROFILE%\.codex` |
| Hook 配置 | `<CODEX_HOME>\hooks.json` |
| Hook 事件与导入状态 | `<CODEX_HOME>\monitor` |
| 配置备份 | `<CODEX_HOME>\hooks.json.codex-profiler-*.bak` |

已存在的程序旁 `data/monitor.sqlite` 缓存继续使用；指定 `--db` 可以选择其他文件。安装器不修改系统 PATH、系统执行策略或 Codex 的模型设置。

### 先下载脚本再安装

如需先查看脚本或指定参数：

```powershell
Invoke-WebRequest https://github.com/jayhan94/CodexProfiler/releases/latest/download/install.ps1 -OutFile install.ps1
# Windows PowerShell 的进程级执行参数不会修改系统或当前用户的策略。
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -NoLaunch
```

固定版本、自定义路径和 Codex 主目录：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Version v0.2.1 -InstallDir "D:\Apps\CodexProfiler" -CodexHome "D:\Codex Home"
```

参数：

| 参数 | 用途 |
| --- | --- |
| `-Version latest` | 默认安装最新正式版；也可以指定 `v0.2.1` |
| `-InstallDir PATH` | 指定用户可写的安装目录 |
| `-CodexHome PATH` | 指定要监控和导入 hooks 的 Codex 主目录 |
| `-NoLaunch` | 安装后不启动窗口 |
| `-NoShortcut` | 不创建开始菜单快捷方式 |
| `-SkipHooks` | 只安装程序，暂不导入 hooks |
| `-PackagePath ZIP` | 从本地发行包离线安装 |
| `-ChecksumPath FILE` | 指定 `SHA256SUMS`；离线包默认读取相邻的同名文件 |

### 离线安装与手动校验

从 [Releases](https://github.com/jayhan94/CodexProfiler/releases) 下载 ZIP、`SHA256SUMS` 和 `install.ps1`，放在同一个目录：

```powershell
Get-FileHash .\CodexProfiler-v0.2.1-windows-x64.zip -Algorithm SHA256
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -PackagePath .\CodexProfiler-v0.2.1-windows-x64.zip -NoLaunch
```

校验失败会停止安装。安装器会拒绝覆盖一个没有安装清单的非空目录；请为首次安装选择新的空目录。当前发行包未进行 Windows 代码签名；下载来源和校验文件均以本仓库 Releases 为准。

## 导入、诊断与移除 hooks

推荐由安装器自动导入，也可以使用程序自带命令：

```powershell
$profiler = "$env:LOCALAPPDATA\Programs\CodexProfiler\codex-profiler.exe"
& $profiler --install-hooks | Out-Host
& $profiler --doctor | Out-Host
```

指定另一个 Codex 主目录：

```powershell
& $profiler --install-hooks --codex-home "D:\Codex Home" | Out-Host
```

发行包内也提供独立脚本：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\import-hooks.ps1 -ExecutablePath "D:\Apps\CodexProfiler\codex-profiler.exe" -CodexHome "D:\Codex Home"
```

导入规则：

- 合并到用户级 `hooks.json`，保留其他应用和用户自己的 hooks。
- 不修改 `config.toml`、项目级或插件中的 hooks。若曾在这些位置手动配置 CodexProfiler，请先移除重复定义，避免同一事件被记录两次。
- 只在配置实际变化时创建备份；重复导入相同定义不会重复添加，也不会无故改变信任状态。
- 升级时根据安装状态识别并更新自己管理的定义；被手动修改的定义会保留。
- 配置不是合法 JSON、出现并发编辑或无法安全写入时，报告错误并保留原文件。
- 程序负责注册定义；Codex 负责审阅信任。更改可执行文件路径或 hook 定义后，可能需要重新审阅。

`--doctor` 输出 JSON 诊断，检查版本、路径、hooks 注册及事件记录等。它不猜测 Codex 的信任结果；收到真实请求事件才说明采集链路已经工作。

只移除本程序管理的 hooks：

```powershell
& $profiler --uninstall-hooks | Out-Host
```

这不会删除聊天日志、监控数据、其他 hooks 或 Codex 的信任设置。

## 使用界面

### 会话分析

左侧按父会话组织。子会话默认折叠，展开后可分别选择：

- **子智能体**：Codex 从父会话启动的 agent。
- **侧边聊天**：带明确附属标记、由父会话发起的聊天。
- 普通 fork 不会仅因为继承了历史而被误标为侧边聊天。

启动时优先加载最近会话及可找到的祖先，最多保留 96 个会话状态；这不是所有历史聊天的全量归档。

父会话或其子会话运行时，该父组保持在活跃区域；同时活跃的组保持稳定相对顺序。无法找到父会话的记录显示在独立分组中。

右侧展示任务耗时、输出 tokens、最近模型请求速率、当前线程的 API 等价成本。运行时间线支持按模型、工具、压缩或全部筛选；点击记录展开命令参数、时间边界、档位和费用明细。主面板可整体上下滚动，长命令详情另有独立滚动区域。

### 进程资源

“进程资源”页显示本机 Codex 进程树及工具子进程，包括 Node.js、PowerShell、命令执行器等。

- CPU 百分比按整机逻辑处理器总容量归一化。
- 内存显示工作集及占整机物理内存比例；合计可能重复计算进程间的共享页。
- 根据实际父子关系识别工具进程，不把其他软件的 Node.js/PowerShell 按名称混入。
- Codex 共享沙箱服务独立标注；权限不足的指标显示未知或部分统计。
- 这是所有会话共享的本机资源，不伪造按聊天分摊的 CPU/内存；不包含 Profiler 窗口自身。
- 采样有固定间隔，短于采样周期、已经退出且从未被观察到的进程可能无法捕获。

这是本机应用和工具的资源占用，不能用来代表云端模型推理服务器的资源。关闭窗口后停止资源采样；已启用的 hooks 仍会记录事件，重新打开后可继续读取。

## 指标与费用口径

| 指标 | 计算与范围 |
| --- | --- |
| 最近模型请求速率 | 输出 tokens ÷ 请求开始到结束的耗时，包含网络和排队 |
| 任务整体速率 | 本轮输出 tokens ÷ 本轮总耗时，包含工具执行及等待 |
| 缓存命中率 | 已报告的缓存读取 tokens ÷ 输入 tokens |
| 会话 API 估算 | 当前线程所有已观测轮次的 canonical usage 之和；子会话单独查看 |
| 压缩阶段 | 根据 PreCompact/PostCompact 和实际子调用关联；无起点时不虚构耗时 |

费用采用 [官方 API 价格](https://developers.openai.com/api/docs/pricing) 的本地快照，分别计算普通输入、缓存读取、缓存写入和输出。缓存读取及写入是输入的子集，不会重复收费；输出中的推理 tokens 也不会重复相加。

当前价格快照日期见界面。覆盖 GPT-6.1 Sol、GPT-6.1 Terra、GPT-6 Sol、GPT-6 Luna 和 GPT-6 Astra；不支持的模型、缺失的用量和不完整的缓存数据标记为未知或部分估算。

- 每次请求固定开始时的档位，中途切换只影响后续请求。
- 采用 API 对应档位的价格，速度“1.5×”不等于费用“1.5×”。
- 根据模型官方规则处理长上下文；当前所列模型以单次输入超过 272K 为门槛，对整次请求加价。
- 缓存写入按 30 分钟保留估算，不包括独立工具调用费用和区域附加费。
- **这是 API 等价估算，不是 Codex/ChatGPT 订阅账单。**

没有 hooks 时仍可读取本机日志，但旧记录可能缺少请求起止、压缩边界或档位；这种数据保留未知，不用相邻事件的时间代替真实模型耗时。

## 升级与卸载

重复运行安装脚本即可升级。安装路径保持稳定；已有配置、统计数据和无关文件会保留。升级失败会尝试回退本次替换的应用文件。

升级自定义安装位置时继续传入同一 `-InstallDir`。现有安装记录的自定义 Codex 主目录会继续使用；切换主目录请先卸载旧关联，再重新安装。

卸载：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\CodexProfiler\scripts\uninstall.ps1"
```

卸载先移除自身管理的 hooks，再删除安装清单中属于本程序的文件及快捷方式。默认保留数据库、事件记录、备份和其他用户文件。数据位置见 `--doctor`。

## 源码运行与开发

预编译包当前只验证 Windows x64。Rust/egui 本身支持跨平台；macOS/Linux 的源码编译还需要对应的窗口、图形和字体依赖，目前没有提供已验证的二进制安装包。

Windows 源码构建需 Rust stable 与 C/C++ 编译环境（推荐 MSVC Build Tools）。

```powershell
git clone https://github.com/jayhan94/CodexProfiler.git
cd CodexProfiler
cargo run --release
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python -m unittest discover -s scripts/tests -v
```

安装脚本行为测试需要 Windows PowerShell 5.1；测试在临时目录运行，不修改真实 Codex 配置。

常用 CLI：

```text
codex-profiler --help
codex-profiler --version
codex-profiler --doctor
codex-profiler --print-hooks
codex-profiler --install-hooks
codex-profiler --uninstall-hooks
codex-profiler --resources-json
codex-profiler --codex-home PATH --db PATH
codex-profiler --headless-seconds 5 --codex-home PATH --db PATH
```

发布者推送与 Cargo.toml 一致的 `vMAJOR.MINOR.PATCH` tag 后，GitHub Actions 将执行检查、测试、构建和打包，再发布 ZIP、SHA256SUMS 与安装脚本。已发布版本的资产不会被流水线覆盖。

## 隐私与许可

监控在本机进行，不上传会话或资源数据。记录可能包含会话标题、路径和工具命令参数；请按本机敏感数据管理，不要把数据库、hook 事件或配置备份提交到仓库。

项目代码采用 [Apache License 2.0](LICENSE)。JetBrains Mono 保留其 [SIL Open Font License](assets/fonts/OFL.txt)。发行包同时附带这些许可文件。
