param(
    [string]$Source,
    [string]$Destination,
    [switch]$NoDesktopShortcut,
    [switch]$NoConfigurationChange
)

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This installer supports Windows only.' }
# Inbox PowerShell/.NET Framework can report the emulated process architecture
# as the OS architecture. Ask Windows directly before choosing a native pair.
if (-not ('GitHubAdapter.NativeInstaller' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace GitHubAdapter {
    public static class NativeInstaller {
        [DllImport("kernel32.dll", ExactSpelling = true, SetLastError = true)]
        public static extern bool IsWow64Process2(IntPtr process, out ushort processMachine, out ushort nativeMachine);
        [DllImport("shell32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern void SHChangeNotify(int eventId, uint flags, string item, IntPtr unused);
    }
}
'@
}
[UInt16]$processMachine = 0
[UInt16]$nativeMachine = 0
if (-not [GitHubAdapter.NativeInstaller]::IsWow64Process2([IntPtr]::new(-1), [ref]$processMachine, [ref]$nativeMachine)) {
    throw 'The native Windows architecture could not be verified; nothing was installed.'
}
$architecture = switch ($nativeMachine) {
    0xAA64 { 'Arm64' }
    0x8664 { 'X64' }
    default { throw "Unsupported native Windows machine: $nativeMachine" }
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$machine, $target = switch ($architecture) {
    'Arm64' { 0xAA64; 'aarch64-pc-windows-msvc' }
    'X64' { 0x8664; 'x86_64-pc-windows-msvc' }
    default { throw "Unsupported native Windows architecture: $architecture" }
}
if (-not $Source) { $Source = Join-Path $root "target\$target\release\github-adapter.exe" }
if (-not $Destination) {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is required for the native installation.' }
    $Destination = Join-Path $env:LOCALAPPDATA 'GitHubAdapter\bin'
}
$Source = [System.IO.Path]::GetFullPath($Source)
$Destination = [System.IO.Path]::GetFullPath($Destination)
$executable = Join-Path $Destination 'github-adapter.exe'
$hostSource = Join-Path (Split-Path -Parent $Source) 'github-adapter-host.exe'
$hostExecutable = Join-Path $Destination 'github-adapter-host.exe'
$iconSource = Join-Path $root 'assets\github-adapter-dark.ico'
$iconTarget = Join-Path $Destination 'github-adapter-dark.ico'
$noticeNames = @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt')

function Assert-RegularPath([string]$Path, [bool]$Directory = $false, [switch]$AllowCloudPlaceholders) {
    $current = $Path
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
                if (-not $AllowCloudPlaceholders) {
                    throw "Native installation paths must not contain reparse points: $current"
                }
                $detail = & "$env:WINDIR\System32\fsutil.exe" reparsepoint query $current 2>&1
                $tag = [regex]::Match(($detail -join "`n"), '0x(?<tag>[0-9a-fA-F]{8})')
                if ($LASTEXITCODE -ne 0 -or -not $tag.Success) {
                    throw 'Could not verify the desktop cloud-placeholder metadata.'
                }
                $number = [uint32]::Parse($tag.Groups['tag'].Value, [Globalization.NumberStyles]::HexNumber)
                $cloudMask = [Convert]::ToUInt32('FFFF0FFF', 16)
                $cloudTag = [Convert]::ToUInt32('9000001A', 16)
                if (($number -band $cloudMask) -ne $cloudTag) {
                    throw "Desktop shortcut paths may contain cloud placeholders, not redirecting or unknown reparse points: $current"
                }
            }
            if (($current -ne $Path -or $Directory) -and -not $item.PSIsContainer) {
                throw "Expected an installation directory: $current"
            }
            if ($current -eq $Path -and -not $Directory -and $item.PSIsContainer) {
                throw "Expected a native installation file: $current"
            }
        }
        $current = Split-Path -Parent $current
    }
}

function Get-NativeVersion([string]$Path, [int]$Subsystem) {
    Assert-RegularPath $Path
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "The native release pair is incomplete: $Path. Build adapter-app; no interpreter fallback is available."
    }
    $reader = [System.IO.BinaryReader]::new([System.IO.File]::OpenRead($Path))
    try {
        if ($reader.BaseStream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
            throw 'Source is not a Windows executable.'
        }
        $reader.BaseStream.Position = 0x3C
        $offset = $reader.ReadUInt32()
        if ($offset -gt $reader.BaseStream.Length - 94) { throw 'Invalid Windows executable header.' }
        $reader.BaseStream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne $machine) {
            throw 'The executable does not match this native Windows architecture.'
        }
        $reader.BaseStream.Position = $offset + 24
        if ($reader.ReadUInt16() -ne 0x20B) { throw 'A native 64-bit Windows executable is required.' }
        $reader.BaseStream.Position = $offset + 24 + 68
        if ($reader.ReadUInt16() -ne $Subsystem) {
            throw 'The native pair must contain a console CLI and a Windows GUI host.'
        }
    } finally { $reader.Dispose() }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $process.StartInfo.FileName = $Path
    $process.StartInfo.Arguments = '--version'
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    try {
        if (-not $process.Start()) { throw 'The native version check could not start.' }
        $output = $process.StandardOutput.ReadToEndAsync()
        $diagnostic = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(10000)) {
            Stop-Process -Id $process.Id -ErrorAction Stop
            if (-not $process.WaitForExit(2000)) { throw 'The owned version-check process did not stop.' }
            throw 'The native version check timed out.'
        }
        if (-not $output.Wait(2000) -or -not $diagnostic.Wait(2000)) {
            throw 'The native version-check output did not close.'
        }
        $text = $output.Result.Trim()
        if ($process.ExitCode -ne 0 -or $text -notmatch '^github-adapter(?:-host)? (?<version>\d+\.\d+\.\d+)$') {
            throw 'The native executable failed its version check.'
        }
        return [pscustomobject]@{ Text = $text; Number = $Matches.version }
    } finally { $process.Dispose() }
}

