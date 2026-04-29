#!/bin/bash
# Context Cost Benchmark: MCP vs CLI instruction
# Measures actual token usage in Claude Code for different tool delivery methods.
#
# Prerequisites: claude CLI installed and authenticated
# Usage: bash tests/context-cost-benchmark.sh [RUNS]
#   RUNS: number of runs per scenario (default: 5, takes median)

set -euo pipefail

RUNS=${1:-5}
BENCHDIR=$(mktemp -d)
trap '[[ -d "$BENCHDIR" ]] && rm -rf "$BENCHDIR"' EXIT

echo "============================================="
echo "Context Cost Benchmark: MCP vs CLI"
echo "Runs per scenario: $RUNS (median)"
echo "============================================="
echo ""

# --- Helper: single measurement ---
measure_once() {
    local dir=$1; shift
    local result
    result=$(cd "$dir" && claude --output-format json \
        --dangerously-skip-permissions "$@" \
        -p "reply with just 'ok'" 2>/dev/null)
    echo "$result" | python3 -c "
import sys,json; u=json.load(sys.stdin)['usage']
print(u['cache_creation_input_tokens'])"
}

# --- Helper: N runs, return median ---
measure() {
    local label=$1; shift
    local dir="$BENCHDIR/$label"
    mkdir -p "$dir"

    local values=()
    for i in $(seq 1 "$RUNS"); do
        echo -n "  run $i/$RUNS... "
        local v
        v=$(measure_once "$dir" "$@")
        values+=("$v")
        echo "$v tokens"
    done

    # Median via python
    local median
    median=$(python3 -c "
v = sorted([$(IFS=,; echo "${values[*]}")])
n = len(v)
print(v[n//2] if n % 2 else (v[n//2-1] + v[n//2]) // 2)")
    echo "  → median: $median"
    echo "$label|$median"
}

# Routing rules shared by both MCP+rules and CLI-only tests
ROUTING_RULES='# AgEnD Terminal Tools

This project uses `agend_*` CLI tools for message routing.

## How to respond
- `[user:NAME via telegram]` → `agend_reply "text"`
- `[from:INSTANCE]` → `agend_send INSTANCE "text"`

## Examples
User: `[user:alice via telegram] hi` → Run: `agend_reply "Hello!"`
Agent: `[from:dev] review this` → Run: `agend_send dev "Sure!"`'

# --- 1. Fake MCP server with 37 tools ---
cat > "$BENCHDIR/mcp-server.py" << 'PYEOF'
import json, sys
def handle(req):
    m = req.get("method", "")
    if m == "initialize":
        return {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "agend-bench", "version": "0.1"}}
    if m == "tools/list":
        tools = [
            {"name":"reply","description":"Reply to the user via Telegram.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
            {"name":"react","description":"React to a message with an emoji.","inputSchema":{"type":"object","properties":{"emoji":{"type":"string"}},"required":["emoji"]}},
            {"name":"download_attachment","description":"Download a file attachment (telegram multimedia).","inputSchema":{"type":"object","properties":{"file_id":{"type":"string"}},"required":["file_id"]}},
            {"name":"send_to_instance","description":"Send a message to another agent instance.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"message":{"type":"string"},"request_kind":{"type":"string","enum":["query","task","report","update"]},"requires_reply":{"type":"boolean"}},"required":["instance_name","message"]}},
            {"name":"delegate_task","description":"Delegate a task to another instance.","inputSchema":{"type":"object","properties":{"target_instance":{"type":"string"},"task":{"type":"string"},"success_criteria":{"type":"string"},"context":{"type":"string"}},"required":["target_instance","task"]}},
            {"name":"report_result","description":"Report results back.","inputSchema":{"type":"object","properties":{"target_instance":{"type":"string"},"summary":{"type":"string"},"correlation_id":{"type":"string"},"artifacts":{"type":"string"}},"required":["target_instance","summary"]}},
            {"name":"request_information","description":"Ask another instance a question.","inputSchema":{"type":"object","properties":{"target_instance":{"type":"string"},"question":{"type":"string"},"context":{"type":"string"}},"required":["target_instance","question"]}},
            {"name":"broadcast","description":"Send a message to multiple instances.","inputSchema":{"type":"object","properties":{"message":{"type":"string"},"targets":{"type":"array","items":{"type":"string"}},"team":{"type":"string"}},"required":["message"]}},
            {"name":"inbox","description":"Check pending messages.","inputSchema":{"type":"object","properties":{}}},
            {"name":"list_instances","description":"List all active agent instances.","inputSchema":{"type":"object","properties":{}}},
            {"name":"create_instance","description":"Create a new agent instance.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"backend":{"type":"string"},"args":{"type":"string"},"model":{"type":"string"},"working_directory":{"type":"string"},"branch":{"type":"string"},"task":{"type":"string"},"role":{"type":"string"}},"required":["name"]}},
            {"name":"delete_instance","description":"Delete an agent instance.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
            {"name":"start_instance","description":"Start an existing agent instance.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
            {"name":"describe_instance","description":"Get details about an agent instance.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
            {"name":"replace_instance","description":"Replace an agent instance.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"reason":{"type":"string"}},"required":["name"]}},
            {"name":"set_display_name","description":"Set display name.","inputSchema":{"type":"object","properties":{"display_name":{"type":"string"}},"required":["display_name"]}},
            {"name":"set_description","description":"Set description.","inputSchema":{"type":"object","properties":{"description":{"type":"string"}},"required":["description"]}},
            {"name":"post_decision","description":"Post a decision.","inputSchema":{"type":"object","properties":{"title":{"type":"string"},"rationale":{"type":"string"},"alternatives":{"type":"string"},"status":{"type":"string"}},"required":["title","rationale"]}},
            {"name":"list_decisions","description":"List decisions.","inputSchema":{"type":"object","properties":{"status":{"type":"string"}}}},
            {"name":"update_decision","description":"Update a decision.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"status":{"type":"string"},"rationale":{"type":"string"}},"required":["id"]}},
            {"name":"task_create","description":"Create a task.","inputSchema":{"type":"object","properties":{"title":{"type":"string"},"description":{"type":"string"},"assignee":{"type":"string"},"priority":{"type":"string"}},"required":["title"]}},
            {"name":"task_list","description":"List tasks.","inputSchema":{"type":"object","properties":{"status":{"type":"string"},"assignee":{"type":"string"}}}},
            {"name":"task_claim","description":"Claim a task.","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}},
            {"name":"task_done","description":"Mark task done.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"summary":{"type":"string"}},"required":["id"]}},
            {"name":"task_update","description":"Update a task.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"status":{"type":"string"},"description":{"type":"string"}},"required":["id"]}},
            {"name":"list_teams","description":"List teams.","inputSchema":{"type":"object","properties":{}}},
            {"name":"delete_team","description":"Delete a team.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
            {"name":"update_team","description":"Update a team.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"members":{"type":"array","items":{"type":"string"}}},"required":["name"]}},
            {"name":"create_schedule","description":"Create a cron schedule.","inputSchema":{"type":"object","properties":{"cron":{"type":"string"},"target":{"type":"string"},"message":{"type":"string"},"label":{"type":"string"}},"required":["cron","target","message"]}},
            {"name":"list_schedules","description":"List schedules.","inputSchema":{"type":"object","properties":{"target":{"type":"string"}}}},
            {"name":"delete_schedule","description":"Delete a schedule.","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}},
            {"name":"deploy_template","description":"Deploy a fleet template.","inputSchema":{"type":"object","properties":{"template":{"type":"string"},"name":{"type":"string"}},"required":["template"]}},
            {"name":"teardown_deployment","description":"Teardown a deployment.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
            {"name":"list_deployments","description":"List deployments.","inputSchema":{"type":"object","properties":{}}},
            {"name":"checkout_repo","description":"Checkout a git repo.","inputSchema":{"type":"object","properties":{"source":{"type":"string"},"branch":{"type":"string"}},"required":["source"]}},
            {"name":"release_repo","description":"Release a repo.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}
        ]
        return {"tools": tools}
    if m == "notifications/initialized":
        return None
    return {"error": {"code": -32601, "message": "not found"}}
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    resp = handle(req)
    if resp is not None:
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req.get("id"),"result":resp}) + "\n")
        sys.stdout.flush()
PYEOF

# --- 2. MCP config ---
cat > "$BENCHDIR/mcp-37.json" << JSON
{"mcpServers":{"agend":{"command":"python3","args":["$BENCHDIR/mcp-server.py"]}}}
JSON

# --- 3. Setup: MCP + routing rules (apples-to-apples with CLI) ---
mkdir -p "$BENCHDIR/mcp37/.claude/rules"
(cd "$BENCHDIR/mcp37" && git init -q)
echo "$ROUTING_RULES" > "$BENCHDIR/mcp37/.claude/rules/agend.md"

# --- 4. Setup: CLI-only (routing rules + command reference) ---
mkdir -p "$BENCHDIR/cli/.claude/rules"
(cd "$BENCHDIR/cli" && git init -q)
cat > "$BENCHDIR/cli/.claude/rules/agend.md" << 'MD'
# AgEnD Terminal Tools

This project uses `agend_*` CLI tools for message routing.

## How to respond
- `[user:NAME via telegram]` → `agend_reply "text"`
- `[from:INSTANCE]` → `agend_send INSTANCE "text"`

## Available tools
```
agend_reply "text"                   Send reply to the current user
agend_send TARGET "text"             Message another agent
agend_delegate TARGET "task"         Assign work to an agent
agend_report TARGET "summary"        Report results to an agent
agend_ask TARGET "question"          Request info from an agent
agend_broadcast "message"            Message all agents
agend_inbox                          Check incoming messages
agend_list                           List available agents
agend_spawn NAME --backend claude    Create a new agent
agend_delete NAME                    Remove an agent
agend_describe NAME                  Get agent details
agend_task create/list/claim/done    Task board operations
agend_team create/list/delete        Team management
agend_schedule create/list/delete    Scheduling
```

## Examples
User: `[user:alice via telegram] hi` → Run: `agend_reply "Hello!"`
Agent: `[from:dev] review this` → Run: `agend_send dev "Sure!"`
MD

# --- Run tests ---
echo "Running 4 scenarios × $RUNS runs = $((4 * RUNS)) API calls..."
echo ""

echo "[1/4] Baseline (no tools, no rules)"
R1=$(measure "baseline")
echo ""

echo "[2/4] MCP 37 tools + routing rules"
R2=$(measure "mcp37" --mcp-config "$BENCHDIR/mcp-37.json")
echo ""

echo "[3/4] CLI instruction only (rules with command reference)"
R3=$(measure "cli")
echo ""

echo "[4/4] MCP 37 tools only (no routing rules)"
mkdir -p "$BENCHDIR/mcp37-bare"
R4=$(measure "mcp37-bare" --mcp-config "$BENCHDIR/mcp-37.json")
echo ""

# --- Report ---
echo "============================================="
echo "Results (median of $RUNS runs)"
echo "============================================="
echo ""

python3 << PYEOF
rows = """$R1
$R2
$R3
$R4""".strip().split("\n")

data = []
for row in rows:
    label, median = row.split("|")
    data.append({"label": label, "tokens": int(median)})

baseline = data[0]["tokens"]

print(f"{'Scenario':<40} {'median':>8} {'delta':>8} {'% of 200K':>10}")
print("-" * 68)
for d in data:
    delta = d["tokens"] - baseline
    pct = delta / 200000 * 100
    delta_str = f"+{delta}" if delta > 0 else "—"
    pct_str = f"{pct:.3f}%" if delta > 0 else "—"
    print(f"{d['label']:<40} {d['tokens']:>8,} {delta_str:>8} {pct_str:>10}")

print()
mcp_rules = data[1]["tokens"] - baseline
cli_only = data[2]["tokens"] - baseline
mcp_bare = data[3]["tokens"] - baseline

print(f"MCP tools + routing rules:  {mcp_rules:>5} tokens  (fair comparison with CLI)")
print(f"CLI instruction only:       {cli_only:>5} tokens")
print(f"MCP tools only (no rules):  {mcp_bare:>5} tokens  (tool schema cost alone)")
print(f"Routing rules alone:        {mcp_rules - mcp_bare:>5} tokens  (MCP+rules minus MCP-bare)")
if cli_only > 0:
    print(f"\nMCP+rules / CLI ratio:      {mcp_rules/cli_only:.2f}x")
print(f"\nOn 200K: MCP+rules={mcp_rules/200000*100:.3f}%, CLI={cli_only/200000*100:.3f}%")
print(f"On 128K: MCP+rules={mcp_rules/128000*100:.3f}%, CLI={cli_only/128000*100:.3f}%")
PYEOF

echo ""
echo "Done. Temp files cleaned up."
