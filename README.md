**English** | [简体中文](README.zh-CN.md)

<!-- Keep user-facing behavior, installation, privacy, and release status aligned with README.zh-CN.md. -->

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![CI](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/yinjianxxx/ai-usage-monitor/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yinjianxxx/ai-usage-monitor)](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

# AI Usage Monitor

> **Independent stability-focused community fork.** This project is based on
> [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)
> v1.4.8 (commit `9b29972`). It uses a separate executable name, mutex, window
> class, settings directory, log directory, and GitHub release channel.

## Screenshots

### Detail popup

![AI Usage Monitor detail popup](.github/screenshot.png)

### Taskbar widget and provider tray icons

| Taskbar widget | Provider tray icons |
| --- | --- |
| ![AI Usage Monitor embedded taskbar widget](.github/taskbar-widget.png) | <img src=".github/tray-icons.png" alt="AI Usage Monitor Claude Code and Codex tray icons" width="160"> |

Captured from a running Windows 11 session and cropped to AI Usage Monitor only.

AI Usage Monitor is a lightweight native Windows taskbar widget and system-tray
app for viewing Claude Code, Codex, and Google Antigravity usage windows without
opening a provider dashboard.

## Features

- Current 5-hour and 7-day usage with reset countdowns
- Optional Claude Code, Codex, and Antigravity providers
- One tray icon per enabled provider, with percentage and two compact usage bars
- A theme-aware detail popup with provider status, relative and absolute reset times
- Reset notifications that are disabled by default
- Multi-monitor taskbar placement using a taskbar-relative saved anchor
- Recovery after explorer.exe taskbar rebuilds and RDP session changes
- Append-only rotating diagnostics with local timestamps and process IDs
- English, Simplified Chinese, Traditional Chinese, and other upstream localizations
- No analytics, telemetry, backend service, or model-generation probes

## Download

Download `ai-usage-monitor.exe` from the
[latest GitHub Release](https://github.com/yinjianxxx/ai-usage-monitor/releases/latest).
The first public release is portable: place the EXE in a user-writable directory
and run it. The v2.0.0 executable is currently unsigned; verify downloads against
the release `SHA256SUMS` file.

### WinGet

The first WinGet package has passed validation and is awaiting maintainer review in
[microsoft/winget-pkgs#400395](https://github.com/microsoft/winget-pkgs/pull/400395).
After Microsoft accepts the submission and the community catalog synchronises:

```powershell
winget install --id yinjianxxx.AIUsageMonitor --exact
```

`winget install CodeZeno.ClaudeCodeUsageMonitor` installs the upstream CodeZeno
application, not AI Usage Monitor.

### Build from source

Requirements:

- Windows 10 or Windows 11
- Stable Rust toolchain

```powershell
git clone https://github.com/yinjianxxx/ai-usage-monitor.git
cd ai-usage-monitor
cargo test --locked
cargo build --release --locked
.\target\release\ai-usage-monitor.exe
```

## Requirements and account access

- Claude Code must already be installed and authenticated to display Claude usage.
- Codex support requires an existing authenticated Codex CLI or app session.
- Antigravity support requires an existing authenticated Antigravity session.
- WSL Claude Code credentials are supported when a usable WSL distribution is available.

The monitor does not create provider accounts or bypass provider authentication.
Availability follows each provider's own account and service rules.

## Use

- Drag the left divider to move the taskbar widget.
- Drag the widget to another taskbar to change monitors.
- Left-click a tray icon or the widget body to open or close the detail popup.
- Right-click the widget or a tray icon for provider selection, refresh interval,
  notifications, startup, position, language, update checks, widget visibility,
  and exit.
- Enable **Start with Windows** from the context menu if desired.

### Tray icons

Each enabled provider receives one frameless tray icon. The number shows current
5-hour usage; the upper and lower bars show the 5-hour and 7-day windows.
While usage is unavailable, the number is replaced with a provider initial.
Near a usage limit, the indicator switches to a warning colour.

For an offline icon preview:

```powershell
.\ai-usage-monitor.exe --dump-tray-icons .\tray-icon-preview
```

## Stability fork behaviour

The fork adds recovery paths for conditions that could terminate or permanently
hide the upstream taskbar widget:

- External `WM_DESTROY` events trigger in-process recreation instead of immediate exit.
- WTS session notifications pause recovery work during lock and RDP transitions.
- Taskbar geometry is allowed to stabilise before the widget is reattached.
- Panic hooks and FFI panic boundaries record failures instead of aborting silently.
- Relaunch is retained as a final fallback after repeated in-process recovery failures.

See [FORK-NOTES.md](FORK-NOTES.md) for the technical summary.

## Local data and diagnostics

Settings:

```text
%APPDATA%\AIUsageMonitor\settings.json
```

Cached percentages and reset times:

```text
%APPDATA%\AIUsageMonitor\usage-cache.json
```

Diagnostics:

```text
%LOCALAPPDATA%\AIUsageMonitor\diagnose.log
%LOCALAPPDATA%\AIUsageMonitor\diagnose.log.old
```

The usage cache contains percentages and reset timestamps only. It does not
persist OAuth tokens or raw provider responses.

## Privacy and network behaviour

The app reads existing local provider credentials only to authenticate read-only
usage requests. Depending on enabled providers, it connects directly to Anthropic,
ChatGPT/Codex, and Google Cloud Code or Antigravity endpoints.

It also connects to GitHub when checking this fork's Releases for an update.
Portable builds can download `ai-usage-monitor.exe` from this repository when
the user accepts an available update.

The app:

- does not collect analytics or telemetry;
- does not upload project files;
- does not send credentials to a separate backend;
- does not edit Codex credentials;
- does not run `claude -p`, `codex exec`, or other model-capable refresh probes;
- does not call generation endpoints such as `/v1/messages`,
  `/v1/chat/completions`, `/v1/responses`, or `/v1/completions`.

Trusted proxy configuration is important because provider bearer tokens remain
inside each proxied TLS request.

## Acknowledgements and independence

AI Usage Monitor is derived from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor),
based on v1.4.8 (commit `9b29972`). The tray-icon presentation and parts of
Claude usage polling, caching, cooldown, and rate-limit handling were adapted
from or informed by
[jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude).

This project is not affiliated with, endorsed by, or sponsored by Code Zeno
Pty Ltd, Anthropic, OpenAI, or Google. Product and company names are used only
to describe compatibility; all trademarks belong to their respective owners.

## License

This project is distributed under the MIT License. See [LICENSE](LICENSE),
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), and
[DEPENDENCY_LICENSES.md](DEPENDENCY_LICENSES.md) for retained notices.
