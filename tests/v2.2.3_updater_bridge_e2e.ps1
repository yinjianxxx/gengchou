[CmdletBinding()]
param(
    [string]$LegacyHelperPath = '',
    [switch]$AllowRealUserProfileWrite,
    [ValidateRange(10, 120)]
    [int]$TimeoutSeconds = 45
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($env:OS -ne 'Windows_NT') {
    throw 'The v2.2.3 updater bridge E2E test requires Windows.'
}
if (-not $AllowRealUserProfileWrite) {
    throw 'This test uses the released helper and therefore writes a unique marker under the real user profile; use an isolated account and pass -AllowRealUserProfileWrite.'
}

$expectedSha256 = '6899cdad3eab8da4c30a1380e3bdc0623c9c31b8d19dae3477b630546328d9a3'
$releaseUrl = 'https://github.com/yinjianxxx/gengchou/releases/download/v2.2.3/gengchou.exe'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$downloadRoot = Join-Path $repoRoot 'target\v2.2.3-updater-bridge-e2e'
$testRoot = Join-Path $downloadRoot ([guid]::NewGuid().ToString('N'))
$ownsTestRoot = $false

try {
    if ([string]::IsNullOrWhiteSpace($LegacyHelperPath)) {
        New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
        $ownsTestRoot = $true
        $LegacyHelperPath = Join-Path $testRoot 'v2.2.3-gengchou.exe'
        & curl.exe --fail --location --silent --show-error `
            --output $LegacyHelperPath $releaseUrl
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to download the pinned v2.2.3 updater (curl exit $LASTEXITCODE)."
        }
    }

    $LegacyHelperPath = (Resolve-Path -LiteralPath $LegacyHelperPath).Path
    $actualSha256 = (Get-FileHash -LiteralPath $LegacyHelperPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $expectedSha256) {
        throw "v2.2.3 updater SHA-256 is $actualSha256; expected $expectedSha256."
    }

    foreach ($scenario in @('Success', 'ChildExit')) {
        & (Join-Path $PSScriptRoot 'updater_e2e.ps1') `
            -HelperPath $LegacyHelperPath `
            -Scenario $scenario `
            -Protocol Legacy `
            -AllowRealUserProfileWrite `
            -TimeoutSeconds $TimeoutSeconds
        if ($LASTEXITCODE -ne 0) {
            throw "v2.2.3 updater bridge scenario $scenario failed with exit code $LASTEXITCODE."
        }
    }

    Write-Output 'v2.2.3 released updater bridge E2E passed with the pinned official SHA-256.'
}
finally {
    if ($ownsTestRoot -and (Test-Path -LiteralPath $testRoot)) {
        $resolvedTestRoot = [IO.Path]::GetFullPath($testRoot).TrimEnd('\')
        $resolvedDownloadRoot = [IO.Path]::GetFullPath($downloadRoot).TrimEnd('\')
        if (
            [StringComparer]::OrdinalIgnoreCase.Equals(
                [IO.Path]::GetDirectoryName($resolvedTestRoot),
                $resolvedDownloadRoot
            )
        ) {
            Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
        }
        else {
            Write-Warning "Refusing to clean an unexpected test directory: $resolvedTestRoot"
        }
    }
}
