[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string] $Executable,
    [Parameter(Position = 1, ValueFromRemainingArguments = $true)]
    [string[]] $ArgumentList
)

$ErrorActionPreference = "Stop"
$TimeoutExitCode = 124
$InvariantFailureExitCode = 125

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    Write-Error "windows-nextest-watchdog.ps1 requires Windows"
    exit $InvariantFailureExitCode
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class AgendWindowsJobObject
{
    private const uint KillOnJobClose = 0x2000;
    private const int JobObjectExtendedLimitInformation = 9;

    [StructLayout(LayoutKind.Sequential)]
    private struct IoCounters
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimitInformation
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimitInformation
    {
        public BasicLimitInformation BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateJobObject(IntPtr attributes, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        IntPtr job,
        int informationClass,
        ref ExtendedLimitInformation information,
        uint length);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(IntPtr job, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    public static IntPtr CreateKillOnClose()
    {
        IntPtr job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero)
        {
            return IntPtr.Zero;
        }

        ExtendedLimitInformation information = new ExtendedLimitInformation();
        information.BasicLimitInformation.LimitFlags = KillOnJobClose;
        uint length = (uint)Marshal.SizeOf(typeof(ExtendedLimitInformation));
        if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, ref information, length))
        {
            CloseHandle(job);
            return IntPtr.Zero;
        }
        return job;
    }

    public static bool Assign(IntPtr job, IntPtr process)
    {
        return AssignProcessToJobObject(job, process);
    }

    public static bool Terminate(IntPtr job, uint exitCode)
    {
        return TerminateJobObject(job, exitCode);
    }

    public static void Close(IntPtr job)
    {
        if (job != IntPtr.Zero)
        {
            CloseHandle(job);
        }
    }

    public static int LastError()
    {
        return Marshal.GetLastWin32Error();
    }
}
'@ -ErrorAction Stop

$diagnosticsDirectory = $env:AGEND_NEXTTEST_DIAGNOSTICS_DIR
if ([string]::IsNullOrWhiteSpace($diagnosticsDirectory)) {
    $diagnosticsDirectory = Join-Path $PSScriptRoot "diagnostics"
}
New-Item -ItemType Directory -Path $diagnosticsDirectory -Force | Out-Null

$liveLogPath = Join-Path $diagnosticsDirectory "nextest-live.log"
$snapshotPath = Join-Path $diagnosticsDirectory "watchdog-diagnostics.json"
$liveLog = [IO.StreamWriter]::new($liveLogPath, $false, [Text.UTF8Encoding]::new($false))
$liveLog.AutoFlush = $true
$sinkLock = [object]::new()

function Write-Sink([string] $Stream, [string] $Line) {
    if ($null -eq $Line) {
        return
    }
    [Threading.Monitor]::Enter($sinkLock)
    try {
        $entry = "[$Stream] $Line"
        [Console]::WriteLine($entry)
        $liveLog.WriteLine($entry)
    }
    finally {
        [Threading.Monitor]::Exit($sinkLock)
    }
}

function Get-TimeoutSeconds {
    $value = $env:AGEND_WATCHDOG_TIMEOUT_SECONDS
    if ([string]::IsNullOrWhiteSpace($value)) {
        return 1200
    }
    $seconds = 0
    if (-not [int]::TryParse($value, [Globalization.NumberStyles]::Integer, [Globalization.CultureInfo]::InvariantCulture, [ref] $seconds) -or $seconds -le 0) {
        throw "AGEND_WATCHDOG_TIMEOUT_SECONDS must be a positive integer"
    }
    return $seconds
}

