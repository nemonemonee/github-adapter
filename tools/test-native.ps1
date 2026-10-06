#requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture,
    [string]$Source
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows) { throw 'Native qualification requires Windows.' }
if (-not ('GitHubAdapter.QualificationMachine' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace GitHubAdapter {
    public static class QualificationMachine {
        [DllImport("kernel32.dll", ExactSpelling = true, SetLastError = true)]
        public static extern bool IsWow64Process2(IntPtr process, out ushort processMachine, out ushort nativeMachine);
    }
}
'@
}
[UInt16]$processMachine = 0
[UInt16]$nativeMachine = 0
if (-not [GitHubAdapter.QualificationMachine]::IsWow64Process2([IntPtr]::new(-1), [ref]$processMachine, [ref]$nativeMachine)) {
    throw 'Cannot determine the real native architecture.'
}
$nativeArchitecture = switch ($nativeMachine) {
    0x8664 { 'x64' }
    0xAA64 { 'arm64' }
    default { throw "Unsupported native machine: $nativeMachine" }
}
if (-not $Architecture) { $Architecture = $nativeArchitecture }
if ($Architecture -ne $nativeArchitecture) { throw 'Fixture execution requires the real native architecture; emulation is not qualification.' }
$root = Split-Path -Parent $PSScriptRoot
$target = if ($Architecture -eq 'x64') { 'x86_64-pc-windows-msvc' } else { 'aarch64-pc-windows-msvc' }
if (-not $Source) { $Source = Join-Path $root "target\$target\release\github-adapter.exe" }
$Source = (Resolve-Path -LiteralPath $Source).Path
if ([IO.Path]::GetFileName($Source) -cne 'github-adapter.exe') { throw '-Source must name github-adapter.exe with its GUI sibling.' }
$fixture = Join-Path $root "artifacts\native-test-$([Guid]::NewGuid().ToString('N'))"
$passed = 0
$savedEnvironment = @{}
$originalCargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$originalRustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
function Assert-Check([bool]$Condition, [string]$Name) {
    if (-not $Condition) { throw "FAIL $Name" }
    $script:passed++
    Write-Host "PASS $Name"
}

function Assert-Rejected([scriptblock]$Action, [string]$Message) {
    $failure = $null
    try { $null = & $Action } catch { $failure = $_.Exception.Message }
    Assert-Check ($null -ne $failure -and $failure.Contains($Message)) "reject $Message (received: $failure)"
}

function Get-SHA256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function New-ChangedArchive([string]$Name, [scriptblock]$Change) {
    $directory = Join-Path $fixture "artifacts\$Name"
    New-Item -ItemType Directory -Path $directory | Out-Null
    $archive = Join-Path $directory ([IO.Path]::GetFileName($report.Archive))
    [IO.File]::Copy($report.Archive, $archive)
    $zip = [IO.Compression.ZipFile]::Open($archive, [IO.Compression.ZipArchiveMode]::Update)
    try { & $Change $zip } finally { $zip.Dispose() }
    [IO.File]::WriteAllText("$archive.sha256", "$(Get-SHA256 $archive)  $([IO.Path]::GetFileName($archive))`n", [Text.UTF8Encoding]::new($false))
    return $archive
}

function Invoke-FixtureProcess([string]$Executable, [string]$Arguments) {
    $process = [Diagnostics.Process]::new()
    $process.StartInfo.FileName = $Executable
    $process.StartInfo.Arguments = $Arguments
    $process.StartInfo.WorkingDirectory = $fixture
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    try {
        if (-not $process.Start()) { throw 'Fixture process could not start.' }
        $output = $process.StandardOutput.ReadToEndAsync()
        $diagnostic = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(10000)) {
            $process.Kill($true)
            if (-not $process.WaitForExit(2000)) { throw 'Owned fixture process did not stop.' }
            throw 'Fixture process exceeded its deadline.'
        }
        if (-not $output.Wait(2000) -or -not $diagnostic.Wait(2000)) { throw 'Fixture process output did not close.' }
        return [pscustomobject]@{ ExitCode = $process.ExitCode; Output = $output.Result; Error = $diagnostic.Result }
    } finally { $process.Dispose() }
}