function Get-CurrentHash([string]$Path) {
    Assert-RegularPath $Path -AllowCloudPlaceholders:($Path -eq $shortcutPath -and -not $NoDesktopShortcut)
    if (Test-Path -LiteralPath $Path -PathType Leaf) {
        return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    }
    return $null
}

function Assert-Stopped {
    $running = @(Get-CimInstance Win32_Process -Filter "Name='github-adapter.exe' OR Name='github-adapter-host.exe'" |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath -in @($executable, $hostExecutable) })
    if ($running.Count) {
        throw 'The installed native CLI or desktop host is running. Use its stop command before updating; no process was stopped.'
    }
}

function New-StagingDirectory([string]$Parent) {
    $path = Join-Path $Parent ".GitHubAdapter-install-$stamp"
    New-Item -ItemType Directory -Path $path -ErrorAction Stop | Out-Null
    $stagingDirectories.Add($path)
    [System.IO.File]::SetAttributes($path, [System.IO.FileAttributes]::Directory -bor [System.IO.FileAttributes]::Hidden)
    return $path
}

function Add-Update([string]$Temporary, [string]$Target, $OriginalHash) {
    $updates.Add([pscustomobject]@{
        Temporary = $Temporary
        Target = $Target
        OriginalHash = $OriginalHash
        Hash = (Get-FileHash -LiteralPath $Temporary -Algorithm SHA256).Hash
        Backup = Join-Path $Destination "$([System.IO.Path]::GetFileName($Target)).previous-$stamp"
        Attempted = $false
    })
}

