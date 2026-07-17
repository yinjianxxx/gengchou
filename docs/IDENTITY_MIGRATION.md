# Internal identity migration

This file is the source of truth for the v2.2.4 bridge and the v2.3.0 cleanup.

## Version boundary

- v2.2.3 is stable but still uses the former internal paths, mutex, window
  classes, startup value, and updater handshake.
- v2.2.4 is the only bridge. It accepts the v2.2.3 updater transaction, moves
  user state to Gengchou, and starts all later updates with the new protocol.
- v2.3.0 removes this bridge code and the compatibility release assets. It is
  the first version eligible for the `yinjianxxx.Gengchou` WinGet package.

## Bridge transaction

The normal application acquires the old mutex and then the Gengchou mutex
before reading migration input. A second v2.2.3 or v2.2.4 process therefore
cannot change settings while the transaction is prepared.

The state file is `%APPDATA%\Gengchou\migration-v2.2.4.json`:

```text
prepared -> ready_seen -> complete
```

- `prepared`: normalized settings were atomically written and read back; a
  fresh cache was copied when available; the startup value was verified.
- `ready_seen`: the UI reached its healthy-launch milestone and any updater
  ready marker was written successfully.
- `complete`: a later launch without an updater marker removed the known files
  owned by this project and independently found no unknown entries or reparse
  points in those retired directories.

Both automatic and manual update actions stay disabled until `complete`.
This prevents a user who installs v2.2.4 after v2.3.0 exists from skipping the
second-start cleanup gate.

The state records the normalized source and destination settings hashes. An
interrupted update may return to v2.2.3, which can change the source before the
bridge is tried again. v2.2.4 rewrites an unchanged prepared destination from
that new source; it refuses to guess when both sides changed.

## Updater boundary

Inbound v2.2.3 transactions use `AIUM_UPDATE_READY_FILE`, the former local
updates directory, and the former marker content. v2.2.4 accepts that exact
combination only. New transactions use `GENGCHOU_UPDATE_READY_FILE`,
`%LOCALAPPDATA%\Gengchou\updates`, and the Gengchou marker content. Both
variables appearing together is an error.

## Cleanup boundary

Cleanup owns only the retired `AIUsageMonitor` data directories and Run value.
The older `ClaudeCodexUsageMonitor` settings directory is a source-only
fallback when the direct v2.2.3 settings do not exist; it and the separate
CodeZeno application identity are never removed or required to be absent.

Cleanup never calls recursive directory deletion. It removes only known
settings, cache, log, updater, temporary, and ready-marker files, then removes
directories only when they are empty. Unknown files, junctions, symlinks,
other reparse points, or an executable located inside an owned retired data
directory keep the state at `ready_seen`. Monitoring remains available, while
updates stay disabled until the exception is reviewed and cleanup completes.
On the very first bridge launch, an unsafe old source path remains a hard
block because no trusted Gengchou settings exist yet; the nonblocking rule
applies after the new settings and migration state have been durably recorded.

The old diagnostic log is not migrated. v2.2.4 starts a new log under the
Gengchou path and removes the old log only at the `complete` transition.

## v2.3.0 gate

Every supported pre-v2.2.4 installation must pass
`verify-v2.2.4-migration.ps1 -RequireMigratedSource -RequireOfficialHash`,
with the release `SHA256SUMS` beside the script or its expected hash supplied
explicitly. A clean v2.2.4 installation has no source receipt and therefore
uses `-RequireOfficialHash` without `-RequireMigratedSource`. The
script independently checks the running binary, state, settings, optional
cache, startup values, data directories, new broadcast window, and both bridge
mutexes. A stored `complete` value by itself is not sufficient.

Keeping the bridge release downloadable is not sufficient: v2.3.0 must not be
published until every supported pre-v2.2.4 installation has completed this
gate. Only
then may v2.3.0 delete the bridge module, old mutex and broadcast probes,
old updater protocol, compatibility release assets, verifier, and this file;
run the strict retired-identity allowlist; publish the immutable release; and
submit the new WinGet package.