function Get-ProcessTreeState([int] $RootPid) {
    try {
        $all = @(Get-CimInstance Win32_Process -ErrorAction Stop | ForEach-Object {
                [pscustomobject] @{
                    pid = [int] $_.ProcessId
                    parent_pid = [int] $_.ParentProcessId
                    name = [string] $_.Name
                    command_line = [string] $_.CommandLine
                }
            })
    }
    catch {
        return [pscustomobject] @{ known = $false; processes = @() }
    }

    $seen = @{}
    $pending = [Collections.Generic.Queue[int]]::new()
    $pending.Enqueue($RootPid)
    while ($pending.Count -gt 0) {
        $current = $pending.Dequeue()
        $key = $current.ToString([Globalization.CultureInfo]::InvariantCulture)
        if ($seen.ContainsKey($key)) {
            continue
        }
        $seen[$key] = $true
        foreach ($candidate in $all) {
            if ([int] $candidate.parent_pid -eq $current) {
                $pending.Enqueue([int] $candidate.pid)
            }
        }
    }

    $processes = @($all | Where-Object {
            $seen.ContainsKey(([int] $_.pid).ToString([Globalization.CultureInfo]::InvariantCulture))
        })
    return [pscustomobject] @{ known = $true; processes = $processes }
}

function Get-ExactMarkers($Processes) {
    return @($Processes | Where-Object {
            $_.command_line -match '(^|\s)--exact(?:\s|$)'
        } | ForEach-Object { $_.command_line })
}

function Test-TreeGone([int] $RootPid) {
    $state = Get-ProcessTreeState $RootPid
    return $state.known -and @($state.processes).Count -eq 0
}

function Wait-TreeGone([int] $RootPid, [int] $Seconds = 10) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        if (Test-TreeGone $RootPid) {
            return $true
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    return $false
}

function Invoke-TaskkillTree([int] $RootPid, $KnownProcesses) {
    $targets = @($RootPid) + @($KnownProcesses | ForEach-Object {
            if ($_ -is [int]) { [int] $_ } else { [int] $_.pid }
        })
    foreach ($target in @($targets | Sort-Object -Unique)) {
        try {
            & taskkill.exe /PID $target /T /F 2>&1 | ForEach-Object {
                Write-Sink "taskkill" ([string] $_)
            }
            $code = $LASTEXITCODE
            Write-Sink "meta" "taskkill /T fallback exit=$code pid=$target"
        }
        catch {
            Write-Sink "meta" "taskkill /T fallback failed: $($_.Exception.Message) pid=$target"
        }
    }
}

function Stop-TrackedTree([IntPtr] $Job, [int] $RootPid, $KnownProcesses) {
    if ($Job -ne [IntPtr]::Zero) {
        $jobStopped = [AgendWindowsJobObject]::Terminate($Job, 1)
        Write-Sink "meta" "TerminateJobObject success=$jobStopped error=$([AgendWindowsJobObject]::LastError())"
    }
    if (Wait-TreeGone $RootPid 5) {
        return $true
    }
    Invoke-TaskkillTree $RootPid $KnownProcesses
    return (Wait-TreeGone $RootPid 10)
}

$timeoutSeconds = 1200
$job = [IntPtr]::Zero
$process = $null
$rootPid = 0
$rootExitCode = $InvariantFailureExitCode
$timedOut = $false
$terminationAttempted = $false
$treeGone = $false
$snapshotProcesses = @()
$exactMarkers = @()
$failure = $null

