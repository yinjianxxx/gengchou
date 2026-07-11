# Provenance

AI Usage Monitor is originally derived from
[CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor),
based on v1.4.8 (commit `9b29972`). The project's initial goal was to survive
Microsoft Remote Desktop transitions and explorer.exe taskbar rebuilds without
crashing or permanently losing the embedded widget; it has since developed
independently.

## Relationship to the original project

- The repository preserves the complete upstream git history; releases begin on the 2.x version line.
- Original source repository: [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor).
- No ongoing synchronization with the upstream projects is planned. Their repositories remain referenced for provenance, comparison, and occasional security review.
- Relevant fixes may still be evaluated independently; this is not a commitment to track or merge upstream releases.

## Identity isolation

| Item | CodeZeno original | AI Usage Monitor |
|---|---|---|
| Package and EXE | claude-code-usage-monitor | ai-usage-monitor |
| Version line | 1.4.x | 2.x |
| Single-instance mutex | Global\ClaudeCodeUsageMonitor | Global\AIUsageMonitor |
| Window class | ClaudeCodeUsageMonitor | AIUsageMonitor |
| Settings directory | %APPDATA%\ClaudeCodeUsageMonitor | %APPDATA%\AIUsageMonitor |
| Diagnostic log | %TEMP%\claude-code-usage-monitor.log | %LOCALAPPDATA%\AIUsageMonitor\diagnose.log |
| Updates | Upstream GitHub Releases | This project's GitHub Releases and independent EXE asset |
| Update staging directory | %LOCALAPPDATA%\ClaudeCodeUsageMonitor\updates | %LOCALAPPDATA%\AIUsageMonitor\updates |

## Stability changes

1. Distinguish an intentional user exit from external destruction by explorer.exe. External destruction starts in-process recreation instead of immediately terminating the process.
2. Remove `panic = "abort"`, install a panic hook, and guard window procedures and WinEvent callbacks against unwinding across FFI.
3. Register WTS session notifications and suspend recovery activity during lock, disconnect, and unstable RDP transitions.
4. Retry relaunch and mutex hand-off, retaining process restart only as a final fallback after repeated in-process recovery failures.
5. Enable append-only rotating diagnostics with readable local timestamps and process IDs.
6. Verify self-update downloads against the release `SHA256SUMS` manifest before replacing the executable.

## Position anchoring

The widget stores `taskbar_index + tray_offset`, not an absolute screen
coordinate. Startup, explorer.exe recovery, and RDP recovery recalculate the
current position from that anchor and temporarily clamp it to the available
taskbar. Only a user drag or **Reset Position** updates the saved anchor.

## Third-party provenance

- The main codebase derives from CodeZeno/Claude-Code-Usage-Monitor v1.4.8 (commit `9b29972`).
- The tray-icon presentation and Claude OAuth usage polling, caching, cooldown, and HTTP 429 handling were adapted from or informed by [jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude).
- Complete notices are provided in `LICENSE`, `THIRD_PARTY_NOTICES.md`, and `DEPENDENCY_LICENSES.md`.

## Build

```powershell
cargo test --locked
cargo build --release --locked
```

Release binary: `target/release/ai-usage-monitor.exe`.

## License

The upstream MIT License and its exact copyright notice are retained in
`LICENSE` (`Copyright (c) 2025 Craig Constable`). Attribution for other
third-party material is retained in `THIRD_PARTY_NOTICES.md`.
