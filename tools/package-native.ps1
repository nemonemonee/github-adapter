#requires -Version 7.0
[CmdletBinding(DefaultParameterSetName = 'Package')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Package')]
    [ValidateSet('arm64', 'x64')]
    [string]$Architecture,
    [Parameter(Mandatory = $true, ParameterSetName = 'Package')]
    [string]$Source,
    [Parameter(ParameterSetName = 'Package')]
    [string]$InstallerSource,
    [Parameter(ParameterSetName = 'Package')]
    [string]$OutputDirectory,
    [Parameter(ParameterSetName = 'Package')]
    [switch]$Unsigned,
    [Parameter(Mandatory = $true, ParameterSetName = 'Verify')]
    [string]$Verify,
    [Parameter(ParameterSetName = 'Verify')]
    [ValidatePattern('^[a-fA-F0-9]{64}$')]
    [string]$ExpectedSHA256,
    [switch]$RequireSigned,
    [string]$ExpectedPublisher
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Native release packaging requires Windows.' }
if ($PSCmdlet.ParameterSetName -eq 'Package' -and $Unsigned.IsPresent -eq $RequireSigned.IsPresent) {
    throw 'Choose exactly one of -Unsigned or -RequireSigned. This tool never signs files.'
}
if ($RequireSigned -and [string]::IsNullOrWhiteSpace($ExpectedPublisher)) {
    throw '-RequireSigned requires the exact certificate Subject in -ExpectedPublisher.'
}
if (-not $RequireSigned -and $ExpectedPublisher) {
    throw '-ExpectedPublisher requires -RequireSigned.'
}

Add-Type -AssemblyName System.IO.Compression
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$artifacts = Join-Path $root 'artifacts'
$utf8 = [Text.UTF8Encoding]::new($false, $true)
$payloadNames = @(
    'bin/github-adapter.exe',
    'bin/github-adapter-host.exe',
    'assets/github-adapter-dark.ico',
    'tools/install-native.ps1',
    'README.txt',
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'THIRD_PARTY_LICENSES.txt'
)
$archiveNames = @($payloadNames) + 'manifest.json'
$limits = @{
    'bin/github-adapter.exe' = 128MB
    'bin/github-adapter-host.exe' = 128MB
    'assets/github-adapter-dark.ico' = 4MB
    'tools/install-native.ps1' = 1MB
    'README.txt' = 64KB
    'manifest.json' = 64KB
    'LICENSE' = 64KB
    'THIRD_PARTY_NOTICES.md' = 64KB
    'THIRD_PARTY_LICENSES.txt' = 8MB
}

function Get-LocalPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path -match '[/?*]' -or
        $Path.StartsWith('\\') -or $Path -match '(^|\\)\.\.(\\|$)') {
        throw "Expected a local filesystem path without traversal, wildcards, or device prefixes: $Path"
    }
    if (-not [IO.Path]::IsPathRooted($Path)) {
        if ((Get-Location).Provider.Name -ne 'FileSystem') { throw 'Use a filesystem working directory.' }
        $Path = Join-Path (Get-Location).ProviderPath $Path
    } elseif ($Path -notmatch '^[A-Za-z]:\\') {
        throw "Use an absolute drive path or an explicitly relative local path: $Path"
    }
    $full = [IO.Path]::GetFullPath($Path)
    if ($full -notmatch '^[A-Za-z]:\\' -or
        $full.Substring(3) -match '[:*?]|(^|\\)[^\\]*[ .](\\|$)') {
        throw "Unexpected local filesystem path (including alternate data streams): $Path"
    }
    return $full.TrimEnd('\')
}

