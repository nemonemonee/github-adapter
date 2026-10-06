param(
    [ValidateSet('check', 'test', 'build', 'clippy')]
    [string]$Action = 'test',
    [ValidateSet('native', 'arm64', 'x64')]
    [string]$Architecture = 'native',
    [switch]$AllowEmulation,
    [switch]$Release,
    [switch]$StaticRuntime,
    [switch]$Locked,
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9_-]*$')]
    [string]$Package,
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9_-]*$')]
    [string]$Test,
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9_-]*$')]
    [string]$Example
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows -and $env:OS -ne 'Windows_NT') {
    throw 'This helper configures the native Windows MSVC environment.'
}
$hostArchitecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$hostTarget = switch ($hostArchitecture) {
    'Arm64' { 'aarch64-pc-windows-msvc' }
    'X64' { 'x86_64-pc-windows-msvc' }
    default { throw "Unsupported Windows build host: $hostArchitecture" }
}
$targetArchitecture = if ($Architecture -eq 'native') { $hostArchitecture } else { $Architecture }
switch ($targetArchitecture) {
    'Arm64' { $target = 'aarch64-pc-windows-msvc'; $msvcTarget = 'arm64' }
    'X64' { $target = 'x86_64-pc-windows-msvc'; $msvcTarget = 'amd64' }
    default { throw "Unsupported Windows artifact architecture: $targetArchitecture" }
}
if ($AllowEmulation -and $Action -ne 'test') { throw '-AllowEmulation applies only to test execution.' }
$execution = 'compile only'
if ($Action -eq 'test') {
    $execution = 'native'
    if ($target -ne $hostTarget) {
        if (-not $AllowEmulation) {
            throw 'Cross-architecture tests require explicit -AllowEmulation; they do not qualify native hardware.'
        }
        if ($hostArchitecture -ne 'Arm64' -or $targetArchitecture -ne 'X64') {
            throw 'This helper supports only explicit x64-on-ARM64 Windows emulation.'
        }
        $execution = 'x64 emulation on ARM64; not native x64 qualification'
    }
}
if ($Test -and $Example) { throw 'Select either an integration test or an example, not both.' }
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
    throw 'Visual Studio Installer is missing. Install the C++ Build Tools and Windows SDK described in docs\development.md.'
}
$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($installation)) {
    throw 'No MSVC C++ Build Tools installation is registered. See docs\development.md; this helper does not elevate or install software.'
}
$developerShell = Join-Path $installation.Trim() 'Common7\Tools\VsDevCmd.bat'
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$cargo = Join-Path $cargoHome 'bin\cargo.exe'
if (-not (Test-Path -LiteralPath $developerShell -PathType Leaf) -or -not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
    throw 'The native MSVC developer shell or Cargo executable is missing.'
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$toolchainFile = Get-Content -LiteralPath (Join-Path $root 'rust-toolchain.toml') -Raw
$pin = [regex]::Match($toolchainFile, '(?m)^\s*channel\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
if (-not $pin.Success) { throw 'rust-toolchain.toml must pin an exact Rust release.' }
$toolchain = "$($pin.Groups['version'].Value)-$hostTarget"
$arguments = @($Action)
if ($Package) { $arguments += @('-p', $Package) } else { $arguments += '--workspace' }
$arguments += @('--target', $target)
if ($Test) {
    if ($Action -ne 'test') { throw '-Test requires -Action test.' }
    $arguments += @('--test', $Test)
}
if ($Example) { $arguments += @('--example', $Example) }
if ($Action -in @('check', 'clippy') -and -not $Example) { $arguments += '--all-targets' }
if ($Release) { $arguments += '--release' }
if ($Locked) { $arguments += '--locked' }
if ($Action -eq 'clippy') { $arguments += @('--', '-D', 'warnings') }
$commandArgs = $arguments -join ' '
$variables = @('GITHUB_ADAPTER_VSDEV', 'GITHUB_ADAPTER_CARGO', 'VSCMD_SKIP_SENDTELEMETRY', 'PATH', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')
$previous = @{}
foreach ($name in $variables) { $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
Push-Location $root
try {
    $env:GITHUB_ADAPTER_VSDEV = $developerShell
    $env:GITHUB_ADAPTER_CARGO = $cargo
    $env:VSCMD_SKIP_SENDTELEMETRY = '1'
    $env:PATH = "$(Split-Path -Parent $vswhere);$(Split-Path -Parent $cargo);$env:PATH"
    if ($StaticRuntime) {
        $encoded = [Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS', 'Process')
        if ($null -ne $encoded) {
            # Cargo gives encoded flags precedence; preserve that winning argument list.
            $env:CARGO_ENCODED_RUSTFLAGS = (@($encoded, '-C', 'target-feature=+crt-static') | Where-Object { $_ }) -join [char]0x1f
        } else {
            $env:RUSTFLAGS = (@($env:RUSTFLAGS, '-C target-feature=+crt-static') | Where-Object { $_ }) -join ' '
        }
    }
    Write-Output "Windows host: $hostArchitecture; artifact target: $target; Cargo action: $Action; execution: $execution"
    & $env:ComSpec /d /c "call `"%GITHUB_ADAPTER_VSDEV%`" -arch=$msvcTarget -host_arch=amd64 >nul && `"%GITHUB_ADAPTER_CARGO%`" +$toolchain $commandArgs"
    if ($LASTEXITCODE -ne 0) { throw "Cargo $Action failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
    foreach ($name in $variables) {
        if ($null -eq $previous[$name]) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
        }
    }
}