Assert-RegularPath $Destination $true
Assert-RegularPath $iconSource
if (-not (Test-Path -LiteralPath $iconSource -PathType Leaf)) { throw 'Application icon is missing.' }
foreach ($name in $noticeNames) {
    $noticeSource = Join-Path $root $name
    Assert-RegularPath $noticeSource
    if (-not (Test-Path -LiteralPath $noticeSource -PathType Leaf)) { throw "Required license notice is missing: $name" }
}
$version = Get-NativeVersion $Source 3
$hostVersion = Get-NativeVersion $hostSource 2
if ($version.Number -ne $hostVersion.Number) { throw 'The CLI and desktop host must have matching release versions.' }

$desktop = if ($NoDesktopShortcut) { $null } else { [Environment]::GetFolderPath('DesktopDirectory') }
$shortcutPath = if ($NoDesktopShortcut) { $null } else { Join-Path $desktop 'GitHub Adapter.lnk' }
$shell = $null
$shortcutChange = $false
$shortcutHash = $null
$iconChanged = (Get-CurrentHash $iconTarget) -ne (Get-CurrentHash $iconSource)
if (-not $NoDesktopShortcut) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcutHash = Get-CurrentHash $shortcutPath
    if ($shortcutHash) {
        $existing = $shell.CreateShortcut($shortcutPath)
        $allowed = @($executable, $hostExecutable, (Join-Path $root 'GitHubAdapter.cmd'), (Join-Path $root 'MAIAdapter.cmd'))
        if ($existing.TargetPath -notin $allowed) {
            throw 'The GitHub Adapter shortcut targets an unrelated application; it was not changed.'
        }
        $shortcutChange = $existing.TargetPath -ne $hostExecutable -or $existing.Arguments -ne '' -or
            $existing.WorkingDirectory -ne $Destination -or $existing.IconLocation -ne "$iconTarget,0" -or $iconChanged
    } else {
        $shortcutChange = $true
    }
}

