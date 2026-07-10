**English** | [简体中文](README.zh-CN.md)

<!-- Keep user-facing behavior, installation, privacy, and release status aligned with README.zh-CN.md. -->

<div align="center">

# AI Usage Monitor

**Claude Code · Codex · Antigravity usage, right on the Windows taskbar.**

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![CI](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yinjianxxx/ai-usage-monitor)](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<img src=".github/screenshot.png" alt="Detail popup showing Claude Code and Codex usage bars with reset times" width="500">

<br>

<img src=".github/taskbar-widget.png" alt="AI Usage Monitor widget embedded in the Windows taskbar">

<sub>Real Windows 11 captures: the detail popup (top) and the embedded taskbar widget (bottom).</sub>

</div>

AI Usage Monitor is a lightweight native Windows app that keeps your current
5-hour and 7-day usage windows visible at a glance — as a taskbar widget and
one tray icon per provider — so you never open a dashboard just to check
quota. Originally derived from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor),
it has since focused on stability and multi-provider support
([provenance](PROVENANCE.md)).

## Install

Download `ai-usage-monitor.exe` from the
[latest release](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest),
put it in any user-writable folder, and run it — no installer. The binary is
currently unsigned; verify it against the release's `SHA256SUMS` if you wish
(in-app updates check it automatically).

Once [microsoft/winget-pkgs#400395](https://github.com/microsoft/winget-pkgs/pull/400395)
is merged, WinGet will also work (not to be confused with the upstream
package `CodeZeno.ClaudeCodeUsageMonitor`):

```powershell
winget install --id yinjianxxx.AIUsageMonitor --exact
```

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

- **Left-click** the widget or a tray icon to open or close the detail popup.
- **Right-click** for the menu: providers, refresh interval, notifications,
  start with Windows, position, language, update checks, widget visibility,
  and exit.
- **Drag** the widget by its left divider to reposition it; drop it on
  another taskbar to change monitors.

### Tray icons

<img src=".github/tray-icons.png" alt="Claude Code, Codex, and Antigravity tray icons">

The number is the current 5-hour usage; the bars underneath show the 5-hour
(upper) and 7-day (lower) windows. While no data is available the number
gives way to a provider initial, and near a limit it switches to a warning
colour. The preview above is rendered by the app itself —
`.\ai-usage-monitor.exe --dump-tray-icons .\preview` exports every state.

## Requirements

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

The project began as a stability rework of the code it derives from: external
`WM_DESTROY`, `explorer.exe` taskbar rebuilds, and RDP session switches
trigger in-process recovery, with relaunch kept only as a last resort, and
panics are logged instead of silently ending the process. See
[PROVENANCE.md](PROVENANCE.md) for the technical summary.

## Acknowledgements & license

Derived from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)
v1.4.8 (commit `9b29972`). The tray-icon presentation and parts of the Claude
usage polling, caching, cooldown, and rate-limit handling were adapted from or
informed by
[jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude).
This project is not affiliated with, endorsed by, or sponsored by Code Zeno
Pty Ltd, Anthropic, OpenAI, or Google; product names are used only to describe
compatibility, and all trademarks belong to their respective owners.

MIT License — see [LICENSE](LICENSE),
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), and
[DEPENDENCY_LICENSES.md](DEPENDENCY_LICENSES.md) for retained notices.
