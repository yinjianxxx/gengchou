[English](README.md) | **简体中文**

<!-- 修改用户可见行为、安装方式、隐私说明或发布状态时，请同步更新 README.md。 -->

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![CI](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yinjianxxx/ai-usage-monitor)](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

# AI Usage Monitor

> **侧重稳定性的独立社区分支。** 本项目基于
> [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)
> v1.4.8（提交 `9b29972`）。本分支使用独立的可执行文件名、互斥体、窗口类、
> 设置目录、日志目录和 GitHub Release 更新通道。

## 界面截图

### 详情弹窗

![AI Usage Monitor 详情弹窗](.github/screenshot.png)

### 任务栏组件和服务商托盘图标

| 任务栏组件 | 服务商托盘图标 |
| --- | --- |
| ![嵌入 Windows 任务栏的 AI Usage Monitor 组件](.github/taskbar-widget.png) | <img src=".github/tray-icons.png" alt="AI Usage Monitor 的 Claude Code 和 Codex 托盘图标" width="160"> |

图片截取自实际运行的 Windows 11 会话，并已裁剪为仅包含 AI Usage Monitor。

AI Usage Monitor 是一款轻量级原生 Windows 任务栏组件和系统托盘应用，
无需打开服务商控制台即可查看 Claude Code、Codex 和 Google Antigravity 的用量周期。

## 主要功能

- 查看当前 5 小时和 7 天用量，以及距离重置的倒计时
- 可选启用 Claude Code、Codex 和 Antigravity 服务商
- 每个已启用的服务商拥有独立托盘图标，显示百分比和两条紧凑用量条
- 详情弹窗跟随系统主题，显示服务状态、相对重置时间和绝对重置时间
- 重置通知默认关闭，可按需启用
- 使用相对任务栏的保存锚点支持多显示器任务栏定位
- 在 `explorer.exe` 重建任务栏和 RDP 会话切换后自动恢复
- 诊断日志采用只追加、自动轮换格式，包含本地时间戳和进程 ID
- 支持英语、简体中文、繁体中文及其他上游本地化语言
- 不包含分析、遥测、独立后端服务或模型生成探测

## 下载

从[最新 GitHub Release](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest)
下载 `ai-usage-monitor.exe`。首个公开版本采用便携方式分发：请将 EXE 放在
当前用户可写的目录中并运行。v2.0.0 可执行文件目前未进行代码签名；请使用
Release 中的 `SHA256SUMS` 文件校验下载内容。

### WinGet

首个 WinGet 软件包已经通过验证，正在
[microsoft/winget-pkgs#400395](https://github.com/microsoft/winget-pkgs/pull/400395)
等待维护者审核。微软接受该提交并完成社区目录同步后，可使用：

```powershell
winget install --id yinjianxxx.AIUsageMonitor --exact
```

`winget install CodeZeno.ClaudeCodeUsageMonitor` 安装的是上游 CodeZeno 应用，
不是 AI Usage Monitor。

### 从源码构建

要求：

- Windows 10 或 Windows 11
- 稳定版 Rust 工具链

```powershell
git clone https://github.com/yinjianxxx/ai-usage-monitor.git
cd ai-usage-monitor
cargo test --locked
cargo build --release --locked
.\target\release\ai-usage-monitor.exe
```

## 使用要求与账户访问

- 显示 Claude 用量前，Claude Code 必须已经安装并完成身份验证。
- Codex 支持需要已有且已登录的 Codex CLI 或应用会话。
- Antigravity 支持需要已有且已登录的 Antigravity 会话。
- 当存在可用的 WSL 发行版时，支持读取 WSL 中的 Claude Code 凭据。

本应用不会创建服务商账户，也不会绕过服务商的身份验证流程。
功能可用性以各服务商自身的账户和服务规则为准。

## 使用方法

- 拖动左侧分隔线可移动任务栏组件。
- 将组件拖到另一条任务栏可切换显示器。
- 左键单击托盘图标或组件主体，可打开或关闭详情弹窗。
- 右键单击组件或托盘图标，可设置服务商、刷新间隔、通知、开机启动、
  位置、语言、更新检查、组件显示状态，以及退出应用。
- 如需开机启动，请在右键菜单中启用 **Start with Windows**。

### 托盘图标

每个已启用的服务商都有一个无边框托盘图标。数字表示当前 5 小时用量；
上方和下方用量条分别表示 5 小时和 7 天周期。无法获取用量时，数字会替换为
服务商首字母。接近用量上限时，指示器会切换为警告颜色。

离线预览图标：

```powershell
.\ai-usage-monitor.exe --dump-tray-icons .\tray-icon-preview
```

## 稳定性分支改进

本分支为上游任务栏组件可能退出或永久隐藏的情况增加了恢复路径：

- 外部 `WM_DESTROY` 事件会触发进程内窗口重建，而不是立即退出。
- WTS 会话通知会在锁屏和 RDP 切换期间暂停恢复操作。
- 重新附加组件前，程序会等待任务栏几何结构稳定。
- Panic hook 和 FFI panic 边界会记录错误，避免无提示中止。
- 多次进程内恢复失败后，仍保留重新启动作为最终后备方案。

技术摘要请参阅 [FORK-NOTES.md](FORK-NOTES.md)（英文）。

## 本地数据与诊断

设置文件：

```text
%APPDATA%\AIUsageMonitor\settings.json
```

缓存的百分比和重置时间：

```text
%APPDATA%\AIUsageMonitor\usage-cache.json
```

诊断日志：

```text
%LOCALAPPDATA%\AIUsageMonitor\diagnose.log
%LOCALAPPDATA%\AIUsageMonitor\diagnose.log.old
```

用量缓存只包含百分比和重置时间戳，不会持久化 OAuth 令牌或服务商原始响应。

## 隐私与网络行为

本应用只读取已有的本地服务商凭据，用于验证只读用量请求。根据启用的服务商，
应用会直接连接 Anthropic、ChatGPT/Codex，以及 Google Cloud Code 或
Antigravity 端点。

检查本分支的 Release 更新时，应用还会连接 GitHub。当用户确认有可用更新后，
便携版可以从本仓库下载 `ai-usage-monitor.exe`。

本应用：

- 不收集分析数据或遥测数据；
- 不上传项目文件；
- 不会将凭据发送到独立后端；
- 不修改 Codex 凭据；
- 不运行 `claude -p`、`codex exec` 或其他可能调用模型的刷新探测；
- 不调用 `/v1/messages`、`/v1/chat/completions`、`/v1/responses`
  或 `/v1/completions` 等生成端点。

配置可信代理时应格外谨慎，因为服务商的 Bearer 令牌仍会包含在每个经代理转发的
TLS 请求中。

## 致谢与独立性声明

AI Usage Monitor 派生自
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)，
基于 v1.4.8（提交 `9b29972`）。托盘图标的呈现方式，以及部分 Claude 用量轮询、
缓存、冷却和速率限制处理，改编自或参考了
[jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude)。

本项目与 Code Zeno Pty Ltd、Anthropic、OpenAI 或 Google 不存在从属、认可、
赞助或合作关系。产品名和公司名仅用于说明兼容性；所有商标均归各自权利人所有。

## 许可证

本项目采用 MIT License 分发。保留的许可与归属声明请参阅 [LICENSE](LICENSE)、
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 和
[DEPENDENCY_LICENSES.md](DEPENDENCY_LICENSES.md)。
