[CmdletBinding()]
param(
    [string]$HelperPath = '',
    [ValidateSet('Success', 'ChildExit')]
    [string]$Scenario = 'Success',
    [ValidateRange(10, 120)]
    [int]$TimeoutSeconds = 45
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

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

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
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
        $startInfo.Arguments = (
            $ArgumentList |
                ForEach-Object { ConvertTo-NativeArgument -Value $_ }
        ) -join ' '
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo

    # Windows PowerShell 5.1 can fail while materializing ProcessStartInfo's
    # environment dictionary when the host process contains case-variant keys
    # such as PATH and Path. Set only the fixture overrides around Start(), so
    # the child inherits them without enumerating or rewriting the full block.
    $previousEnvironment = @{}
    foreach ($name in $Environment.Keys) {
        $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable(
            $name,
            [string]$Environment[$name],
            'Process'
        )
    }
    try {
        if (-not $process.Start()) {
            throw "Unable to start process: $FilePath"
        }
    }
    finally {
        foreach ($name in $previousEnvironment.Keys) {
            [Environment]::SetEnvironmentVariable(
                $name,
                $previousEnvironment[$name],
                'Process'
            )
        }
    }
    return $process
}

function Wait-ForNonEmptyFile {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Description,
        [int]$TimeoutSeconds = 10
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            try {
                $content = Get-Content -LiteralPath $Path -Raw -Encoding utf8
                if (-not [string]::IsNullOrWhiteSpace($content)) {
                    return $content.Trim()
                }
            }
            catch {
                # The fixture may still have the file open; retry until timeout.
            }
        }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)

    throw "Timed out waiting for $Description at $Path"
}

function Stop-TestProcess {
    param([AllowNull()][System.Diagnostics.Process]$Process)

    if ($null -eq $Process) {
        return
    }
    try {
        if (-not $Process.HasExited) {
            $Process.Kill()
            [void]$Process.WaitForExit(5000)
        }
    }
    catch {
        Write-Warning "Unable to stop test process $($Process.Id): $_"
    }
}

function Stop-ProcessesAtExecutable {
    param([Parameter(Mandatory)][string]$ExecutablePath)

    $expected = [IO.Path]::GetFullPath($ExecutablePath)
    foreach ($candidate in Get-Process -ErrorAction SilentlyContinue) {
        try {
            $candidatePath = $candidate.Path
            if (
                $candidatePath -and
                [StringComparer]::OrdinalIgnoreCase.Equals(
                    [IO.Path]::GetFullPath($candidatePath),
                    $expected
                )
            ) {
                Stop-Process -Id $candidate.Id -Force -ErrorAction SilentlyContinue
            }
        }
        catch {
            # Protected or concurrently exiting processes do not belong to the
            # unique fixture path, so they can be ignored.
        }
    }
}

function Remove-TestDirectory {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$AllowedParent
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }

    $resolvedPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $resolvedParent = [IO.Path]::GetFullPath($AllowedParent).TrimEnd('\')
    if (
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($resolvedPath),
            $resolvedParent
        )
    ) {
        throw "Refusing to clean a path outside the updater E2E root: $resolvedPath"
    }

    for ($attempt = 1; $attempt -le 10; $attempt++) {
        try {
            Remove-Item -LiteralPath $resolvedPath -Recurse -Force
            return
        }
        catch {
            if ($attempt -eq 10) {
                throw
            }
            Start-Sleep -Milliseconds 200
        }
    }
}

if ($env:OS -ne 'Windows_NT') {
    throw 'The updater helper E2E test requires Windows.'
}

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([string]::IsNullOrWhiteSpace($HelperPath)) {
    $HelperPath = Join-Path $repoRoot 'target\debug\gengchou.exe'
}
$HelperPath = (Resolve-Path -LiteralPath $HelperPath).Path
if (-not (Test-Path -LiteralPath $HelperPath -PathType Leaf)) {
    throw "Updater helper binary not found: $HelperPath"
}

$rustc = Get-Command rustc -ErrorAction Stop
$fixtureDir = Join-Path $PSScriptRoot 'fixtures'
$e2eRoot = Join-Path $repoRoot 'target\updater-e2e'
$testRoot = Join-Path $e2eRoot ([guid]::NewGuid().ToString('N'))
$readyDir = Join-Path $testRoot 'ready'
$target = Join-Path $testRoot 'app-under-test.exe'
$source = Join-Path $testRoot 'vnext-source.exe'
$parentMarker = [IO.Path]::ChangeExtension($target, 'parent-ready')
$vnextPidFile = Join-Path $testRoot 'vnext.pid'
$readyPathFile = Join-Path $testRoot 'vnext-ready-path.txt'
$backup = "$target.old"

$parentProcess = $null
$helperProcess = $null
$vnextProcess = $null
$vnextPid = $null
$restartedParentProcess = $null

