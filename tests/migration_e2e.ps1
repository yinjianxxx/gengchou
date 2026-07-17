[CmdletBinding()]
param(
    [string]$BinaryPath = '',
    [ValidateRange(5, 30)]
    [int]$TimeoutSeconds = 15
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $PSScriptRoot '..\target\debug\gengchou.exe'
}
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Debug binary not found: $BinaryPath"
}

$nonce = [Guid]::NewGuid().ToString('N')
$root = Join-Path ([IO.Path]::GetTempPath()) "gengchou-migration-e2e-$PID-$nonce"
$appdata = Join-Path $root 'Roaming'
$localAppdata = Join-Path $root 'Local'
$legacyApp = Join-Path $appdata 'AIUsageMonitor'
$legacyLocal = Join-Path $localAppdata 'AIUsageMonitor'
$legacyUpdates = Join-Path $legacyLocal 'updates'
$upstreamApp = Join-Path $appdata 'ClaudeCodexUsageMonitor'
$upstreamLocal = Join-Path $localAppdata 'ClaudeCodexUsageMonitor'
$currentApp = Join-Path $appdata 'Gengchou'
$runSubkey = "Software\GengchouTests\Migration-$PID-$nonce"
$runKey = "HKCU:\$runSubkey"
$nameGateRunKey = $null
$utf8NoBom = [Text.UTF8Encoding]::new($false)

function Invoke-MigrationStart {
    param(
        [Parameter(Mandatory)][hashtable]$Environment,
        [string]$ExecutablePath = $BinaryPath
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $ExecutablePath
    $startInfo.WorkingDirectory = $root
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    if ($startInfo.PSObject.Properties.Name -contains 'ArgumentList') {
        $startInfo.ArgumentList.Add('--migration-e2e-ready')
    }
    else {
        $startInfo.Arguments = '--migration-e2e-ready'
    }
    foreach ($entry in $Environment.GetEnumerator()) {
        if (($startInfo.PSObject.Properties.Name -contains 'Environment') -and
            $null -ne $startInfo.Environment) {
            $startInfo.Environment[[string]$entry.Key] = [string]$entry.Value
        }
        else {
            $startInfo.EnvironmentVariables[[string]$entry.Key] = [string]$entry.Value
        }
    }
    $process = [Diagnostics.Process]::Start($startInfo)
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        $process.Kill()
        throw 'Migration E2E process timed out.'
    }
    return $process.ExitCode
}

