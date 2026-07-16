[English](README.md) | **简体中文**

<!-- 修改用户可见行为、安装方式、隐私说明或发布状态时，请同步更新 README.md。 -->
<!-- 所有预览图均由应用自身渲染；用 tools\render-readme-images.ps1 重新生成。 -->

<div align="center">

# 更筹 Gengchou

**AI 配额，一目了然。**

<sub>Windows 任务栏 AI 用量监控工具 · 原 AI Usage Monitor</sub>

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![CI](https://github.com/yinjianxxx/gengchou/actions/workflows/ci.yml/badge.svg)](https://github.com/yinjianxxx/gengchou/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yinjianxxx/gengchou)](https://github.com/yinjianxxx/gengchou/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<img src=".github/readme/detail-popup-zh-dark.png" alt="深色主题详情弹窗：Claude Code 的 7 天窗口达 92% 被标记接近上限并高亮重置时间；Codex 51% 正常；Antigravity 空闲" width="400"> <img src=".github/readme/detail-popup-zh-light.png" alt="同一详情弹窗的浅色主题" width="400">

<sub>详情弹窗的深浅两种主题——包括接近上限时的警示样式。</sub>

</div>

更筹 Gengchou（读作 `gēng chóu`）把各家服务商实际返回的配额窗口——已用
多少、何时重置——直接放上 Windows 任务栏。Claude Code、Codex 和
Antigravity 各自的实时百分比可以落在你偏好的任何视图上：从完整的详情卡片
到一枚托盘小数字，查配额不必再打开任何控制台。

> 烧香知夜漏，刻烛验更筹。
>
> ——南朝梁·庾肩吾《奉和春夜应令》

“更筹”原指古代夜间计时、报更所用的筹签，亦可借指时间本身。

本项目原名 **AI Usage Monitor**：更名只改产品名称，不动你的数据——设置、
缓存与更新路径全部延续。项目最初派生自
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)，
现已独立开发（[项目起源](PROVENANCE.md)）。

## 视图总览

|  | 深色 | 浅色 |
| ---: | :--- | :--- |
| **任务栏组件** | <img src=".github/readme/widget-badges-dark.png" alt="任务栏组件（深色）：每家服务商一个徽章，含 logo、窗口标签和用量百分比"> | <img src=".github/readme/widget-badges-light.png" alt="任务栏组件（浅色）"> |
| **浮窗** | <img src=".github/readme/floating-rows-dark.png" alt="浮窗（深色）：每家服务商最多两个配额窗口，百分比、倒计时与微量表对齐"> | <img src=".github/readme/floating-rows-light.png" alt="浮窗（浅色）"> |
| **托盘图标** | <img src=".github/readme/tray-icons-dark.png" alt="托盘图标（深色）：各服务商用量数字叠于自适应量条之上"> | <img src=".github/readme/tray-icons-light.png" alt="托盘图标（浅色）"> |

以上预览不是截图：全部由应用自身的 `--dump-widget`、`--dump-tray-icons`、
`--dump-detail-popup` 模式渲染，呈现的就是发布代码绘制的原始像素。随时可用
[`tools/render-readme-images.ps1`](tools/render-readme-images.ps1) 重新生成。

- **任务栏组件。** 直接嵌入任务栏本体：每家服务商一个内容自适应的单行徽章，
  显示 logo、额度窗口标签和短窗口用量。悬停徽章可查看该服务商报告的全部
  额度窗口与重置时间；拖动左侧分隔线调整位置，拖到另一条任务栏即可切换
  显示器。Explorer 暂时不可用时组件保持隐藏而不是退回桌面，任务栏恢复后
  自动重新嵌入。
- **浮窗。** 独立的置顶数字视图，不是任务栏徽章的拉宽副本：每家服务商保留
  最多两个用量最高的配额窗口，标签、百分比和倒计时对齐在各自微量表上方。
  整窗任意位置可拖动，短按仍会打开详情弹窗；位置跨重启记忆，以 8 逻辑像素
  的安全间距保持在工作区内，并可在**设置**中恢复默认位置。
- **托盘图标。** 每个已启用的服务商一枚实时图标——数字和自适应量条跟随
  接口实际返回的额度窗口，暂无数据时显示服务商首字母。关闭**图标**则只保留
  一个中性软件图标。
- **详情弹窗。** 在任意视图上左键单击打开：各服务商状态徽章、精确的重置
  时刻、实时刷新倒计时，以及本次打开期间的临时位置锁定。

任何配额窗口达到 90% 时，它会接管该服务商的徽章、变红并显示自己的重置
倒计时——是警示来找你，而不是你去翻控制台：

<div align="center">
<img src=".github/readme/widget-badges-warn-dark.png" alt="警示状态的任务栏组件：Claude Code 的 7 天窗口达 92%，红色徽章接管并显示重置倒计时">
</div>

## 安装

推荐按以下顺序选择安装方式：

1. **WinGet（可用时首选）。** 更筹使用新的软件包标识；原 AI Usage Monitor
   从未正式进入 WinGet。新包可用后运行：

   ```powershell
   winget install --id yinjianxxx.Gengchou --exact
   ```

   如果 WinGet 仍找不到 `yinjianxxx.Gengchou`，请使用下面的 ZIP。

