[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ReportPath,
    [ValidateRange(1, 259200)]
    [int]$DurationSeconds = 60,
    [ValidateRange(1, 16)]
    [int]$Concurrency = 4,
    [ValidateRange(1, 60)]
    [int]$SampleSeconds = 5,
    [switch]$Resume,
    [switch]$SkipBuild,
    [ValidateRange(0, 259200)]
    [int]$StopAfterSeconds = 0
)

$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7 -or -not $IsWindows) {
    throw 'Use PowerShell 7 on Windows. This launcher never installs tools or changes machine settings.'
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$report = [IO.Path]::GetFullPath($ReportPath, $root)
$relativeReport = [IO.Path]::GetRelativePath($root, $report)
if ([IO.Path]::IsPathRooted($relativeReport) -or $relativeReport.Split('\') -contains '..') {
    throw 'Reports must stay in a dedicated directory below the repository root.'
}
$architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($architecture) {
    'Arm64' { $target = 'aarch64-pc-windows-msvc'; $msvcTarget = 'arm64' }
    'X64' { $target = 'x86_64-pc-windows-msvc'; $msvcTarget = 'amd64' }
    default { throw "Unsupported native Windows architecture: $architecture" }
}
$executable = Join-Path $root "target\$target\release\examples\soak_fixture.exe"

function Get-SourceIdentity {
    $paths = @('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml',
        'crates\adapter-runtime\Cargo.toml', 'crates\adapter-protocol\Cargo.toml',
        'tools\soak-native.ps1')
    foreach ($directory in @('crates\adapter-runtime\src', 'crates\adapter-runtime\examples',
            'crates\adapter-protocol\src')) {
        $paths += Get-ChildItem -LiteralPath (Join-Path $root $directory) -Recurse -File -Filter '*.rs' |
            ForEach-Object { [IO.Path]::GetRelativePath($root, $_.FullName) }
    }
    $manifest = @($paths | Sort-Object -Unique | ForEach-Object {
        "$_`t$((Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $root $_)).Hash.ToLowerInvariant())"
    })
    $hash = [Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($manifest -join "`n"))
    $head = & git --no-pager -C $root rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify the local source HEAD.' }
    $dirty = @(& git --no-pager -C $root status --porcelain --untracked-files=all -- `
        Cargo.toml Cargo.lock rust-toolchain.toml crates\adapter-runtime crates\adapter-protocol tools\soak-native.ps1)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify the local source worktree state.' }
    return @{
        source_sha256 = [Convert]::ToHexString($hash).ToLowerInvariant()
        source_files = $manifest.Count
        source_head = $head.Trim()
        source_dirty = $dirty.Count -gt 0
    }
}

if (-not $SkipBuild) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { throw 'vswhere is missing; no tools were installed.' }
    $installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($installation)) { throw 'MSVC installation not found.' }
    $vsdev = Join-Path $installation.Trim() 'Common7\Tools\VsDevCmd.bat'
    $vsVersion = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationVersion
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $cargo = Join-Path $cargoHome 'bin\cargo.exe'
    $rustc = Join-Path $cargoHome 'bin\rustc.exe'
    $pin = [regex]::Match((Get-Content -LiteralPath (Join-Path $root 'rust-toolchain.toml') -Raw),
        '(?m)^\s*channel\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
    if (-not $pin.Success) { throw 'An exact Rust toolchain pin is required.' }
    $toolchain = "$($pin.Groups['version'].Value)-$target"
    $names = @('PATH', 'TMP', 'TEMP', 'VSCMD_SKIP_SENDTELEMETRY', 'ADAPTER_SOAK_BUILD_IDENTITY')
    $previous = @{}
    foreach ($name in $names) { $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
    Push-Location $root
    try {
        $scratch = New-Item -ItemType Directory -Force -Path '.\target\native-soak-build-scratch'
        $env:TMP = $scratch.FullName
        $env:TEMP = $scratch.FullName
        $env:VSCMD_SKIP_SENDTELEMETRY = '1'
        $env:PATH = "$(Split-Path -Parent $vswhere);$(Split-Path -Parent $cargo);$env:PATH"
        $compilerEnvironment = & $env:ComSpec /d /c "call `"$vsdev`" -arch=$msvcTarget -host_arch=amd64 >nul && set VCToolsVersion && set VCToolsInstallDir && set WindowsSDKVersion"
        if ($LASTEXITCODE -ne 0) { throw 'MSVC environment setup failed.' }
        $compiler = @{}
        foreach ($line in $compilerEnvironment) {
            if ($line -match '^(VCToolsVersion|VCToolsInstallDir|WindowsSDKVersion)=(.*)$') {
                $compiler[$Matches[1]] = $Matches[2].Trim().TrimEnd('\')
            }
        }
        if (-not $compiler.VCToolsVersion -or -not $compiler.WindowsSDKVersion) {
            throw 'MSVC/SDK version attribution is unavailable.'
        }
        $clTarget = if ($msvcTarget -eq 'arm64') { 'arm64' } else { 'x64' }
        $cl = Join-Path $compiler.VCToolsInstallDir "bin\Hostx64\$clTarget\cl.exe"
        $rustVersion = & $rustc "+$toolchain" -vV
        if ($LASTEXITCODE -ne 0) { throw 'Pinned rustc is unavailable.' }
        $build = Get-SourceIdentity
        $build.schema_version = 1
        $build.target = $target
        $build.profile = 'release'
        $build.rustc_verbose = $rustVersion -join "`n"
        $build.rustflags = $env:RUSTFLAGS
        $build.cargo_encoded_rustflags = $env:CARGO_ENCODED_RUSTFLAGS
        $build.visual_studio = $vsVersion.Trim()
        $build.msvc_toolset = $compiler.VCToolsVersion
        $build.msvc_cl_version = (Get-Item -LiteralPath $cl).VersionInfo.FileVersion
        $build.windows_sdk = $compiler.WindowsSDKVersion
        $env:ADAPTER_SOAK_BUILD_IDENTITY = $build | ConvertTo-Json -Depth 5 -Compress
        if ($env:ADAPTER_SOAK_BUILD_IDENTITY.Length -gt 8192) { throw 'Build identity exceeds its bound.' }
        Write-Output "Building offline, pinned native target $target (MSVC $($compiler.VCToolsVersion), SDK $($compiler.WindowsSDKVersion))."
        & $env:ComSpec /d /c "call `"$vsdev`" -arch=$msvcTarget -host_arch=amd64 >nul && `"$cargo`" +$toolchain build --offline --locked --release -p adapter-runtime --example soak_fixture --target $target"
        if ($LASTEXITCODE -ne 0) { throw "Native soak build failed with exit code $LASTEXITCODE." }
    } finally {
        Pop-Location
        foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process') }
    }
}

if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw 'The native soak example is not built. Omit -SkipBuild.'
}
$artifactHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $executable).Hash.ToLowerInvariant()
$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $executable
$startInfo.WorkingDirectory = $root
$startInfo.UseShellExecute = $false
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
foreach ($argument in @('--report', $relativeReport, '--duration-secs', "$DurationSeconds",
        '--concurrency', "$Concurrency", '--sample-secs', "$SampleSeconds", '--stdin-control')) {
    $startInfo.ArgumentList.Add($argument)
}
if ($Resume) { $startInfo.ArgumentList.Add('--resume') }
$process = [Diagnostics.Process]::new()
$process.StartInfo = $startInfo
$started = $false
$forced = $false
$stopSent = $false
$stopSentAtMs = $null
$stopMarker = Join-Path (Split-Path -Parent $report) 'native-soak.stop'
$collector = $null
$wall = [Diagnostics.Stopwatch]::new()
$startedUtc = [DateTimeOffset]::UtcNow.ToString('O')
try {
    if (-not $process.Start()) { throw 'Native process did not start.' }
    $started = $true
    $wall.Start()
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    Write-Output "Owned native soak PID $($process.Id). Report: $report"
    Write-Output "Graceful stop: create $(Join-Path (Split-Path -Parent $report) 'native-soak.stop')"
    while (-not $process.WaitForExit(200)) {
        if (-not $stopSent -and [IO.File]::Exists($stopMarker)) {
            $stopSent = $true
            $stopSentAtMs = $wall.ElapsedMilliseconds
        }
        if (-not $stopSent -and (($StopAfterSeconds -gt 0 -and $wall.Elapsed.TotalSeconds -ge $StopAfterSeconds) `
                -or $wall.Elapsed.TotalSeconds -ge $DurationSeconds + 60)) {
            $process.StandardInput.WriteLine('stop')
            $process.StandardInput.Flush()
            $process.StandardInput.Close()
            $stopSent = $true
            $stopSentAtMs = $wall.ElapsedMilliseconds
        }
        if (($stopSent -and $wall.ElapsedMilliseconds - $stopSentAtMs -ge 30000) `
                -or $wall.Elapsed.TotalSeconds -ge $DurationSeconds + 85) {
            throw 'Owned native process exceeded the stop or overall wall-clock guard.'
        }
    }
} finally {
    if ($started) {
        if (-not $process.HasExited) {
            try {
                $process.StandardInput.WriteLine('stop')
                $process.StandardInput.Flush()
                $process.StandardInput.Close()
            } catch { Write-Warning 'Control input was already closed.' }
            if (-not $process.WaitForExit(20000)) {
                $process.Kill()
                $forced = $true
                if (-not $process.WaitForExit(5000)) { throw "Owned PID $($process.Id) did not exit after termination." }
            }
        }
        $wall.Stop()
        $exitCode = $process.ExitCode
        $out = $stdout.GetAwaiter().GetResult()
        $err = $stderr.GetAwaiter().GetResult()
        Write-Output ($out.Substring(0, [Math]::Min(4096, $out.Length)).Trim())
        if ($err) { Write-Warning ($err.Substring(0, [Math]::Min(4096, $err.Length)).Trim()) }
        if ((Test-Path -LiteralPath $report -PathType Leaf) -and (Get-Item -LiteralPath $report).Length -le 8MB) {
            $native = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
            $ownedRunId = $null
            foreach ($line in ($out -split '\r?\n')) {
                if ($line -and $line.StartsWith('{')) {
                    $announcement = $line | ConvertFrom-Json
                    if ($announcement.event -eq 'ready' -and $announcement.pid -eq $process.Id) {
                        $ownedRunId = $announcement.run_id
                    }
                }
            }
            $run = if ($ownedRunId) {
                $native.runs | Where-Object { $_.id -eq $ownedRunId } | Select-Object -Last 1
            } elseif ($exitCode -ne 0) {
                $native.runs | Where-Object { $_.pid -eq $process.Id } | Select-Object -Last 1
            } else { $null }
            if ($native.schema_version -eq 1 -and $native.kind -eq 'native-runtime-soak' `
                    -and $run.pid -eq $process.Id -and $run.id -match '^\d+-\d+$') {
                $unchanged = $artifactHash -eq (Get-FileHash -Algorithm SHA256 -LiteralPath $executable).Hash.ToLowerInvariant()
                $passed = -not $forced -and $exitCode -eq 0 -and $unchanged -and `
                    $run.state -eq 'completed' -and $run.checks.qualification_passed -eq $true
                $collector = [ordered]@{
                    schema_version = 1
                    kind = 'native-runtime-soak-launch'
                    run_id = $run.id
                    pid = $process.Id
                    executable = $executable
                    artifact_sha256 = $artifactHash
                    artifact_unchanged_through_exit = $unchanged
                    started_utc = $startedUtc
                    finished_utc = [DateTimeOffset]::UtcNow.ToString('O')
                    launcher_wall_elapsed_ms = $wall.ElapsedMilliseconds
                    native_active_elapsed_ms = $run.active_elapsed_ms
                    native_exit_code = $exitCode
                    process_exit_confirmed = $process.HasExited
                    forced_termination = $forced
                    qualification_passed = $passed
                    qualified_24h = $passed -and $run.checks.qualified_24h -eq $true
                    qualified_72h = $passed -and $run.checks.qualified_72h -eq $true
                    note = 'Resource samples belong to the native PID in report.json, not PowerShell. Require BOTH reports and matching run_id.'
                }
                $collectorPath = Join-Path (Split-Path -Parent $report) "native-soak-launch-$($run.id).json"
                $pending = "$collectorPath.pending"
                if (Test-Path -LiteralPath $collectorPath) { throw 'Refusing to overwrite an existing launch result.' }
                [IO.File]::WriteAllText($pending, ($collector | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))
                $file = [IO.File]::Open($pending, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
                try { $file.Flush($true) } finally { $file.Dispose() }
                [IO.File]::Move($pending, $collectorPath)
                Write-Output "Exit-confirmed result: $collectorPath"
            }
        }
        $process.Dispose()
    }
}
if (-not $collector -or -not $collector.qualification_passed) {
    throw "Native soak is incomplete, failed, or lacks required evidence (exit $exitCode). No long-soak completion is claimed."
}
Write-Output "Native bounded soak passed its requested duration. 24h=$($collector.qualified_24h); 72h=$($collector.qualified_72h)."
