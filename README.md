**English** | [简体中文](README.zh-CN.md)

<!-- Keep user-facing behavior, installation, privacy, and release status aligned with README.zh-CN.md. -->

<div align="center">

# AI Usage Monitor

**Claude Code · Codex · Antigravity usage, right on the Windows taskbar.**

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![CI](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yinjianxxx/ai-usage-monitor)](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<img src=".github/screenshot.png" alt="Detail popup showing Claude Code, Codex, and Antigravity usage with reset times" width="480">

<sub>The detail popup — captured on Windows 11, dark theme.</sub>

</div>

AI Usage Monitor is a lightweight native Windows app that puts your current
5-hour and 7-day usage in a taskbar widget and one tray icon per provider,
so checking your quota never means opening a dashboard. Originally derived
from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor),
it is now developed independently ([provenance](PROVENANCE.md)).

## Install

Download `ai-usage-monitor.exe` from the
[latest release](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest)
and run it from any folder you can write to — no installer. The binary is
currently unsigned; the release's `SHA256SUMS` lets you verify the download,
and self-updates check it automatically.

A WinGet package is pending review in
[microsoft/winget-pkgs#400395](https://github.com/microsoft/winget-pkgs/pull/400395).
Once it lands:

```powershell
winget install --id yinjianxxx.AIUsageMonitor --exact
```

The similarly named `CodeZeno.ClaudeCodeUsageMonitor` package is the
original project, not this app.

<details>
<summary><b>Build from source</b> (Windows 10/11, stable Rust)</summary>

```powershell
git clone https://github.com/yinjianxxx/ai-usage-monitor.git
cd ai-usage-monitor
cargo build --release --locked
.\target\release\ai-usage-monitor.exe
```

</details>

## Features

- Live 5-hour and 7-day usage with reset countdowns
- Claude Code, Codex, and Google Antigravity — enable any combination
- One compact tray icon per provider: usage number plus 5h / 7d bars
- Theme-aware detail popup with per-provider status and exact reset times
- Optional reset notifications (off by default)
- Survives `explorer.exe` restarts and RDP / lock-screen transitions
- Multi-monitor and multi-taskbar aware
- 11 languages · no telemetry · a single ~1 MB portable executable

## Usage

<img src=".github/taskbar-widget.png" alt="AI Usage Monitor widget embedded in a Windows taskbar">

<sub>The widget embedded in a Windows 11 taskbar.</sub>

- **Left-click** the widget or a tray icon to open or close the detail popup.
- **Right-click** either one for settings: providers, refresh interval,
  notifications, start with Windows, and more.
- **Drag** the widget by its left divider to reposition it; drop it on
  another taskbar to change monitors.

### Tray icons

<img src=".github/tray-icons.png" alt="Claude Code, Codex, and Antigravity tray icons">

The number is the current 5-hour usage; the bars underneath track the 5-hour
(upper) and 7-day (lower) usage. With no data, the number gives way to the
provider's initial; near a limit, it turns a warning colour. The preview
above is rendered by the app itself —
`.\ai-usage-monitor.exe --dump-tray-icons .\preview` exports every state.

## Provider requirements

The monitor only reads your existing local sessions — it never creates
accounts or bypasses provider authentication, and what it can show follows
each provider's own account rules:

- **Claude Code** — installed and signed in (WSL credentials are picked up
  when a usable distribution exists)
- **Codex** — a signed-in Codex CLI or app session
- **Antigravity** — a signed-in Antigravity session

## Data & privacy

| What | Where |
| --- | --- |
| Settings | `%APPDATA%\AIUsageMonitor\settings.json` |
| Usage cache — percentages and reset times only, never tokens | `%APPDATA%\AIUsageMonitor\usage-cache.json` |
| Diagnostics (append-only, rotated) | `%LOCALAPPDATA%\AIUsageMonitor\diagnose.log` |

To uninstall: disable **Start with Windows** if you enabled it, then delete
the executable, `%APPDATA%\AIUsageMonitor`, and
`%LOCALAPPDATA%\AIUsageMonitor`.

Network traffic goes directly to the enabled providers (Anthropic,
ChatGPT/Codex, Google) for read-only usage queries, plus GitHub for update
checks and user-approved update downloads. The app never:

- collects analytics or telemetry, or uploads any files;
- sends credentials anywhere except the provider that issued them;
- modifies your credentials;
- triggers model generation — no `claude -p`, `codex exec`, or calls to
  `/v1/messages`, `/v1/chat/completions`, and similar endpoints.

Provider bearer tokens travel inside each TLS request, so only configure
proxies you trust.

## Stability

The project began as a stability rework of the original code. External
`WM_DESTROY`, `explorer.exe` taskbar rebuilds, and RDP session switches
trigger in-process recovery — relaunch is only a last resort — and panics
are logged instead of silently ending the process. See
[PROVENANCE.md](PROVENANCE.md) for the technical summary.

## Acknowledgements & license

Derived from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)
v1.4.8 (commit `9b29972`). The tray-icon presentation and parts of the Claude
usage polling, caching, cooldown, and rate-limit handling were adapted from or
informed by
[jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude).
This project is not affiliated with, endorsed by, or sponsored by Code Zeno
Pty Ltd, Anthropic, OpenAI, or Google. Product names are used only to
describe compatibility; all trademarks belong to their respective owners.

MIT License — see [LICENSE](LICENSE),
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), and
[DEPENDENCY_LICENSES.md](DEPENDENCY_LICENSES.md) for retained notices.