function Test-InDirectory([string]$Path, [string]$Directory) {
    return $Path.Equals($Directory, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($Directory.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Assert-PlainPath([string]$Path, [bool]$Directory = $false, [bool]$MustExist = $true) {
    $current = $Path
    while ($current) {
        $item = $null
        try { $item = Get-Item -LiteralPath $current -Force -ErrorAction Stop }
        catch [System.Management.Automation.ItemNotFoundException] {
            if ($current -eq $Path -and $MustExist) { throw "Required package input is missing: $Path" }
        }
        if ($null -ne $item) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Package paths must not contain links or reparse points: $current"
            }
            if (($current -ne $Path -or $Directory) -and -not $item.PSIsContainer) {
                throw "Expected a package directory: $current"
            }
            if ($current -eq $Path -and -not $Directory -and $item.PSIsContainer) {
                throw "Expected a package file: $current"
            }
            if (-not $item.PSIsContainer) {
                if ($null -eq $item.PSObject.Properties['LinkType']) {
                    throw 'This PowerShell filesystem provider cannot verify hard links.'
                }
                if ($item.LinkType) {
                    throw "Package input must be an independent regular file, not a hard link: $current. Copy build outputs first."
                }
            }
        }
        $current = Split-Path -Parent $current
    }
}

function Get-Hash([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $hasher = [Security.Cryptography.SHA256]::Create()
        try { return [BitConverter]::ToString($hasher.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
        finally { $hasher.Dispose() }
    } finally { $stream.Dispose() }
}

function Get-Platform([string]$Name) {
    switch -CaseSensitive ($Name) {
        'arm64' { return @{ Machine = 0xAA64; Target = 'aarch64-pc-windows-msvc' } }
        'x64' { return @{ Machine = 0x8664; Target = 'x86_64-pc-windows-msvc' } }
        default { throw "Unsupported package architecture: $Name" }
    }
}

function Get-PeMetadata([string]$Path, [string]$Name, [string]$Arch, [int]$Subsystem) {
    Assert-PlainPath $Path
    $platform = Get-Platform $Arch
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        $length = $reader.BaseStream.Length
        if ($length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
            throw "Invalid DOS/PE header: $Name"
        }
        $reader.BaseStream.Position = 0x3C
        $offset = [long]$reader.ReadUInt32()
        if ($offset -lt 64 -or $offset -gt $length - 24) { throw "Invalid PE offset: $Name" }
        $reader.BaseStream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne $platform.Machine) {
            throw "PE machine does not match Windows $Arch`: $Name"
        }
        $sections = $reader.ReadUInt16()
        $reader.BaseStream.Position = $offset + 20
        $optionalSize = $reader.ReadUInt16()
        $characteristics = $reader.ReadUInt16()
        if ($sections -lt 1 -or $sections -gt 96 -or $optionalSize -lt 112 -or
            $offset + 24 + $optionalSize + 40 * $sections -gt $length -or
            -not ($characteristics -band 0x0002) -or ($characteristics -band 0x2000)) {
            throw "Invalid executable PE structure: $Name"
        }
        if ($reader.ReadUInt16() -ne 0x20B) { throw "A PE32+ executable is required: $Name" }
        $reader.BaseStream.Position = $offset + 24 + 68
        if ($reader.ReadUInt16() -ne $Subsystem) {
            throw 'The native pair must contain a console CLI (3) and Windows GUI host (2).'
        }
        for ($index = 0; $index -lt $sections; $index++) {
            $reader.BaseStream.Position = $offset + 24 + $optionalSize + 40 * $index + 16
            $size = [long]$reader.ReadUInt32()
            $start = [long]$reader.ReadUInt32()
            if ($size -gt 0 -and ($start -eq 0 -or $start + $size -gt $length)) {
                throw "Truncated PE section: $Name"
            }
        }
    } finally { $reader.Dispose() }
    # Resource inspection never executes either image, including a cross-built image.
    $info = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
    $fixedFileVersion = "$($info.FileMajorPart).$($info.FileMinorPart).$($info.FileBuildPart)"
    $fixedProductVersion = "$($info.ProductMajorPart).$($info.ProductMinorPart).$($info.ProductBuildPart)"
    if ($info.OriginalFilename -cne $Name -or $info.InternalName -cne [IO.Path]::GetFileNameWithoutExtension($Name) -or
        $info.ProductName -cne 'GitHub Adapter' -or $info.FileVersion -cnotmatch '^\d+\.\d+\.\d+$' -or
        $info.FileVersion -cne $info.ProductVersion -or $info.FileVersion -cne $fixedFileVersion -or
        $info.FileVersion -cne $fixedProductVersion -or $info.FilePrivatePart -ne 0 -or $info.ProductPrivatePart -ne 0) {
        throw "Embedded version or product identity is invalid: $Name"
    }
    return [ordered]@{
        machine = ('0x{0:X4}' -f $platform.Machine)
        subsystem = $Subsystem
        fileVersion = $info.FileVersion
        productVersion = $info.ProductVersion
        originalFilename = $info.OriginalFilename
    }
}

function Get-SignatureMetadata([string]$Path) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path -ErrorAction Stop
    if ($RequireSigned) {
        if ($signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate) {
            throw "A valid Authenticode signature is required: $Path (status: $($signature.Status))."
        }
        if ($signature.SignatureType -ne 'Authenticode') {
            throw "An embedded Authenticode signature is required; catalog trust is not packaged: $Path"
        }
        if (-not $signature.SignerCertificate.Subject.Equals($ExpectedPublisher, [StringComparison]::Ordinal)) {
            throw "Authenticode publisher does not match -ExpectedPublisher: $Path"
        }
        $eku = @($signature.SignerCertificate.Extensions |
            Where-Object { $_.Oid.Value -eq '2.5.29.37' } |
            ForEach-Object { $_.EnhancedKeyUsages } |
            ForEach-Object { $_.Value })
        if ('1.3.6.1.5.5.7.3.3' -notin $eku) {
            throw "An explicit Code Signing EKU is required: $Path"
        }
    } elseif ($signature.Status -ne 'NotSigned') {
        throw "Unsigned packages require NotSigned payloads; refusing $($signature.Status): $Path"
    }
    return [ordered]@{
        status = [string]$signature.Status
        publisher = $(if ($RequireSigned) { $signature.SignerCertificate.Subject } else { $null })
        thumbprint = $(if ($RequireSigned) { $signature.SignerCertificate.Thumbprint } else { $null })
    }
}

