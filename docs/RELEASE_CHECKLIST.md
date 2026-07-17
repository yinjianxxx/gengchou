# Release checklist

Use this checklist before creating a version tag. Automated checks remain the
release gate; unavailable hardware-specific rows may be marked not applicable
with a short note in the release runbook.

## Automated

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- legacy updater inbound bridge E2E
- pinned released-v2.2.3 updater component E2E in an isolated Windows profile:
  successful replacement and rollback; pass `-AllowRealUserProfileWrite`
- identity migration state-machine E2E
- updater helper E2E: `Success` and `ChildExit`
- `cargo build --release --locked`
- Confirm the built file is `target/release/gengchou.exe`; inspect PE properties
  for ProductName `Gengchou`, version/tag agreement, retained upstream
  copyright/Comments, and the unchanged v2.1.0 application icon.
- Debug compact-surface gate: `cargo run --locked -- --dump-widget
  tmp/compact-release-check`; inspect every generated theme, warning/error,
  High Contrast, tooltip, and mixed-digit alignment fixture.

## Manual Windows smoke test

- Start one instance, launch the EXE again, and confirm the existing detail
  popup opens without a second resident process.
- Confirm taskbar widget, all enabled tray icons, detail popup, context menu,
  manual refresh, and clean Exit.
- Confirm Refresh is one submenu whose first item is Now, followed by a
  separator and the six checked polling intervals (1, 2, 5, 10, 15, and 30
  minutes); exercise Now and each interval once.
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
  Settings as direct checked toggles. Confirm the taskbar and floating-window
  position resets remain under Settings and no floating-window lock item is
  present.
- Hide/show the taskbar widget and restore its default position; confirm it
  returns next to the notification area on the primary taskbar.
- Show the floating window, drag it from several points across the whole
  compact surface, restart the app and confirm the position is remembered,
  then restore it to the primary work area's bottom-right. Confirm it remains
  draggable after reopening, a short click still opens details, and it never
  appears automatically as a taskbar fallback. At every work-area edge verify
  an 8-logical-pixel safety margin. Confirm the taskbar-only left divider is
  absent and the floating window remains above normal windows after dragging,
  changing display configuration, and restoring a remote session.
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
- Hover each taskbar badge and confirm the custom theme-aware hover card appears
  after the delay, lists every reported window with reset timing, stays within
  the work area, and disappears on pointer leave, click, display change, or
  Explorer rebuild.
- Exercise 100%, 125%, 150%, 175%, and 200% DPI where available. On mixed-DPI
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
- Test both a dark and a light Windows High Contrast theme. Confirm widget,
  tray icons, popup, compact floating window, tracks, and focus cues remain
  legible; warning row text must remain visible on the window canvas and every
  character inside warning/error pills must contrast with the highlight fill.
- Re-render the README previews from the final build with
  `tools\render-readme-images.ps1` and commit any changed `.github/readme/*.png`;
  verify the README text, alt text, provider marks, compact layout, and the
  version shown in the detail-popup images match the release.
- With Codex Desktop signed in and the CLI absent or unavailable, confirm Codex
  usage still loads from a supported local session.

## Update and release hand-off

- Verify a portable update releases the old PID and single-instance mutex,
  replaces the target, starts one new PID, and preserves the rollback backup
  until the new process reports ready.
- As a separate tag-blocking integration test, update a v2.2.3 copy through
  the real helper into the actual v2.2.4 application. Confirm the first launch
  preserves old settings and cache, writes `ready_seen`, uses the new runtime
  window classes, and holds both bridge mutexes without creating two resident
  processes. On an interactive Windows desktop with Gengchou closed, run
  `tests\v2.2.3_to_v2.2.4_real_e2e.ps1 -AllowInteractiveDesktopAndRealProfileWrite`.
- Exit and start v2.2.4 once more. Place the release `SHA256SUMS` beside the
  verifier and run
  `tools\verify-v2.2.4-migration.ps1 -RequireMigratedSource -RequireOfficialHash`
  for an upgraded installation (omit `-RequireMigratedSource` only for a clean
  v2.2.4 installation);
  require a clean PASS with state
  `complete`, valid new settings, no old data directories or Run values, and
  no unknown-file/reparse-point exception.
- Confirm the draft release has exactly nine attachments: new and legacy EXE
  and ZIP names, the migration verifier, three compliance files, and
  `SHA256SUMS`. The manifest must cover all eight payload assets, and all four
  EXE/ZIP assets must have build provenance attestations.
- Verify the WinGet path with `yinjianxxx.Gengchou` on an installed build when
  the new package update exists; do not test the unpublished former ID.
- Confirm the draft release re-download passes `SHA256SUMS` and GitHub
  attestation verification before the workflow publishes it.
- Confirm release notes mention any one-time migration, including notification
  icon placement changes.
- Do not publish v2.3.0 until every supported pre-v2.2.4 installation has run
  v2.2.4 twice and passed the verifier; release availability alone does not
  satisfy the gate.
