[CmdletBinding()]
param(
    [string]$ExecutablePath = '',
    [string]$ExpectedSha256 = '',
    [switch]$RequireOfficialHash,
    [switch]$RequireMigratedSource,
    [switch]$Json
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class GengchouWindowProbe
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr FindWindow(string lpClassName, string lpWindowName);
}
'@

$checks = [Collections.Generic.List[object]]::new()
function Add-Check {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][bool]$Passed,
        [Parameter(Mandatory)][string]$Detail
    )
    $checks.Add([pscustomobject]@{
        name = $Name
        passed = $Passed
        detail = $Detail
    })
}

function Get-NamedMutexState {
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

$appdata = [Environment]::GetFolderPath('ApplicationData')
$localAppdata = [Environment]::GetFolderPath('LocalApplicationData')
$currentApp = Join-Path $appdata 'Gengchou'
$statePath = Join-Path $currentApp 'migration-v2.2.4.json'
$settingsPath = Join-Path $currentApp 'settings.json'
$cachePath = Join-Path $currentApp 'usage-cache.json'

if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
    $processes = @(Get-Process -Name 'gengchou' -ErrorAction SilentlyContinue)
    if ($processes.Count -eq 1) {
        $ExecutablePath = $processes[0].Path
    }
    elseif ($processes.Count -eq 0) {
        Add-Check -Name 'running_process' -Passed $false -Detail 'gengchou.exe is not running.'
    }
    else {
        Add-Check -Name 'running_process' -Passed $false -Detail "Multiple gengchou.exe processes are running: $($processes.Id -join ', ')"
    }
}

if (-not [string]::IsNullOrWhiteSpace($ExecutablePath)) {
    $ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
    $exists = Test-Path -LiteralPath $ExecutablePath -PathType Leaf
    Add-Check -Name 'executable_exists' -Passed $exists -Detail $ExecutablePath
    if ($exists) {
        $version = (Get-Item -LiteralPath $ExecutablePath).VersionInfo
        $versionOk = $version.FileVersion -match '^2\.2\.4(?:\.0)?$' -and
            $version.ProductName -eq 'Gengchou' -and
            $version.OriginalFilename -eq 'gengchou.exe' -and
            [IO.Path]::GetFileName($ExecutablePath) -ieq 'gengchou.exe'
        Add-Check -Name 'executable_identity' -Passed $versionOk -Detail (
            "FileVersion={0}; ProductName={1}; OriginalFilename={2}; ActualFilename={3}" -f
                $version.FileVersion, $version.ProductName, $version.OriginalFilename,
                [IO.Path]::GetFileName($ExecutablePath)
        )
        $hash = (Get-FileHash -LiteralPath $ExecutablePath -Algorithm SHA256).Hash
        $hashExpectation = $ExpectedSha256
        if ([string]::IsNullOrWhiteSpace($hashExpectation)) {
            $manifestPath = Join-Path $PSScriptRoot 'SHA256SUMS'
            if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
                $assetNames = @([IO.Path]::GetFileName($ExecutablePath), 'gengchou.exe') |
                    Select-Object -Unique
                foreach ($line in Get-Content -LiteralPath $manifestPath) {
                    if ($line -match '^(?<hash>[0-9a-fA-F]{64})  (?<name>.+)$' -and
                        $Matches.name -in $assetNames) {
                        $hashExpectation = $Matches.hash
                        break
                    }
                }
            }
        }
        $hashOk = $hash.Length -eq 64
        if (-not [string]::IsNullOrWhiteSpace($hashExpectation)) {
            $hashOk = $hashOk -and $hashExpectation -match '^[0-9a-fA-F]{64}$' -and
                $hash.Equals($hashExpectation, [StringComparison]::OrdinalIgnoreCase)
        }
        elseif ($RequireOfficialHash) {
            $hashOk = $false
        }
        Add-Check -Name 'executable_sha256' -Passed $hashOk -Detail (
            "actual={0}; expected={1}" -f $hash,
                $(if ([string]::IsNullOrWhiteSpace($hashExpectation)) { 'not supplied' } else { $hashExpectation })
        )
    }
}

foreach ($currentDir in @($currentApp, (Join-Path $localAppdata 'Gengchou'))) {
    if (Test-Path -LiteralPath $currentDir -PathType Container) {
        $item = Get-Item -LiteralPath $currentDir -Force
        $safeDirectory = ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0
        Add-Check -Name 'current_directory_not_reparse_point' -Passed $safeDirectory -Detail $currentDir
    }
    else {
        Add-Check -Name 'current_directory_exists' -Passed $false -Detail $currentDir
    }
}

$state = $null
try {
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    $stateOk = $state.schema_version -eq 1 -and
        $state.stage -eq 'complete' -and
        -not [bool]$state.settings_pending -and
        ([string]$state.current_settings_sha256) -match '^[0-9a-fA-F]{64}$'
    if ($RequireMigratedSource) {
        $stateOk = $stateOk -and
            ([string]$state.source_settings_sha256) -match '^[0-9a-fA-F]{64}$'
    }
    Add-Check -Name 'migration_state' -Passed $stateOk -Detail (
        "stage={0}; settings_pending={1}; source_sha256={2}" -f
            $state.stage, $state.settings_pending, $state.source_settings_sha256
    )
}
catch {
    Add-Check -Name 'migration_state' -Passed $false -Detail $_.Exception.Message
}

