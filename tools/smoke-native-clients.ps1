#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$Endpoint,
    [string]$Codex,
    [string]$Claude,
    [string]$Model = 'gpt-6-astra',
    [string]$Output
)

function New-ProbeStartInfo([string]$Executable, [string[]]$Arguments, [string]$WorkingDirectory, [System.Collections.IDictionary]$Environment) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.Environment.Clear()
    foreach ($name in $Environment.Keys) { $info.Environment[$name] = [string]$Environment[$name] }
    if ([IO.Path]::GetExtension($Executable) -iin @('.cmd', '.bat')) {
        foreach ($path in @($Executable, $WorkingDirectory)) {
            if ($path -match '[&|<>^%!"\x00-\x1f]') { throw 'Unsafe CMD path.' }
        }
        $quoted = [Collections.Generic.List[string]]::new()
        $quoted.Add('"' + $Executable + '"')
        foreach ($argument in $Arguments) {
            if ($argument -match '[&|<>^%!\x00-\x1f]') { throw 'Unsafe CMD argument.' }
            if ($argument.Contains('"') -and $argument -cnotmatch '^(openai_base_url="http://(127\.0\.0\.1|\[::1\])(:[0-9]{1,5})?/?"|model_reasoning_effort="low")$') {
                throw 'Unsupported CMD quoting.'
            }
            $escaped = $argument.Replace('"', '\"') -replace '(\\+)$', '$1$1'
            $quoted.Add('"' + $escaped + '"')
        }
        $info.FileName = Join-Path ([Environment]::GetFolderPath('System')) 'cmd.exe'
        $info.Arguments = '/d /v:off /s /c "' + ($quoted -join ' ') + '"'
    } else {
        foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    }
    return $info
}

