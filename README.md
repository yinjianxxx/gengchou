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
provider-reported quota windows in a taskbar widget and compact tray icons, so
checking your quota never means opening a dashboard. Originally derived
from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor),
it is now developed independently ([provenance](PROVENANCE.md)).

## Install

Installation options, in recommended order:

1. **WinGet (preferred when available).** The initial package status is tracked
   in [microsoft/winget-pkgs#400395](https://github.com/microsoft/winget-pkgs/pull/400395).
   Try:

   ```powershell
   winget install --id yinjianxxx.AIUsageMonitor --exact
   ```

   If WinGet does not find `yinjianxxx.AIUsageMonitor` yet, use the ZIP below.

2. **Portable ZIP (recommended manual download).** Download
   `ai-usage-monitor-windows-x64.zip` from the
   [latest release](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest),
   extract it to any folder you can write to, and run
   `ai-usage-monitor.exe`. The bundle includes both READMEs and the retained
   license and attribution notices.

3. **Standalone EXE.** For a single-file download, get
   `ai-usage-monitor.exe` from the same release and run it from any writable
   folder.

The executable is currently unsigned. Each release includes `SHA256SUMS` for
download verification, and self-updates check it automatically. Starting with
v2.1.0, release binaries also carry GitHub artifact attestations; these provide
build provenance but do not replace Authenticode signing.

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

Release maintainers should also follow the [release checklist](docs/RELEASE_CHECKLIST.md).

## Features

- Live provider-reported quota windows with reset countdowns
- Claude Code, Codex, and Google Antigravity — enable any combination
- Icons enabled by default: one compact usage icon per provider, with an
  optional single app-icon mode
- Theme-aware detail popup with per-provider status, exact reset times, a
  live-updating refresh countdown, and a temporary position lock
- Optional long-lived floating copy of the compact widget, draggable from its
  whole surface, position-aware across restarts, and resettable to the primary
  work area's bottom-right corner
- Windows system colours in High Contrast mode
- Optional reset notifications (off by default)
- Survives `explorer.exe` restarts and RDP / lock-screen transitions
- Keeps provider polling on its configured cadence while Windows is locked or
  RDP is disconnected; restoration only rebuilds local UI surfaces
- Multi-monitor and multi-taskbar aware
- 11 languages · no telemetry · a single ~1 MB portable executable

## Usage

- **Left-click** the widget or a tray icon to open or close the detail popup.
- The detail popup is movable by default. Use its lock button to fix it in
  place for the current opening; closing it restores automatic placement and
  movable mode next time.
- **Right-click** a widget or tray icon, then click **Icons**, **Widget**, or
  **Floating Window** directly to toggle it. Position resets, notifications,
  and start-with-Windows are under **Settings**.
- Open **Refresh** to refresh immediately with **Now** or select the automatic
  polling interval.

### Taskbar widget

<img src=".github/taskbar-widget.png" alt="AI Usage Monitor widget embedded in a Windows taskbar">

The widget embeds directly in the taskbar. Each provider gets a content-sized,
single-line badge with its logo, quota-window label, and short-window usage.
If any other window reaches 90%, that warning window takes over, the badge
turns red, and its countdown appears. Hover a badge to inspect every reported
quota window and reset time. Drag the left
divider to reposition it or drop it on another taskbar to change monitors. If
Explorer is temporarily unavailable the widget remains hidden instead of
appearing on the desktop, then re-embeds when the taskbar returns.

### Floating window

The optional floating window is a separate long-lived numeric view, not an
automatic fallback or a stretched copy of the taskbar widget. It keeps up to
two highest-usage quota windows visible beside each provider logo, with each
label, percentage, and countdown aligned above its micro gauge. Drag anywhere
on its surface; a short click still opens the detail popup. It stays above
normal windows, remembers its position across restarts, remains inside the
active work area with an 8-logical-pixel safety margin, and can be reset from
**Settings**. Pairing these two distinct views with the single neutral app-icon
mode avoids repeating the same number in the tray.

### Tray icons

<img src=".github/tray-icons.png" alt="Claude Code, Codex, and Antigravity tray icons">

With **Icons** enabled, one icon appears per enabled provider:
the number and adaptive bars follow the quota windows that provider actually
reports. With no data, the number gives way to the provider's initial; near a
limit, it turns a warning colour. Disable the setting to keep one neutral app
icon shared with the executable. The preview above is rendered by the app —
run `.\ai-usage-monitor.exe --dump-tray-icons .\preview` to export every
provider-icon state.

## Provider requirements

The monitor only reads your existing local sessions — it never creates
accounts or bypasses provider authentication, and what it can show follows
each provider's own account rules:

- **Claude Code** — installed and signed in (WSL credentials are picked up
  when a usable distribution exists)
- **Codex** — a signed-in Codex Desktop or CLI session; the CLI executable is
  not required when Desktop has already saved a supported local session
- **Antigravity** — a signed-in Antigravity session

## Data & privacy

| What | Where |
| --- | --- |
| Settings | `%APPDATA%\AIUsageMonitor\settings.json` |
| Usage cache — percentages, quota-window metadata, and reset times only; never tokens | `%APPDATA%\AIUsageMonitor\usage-cache.json` |
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
