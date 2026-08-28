#!/usr/bin/env bash
# Deterministic given PAIR except for daemon-minted ids (message ids and task
# ids embed a timestamp + daemon pid, see the note in expect.py).  Every id the
# grader needs is therefore RECORDED in the seed JSON printed on stdout.
set -euo pipefail

HOME_DIR="${1:?usage: seed.sh HOME_DIR PAIR}"
PAIR="${2:?usage: seed.sh HOME_DIR PAIR}"
export AGEND_HOME="$HOME_DIR"
unset AGEND_INSTANCE_NAME 2>/dev/null || true

# Run `agend-terminal tool` as a given fleet identity.
as() { local who="$1"; shift; AGEND_INSTANCE_NAME="$who" agend-terminal tool "$@"; }
mktask() { as "$1" task --action create --json "$2" |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }

T3=$(mktask lead "$(python3 -c 'import json,sys; print(json.dumps({"title":"migrate the audit log writer","description":"pair-"+sys.argv[1]+" in-flight work","assignee":"probe","priority":"high"}))' "$PAIR")")
as probe task --action claim --arg id="$T3" >/dev/null
as probe task --action update --json "{\"id\":\"$T3\",\"status\":\"in_progress\"}" >/dev/null

T4=$(mktask lead "$(python3 -c 'import json,sys; print(json.dumps({"title":"backfill the retention index","description":"pair-"+sys.argv[1]+" brand new dispatch","priority":"urgent"}))' "$PAIR")")
# probe is already in_progress, so the daemon answers {"busy":true} and drops an
# unforced kind=task dispatch; the dispatcher must override to make it land.
DISPATCH="pair-${PAIR} new one for you: backfill the retention index before Friday"
as lead send --json - >/dev/null <<EOF
{"instance":"probe","request_kind":"task","task_id":"$T4","message":$(printf '%s' "$DISPATCH" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),"force":true,"force_reason":"pair-${PAIR} eval seeding"}
EOF

T3="$T3" T4="$T4" DISPATCH="$DISPATCH" python3 - "$PAIR" <<'PY'
import json, os, re, sys
HOME = os.environ["AGEND_HOME"]
PAIR = sys.argv[1]

def _instances():
    m, cur = {}, None
    for ln in open(os.path.join(HOME, "fleet.yaml"), encoding="utf-8"):
        g = re.match(r"^  ([A-Za-z0-9_.\-]+):\s*$", ln)
        if g:
            cur = g.group(1)
            continue
        g = re.match(r"^\s+id:\s*\"?([0-9a-fA-F-]{36})\"?\s*$", ln)
        if g and cur:
            m[cur] = g.group(1)
    return m

INST = _instances()

def rows(name):
    p = os.path.join(HOME, "inbox", INST[name] + ".jsonl")
    if not os.path.exists(p):
        return []
    out = []
    for ln in open(p, encoding="utf-8"):
        ln = ln.strip()
        if ln:
            try:
                out.append(json.loads(ln))
            except Exception:
                pass
    return out

def mid(name, text):
    c = [r for r in rows(name) if r.get("text") == text]
    if not c:
        sys.exit("seed: no inbox row for %s with text %r" % (name, text))
    return c[-1]["id"]

def mid_where(name, pred):
    c = [r for r in rows(name) if pred(r)]
    if not c:
        sys.exit("seed: no matching inbox row in %s" % name)
    return c[-1]


def emit(d):
    d["pair"] = PAIR
    d["instances"] = INST
    json.dump(d, sys.stdout, ensure_ascii=False, sort_keys=True)
    sys.stdout.write("\n")

T3, T4 = os.environ["T3"], os.environ["T4"]
row = mid_where("probe", lambda r: r.get("task_id") == T4 and r.get("kind") == "task")
seeded = 0
p = os.path.join(HOME, "task_events.jsonl")
if os.path.exists(p):
    for ln in open(p, encoding="utf-8"):
        ln = ln.strip()
        if not ln:
            continue
        try:
            e = json.loads(ln)["event"]
        except Exception:
            continue
        if e.get("task_id") == T3 and e.get("kind") == "Claimed":
            seeded += 1
emit({"scenario": "S06",
      "in_flight": {"id": T3, "seeded_claim_events": seeded},
      "new_task": {"id": T4},
      "dispatch": {"id": row["id"], "text": row["text"]}})
