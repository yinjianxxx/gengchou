[CmdletBinding()]
param(
    [string]$LegacyHelperPath = '',
    [string]$CurrentBinaryPath = '',
    [switch]$AllowInteractiveDesktopAndRealProfileWrite,
    [ValidateRange(15, 120)]
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($env:OS -ne 'Windows_NT') {
    throw 'The real v2.2.3 to v2.2.4 integration test requires Windows.'
}
if (-not $AllowInteractiveDesktopAndRealProfileWrite) {
    throw 'This test launches the actual desktop application and uses one uniquely named marker under the real user profile. Close Gengchou, then pass -AllowInteractiveDesktopAndRealProfileWrite.'
}

$expectedLegacySha256 = '6899cdad3eab8da4c30a1380e3bdc0623c9c31b8d19dae3477b630546328d9a3'
$legacyReleaseUrl = 'https://github.com/yinjianxxx/gengchou/releases/download/v2.2.3/gengchou.exe'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([string]::IsNullOrWhiteSpace($CurrentBinaryPath)) {
    $cargo = Get-Command cargo -ErrorAction Stop
    & $cargo.Source build --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --locked failed ($LASTEXITCODE)."
    }
    $CurrentBinaryPath = Join-Path $repoRoot 'target\debug\gengchou.exe'
}
$CurrentBinaryPath = (Resolve-Path -LiteralPath $CurrentBinaryPath).Path
$currentVersion = (Get-Item -LiteralPath $CurrentBinaryPath).VersionInfo
if ($currentVersion.FileVersion -notmatch '^2\.2\.4(?:\.0)?$' -or
    $currentVersion.ProductName -ne 'Gengchou' -or
    $currentVersion.OriginalFilename -ne 'gengchou.exe') {
    throw "Current integration binary has the wrong identity: $CurrentBinaryPath"
}

function ConvertTo-NativeArgument {
    param([Parameter(Mandatory)][string]$Value)

    if ($Value.Contains('"')) {
        throw 'Test paths containing a double quote are not supported.'
    }
    if ($Value -match '\s') {
        return '"' + $Value + '"'
    }
    return $Value
}

function Start-HiddenProcess {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [hashtable]$Environment = @{},
        [Parameter(Mandatory)][string]$WorkingDirectory
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    if ($startInfo.PSObject.Properties.Name -contains 'ArgumentList') {
        foreach ($argument in $ArgumentList) {
            $startInfo.ArgumentList.Add($argument)
        }
    }
    else {
        $startInfo.Arguments = ($ArgumentList | ForEach-Object {
            ConvertTo-NativeArgument -Value $_
        }) -join ' '
    }
    foreach ($entry in $Environment.GetEnumerator()) {
        $startInfo.Environment[[string]$entry.Key] = [string]$entry.Value
    }
    return [Diagnostics.Process]::Start($startInfo)
}

function Wait-ForFileContent {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][scriptblock]$Predicate,
        [Parameter(Mandatory)][string]$Description
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            try {
                $content = Get-Content -LiteralPath $Path -Raw
                if (& $Predicate $content) {
                    return $content
                }
            }
            catch {
                # Atomic replacement can briefly race this read; retry.
            }
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Description at $Path."
}

function Find-TargetProcess {
    param([Parameter(Mandatory)][string]$Path)

    $expected = [IO.Path]::GetFullPath($Path)
    foreach ($process in Get-Process -ErrorAction SilentlyContinue) {
        try {
            if ([StringComparer]::OrdinalIgnoreCase.Equals(
                [IO.Path]::GetFullPath($process.Path),
                $expected
            )) {
                return $process
            }
        }
        catch {
            # Access to unrelated protected processes is not required.
        }
    }
    return $null
}

