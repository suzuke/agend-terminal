#!/bin/bash
# Context Cost Benchmark: MCP vs CLI instruction
# Measures actual token usage in Claude Code for different tool delivery methods.
#
# Prerequisites: claude CLI installed and authenticated
# Usage: bash tests/context-cost-benchmark.sh

set -euo pipefail

BENCHDIR=$(mktemp -d)
trap '[[ -d "$BENCHDIR" ]] && rm -rf "$BENCHDIR"' EXIT

echo "============================================="
echo "Context Cost Benchmark: MCP vs CLI"
echo "============================================="
echo ""

# --- Helper ---
measure() {
    local label=$1; shift
    local dir="$BENCHDIR/$label"
    mkdir -p "$dir"

    local result
    result=$(cd "$dir" && claude --output-format json \
        --dangerously-skip-permissions "$@" \
        -p "reply with just 'ok'" 2>/dev/null)

    local tokens
    tokens=$(echo "$result" | python3 -c "
import sys,json; u=json.load(sys.stdin)['usage']
print(u['cache_creation_input_tokens'], u['cache_read_input_tokens'], u['input_tokens'], u['output_tokens'])")

    echo "$label|$tokens"
}

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
            {"name":"edit_message","description":"Edit a previously sent message.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string"},"text":{"type":"string"}},"required":["message_id","text"]}},
            {"name":"download_attachment","description":"Download a file attachment.","inputSchema":{"type":"object","properties":{"file_id":{"type":"string"}},"required":["file_id"]}},
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
            {"name":"create_team","description":"Create a team.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"members":{"type":"array","items":{"type":"string"}}},"required":["name","members"]}},
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

# --- 3. CLI instruction (needs git init for Claude Code to load rules) ---
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
echo "Running 3 tests (each calls Claude API once)..."
echo ""

echo -n "[1/3] Baseline (no tools)... "
R1=$(measure "baseline")
echo "done"

echo -n "[2/3] MCP 37 tools... "
R2=$(measure "mcp37" --mcp-config "$BENCHDIR/mcp-37.json")
echo "done"

echo -n "[3/3] CLI instruction (.claude/rules/)... "
R3=$(measure "cli")
echo "done"

# --- Report ---
echo ""
echo "============================================="
echo "Results"
echo "============================================="
echo ""

python3 << PYEOF
rows = """$R1
$R2
$R3""".strip().split("\n")

data = []
for row in rows:
    parts = row.split("|")
    label = parts[0]
    nums = parts[1].split()
    data.append({
        "label": label,
        "cache_create": int(nums[0]),
        "cache_read": int(nums[1]),
        "input": int(nums[2]),
        "output": int(nums[3]),
    })

baseline = data[0]["cache_create"]

print(f"{'Scenario':<30} {'cache_create':>12} {'cache_read':>11} {'delta':>8} {'% of 200K':>10}")
print("-" * 73)
for d in data:
    delta = d["cache_create"] - baseline
    pct = delta / 200000 * 100
    delta_str = f"+{delta}" if delta > 0 else "—"
    pct_str = f"{pct:.3f}%" if delta > 0 else "—"
    print(f"{d['label']:<30} {d['cache_create']:>12,} {d['cache_read']:>11,} {delta_str:>8} {pct_str:>10}")

print()
mcp_delta = data[1]["cache_create"] - baseline
cli_delta = data[2]["cache_create"] - baseline
print(f"MCP 37 tools overhead:    {mcp_delta:>6} tokens")
print(f"CLI instruction overhead:  {cli_delta:>5} tokens")
if cli_delta > 0:
    print(f"MCP / CLI ratio:          {mcp_delta/cli_delta:.1f}x")
print()
print(f"On 200K context: MCP={mcp_delta/200000*100:.3f}%, CLI={cli_delta/200000*100:.3f}%")
print(f"On 128K context: MCP={mcp_delta/128000*100:.3f}%, CLI={cli_delta/128000*100:.3f}%")
print(f"On   1M context: MCP={mcp_delta/1000000*100:.4f}%, CLI={cli_delta/1000000*100:.4f}%")
PYEOF

echo ""
echo "Done. Temp files cleaned up."
