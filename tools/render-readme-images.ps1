# Renders every README preview image from the app's deterministic dump
# modes, so the documentation never depends on hand-taken screenshots.
#
#   powershell -ExecutionPolicy Bypass -File tools\render-readme-images.ps1
#
# Outputs (all PNG, written to .github/readme/):
#   detail-popup-en.png / detail-popup-zh.png   dark-theme detail popup, per language
#   widget-badges-dark.png / -light.png         taskbar widget, normal state
#   widget-badges-warn-dark.png                 taskbar widget, warning takeover
#   floating-rows-dark.png / -light.png         floating window, normal state
#   tray-icons-dark.png / -light.png            three provider tray icons composed
#                                               at taskbar size on the widget's
#                                               own background colour
#
# The tray strip mirrors the app's real 26px taskbar rendering: 64px dumps are
# downscaled with high-quality bicubic interpolation and composed with 12px
# margins/gaps; the background colour is sampled from the matching widget dump
# so every image in the README shares one palette.

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$exe = Join-Path $repoRoot 'target\debug\gengchou.exe'
$dumpRoot = Join-Path $repoRoot 'tmp\readme-dumps'
$outDir = Join-Path $repoRoot '.github\readme'

Push-Location $repoRoot
try {
    cargo build --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

    if (Test-Path $dumpRoot) { Remove-Item -Recurse -Force $dumpRoot }
    New-Item -ItemType Directory -Force $dumpRoot | Out-Null
    New-Item -ItemType Directory -Force $outDir | Out-Null

    # GUI-subsystem binary: Start-Process -Wait is required, the shell would
    # otherwise continue before the dump files exist.
    Start-Process -Wait -FilePath $exe -ArgumentList '--dump-detail-popup', "$dumpRoot\popup-en", 'en', 'dark'
    Start-Process -Wait -FilePath $exe -ArgumentList '--dump-detail-popup', "$dumpRoot\popup-zh", 'zh', 'dark'
    Start-Process -Wait -FilePath $exe -ArgumentList '--dump-widget', "$dumpRoot\widget"
    Start-Process -Wait -FilePath $exe -ArgumentList '--dump-tray-icons', "$dumpRoot\tray"

    function Convert-BmpToPng([string]$bmpPath, [string]$pngPath) {
        if (-not (Test-Path -LiteralPath $bmpPath)) { throw "missing dump: $bmpPath" }
        $img = [System.Drawing.Image]::FromFile($bmpPath)
        try { $img.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png) }
        finally { $img.Dispose() }
        Write-Host "wrote $pngPath"
    }

    Convert-BmpToPng "$dumpRoot\popup-en\detail-popup.bmp" "$outDir\detail-popup-en.png"
    Convert-BmpToPng "$dumpRoot\popup-zh\detail-popup.bmp" "$outDir\detail-popup-zh.png"
    Convert-BmpToPng "$dumpRoot\widget\badges-normal-dark.bmp" "$outDir\widget-badges-dark.png"
    Convert-BmpToPng "$dumpRoot\widget\badges-normal-light.bmp" "$outDir\widget-badges-light.png"
    Convert-BmpToPng "$dumpRoot\widget\badges-warn-dark.bmp" "$outDir\widget-badges-warn-dark.png"
    Convert-BmpToPng "$dumpRoot\widget\rows-normal-dark.bmp" "$outDir\floating-rows-dark.png"
    Convert-BmpToPng "$dumpRoot\widget\rows-normal-light.bmp" "$outDir\floating-rows-light.png"

    function Read-ArgbBmp([string]$bmpPath) {
        # The tray dumps are 32bpp BGRA with real per-pixel alpha (icons have
        # no rectangular plate; the taskbar shows through). System.Drawing
        # loads such BMPs as alpha-less 32bppRgb, so copy the raw pixels into
        # an ARGB bitmap instead.
        $bytes = [IO.File]::ReadAllBytes($bmpPath)
        $size = 64
        $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
        $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $bmp.PixelFormat)
        try {
            [System.Runtime.InteropServices.Marshal]::Copy($bytes, 54, $data.Scan0, $size * $size * 4)
        }
        finally { $bmp.UnlockBits($data) }
        return $bmp
    }

    function Compose-TrayStrip([string]$theme, [string]$pngPath) {
        # Background sampled from the same theme's widget dump keeps the tray
        # strip visually continuous with the widget previews.
        $sampleBmp = [System.Drawing.Bitmap]::FromFile("$dumpRoot\widget\badges-normal-$theme.bmp")
        try { $bg = $sampleBmp.GetPixel(0, 0) } finally { $sampleBmp.Dispose() }

        $iconSize = 26
        $margin = 12
        $gap = 12
        $icons = @("claude-72-48-$theme.bmp", "codex-42-12-$theme.bmp", "ag-single-60-$theme.bmp")
        $width = $margin * 2 + $iconSize * $icons.Count + $gap * ($icons.Count - 1)
        $height = $iconSize + $margin * 2

        $canvas = New-Object System.Drawing.Bitmap($width, $height)
        $g = [System.Drawing.Graphics]::FromImage($canvas)
        try {
            $g.Clear($bg)
            $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $x = $margin
            foreach ($name in $icons) {
                $icon = Read-ArgbBmp "$dumpRoot\tray\$name"
                try { $g.DrawImage($icon, $x, $margin, $iconSize, $iconSize) }
                finally { $icon.Dispose() }
                $x += $iconSize + $gap
            }
        }
        finally { $g.Dispose() }
        try { $canvas.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png) }
        finally { $canvas.Dispose() }
        Write-Host "wrote $pngPath"
    }

    Compose-TrayStrip 'dark' "$outDir\tray-icons-dark.png"
    Compose-TrayStrip 'light' "$outDir\tray-icons-light.png"

    Write-Host 'README images rendered.'
}
finally {
    Pop-Location
}