$hasher = [System.Security.Cryptography.SHA256]::Create()
try {
    $identity = [Text.Encoding]::UTF8.GetBytes($Destination.ToLowerInvariant())
    $lockId = [BitConverter]::ToString($hasher.ComputeHash($identity)).Replace('-', '')
} finally { $hasher.Dispose() }
$mutex = [System.Threading.Mutex]::new($false, "Global\GitHubAdapter-Install-$lockId")
$locked = $false
$stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '-' + [Guid]::NewGuid().ToString('N')
$updates = [System.Collections.Generic.List[object]]::new()
$temporaryFiles = [System.Collections.Generic.List[string]]::new()
$stagingDirectories = [System.Collections.Generic.List[string]]::new()
$recoveryErrors = [System.Collections.Generic.List[string]]::new()
$cleanupErrors = [System.Collections.Generic.List[string]]::new()
$failure = $null
$operation = 'preparing the native installation'
$restoredFiles = 0
try {
    try { $locked = $mutex.WaitOne(0) } catch [System.Threading.AbandonedMutexException] {
        $locked = $true
        throw 'A prior native installation was interrupted. Inspect its native files and recovery copies before retrying.'
    }
    if (-not $locked) { throw 'Another native installation is in progress.' }
    Assert-Stopped
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    $stage = New-StagingDirectory $Destination
    $copyPairs = @(@($Source, $executable), @($hostSource, $hostExecutable), @($iconSource, $iconTarget))
    foreach ($name in $noticeNames) { $copyPairs += ,@((Join-Path $root $name), (Join-Path $Destination $name)) }
    foreach ($pair in $copyPairs) {
        $from, $to = $pair
        $original = Get-CurrentHash $to
        $expected = (Get-FileHash -LiteralPath $from -Algorithm SHA256).Hash
        if ($original -eq $expected) { continue }
        $temporary = Join-Path $stage ([System.IO.Path]::GetFileName($to))
        $temporaryFiles.Add($temporary)
        [System.IO.File]::Copy($from, $temporary, $false)
        if ((Get-FileHash -LiteralPath $temporary -Algorithm SHA256).Hash -ne $expected) {
            throw 'Native installation copy failed integrity verification.'
        }
        Add-Update $temporary $to $original
    }
    if ($shortcutChange) {
        $shortcutStage = if ($desktop -eq $Destination) { $stage } else { New-StagingDirectory $desktop }
        $temporary = Join-Path $shortcutStage 'GitHub Adapter.lnk'
        $temporaryFiles.Add($temporary)
        $link = $shell.CreateShortcut($temporary)
        $link.TargetPath = $hostExecutable
        $link.Arguments = ''
        $link.WorkingDirectory = $Destination
        $link.Description = 'GitHub Adapter - native desktop host, automatic model selection, and Codex'
        $link.IconLocation = "$iconTarget,0"
        $link.Save()
        Add-Update $temporary $shortcutPath $shortcutHash
    }
    foreach ($update in $updates) {
        $operation = "preserving $($update.Target)"
        if ((Get-CurrentHash $update.Target) -ne $update.OriginalHash) {
            throw 'An installation target changed during preparation; it was not overwritten.'
        }
        if ($update.OriginalHash) {
            [System.IO.File]::Copy($update.Target, $update.Backup, $false)
            if ((Get-FileHash -LiteralPath $update.Backup -Algorithm SHA256).Hash -ne $update.OriginalHash) {
                throw 'An installation recovery copy failed integrity verification.'
            }
        }
    }
    $operation = 'checking running native processes'
    Assert-Stopped
    foreach ($update in $updates) {
        $operation = "publishing $($update.Target)"
        if ((Get-CurrentHash $update.Target) -ne $update.OriginalHash) {
            throw 'An installation target changed before publication; it was not overwritten.'
        }
        $update.Attempted = $true
        if ($update.OriginalHash) {
            # PowerShell converts $null to ""; File.Replace needs a genuine null backup path.
            [System.IO.File]::Replace($update.Temporary, $update.Target, [NullString]::Value)
        } else {
            [System.IO.File]::Move($update.Temporary, $update.Target)
        }
    }
    $operation = 'verifying the installed native pair'
    if ((Get-NativeVersion $executable 3).Text -ne $version.Text -or
        (Get-NativeVersion $hostExecutable 2).Text -ne $hostVersion.Text) {
        throw 'Installed executable verification failed.'
    }
    foreach ($update in $updates) {
        if ((Get-CurrentHash $update.Target) -ne $update.Hash) { throw 'Installed file hash verification failed.' }
    }
    if (-not $NoDesktopShortcut) {
        $operation = 'verifying the desktop shortcut'
        $actual = $shell.CreateShortcut($shortcutPath)
        if ($actual.TargetPath -ne $hostExecutable -or $actual.Arguments -ne '' -or
            $actual.WorkingDirectory -ne $Destination -or $actual.IconLocation -ne "$iconTarget,0") {
            throw 'Shortcut read-back did not match the direct native desktop host.'
        }
    }
} catch {
    $failure = "$operation`: $($_.Exception.Message)"
    for ($index = $updates.Count - 1; $index -ge 0; $index--) {
        $update = $updates[$index]
        if (-not $update.Attempted) { continue }
        try {
            $currentHash = Get-CurrentHash $update.Target
            if ($currentHash -eq $update.OriginalHash) { continue }
            if ($currentHash -ne $update.Hash) {
                throw 'The published file changed externally; automatic recovery did not overwrite it.'
            }
            if ($update.OriginalHash) {
                if ((Get-FileHash -LiteralPath $update.Backup -Algorithm SHA256).Hash -ne $update.OriginalHash) {
                    throw 'The recovery copy no longer matches the original file.'
                }
                $restore = "$($update.Temporary).restore"
                $temporaryFiles.Add($restore)
                [System.IO.File]::Copy($update.Backup, $restore, $false)
                [System.IO.File]::Replace($restore, $update.Target, [NullString]::Value)
            } else {
                [System.IO.File]::Delete($update.Target)
            }
            $restoredFiles++
        } catch {
            $recoveryErrors.Add("$($update.Target): $($_.Exception.Message)")
        }
    }
} finally {
    foreach ($temporary in $temporaryFiles) {
        try {
            if ([System.IO.File]::Exists($temporary)) { [System.IO.File]::Delete($temporary) }
        } catch { $cleanupErrors.Add("$temporary`: $($_.Exception.Message)") }
    }
    foreach ($directory in $stagingDirectories) {
        try { [System.IO.Directory]::Delete($directory, $false) }
        catch { $cleanupErrors.Add("$directory`: $($_.Exception.Message)") }
    }
    if ($locked) { $mutex.ReleaseMutex() }
    $mutex.Dispose()
}
if ($failure) {
    $recovery = if ($recoveryErrors.Count) { " Recovery incomplete: $($recoveryErrors -join '; ')." } else { '' }
    $cleanup = if ($cleanupErrors.Count) { " Cleanup incomplete: $($cleanupErrors -join '; ')." } else { '' }
    throw "Native installation failed: $failure$recovery$cleanup Restored $restoredFiles changed file(s). Previous native recovery copies were retained."
}
if ($cleanupErrors.Count) { throw "Native files installed, but staging cleanup failed: $($cleanupErrors -join '; ')" }
# Stable icon paths are intentional. Notify only our files after publication;
# never restart Explorer or delete the user's shared icon cache.
if (-not $NoDesktopShortcut -and $updates.Count -gt 0) {
    try {
        foreach ($path in @($iconTarget, $executable, $hostExecutable, $shortcutPath)) {
            # SHCNE_UPDATEITEM, SHCNF_PATHW: invalidate only the affected Shell items.
            [GitHubAdapter.NativeInstaller]::SHChangeNotify(0x00002000, 0x0005, $path, [IntPtr]::Zero)
        }
    } catch {
        Write-Warning "Native files are installed, but Explorer could not be notified. Refresh the desktop to reload the icon. $($_.Exception.Message)"
    }
}
# A stopped legacy installation must not leave ordinary Codex pointing at its
# dead listener. Test/custom installers can explicitly opt out of client changes.
if (-not $NoConfigurationChange) {
    $recoveryInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $recoveryInfo.FileName = $executable
    $recoveryInfo.Arguments = '--restore-normal' # One fixed argument; compatible with Windows PowerShell 5.1.
    $recoveryInfo.UseShellExecute = $false
    $recoveryInfo.CreateNoWindow = $true
    $recoveryInfo.RedirectStandardError = $true
    $recovery = [System.Diagnostics.Process]::Start($recoveryInfo)
    try {
        $recoveryError = $recovery.StandardError.ReadToEndAsync()
        if (-not $recovery.WaitForExit(30000)) {
            $recovery.Kill()
            throw 'Native files are installed; normal-routing recovery exceeded its deadline. Its independent recovery companion was not terminated.'
        }
        if ($recovery.ExitCode -ne 0) { throw "Native files are installed, but normal routing could not be recovered: $($recoveryError.GetAwaiter().GetResult())" }
    } finally { $recovery.Dispose() }
}
[pscustomobject]@{
    Executable = $executable
    DesktopHost = $hostExecutable
    Version = $version.Text
    DesktopVersion = $hostVersion.Text
    Architecture = $architecture
    SHA256 = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash
    DesktopHostSHA256 = (Get-FileHash -LiteralPath $hostExecutable -Algorithm SHA256).Hash
    IconSHA256 = (Get-FileHash -LiteralPath $iconTarget -Algorithm SHA256).Hash
    ShortcutUpdated = $shortcutChange
    Shortcut = $(if ($NoDesktopShortcut) { $null } else { $shortcutPath })
    NativeOnly = $true
    HostStarted = $false
    NormalRoutingRecovered = -not $NoConfigurationChange
} | ConvertTo-Json