try {
    $settings = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json
    $settingsOk = $null -ne $settings -and $settings.poll_interval_ms -gt 0
    Add-Check -Name 'current_settings' -Passed $settingsOk -Detail $settingsPath
    $settingsHash = (Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash
    $stateSettingsHash = if ($null -ne $state) {
        [string]$state.current_settings_sha256
    }
    else {
        ''
    }
    Add-Check -Name 'settings_hash_audit' -Passed $true -Detail (
        "file={0}; startup_state={1}; equal={2}" -f
            $settingsHash,
            $stateSettingsHash,
            $settingsHash.Equals($stateSettingsHash, [StringComparison]::OrdinalIgnoreCase)
    )
}
catch {
    Add-Check -Name 'current_settings' -Passed $false -Detail $_.Exception.Message
}

if (Test-Path -LiteralPath $cachePath -PathType Leaf) {
    try {
        $cache = Get-Content -LiteralPath $cachePath -Raw | ConvertFrom-Json
        Add-Check -Name 'current_cache' -Passed ($cache.saved_unix -gt 0) -Detail $cachePath
    }
    catch {
        Add-Check -Name 'current_cache' -Passed $false -Detail $_.Exception.Message
    }
}
else {
    Add-Check -Name 'current_cache' -Passed $true -Detail 'No cache is present; a fresh poll is allowed.'
}

$legacyDirs = @(
    (Join-Path $appdata 'AIUsageMonitor'),
    (Join-Path $localAppdata 'AIUsageMonitor')
)
foreach ($legacyDir in $legacyDirs) {
    $absent = -not (Test-Path -LiteralPath $legacyDir)
    Add-Check -Name 'legacy_directory_absent' -Passed $absent -Detail $legacyDir
    if (-not [string]::IsNullOrWhiteSpace($ExecutablePath)) {
        $prefix = [IO.Path]::GetFullPath($legacyDir).TrimEnd('\') + '\'
        $outside = -not $ExecutablePath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
        Add-Check -Name 'executable_outside_legacy_directory' -Passed $outside -Detail $legacyDir
    }
}

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$runValues = if (Test-Path -LiteralPath $runKey) {
    Get-ItemProperty -Path $runKey
}
else {
    [pscustomobject]@{}
}
foreach ($legacyName in @('AIUsageMonitor')) {
    $absent = $runValues.PSObject.Properties.Name -notcontains $legacyName
    Add-Check -Name 'legacy_run_value_absent' -Passed $absent -Detail $legacyName
}
if ($runValues.PSObject.Properties.Name -contains 'Gengchou') {
    $configured = ([string]$runValues.Gengchou).Trim().Trim('"')
    $matchesExe = -not [string]::IsNullOrWhiteSpace($ExecutablePath) -and
        [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetFullPath($configured),
            $ExecutablePath
        )
    Add-Check -Name 'current_run_value' -Passed $matchesExe -Detail $configured
}
else {
    Add-Check -Name 'current_run_value' -Passed $true -Detail 'Startup is disabled; no Gengchou Run value is expected.'
}

$currentBroadcast = [GengchouWindowProbe]::FindWindow('GengchouBroadcast', $null)
$legacyBroadcast = [GengchouWindowProbe]::FindWindow('AIUsageMonitorBroadcast', $null)
Add-Check -Name 'current_broadcast_window' -Passed ($currentBroadcast -ne [IntPtr]::Zero) -Detail "HWND=$currentBroadcast"
Add-Check -Name 'legacy_broadcast_window_absent' -Passed ($legacyBroadcast -eq [IntPtr]::Zero) -Detail "HWND=$legacyBroadcast"
Add-Check -Name 'current_mutex' -Passed (Get-NamedMutexState -Name 'Global\Gengchou') -Detail 'Global\Gengchou'
Add-Check -Name 'legacy_bridge_mutex' -Passed (Get-NamedMutexState -Name 'Global\AIUsageMonitor') -Detail 'Expected only in the v2.2.4 bridge.'

$failed = @($checks | Where-Object { -not $_.passed })
$result = [pscustomobject]@{
    passed = $failed.Count -eq 0
    computer = $env:COMPUTERNAME
    user_sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    executable = $ExecutablePath
    checks = $checks
}

if ($Json) {
    $result | ConvertTo-Json -Depth 6
}
else {
    $checks | Format-Table -AutoSize name, passed, detail
    if ($result.passed) {
        Write-Host 'PASS: v2.2.4 migration is complete.' -ForegroundColor Green
    }
    else {
        Write-Host "FAIL: $($failed.Count) migration check(s) need attention." -ForegroundColor Red
    }
}

if (-not $result.passed) {
    exit 1
}