try {
    New-Item -ItemType Directory -Path $legacyApp -Force | Out-Null
    New-Item -ItemType Directory -Path $legacyUpdates -Force | Out-Null
    New-Item -ItemType Directory -Path $upstreamApp -Force | Out-Null
    New-Item -ItemType Directory -Path $upstreamLocal -Force | Out-Null
    New-Item -ItemType Directory -Path $runKey -Force | Out-Null

    $settings = @{
        tray_offset = 17
        taskbar_index = 1
        poll_interval_ms = 600000
        language = 'zh-CN'
        widget_visible = $true
        floating_visible = $true
        floating_x = 321
        floating_y = 654
        show_claude_code = $true
        show_codex = $true
        show_antigravity = $true
        provider_order = @('codex', 'antigravity', 'claude')
    } | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText((Join-Path $legacyApp 'settings.json'), $settings, $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $upstreamApp 'settings.json'),
        '{"tray_offset":99,"poll_interval_ms":60000}',
        $utf8NoBom
    )

    $cache = @{
        saved_unix = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
        codex = @{
            windows = @(@{ percent = 23.0; duration_seconds = 18000 })
        }
    } | ConvertTo-Json -Depth 6 -Compress
    [IO.File]::WriteAllText((Join-Path $legacyApp 'usage-cache.json'), $cache, $utf8NoBom)
    [IO.File]::WriteAllText((Join-Path $legacyLocal 'diagnose.log'), "legacy log`n", $utf8NoBom)
    [IO.File]::WriteAllText((Join-Path $upstreamLocal 'diagnose.log'), "upstream log`n", $utf8NoBom)
    [IO.File]::WriteAllText((Join-Path $legacyUpdates 'updater-helper.exe'), 'fixture', $utf8NoBom)

    $quotedExe = '"' + $BinaryPath + '"'
    New-ItemProperty -Path $runKey -Name 'AIUsageMonitor' -Value $quotedExe -PropertyType String -Force | Out-Null

    $baseEnvironment = @{
        'APPDATA' = $appdata
        'LOCALAPPDATA' = $localAppdata
        'GENGCHOU_MIGRATION_TEST_RUN_KEY_PATH' = $runSubkey
        'GENGCHOU_MIGRATION_TEST_MUTEX_SUFFIX' = $nonce
    }
    $firstEnvironment = @{}
    foreach ($entry in $baseEnvironment.GetEnumerator()) {
        $firstEnvironment[$entry.Key] = $entry.Value
    }
    $firstEnvironment['AIUM_UPDATE_READY_FILE'] = Join-Path $legacyUpdates 'update-ready-e2e.marker'

    $exitCode = Invoke-MigrationStart -Environment $firstEnvironment
    if ($exitCode -ne 0) {
        throw "First migration start exited with $exitCode."
    }

    $statePath = Join-Path $currentApp 'migration-v2.2.4.json'
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if ($state.stage -ne 'ready_seen' -or $state.settings_pending) {
        throw 'First start did not commit ready_seen with a completed settings write.'
    }
    $migratedSettings = Get-Content -LiteralPath (Join-Path $currentApp 'settings.json') -Raw | ConvertFrom-Json
    if ($migratedSettings.tray_offset -ne 17 -or $migratedSettings.floating_x -ne 321) {
        throw 'Migrated settings did not preserve sentinel values.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $currentApp 'usage-cache.json') -PathType Leaf)) {
        throw 'Fresh usage cache was not migrated.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $legacyApp 'settings.json') -PathType Leaf)) {
        throw 'First start deleted legacy settings before the helper-safe restart.'
    }
    $runValues = Get-ItemProperty -Path $runKey
    $hasLegacyRun = $runValues.PSObject.Properties.Name -contains 'AIUsageMonitor'
    if (-not $hasLegacyRun -or $runValues.Gengchou -ne $quotedExe) {
        throw 'First start did not preserve the rollback Run value while verifying the new value.'
    }

    [IO.File]::WriteAllText((Join-Path $legacyApp 'user-note.txt'), 'keep', $utf8NoBom)
    $exitCode = Invoke-MigrationStart -Environment $baseEnvironment
    if ($exitCode -ne 0) {
        throw "Second migration start exited with $exitCode."
    }
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if ($state.stage -ne 'ready_seen' -or $state.settings_pending) {
        throw 'Unknown retired data did not keep migration at ready_seen.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $legacyApp 'user-note.txt') -PathType Leaf)) {
        throw 'Unknown retired data was removed instead of preserving it for review.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $legacyApp 'settings.json') -PathType Leaf)) {
        throw 'Cleanup removed the rollback settings source before the full inventory passed.'
    }
    $runValues = Get-ItemProperty -Path $runKey
    if ($runValues.PSObject.Properties.Name -notcontains 'AIUsageMonitor') {
        throw 'Cleanup removed the rollback Run value before the full inventory passed.'
    }

    Remove-Item -LiteralPath (Join-Path $legacyApp 'user-note.txt') -Force
    $exitCode = Invoke-MigrationStart -Environment $baseEnvironment
    if ($exitCode -ne 0) {
        throw "Third migration start exited with $exitCode."
    }
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if ($state.stage -ne 'complete' -or $state.settings_pending) {
        throw 'Third start did not commit migration stage complete after the unknown file was removed.'
    }
    $settingsHash = (Get-FileHash -LiteralPath (Join-Path $currentApp 'settings.json') -Algorithm SHA256).Hash
    if (-not $settingsHash.Equals(
        [string]$state.current_settings_sha256,
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw 'Completed migration state does not match the persisted settings hash.'
    }
    if (
        (Test-Path -LiteralPath $legacyApp) -or
        (Test-Path -LiteralPath $legacyLocal)
    ) {
        throw 'Completed cleanup left an owned retired data directory behind.'
    }
    if (
        -not (Test-Path -LiteralPath (Join-Path $upstreamApp 'settings.json') -PathType Leaf) -or
        -not (Test-Path -LiteralPath (Join-Path $upstreamLocal 'diagnose.log') -PathType Leaf)
    ) {
        throw 'Migration modified the source-only upstream application data.'
    }
    $runValues = Get-ItemProperty -Path $runKey
    if ($runValues.PSObject.Properties.Name -contains 'AIUsageMonitor') {
        throw 'Completed cleanup did not retire the owned old Run value.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $localAppdata 'Gengchou\diagnose.log') -PathType Leaf)) {
        throw 'Diagnostics did not move to the Gengchou local directory.'
    }

    $sourceReceipt = [string]$state.source_settings_sha256
    Remove-Item -LiteralPath $runKey -Recurse -Force
    $exitCode = Invoke-MigrationStart -Environment $baseEnvironment
    if ($exitCode -ne 0) {
        throw "Fourth migration start exited with $exitCode."
    }
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if (
        $state.stage -ne 'complete' -or
        [string]$state.source_settings_sha256 -ne $sourceReceipt -or
        $sourceReceipt -notmatch '^[0-9a-fA-F]{64}$'
    ) {
        throw 'Completed migration did not retain its source-settings receipt when the Run key was absent.'
    }

    $nameGateRoot = Join-Path $root 'filename-gate'
    $nameGateAppdata = Join-Path $nameGateRoot 'Roaming'
    $nameGateLocal = Join-Path $nameGateRoot 'Local'
    $nameGateLegacy = Join-Path $nameGateAppdata 'AIUsageMonitor'
    $nameGateState = Join-Path $nameGateAppdata 'Gengchou\migration-v2.2.4.json'
    $oldNameBinary = Join-Path $nameGateRoot 'ai-usage-monitor.exe'
    $canonicalBinary = Join-Path $nameGateRoot 'gengchou.exe'
    $nameGateRunSubkey = "$runSubkey-FilenameGate"
    $nameGateRunKey = "HKCU:\$nameGateRunSubkey"
    New-Item -ItemType Directory -Path $nameGateLegacy -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $nameGateLegacy 'settings.json'), $settings, $utf8NoBom)
    Copy-Item -LiteralPath $BinaryPath -Destination $oldNameBinary
    New-Item -ItemType Directory -Path $nameGateRunKey -Force | Out-Null
    New-ItemProperty -Path $nameGateRunKey -Name 'AIUsageMonitor' -Value ('"' + $oldNameBinary + '"') -PropertyType String -Force | Out-Null
    $nameGateEnvironment = @{
        'APPDATA' = $nameGateAppdata
        'LOCALAPPDATA' = $nameGateLocal
        'GENGCHOU_MIGRATION_TEST_RUN_KEY_PATH' = $nameGateRunSubkey
        'GENGCHOU_MIGRATION_TEST_MUTEX_SUFFIX' = "$nonce-filename"
    }
    foreach ($attempt in 1..2) {
        $exitCode = Invoke-MigrationStart -Environment $nameGateEnvironment -ExecutablePath $oldNameBinary
        if ($exitCode -ne 0) {
            throw "Old-filename migration start $attempt exited with $exitCode."
        }
    }
    $nameGate = Get-Content -LiteralPath $nameGateState -Raw | ConvertFrom-Json
    if ($nameGate.stage -ne 'ready_seen' -or
        -not (Test-Path -LiteralPath (Join-Path $nameGateLegacy 'settings.json') -PathType Leaf)) {
        throw 'The retired executable filename was allowed to complete cleanup.'
    }
    Copy-Item -LiteralPath $oldNameBinary -Destination $canonicalBinary
    $exitCode = Invoke-MigrationStart -Environment $nameGateEnvironment -ExecutablePath $canonicalBinary
    if ($exitCode -ne 0) {
        throw "Canonical-filename migration start exited with $exitCode."
    }
    $nameGate = Get-Content -LiteralPath $nameGateState -Raw | ConvertFrom-Json
    if ($nameGate.stage -ne 'complete' -or (Test-Path -LiteralPath $nameGateLegacy)) {
        throw 'Renaming to gengchou.exe did not release the cleanup gate.'
    }
    $nameGateRunValues = Get-ItemProperty -Path $nameGateRunKey
    if ($nameGateRunValues.PSObject.Properties.Name -contains 'AIUsageMonitor' -or
        ([string]$nameGateRunValues.Gengchou).Trim('"') -ne $canonicalBinary) {
        throw 'Renaming to gengchou.exe did not rewrite the current Run value and retire the old one.'
    }

    Write-Output 'Migration E2E passed: source priority, ready_seen, nonblocking unknown data, canonical filename gate, owned cleanup, upstream preservation, receipt, settings/cache, and Run migration verified.'
}
finally {
    if (Test-Path -LiteralPath $runKey) {
        Remove-Item -LiteralPath $runKey -Recurse -Force
    }
    if ($null -ne $nameGateRunKey -and (Test-Path -LiteralPath $nameGateRunKey)) {
        Remove-Item -LiteralPath $nameGateRunKey -Recurse -Force
    }
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}