2. **便携 ZIP（推荐的手动下载方式）。** 从
   [最新 Release](https://github.com/yinjianxxx/gengchou/releases/latest)
   下载 `gengchou-windows-x64.zip`，解压到任意可写目录后运行
   `gengchou.exe`。压缩包同时包含中英文 README，以及保留的许可和
   归属声明。

3. **独立 EXE。** 如需单文件下载，可从同一 Release 获取
   `gengchou.exe`，放在任意可写目录直接运行。

可执行文件目前未做代码签名；每个 Release 均附带 `SHA256SUMS` 供校验
下载，应用内更新也会自动核对。从 v2.1.0 起，发布资产还带有 GitHub
artifact attestation，用于证明构建来源，但不能替代 Authenticode 签名。

名称相近的 `CodeZeno.ClaudeCodeUsageMonitor` 是原项目的软件包，不是本应用。

<details>
<summary><b>从源码构建</b>（Windows 10/11，稳定版 Rust）</summary>

```powershell
git clone https://github.com/yinjianxxx/gengchou.git
cd gengchou
cargo build --release --locked
.\target\release\gengchou.exe
```

</details>

发布维护者还应执行[发布检查清单](docs/RELEASE_CHECKLIST.md)。

## 操作方式

- **左键单击**组件或托盘图标，打开或关闭详情弹窗。
- 详情弹窗默认可移动；单击锁定按钮可在本次打开期间固定位置，关闭后下次
  恢复自动定位和可移动状态。
- **右键单击**任意视图打开菜单，直接单击**图标**、**小组件**或**浮窗**即可
  切换对应视图。位置重置、通知和开机启动等位于**设置**。
- 展开**刷新**后，可单击顶部的**现在**立即刷新，也可选择自动刷新频率。

## 视图之外

- 配额数据来自各服务商的实际返回——窗口与重置时间从不靠猜测或外推
- Claude Code、Codex、Google Antigravity 可任意组合启用
- 高对比度模式下使用 Windows 系统颜色
- 可选的重置通知（默认关闭）
- 在 `explorer.exe` 重启和 RDP / 锁屏切换后自动恢复；锁屏期间仍按既定间隔
  轮询，恢复时只重建本地界面，不额外突发请求
- 支持多显示器、多任务栏
- 11 种语言 · 无遥测 · 单个约 1 MB 的便携可执行文件

## 服务商要求

本应用只读取本机已有的登录会话，不会创建账户或绕过服务商身份验证，
可显示的内容以各服务商自身的账户规则为准：

- **Claude Code** —— 已安装并完成登录（存在可用 WSL 发行版时，
  也会读取 WSL 中的凭据）
- **Codex** —— 已登录的 Codex Desktop 或 CLI 会话；如果 Desktop 已保存受支持
  的本地会话，无需另外安装 CLI
- **Antigravity** —— 已登录的 Antigravity 会话

## 数据与隐私

| 内容 | 位置 |
| --- | --- |
| 设置 | `%APPDATA%\AIUsageMonitor\settings.json` |
| 用量缓存——仅百分比、配额窗口元数据和重置时间，绝不含令牌 | `%APPDATA%\AIUsageMonitor\usage-cache.json` |
| 诊断日志（只追加、自动轮换） | `%LOCALAPPDATA%\AIUsageMonitor\diagnose.log` |

内部目录名 `AIUsageMonitor` 会继续保留，确保从旧版本升级后无需迁移即可沿用
设置、缓存、诊断日志和开机启动状态。

卸载方法：如启用了**开机启动**请先在菜单中关闭，然后删除可执行文件以及
`%APPDATA%\AIUsageMonitor` 和 `%LOCALAPPDATA%\AIUsageMonitor` 两个目录。

网络请求直接发往已启用的服务商（Anthropic、ChatGPT/Codex、Google）执行
只读用量查询，另在检查更新和用户确认更新时连接 GitHub。本应用绝不会：

- 收集分析或遥测数据，或上传任何文件；
- 将凭据发送给其签发者以外的任何一方；
- 修改你的凭据；
- 触发模型生成——不运行 `claude -p`、`codex exec`，也不调用
  `/v1/messages`、`/v1/chat/completions` 等生成端点。

服务商 Bearer 令牌包含在每个 TLS 请求中，请只配置你信任的代理。

## 稳定性

本项目起步于对原项目代码的稳定性重做。外部 `WM_DESTROY`、`explorer.exe`
任务栏重建和 RDP 会话切换会触发进程内恢复，重启进程仅是最终后备；panic
会被记录，而不是让进程无声退出。技术摘要见
[PROVENANCE.md](PROVENANCE.md)（英文）。

## 致谢与许可证

本项目派生自
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)
v1.4.8（提交 `9b29972`）。托盘图标呈现方式及部分 Claude 用量轮询、缓存、
冷却与速率限制处理，改编自或参考了
[jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude)。
本项目与 Code Zeno Pty Ltd、Anthropic、OpenAI 或 Google 不存在从属、认可或
赞助关系。产品名仅用于说明兼容性；所有商标归各自权利人所有。

MIT License——保留的许可与归属声明见 [LICENSE](LICENSE)、
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 和
[DEPENDENCY_LICENSES.md](DEPENDENCY_LICENSES.md)。