function Invoke-ProbeProcess([Diagnostics.ProcessStartInfo]$Info, [ValidateRange(1, 2147483647)][int]$TimeoutMilliseconds = 120000) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    if (-not ('NativeProbeOwnedProcess' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Diagnostics;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;
public sealed class NativeProbeJob : SafeHandleZeroOrMinusOneIsInvalid {
    private NativeProbeJob() : base(true) { }
    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimits {
        public long ProcessTime, JobTime;
        public uint Flags;
        public UIntPtr MinimumWorkingSet, MaximumWorkingSet;
        public uint ActiveProcesses;
        public UIntPtr Affinity;
        public uint Priority, Scheduling;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimits {
        public BasicLimits Basic;
        public ulong ReadOperations, WriteOperations, OtherOperations, ReadBytes, WriteBytes, OtherBytes;
        public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern NativeProbeJob CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(NativeProbeJob job, int kind, ref ExtendedLimits limits, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(NativeProbeJob job, SafeProcessHandle process);
    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);
    protected override bool ReleaseHandle() { return CloseHandle(handle); }
    public static NativeProbeJob Create() {
        var job = CreateJobObject(IntPtr.Zero, null);
        var limits = new ExtendedLimits();
        limits.Basic.Flags = 0x2000;
        if (job.IsInvalid || !SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf<ExtendedLimits>())) {
            job.Dispose();
            throw new InvalidOperationException("Job containment unavailable.");
        }
        return job;
    }
    public void Assign(SafeProcessHandle process) {
        if (!AssignProcessToJobObject(this, process)) throw new InvalidOperationException("Job assignment denied.");
    }
}
public sealed class NativeProbeOwnedProcess : IDisposable {
    private const int DesktopAppPolicyAttribute = 0x20012;
    private const int DisableDesktopAppBreakaway = 0x02;
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct StartupInfo {
        public int Size;
        public IntPtr Reserved, Desktop, Title;
        public int X, Y, Width, Height, XChars, YChars, Fill, Flags;
        public short Show, ReservedSize;
        public IntPtr ReservedBytes, Input, Output, Error;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct StartupInfoEx { public StartupInfo Basic; public IntPtr Attributes; }
    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessInformation { public IntPtr Process, Thread; public int ProcessId, ThreadId; }
    [StructLayout(LayoutKind.Sequential)]
    private struct SecurityAttributes { public int Size; public IntPtr Descriptor; public int Inherit; }
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CreatePipe(out SafeFileHandle read, out SafeFileHandle write, ref SecurityAttributes attributes, int size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetHandleInformation(SafeFileHandle handle, int mask, int flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previous, IntPtr returned);
    [DllImport("kernel32.dll")]
    private static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool CreateProcess(string application, StringBuilder command, IntPtr processAttributes, IntPtr threadAttributes,
        bool inherit, uint flags, IntPtr environment, string directory, ref StartupInfoEx startup, out ProcessInformation process);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(SafeWaitHandle thread);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(SafeProcessHandle process, uint code);
    public Process Process { get; private set; }
    public StreamReader Output { get; private set; }
    public StreamReader Error { get; private set; }
    private static string Quote(string argument) {
        var quoted = new StringBuilder("\"");
        int slashes = 0;
        foreach (char character in argument) {
            if (character == '\\') { slashes++; continue; }
            quoted.Append('\\', character == '"' ? slashes * 2 + 1 : slashes);
            quoted.Append(character);
            slashes = 0;
        }
        return quoted.Append('\\', slashes * 2).Append('"').ToString();
    }
    public static NativeProbeOwnedProcess Start(ProcessStartInfo info, NativeProbeJob job, int remaining) {
        var clock = Stopwatch.StartNew();
        var owned = new NativeProbeOwnedProcess();
        var security = new SecurityAttributes { Size = Marshal.SizeOf<SecurityAttributes>(), Inherit = 1 };
        SafeFileHandle outputRead = null, outputWrite = null, errorRead = null, errorWrite = null, inputRead = null, inputWrite = null;
        IntPtr attributes = IntPtr.Zero, handles = IntPtr.Zero, environment = IntPtr.Zero, desktopPolicy = IntPtr.Zero;
        bool initialized = false;
        SafeProcessHandle processHandle = null;
        SafeWaitHandle thread = null;
        try {
            if (!CreatePipe(out outputRead, out outputWrite, ref security, 0) ||
                !CreatePipe(out errorRead, out errorWrite, ref security, 0) ||
                !CreatePipe(out inputRead, out inputWrite, ref security, 0) ||
                !SetHandleInformation(outputRead, 1, 0) || !SetHandleInformation(errorRead, 1, 0) ||
                !SetHandleInformation(inputWrite, 1, 0)) throw new InvalidOperationException();
            IntPtr size = IntPtr.Zero;
            InitializeProcThreadAttributeList(IntPtr.Zero, 2, 0, ref size);
            attributes = Marshal.AllocHGlobal(size);
            if (!InitializeProcThreadAttributeList(attributes, 2, 0, ref size)) throw new InvalidOperationException();
            initialized = true;
            handles = Marshal.AllocHGlobal(IntPtr.Size * 3);
            Marshal.WriteIntPtr(handles, 0, inputRead.DangerousGetHandle());
            Marshal.WriteIntPtr(handles, IntPtr.Size, outputWrite.DangerousGetHandle());
            Marshal.WriteIntPtr(handles, IntPtr.Size * 2, errorWrite.DangerousGetHandle());
            if (!UpdateProcThreadAttribute(attributes, 0, (IntPtr)0x20002, handles, (IntPtr)(IntPtr.Size * 3), IntPtr.Zero, IntPtr.Zero)) throw new InvalidOperationException();
            desktopPolicy = Marshal.AllocHGlobal(sizeof(int));
            Marshal.WriteInt32(desktopPolicy, DisableDesktopAppBreakaway);
            if (!UpdateProcThreadAttribute(attributes, 0, (IntPtr)DesktopAppPolicyAttribute, desktopPolicy, (IntPtr)sizeof(int), IntPtr.Zero, IntPtr.Zero)) throw new InvalidOperationException();
            var startup = new StartupInfoEx { Basic = new StartupInfo { Size = Marshal.SizeOf<StartupInfoEx>(), Flags = 0x100,
                Input = inputRead.DangerousGetHandle(), Output = outputWrite.DangerousGetHandle(), Error = errorWrite.DangerousGetHandle() }, Attributes = attributes };
            string block = string.Join("\0", info.Environment.OrderBy(entry => entry.Key, StringComparer.OrdinalIgnoreCase)
                .Select(entry => entry.Key + "=" + entry.Value)) + "\0\0";
            environment = Marshal.StringToHGlobalUni(block);
            string arguments = info.ArgumentList.Count == 0 ? info.Arguments : string.Join(" ", info.ArgumentList.Select(Quote));
            if (clock.ElapsedMilliseconds >= remaining) throw new TimeoutException();
            if (!CreateProcess(info.FileName, new StringBuilder(Quote(info.FileName) + " " + arguments), IntPtr.Zero, IntPtr.Zero,
                true, 0x08080404, environment, info.WorkingDirectory, ref startup, out var created)) throw new InvalidOperationException();
            processHandle = new SafeProcessHandle(created.Process, true);
            thread = new SafeWaitHandle(created.Thread, true);
            owned.Process = Process.GetProcessById(created.ProcessId);
            var retainedHandle = owned.Process.SafeHandle;
            job.Assign(processHandle);
            owned.Output = new StreamReader(new FileStream(outputRead, FileAccess.Read), info.StandardOutputEncoding ?? Console.OutputEncoding);
            outputRead = null;
            owned.Error = new StreamReader(new FileStream(errorRead, FileAccess.Read), info.StandardErrorEncoding ?? Console.OutputEncoding);
            errorRead = null;
            if (clock.ElapsedMilliseconds >= remaining) throw new TimeoutException();
            if (ResumeThread(thread) == uint.MaxValue) throw new InvalidOperationException();
            return owned;
        } catch {
            if (processHandle != null && !processHandle.IsInvalid) TerminateProcess(processHandle, 1);
            owned.Dispose();
            throw;
        } finally {
            thread?.Dispose(); processHandle?.Dispose();
            outputRead?.Dispose(); outputWrite?.Dispose(); errorRead?.Dispose(); errorWrite?.Dispose(); inputRead?.Dispose(); inputWrite?.Dispose();
            if (initialized) DeleteProcThreadAttributeList(attributes);
            if (attributes != IntPtr.Zero) Marshal.FreeHGlobal(attributes);
            if (handles != IntPtr.Zero) Marshal.FreeHGlobal(handles);
            if (environment != IntPtr.Zero) Marshal.FreeHGlobal(environment);
            if (desktopPolicy != IntPtr.Zero) Marshal.FreeHGlobal(desktopPolicy);
        }
    }
    public void Dispose() { Output?.Dispose(); Error?.Dispose(); Process?.Dispose(); }
    public static async Task<string> Drain(StreamReader reader) {
        var text = new StringBuilder();
        var buffer = new char[8192];
        bool overflow = false;
        int count;
        while ((count = await reader.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false)) != 0) {
            if (text.Length + count <= 1048576 && !overflow) text.Append(buffer, 0, count);
            else overflow = true;
        }
        return overflow ? null : text.ToString();
    }
}
'@ -ErrorAction Stop
    }
    $owned = $null
    $job = $null
    $stdout = $null
    $stderr = $null
    $diagnostic = 'Client containment unavailable. Check Windows job nesting policy; no client was started.'
    try {
        $job = [NativeProbeJob]::Create()
        $diagnostic = 'Client launch or capture failed.'
        $owned = [NativeProbeOwnedProcess]::Start($Info, $job, [Math]::Max(0, $TimeoutMilliseconds - [int]$clock.ElapsedMilliseconds))
        $process = $owned.Process
        $stdout = [NativeProbeOwnedProcess]::Drain($owned.Output)
        $stderr = [NativeProbeOwnedProcess]::Drain($owned.Error)
        if (-not $process.WaitForExit([Math]::Max(0, $TimeoutMilliseconds - [int]$clock.ElapsedMilliseconds)) -or
            -not [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($stdout, $stderr), [Math]::Max(0, $TimeoutMilliseconds - [int]$clock.ElapsedMilliseconds))) {
            throw [TimeoutException]::new()
        }
        if ($null -eq $stdout.Result -or $null -eq $stderr.Result) { throw 'Capture limit exceeded.' }
        return @{ Text = $stdout.Result; ExitCode = $process.ExitCode; Diagnostic = '' }
    } catch [TimeoutException] {
        return @{ Text = ''; ExitCode = $null; Diagnostic = 'Client exceeded the synthetic check deadline.' }
    } catch {
        return @{ Text = ''; ExitCode = $null; Diagnostic = $diagnostic }
    } finally {
        if ($null -ne $job) { $job.Dispose() }
        $cleanup = [Diagnostics.Stopwatch]::StartNew()
        if ($null -ne $owned) {
            try { $null = $owned.Process.WaitForExit(1000) } catch { }
        }
        $pending = @(@($stdout, $stderr) | Where-Object { $null -ne $_ })
        try { if ($pending.Count) { $null = [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]$pending, [Math]::Max(0, 1000 - [int]$cleanup.ElapsedMilliseconds)) } } catch { }
        if ($null -ne $owned) { $owned.Dispose() }
    }
}

function Test-ProbeMarker([string]$Client, [string]$Text) {
    try {
        if ($Client -ceq 'claude') {
            $answer = ConvertFrom-Json -InputObject $Text -AsHashtable -NoEnumerate -ErrorAction Stop
            return $answer -is [System.Collections.IDictionary] -and $answer['is_error'] -is [bool] -and
                $answer['is_error'] -eq $false -and $answer['result'] -is [string] -and
                $answer['result'].Trim() -ceq 'ADAPTER_NATIVE_CLAUDE_OK'
        }
        if ($Client -cne 'codex') { return $false }
        $lastAnswer = $null
        $completed = $false
        foreach ($line in $Text -split '\r?\n') {
            if (-not $line.Trim()) { continue }
            $event = ConvertFrom-Json -InputObject $line -AsHashtable -NoEnumerate -ErrorAction Stop
            if ($event -isnot [System.Collections.IDictionary] -or $event['type'] -isnot [string]) { return $false }
            if ($event['type'] -cin @('error', 'turn.failed')) { return $false }
            if ($event['type'] -ceq 'turn.completed') { $completed = $true }
            if ($event['type'] -ceq 'item.completed') {
                $item = $event['item']
                if ($item -isnot [System.Collections.IDictionary] -or $item['type'] -isnot [string]) { return $false }
                if ($item['type'] -ceq 'agent_message') {
                    if ($item['text'] -isnot [string]) { return $false }
                    $lastAnswer = $item['text'].Trim()
                }
            }
        }
        return $completed -and $lastAnswer -ceq 'ADAPTER_NATIVE_CODEX_OK'
    } catch { return $false }
}

function Invoke-NativeClientProbe {
    [CmdletBinding()]
    param(
        [string]$Endpoint, [string]$Codex, [string]$Claude, [string]$Output, [string]$Model = 'gpt-6-astra',
        [System.Collections.IDictionary]$ParentEnvironment = [Environment]::GetEnvironmentVariables('Process'),
        [scriptblock]$Runner = { param($info, $deadline) Invoke-ProbeProcess $info $deadline }
    )
    if ($Endpoint -cnotmatch '\Ahttp://(127\.0\.0\.1|\[::1\])(?::([0-9]{1,5}))?/?\z' -or
        ($Matches[2] -and ([int]$Matches[2] -lt 1 -or [int]$Matches[2] -gt 65535))) { throw 'Use an HTTP loopback adapter root.' }
    if ($Model -cnotmatch '\A[A-Za-z0-9][A-Za-z0-9._:/-]*\z') { throw 'Use a literal model name.' }
    foreach ($executable in @($Codex, $Claude)) {
        if (-not [IO.File]::Exists($executable)) { throw 'Both installed client executables must exist.' }
    }
    if (-not $Output) { throw 'A report output path is required.' }
    $outputPath = [IO.Path]::GetFullPath($Output)
    $codexPath = [IO.Path]::GetFullPath($Codex)
    $claudePath = [IO.Path]::GetFullPath($Claude)
    $results = [Collections.Generic.List[object]]::new()
    $temporary = Join-Path ([IO.Path]::GetTempPath()) "github-adapter-native-clients-$([Guid]::NewGuid().ToString('N'))"
    try {
        $work = Join-Path $temporary 'empty-workspace'
        $codexHome = Join-Path $temporary 'codex'
        $claudeHome = Join-Path $temporary 'claude'
        foreach ($directory in @($work, $codexHome, $claudeHome)) { [IO.Directory]::CreateDirectory($directory) | Out-Null }
        $environment = @{}
        foreach ($name in $ParentEnvironment.Keys) {
            if ($name -notmatch '^(GH_|GITHUB_|MAI_|OPENAI_|ANTHROPIC_|CODEX_|CLAUDE_|API_)|ADAPTER|API[_]?KEY|AUTH[_]?TOKEN') { $environment[$name] = $ParentEnvironment[$name] }
        }
        $environment['CODEX_HOME'] = $codexHome
        $environment['CLAUDE_CONFIG_DIR'] = $claudeHome
        $environment['OPENAI_API_KEY'] = 'synthetic-local-client-marker'
        $environment['ANTHROPIC_AUTH_TOKEN'] = 'synthetic-local-client-marker'
        $environment['ANTHROPIC_BASE_URL'] = $Endpoint
        $environment['CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC'] = '1'

        $settings = Join-Path $temporary 'claude-settings.json'
        [IO.File]::WriteAllText($settings, (@{ env = @{ ANTHROPIC_BASE_URL = $Endpoint } } | ConvertTo-Json -Compress), [Text.UTF8Encoding]::new($false))
        $mcp = Join-Path $temporary 'empty-mcp.json'
        [IO.File]::WriteAllText($mcp, '{"mcpServers":{}}', [Text.UTF8Encoding]::new($false))

        $commands = @(
            [pscustomobject]@{
                Name = 'codex'; Executable = $codexPath; Arguments = @(
                    'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral', '--skip-git-repo-check', '--sandbox', 'read-only',
                    '--cd', $work, '--json', '--color', 'never', '--model', $Model,
                    '-c', "openai_base_url=`"$Endpoint`"", '-c', 'model_reasoning_effort="low"', '-c', 'features.plugins=false',
                    'Reply exactly ADAPTER_NATIVE_CODEX_OK. Do not use any tools or inspect files.'
                )
            },
            [pscustomobject]@{
                Name = 'claude'; Executable = $claudePath; Arguments = @(
                    '--bare', '--settings', $settings, '--setting-sources', '', '--strict-mcp-config', '--mcp-config', $mcp,
                    '--tools', '', '--no-session-persistence', '--system-prompt', 'Reply exactly ADAPTER_NATIVE_CLAUDE_OK. No tool use.',
                    '--print', '--model', $Model, '--output-format', 'json', 'Return the requested marker.'
                )
            }
        )
        $startInfos = @($commands | ForEach-Object { New-ProbeStartInfo $_.Executable $_.Arguments $work $environment })
        for ($index = 0; $index -lt $commands.Count; $index++) {
            $exitCode = $null
            $markerReturned = $false
            $diagnostic = 'Client launch or capture failed.'
            try {
                $result = & $Runner $startInfos[$index] 120000
                $exitCode = $result.ExitCode
                $markerReturned = Test-ProbeMarker $commands[$index].Name $result.Text
                $diagnostic = if ($result.Diagnostic -cin @('Client exceeded the synthetic check deadline.', 'Client launch or capture failed.', 'Client containment unavailable. Check Windows job nesting policy; no client was started.')) { $result.Diagnostic } else { 'Client did not return a successful completed marker.' }
            } catch { }
            $success = $null -ne $exitCode -and $exitCode -eq 0 -and $markerReturned
            $results.Add([ordered]@{ client = $commands[$index].Name; passed = $success; exit_code = $exitCode; marker_returned = $markerReturned; diagnostic = $(if ($success) { '' } else { $diagnostic }) })
        }
    } finally {
        if ([IO.Directory]::Exists($temporary)) { [IO.Directory]::Delete($temporary, $true) }
    }
    $report = [ordered]@{ endpoint = $Endpoint; model = $Model; results = $results.ToArray(); real_settings_modified = $false; project_content_sent = $false; tools_requested = $false; codex_plugins_enabled = $false }
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($outputPath)) | Out-Null
    $json = $report | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($outputPath, $json + "`n", [Text.UTF8Encoding]::new($false))
    return @{ Json = $json; ExitCode = $(if (@($results | Where-Object { -not $_.passed }).Count) { 1 } else { 0 }) }
}

if ($MyInvocation.InvocationName -ne '.') {
    try {
        $result = Invoke-NativeClientProbe -Endpoint $Endpoint -Codex $Codex -Claude $Claude -Model $Model -Output $Output
        Write-Output $result.Json
        exit $result.ExitCode
    } catch {
        [Console]::Error.WriteLine('Probe failed. Check loopback endpoint, executable paths, model and report path.')
        exit 2
    }
}