function Get-UsageText([string]$Version, [string]$Arch) {
    $label = if ($RequireSigned) { 'SIGNED PAYLOADS' } else { 'UNSIGNED PACKAGE' }
    return (@"
GitHub Adapter $Version - Windows $Arch - $label

Contains the native desktop app and command-line tools.
Extract the complete ZIP, retaining bin\, assets\, and tools\.
Verify the archive with a trusted checkout's tools\package-native.ps1 before installation.
From the extracted package root, on the matching Windows architecture:

  pwsh -NoProfile -File .\tools\install-native.ps1 -Source .\bin\github-adapter.exe

The installer requires both binary siblings and the bundled icon.
Close the installed adapter before updating. If optional images are enabled,
close Codex too so its native image MCP helper releases the executable.
Installation creates the native GUI shortcut but does not start the host.
It restores stopped legacy Codex routing when verified recovery data exists.
Use -NoConfigurationChange to leave client settings unchanged.
Sign in once using bin\github-adapter.exe login, then open GitHub Adapter.
Quit from the tray to restore normal Codex settings.
LICENSE and THIRD_PARTY_LICENSES.txt contain the project and dependency licenses.
Follow your organization's PowerShell, Authenticode, and SmartScreen policies.
If policy blocks this package, stop; this bundle does not bypass trust policy.

manifest.json records SHA256 for every payload file. The external .zip.sha256
records the entire ZIP hash. Hashes detect changes only relative to a trusted
reference; replacing a ZIP and its hashes defeats unauthenticated hash checks.
Even signed-payload verification is not a signature on the ZIP, icon, or metadata.
Use an independently trusted distribution channel and approved publisher identity.
"@).Replace("`r`n", "`n") + "`n"
}

function Assert-ApplicationArtwork([string]$Executable, [string]$Icon) {
    if (-not ('GitHubAdapter.PackageArtwork' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
namespace GitHubAdapter {
    public static class PackageArtwork {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
        [DllImport("kernel32.dll", ExactSpelling = true)]
        static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);
        [DllImport("kernel32.dll", ExactSpelling = true)]
        static extern uint SizeofResource(IntPtr module, IntPtr resource);
        [DllImport("kernel32.dll", ExactSpelling = true)]
        static extern IntPtr LoadResource(IntPtr module, IntPtr resource);
        [DllImport("kernel32.dll", ExactSpelling = true)]
        static extern IntPtr LockResource(IntPtr resource);
        [DllImport("kernel32.dll", ExactSpelling = true)]
        static extern bool FreeLibrary(IntPtr module);
        static InvalidDataException Mismatch() {
            return new InvalidDataException("Embedded application icon does not match the bundled ICO; rebuild both native executables.");
        }
        static byte[] Resource(IntPtr module, int type, int id) {
            IntPtr resource = FindResourceW(module, new IntPtr(id), new IntPtr(type));
            if (resource == IntPtr.Zero) throw Mismatch();
            uint size = SizeofResource(module, resource);
            if (size == 0 || size > 4 * 1024 * 1024) throw Mismatch();
            IntPtr loaded = LoadResource(module, resource);
            IntPtr data = loaded == IntPtr.Zero ? IntPtr.Zero : LockResource(loaded);
            if (data == IntPtr.Zero) throw Mismatch();
            byte[] bytes = new byte[(int)size];
            Marshal.Copy(data, bytes, 0, bytes.Length);
            return bytes;
        }
        public static void Verify(string executable, string iconPath) {
            if (new FileInfo(iconPath).Length > 4 * 1024 * 1024) throw Mismatch();
            byte[] icon = File.ReadAllBytes(iconPath);
            if (icon.Length < 22 || BitConverter.ToUInt16(icon, 0) != 0 || BitConverter.ToUInt16(icon, 2) != 1) throw Mismatch();
            int count = BitConverter.ToUInt16(icon, 4);
            if (count < 1 || count > 256 || 6 + count * 16 > icon.Length) throw Mismatch();
            // Data/resource mapping only: never execute the image, including cross-built x64.
            IntPtr module = LoadLibraryExW(executable, IntPtr.Zero, 0x60);
            if (module == IntPtr.Zero) throw Mismatch();
            try {
                byte[] group = Resource(module, 14, 1); // RT_GROUP_ICON 1 is used by the tray/window.
                if (group.Length != 6 + count * 14) throw Mismatch();
                for (int i = 0; i < 6; i++) if (group[i] != icon[i]) throw Mismatch();
                for (int i = 0; i < count; i++) {
                    int entry = 6 + i * 16, embedded = 6 + i * 14;
                    for (int j = 0; j < 12; j++) {
                        if (j != 4 && j != 5 && icon[entry + j] != group[embedded + j]) throw Mismatch();
                    }
                    // rc.exe normalizes the ICO's unspecified color planes to one.
                    int planes = BitConverter.ToUInt16(icon, entry + 4);
                    if (BitConverter.ToUInt16(group, embedded + 4) != Math.Max(1, planes)) throw Mismatch();
                    uint size = BitConverter.ToUInt32(icon, entry + 8);
                    uint offset = BitConverter.ToUInt32(icon, entry + 12);
                    if (size == 0 || offset < 6 + count * 16 || (ulong)offset + size > (ulong)icon.Length) throw Mismatch();
                    byte[] frame = Resource(module, 3, BitConverter.ToUInt16(group, embedded + 12));
                    if (frame.Length != size) throw Mismatch();
                    for (int j = 0; j < frame.Length; j++) if (frame[j] != icon[(int)offset + j]) throw Mismatch();
                }
            } finally { FreeLibrary(module); }
        }
    }
}
'@
    }
    [GitHubAdapter.PackageArtwork]::Verify($Executable, $Icon)
}

function Get-Manifest([string]$Payload, [string]$Arch) {
    $cli = Get-PeMetadata (Join-Path $Payload 'bin\github-adapter.exe') 'github-adapter.exe' $Arch 3
    $hostInfo = Get-PeMetadata (Join-Path $Payload 'bin\github-adapter-host.exe') 'github-adapter-host.exe' $Arch 2
    if ($cli.fileVersion -cne $hostInfo.fileVersion) { throw 'The native pair must have matching embedded release versions.' }
    $readme = $utf8.GetString([IO.File]::ReadAllBytes((Join-Path $Payload 'README.txt')))
    if ($readme -cne (Get-UsageText $cli.fileVersion $Arch)) { throw 'Package usage text does not match the release.' }
    $records = @()
    foreach ($name in $payloadNames) {
        $path = Join-Path $Payload $name.Replace('/', '\')
        Assert-PlainPath $path
        $length = (Get-Item -LiteralPath $path -Force).Length
        if ($length -lt 1 -or $length -gt $limits[$name]) { throw "Unexpected payload size: $name" }
        $pe = switch ($name) {
            'bin/github-adapter.exe' { $cli }
            'bin/github-adapter-host.exe' { $hostInfo }
            default { $null }
        }
        $signature = if ($name -match '\.(exe|ps1)$') { Get-SignatureMetadata $path } else { $null }
        $records += [ordered]@{ path = $name; length = $length; sha256 = Get-Hash $path; pe = $pe; signature = $signature }
    }
    # Validate the relationship between payload files, not only each file in isolation.
    foreach ($name in @('github-adapter.exe', 'github-adapter-host.exe')) {
        Assert-ApplicationArtwork (Join-Path $Payload "bin\$name") (Join-Path $Payload 'assets\github-adapter-dark.ico')
    }
    return [ordered]@{
        schemaVersion = 2
        product = 'GitHub Adapter'
        version = $cli.fileVersion
        architecture = $Arch
        target = (Get-Platform $Arch).Target
        signing = $(if ($RequireSigned) { 'signed' } else { 'unsigned' })
        publisher = $(if ($RequireSigned) { $ExpectedPublisher } else { $null })
        files = $records
    }
}

function Get-ManifestText($Manifest) {
    return ($Manifest | ConvertTo-Json -Depth 8).Replace("`r`n", "`n") + "`n"
}

function Get-PackageName($Manifest) {
    return "github-adapter-$($Manifest.version)-windows-$($Manifest.architecture)-$($Manifest.signing)"
}

function Copy-Payload([string]$From, [string]$To) {
    Assert-PlainPath $From
    $inputStream = [IO.File]::Open($From, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $outputStream = [IO.File]::Open($To, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $inputStream.CopyTo($outputStream) } finally { $outputStream.Dispose() }
        $inputStream.Position = 0
        $hasher = [Security.Cryptography.SHA256]::Create()
        try { $expected = [BitConverter]::ToString($hasher.ComputeHash($inputStream)).Replace('-', '').ToLowerInvariant() }
        finally { $hasher.Dispose() }
        if ((Get-Hash $To) -cne $expected) { throw "Payload copy failed SHA256 verification: $From" }
    } finally { $inputStream.Dispose() }
}

function Write-Zip([string]$Payload, [string]$ArchivePath) {
    $stream = [IO.File]::Open($ArchivePath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true, $utf8)
        try {
            foreach ($name in $archiveNames) {
                $entry = $zip.CreateEntry($name, [IO.Compression.CompressionLevel]::NoCompression)
                $entry.LastWriteTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
                $entry.ExternalAttributes = 0x20
                $entryStream = $entry.Open()
                try {
                    $inputStream = [IO.File]::OpenRead((Join-Path $Payload $name.Replace('/', '\')))
                    try { $inputStream.CopyTo($entryStream) } finally { $inputStream.Dispose() }
                } finally { $entryStream.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $stream.Dispose() }
}

function Move-PackageDirectory([string]$From, [string]$To) {
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        Assert-PlainPath $From $true
        Assert-PlainPath $To $true $false
        if (Test-Path -LiteralPath $To) { throw "Output collision: refusing to overwrite $To" }
        try {
            [IO.Directory]::Move($From, $To)
            return
        } catch {
            $cause = $_.Exception
            while ($cause.InnerException) { $cause = $cause.InnerException }
            $code = $cause.HResult -band 0xFFFF
            if (($cause -isnot [IO.IOException] -and $cause -isnot [UnauthorizedAccessException]) -or
                $code -notin @(5, 32, 33) -or $deadline.ElapsedMilliseconds -ge 2000) {
                throw
            }
            # Windows scanners may briefly retain handles after verification/deletion.
            Start-Sleep -Milliseconds 50
        }
    }
}

function Read-VerifiedZip([string]$ArchivePath, [string]$ExtractionPath) {
    Assert-PlainPath $ArchivePath
    Assert-PlainPath "$ArchivePath.sha256"
    $stream = [IO.File]::OpenRead($ArchivePath)
    try {
        if ($stream.Length -gt 270MB) { throw 'Package archive exceeds the bounded size limit.' }
        $hash = Get-Hash $ArchivePath
        $name = [IO.Path]::GetFileName($ArchivePath)
        $checksumText = "$hash  $name`n"
        if ((Get-Item -LiteralPath "$ArchivePath.sha256" -Force).Length -ne $utf8.GetByteCount($checksumText) -or
            [IO.File]::ReadAllText("$ArchivePath.sha256", $utf8) -cne $checksumText) {
            throw 'Archive SHA256 sidecar does not match.'
        }
        if ($ExpectedSHA256 -and $hash -ine $ExpectedSHA256) { throw 'Archive does not match -ExpectedSHA256.' }
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read, $true, $utf8)
        try {
            if ($zip.Entries.Count -ne $archiveNames.Count) { throw 'Unexpected package archive entries.' }
            $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            foreach ($entry in $zip.Entries) {
                if ($entry.FullName -cnotin $archiveNames -or -not $seen.Add($entry.FullName)) {
                    throw "Unexpected or duplicate package archive path: $($entry.FullName)"
                }
                $unixType = ($entry.ExternalAttributes -shr 16) -band 0xF000
                if (($entry.ExternalAttributes -band 0x410) -ne 0 -or $unixType -notin @(0, 0x8000)) {
                    throw "Archive links and nonregular entries are not allowed: $($entry.FullName)"
                }
                if ($entry.Length -lt 1 -or $entry.Length -gt $limits[$entry.FullName]) {
                    throw "Unexpected archive entry size: $($entry.FullName)"
                }
            }
            [IO.Directory]::CreateDirectory($ExtractionPath) | Out-Null
            foreach ($entry in $zip.Entries) {
                $destination = Join-Path $ExtractionPath $entry.FullName.Replace('/', '\')
                [IO.Directory]::CreateDirectory((Split-Path -Parent $destination)) | Out-Null
                $entryStream = $entry.Open()
                try {
                    $outputStream = [IO.File]::Open($destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                    try {
                        $buffer = [byte[]]::new(65536)
                        $written = 0L
                        while (($count = $entryStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                            $written += $count
                            if ($written -gt $entry.Length) { throw 'Archive entry exceeds its declared length.' }
                            $outputStream.Write($buffer, 0, $count)
                        }
                        if ($written -ne $entry.Length) { throw 'Archive entry is truncated.' }
                    } finally { $outputStream.Dispose() }
                } finally { $entryStream.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $stream.Dispose() }
    $manifestPath = Join-Path $ExtractionPath 'manifest.json'
    $manifestText = $utf8.GetString([IO.File]::ReadAllBytes($manifestPath))
    $manifest = $manifestText | ConvertFrom-Json -ErrorAction Stop
    $signing = if ($RequireSigned) { 'signed' } else { 'unsigned' }
    if ($manifest.signing -cne $signing) {
        throw 'Package signing label does not match the requested policy; signed packages require -RequireSigned and -ExpectedPublisher.'
    }
    $actual = Get-Manifest $ExtractionPath ([string]$manifest.architecture)
    # Exact regeneration also rejects extra fields, duplicate JSON keys, and forged metadata.
    if ($manifestText -cne (Get-ManifestText $actual)) { throw 'Payload SHA256 manifest or release metadata does not match.' }
    if ($name -cne "$(Get-PackageName $actual).zip") { throw 'Archive filename does not match the version, architecture, and signing label.' }
    # Reject hidden local entries, conflicting ZIP headers, extra link metadata, and overlays.
    $canonical = Join-Path $ExtractionPath '.canonical.zip'
    Write-Zip $ExtractionPath $canonical
    if ((Get-Hash $canonical) -cne $hash) { throw 'Archive is not the canonical ZIP representation produced by this packager.' }
    return [pscustomobject]@{ Manifest = $actual; Hash = $hash }
}

Assert-PlainPath $artifacts $true $false
$stage = $null
try {
    if ($PSCmdlet.ParameterSetName -eq 'Verify') {
        $archivePath = Get-LocalPath $Verify
        if (-not (Test-InDirectory $archivePath $artifacts)) { throw 'Place the archive and sidecar under this checkout''s artifacts directory.' }
        Assert-PlainPath $archivePath
        $stage = Join-Path $artifacts ('.verify-native-' + [Guid]::NewGuid().ToString('N'))
        $verified = Read-VerifiedZip $archivePath $stage
    } else {
        $Architecture = $Architecture.ToLowerInvariant()
        $sourcePath = Get-LocalPath $Source
        if ([IO.Path]::GetFileName($sourcePath) -cne 'github-adapter.exe') { throw '-Source must name github-adapter.exe exactly.' }
        $hostPath = Join-Path (Split-Path -Parent $sourcePath) 'github-adapter-host.exe'
        $installerPath = if ($InstallerSource) { Get-LocalPath $InstallerSource } else { Join-Path $root 'tools\install-native.ps1' }
        if ([IO.Path]::GetFileName($installerPath) -cne 'install-native.ps1') { throw '-InstallerSource must name install-native.ps1 exactly.' }
        $iconPath = Join-Path $root 'assets\github-adapter-dark.ico'
        foreach ($path in @($sourcePath, $hostPath, $installerPath, $iconPath)) { Assert-PlainPath $path }
        $cli = Get-PeMetadata $sourcePath 'github-adapter.exe' $Architecture 3
        $hostInfo = Get-PeMetadata $hostPath 'github-adapter-host.exe' $Architecture 2
        if ($cli.fileVersion -cne $hostInfo.fileVersion) { throw 'The native pair must have matching embedded release versions.' }
        $signing = if ($RequireSigned) { 'signed' } else { 'unsigned' }
        $packageName = "github-adapter-$($cli.fileVersion)-windows-$Architecture-$signing"
        $destination = if ($OutputDirectory) { Get-LocalPath $OutputDirectory } else { Join-Path $artifacts $packageName }
        if ($destination -ieq $artifacts -or -not (Test-InDirectory $destination $artifacts)) {
            throw 'OutputDirectory must be a new directory strictly beneath this checkout''s artifacts directory.'
        }
        Assert-PlainPath $destination $true $false
        foreach ($path in @($sourcePath, $hostPath, $installerPath, $iconPath)) {
            $sourceDirectory = Split-Path -Parent $path
            if ((Test-InDirectory $destination $sourceDirectory) -or (Test-InDirectory $sourceDirectory $destination)) {
                throw 'Source and output directories must be separate, not equal or nested.'
            }
        }
        if (Test-Path -LiteralPath $destination) { throw "Output collision: refusing to overwrite $destination" }
        $parent = Split-Path -Parent $destination
        [IO.Directory]::CreateDirectory($parent) | Out-Null
        $stage = Join-Path $parent ('.package-native-' + [Guid]::NewGuid().ToString('N'))
        $payload = Join-Path $stage 'payload'
        foreach ($directory in @('bin', 'assets', 'tools')) {
            [IO.Directory]::CreateDirectory((Join-Path $payload $directory)) | Out-Null
        }
        Copy-Payload $sourcePath (Join-Path $payload 'bin\github-adapter.exe')
        Copy-Payload $hostPath (Join-Path $payload 'bin\github-adapter-host.exe')
        Copy-Payload $installerPath (Join-Path $payload 'tools\install-native.ps1')
        Copy-Payload $iconPath (Join-Path $payload 'assets\github-adapter-dark.ico')
        foreach ($notice in @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt')) {
            Copy-Payload (Join-Path $root $notice) (Join-Path $payload $notice)
        }
        [IO.File]::WriteAllText((Join-Path $payload 'README.txt'), (Get-UsageText $cli.fileVersion $Architecture), $utf8)
        $manifest = Get-Manifest $payload $Architecture
        if ($manifest.version -cne $cli.fileVersion) { throw 'Source version changed during packaging.' }
        [IO.File]::WriteAllText((Join-Path $payload 'manifest.json'), (Get-ManifestText $manifest), $utf8)
        $archivePath = Join-Path $stage "$packageName.zip"
        Write-Zip $payload $archivePath
        [IO.File]::WriteAllText("$archivePath.sha256", "$(Get-Hash $archivePath)  $packageName.zip`n", $utf8)
        $extraction = Join-Path $stage 'verification'
        $verified = Read-VerifiedZip $archivePath $extraction
        Remove-Item -LiteralPath $payload, $extraction -Recurse -Force
        Assert-PlainPath $destination $true $false
        Move-PackageDirectory $stage $destination
        $stage = $null
        $archivePath = Join-Path $destination "$packageName.zip"
    }
    [ordered]@{
        Archive = $archivePath
        SHA256 = $verified.Hash
        Version = $verified.Manifest.version
        Architecture = $verified.Manifest.architecture
        Signing = $verified.Manifest.signing
        Publisher = $verified.Manifest.publisher
        Verified = $true
        HostStarted = $false
        InstallPerformed = $false
    } | ConvertTo-Json
} finally {
    if ($stage -and (Test-Path -LiteralPath $stage)) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