try {
    foreach ($directory in @('tools', 'assets', 'artifacts', 'input')) {
        New-Item -ItemType Directory -Path (Join-Path $fixture $directory) | Out-Null
    }
    foreach ($relative in @('tools\package-native.ps1', 'tools\install-native.ps1', 'assets\github-adapter-dark.ico', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt')) {
        [IO.File]::Copy((Join-Path $root $relative), (Join-Path $fixture $relative))
    }
    foreach ($name in @('github-adapter.exe', 'github-adapter-host.exe')) {
        [IO.File]::Copy((Join-Path (Split-Path -Parent $Source) $name), (Join-Path $fixture "input\$name"))
    }
    $approvedSources = @{
        svg = 'ea163aabdf420579775597f7f9bab6ae8c7d5f2b5198499f170612385fcd55c9'
        png = '275f82e1307100e637c37a7a64c91299feeb53b4ecfee39569889f8def731a3a'
    }
    foreach ($extension in $approvedSources.Keys) {
        Assert-Check ((Get-SHA256 (Join-Path $root "assets\github-adapter-dark.$extension")) -ceq $approvedSources[$extension]) "approved $extension source unchanged; changed artwork requires reapproval, ICO regeneration and native rebuild"
    }
    if (-not ('GitHubAdapter.QualificationTrayArtwork' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
namespace GitHubAdapter {
    public static class QualificationTrayArtwork {
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
            return new InvalidDataException("Embedded tray group 2 does not match the canonical ICO; rebuild both native executables.");
        }
        static byte[] Resource(IntPtr module, int kind, int identifier) {
            IntPtr resource = FindResourceW(module, new IntPtr(identifier), new IntPtr(kind));
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
            if (icon.Length < 166 || BitConverter.ToUInt16(icon, 0) != 0 ||
                BitConverter.ToUInt16(icon, 2) != 1 || BitConverter.ToUInt16(icon, 4) != 10) throw Mismatch();
            IntPtr module = LoadLibraryExW(executable, IntPtr.Zero, 0x60);
            if (module == IntPtr.Zero) throw Mismatch();
            try {
                byte[] group = Resource(module, 14, 2);
                if (group.Length != 146) throw Mismatch();
                for (int index = 0; index < 6; index++) if (group[index] != icon[index]) throw Mismatch();
                for (int index = 0; index < 10; index++) {
                    int entry = 6 + index * 16, embedded = 6 + index * 14;
                    for (int field = 0; field < 12; field++) {
                        if (field != 4 && field != 5 && icon[entry + field] != group[embedded + field]) throw Mismatch();
                    }
                    int planes = BitConverter.ToUInt16(icon, entry + 4);
                    if (planes > 1 || BitConverter.ToUInt16(group, embedded + 4) != 1) throw Mismatch();
                    uint size = BitConverter.ToUInt32(icon, entry + 8);
                    uint offset = BitConverter.ToUInt32(icon, entry + 12);
                    if (size == 0 || offset < 166 || (ulong)offset + size > (ulong)icon.Length) throw Mismatch();
                    byte[] frame = Resource(module, 3, BitConverter.ToUInt16(group, embedded + 12));
                    if (frame.Length != size) throw Mismatch();
                    for (int pixel = 0; pixel < frame.Length; pixel++) if (frame[pixel] != icon[(int)offset + pixel]) throw Mismatch();
                }
            } finally { FreeLibrary(module); }
        }
    }
}
'@
    }
    foreach ($name in @('github-adapter.exe', 'github-adapter-host.exe')) {
        [GitHubAdapter.QualificationTrayArtwork]::Verify((Join-Path $fixture "input\$name"), (Join-Path $root 'assets\github-adapter-tray.ico'))
        Assert-Check $true "data-only tray resource group 2: all ten frame headers and bytes match canonical ICO in $name ($Architecture)"
    }
    foreach ($name in @('LOCALAPPDATA', 'APPDATA', 'USERPROFILE', 'HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR')) {
        $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        $directory = Join-Path $fixture "private-state\$name"
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
        [IO.File]::WriteAllText((Join-Path $directory 'sentinel.json'), (@{ untouched = $true } | ConvertTo-Json))
        [Environment]::SetEnvironmentVariable($name, $directory, 'Process')
    }
    $buildFixture = Join-Path $fixture 'build-flags'
    foreach ($directory in @('tools', 'src')) {
        New-Item -ItemType Directory -Path (Join-Path $buildFixture $directory) | Out-Null
    }
    [IO.File]::Copy((Join-Path $root 'tools\build-windows.ps1'), (Join-Path $buildFixture 'tools\build-windows.ps1'))
    [IO.File]::Copy((Join-Path $root 'rust-toolchain.toml'), (Join-Path $buildFixture 'rust-toolchain.toml'))
    [IO.File]::WriteAllText((Join-Path $buildFixture 'Cargo.toml'), @'
[package]
name = "static-crt-contract"
version = "0.0.0"
edition = "2024"
[features]
kept = []
ignored = []
forced_failure = []
[workspace]
'@)
    [IO.File]::WriteAllText((Join-Path $buildFixture 'src\lib.rs'), @'
#[cfg(not(target_feature = "crt-static"))]
compile_error!("Static runtime was not effective");
#[cfg(not(feature = "kept"))]
compile_error!("Inherited compiler flag was lost");
#[cfg(feature = "ignored")]
compile_error!("Encoded flags did not take precedence");
#[cfg(feature = "forced_failure")]
compile_error!("Controlled build flag fixture failure");
'@)
    $buildEnvironment = @{}
    $buildNames = @('USERPROFILE', 'CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR', 'CARGO_NET_OFFLINE', 'GITHUB_ADAPTER_VSDEV', 'GITHUB_ADAPTER_CARGO', 'VSCMD_SKIP_SENDTELEMETRY', 'PATH', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')
    foreach ($name in $buildNames) { $buildEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
    try {
        $env:USERPROFILE = Join-Path $buildFixture 'profile'
        New-Item -ItemType Directory -Path $env:USERPROFILE | Out-Null
        $env:CARGO_HOME = $originalCargoHome
        $env:RUSTUP_HOME = $originalRustupHome
        $env:CARGO_NET_OFFLINE = 'true'
        foreach ($case in @('unencoded', 'encoded', 'compiler-failure')) {
            $env:CARGO_TARGET_DIR = Join-Path $buildFixture "target\$case"
            $env:RUSTFLAGS = '--cfg feature="kept" -C target-feature=-crt-static'
            Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
            if ($case -ne 'unencoded') {
                $env:RUSTFLAGS = '--cfg feature="ignored" -C target-feature=-crt-static'
                $flags = @('--cfg', 'feature="kept"', '-C', 'target-feature=-crt-static')
                if ($case -eq 'compiler-failure') { $flags += @('--cfg', 'feature="forced_failure"') }
                $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
            }
            $before = @{}
            foreach ($name in $buildNames) { $before[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
            $locationBefore = (Get-Location).Path
            $failure = $null
            $buildOutput = [Collections.Generic.List[string]]::new()
            try {
                & (Join-Path $buildFixture 'tools\build-windows.ps1') -Action check -Architecture $Architecture -StaticRuntime 2>&1 | ForEach-Object {
                    $buildOutput.Add("$_")
                    Write-Host "$_"
                }
            } catch { $failure = $_.Exception.Message }
            finally {
                $restored = (Get-Location).Path -ceq $locationBefore
                foreach ($name in $buildNames) {
                    $restored = $restored -and [object]::Equals([Environment]::GetEnvironmentVariable($name, 'Process'), $before[$name])
                }
                Assert-Check $restored "exact process build environment and location restoration: $case"
            }
            if ($case -eq 'compiler-failure') {
                $diagnostic = $buildOutput -join "`n"
                Assert-Check ($null -ne $failure -and $failure.Contains('Cargo check failed with exit code') -and $diagnostic.Contains('Controlled build flag fixture failure') -and $diagnostic -notmatch 'Static runtime was not effective|Inherited compiler flag was lost|Encoded flags did not take precedence') 'controlled compiler failure with effective static CRT and preserved encoded flags'
            } else {
                Assert-Check ($null -eq $failure) "compiled static CRT overrides -crt-static and preserves inherited cfg: $case (failure: $failure)"
            }
        }
    } finally {
        foreach ($name in $buildNames) {
            if ($null -eq $buildEnvironment[$name]) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
            else { [Environment]::SetEnvironmentVariable($name, $buildEnvironment[$name], 'Process') }
        }
    }
    $packager = Join-Path $fixture 'tools\package-native.ps1'
    $inputCli = Join-Path $fixture 'input\github-adapter.exe'
    $report = & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'artifacts\first') | ConvertFrom-Json
    if (-not $report.Verified -or $report.HostStarted -or $report.InstallPerformed) { throw 'Unexpected package qualification report.' }
    Assert-Check ($report.Architecture -eq $Architecture -and $report.Signing -eq 'unsigned' -and $null -eq $report.Publisher) 'native pair, PE resources, canonical artwork, unsigned package verification'
    $verified = & $packager -Verify $report.Archive -ExpectedSHA256 $report.SHA256 | ConvertFrom-Json
    Assert-Check ($verified.Verified -and $verified.SHA256 -eq (Get-SHA256 $report.Archive)) 'archive SHA256 verification'
    $zip = [IO.Compression.ZipFile]::OpenRead($report.Archive)
    try {
        $expected = @('bin/github-adapter.exe', 'bin/github-adapter-host.exe', 'assets/github-adapter-dark.ico', 'tools/install-native.ps1', 'README.txt', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt', 'manifest.json')
        Assert-Check (($zip.Entries.FullName -join '|') -ceq ($expected -join '|')) 'exact ordered archive payload'
        $reader = [IO.StreamReader]::new($zip.GetEntry('manifest.json').Open())
        try { $manifest = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        Assert-Check ($manifest.schemaVersion -eq 2 -and $manifest.target -eq $target -and $manifest.version -eq $report.Version -and $manifest.files.Count -eq 8) 'manifest schema, target and version'
        foreach ($record in $manifest.files) {
            $entryStream = $zip.GetEntry($record.path).Open()
            try { $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($entryStream)).ToLowerInvariant() } finally { $entryStream.Dispose() }
            Assert-Check ($hash -ceq $record.sha256 -and $zip.GetEntry($record.path).Length -eq $record.length) "manifest hash and size: $($record.path)"
        }
        Assert-Check ($manifest.files[0].pe.subsystem -eq 3 -and $manifest.files[1].pe.subsystem -eq 2 -and $manifest.files[0].pe.fileVersion -eq $manifest.files[1].pe.fileVersion) 'CLI console and GUI subsystem, matching embedded versions'
    } finally { $zip.Dispose() }
    [IO.File]::SetLastWriteTimeUtc($inputCli, [DateTime]::new(2001, 2, 3))
    $second = & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'artifacts\second') | ConvertFrom-Json
    Assert-Check ($second.SHA256 -ceq $report.SHA256) 'reproducible bytes across timestamps and output locations'
    Assert-Rejected { & $packager -Verify $report.Archive -ExpectedSHA256 ('0' * 64) } 'Archive does not match -ExpectedSHA256'
    Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'artifacts\first') } 'Output collision'
    Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'outside') } 'strictly beneath'
    Assert-Rejected { & $packager -Architecture $Architecture -Source "$inputCli`:stream" -Unsigned } 'alternate data streams'
    Assert-Rejected { & $packager -Architecture $Architecture -Source (Join-Path $fixture 'input\..\input\github-adapter.exe') -Unsigned } 'without traversal'
    Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -RequireSigned } 'requires the exact certificate Subject'
    Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -RequireSigned -ExpectedPublisher 'CN=Qualification Unapproved Publisher' -OutputDirectory (Join-Path $fixture 'artifacts\signed') } 'A valid Authenticode signature is required'
    Assert-Rejected { & $packager -Verify $report.Archive -RequireSigned -ExpectedPublisher 'CN=Qualification Unapproved Publisher' } 'Package signing label does not match'
    $sidecar = "$($report.Archive).sha256"
    $originalSidecar = [IO.File]::ReadAllText($sidecar)
    try {
        [IO.File]::WriteAllText($sidecar, ('0' * 64))
        Assert-Rejected { & $packager -Verify $report.Archive } 'Archive SHA256 sidecar does not match'
    } finally { [IO.File]::WriteAllText($sidecar, $originalSidecar, [Text.UTF8Encoding]::new($false)) }
    $archive = New-ChangedArchive 'payload-tamper' {
        param($zip)
        $stream = $zip.GetEntry('tools/install-native.ps1').Open()
        try { $stream.Position = $stream.Length; $stream.WriteByte(32) } finally { $stream.Dispose() }
    }
    Assert-Rejected { & $packager -Verify $archive } 'Payload SHA256 manifest or release metadata does not match'
    $archive = New-ChangedArchive 'manifest-tamper' {
        param($zip)
        $entry = $zip.GetEntry('manifest.json')
        $reader = [IO.StreamReader]::new($entry.Open())
        try { $data = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        $data.version = '0.0.0'
        $stream = $entry.Open()
        try {
            $stream.SetLength(0)
            $writer = [IO.StreamWriter]::new($stream, [Text.UTF8Encoding]::new($false))
            try { $writer.Write(($data | ConvertTo-Json -Depth 8)) } finally { $writer.Dispose() }
        } finally { $stream.Dispose() }
    }
    Assert-Rejected { & $packager -Verify $archive } 'Payload SHA256 manifest or release metadata does not match'
    foreach ($badName in @('../escape.txt', 'bin/github-adapter.exe', 'BIN/github-adapter.exe')) {
        $archive = New-ChangedArchive ([Guid]::NewGuid().ToString('N')) {
            param($zip)
            $zip.GetEntry('README.txt').Delete()
            $stream = $zip.CreateEntry($badName).Open()
            try { $stream.WriteByte(32) } finally { $stream.Dispose() }
        }
        Assert-Rejected { & $packager -Verify $archive } 'Unexpected or duplicate package archive path'
    }
    $archive = New-ChangedArchive 'link-entry' {
        param($zip)
        $zip.GetEntry('README.txt').ExternalAttributes = 0x10
    }
    Assert-Rejected { & $packager -Verify $archive } 'Archive links and nonregular entries are not allowed'
    $archive = New-ChangedArchive 'noncanonical' {
        param($zip)
        $zip.GetEntry('README.txt').LastWriteTime = [DateTimeOffset]::new(2001, 2, 3, 0, 0, 0, [TimeSpan]::Zero)
    }
    Assert-Rejected { & $packager -Verify $archive } 'Archive is not the canonical ZIP representation'
    $stream = [IO.File]::Open($archive, [IO.FileMode]::Append)
    try { $stream.WriteByte(32) } finally { $stream.Dispose() }
    Assert-Rejected { & $packager -Verify $archive } 'Archive SHA256 sidecar does not match'
    $installer = Join-Path $fixture 'tools\install-native.ps1'
    $destination = Join-Path $fixture 'installed native'
    $installArguments = @{ Source = $inputCli; Destination = $destination; NoDesktopShortcut = $true; NoConfigurationChange = $true }
    foreach ($attempt in 1..2) {
        $installed = & $installer @installArguments | ConvertFrom-Json
        Assert-Check ($installed.NativeOnly -and -not $installed.HostStarted -and -not $installed.NormalRoutingRecovered -and -not $installed.ShortcutUpdated -and $null -eq $installed.Shortcut) "isolated installer safety report, attempt $attempt"
        Assert-Check ($installed.Executable -eq (Join-Path $destination 'github-adapter.exe') -and $installed.DesktopHost -eq (Join-Path $destination 'github-adapter-host.exe')) "explicit install destination, attempt $attempt"
        foreach ($item in @(
            @('github-adapter.exe', $inputCli, $installed.SHA256),
            @('github-adapter-host.exe', (Join-Path $fixture 'input\github-adapter-host.exe'), $installed.DesktopHostSHA256),
            @('github-adapter-dark.ico', (Join-Path $fixture 'assets\github-adapter-dark.ico'), $installed.IconSHA256)
        )) {
            $expectedHash = Get-SHA256 $item[1]
            Assert-Check ((Get-SHA256 (Join-Path $destination $item[0])) -eq $expectedHash -and $item[2] -eq $expectedHash) "installed bytes and report hash: $($item[0]), attempt $attempt"
        }
        foreach ($name in @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt')) {
            Assert-Check ((Get-SHA256 (Join-Path $destination $name)) -eq (Get-SHA256 (Join-Path $fixture $name))) "installed notice retained: $name, attempt $attempt"
        }
        Assert-Check (@(Get-ChildItem -LiteralPath $destination -Force).Count -eq 6) "idempotent installation has no extra files, attempt $attempt"
    }
    $sentinel = Join-Path $destination 'unrelated.txt'
    $recovery = Join-Path $destination 'github-adapter.exe.previous-unrelated'
    [IO.File]::WriteAllText($sentinel, 'unrelated fixture sentinel')
    [IO.File]::WriteAllText($recovery, 'unrelated recovery copy')
    $oldIcon = Join-Path $destination 'github-adapter-dark.ico'
    [IO.File]::WriteAllText($oldIcon, 'previous fixture icon')
    $oldIconHash = Get-SHA256 $oldIcon
    $null = & $installer @installArguments | ConvertFrom-Json
    $backups = @(Get-ChildItem -LiteralPath $destination -Filter 'github-adapter-dark.ico.previous-*')
    Assert-Check ($backups.Count -eq 1 -and (Get-SHA256 $backups[0].FullName) -eq $oldIconHash) 'changed icon preserves original recovery bytes'
    $null = & $installer @installArguments | ConvertFrom-Json
    Assert-Check (@(Get-ChildItem -LiteralPath $destination -Filter '*.previous-*').Count -eq 2 -and [IO.File]::ReadAllText($sentinel) -ceq 'unrelated fixture sentinel' -and [IO.File]::ReadAllText($recovery) -ceq 'unrelated recovery copy') 'reinstallation preserves unrelated sentinels and recovery copies'
    $rollbackSource = Join-Path $fixture 'rollback-input'
    New-Item -ItemType Directory -Path $rollbackSource | Out-Null
    $priorHashes = @{}
    foreach ($name in @('github-adapter.exe', 'github-adapter-host.exe', 'github-adapter-dark.ico', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt')) {
        $priorHashes[$name] = Get-SHA256 (Join-Path $destination $name)
        if ($name -notin @('github-adapter.exe', 'github-adapter-host.exe')) { continue }
        $changedPath = Join-Path $rollbackSource $name
        [IO.File]::Copy((Join-Path $fixture "input\$name"), $changedPath)
        $stream = [IO.File]::Open($changedPath, [IO.FileMode]::Append)
        try { $stream.Write([Text.Encoding]::ASCII.GetBytes('OWNED_ROLLBACK_FIXTURE')) } finally { $stream.Dispose() }
        Assert-Check ((Get-SHA256 $changedPath) -ne $priorHashes[$name]) "rollback fixture changes native bytes: $name"
    }
    $faultInstaller = Join-Path $fixture 'tools\install-fault.ps1'
    $installerText = [IO.File]::ReadAllText($installer)
    $anchor = '$update.Attempted = $true'
    if ($installerText.Split($anchor, [StringSplitOptions]::None).Count -ne 2) { throw 'Expected one installer publication injection anchor.' }
    $injection = 'if ($update.Target -eq $hostExecutable) { throw ''Controlled second publication failure'' }; '
    [IO.File]::WriteAllText($faultInstaller, $installerText.Replace($anchor, $injection + $anchor))
    $rollbackFailure = $null
    try {
        $null = & $faultInstaller -Source (Join-Path $rollbackSource 'github-adapter.exe') -Destination $destination -NoDesktopShortcut -NoConfigurationChange
    } catch { $rollbackFailure = $_.Exception.Message }
    Assert-Check ($null -ne $rollbackFailure -and $rollbackFailure -match 'Native installation failed: publishing .*github-adapter-host\.exe: Controlled second publication failure' -and $rollbackFailure.Contains('Restored 1 changed file(s).') -and $rollbackFailure -notmatch 'Recovery incomplete|Cleanup incomplete') 'controlled second publication failure restores the first published binary'
    foreach ($name in $priorHashes.Keys) {
        Assert-Check ((Get-SHA256 (Join-Path $destination $name)) -eq $priorHashes[$name]) "rollback restores prior installed bytes: $name"
    }
    foreach ($name in @('github-adapter.exe', 'github-adapter-host.exe')) {
        $copies = @(Get-ChildItem -LiteralPath $destination -Filter "$name.previous-*" | Where-Object { $_.FullName -ne $recovery })
        Assert-Check ($copies.Count -eq 1 -and (Get-SHA256 $copies[0].FullName) -eq $priorHashes[$name]) "rollback retains original recovery bytes: $name"
    }
    Assert-Check ([IO.File]::ReadAllText($sentinel) -ceq 'unrelated fixture sentinel' -and [IO.File]::ReadAllText($recovery) -ceq 'unrelated recovery copy' -and (Get-SHA256 $backups[0].FullName) -eq $oldIconHash) 'rollback preserves unrelated sentinels and existing recovery copies'
    $junction = Join-Path $fixture 'artifacts\junction'
    try {
        $null = New-Item -ItemType Junction -Path $junction -Target (Join-Path $fixture 'input')
        Assert-Check ([bool]((Get-Item -LiteralPath $junction -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) 'owned unprivileged junction fixture is a reparse point'
        $junctionCli = Join-Path $junction 'github-adapter.exe'
        $junctionOutput = Join-Path $junction 'output'
        Assert-Rejected { & $packager -Architecture $Architecture -Source $junctionCli -Unsigned } 'links or reparse points'
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory $junctionOutput } 'links or reparse points'
        Assert-Rejected { & $installer -Source $junctionCli -Destination $destination -NoDesktopShortcut -NoConfigurationChange } 'must not contain reparse points'
        Assert-Rejected { & $installer -Source $inputCli -Destination $junctionOutput -NoDesktopShortcut -NoConfigurationChange } 'must not contain reparse points'
        Assert-Check (-not [IO.Directory]::Exists((Join-Path $fixture 'input\output'))) 'junction rejection creates no redirected output'
        foreach ($name in @('github-adapter.exe', 'github-adapter-host.exe')) {
            Assert-Check ((Get-SHA256 (Join-Path $fixture "input\$name")) -eq $priorHashes[$name] -and (Get-SHA256 (Join-Path $destination $name)) -eq $priorHashes[$name]) "junction rejection preserves source and installed bytes: $name"
        }
    } finally {
        if ([IO.Directory]::Exists($junction)) { [IO.Directory]::Delete($junction) }
    }
    $inputHost = Join-Path $fixture 'input\github-adapter-host.exe'
    $savedHost = Join-Path $fixture 'input\saved-host.exe'
    [IO.File]::Move($inputHost, $savedHost)
    try {
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'artifacts\incomplete') } 'Required package input is missing'
        Assert-Rejected { & $installer @installArguments } 'native release pair is incomplete'
        Assert-Check ((Get-SHA256 (Join-Path $destination 'github-adapter-host.exe')) -eq (Get-SHA256 $savedHost)) 'incomplete source leaves installed host untouched'
        [IO.File]::Copy($inputCli, $inputHost)
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'artifacts\masquerade') } 'console CLI (3) and Windows GUI host (2)'
        Assert-Rejected { & $installer @installArguments } 'console CLI and a Windows GUI host'
    } finally {
        if ([IO.File]::Exists($inputHost)) { [IO.File]::Delete($inputHost) }
        [IO.File]::Move($savedHost, $inputHost)
    }
    $originalHost = [IO.File]::ReadAllBytes($inputHost)
    $peOffset = [BitConverter]::ToInt32($originalHost, 0x3C)
    try {
        $changed = [byte[]]$originalHost.Clone()
        $wrongMachine = if ($Architecture -eq 'x64') { [UInt16]0xAA64 } else { [UInt16]0x8664 }
        [BitConverter]::GetBytes($wrongMachine).CopyTo($changed, $peOffset + 4)
        [IO.File]::WriteAllBytes($inputHost, $changed)
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned } 'PE machine does not match'
        Assert-Rejected { & $installer @installArguments } 'does not match this native Windows architecture'
        [IO.File]::WriteAllBytes($inputHost, $originalHost[0..($peOffset + 128)])
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned } 'Invalid executable PE structure'
        $changed = [byte[]]$originalHost.Clone()
        $sections = [BitConverter]::ToUInt16($changed, $peOffset + 6)
        $optionalSize = [BitConverter]::ToUInt16($changed, $peOffset + 20)
        $resourceStart = 0
        $resourceEnd = 0
        foreach ($index in 0..($sections - 1)) {
            $section = $peOffset + 24 + $optionalSize + 40 * $index
            if ([Text.Encoding]::ASCII.GetString($changed, $section, 8).TrimEnd([char]0) -eq '.rsrc') {
                $resourceStart = [BitConverter]::ToInt32($changed, $section + 20)
                $resourceEnd = $resourceStart + [BitConverter]::ToInt32($changed, $section + 16)
            }
        }
        if ($resourceStart -eq 0 -or $resourceEnd -gt $changed.Length) { throw 'Missing bounded version resource fixture.' }
        $version = [Version]$report.Version
        $newPatch = $version.Build - $version.Build % 10 + (($version.Build + 1) % 10)
        $oldText = [Text.Encoding]::Unicode.GetBytes("$($report.Version)`0")
        $newText = [Text.Encoding]::Unicode.GetBytes("$($version.Major).$($version.Minor).$newPatch`0")
        if ($oldText.Length -ne $newText.Length) { throw 'Version fixture must preserve resource size.' }
        $stringChanges = 0
        $fixedChanges = 0
        for ($offset = $resourceStart; $offset -le $resourceEnd - [Math]::Max($oldText.Length, 24); $offset++) {
            if ([BitConverter]::ToUInt32($changed, $offset) -eq 0xFEEF04BDL) {
                [BitConverter]::GetBytes([UInt32]($newPatch -shl 16)).CopyTo($changed, $offset + 12)
                [BitConverter]::GetBytes([UInt32]($newPatch -shl 16)).CopyTo($changed, $offset + 20)
                $fixedChanges++
            }
            $matchesVersion = $true
            for ($index = 0; $index -lt $oldText.Length; $index++) {
                if ($changed[$offset + $index] -ne $oldText[$index]) { $matchesVersion = $false; break }
            }
            if ($matchesVersion) { $newText.CopyTo($changed, $offset); $stringChanges++ }
        }
        if ($stringChanges -ne 2 -or $fixedChanges -ne 1) { throw 'Expected exactly two version strings and one fixed resource in the fixture.' }
        [IO.File]::WriteAllBytes($inputHost, $changed)
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned } 'matching embedded release versions'
    } finally { [IO.File]::WriteAllBytes($inputHost, $originalHost) }
    $iconPath = Join-Path $fixture 'assets\github-adapter-dark.ico'
    $originalIcon = [IO.File]::ReadAllBytes($iconPath)
    try {
        $changedIcon = [byte[]]$originalIcon.Clone()
        $changedIcon[$changedIcon.Length - 1] = $changedIcon[$changedIcon.Length - 1] -bxor 1
        [IO.File]::WriteAllBytes($iconPath, $changedIcon)
        Assert-Rejected { & $packager -Architecture $Architecture -Source $inputCli -Unsigned -OutputDirectory (Join-Path $fixture 'artifacts\wrong-artwork') } 'Embedded application icon does not match'
    } finally { [IO.File]::WriteAllBytes($iconPath, $originalIcon) }
    $linkedDirectory = Join-Path $fixture 'artifacts\linked-input'
    New-Item -ItemType Directory -Path $linkedDirectory | Out-Null
    $linkedCli = Join-Path $linkedDirectory 'github-adapter.exe'
    New-Item -ItemType HardLink -Path $linkedCli -Target $inputCli | Out-Null
    try {
        Assert-Rejected { & $packager -Architecture $Architecture -Source $linkedCli -Unsigned } 'not a hard link'
    } finally { [IO.File]::Delete($linkedCli) }
    [IO.File]::Copy($inputCli, $linkedCli)
    [IO.File]::Copy($inputHost, (Join-Path $linkedDirectory 'github-adapter-host.exe'))
    Assert-Rejected { & $packager -Architecture $Architecture -Source $linkedCli -Unsigned -OutputDirectory (Join-Path $linkedDirectory 'output') } 'Source and output directories must be separate'
    foreach ($name in @('GitHubAdapter.cmd', 'MAIAdapter.cmd')) {
        $wrapper = Join-Path $fixture $name
        [IO.File]::Copy((Join-Path $root $name), $wrapper)
        Assert-Check ([IO.File]::ReadAllText($wrapper) -notmatch '(?i)python|pythonw|\bpy\b|adapter\.py') "no interpreter fallback in $name"
    }
    $wrapperDirectory = Join-Path $env:LOCALAPPDATA 'GitHubAdapter\bin'
    New-Item -ItemType Directory -Path $wrapperDirectory | Out-Null
    foreach ($name in @('GitHubAdapter.cmd', 'MAIAdapter.cmd')) {
        $wrapper = Join-Path $fixture $name
        $result = Invoke-FixtureProcess $env:ComSpec ('/d /s /c ""{0}" --version"' -f $wrapper)
        Assert-Check ($result.ExitCode -eq 1 -and $result.Output.Contains('native executable is not installed')) "missing native binary fails explicitly: $name"
    }
    $wrapperCli = Join-Path $wrapperDirectory 'github-adapter.exe'
    [IO.File]::Copy($inputCli, $wrapperCli)
    try {
        foreach ($name in @('GitHubAdapter.cmd', 'MAIAdapter.cmd')) {
            foreach ($argument in @('--version', '--not-an-adapter-option')) {
                $result = Invoke-FixtureProcess $env:ComSpec ('/d /s /c ""{0}" {1}"' -f (Join-Path $fixture $name), $argument)
                if ($argument -eq '--version') {
                    Assert-Check ($result.ExitCode -eq 0 -and $result.Output.Trim() -ceq "github-adapter $($report.Version)") "native installed CLI version forwarding: $name"
                } else {
                    Assert-Check ($result.ExitCode -eq 2) "native CLI failure exit-code forwarding: $name"
                }
            }
        }
    } finally { [IO.Directory]::Delete((Join-Path $env:LOCALAPPDATA 'GitHubAdapter'), $true) }
    $result = Invoke-FixtureProcess $inputHost '--version'
    Assert-Check ($result.ExitCode -eq 0 -and $result.Output.Trim() -cin @("github-adapter $($report.Version)", "github-adapter-host $($report.Version)")) 'bounded GUI version command without desktop launch'
    foreach ($name in $savedEnvironment.Keys) {
        $directory = [Environment]::GetEnvironmentVariable($name, 'Process')
        $items = @(Get-ChildItem -LiteralPath $directory -Force -Recurse)
        Assert-Check ($items.Count -eq 1 -and $items[0].Name -eq 'sentinel.json' -and ([IO.File]::ReadAllText($items[0].FullName) | ConvertFrom-Json).untouched) "isolated user state unchanged: $name"
    }
    Assert-Check (@(Get-ChildItem -LiteralPath $fixture -Directory -Recurse -Force | Where-Object { $_.Name -match '^\.(package-native-|verify-native-|GitHubAdapter-install-)' }).Count -eq 0) 'no owned package, verifier or installer staging leaks'
    Write-Host "Native qualification: $passed passed, 0 failed ($Architecture)."
} finally {
    foreach ($name in $savedEnvironment.Keys) {
        if ($null -eq $savedEnvironment[$name]) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    }
    if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
}
