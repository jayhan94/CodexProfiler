# CodexProfiler

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Windows x64](https://img.shields.io/badge/platform-Windows_x64-yellow.svg)](https://github.com/jayhan94/CodexProfiler/releases)

看清 Codex 的每一步：模型请求、工具调用、token 速率、耗时和 API 费用估算。
监控在本机进行，无需 API Key，不额外调用模型。

## 五分钟上手

支持 **Windows 10/11 x64**，无需管理员权限。请先安装并启动最新版 Codex。

### 1. 安装

打开 PowerShell，粘贴运行：

```powershell
irm https://github.com/jayhan94/CodexProfiler/releases/latest/download/install.ps1 | iex
```

程序会自动安装、配置监控 hooks 并启动，开始菜单中也会出现 CodexProfiler。
也可从 [下载页面](https://github.com/jayhan94/CodexProfiler/releases/latest) 获取安装脚本和离线包。

### 2. 启用采集

在 Codex **设置 → Hooks** 中审阅并信任 CodexProfiler；CLI 用户输入 `/hooks`。
如果提示重新加载，请重新打开聊天。

### 3. 开始查看

继续一个 Codex 聊天，回到 CodexProfiler，点击左侧会话即可查看实时记录。
无需添加 API Key 或更改模型设置。

## 怎么用

- **会话分析**：查看当前步骤、任务耗时、输出 tokens、最近请求速率和费用估算。
- **运行时间线**：按工具、模型或压缩筛选；输入关键词搜索操作和完整命令，点击记录展开明细。
- **子会话**：展开父会话下的分组，分别查看子智能体和侧边聊天；各自统计，父会话费用不含子会话。
- **进程资源**：查看 Codex 及其工具子进程的 CPU、内存总量与明细；这是所有会话共享的本机用量。

界面支持上下滚动。监控只加载近期记录，不是全部聊天历史的归档。

## 如何理解数字

- **最近请求速率**：一次模型请求的输出 tokens ÷ 请求耗时，包含网络和排队。
- **任务整体速率**：本轮输出 tokens ÷ 总耗时，包含工具执行和等待。
- **API 费用估算**：按官方 API 价格区分普通输入、缓存、输出及请求档位；**不是订阅账单**。

价格日期和费用明细可在界面查看。缺少计时或用量时会显示未知或部分估算。
CPU、内存反映本机应用和工具的占用，不代表云端模型的资源消耗。

## 升级与卸载

**升级**：重新运行上面的安装命令，已有配置和监控数据会保留。

**卸载**：在 PowerShell 中运行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\CodexProfiler\scripts\uninstall.ps1"
```

卸载会移除程序及其 hooks，保留监控数据，不影响其他 hooks。
自定义安装位置的用户请运行对应目录中的 `scripts\uninstall.ps1`。

## 常见问题

**没有记录，或速率显示未知？**

确认已在 Codex 中信任 hooks，然后继续一次新请求。较早的记录可能没有完整计时。

**使用了自定义 Codex 目录？**

下载安装脚本后，指定你的 Codex 目录：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 `
  -CodexHome "D:\Codex Home"
```

**数据会上传吗？**

不会。记录保存在本机，可能包含会话标题、文件路径和工具命令，请勿公开分享原始记录。

遇到问题可在 [Issues](https://github.com/jayhan94/CodexProfiler/issues) 反馈。

## 许可

[Apache License 2.0](LICENSE)。随附字体使用 [SIL Open Font License](assets/fonts/OFL.txt)。
