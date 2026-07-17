[CmdletBinding()]
param(
    [string]$BinaryPath = '',
    [ValidateRange(5, 30)]
    [int]$TimeoutSeconds = 10
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $repoRoot 'target\debug\gengchou.exe'
}
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Debug binary not found: $BinaryPath"
}

$e2eRoot = Join-Path $repoRoot 'target\update-ready-inbound-e2e'
$testRoot = Join-Path $e2eRoot ([Guid]::NewGuid().ToString('N'))
$readyDir = Join-Path $testRoot 'ready'
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
            -WorkingDirectory $testRoot `
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
    $environment = @{
        'APPDATA' = (Join-Path $testRoot 'Roaming')
        'LOCALAPPDATA' = (Join-Path $testRoot 'Local')
        'GENGCHOU_UPDATE_TEST_READY_DIR' = $readyDir
        'GENGCHOU_UPDATE_READY_FILE' = $marker
    }
    $exitCode = Invoke-ReadyProbe -Environment $environment
    if ($exitCode -ne 0) {
        throw "Inbound readiness probe exited with $exitCode."
    }
    $content = [IO.File]::ReadAllText($marker, [Text.Encoding]::UTF8)
    if ($content -ne "Gengchou update ready`n") {
        throw 'Update readiness marker content was not preserved exactly.'
    }

    Write-Output 'Updater inbound E2E passed: the current marker was validated and written.'
}
finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $allowedPrefix = [IO.Path]::GetFullPath($e2eRoot).TrimEnd('\') + '\'
    if (-not $resolvedRoot.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing cleanup outside the updater inbound E2E root: $resolvedRoot"
    }
    if (Test-Path -LiteralPath $resolvedRoot) {
        Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
    }
}
