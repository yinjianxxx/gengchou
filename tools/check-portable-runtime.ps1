[CmdletBinding()]
param(
    [string]$ExecutablePath = 'target\release\gengchou.exe'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
    throw "Executable not found: $ExecutablePath"
}

# PE import names are stored as ASCII strings. Reject runtime DLL families that
# make the portable executable depend on a separately installed MSVC runtime.
$contents = [Text.Encoding]::ASCII.GetString(
    [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $ExecutablePath))
)
$forbiddenPattern = '(?i)(?:VCRUNTIME[0-9_]*|MSVCP[0-9_]*|api-ms-win-crt-[a-z0-9-]+)\.dll'
$forbiddenImports = @(
    [regex]::Matches($contents, $forbiddenPattern) |
        ForEach-Object { $_.Value.ToLowerInvariant() } |
        Sort-Object -Unique
)

if ($forbiddenImports.Count -ne 0) {
    $details = 'External runtime imports: {0}.' -f ($forbiddenImports -join ', ')
    throw (
        "Portable runtime check failed. $details " +
        'Build the x64 MSVC target with target-feature=+crt-static.'
    )
}

Write-Output "Portable runtime check passed: $ExecutablePath"