function Wait-ForTargetProcess {
    param([Parameter(Mandatory)][string]$Path)

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $process = Find-TargetProcess -Path $Path
        if ($null -ne $process -and -not $process.HasExited) {
            return $process
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for the actual v2.2.4 process at $Path."
}

function Test-NamedMutex {
    param([Parameter(Mandatory)][string]$Name)

    try {
        $mutex = [Threading.Mutex]::OpenExisting($Name)
        $mutex.Dispose()
        return $true
    }
    catch [Threading.WaitHandleCannotBeOpenedException] {
        return $false
    }
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class GengchouRealBridgeProbe
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindWindow(string className, string windowName);
}
'@

if (@(Get-Process -Name 'gengchou' -ErrorAction SilentlyContinue).Count -ne 0 -or
    (Test-NamedMutex -Name 'Global\AIUsageMonitor') -or
    (Test-NamedMutex -Name 'Global\Gengchou')) {
    throw 'Close every running v2.2.3/v2.2.4 Gengchou instance before this real integration test.'
}

$nonce = [Guid]::NewGuid().ToString('N')
$testParent = Join-Path $repoRoot 'target\real-bridge-e2e'
$testRoot = Join-Path $testParent $nonce
$appdata = Join-Path $testRoot 'Roaming'
$localAppdata = Join-Path $testRoot 'Local'
$legacyApp = Join-Path $appdata 'AIUsageMonitor'
$legacyLocal = Join-Path $localAppdata 'AIUsageMonitor'
$upstreamApp = Join-Path $appdata 'ClaudeCodexUsageMonitor'
$currentApp = Join-Path $appdata 'Gengchou'
$statePath = Join-Path $currentApp 'migration-v2.2.4.json'
$target = Join-Path $testRoot 'gengchou.exe'
$source = Join-Path $testRoot 'v2.2.4-source.exe'
$parentMarker = [IO.Path]::ChangeExtension($target, 'parent-ready')
$runSubkey = "Software\GengchouTests\RealBridge-$PID-$nonce"
$runKey = "HKCU:\$runSubkey"
$realLegacyUpdates = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'AIUsageMonitor\updates'
$realLegacyLocal = Split-Path $realLegacyUpdates -Parent
$realLegacyLocalExisted = Test-Path -LiteralPath $realLegacyLocal -PathType Container
$realLegacyUpdatesExisted = Test-Path -LiteralPath $realLegacyUpdates -PathType Container
$utf8NoBom = [Text.UTF8Encoding]::new($false)

$parentProcess = $null
$helperProcess = $null
$firstChild = $null
$secondChild = $null
$downloadedHelper = $false

try {
    New-Item -ItemType Directory -Path $legacyApp, $legacyLocal, $upstreamApp, $testRoot -Force | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $legacyApp 'settings.json'),
        '{"tray_offset":37,"poll_interval_ms":300000,"language":"en","show_claude_code":false,"show_codex":false,"show_antigravity":false}',
        $utf8NoBom
    )
    [IO.File]::WriteAllText(
        (Join-Path $legacyApp 'usage-cache.json'),
        ('{{"saved_unix":{0},"codex":{{"windows":[{{"percent":23.0,"duration_seconds":18000}}]}}}}' -f [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()),
        $utf8NoBom
    )
    [IO.File]::WriteAllText((Join-Path $legacyLocal 'diagnose.log'), "old log`n", $utf8NoBom)
    [IO.File]::WriteAllText((Join-Path $upstreamApp 'settings.json'), '{"tray_offset":99}', $utf8NoBom)

    if ([string]::IsNullOrWhiteSpace($LegacyHelperPath)) {
        $LegacyHelperPath = Join-Path $testRoot 'v2.2.3-helper.exe'
        & curl.exe --fail --location --silent --show-error --output $LegacyHelperPath $legacyReleaseUrl
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to download the pinned v2.2.3 helper (curl exit $LASTEXITCODE)."
        }
        $downloadedHelper = $true
    }
    $LegacyHelperPath = (Resolve-Path -LiteralPath $LegacyHelperPath).Path
    $legacyHash = (Get-FileHash -LiteralPath $LegacyHelperPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($legacyHash -ne $expectedLegacySha256) {
        throw "v2.2.3 helper SHA-256 is $legacyHash; expected $expectedLegacySha256."
    }

    Copy-Item -LiteralPath $CurrentBinaryPath -Destination $source
    $expectedCurrentHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $rustc = Get-Command rustc -ErrorAction Stop
    & $rustc.Source (Join-Path $PSScriptRoot 'fixtures\updater_parent.rs') --edition=2021 -C debuginfo=0 -o $target
    if ($LASTEXITCODE -ne 0) {
        throw "rustc failed to build the parent fixture ($LASTEXITCODE)."
    }
    New-Item -ItemType Directory -Path $runKey -Force | Out-Null
    $quotedTarget = '"' + $target + '"'
    New-ItemProperty -Path $runKey -Name 'AIUsageMonitor' -Value $quotedTarget -PropertyType String -Force | Out-Null

    $baseEnvironment = @{
        'APPDATA' = $appdata
        'LOCALAPPDATA' = $localAppdata
        'GENGCHOU_MIGRATION_TEST_RUN_KEY_PATH' = $runSubkey
        'AIUM_UPDATE_TEST_READY_DIR' = $realLegacyUpdates
        'AIUM_UPDATE_TEST_NO_UI' = '1'
        'GENGCHOU_UPDATE_TEST_NO_UI' = '1'
    }
    $parentProcess = Start-HiddenProcess -FilePath $target -WorkingDirectory $testRoot
    [void](Wait-ForFileContent -Path $parentMarker -Description 'the parent fixture PID' -Predicate {
        param($content) $content.Trim() -eq [string]$parentProcess.Id
    })

    $helperArguments = @(
        '--apply-update',
        $target,
        $source,
        $parentProcess.Id.ToString(),
        $expectedCurrentHash.ToLowerInvariant()
    )
    $helperProcess = Start-HiddenProcess -FilePath $LegacyHelperPath -ArgumentList $helperArguments -Environment $baseEnvironment -WorkingDirectory $testRoot
    Start-Sleep -Milliseconds 750
    if ($helperProcess.HasExited) {
        throw "The v2.2.3 helper exited before the parent was released (code $($helperProcess.ExitCode))."
    }
    $parentProcess.Kill()
    if (-not $parentProcess.WaitForExit(5000)) {
        throw 'The parent fixture did not exit within five seconds.'
    }
    if (-not $helperProcess.WaitForExit($TimeoutSeconds * 1000)) {
        throw 'The v2.2.3 helper timed out while launching the actual v2.2.4 application.'
    }
    if ($helperProcess.ExitCode -ne 0) {
        throw "The v2.2.3 helper rejected the actual v2.2.4 application (exit $($helperProcess.ExitCode))."
    }

    $firstChild = Wait-ForTargetProcess -Path $target
    $firstStateJson = Wait-ForFileContent -Path $statePath -Description 'migration state ready_seen' -Predicate {
        param($content)
        try { ($content | ConvertFrom-Json).stage -eq 'ready_seen' } catch { $false }
    }
    $firstState = $firstStateJson | ConvertFrom-Json
    $migrated = Get-Content -LiteralPath (Join-Path $currentApp 'settings.json') -Raw | ConvertFrom-Json
    if ($migrated.tray_offset -ne 37 -or $firstState.source_settings_sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'The actual first launch did not preserve the v2.2.3 settings sentinel and source receipt.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $legacyApp 'settings.json') -PathType Leaf)) {
        throw 'The helper-launched first start removed its migration source too early.'
    }
    [void](Wait-ForFileContent -Path (Join-Path $localAppdata 'Gengchou\diagnose.log') -Description 'the cache migration receipt' -Predicate {
        param($content)
        $content -match 'fresh usage cache migrated sha256=[0-9a-f]{64}'
    })
    $firstRunValues = Get-ItemProperty -Path $runKey
    if ($firstRunValues.AIUsageMonitor -ne $quotedTarget -or $firstRunValues.Gengchou -ne $quotedTarget) {
        throw 'The actual first launch did not retain the rollback Run value while creating the new one.'
    }
    if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne $expectedCurrentHash -or
        (Test-Path -LiteralPath $source)) {
        throw 'The helper transaction did not leave exactly the current v2.2.4 target.'
    }
    if (-not (Test-NamedMutex -Name 'Global\AIUsageMonitor') -or
        -not (Test-NamedMutex -Name 'Global\Gengchou') -or
        [GengchouRealBridgeProbe]::FindWindow('GengchouBroadcast', $null) -eq [IntPtr]::Zero) {
        throw 'The actual first launch did not expose both bridge mutexes and the new broadcast window.'
    }

    $firstChild.Kill()
    [void]$firstChild.WaitForExit(5000)
    $firstChild = $null
    $secondChild = Start-HiddenProcess -FilePath $target -Environment $baseEnvironment -WorkingDirectory $testRoot
    [void](Wait-ForFileContent -Path $statePath -Description 'migration state complete' -Predicate {
        param($content)
        try { ($content | ConvertFrom-Json).stage -eq 'complete' } catch { $false }
    })
    if ($secondChild.HasExited) {
        throw "The actual second v2.2.4 launch exited unexpectedly (code $($secondChild.ExitCode))."
    }
    if ((Test-Path -LiteralPath $legacyApp) -or (Test-Path -LiteralPath $legacyLocal)) {
        throw 'The actual second launch did not clean the owned retired directories.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $upstreamApp 'settings.json') -PathType Leaf)) {
        throw 'The actual second launch modified the source-only upstream directory.'
    }
    $secondRunValues = Get-ItemProperty -Path $runKey
    if ($secondRunValues.PSObject.Properties.Name -contains 'AIUsageMonitor' -or
        $secondRunValues.Gengchou -ne $quotedTarget) {
        throw 'The actual second launch did not retire only the old Run value.'
    }
    $leftoverMarkers = @(Get-ChildItem -LiteralPath $realLegacyUpdates -Filter "update-ready-$($helperProcess.Id)-*.marker" -File -ErrorAction SilentlyContinue)
    if ($leftoverMarkers.Count -ne 0) {
        throw "The v2.2.3 helper left its real-profile marker behind: $($leftoverMarkers.FullName -join ', ')"
    }

    Write-Output 'Real bridge E2E passed: pinned v2.2.3 helper launched the actual v2.2.4 app, observed ready_seen/new identity/dual mutexes, and the second launch completed owned cleanup.'
}
finally {
    foreach ($process in @($firstChild, $secondChild, $helperProcess, $parentProcess)) {
        if ($null -ne $process) {
            try {
                if (-not $process.HasExited) {
                    $process.Kill()
                    [void]$process.WaitForExit(5000)
                }
            }
            catch {}
        }
    }
    if (Test-Path -LiteralPath $runKey) {
        Remove-Item -LiteralPath $runKey -Recurse -Force
    }
    if ($null -ne $helperProcess -and (Test-Path -LiteralPath $realLegacyUpdates -PathType Container)) {
        Get-ChildItem -LiteralPath $realLegacyUpdates -Filter "update-ready-$($helperProcess.Id)-*.marker" -File -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
    }
    if (-not $realLegacyUpdatesExisted -and (Test-Path -LiteralPath $realLegacyUpdates -PathType Container)) {
        if (@(Get-ChildItem -LiteralPath $realLegacyUpdates -Force).Count -eq 0) {
            Remove-Item -LiteralPath $realLegacyUpdates -Force
        }
    }
    if (-not $realLegacyLocalExisted -and (Test-Path -LiteralPath $realLegacyLocal -PathType Container)) {
        if (@(Get-ChildItem -LiteralPath $realLegacyLocal -Force).Count -eq 0) {
            Remove-Item -LiteralPath $realLegacyLocal -Force
        }
    }
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedRoot = [IO.Path]::GetFullPath($testRoot).TrimEnd('\')
        $resolvedParent = [IO.Path]::GetFullPath($testParent).TrimEnd('\')
        if ([StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetDirectoryName($resolvedRoot), $resolvedParent)) {
            Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
        }
    }
}
