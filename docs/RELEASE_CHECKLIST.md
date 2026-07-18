# Release checklist

Use this checklist before creating a version tag. Automated checks remain the
release gate; unavailable hardware-specific rows may be marked not applicable
with a short note in the release runbook.

## Automated

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- `tools\check-retired-identity.ps1`
- current updater inbound readiness E2E
- updater helper E2E: `Success` and `ChildExit`
- `cargo build --release --locked`
- `tools\check-portable-runtime.ps1` rejects external MSVC/UCRT runtime DLLs
  so the portable executable starts without a separate redistributable.
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
- For the v2.3.0 tag only, confirm every supported older installation has run
  v2.2.4 twice and passed the official migration verifier. Record the result
  outside the repository before creating the tag.
- Confirm the draft release has exactly six attachments: `gengchou.exe`,
  `gengchou-windows-x64.zip`, three compliance files, and `SHA256SUMS`. The
  manifest must cover all five payload assets, and the EXE and ZIP must both
  have build provenance attestations.
- Verify the WinGet path with `ynjmxn.Gengchou` on an installed build when
  the new package update exists; do not test the unpublished former ID.
- Confirm the draft release re-download passes `SHA256SUMS` and GitHub
  attestation verification before the workflow publishes it.
- Confirm release notes describe user-visible changes and any required upgrade
  order without reintroducing retired asset names.
- Confirm the GitHub About description and topics contain only the current
  product identity. Preserve the renamed repository redirect, historical
  releases, tags, and git history.

## Post-release WinGet hand-off

- Publish and re-download the final GitHub release before preparing any WinGet
  manifest; use the public `gengchou-windows-x64.zip` URL and its released
  SHA-256, never a draft or local build.
- Submit the first `ynjmxn.Gengchou` package only after the matching GitHub
  release is public, then wait for the WinGet validation pipeline and review.
- After the WinGet pull request is merged, install the public package on a
  clean Windows profile, confirm the installed command is `gengchou`, and test
  launch, update detection, and uninstall.
