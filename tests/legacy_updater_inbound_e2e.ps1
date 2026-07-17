[CmdletBinding()]
param(
    [string]$BinaryPath = '',
    [ValidateRange(5, 30)]
    [int]$TimeoutSeconds = 10
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

$root = Join-Path ([IO.Path]::GetTempPath()) (
    'gengchou-legacy-inbound-{0}-{1}' -f $PID, [Guid]::NewGuid().ToString('N')
)
$readyDir = Join-Path $root 'legacy-ready'
$marker = Join-Path $readyDir 'update-ready-e2e.marker'
New-Item -ItemType Directory -Path $readyDir -Force | Out-Null

function Invoke-ReadyProbe {
    param([hashtable]$Environment)

    $previous = @{}
    foreach ($name in $Environment.Keys) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, [string]$Environment[$name], 'Process')
    }
    try {
        $process = Start-Process -FilePath $BinaryPath `
            -ArgumentList '--confirm-update-ready-test' `
            -WorkingDirectory $root `
            -WindowStyle Hidden `
            -PassThru
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            throw 'Update readiness probe timed out.'
        }
        return $process.ExitCode
    }
    finally {
        foreach ($name in $previous.Keys) {
            [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
        }
    }
}

try {
    $baseEnvironment = @{
        'APPDATA' = (Join-Path $root 'Roaming')
        'LOCALAPPDATA' = (Join-Path $root 'Local')
        'AIUM_UPDATE_TEST_READY_DIR' = $readyDir
        'AIUM_UPDATE_READY_FILE' = $marker
    }
    $exitCode = Invoke-ReadyProbe -Environment $baseEnvironment
    if ($exitCode -ne 0) {
        throw "Legacy inbound readiness probe exited with $exitCode."
    }
    $content = [IO.File]::ReadAllText($marker, [Text.Encoding]::UTF8)
    if ($content -ne "AIUM update ready`n") {
        throw 'Legacy helper marker content was not preserved exactly.'
    }

    Remove-Item -LiteralPath $marker -Force
    $dualEnvironment = @{}
    foreach ($entry in $baseEnvironment.GetEnumerator()) {
        $dualEnvironment[$entry.Key] = $entry.Value
    }
    $dualEnvironment['GENGCHOU_UPDATE_READY_FILE'] = $marker
    $dualEnvironment['GENGCHOU_UPDATE_TEST_READY_DIR'] = $readyDir
    $exitCode = Invoke-ReadyProbe -Environment $dualEnvironment
    if ($exitCode -eq 0 -or (Test-Path -LiteralPath $marker)) {
        throw 'Dual readiness variables were not rejected fail-closed.'
    }

    Write-Output 'Legacy updater inbound E2E passed: old marker accepted; dual env rejected.'
}
finally {
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}