try {
    $timeoutSeconds = Get-TimeoutSeconds
    $psi = [Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $Executable
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    foreach ($argument in @($ArgumentList)) {
        [void] $psi.ArgumentList.Add($argument)
    }

    $job = [AgendWindowsJobObject]::CreateKillOnClose()
    if ($job -eq [IntPtr]::Zero) {
        throw "CreateJobObject/SetInformationJobObject failed: $([AgendWindowsJobObject]::LastError())"
    }

    $commandJson = @($Executable) + @($ArgumentList) | ConvertTo-Json -Compress
    Write-Sink "meta" "starting command=$commandJson timeout_seconds=$timeoutSeconds"

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $psi
    $process.EnableRaisingEvents = $true
    $process.add_OutputDataReceived({ param($sender, $event) Write-Sink "stdout" $event.Data })
    $process.add_ErrorDataReceived({ param($sender, $event) Write-Sink "stderr" $event.Data })
    if (-not $process.Start()) {
        throw "Process.Start returned false"
    }
    $rootPid = $process.Id
    if (-not [AgendWindowsJobObject]::Assign($job, $process.Handle)) {
        throw "AssignProcessToJobObject failed: $([AgendWindowsJobObject]::LastError())"
    }
    $process.BeginOutputReadLine()
    $process.BeginErrorReadLine()

    $waitMilliseconds = [int] ([Math]::Min([int]::MaxValue, [long] $timeoutSeconds * 1000))
    if ($process.WaitForExit($waitMilliseconds)) {
        $process.WaitForExit()
        $rootExitCode = $process.ExitCode
        $state = Get-ProcessTreeState $rootPid
        if ($state.known) {
            $snapshotProcesses = @($state.processes)
            $exactMarkers = @(Get-ExactMarkers $snapshotProcesses)
            if ($snapshotProcesses.Count -eq 0) {
                $treeGone = $true
            }
            else {
                $terminationAttempted = $true
                $treeGone = Stop-TrackedTree $job $rootPid $snapshotProcesses
                if (-not $treeGone) {
                    $failure = "normal exit left a process tree that could not be verified gone"
                    $rootExitCode = $InvariantFailureExitCode
                }
            }
        }
        else {
            $failure = "process-tree snapshot unavailable after normal exit"
            $rootExitCode = $InvariantFailureExitCode
        }
    }
    else {
        $timedOut = $true
        $terminationAttempted = $true
        $state = Get-ProcessTreeState $rootPid
        if ($state.known) {
            $snapshotProcesses = @($state.processes)
            $exactMarkers = @(Get-ExactMarkers $snapshotProcesses)
        }
        $treeGone = Stop-TrackedTree $job $rootPid $snapshotProcesses
        $rootExitCode = if ($treeGone) { $TimeoutExitCode } else { $InvariantFailureExitCode }
        if (-not $treeGone) {
            $failure = "timeout process tree could not be verified gone"
        }
    }
}
catch {
    $failure = $_.Exception.Message
    $rootExitCode = $InvariantFailureExitCode
    if ($rootPid -ne 0) {
        $terminationAttempted = $true
        $state = Get-ProcessTreeState $rootPid
        if ($state.known) {
            $snapshotProcesses = @($state.processes)
            $exactMarkers = @(Get-ExactMarkers $snapshotProcesses)
        }
        $treeGone = Stop-TrackedTree $job $rootPid $snapshotProcesses
    }
}
finally {
    if ($null -ne $process) {
        try {
            if (-not $process.HasExited) {
                $terminationAttempted = $true
                if ($rootPid -ne 0) {
                    $treeGone = Stop-TrackedTree $job $rootPid $snapshotProcesses
                }
            }
            [void] $process.WaitForExit(2000)
            if ($process.HasExited) {
                # The parameterless wait drains pending async stdout/stderr callbacks.
                $process.WaitForExit()
            }
        }
        catch {
            if ($null -eq $failure) {
                $failure = $_.Exception.Message
            }
            $rootExitCode = $InvariantFailureExitCode
        }
        try { $process.Dispose() } catch { }
    }

    $diagnostic = [ordered] @{
        timestamp_utc = [DateTime]::UtcNow.ToString("o")
        executable = $Executable
        arguments = @($ArgumentList)
        root_pid = $rootPid
        timeout_seconds = $timeoutSeconds
        timed_out = $timedOut
        termination_attempted = $terminationAttempted
        tree_gone = $treeGone
        exit_code = $rootExitCode
        failure = $failure
        exact_markers = @($exactMarkers)
        processes = @($snapshotProcesses)
    }
    $diagnostic | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $snapshotPath -Encoding utf8
    if ($null -ne $failure) {
        Write-Sink "error" $failure
    }
    Write-Sink "meta" "finished exit=$rootExitCode timed_out=$timedOut tree_gone=$treeGone"
    try { $liveLog.Dispose() } catch { }
    [AgendWindowsJobObject]::Close($job)
}

exit $rootExitCode
