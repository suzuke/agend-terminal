[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$TimeoutExitCode = 124

$watchdog = Join-Path $PSScriptRoot "windows-nextest-watchdog.ps1"
$powerShell = Get-Command pwsh.exe -ErrorAction SilentlyContinue
if ($null -eq $powerShell) {
    $powerShell = Get-Command powershell.exe -ErrorAction SilentlyContinue
}
if ($null -eq $powerShell) {
    throw "PowerShell executable not found"
}

$root = Join-Path ([IO.Path]::GetTempPath()) "agend-nextest-watchdog-smoke-$PID"
$fakeCargo = Join-Path $root "fake-cargo.ps1"

function Assert-Equal([int]$Actual, [int]$Expected, [string]$Label) {
    if ($Actual -ne $Expected) {
        throw "${Label}: expected $Expected, got $Actual"
    }
}

function Assert-True([bool]$Condition, [string]$Label) {
    if (-not $Condition) {
        throw "${Label}: assertion failed"
    }
}

try {
    Assert-True (Test-Path -LiteralPath $watchdog -PathType Leaf) "watchdog script exists"
    New-Item -ItemType Directory -Path $root -Force | Out-Null

    @'
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string] $Case,
    [Parameter(Position = 1)]
    [string] $Marker,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $Rest
)

$ErrorActionPreference = "Stop"

Write-Output "fake stdout $Case"
[Console]::Error.WriteLine("fake stderr $Case")

switch ($Case) {
    "exit0" {
        exit 0
    }
    "exit7" {
        exit 7
    }
    "hung" {
        $childPowerShell = Get-Command pwsh.exe -ErrorAction SilentlyContinue
        if ($null -eq $childPowerShell) {
            $childPowerShell = Get-Command powershell.exe -ErrorAction Stop
        }
        $child = Start-Process -FilePath $childPowerShell.Source -ArgumentList @(
            "-NoProfile", "-File", $PSCommandPath, "child", $Marker, "--exact"
        ) -PassThru -WindowStyle Hidden
        Set-Content -LiteralPath $Marker -Value $child.Id -NoNewline
        Wait-Process -Id $child.Id
        exit 0
    }
    "child" {
        while ($true) {
            Start-Sleep -Seconds 1
        }
    }
    default {
        throw "unknown smoke case: $Case"
    }
}
'@ | Set-Content -LiteralPath $fakeCargo -Encoding utf8

    $oldDiagnostics = $env:AGEND_NEXTTEST_DIAGNOSTICS_DIR
    $oldTimeout = $env:AGEND_WATCHDOG_TIMEOUT_SECONDS
    $env:AGEND_WATCHDOG_TIMEOUT_SECONDS = "5"

    try {
        foreach ($case in @(
            @{ Name = "exit0"; Expected = 0 },
            @{ Name = "exit7"; Expected = 7 },
            @{ Name = "hung"; Expected = 124 }
        )) {
            $diagnostics = Join-Path $root "diagnostics-$($case.Name)"
            New-Item -ItemType Directory -Path $diagnostics -Force | Out-Null
            $env:AGEND_NEXTTEST_DIAGNOSTICS_DIR = $diagnostics
            $marker = Join-Path $diagnostics "child.pid"
            $arguments = @(
                "-NoProfile",
                "-File",
                $watchdog,
                $powerShell.Source,
                "-NoProfile",
                "-File",
                $fakeCargo,
                $case.Name,
                $marker,
                "--exact"
            )

            & $powerShell.Source @arguments
            $actual = $LASTEXITCODE
            Assert-Equal $actual $case.Expected "$($case.Name) exit code"

            $snapshotPath = Join-Path $diagnostics "watchdog-diagnostics.json"
            Assert-True (Test-Path -LiteralPath $snapshotPath -PathType Leaf) "$($case.Name) diagnostics"
            $liveLogPath = Join-Path $diagnostics "nextest-live.log"
            $liveLog = Get-Content -LiteralPath $liveLogPath -Raw
            Assert-True ($liveLog -match "\[stdout\]") "$($case.Name) stdout drain"
            Assert-True ($liveLog -match "\[stderr\]") "$($case.Name) stderr drain"
            $snapshot = Get-Content -LiteralPath $snapshotPath -Raw | ConvertFrom-Json
            if ($case.Name -eq "hung") {
                Assert-Equal $actual $TimeoutExitCode "hung timeout contract"
                Assert-True ([bool]$snapshot.timed_out) "hung timeout marker"
                Assert-True ([bool]$snapshot.tree_gone) "hung tree-gone marker"
                Assert-True ($snapshot.exact_markers.Count -gt 0) "hung --exact marker"
                $childPid = [int](Get-Content -LiteralPath $marker -Raw)
                Assert-True ($null -eq (Get-Process -Id $childPid -ErrorAction SilentlyContinue)) "hung child terminated"
            }
            Write-Host "SMOKE PASS $($case.Name) exit=$actual"
        }
    }
    finally {
        if ($null -eq $oldDiagnostics) {
            Remove-Item Env:AGEND_NEXTTEST_DIAGNOSTICS_DIR -ErrorAction SilentlyContinue
        }
        else {
            $env:AGEND_NEXTTEST_DIAGNOSTICS_DIR = $oldDiagnostics
        }
        if ($null -eq $oldTimeout) {
            Remove-Item Env:AGEND_WATCHDOG_TIMEOUT_SECONDS -ErrorAction SilentlyContinue
        }
        else {
            $env:AGEND_WATCHDOG_TIMEOUT_SECONDS = $oldTimeout
        }
    }
}
finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}

# Do not leak the expected hung-case timeout code to the CI shell.
exit 0
