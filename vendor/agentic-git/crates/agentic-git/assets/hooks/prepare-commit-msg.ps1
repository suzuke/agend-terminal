# agentic-git prepare-commit-msg hook (PowerShell) — injects fleet trailers.
# Windows equivalent of the bash hook.

param($CommitMsgFile, $CommitSource)

# Skip merge/squash/template commits.
if ($CommitSource -in @("merge", "squash", "template")) { exit 0 }

# Legacy fallbacks mirror the bash hook: a legacy agend-terminal fleet only
# sets AGEND_INSTANCE_NAME / AGEND_HOME.
$Agent = $env:AGENTIC_GIT_AGENT
if (-not $Agent) { $Agent = $env:AGEND_INSTANCE_NAME }
if (-not $Agent) { exit 0 }

$HomeDir = $env:AGENTIC_GIT_HOME
if (-not $HomeDir) { $HomeDir = $env:AGEND_HOME }
if (-not $HomeDir) { exit 0 }

$Binding = Join-Path $HomeDir "runtime" $Agent "binding.json"
if (-not (Test-Path $Binding)) { exit 0 }

# Idempotent: skip if trailer already present.
$Content = Get-Content $CommitMsgFile -Raw -ErrorAction SilentlyContinue
if ($Content -match "^Agentic-Agent:") { exit 0 }

# Parse binding.json.
try {
    $Json = Get-Content $Binding -Raw | ConvertFrom-Json
    $Task = $Json.task_id
    $Branch = $Json.branch
    $Issued = $Json.issued_at
} catch { exit 0 }

# Append trailers.
$Trailers = "`n`nAgentic-Agent: $Agent"
if ($Task) { $Trailers += "`nAgentic-Task: $Task" }
if ($Branch) { $Trailers += "`nAgentic-Branch: $Branch" }
if ($Issued) { $Trailers += "`nAgentic-Issued-At: $Issued" }

Add-Content -Path $CommitMsgFile -Value $Trailers
exit 0
