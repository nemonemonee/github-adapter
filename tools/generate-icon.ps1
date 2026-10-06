#requires -Version 7.0
[CmdletBinding()]
param(
    [switch]$Check,
    [string]$SourcePng,
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $IsWindows) { throw 'Icon generation requires Windows and PowerShell 7.' }
$root = Split-Path -Parent $PSScriptRoot
if (-not $SourcePng) { $SourcePng = Join-Path $root 'assets\github-adapter-dark.png' }
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root 'assets' }
$SourcePng = [IO.Path]::GetFullPath($SourcePng)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$sizes = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)

Add-Type -AssemblyName System.Drawing

function Get-VisibleBounds([Drawing.Bitmap]$Image) {
    $left = $Image.Width
    $top = $Image.Height
    $right = -1
    $bottom = -1
    for ($row = 0; $row -lt $Image.Height; $row++) {
        for ($column = 0; $column -lt $Image.Width; $column++) {
            if ($Image.GetPixel($column, $row).A -gt 0) {
                if ($column -lt $left) { $left = $column }
                if ($row -lt $top) { $top = $row }
                if ($column -gt $right) { $right = $column }
                if ($row -gt $bottom) { $bottom = $row }
            }
        }
    }
    if ($right -lt 0) { throw 'Source PNG must contain visible pixels.' }
    return [Drawing.Rectangle]::FromLTRB($left, $top, $right + 1, $bottom + 1)
}

