[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$retiredTokens = @(
    ('AI' + 'UsageMonitor'),
    ('AI Usage' + ' Monitor'),
    ('ai-usage-' + 'monitor'),
    ('AI' + 'UM_')
)
$grepPattern = ($retiredTokens | ForEach-Object { [regex]::Escape($_) }) -join '|'
$retiredPublisherPattern = [regex]::Escape(('yin' + 'jianxxx'))

$retiredPublisherResults = @(& git grep --untracked -n -i -E $retiredPublisherPattern -- .)
$retiredPublisherExitCode = $LASTEXITCODE
if ($retiredPublisherExitCode -notin @(0, 1)) {
    throw "Retired publisher identity scan failed with exit code $retiredPublisherExitCode."
}
if ($retiredPublisherResults.Count -ne 0) {
    $details = $retiredPublisherResults -join [Environment]::NewLine
    throw "Retired publisher identity remains in the current tree:`n$details"
}

$grepResults = @(& git grep --untracked -n -E $grepPattern -- .)
$grepExitCode = $LASTEXITCODE
if ($grepExitCode -notin @(0, 1)) {
    throw "git grep failed with exit code $grepExitCode."
}

$historicalReleaseNotes = @(
    '.github/release-notes/v2.0.1.md',
    '.github/release-notes/v2.1.0.md',
    '.github/release-notes/v2.2.0.md',
    '.github/release-notes/v2.2.1.md',
    '.github/release-notes/v2.2.2.md',
    '.github/release-notes/v2.2.3.md',
    '.github/release-notes/v2.2.4.md'
)
$chineseHeadingText = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String('IyMg6Ie06LCi5LiO6K645Y+v6K+B')
)
$englishHeadings = @(Select-String -LiteralPath 'README.md' -Pattern '^## Acknowledgements & license$' -Encoding UTF8)
$chineseHeadings = @(Select-String -LiteralPath 'README.zh-CN.md' -Pattern ('^' + [regex]::Escape($chineseHeadingText) + '$') -Encoding UTF8)
if ($englishHeadings.Count -ne 1 -or $chineseHeadings.Count -ne 1) {
    throw 'Each README must contain exactly one acknowledgements heading.'
}
$englishLastHeading = Select-String -LiteralPath 'README.md' -Pattern '^## ' -Encoding UTF8 |
    Select-Object -Last 1
$chineseLastHeading = Select-String -LiteralPath 'README.zh-CN.md' -Pattern '^## ' -Encoding UTF8 |
    Select-Object -Last 1
if (
    $englishHeadings[0].LineNumber -ne $englishLastHeading.LineNumber -or
    $chineseHeadings[0].LineNumber -ne $chineseLastHeading.LineNumber
) {
    throw 'The acknowledgements section must be the final level-two README section.'
}
$readmeSectionLines = @{
    'README.md' = $englishHeadings[0].LineNumber
    'README.zh-CN.md' = $chineseHeadings[0].LineNumber
}
$readmeHits = @{
    'README.md' = 0
    'README.zh-CN.md' = 0
}
$violations = [System.Collections.Generic.List[string]]::new()

$filePaths = @(& git ls-files --cached --others --exclude-standard)
if ($LASTEXITCODE -ne 0) {
    throw "git ls-files failed with exit code $LASTEXITCODE."
}
foreach ($filePath in $filePaths) {
    if ($filePath -match $grepPattern) {
        $violations.Add("Retired identity in file name: $filePath")
    }
}

foreach ($result in $grepResults) {
    if ($result -notmatch '^(?<path>.*?):(?<line>\d+):(?<text>.*)$') {
        $violations.Add("Unparseable grep result: $result")
        continue
    }

    $path = $Matches.path.Replace('\', '/')
    $lineNumber = [int]$Matches.line
    if ($path -eq 'PROVENANCE.md' -or $path -in $historicalReleaseNotes) {
        continue
    }

    if ($readmeSectionLines.ContainsKey($path)) {
        if ($lineNumber -gt $readmeSectionLines[$path]) {
            $readmeHits[$path]++
            continue
        }
    }

    $violations.Add($result)
}

foreach ($path in $readmeHits.Keys) {
    if ($readmeHits[$path] -ne 1) {
        $violations.Add(
            "$path must contain exactly one retired-name history line in its acknowledgements section; found $($readmeHits[$path])."
        )
    }
}

if ($violations.Count -ne 0) {
    $details = $violations -join [Environment]::NewLine
    throw "Retired identity allowlist check failed:`n$details"
}

Write-Output "Retired identity and publisher checks passed ($($grepResults.Count) historical lines)."
