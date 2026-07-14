# Release checklist

Use this checklist before creating a version tag. Automated checks remain the
release gate; unavailable hardware-specific rows may be marked not applicable
with a short note in the release runbook.

## Automated

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- updater helper E2E: `Success` and `ChildExit`
- `cargo build --release --locked`

## Manual Windows smoke test

- Start one instance, launch the EXE again, and confirm the existing detail
  popup opens without a second resident process.
- Confirm taskbar widget, all enabled tray icons, detail popup, context menu,
  manual refresh, and clean Exit.
- Confirm Refresh is one submenu whose first item is Now, followed by a
  separator and the four checked polling intervals; exercise Now and each
  interval once.
- Keep the detail popup open across a poll boundary and confirm Updated /
  next-in advances once per second without extra provider requests.
- Drag the detail popup in its default movable state, lock it and confirm it no
  longer moves, then close and reopen it; movable mode must return and the
  moved position must not be restored.
- Restart Explorer and confirm the widget and tray icons recover once, without
  duplicate icons or processes.
- Lock/unlock Windows and, when available, disconnect/reconnect RDP; confirm no
  extra wake-time poll occurs and the normal configured polling cadence
  continues while inactive. The widget must stay hidden rather than appearing
  as a desktop popup, then re-embed from cached state as soon as the taskbar
  returns.
- Confirm Icons, Widget, and Floating Window appear in that order before
  Settings as direct checked toggles. Confirm both position resets and Floating
  Window Position Lock remain under Settings.
- Hide/show the taskbar widget and restore its default position; confirm it
  returns next to the notification area on the primary taskbar.
- Show the floating window, drag it from several points across the whole
  compact surface, restart the app and confirm the position is remembered,
  lock it and confirm it no longer moves, then restore it to the primary work
  area's bottom-right. Confirm a short click still opens details and it never
  appears automatically as a taskbar fallback. Confirm the taskbar-only left
  divider is absent and the floating window remains above normal windows after
  dragging, changing display configuration, and restoring a remote session.
- Switch the UI through at least English, Simplified Chinese, and one other
  language; confirm taskbar and floating duration labels and countdowns still
  use only `d`/`h`/`m`/`s`/`now`, while detail-popup prose remains localized.
- Drag the detailed tray icons into a different order and confirm the taskbar
  widget and floating window change together after the short stability delay
  (normally about 120ms), without showing an intermediate order or waiting for
  the next countdown refresh.
- Disable Icons and confirm the three provider icons are replaced
  by one app icon matching the executable; re-enable it and confirm all enabled
  provider icons return without duplicates. At each tested DPI, confirm the app
  icon fills the Shell slot without clipping. Exercise a notification in both
  modes.
- Hover each provider icon and confirm its title and quota windows use separate
  lines with reset timing in parentheses. Disable Icons and confirm the app
  icon uses one compact line per provider without mid-line truncation.
- Exercise 100%, 125%, 150%, and 200% DPI where available. On mixed-DPI
  monitors, move the detail and floating windows across monitor boundaries and
  confirm their suggested position, size, hit targets, and remembered floating
  position remain correct while the taskbar widget keeps its own scale.
- Exercise horizontal, vertical/third-party, and multi-row taskbars where
  available; failed embedding must keep the widget hidden and recovery armed.
- On a multi-monitor system, switch the primary display and drag the widget
  between taskbars; verify saved position and tray-driven provider order.
  During the display transition, confirm a still-valid embedded widget is not
  detached merely because Windows briefly enumerates only one taskbar, and
  confirm tray and floating-window context menus remain responsive.
- Enable Windows High Contrast and confirm widget, tray icons, popup, compact
  floating window, tracks, and focus cues remain legible.
- With Codex Desktop signed in and the CLI absent or unavailable, confirm Codex
  usage still loads from a supported local session.

## Update and release hand-off

- Verify a portable update releases the old PID and single-instance mutex,
  replaces the target, starts one new PID, and preserves the rollback backup
  until the new process reports ready.
- Verify the WinGet path on an installed build when the package update exists.
- Confirm the draft release re-download passes `SHA256SUMS` and GitHub
  attestation verification before the workflow publishes it.
- Confirm release notes mention any one-time migration, including notification
  icon placement changes.
