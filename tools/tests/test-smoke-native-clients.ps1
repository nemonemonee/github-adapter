#requires -Version 7.0
param([switch]$ExitFirstOnly)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot '..\smoke-native-clients.ps1')
$passed = 0
function Assert-Check([bool]$Condition, [string]$Name) {
    if (-not $Condition) { throw "FAIL $Name" }
    $script:passed++
    Write-Host "PASS $Name"
}
function Assert-Rejected([scriptblock]$Action, [string]$Name) {
    $rejected = $false
    try { $null = & $Action } catch { $rejected = $true }
    Assert-Check $rejected $Name
}
$fixture = Join-Path ([IO.Path]::GetTempPath()) "probe tests $([Guid]::NewGuid().ToString('N'))"
$null = [IO.Directory]::CreateDirectory($fixture)
try {
    $executable = Join-Path $PSHOME 'pwsh.exe'
    foreach ($launchKind in @('native', 'cmd')) {
        $owned = Join-Path $fixture $launchKind
        $null = [IO.Directory]::CreateDirectory($owned)
        $childScript = Join-Path $owned 'child.ps1'
        $launcherScript = Join-Path $owned 'launcher.ps1'
        $record = Join-Path $owned 'pids.json'
        $readyName = 'Local\NativeProbeTest-' + [Guid]::NewGuid().ToString('N')
        $ready = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset, $readyName)
        [IO.File]::WriteAllText($childScript, @'
param($Record, $ReadyName, $Launcher)
$lock = [IO.File]::Open(($Record + '.lock'), 'Create', 'ReadWrite', 'None')
[IO.File]::WriteAllText($Record, (@{ Child = $PID; Launcher = [int]$Launcher } | ConvertTo-Json -Compress))
$ready = [Threading.EventWaitHandle]::OpenExisting($ReadyName)
$null = $ready.Set()
$wait = [Threading.ManualResetEvent]::new($false)
$null = $wait.WaitOne(60000)
$lock.Dispose()
'@)
        [IO.File]::WriteAllText($launcherScript, @'
param($ChildScript, $Record, $ReadyName)
$info = [Diagnostics.ProcessStartInfo]::new()
$info.FileName = Join-Path $PSHOME 'pwsh.exe'
$info.UseShellExecute = $false
foreach ($argument in @('-NoProfile', '-File', $ChildScript, $Record, $ReadyName, [string]$PID)) { $info.ArgumentList.Add($argument) }
$child = [Diagnostics.Process]::Start($info)
$ready = [Threading.EventWaitHandle]::OpenExisting($ReadyName)
if (-not $ready.WaitOne(5000)) { exit 9 }
[IO.File]::WriteAllText(($Record + '.exit'), 'launcher exiting')
exit 0
'@)
        $fixtureEnvironment = @{ SystemRoot = $env:SystemRoot; TEMP = $owned; TMP = $owned }
        $launcherArguments = @('-NoProfile', '-File', $launcherScript, $childScript, $record, $readyName)
        $launcher = $executable
        if ($launchKind -eq 'cmd') {
            $launcher = Join-Path $owned 'launcher.cmd'
            [IO.File]::WriteAllText($launcher, "@echo off`r`n`"$executable`" %*`r`n")
        }
        try {
            $elapsed = [Diagnostics.Stopwatch]::StartNew()
            $exitFirst = Invoke-ProbeProcess (New-ProbeStartInfo $launcher $launcherArguments $owned $fixtureEnvironment) 10000
            Assert-Check ([IO.File]::Exists($record) -and [IO.File]::Exists($record + '.exit')) "$launchKind exit-first child actually started and launcher finished"
            Assert-Check ($null -eq $exitFirst.ExitCode -and $exitFirst.Diagnostic -like '*deadline*' -and $elapsed.ElapsedMilliseconds -lt 13000) "$launchKind inherited stdout deadline bounded"
            $pids = [IO.File]::ReadAllText($record) | ConvertFrom-Json
            foreach ($ownedId in @($pids.Launcher, $pids.Child)) {
                $remaining = $null
                try { $remaining = [Diagnostics.Process]::GetProcessById($ownedId) } catch [ArgumentException] { }
                try { Assert-Check ($null -eq $remaining -or $remaining.WaitForExit(1000)) "$launchKind owned PID $ownedId exited" }
                finally { if ($null -ne $remaining) { $remaining.Dispose() } }
            }
            [IO.Directory]::Delete($owned, $true)
            Assert-Check (-not [IO.Directory]::Exists($owned)) "$launchKind scratch deletable after capture cleanup"
        } finally {
            $ready.Dispose()
            if ([IO.File]::Exists($record)) {
                $pids = [IO.File]::ReadAllText($record) | ConvertFrom-Json
                foreach ($ownedId in @($pids.Launcher, $pids.Child)) {
                    $remaining = $null
                    try {
                        $remaining = [Diagnostics.Process]::GetProcessById($ownedId)
                        if (-not $remaining.WaitForExit(100)) { $remaining.Kill($true); $null = $remaining.WaitForExit(1000) }
                    } catch [ArgumentException] { }
                    finally { if ($null -ne $remaining) { $remaining.Dispose() } }
                }
            }
        }
    }
    if ($ExitFirstOnly) { Write-Host "PASS $passed exit-first probe checks"; return }
    $codexText = '{"type":"item.completed","item":{"type":"agent_message","text":"ADAPTER_NATIVE_CODEX_OK"}}'
    $completedText = $codexText + "`n" + '{"type":"turn.completed"}'
    $claudeText = '{"is_error":false,"result":"ADAPTER_NATIVE_CLAUDE_OK"}'
    Assert-Check (Test-ProbeMarker codex $completedText) 'Codex exact completed marker'
    Assert-Check (Test-ProbeMarker claude $claudeText) 'Claude exact marker'
    foreach ($text in @('null', '[]', '42', '"text"', '{', '{}', '{"type":4}',
        '{"type":"item.completed","item":[]}', '{"type":"item.completed","item":{"type":"agent_message","text":7}}',
        $codexText, ('[' + $codexText + ']' + "`n" + '{"type":"turn.completed"}'),
        ($completedText + "`n" + '{"type":"error"}'), ($completedText + "`n" + '{"type":"turn.failed"}'),
        ($completedText + "`n" + '{"type":"item.completed","item":{"type":"agent_message","text":"wrong"}}'))) {
        Assert-Check (-not (Test-ProbeMarker codex $text)) 'Codex rejects malformed, incomplete or failed output'
    }
    foreach ($text in @('null', '[]', '42', '"text"', '{', '{}', '{"is_error":false}',
        '{"is_error":true,"result":"ADAPTER_NATIVE_CLAUDE_OK"}', '{"is_error":"false","result":"ADAPTER_NATIVE_CLAUDE_OK"}',
        ('[' + $claudeText + ']'), '{"is_error":0,"result":"ADAPTER_NATIVE_CLAUDE_OK"}',
        '{"is_error":false,"result":[]}', '{"is_error":false,"result":"wrong"}')) {
        Assert-Check (-not (Test-ProbeMarker claude $text)) 'Claude rejects malformed, missing or error output'
    }
    $parent = @{ PATH = $env:PATH; SystemRoot = $env:SystemRoot; TEMP = $fixture; TMP = $fixture; GH_TOKEN = 'synthetic-parent-secret';
        GITHUB_TOKEN = 'synthetic-parent-secret'; MAI_KEY = 'synthetic-parent-secret'; OPENAI_BASE_URL = 'https://unused.invalid';
        CODEX_API_KEY = 'synthetic-parent-secret'; ANTHROPIC_API_KEY = 'synthetic-parent-secret'; CLAUDE_CONFIG_DIR = 'unused';
        API_TOKEN = 'synthetic-parent-secret'; CUSTOM_ADAPTER_SECRET = 'synthetic-parent-secret' }
    $parentSnapshot = $parent.Clone()
    $processSnapshot = [Environment]::GetEnvironmentVariables('Process')
    $output = Join-Path $fixture 'report folder\report.json'
    $options = @{ Endpoint = 'http://127.0.0.1:5001'; Codex = $executable; Claude = $executable; Output = $output; ParentEnvironment = $parent }
    foreach ($scenario in @('complete', 'incomplete', 'nonzero', 'malformed', 'launch', 'timeout', 'both-launch', 'both-timeout', 'claude-error')) {
        $state = @{ Calls = 0; Root = '' }
        $runner = {
            param($info, $deadline)
            $state.Calls++
            $state.Root = Split-Path $info.WorkingDirectory
            Assert-Check ($deadline -eq 120000) 'default deadline'
            Assert-Check ([IO.Directory]::Exists($info.WorkingDirectory)) 'isolated workspace exists'
            Assert-Check ([IO.Directory]::Exists($info.Environment['CODEX_HOME']) -and [IO.Directory]::Exists($info.Environment['CLAUDE_CONFIG_DIR'])) 'isolated homes exist'
            Assert-Check (-not ($info.Environment.Values -contains 'synthetic-parent-secret')) 'parent credentials stripped'
            Assert-Check (-not $info.Environment.ContainsKey('OPENAI_BASE_URL')) 'parent endpoint stripped'
            Assert-Check ($info.Environment['OPENAI_API_KEY'] -eq 'synthetic-local-client-marker') 'local placeholder only'
            if ($state.Calls -eq 1) {
                foreach ($flag in @('--ignore-user-config', '--ignore-rules', '--ephemeral', 'read-only', 'features.plugins=false')) {
                    Assert-Check ($info.ArgumentList.Contains($flag)) "Codex isolation $flag"
                }
                if ($scenario -in @('launch', 'both-launch')) { throw 'synthetic-parent-secret' }
                if ($scenario -in @('timeout', 'both-timeout')) { return @{ Text = ''; ExitCode = $null; Diagnostic = 'Client exceeded the synthetic check deadline.' } }
                $text = switch ($scenario) { incomplete { $codexText }; malformed { '[]' }; default { $completedText } }
                return @{ Text = $text; ExitCode = $(if ($scenario -eq 'nonzero') { 1 } else { 0 }); Diagnostic = 'synthetic-parent-secret' }
            }
            foreach ($flag in @('--bare', '--strict-mcp-config', '--no-session-persistence')) {
                Assert-Check ($info.ArgumentList.Contains($flag)) "Claude isolation $flag"
            }
            foreach ($flag in @('--tools', '--setting-sources')) {
                Assert-Check ($info.ArgumentList[$info.ArgumentList.IndexOf($flag) + 1] -ceq '') "empty $flag argument"
            }
            $settingsPath = $info.ArgumentList[$info.ArgumentList.IndexOf('--settings') + 1]
            $settings = [IO.File]::ReadAllText($settingsPath) | ConvertFrom-Json
            Assert-Check ($settings.env.ANTHROPIC_BASE_URL -eq $options.Endpoint) 'synthetic settings endpoint'
            $mcpPath = $info.ArgumentList[$info.ArgumentList.IndexOf('--mcp-config') + 1]
            Assert-Check ([IO.File]::ReadAllText($mcpPath) -eq '{"mcpServers":{}}') 'empty MCP configuration'
            if ($scenario -eq 'both-launch') { throw 'synthetic-parent-secret' }
            if ($scenario -eq 'both-timeout') { return @{ Text = ''; ExitCode = $null; Diagnostic = 'Client exceeded the synthetic check deadline.' } }
            if ($scenario -eq 'claude-error') { return @{ Text = '{"is_error":true,"result":"ADAPTER_NATIVE_CLAUDE_OK"}'; ExitCode = 0; Diagnostic = '' } }
            return @{ Text = $claudeText; ExitCode = 0; Diagnostic = '' }
        }
        $result = Invoke-NativeClientProbe @options -Runner $runner
        $report = [IO.File]::ReadAllText($output) | ConvertFrom-Json
        Assert-Check ($result.ExitCode -eq $(if ($scenario -eq 'complete') { 0 } else { 1 })) "report exit $scenario"
        Assert-Check ($state.Calls -eq 2 -and $report.results.Count -eq 2 -and $report.results[1].passed -eq ($scenario -notin @('both-launch', 'both-timeout', 'claude-error'))) 'both clients reported independently'
        Assert-Check (@($report.results | Where-Object { $null -eq $_.PSObject.Properties['exit_code'] -or $null -eq $_.PSObject.Properties['marker_returned'] }).Count -eq 0) 'all reports include exit and marker fields'
        if ($scenario -eq 'nonzero') { Assert-Check ($report.results[0].marker_returned -and -not $report.results[0].passed) 'marker does not override nonzero exit' }
        if ($scenario -eq 'incomplete') { Assert-Check (-not $report.results[0].marker_returned -and $report.results[0].exit_code -eq 0) 'zero exit does not override missing completion' }
        Assert-Check (-not [IO.Directory]::Exists($state.Root)) 'scratch cleaned'
        Assert-Check (-not $result.Json.Contains('synthetic-parent-secret')) 'diagnostic contains no secret'
        Assert-Check (-not $report.tools_requested -and -not $report.project_content_sent -and -not $report.real_settings_modified -and -not $report.codex_plugins_enabled) 'report safety fields'
    }
    Assert-Check ($parent.Count -eq $parentSnapshot.Count -and @($parentSnapshot.Keys | Where-Object { -not $parent.ContainsKey($_) -or $parent[$_] -cne $parentSnapshot[$_] }).Count -eq 0) 'provided parent environment unchanged'
    $currentEnvironment = [Environment]::GetEnvironmentVariables('Process')
    Assert-Check ($currentEnvironment.Count -eq $processSnapshot.Count -and @($processSnapshot.Keys | Where-Object { -not $currentEnvironment.Contains($_) -or $currentEnvironment[$_] -cne $processSnapshot[$_] }).Count -eq 0) 'process environment unchanged'
    $noLaunch = { throw 'Unexpected runner invocation' }
    foreach ($endpoint in @('https://127.0.0.1', 'http://localhost', 'http://127.1', 'http://2130706433',
        'http://127.0.0.2', 'http://user@127.0.0.1', 'http://127.0.0.1?x', 'http://127.0.0.1#x',
        'http://127.0.0.1/path', 'http://127.0.0.1:0', 'http://127.0.0.1:65536', 'http://127.0.0.1:', 'http://[::2]',
        'http://0177.0.0.1', 'http://0x7f000001', 'http://127.0.0.1?', 'http://127.0.0.1#', 'http://127.0.0.1:999999',
        'http://[::ffff:127.0.0.1]', 'http://127.0.0.1/..', "http://127.0.0.1`n")) {
        Assert-Rejected { Invoke-NativeClientProbe -Endpoint $endpoint -Codex $executable -Claude $executable -Output $output -Runner $noLaunch } 'invalid endpoint before launch'
    }
    Assert-Rejected { Invoke-NativeClientProbe -Endpoint 'http://127.0.0.1' -Codex "$fixture\missing.exe" -Claude $executable -Output $output -Runner $noLaunch } 'missing executable rejected'
    foreach ($model in @('', '--help', '-c', 'model name', 'x&whoami', 'x"y', '%PATH%', "model`n")) {
        Assert-Rejected { Invoke-NativeClientProbe @options -Model $model -Runner $noLaunch } 'model injection rejected before launch'
    }
    $validRunner = { param($info, $deadline) @{ ExitCode = 0; Text = $(if ($info.ArgumentList.Contains('exec')) { $completedText } else { $claudeText }); Diagnostic = '' } }
    $ipv6 = Invoke-NativeClientProbe -Endpoint 'http://[::1]:5001/' -Codex $executable -Claude $executable -Output $output -Runner $validRunner
    Assert-Check ($ipv6.ExitCode -eq 0) 'literal IPv6 accepted offline'
    foreach ($endpoint in @('http://127.0.0.1', 'http://127.0.0.1:1/', 'http://[::1]', 'http://[::1]:65535/')) {
        $valid = Invoke-NativeClientProbe -Endpoint $endpoint -Codex $executable -Claude $executable -Output $output -Runner $validRunner
        Assert-Check ($valid.ExitCode -eq 0) 'literal root and port boundaries accepted'
    }
    $echo = Join-Path $fixture 'echo arguments.ps1'
    [IO.File]::WriteAllText($echo, 'ConvertTo-Json -InputObject @($args) -Compress')
    $arguments = @('-NoLogo', '-NoProfile', '-File', $echo, '', 'path with spaces', 'openai_base_url="http://127.0.0.1:5001"')
    $info = New-ProbeStartInfo $executable $arguments $fixture $parent
    $captured = Invoke-ProbeProcess $info 10000
    $values = ConvertFrom-Json -InputObject $captured.Text
    Assert-Check ($captured.ExitCode -eq 0 -and $values.Count -eq 3 -and $values[0] -ceq '' -and $values[1] -ceq 'path with spaces' -and $values[2] -ceq $arguments[-1]) 'native argument spaces, empty and quotes preserved'
    $batchArguments = @('', 'path with spaces', '-c', 'openai_base_url="http://[::1]:5001/"', '-c', 'model_reasoning_effort="low"', 'trailing space path\')
    foreach ($extension in @('cmd', 'bat')) {
        $batch = Join-Path $fixture "client wrapper.$extension"
        [IO.File]::WriteAllText($batch, "@echo off`r`n`"$executable`" -NoLogo -NoProfile -File `"$echo`" %*`r`n")
        $info = New-ProbeStartInfo $batch $batchArguments $fixture $parent
        Assert-Check ($info.ArgumentList.Count -eq 0 -and $info.Arguments.StartsWith('/d /v:off /s /c ')) 'CMD uses explicit command string'
        $captured = Invoke-ProbeProcess $info 10000
        $values = ConvertFrom-Json -InputObject $captured.Text
        Assert-Check ($captured.ExitCode -eq 0 -and $values.Count -eq $batchArguments.Count -and @(
            for ($index = 0; $index -lt $values.Count; $index++) { if ($values[$index] -cne $batchArguments[$index]) { $index } }
        ).Count -eq 0) "$extension preserves empty, spaces, trailing slash and TOML quotes"
    }
    foreach ($unsafe in @('x&whoami', 'x|whoami', 'x>file', 'x<file', 'x^y', '%PATH%', '!PATH!', "x`ny", 'x"y')) {
        Assert-Rejected { New-ProbeStartInfo $batch @($unsafe) $fixture $parent } 'CMD injection rejected'
        Assert-Rejected { New-ProbeStartInfo $batch @('safe') "$fixture\$unsafe" $parent } 'CMD working path injection rejected'
        Assert-Rejected { New-ProbeStartInfo "$fixture\$unsafe.cmd" @('safe') $fixture $parent } 'CMD executable path injection rejected'
    }
    $invalidExe = Join-Path $fixture 'invalid.exe'
    [IO.File]::WriteAllText($invalidExe, 'not an executable')
    $failed = Invoke-ProbeProcess (New-ProbeStartInfo $invalidExe @() $fixture $parent) 1000
    Assert-Check ($null -eq $failed.ExitCode -and $failed.Diagnostic -eq 'Client launch or capture failed.') 'real launch failure bounded and generic'
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $timed = Invoke-ProbeProcess (New-ProbeStartInfo $executable @('-NoProfile', '-Command', 'while ($true) {}') $fixture $parent) 300
    Assert-Check ($null -eq $timed.ExitCode -and $timed.Diagnostic -like '*deadline*' -and $clock.ElapsedMilliseconds -lt 5000) 'real owned process timeout bounded'
    $captureCode = '[Console]::Out.Write(("x" * 100000)); [Console]::Error.Write(("y" * 100000))'
    $captured = Invoke-ProbeProcess (New-ProbeStartInfo $executable @('-NoProfile', '-Command', $captureCode) $fixture $parent) 10000
    Assert-Check ($captured.ExitCode -eq 0 -and $captured.Text.Length -eq 100000) 'stdout and stderr drain concurrently'
    $captured = Invoke-ProbeProcess (New-ProbeStartInfo $executable @('-NoProfile', '-Command', '[Console]::Out.Write(("x" * 1100000))') $fixture $parent) 10000
    Assert-Check ($null -eq $captured.ExitCode -and $captured.Text -ceq '' -and $captured.Diagnostic -eq 'Client launch or capture failed.') 'capture overflow fails without retaining raw output'
    $codexFixture = Join-Path $fixture 'synthetic codex.ps1'
    $claudeFixture = Join-Path $fixture 'synthetic claude.ps1'
    $guard = 'if ($env:GH_TOKEN -or $env:MAI_KEY -or $env:API_TOKEN -or $env:CUSTOM_ADAPTER_SECRET -or $env:CODEX_API_KEY) { exit 9 }; '
    [IO.File]::WriteAllText($codexFixture, $guard + "Write-Output '$codexText'; Write-Output '{`"type`":`"turn.completed`"}'")
    [IO.File]::WriteAllText($claudeFixture, $guard + "Write-Output '$claudeText'")
    $codexWrapper = Join-Path $fixture 'synthetic codex.cmd'
    $claudeWrapper = Join-Path $fixture 'synthetic claude.bat'
    [IO.File]::WriteAllText($codexWrapper, "@echo off`r`n`"$executable`" -NoLogo -NoProfile -File `"$codexFixture`" %*`r`n")
    [IO.File]::WriteAllText($claudeWrapper, "@echo off`r`n`"$executable`" -NoLogo -NoProfile -File `"$claudeFixture`" %*`r`n")
    $probePath = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\smoke-native-clients.ps1'))
    foreach ($scenario in @('complete', 'incomplete', 'invalid')) {
        if ($scenario -eq 'incomplete') { [IO.File]::WriteAllText($codexFixture, $guard + "Write-Output '$codexText'") }
        $cliOutput = Join-Path $fixture "cli-$scenario.json"
        $cliArguments = @('-NoProfile', '-File', $probePath, '-Endpoint', $(if ($scenario -eq 'invalid') { 'http://localhost' } else { 'http://127.0.0.1:5001' }),
            '-Codex', $codexWrapper, '-Claude', $claudeWrapper, '-Output', $cliOutput)
        $captured = Invoke-ProbeProcess (New-ProbeStartInfo $executable $cliArguments $fixture $parent) 15000
        $expectedExit = switch ($scenario) { complete { 0 }; incomplete { 1 }; invalid { 2 } }
        if ($captured.ExitCode -ne $expectedExit) { throw "CLI $scenario expected $expectedExit, got $($captured.ExitCode): $($captured.Diagnostic) $($captured.Text)" }
        Assert-Check ($captured.ExitCode -eq $expectedExit) "synthetic CLI $scenario exit $expectedExit"
        if ($scenario -ne 'invalid') {
            $cliReport = [IO.File]::ReadAllText($cliOutput) | ConvertFrom-Json
            Assert-Check ($cliReport.results.Count -eq 2 -and $cliReport.results[1].passed -and $cliReport.results[0].passed -eq ($scenario -eq 'complete')) 'CLI runs both wrappers with runtime environment isolation'
            Assert-Check (($captured.Text | ConvertFrom-Json).results.Count -eq 2) 'CLI emits JSON as well as report file'
        } else { Assert-Check (-not [IO.File]::Exists($cliOutput)) 'invalid CLI input writes no report' }
    }
    $currentEnvironment = [Environment]::GetEnvironmentVariables('Process')
    Assert-Check ($currentEnvironment.Count -eq $processSnapshot.Count -and @($processSnapshot.Keys | Where-Object { -not $currentEnvironment.Contains($_) -or $currentEnvironment[$_] -cne $processSnapshot[$_] }).Count -eq 0) 'process environment unchanged after real synthetic children'
    Write-Host "PASS $passed offline client probe checks"
} finally {
    [IO.Directory]::Delete($fixture, $true)
}