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

T=$(mktask lead "$(python3 -c 'import json,sys; print(json.dumps({"title":"tighten the flake quarantine","description":"pair-"+sys.argv[1]+" status probe subject"}))' "$PAIR")")
as lead task --action claim --arg id="$T" >/dev/null
as lead task --action update --json "{\"id\":\"$T\",\"status\":\"in_review\"}" >/dev/null

ASK="pair-${PAIR} quick favour: run \`agend-terminal tool task --action get --arg id=$T\` and tell me its status"
as ane-review send --json - >/dev/null <<EOF
{"instance":"probe","request_kind":"query","message":$(printf '%s' "$ASK" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}
EOF

T="$T" ASK="$ASK" python3 - "$PAIR" <<'PY'
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

wire = "[request_information] %s" % os.environ["ASK"]
emit({"scenario": "S13", "task": {"id": os.environ["T"], "status": "in_review"},
      "ask": {"id": mid("probe", wire), "text": wire}})
