#requires -Version 7.0
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$renderer = Join-Path $root 'tools\generate-icon.ps1'
$temporary = Join-Path ([IO.Path]::GetTempPath()) "github-adapter-icon-tests-$([Guid]::NewGuid().ToString('N'))"

Add-Type -AssemblyName System.Drawing

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
    $script:assertions++
}

$script:assertions = 0
$sizes = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)

function Assert-Fails([scriptblock]$Action, [string]$Pattern) {
    $message = ''
    $failed = $false
    try { & $Action | Out-Null } catch { $failed = $true; $message = $_.Exception.Message }
    Assert-True ($failed -and $message -like $Pattern) "Expected failure '$Pattern', got '$message'."
}

function Get-Hashes([string]$Directory) {
    return @(Get-ChildItem -LiteralPath $Directory -File -Recurse | Sort-Object FullName | ForEach-Object {
        "$($_.FullName):$((Get-FileHash -LiteralPath $_.FullName).Hash):$($_.LastWriteTimeUtc.Ticks)"
    }) -join '|'
}

function Assert-Frames([string]$Path, [Drawing.Bitmap]$Expected = $null) {
    [byte[]]$bytes = [IO.File]::ReadAllBytes($Path)
    Assert-True ($bytes.Length -gt 166) "$Path is too small."
    Assert-True ([BitConverter]::ToUInt16($bytes, 0) -eq 0 -and [BitConverter]::ToUInt16($bytes, 2) -eq 1 -and [BitConverter]::ToUInt16($bytes, 4) -eq 10) "$Path needs ten frames."
    $nextOffset = 166
    foreach ($index in 0..9) {
        $entry = 6 + 16 * $index
        $size = $sizes[$index]
        $encoded = $size % 256
        Assert-True ($bytes[$entry] -eq $encoded -and $bytes[$entry + 1] -eq $encoded -and $bytes[$entry + 2] -eq 0 -and $bytes[$entry + 3] -eq 0 -and [BitConverter]::ToUInt16($bytes, $entry + 4) -eq 1 -and [BitConverter]::ToUInt16($bytes, $entry + 6) -eq 32) "Invalid $size frame header."
        $length = [BitConverter]::ToUInt32($bytes, $entry + 8)
        $offset = [BitConverter]::ToUInt32($bytes, $entry + 12)
        Assert-True ($offset -eq $nextOffset -and $length -gt 33 -and $offset + $length -le $bytes.Length) 'Invalid frame offsets.'
        [byte[]]$png = $bytes[$offset..($offset + $length - 1)]
        Assert-True ([Convert]::ToHexString($png[0..7]) -eq '89504E470D0A1A0A' -and $png[24] -eq 8 -and $png[25] -eq 6) 'Frame must be 8-bit RGBA PNG.'
        $stream = [IO.MemoryStream]::new($png, $false)
        $image = [Drawing.Bitmap]::new($stream)
        $reference = $null
        try {
            Assert-True ($image.Width -eq $size -and $image.Height -eq $size) 'Decoded frame dimensions differ.'
            $visible = 0
            $transparent = 0
            if ($Expected) {
                $reference = [Drawing.Bitmap]::new($size, $size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
                $graphics = [Drawing.Graphics]::FromImage($reference)
                try {
                    $graphics.Clear([Drawing.Color]::Transparent)
                    $graphics.CompositingQuality = [Drawing.Drawing2D.CompositingQuality]::HighQuality
                    $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                    $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
                    $graphics.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                    $graphics.DrawImage($Expected, 0, 0, $size, $size)
                } finally { $graphics.Dispose() }
            }
            $matches = $true
            for ($row = 0; $row -lt $size; $row++) {
                for ($column = 0; $column -lt $size; $column++) {
                    $pixel = $image.GetPixel($column, $row)
                    if ($pixel.A -gt 0) { $visible++ } else { $transparent++ }
                    if ($reference -and $pixel.ToArgb() -ne $reference.GetPixel($column, $row).ToArgb()) { $matches = $false }
                }
            }
            Assert-True ($visible -gt 0 -and $transparent -gt 0) 'Frame must contain both visible artwork and transparency.'
            if ($Expected) { Assert-True $matches 'Frame differs from expected centered square crop including transparent padding.' }
        } finally {
            if ($reference) { $reference.Dispose() }
            $image.Dispose()
            $stream.Dispose()
        }
        $nextOffset += $length
    }
    Assert-True ($nextOffset -eq $bytes.Length) 'ICO contains trailing or missing bytes.'
}

try {
    $assetsBefore = Get-Hashes (Join-Path $root 'assets')
    [IO.Directory]::CreateDirectory($temporary) | Out-Null
    $source = Join-Path $temporary 'source.png'
    $output = Join-Path $temporary 'out'
    [IO.Directory]::CreateDirectory($output) | Out-Null
    $bitmap = [Drawing.Bitmap]::new(1024, 1024, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    # The approved PNG is 72 DPI; cropping must preserve pixels, not physical size.
    $bitmap.SetResolution(72, 72)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.Clear([Drawing.Color]::Transparent)
        $graphics.FillRectangle([Drawing.Brushes]::Red, 0, 100, 159, 639)
        $bitmap.SetPixel(159, 739, [Drawing.Color]::FromArgb(1, 0, 0, 255))
        $bitmap.Save($source, [Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }

    $sourceHash = (Get-FileHash $source).Hash
    $missing = Join-Path $temporary 'missing-output'
    Assert-Fails { & $renderer -Check -SourcePng $source -OutputDirectory $missing } '*is stale*'
    Assert-True (-not (Test-Path $missing)) 'Check created the missing output directory.'
    & $renderer -SourcePng $source -OutputDirectory $output | Out-Null
    foreach ($name in @('github-adapter-dark.ico', 'github-adapter-tray.ico')) {
        $path = Join-Path $output $name
        Assert-Frames $path
    }
    $expected = [Drawing.Bitmap]::new(640, 640, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [Drawing.Graphics]::FromImage($expected)
    try {
        $graphics.Clear([Drawing.Color]::Transparent)
        $graphics.FillRectangle([Drawing.Brushes]::Red, 240, 0, 159, 639)
        $expected.SetPixel(399, 639, [Drawing.Color]::FromArgb(1, 0, 0, 255))
        $graphics.Dispose()
        $graphics = $null
        Assert-Frames (Join-Path $output 'github-adapter-tray.ico') $expected
        $dpiVariant = [Drawing.Bitmap]::new($source)
        try {
            $dpiVariant.SetResolution(144, 144)
            $dpiSource = Join-Path $temporary 'dpi-144.png'
            $dpiVariant.Save($dpiSource, [Drawing.Imaging.ImageFormat]::Png)
            $dpiOutput = Join-Path $temporary 'dpi-out'
            & $renderer -SourcePng $dpiSource -OutputDirectory $dpiOutput | Out-Null
            Assert-Frames (Join-Path $dpiOutput 'github-adapter-tray.ico') $expected
        } finally { $dpiVariant.Dispose() }
        $rotated = [Drawing.Bitmap]::new($source)
        try {
            $rotated.RotateFlip([Drawing.RotateFlipType]::Rotate90FlipNone)
            $rotatedSource = Join-Path $temporary 'rotated.png'
            $rotated.Save($rotatedSource, [Drawing.Imaging.ImageFormat]::Png)
            $rotatedHash = (Get-FileHash $rotatedSource).Hash
            $rotatedOutput = Join-Path $temporary 'rotated-out'
            & $renderer -SourcePng $rotatedSource -OutputDirectory $rotatedOutput | Out-Null
            $expected.RotateFlip([Drawing.RotateFlipType]::Rotate90FlipNone)
            Assert-Frames (Join-Path $rotatedOutput 'github-adapter-tray.ico') $expected
            Assert-True ((Get-FileHash $rotatedSource).Hash -eq $rotatedHash) 'Rotated source changed.'
        } finally { $rotated.Dispose() }
    } finally {
        if ($graphics) { $graphics.Dispose() }
        $expected.Dispose()
    }
    $firstHashes = @(Get-ChildItem $output -File | Get-FileHash | Select-Object -ExpandProperty Hash) -join '|'
    & $renderer -SourcePng $source -OutputDirectory $output | Out-Null
    Assert-True ($firstHashes -eq (@(Get-ChildItem $output -File | Get-FileHash | Select-Object -ExpandProperty Hash) -join '|')) 'Repeated canonical generation is not deterministic.'
    $beforeCheck = Get-Hashes $output
    & $renderer -Check -SourcePng $source -OutputDirectory $output | Out-Null
    Assert-True ($beforeCheck -eq (Get-Hashes $output)) 'Successful check changed output contents or timestamps.'
    foreach ($name in @('github-adapter-dark.ico', 'github-adapter-tray.ico')) {
        $path = Join-Path $output $name
        $original = [IO.File]::ReadAllBytes($path)
        [IO.File]::WriteAllBytes($path, [byte[]]@(0, 1, 2))
        $beforeCheck = Get-Hashes $output
        Assert-Fails { & $renderer -Check -SourcePng $source -OutputDirectory $output } "*$name is stale*"
        Assert-True ($beforeCheck -eq (Get-Hashes $output)) 'Failed check changed existing outputs.'
        [IO.File]::WriteAllBytes($path, $original)
    }
    foreach ($case in @('dimensions', 'rgb', 'transparent', 'missing')) {
        $invalid = Join-Path $temporary "$case.png"
        if ($case -ne 'missing') {
            $dimension = if ($case -eq 'dimensions') { 64 } else { 1024 }
            $format = if ($case -eq 'rgb') { [Drawing.Imaging.PixelFormat]::Format24bppRgb } else { [Drawing.Imaging.PixelFormat]::Format32bppArgb }
            $bitmap = [Drawing.Bitmap]::new($dimension, $dimension, $format)
            try { $bitmap.Save($invalid, [Drawing.Imaging.ImageFormat]::Png) } finally { $bitmap.Dispose() }
        }
        foreach ($checkMode in @($false, $true)) {
            $beforeCheck = Get-Hashes $output
            Assert-Fails { & $renderer -Check:$checkMode -SourcePng $invalid -OutputDirectory $output } '*'
            Assert-True ($beforeCheck -eq (Get-Hashes $output)) "$case input changed existing output."
            Assert-Fails { & $renderer -Check:$checkMode -SourcePng $invalid -OutputDirectory $missing } '*'
            Assert-True (-not (Test-Path $missing)) "$case input created output directory."
        }
    }
    Assert-True ((Get-FileHash $source).Hash -eq $sourceHash) 'Source PNG changed.'

    $approvedOutput = Join-Path $temporary 'approved'
    & $renderer -OutputDirectory $approvedOutput | Out-Null
    foreach ($name in @('github-adapter-dark.ico', 'github-adapter-tray.ico')) { Assert-Frames (Join-Path $approvedOutput $name) }
    & $renderer -Check -OutputDirectory $approvedOutput | Out-Null

    Assert-True ($assetsBefore -eq (Get-Hashes (Join-Path $root 'assets'))) 'Fixtures changed repository artwork.'
} finally {
    if ([IO.Directory]::Exists($temporary)) { [IO.Directory]::Delete($temporary, $true) }
}

Write-Host "Icon tooling tests: $script:assertions assertions passed."