try {
    New-Item -ItemType Directory -Path $readyDir -Force | Out-Null

    & $rustc.Source (Join-Path $fixtureDir 'updater_parent.rs') --edition=2021 -C debuginfo=0 -o $target
    if ($LASTEXITCODE -ne 0) {
        throw "rustc failed to build updater_parent.rs ($LASTEXITCODE)."
    }

    $vnextFixture = if ($Scenario -eq 'Success') {
        'updater_vnext.rs'
    }
    else {
        'updater_vnext_fail.rs'
    }
    & $rustc.Source (Join-Path $fixtureDir $vnextFixture) --edition=2021 -C debuginfo=0 -o $source
    if ($LASTEXITCODE -ne 0) {
        throw "rustc failed to build $vnextFixture ($LASTEXITCODE)."
    }

    $oldHash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
    $expectedHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    if ($oldHash -eq $expectedHash) {
        throw 'Old and vNext fixtures unexpectedly have the same SHA-256 hash.'
    }

    $parentProcess = Start-HiddenProcess -FilePath $target -WorkingDirectory $testRoot
    $reportedParentPid = Wait-ForNonEmptyFile -Path $parentMarker -Description 'the old parent fixture'
    if ([int]$reportedParentPid -ne $parentProcess.Id) {
        throw "Parent fixture reported PID $reportedParentPid, expected $($parentProcess.Id)."
    }

    $helperEnvironment = @{
        'LOCALAPPDATA' = $testRoot
        'AIUM_UPDATE_TEST_READY_DIR' = $readyDir
        'AIUM_UPDATE_TEST_NO_UI' = '1'
    }
    $helperArguments = @(
        '--apply-update',
        $target,
        $source,
        $parentProcess.Id.ToString(),
        $expectedHash.ToLowerInvariant()
    )
    $helperProcess = Start-HiddenProcess -FilePath $HelperPath -ArgumentList $helperArguments -Environment $helperEnvironment -WorkingDirectory $testRoot

    # Give the helper time to open the parent process before ending it.
    Start-Sleep -Milliseconds 750
    if ($helperProcess.HasExited) {
        throw "Updater helper exited before the parent was released (code $($helperProcess.ExitCode))."
    }

    if ($Scenario -eq 'ChildExit') {
        Remove-Item -LiteralPath $parentMarker -Force
    }
    $parentProcess.Kill()
    if (-not $parentProcess.WaitForExit(5000)) {
        throw 'The old parent fixture did not exit within five seconds.'
    }

    if (-not $helperProcess.WaitForExit($TimeoutSeconds * 1000)) {
        $helperProcess.Kill()
        [void]$helperProcess.WaitForExit(5000)
        throw "Updater helper exceeded the $TimeoutSeconds-second test timeout."
    }
    $expectedHelperExit = if ($Scenario -eq 'Success') { 0 } else { 1 }
    if ($helperProcess.ExitCode -ne $expectedHelperExit) {
        throw "Updater helper exited with $($helperProcess.ExitCode); scenario $Scenario expected $expectedHelperExit."
    }

    if ($Scenario -eq 'Success') {
        $vnextPidText = Wait-ForNonEmptyFile -Path $vnextPidFile -Description 'the vNext PID sidecar'
        $vnextPid = [int]::Parse($vnextPidText)
        $vnextProcess = Get-Process -Id $vnextPid -ErrorAction Stop
        if ($vnextProcess.HasExited) {
            throw "The relaunched vNext process $vnextPid exited before hand-off verification."
        }
        if (
            -not [StringComparer]::OrdinalIgnoreCase.Equals(
                [IO.Path]::GetFullPath($vnextProcess.Path),
                [IO.Path]::GetFullPath($target)
            )
        ) {
            throw "PID $vnextPid is not running the replaced target executable."
        }

        $actualHash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
        if ($actualHash -ne $expectedHash) {
            throw "Target SHA-256 is $actualHash; expected vNext hash $expectedHash."
        }
        if ($actualHash -eq $oldHash) {
            throw 'Target still has the old parent fixture hash.'
        }
        if (Test-Path -LiteralPath $source) {
            throw "Updater left the source executable behind: $source"
        }

        $transactionLeftovers = @(
            Get-ChildItem -LiteralPath $testRoot -File -Recurse |
                Where-Object {
                    $_.Name -like '*.old' -or
                    $_.Name -like '*.new' -or
                    $_.Name -like '*.restore' -or
                    $_.Name -like '*.failed-*' -or
                    $_.Name -like '*.tmp'
                }
        )
        if ($transactionLeftovers.Count -ne 0) {
            throw (
                'Updater left transaction files behind: ' +
                (($transactionLeftovers.FullName) -join ', ')
            )
        }

        $readyPath = Wait-ForNonEmptyFile -Path $readyPathFile -Description 'the recorded update-ready path'
        $normalizedReadyPath = [IO.Path]::GetFullPath($readyPath)
        $normalizedReadyRoot = [IO.Path]::GetFullPath($readyDir).TrimEnd('\') + '\'
        if (-not $normalizedReadyPath.StartsWith(
            $normalizedReadyRoot,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw "Ready marker escaped the unique test directory: $normalizedReadyPath"
        }
        if (Test-Path -LiteralPath $normalizedReadyPath) {
            throw "Updater did not clean the consumed ready marker: $normalizedReadyPath"
        }

        $diagnoseLog = Join-Path $testRoot 'AIUsageMonitor\diagnose.log'
        if (-not (Test-Path -LiteralPath $diagnoseLog -PathType Leaf)) {
            throw 'Updater helper did not keep its diagnostic log inside the test directory.'
        }

        Write-Output (
            (
                'Updater E2E passed: old PID {0} exited; new PID {1} is alive; ' +
                'target hash, ready hand-off, and transaction cleanup verified.'
            ) -f $parentProcess.Id, $vnextPid
        )
    }
    else {
        $restartedPidText = Wait-ForNonEmptyFile -Path $parentMarker -Description 'the restarted old parent fixture'
        $restartedPid = [int]::Parse($restartedPidText)
        if ($restartedPid -eq $parentProcess.Id) {
            throw "Rollback marker still reports the original parent PID $restartedPid."
        }

        $restartedParentProcess = Get-Process -Id $restartedPid -ErrorAction Stop
        if ($restartedParentProcess.HasExited) {
            throw "The restored old process $restartedPid exited before rollback verification."
        }
        if (
            -not [StringComparer]::OrdinalIgnoreCase.Equals(
                [IO.Path]::GetFullPath($restartedParentProcess.Path),
                [IO.Path]::GetFullPath($target)
            )
        ) {
            throw "Rollback PID $restartedPid is not running the restored target executable."
        }

        $restoredHash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
        if ($restoredHash -ne $oldHash) {
            throw "Rollback target SHA-256 is $restoredHash; expected old hash $oldHash."
        }
        if (-not (Test-Path -LiteralPath $backup -PathType Leaf)) {
            throw "Rollback did not retain the recovery copy: $backup"
        }
        $backupHash = (Get-FileHash -LiteralPath $backup -Algorithm SHA256).Hash
        if ($backupHash -ne $oldHash) {
            throw "Recovery copy SHA-256 is $backupHash; expected old hash $oldHash."
        }

        $rollbackLeftovers = @(
            Get-ChildItem -LiteralPath $testRoot -File -Recurse |
                Where-Object {
                    $_.Name -like '*.new' -or
                    $_.Name -like '*.restore' -or
                    $_.Name -like '*.failed-*' -or
                    $_.Name -like '*.tmp'
                }
        )
        if ($rollbackLeftovers.Count -ne 0) {
            throw (
                'Rollback left an invalid transaction layout: ' +
                (($rollbackLeftovers.FullName) -join ', ')
            )
        }

        $unexpectedReadyMarkers = @(
            Get-ChildItem -LiteralPath $readyDir -File -Filter 'update-ready-*.marker'
        )
        if ($unexpectedReadyMarkers.Count -ne 0) {
            throw (
                'Failed child unexpectedly published a ready marker: ' +
                (($unexpectedReadyMarkers.FullName) -join ', ')
            )
        }

        $diagnoseLog = Join-Path $testRoot 'AIUsageMonitor\diagnose.log'
        if (-not (Test-Path -LiteralPath $diagnoseLog -PathType Leaf)) {
            throw 'Updater helper did not keep its diagnostic log inside the test directory.'
        }

        Write-Output (
            (
                'Updater ChildExit E2E passed: helper rejected the child, ' +
                'restored old hash, relaunched PID {0}, and retained verified .old.'
            ) -f $restartedPid
        )
    }
}
catch {
    $failureLog = Join-Path $testRoot 'AIUsageMonitor\diagnose.log'
    if (Test-Path -LiteralPath $failureLog -PathType Leaf) {
        Write-Warning 'Updater helper diagnostic log follows:'
        Get-Content -LiteralPath $failureLog -ErrorAction SilentlyContinue |
            ForEach-Object { Write-Warning $_ }
    }
    else {
        Write-Warning "Updater helper diagnostic log was not found at $failureLog"
    }
    throw
}
finally {
    Stop-TestProcess -Process $helperProcess
    Stop-TestProcess -Process $parentProcess
    Stop-TestProcess -Process $vnextProcess
    Stop-TestProcess -Process $restartedParentProcess
    Stop-ProcessesAtExecutable -ExecutablePath $target
    Start-Sleep -Milliseconds 100
    Remove-TestDirectory -Path $testRoot -AllowedParent $e2eRoot
}