function Copy-TrayImage([Drawing.Bitmap]$Source) {
    $bounds = Get-VisibleBounds $Source
    $side = [Math]::Max($bounds.Width, $bounds.Height)
    $left = [int][Math]::Floor(($bounds.Left + $bounds.Right - $side) / 2.0)
    $top = [int][Math]::Floor(($bounds.Top + $bounds.Bottom - $side) / 2.0)
    $target = [Drawing.Bitmap]::new($side, $side, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [Drawing.Graphics]::FromImage($target)
    try {
        $graphics.Clear([Drawing.Color]::Transparent)
        $graphics.DrawImage(
            $Source,
            [Drawing.Rectangle]::new(-$left, -$top, $Source.Width, $Source.Height),
            0, 0, $Source.Width, $Source.Height,
            [Drawing.GraphicsUnit]::Pixel
        )
        return $target
    } finally { $graphics.Dispose() }
}

function New-PngFrame([Drawing.Image]$Image, [int]$Size) {
    $frame = [Drawing.Bitmap]::new($Size, $Size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [Drawing.Graphics]::FromImage($frame)
    try {
        $graphics.Clear([Drawing.Color]::Transparent)
        $graphics.CompositingQuality = [Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
        $graphics.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.DrawImage($Image, 0, 0, $Size, $Size)
        $stream = [IO.MemoryStream]::new()
        try {
            $frame.Save($stream, [Drawing.Imaging.ImageFormat]::Png)
            return $stream.ToArray()
        } finally { $stream.Dispose() }
    } finally {
        $graphics.Dispose()
        $frame.Dispose()
    }
}

function ConvertTo-IcoBytes([Drawing.Image]$Image) {
    $frames = foreach ($size in $sizes) { [pscustomobject]@{ Size = $size; Bytes = [byte[]](New-PngFrame -Image $Image -Size $size) } }
    $stream = [IO.MemoryStream]::new()
    $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([UInt16]0)
        $writer.Write([UInt16]1)
        $writer.Write([UInt16]$frames.Count)
        $offset = 6 + 16 * $frames.Count
        foreach ($frame in $frames) {
            $dimension = if ($frame.Size -eq 256) { 0 } else { $frame.Size }
            $writer.Write([byte]$dimension)
            $writer.Write([byte]$dimension)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([UInt16]1)
            $writer.Write([UInt16]32)
            $writer.Write([UInt32]$frame.Bytes.Length)
            $writer.Write([UInt32]$offset)
            $offset += $frame.Bytes.Length
        }
        foreach ($frame in $frames) { $writer.Write([byte[]]$frame.Bytes) }
        $writer.Flush()
        return $stream.ToArray()
    } finally {
        $writer.Dispose()
        $stream.Dispose()
    }
}

function Get-IcoFramePixels([byte[]]$Bytes, [int]$Index) {
    $entry = 6 + 16 * $Index
    $length = [BitConverter]::ToUInt32($Bytes, $entry + 8)
    $offset = [BitConverter]::ToUInt32($Bytes, $entry + 12)
    if ($length -lt 33 -or $offset -gt $Bytes.Length -or $length -gt $Bytes.Length - $offset) {
        throw "Invalid ICO frame $Index bounds."
    }
    $stream = [IO.MemoryStream]::new($Bytes, [int]$offset, [int]$length, $false)
    $image = $null
    try {
        $image = [Drawing.Bitmap]::new($stream)
        if ($image.Width -ne $sizes[$Index] -or $image.Height -ne $sizes[$Index]) {
            throw "Invalid ICO frame $Index dimensions: $($image.Width)x$($image.Height)."
        }
        $rgba = [byte[]]::new(4 * $image.Width * $image.Height)
        $next = 0
        for ($row = 0; $row -lt $image.Height; $row++) {
            for ($column = 0; $column -lt $image.Width; $column++) {
                $pixel = $image.GetPixel($column, $row)
                $rgba[$next++] = $pixel.R
                $rgba[$next++] = $pixel.G
                $rgba[$next++] = $pixel.B
                $rgba[$next++] = $pixel.A
            }
        }
        return [pscustomobject]@{
            EncodedLength = $length
            Rgba = $rgba
            Hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($rgba))
        }
    } finally {
        if ($image) { $image.Dispose() }
        $stream.Dispose()
    }
}

function Write-IcoDifferences([byte[]]$Canonical, [byte[]]$Generated) {
    try {
        foreach ($bytes in @($Canonical, $Generated)) {
            if ($bytes.Length -lt 166 -or [BitConverter]::ToUInt16($bytes, 0) -ne 0 -or [BitConverter]::ToUInt16($bytes, 2) -ne 1 -or [BitConverter]::ToUInt16($bytes, 4) -ne $sizes.Count) {
                throw 'Invalid ICO header; expected ten image frames.'
            }
        }
        foreach ($index in 0..($sizes.Count - 1)) {
            $canonicalFrame = Get-IcoFramePixels $Canonical $index
            $generatedFrame = Get-IcoFramePixels $Generated $index
            $mismatches = 0
            $maxDelta = 0
            for ($offset = 0; $offset -lt $canonicalFrame.Rgba.Length; $offset += 4) {
                $different = $false
                foreach ($channel in 0..3) {
                    $delta = [Math]::Abs([int]$canonicalFrame.Rgba[$offset + $channel] - [int]$generatedFrame.Rgba[$offset + $channel])
                    if ($delta -ne 0) { $different = $true }
                    if ($delta -gt $maxDelta) { $maxDelta = $delta }
                }
                if ($different) { $mismatches++ }
            }
            Write-Host "ICO frame $($sizes[$index])x$($sizes[$index]): canonical PNG bytes=$($canonicalFrame.EncodedLength), RGBA SHA256=$($canonicalFrame.Hash); generated PNG bytes=$($generatedFrame.EncodedLength), RGBA SHA256=$($generatedFrame.Hash); mismatched pixels=$mismatches; max channel delta=$maxDelta."
        }
    } catch {
        Write-Host "ICO mismatch diagnostics unavailable: $($_.Exception.Message)"
    }
}

$sourceBytes = [IO.File]::ReadAllBytes($SourcePng)
if ($sourceBytes.Length -lt 33 -or [Convert]::ToHexString($sourceBytes[0..7]) -cne '89504E470D0A1A0A' -or [Convert]::ToHexString($sourceBytes[12..15]) -cne '49484452' -or $sourceBytes[24] -ne 8 -or $sourceBytes[25] -ne 6) {
    throw 'Source must be a valid 1024x1024 PNG with 8-bit RGBA channels.'
}
$sourceStream = [IO.MemoryStream]::new($sourceBytes, $false)
$source = $null
$tray = $null
try {
    $source = [Drawing.Bitmap]::new($sourceStream)
    if ($source.Width -ne 1024 -or $source.Height -ne 1024) { throw 'Source must be a 1024x1024 PNG.' }
    $tray = Copy-TrayImage $source
    $outputs = @(
        [pscustomobject]@{ Name = 'github-adapter-dark.ico'; Bytes = (ConvertTo-IcoBytes $source) },
        [pscustomobject]@{ Name = 'github-adapter-tray.ico'; Bytes = (ConvertTo-IcoBytes $tray) }
    )
} finally {
    if ($tray) { $tray.Dispose() }
    if ($source) { $source.Dispose() }
    $sourceStream.Dispose()
}

if (-not $Check) { [IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null }
foreach ($output in $outputs) {
    $target = Join-Path $OutputDirectory $output.Name
    if ($Check) {
        if (-not [IO.File]::Exists($target) -or -not [Linq.Enumerable]::SequenceEqual([byte[]][IO.File]::ReadAllBytes($target), [byte[]]$output.Bytes)) {
            if ([IO.File]::Exists($target)) {
                Write-Host "ICO byte mismatch: $($output.Name)."
                Write-IcoDifferences ([IO.File]::ReadAllBytes($target)) ([byte[]]$output.Bytes)
            }
            throw "$($output.Name) is stale; run tools/generate-icon.ps1 before rebuilding both native executables."
        }
    } else {
        [IO.File]::WriteAllBytes($target, $output.Bytes)
        Write-Host "Exported $target"
    }
}

if ($Check) { Write-Host 'The application and tray icons match the current Windows .NET export.' }
