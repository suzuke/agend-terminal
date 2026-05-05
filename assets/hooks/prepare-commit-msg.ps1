# agend-terminal prepare-commit-msg hook (PowerShell) — injects fleet trailers.
# Windows equivalent of the bash hook.

param($CommitMsgFile, $CommitSource)

# Skip merge/squash/template commits.
if ($CommitSource -in @("merge", "squash", "template")) { exit 0 }

$Agent = $env:AGEND_INSTANCE_NAME
if (-not $Agent) { exit 0 }

$HomeDir = $env:AGEND_HOME
if (-not $HomeDir) { exit 0 }

$Binding = Join-Path $HomeDir "runtime" $Agent "binding.json"
if (-not (Test-Path $Binding)) { exit 0 }

# Idempotent: skip if trailer already present.
$Content = Get-Content $CommitMsgFile -Raw -ErrorAction SilentlyContinue
if ($Content -match "^Agend-Agent:") { exit 0 }

# Parse binding.json.
try {
    $Json = Get-Content $Binding -Raw | ConvertFrom-Json
    $Task = $Json.task_id
    $Branch = $Json.branch
    $Issued = $Json.issued_at
} catch { exit 0 }

# Append trailers.
$Trailers = "`n`nAgend-Agent: $Agent"
if ($Task) { $Trailers += "`nAgend-Task: $Task" }
if ($Branch) { $Trailers += "`nAgend-Branch: $Branch" }
if ($Issued) { $Trailers += "`nAgend-Issued-At: $Issued" }

Add-Content -Path $CommitMsgFile -Value $Trailers
exit 0
