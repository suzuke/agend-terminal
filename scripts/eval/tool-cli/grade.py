#!/usr/bin/env python3
"""Grade one tool-cli Phase 0b run directory, or aggregate a whole matrix.

SPEC.txt sections 4 and 9.  Standard library only.

Execution-time statistics are deliberately absent: the grader counts the four
cells of the paired table and LOOKS UP the frozen acceptance table.  The only
path that computes an interval at grade time is the N != 60 fallback, which
delegates to tango.py and flags the summary.

Observed on-disk shapes (recorded from a real daemon on 2026-08-28, release
binary at b733c4de; see tests/fixtures/final_state_sample/):

  fleet.yaml            instances.<name>.id = UUID assigned on first load; the
                        UUID is the inbox file stem, so the name<->uuid map has
                        to come from fleet.yaml.
  inbox/<uuid>.jsonl    one row per line:
                        {"schema_version":1,"id":"m-<ts>-<n>","from":"from:lead",
                         "from_id":"<sender uuid>","text":...,"kind":"update|task|
                         report|query|...","timestamp":...,"read_at":null|ts,
                         "delivery_mode":...,"task_id"?,"correlation_id"?,
                         "parent_id"?,"thread_id"?,"delivery_count"?,
                         "delivering_at"?,"first_delivered_at"?}
  task_events.jsonl     {"schema_version":3,"seq":n,"timestamp":...,
                         "instance":"probe","emitter_id":"<uuid>",
                         "event":{"kind":"Created|Claimed|InProgress|Done|...",
                                  "task_id":"t-<ts>-<pid>-<n>", ...}}
                        Done carries source.result.  There is NO task snapshot
                        file on disk (catalog.checkpoint.json stays empty in a
                        short-lived home), so task state is FOLDED from these
                        events.
  sent_ledger.jsonl     {"message_id","agent","channel","chat_id","topic_id",
                         "excerpt","task_id","correlation_id","ts"} — written
                        ONLY for outbound operator/channel replies.  Agent ->
                        agent sends do NOT land here (verified empty after four
                        instance-to-instance sends), so duplicate-send detection
                        reads the RECIPIENT inbox rows instead.
  mcp-usage-stats.jsonl {"tool","action","opt_params","ts"} per tool call,
                        identical for the MCP bridge and the CLI front end.
  task_index.jsonl      does not exist in this build (SPEC section 3 lists it);
                        loaded when present, otherwise ignored.
"""

from __future__ import annotations

import argparse
import collections
import importlib.util
import json
import os
import re
import shlex
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

# expect.py files do `from grade import Verdict`.  When grade.py runs as a
# script it is `__main__`, so that import would build a SECOND copy of this
# module with a different `Verdict` class and every isinstance check would fail.
# Publish this module under its import name so both spellings share one class.
sys.modules.setdefault("grade", sys.modules[__name__])

# ---------------------------------------------------------------------------
# frozen constants
# ---------------------------------------------------------------------------

DELTA_DEFINITION = "fail_cli - fail_mcp"
AGENT_UNDER_TEST = "probe"
TARGET_N = 60
#: The only two arms a run may declare. `check_mixing` COMPARES against these,
#: so a run whose arm is outside them cannot be checked at all — it is invalid,
#: not clean (#3412 review F2).
#: The model the frozen runner requests (run.sh MODEL). A run resolved to any
#: other model is a different experiment wearing this matrix's name.
FROZEN_MODEL = "claude-fable-5"

ARMS = ("mcp", "cli")


def _is_str(value):
    return isinstance(value, str) and value.strip() != ""


def _is_int(value):
    # bool is an int in Python; a flag is not a count.
    return isinstance(value, int) and not isinstance(value, bool)


#: What SPEC.txt section 3 says a run records, with the type each field must
#: have. model_requested/model_resolved are checked separately, with their own
#: reasons. Extra keys are fine — this is a floor, not a schema.
REQUIRED_META = (
    ("schema", lambda v: _is_int(v) and v == 1),
    ("scenario", _is_str),
    ("pair", lambda v: _is_int(v) and v >= 1),
    ("order_in_pair", lambda v: v in ("first", "second", "only")),
    ("claude_version", _is_str),
    ("git_head", _is_str),
    ("binary_sha256", lambda v: isinstance(v, dict) and all(
        _is_str(v.get(name)) for name in ("agend-terminal", "agend-mcp-bridge"))),
    ("system_prompt_sha256", _is_str),
    ("prompt_sha256", _is_str),
    ("fence", lambda v: v is True),
    ("fleet_sha256", _is_str),
    ("seed_sha256", _is_str),
    ("started_at", _is_str),
    ("ended_at", _is_str),
    ("duration_ms", _is_int),
    ("exit_code", _is_int),
    ("turns", _is_int),
    ("timed_out", lambda v: isinstance(v, bool)),
    ("invalid_reason", lambda v: v is None or isinstance(v, str)),
)

#: The fields that say WHICH experiment a run belongs to.
IDENTITY_FIELDS = ("git_head", "binary_sha256", "model_requested", "model_resolved",
                   "fleet_sha256", "claude_version", "seed_sha256", "prompt_sha256",
                   "system_prompt_sha256")


def frozen_order(pair, arm):
    """Which arm goes first in a pair: odd -> mcp, even -> cli (run.sh's rule).

    Single-arm scenarios record it too — SPEC.txt:65 gives metadata only
    ("first"|"second") — so plan, run and resume all name the same value.
    """
    return "first" if arm == ("mcp" if pair % 2 else "cli") else "second"


def _frozen_plan():
    """SPEC section 6's plan, in the order matrix.sh lays it down."""
    plan = []
    for index in range(1, 7):
        scenario = "S%02d" % index
        for pair in range(1, 11):
            # predeclared interleave: odd pair -> mcp first, even -> cli first
            for arm in (("mcp", "cli") if pair % 2 else ("cli", "mcp")):
                plan.append((scenario, pair, arm))
    plan.extend(("S13", pair, "mcp") for pair in range(1, 46))
    plan.extend(("S14", pair, "cli") for pair in range(1, 46))
    return tuple(plan)


FROZEN_PLAN = _frozen_plan()
FROZEN_PLAN_CELLS = frozenset(FROZEN_PLAN)

#: What matrix.sh records about the run it is describing. A tree with no account
#: of itself, or a partial one, is not a matrix we can accept.
MANIFEST_FIELDS = ("schema", "stamp", "created_at", "dry_run", "git_head", "model",
                   "jobs", "binary_sha256", "prompt_sha256", "missing_scenarios",
                   "total_runs", "plan")

MIXING_DENOMINATOR = 45
CONFIRMATION_SCENARIOS = ("S01", "S02", "S03", "S04", "S05", "S06")
MIXING_SCENARIOS = ("S13", "S14")
MCP_PREFIX = "mcp__"
AGEND_MCP_PREFIX = "mcp__agend-terminal__"
CLI_BINARY = "agend-terminal"

TAXONOMY_PATH = os.path.join(HERE, "taxonomy.json")
TABLE_PATH = os.path.join(HERE, "acceptance_table.json")


def load_taxonomy():
    with open(TAXONOMY_PATH, "r", encoding="utf-8") as fh:
        return json.load(fh)


CRITICAL_CLASSES = tuple(load_taxonomy()["critical"])


class Verdict:
    """Result of a scenario-specific grade(ctx)."""

    def __init__(self, passed, critical=None, notes=None):
        self.passed = bool(passed)
        self.critical = list(critical or [])
        self.notes = list(notes or [])
        for name in self.critical:
            if name not in CRITICAL_CLASSES:
                raise ValueError("unknown critical class %r" % (name,))

    def as_dict(self):
        return {"passed": self.passed, "critical": self.critical, "notes": self.notes}


def _is_verdict(value):
    """Accept a Verdict from any module identity (see the sys.modules note)."""
    if isinstance(value, Verdict):
        return True
    return (type(value).__name__ == "Verdict"
            and hasattr(value, "passed") and hasattr(value, "critical")
            and hasattr(value, "notes"))


def critical(name):
    """Validate and return a taxonomy critical class name (used by expect.py)."""
    if name not in CRITICAL_CLASSES:
        raise ValueError("unknown critical class %r" % (name,))
    return name


# ---------------------------------------------------------------------------
# tool-call normalisation
# ---------------------------------------------------------------------------

HEREDOC_RE = re.compile(
    r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1[^\n]*\n(.*?)\n[ \t]*\2[ \t]*(?=\n|$)",
    re.DOTALL,
)
SEPARATORS = {"&&", "||", ";", "|", "&", "\n"}
VALUE_OPTIONS = {"--action", "--json", "--arg", "--home", "--request-id"}
FALLBACK_TOOL_RE = re.compile(r"\bagend-terminal\s+tool\s+([A-Za-z_][A-Za-z0-9_-]*)")
FALLBACK_ACTION_RE = re.compile(r"--action[=\s]+([A-Za-z_][A-Za-z0-9_-]*)")


def _strip_heredocs(command):
    """Return (command_without_heredocs, [heredoc bodies in order])."""
    bodies = []

    def take(match):
        bodies.append(match.group(3))
        return " "

    return HEREDOC_RE.sub(take, command), bodies


def normalise_bash_command(command):
    """Extract every `agend-terminal tool ...` invocation from a Bash command.

    Never raises: an unparsable command degrades to args=None with the raw
    command retained (SPEC section 4).
    """
    if not isinstance(command, str) or CLI_BINARY not in command:
        return []
    stripped, heredocs = _strip_heredocs(command)
    try:
        tokens = shlex.split(stripped, comments=False, posix=True)
    except ValueError:
        return _fallback_parse(command)
    # shlex does not split on ';' — `tool schema inbox;` yields the token
    # "inbox;". Peel trailing shell punctuation into its own separator token.
    split_tokens = []
    for tok in tokens:
        if len(tok) > 1 and tok[-1] in ";&|" and not tok.startswith("-"):
            split_tokens.append(tok.rstrip(";&|"))
            split_tokens.append(";")
        else:
            split_tokens.append(tok)
    tokens = split_tokens
    calls = []
    heredoc_cursor = [0]
    i = 0
    while i < len(tokens):
        if (os.path.basename(tokens[i]) == CLI_BINARY
                and i + 1 < len(tokens) and tokens[i + 1] == "tool"):
            call, i = _parse_one(tokens, i + 2, command, heredocs, heredoc_cursor)
            calls.append(call)
        else:
            i += 1
    if not calls and FALLBACK_TOOL_RE.search(command):
        return _fallback_parse(command)
    return calls


def _fallback_parse(command):
    out = []
    for match in FALLBACK_TOOL_RE.finditer(command):
        action = FALLBACK_ACTION_RE.search(command)
        out.append({
            "surface": "cli",
            "tool": match.group(1),
            "action": action.group(1) if action else None,
            "args": None,
            "raw": command,
            "parse_error": "unparsable shell command",
        })
    return out


def _parse_one(tokens, start, raw_command, heredocs, cursor):
    tool = None
    action = None
    json_text = None
    json_unresolved = None
    arg_pairs = []
    i = start
    while i < len(tokens):
        tok = tokens[i]
        if tok in SEPARATORS:
            break
        if os.path.basename(tok) == CLI_BINARY and i + 1 < len(tokens) and tokens[i + 1] == "tool":
            break
        if tok in VALUE_OPTIONS:
            value = tokens[i + 1] if i + 1 < len(tokens) else None
            if tok == "--action":
                action = value
            elif tok == "--arg" and value is not None:
                arg_pairs.append(value)
            elif tok == "--json" and value is not None:
                if value == "-":
                    if cursor[0] < len(heredocs):
                        json_text = heredocs[cursor[0]]
                        cursor[0] += 1
                    else:
                        json_unresolved = "stdin body not found"
                elif value.startswith("@"):
                    json_unresolved = "json from file %s" % value[1:]
                else:
                    json_text = value
            i += 2
            continue
        if tok.startswith("--") and "=" in tok:
            key, value = tok.split("=", 1)
            if key == "--action":
                action = value
            elif key == "--arg":
                arg_pairs.append(value)
            elif key == "--json":
                if value == "-":
                    if cursor[0] < len(heredocs):
                        json_text = heredocs[cursor[0]]
                        cursor[0] += 1
                    else:
                        json_unresolved = "stdin body not found"
                else:
                    json_text = value
            i += 1
            continue
        if tok.startswith("-"):
            i += 1
            continue
        if tool is None:
            tool = tok
        elif tool in ("schema", "list"):
            tool = "%s %s" % (tool, tok)
        i += 1

    args = {}
    parse_error = json_unresolved
    if json_text is not None:
        try:
            decoded = json.loads(json_text)
        except (ValueError, TypeError):
            decoded = None
            parse_error = "invalid --json payload"
        if isinstance(decoded, dict):
            args.update(decoded)
        else:
            args = None
            if parse_error is None:
                parse_error = "--json payload is not an object"
    if args is not None:
        for pair in arg_pairs:
            key, sep, value = pair.partition("=")
            if sep:
                args[key] = value
            else:
                args[key] = True
    if json_unresolved is not None and args == {}:
        args = None
    if action is None and isinstance(args, dict):
        action = args.get("action")
    call = {
        "surface": "cli",
        "tool": tool,
        "action": action,
        "args": args,
        "raw": raw_command,
    }
    if parse_error:
        call["parse_error"] = parse_error
    return call, i


def _iter_tool_use(node):
    """Depth-first walk yielding every tool_use block in a stream-json event."""
    if isinstance(node, dict):
        if node.get("type") == "tool_use" and "name" in node:
            yield node
        for value in node.values():
            for found in _iter_tool_use(value):
                yield found
    elif isinstance(node, list):
        for value in node:
            for found in _iter_tool_use(value):
                yield found


def _tool_results_by_id(events):
    """Map tool_use id -> (is_error, content_text) from user tool_result blocks."""
    results = {}
    for event in events:
        if event.get("type") != "user":
            continue
        message = event.get("message") or {}
        for block in message.get("content") or []:
            if isinstance(block, dict) and block.get("type") == "tool_result":
                content = block.get("content")
                if isinstance(content, list):
                    content = "".join(str(c.get("text", "")) if isinstance(c, dict) else str(c)
                                      for c in content)
                results[block.get("tool_use_id")] = (bool(block.get("is_error")), str(content or ""))
    return results


def _outcome_for(surface, is_error, text):
    """ok | error | indeterminate — what the agent could observe after the call.
    A resend after `error` is NOT a duplicate side effect (nothing happened);
    a resend after `ok`/`indeterminate` is."""
    head = text.lstrip()[:200]
    if surface == "cli":
        m = re.match(r"Exit code (\d+)", head)
        if m:
            code = int(m.group(1))
            return "error" if code in (1, 2, 3) else ("indeterminate" if code == 4 else "ok")
        if '"status": "accepted_in_progress"' in text or '"status":"accepted_in_progress"' in text:
            return "indeterminate"
        return "error" if is_error else "ok"
    if is_error:
        return "error"
    if '"accepted_in_progress"' in text:
        return "indeterminate"
    if re.search(r'"error"\s*:', head):
        return "error"
    return "ok"


def normalise_tool_calls(events):
    calls = []
    results = _tool_results_by_id(events)
    for event in events:
        for block in _iter_tool_use(event):
            block_result = results.get(block.get("id"))
            start = len(calls)
            name = block.get("name") or ""
            payload = block.get("input")
            if name.startswith(AGEND_MCP_PREFIX):
                args = payload if isinstance(payload, dict) else None
                calls.append({
                    "surface": "mcp",
                    "tool": name[len(AGEND_MCP_PREFIX):],
                    "action": args.get("action") if isinstance(args, dict) else None,
                    "args": args,
                    "raw": {"name": name, "input": payload},
                })
            elif name.startswith(MCP_PREFIX):
                args = payload if isinstance(payload, dict) else None
                calls.append({
                    "surface": "mcp",
                    "tool": name,
                    "action": args.get("action") if isinstance(args, dict) else None,
                    "args": args,
                    "raw": {"name": name, "input": payload},
                })
            elif name == "Bash":
                command = payload.get("command") if isinstance(payload, dict) else None
                cli_calls = normalise_bash_command(command or "")
                if cli_calls:
                    calls.extend(cli_calls)
                else:
                    calls.append({"surface": "other", "tool": "Bash", "action": None,
                                  "args": payload if isinstance(payload, dict) else None,
                                  "raw": {"name": name, "input": payload}})
            else:
                calls.append({"surface": "other", "tool": name, "action": None,
                              "args": payload if isinstance(payload, dict) else None,
                              "raw": {"name": name, "input": payload}})
            for call in calls[start:]:
                if block_result is None:
                    call["outcome"] = None
                else:
                    call["outcome"] = _outcome_for(call["surface"], block_result[0], block_result[1])
    return calls


# ---------------------------------------------------------------------------
# final_state loading
# ---------------------------------------------------------------------------

def parse_fleet_yaml(text):
    """Minimal indent parser for the two-level fleet.yaml this eval writes."""
    instances = {}
    in_instances = False
    current = None
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        stripped = line.strip()
        if indent == 0:
            in_instances = stripped.startswith("instances:")
            current = None
            continue
        if not in_instances:
            continue
        if stripped.endswith(":") and ":" not in stripped[:-1]:
            current = stripped[:-1].strip().strip("\"'")
            instances[current] = {}
        elif current is not None and ":" in stripped:
            key, _, value = stripped.partition(":")
            instances[current][key.strip()] = value.strip().strip("\"'")
    return instances


def _read_jsonl(path):
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except ValueError:
                rows.append({"_unparsable": line})
    return rows


TASK_STATUS_BY_KIND = {
    "Created": "open",
    "Claimed": "claimed",
    "InProgress": "in_progress",
    "Verified": "verified",
    "Done": "done",
    "Cancelled": "cancelled",
    "Superseded": "superseded",
    "Blocked": "blocked",
    "Unblocked": "in_progress",
    "Reopened": "open",
    "Released": "open",
    "MovedToBacklog": "backlog",
    "MovedToReview": "in_review",
}


def fold_tasks(events):
    """Fold task_events.jsonl into per-task state (no snapshot file exists)."""
    tasks = {}
    for row in events:
        event = row.get("event")
        if not isinstance(event, dict):
            continue
        kind = event.get("kind")
        task_id = event.get("task_id")
        if not task_id:
            continue
        task = tasks.setdefault(task_id, {
            "id": task_id, "status": None, "assignee": None, "title": None,
            "result": None, "created_by": None, "events": [], "actors": [],
        })
        task["events"].append(kind)
        task["actors"].append(row.get("instance"))
        if kind in TASK_STATUS_BY_KIND:
            task["status"] = TASK_STATUS_BY_KIND[kind]
        if kind == "Created":
            task["title"] = event.get("title")
            task["created_by"] = row.get("instance")
            if event.get("owner"):
                task["assignee"] = event.get("owner")
        elif kind in ("Claimed", "InProgress"):
            task["assignee"] = event.get("by") or task["assignee"]
        elif kind == "Released":
            task["assignee"] = None
        elif kind == "OwnerAssigned":
            task["assignee"] = event.get("owner") or task["assignee"]
        elif kind == "Done":
            task["assignee"] = event.get("by") or task["assignee"]
            source = event.get("source")
            if isinstance(source, dict) and source.get("result") is not None:
                task["result"] = source.get("result")
        elif kind == "ResultSet":
            task["result"] = event.get("result", task["result"])
    return tasks


def load_final_state(final_dir):
    """Load a copied final_state/ tree.  Missing files degrade to empties."""
    state = {"inbox": {}, "tasks": {}, "task_events": [], "task_index": [],
             "sent_ledger": [], "decisions": [], "mcp_usage": [],
             "instances": {}, "uuid_to_name": {}, "root": final_dir}
    if not final_dir or not os.path.isdir(final_dir):
        return state
    root = final_dir
    if os.path.isdir(os.path.join(root, "home")):
        root = os.path.join(root, "home")
    state["root"] = root

    fleet_path = os.path.join(root, "fleet.yaml")
    if os.path.exists(fleet_path):
        with open(fleet_path, "r", encoding="utf-8", errors="replace") as fh:
            state["instances"] = parse_fleet_yaml(fh.read())
        for name, cfg in state["instances"].items():
            if cfg.get("id"):
                state["uuid_to_name"][cfg["id"]] = name

    inbox_dir = os.path.join(root, "inbox")
    if os.path.isdir(inbox_dir):
        for entry in sorted(os.listdir(inbox_dir)):
            if not entry.endswith(".jsonl"):
                continue
            stem = entry[: -len(".jsonl")]
            owner = state["uuid_to_name"].get(stem, stem)
            state["inbox"][owner] = _read_jsonl(os.path.join(inbox_dir, entry))

    state["task_events"] = _read_jsonl(os.path.join(root, "task_events.jsonl"))
    state["task_index"] = _read_jsonl(os.path.join(root, "task_index.jsonl"))
    state["sent_ledger"] = _read_jsonl(os.path.join(root, "sent_ledger.jsonl"))
    state["mcp_usage"] = _read_jsonl(os.path.join(root, "mcp-usage-stats.jsonl"))
    state["tasks"] = fold_tasks(state["task_events"])

    decisions_dir = os.path.join(root, "decisions")
    if os.path.isdir(decisions_dir):
        for dirpath, _dirnames, filenames in os.walk(decisions_dir):
            for filename in sorted(filenames):
                path = os.path.join(dirpath, filename)
                if filename.endswith(".jsonl"):
                    state["decisions"].extend(_read_jsonl(path))
                elif filename.endswith(".json"):
                    try:
                        with open(path, "r", encoding="utf-8") as fh:
                            state["decisions"].append(json.load(fh))
                    except ValueError:
                        pass
    return state


def sender_name(row, uuid_to_name):
    raw = row.get("from")
    if isinstance(raw, str):
        name = raw.split(":", 1)[1] if raw.startswith("from:") else raw
        if name:
            return name
    return uuid_to_name.get(row.get("from_id"), row.get("from_id"))


# ---------------------------------------------------------------------------
# context
# ---------------------------------------------------------------------------

class Ctx:
    def __init__(self, run_dir, meta, events, tool_calls, final, seed):
        self.run_dir = run_dir
        self.meta = meta
        self.events = events
        self.tool_calls = tool_calls
        self.final = final
        self.seed = seed
        self.scenario = meta.get("scenario")
        self.arm = meta.get("arm")
        self.pair = meta.get("pair")

    # -- convenience readers used by expect.py -----------------------------
    def inbox(self, instance):
        return self.final["inbox"].get(instance, [])

    def task(self, task_id):
        return self.final["tasks"].get(task_id)

    def tasks(self):
        return self.final["tasks"]

    def sends_by(self, sender=AGENT_UNDER_TEST):
        """Agent -> agent sends, reconstructed from recipient inbox rows."""
        out = []
        for recipient, rows in sorted(self.final["inbox"].items()):
            for row in rows:
                if sender_name(row, self.final["uuid_to_name"]) == sender:
                    out.append((recipient, row))
        out.sort(key=lambda item: item[1].get("timestamp") or "")
        return out

    def calls(self, surface=None, tool=None):
        return [c for c in self.tool_calls
                if (surface is None or c["surface"] == surface)
                and (tool is None or c["tool"] == tool)]

    def final_assistant_text(self):
        chunks = []
        for event in self.events:
            message = event.get("message") if isinstance(event, dict) else None
            content = message.get("content") if isinstance(message, dict) else None
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "text":
                        chunks.append(block.get("text") or "")
            result = event.get("result") if isinstance(event, dict) else None
            if isinstance(result, str):
                chunks.append(result)
        return "\n".join(chunks)


# ---------------------------------------------------------------------------
# generic checks
# ---------------------------------------------------------------------------

SEND_TOOLS = {"send", "reply"}
READ_TOOLS = {"inbox"}


def check_mixing(ctx):
    if ctx.arm == "mcp" and any(c["surface"] == "cli" for c in ctx.tool_calls):
        return ["mixing"], ["mcp arm used the CLI surface"]
    if ctx.arm == "cli" and any(c["surface"] == "mcp" for c in ctx.tool_calls):
        return ["mixing"], ["cli arm used an mcp__ tool"]
    return [], []


def check_duplicate_side_effect(ctx):
    notes = []

    # 1. identical (recipient, body) delivered twice — the durable evidence of a
    #    duplicate send, because sent_ledger.jsonl only records channel replies.
    seen = collections.Counter()
    for recipient, row in ctx.sends_by(AGENT_UNDER_TEST):
        seen[(recipient, row.get("text"))] += 1
    for (recipient, _text), count in sorted(seen.items(), key=lambda kv: str(kv[0])):
        if count > 1:
            notes.append("duplicate send to %s (%d copies)" % (recipient, count))

    # 2. sent_ledger duplicates, when the run did produce channel replies.
    ledger = collections.Counter()
    for row in ctx.final["sent_ledger"]:
        ledger[(row.get("agent"), row.get("excerpt"))] += 1
    for (agent, _excerpt), count in sorted(ledger.items(), key=lambda kv: str(kv[0])):
        if count > 1:
            notes.append("duplicate sent_ledger row for %s (%d copies)" % (agent, count))

    # 3. claim/done issued twice for one task by the agent under test.
    for task_id, task in sorted(ctx.final["tasks"].items()):
        pairs = list(zip(task["events"], task["actors"]))
        for kind in ("Claimed", "Done"):
            count = sum(1 for k, actor in pairs
                        if k == kind and actor == AGENT_UNDER_TEST)
            if count > 1:
                notes.append("%s issued %d times on %s" % (kind.lower(), count, task_id))

    # 4. identical send re-issued with no intervening inbox read.
    issued = set()
    for call in ctx.tool_calls:
        if call["tool"] in READ_TOOLS:
            issued.clear()
            continue
        if call["tool"] not in SEND_TOOLS or not isinstance(call["args"], dict):
            continue
        key = (call["args"].get("instance") or call["args"].get("to")
               or call["args"].get("team"), call["args"].get("message"))
        if key in issued:
            notes.append("resent %r with no intervening read" % (key[0],))
        # A send whose observable outcome was an error (CLI exit 1/2/3, MCP
        # isError / error result) produced no side effect; re-issuing it is
        # correct behaviour, not a duplicate. Only ok/indeterminate outcomes
        # (and unknown ones, conservatively) arm the resend check.
        if call.get("outcome") != "error":
            issued.add(key)

    if notes:
        return ["duplicate_side_effect"], notes
    return [], []


def generic_checks(ctx):
    critical_hits = []
    notes = []
    for check in (check_mixing, check_duplicate_side_effect):
        hits, why = check(ctx)
        critical_hits.extend(hits)
        notes.extend(why)
    return critical_hits, notes


# ---------------------------------------------------------------------------
# run grading
# ---------------------------------------------------------------------------

def load_expect(scenarios_dir, scenario):
    """Import scenarios/<id>/expect.py, injecting Verdict/critical helpers."""
    if not scenarios_dir or not scenario:
        return None
    path = os.path.join(scenarios_dir, scenario, "expect.py")
    if not os.path.exists(path):
        return None
    spec = importlib.util.spec_from_file_location("expect_%s" % scenario, path)
    module = importlib.util.module_from_spec(spec)
    module.__dict__["Verdict"] = Verdict
    module.__dict__["critical"] = critical
    module.__dict__["AGENT_UNDER_TEST"] = AGENT_UNDER_TEST
    spec.loader.exec_module(module)
    return module


def detect_invalid(meta, run_dir, expect_module, expect_missing, scenarios_dir=None,
                   events=None):
    # SPEC.txt:68 types this null|string. Anything else used to become the run's
    # own account of why it was excluded (#3412 A\u2374 review).
    reason = meta.get("invalid_reason")
    if reason is not None and not isinstance(reason, str):
        return "metadata_incomplete"
    if reason:
        return reason
    if meta.get("timed_out"):
        return "timed_out"
    # An absent or empty field used to read as agreement, so a run whose model was
    # never resolved counted as a clean run of this experiment (#3412 r2 review).
    requested = meta.get("model_requested")
    resolved = meta.get("model_resolved")
    if not requested or not resolved:
        return "model_missing"
    if requested != resolved:
        return "model_mismatch"
    if resolved != FROZEN_MODEL:
        return "model_not_frozen"
    arm = meta.get("arm")
    if arm not in ARMS:
        return "bad_arm"
    for field, ok in REQUIRED_META:
        if field not in meta or not ok(meta[field]):
            return "metadata_incomplete"
    if not os.path.exists(os.path.join(run_dir, "stream.jsonl")):
        return "missing_stream"
    # SPEC section 3: the model the STREAM resolved must equal MODEL. Comparing
    # metadata to metadata only proves the runner was self-consistent.
    stream_model = stream_init_model(events)
    if stream_model is None:
        return "stream_model_missing"
    if stream_model != resolved:
        return "stream_model_mismatch"
    if expect_missing:
        return "missing_expect"
    declared = declared_arms(meta.get("scenario"), scenarios_dir)
    if declared is None:
        return "scenario_declaration_invalid"
    if arm not in declared:
        return "arm_not_declared"
    cell = (meta.get("scenario"), meta.get("pair"), arm)
    if cell in FROZEN_PLAN_CELLS and meta.get("order_in_pair") != frozen_order(cell[1], arm):
        return "order_in_pair_mismatch"
    return None


def stream_init_model(events):
    """The model named by the stream's `system/init` event, or None."""
    for event in events or []:
        if not isinstance(event, dict):
            continue
        if event.get("type") == "system" and event.get("subtype") == "init":
            model = event.get("model")
            return model if _is_str(model) else None
    return None


def grade_run(run_dir, scenarios_dir=None):
    meta_path = os.path.join(run_dir, "metadata.json")
    with open(meta_path, "r", encoding="utf-8") as fh:
        meta = json.load(fh)

    events = _read_jsonl(os.path.join(run_dir, "stream.jsonl"))
    tool_calls = normalise_tool_calls(events)
    final = load_final_state(os.path.join(run_dir, "final_state"))
    seed = {}
    seed_path = os.path.join(run_dir, "seed.json")
    if os.path.exists(seed_path):
        try:
            with open(seed_path, "r", encoding="utf-8") as fh:
                seed = json.load(fh)
        except ValueError:
            seed = {}

    scenario = meta.get("scenario")
    expect_module = load_expect(scenarios_dir, scenario)
    expect_missing = expect_module is None

    result = {
        "passed": False,
        "critical": [],
        "notes": [],
        "tool_calls": tool_calls,
        "scenario": scenario,
        "arm": meta.get("arm"),
        "pair": meta.get("pair"),
        "invalid": False,
        "invalid_reason": None,
        "identity": {field: meta.get(field) for field in IDENTITY_FIELDS},
        "run_dir": os.path.abspath(run_dir),
    }

    reason = detect_invalid(meta, run_dir, expect_module, expect_missing, scenarios_dir,
                            events)
    if reason:
        result["invalid"] = True
        result["invalid_reason"] = reason
        result["notes"].append("run excluded: %s" % reason)
        return result

    ctx = Ctx(run_dir, meta, events, tool_calls, final, seed)
    hits, notes = generic_checks(ctx)
    result["critical"].extend(hits)
    result["notes"].extend(notes)

    try:
        verdict = expect_module.grade(ctx)
    except Exception as exc:  # a broken expect.py invalidates the run, never crashes the matrix
        result["invalid"] = True
        result["invalid_reason"] = "expect_error"
        result["notes"].append("expect.py raised %s: %s" % (type(exc).__name__, exc))
        return result

    if not _is_verdict(verdict):
        result["invalid"] = True
        result["invalid_reason"] = "expect_bad_verdict"
        result["notes"].append("expect.py returned %r" % (type(verdict).__name__,))
        return result

    unknown = [n for n in verdict.critical if n not in CRITICAL_CLASSES]
    if unknown:
        result["invalid"] = True
        result["invalid_reason"] = "expect_bad_verdict"
        result["notes"].append("expect.py emitted non-taxonomy classes %r" % (unknown,))
        return result

    result["passed"] = bool(verdict.passed) and not result["critical"]
    for name in verdict.critical:
        if name not in result["critical"]:
            result["critical"].append(name)
    result["notes"].extend(verdict.notes)
    if result["critical"]:
        result["passed"] = False
    return result


# ---------------------------------------------------------------------------
# aggregation
# ---------------------------------------------------------------------------

def find_run_dirs(runs_dir):
    found = []
    for dirpath, dirnames, filenames in os.walk(runs_dir):
        if "metadata.json" in filenames:
            found.append(dirpath)
            dirnames[:] = []
    return sorted(found)


def declared_arms(scenario, scenarios_dir):
    """The arms a scenario declares, or None when the declaration is unusable.

    Absent, malformed, non-object, or declaring no usable arm all return None —
    a declaration that cannot be read is not permission to skip the check
    (#3412 A\u2034 review).
    """
    meta_path = os.path.join(scenarios_dir or "", scenario or "", "meta.json")
    if not (scenario and scenarios_dir and os.path.exists(meta_path)):
        return None
    try:
        with open(meta_path, "r", encoding="utf-8") as fh:
            declared = json.load(fh)
    except ValueError:
        return None
    if not isinstance(declared, dict):
        return None
    arms = declared.get("arms")
    if not isinstance(arms, list) or not arms or not all(a in ARMS for a in arms):
        return None
    return arms


def classify_scenario(scenario, scenarios_dir):
    arms = declared_arms(scenario, scenarios_dir)
    if arms:
        return "confirmation" if len(arms) > 1 else "mixing"
    if scenario in CONFIRMATION_SCENARIOS:
        return "confirmation"
    if scenario in MIXING_SCENARIOS:
        return "mixing"
    return "unclassified"


def lookup_rate_gate(n, b, c, margin=0.10):
    """The frozen table AT N=60 is the gate; anything else recomputes and FAILS.

    SPEC section 9 pins the gate to "acceptance_table lookup for (b, c) at N=60".
    Reaching the recomputation means the frozen lookup did not decide — a
    deviating N, a missing cell, a missing table — so the interval is computed
    for the record, reported, and cannot grant a pass (#3412 review F3).
    """
    flags = []
    if n == TARGET_N and os.path.exists(TABLE_PATH):
        with open(TABLE_PATH, "r", encoding="utf-8") as fh:
            table = json.load(fh)
        cell = table["cells"].get("%d,%d" % (b, c))
        if cell is not None:
            return {"pass": bool(cell["accept"]), "ucb": cell["ucb"], "n": n,
                    "b": b, "c": c, "margin": table["margin"],
                    "source": "frozen_table", "flags": flags}
        flags.append("cell_missing_from_frozen_table")
    if n != TARGET_N:
        flags.append("n_deviates_from_frozen_table")
    if n <= 0:
        return {"pass": False, "ucb": None, "n": n, "b": b, "c": c,
                "margin": margin, "source": "no_pairs",
                "flags": flags + ["no_valid_pairs"]}
    import tango  # local import: only the off-table path needs the statistics
    ucb = tango.upper_bound(n, b, c)
    return {"pass": False, "ucb": ucb, "n": n, "b": b, "c": c,
            "margin": margin, "source": "tango_runtime", "flags": flags}


def aggregate(runs_dir, scenarios_dir=None):
    graded = []
    for run_dir in find_run_dirs(runs_dir):
        graded.append(grade_run(run_dir, scenarios_dir))

    # Two runs claiming one (scenario, pair, arm) used to collapse into whichever
    # arrived last. Which one is the real run is not the grader's to guess, so
    # both go (#3412 r2 review).
    # Every discovered run occupies its cell, whatever its verdict: a second copy
    # marked invalid is still a second copy (#3412 A\u2374 review).
    cells_seen = collections.Counter(
        (g["scenario"], g["pair"], g["arm"]) for g in graded)
    duplicate_cells = sorted((cell for cell, count in cells_seen.items() if count > 1),
                             key=str)
    for g in graded:
        if not g["invalid"] and cells_seen[(g["scenario"], g["pair"], g["arm"])] > 1:
            g["invalid"] = True
            g["invalid_reason"] = "duplicate_cell"
            g["notes"].append("run excluded: duplicate_cell")

    invalid = [{"run_dir": g["run_dir"], "scenario": g["scenario"], "arm": g["arm"],
                "pair": g["pair"], "reason": g["invalid_reason"]}
               for g in graded if g["invalid"]]
    valid = [g for g in graded if not g["invalid"]]

    by_key = {}
    for g in valid:
        by_key[(g["scenario"], g["pair"], g["arm"])] = g

    pairs = []
    cells = collections.Counter()
    for (scenario, pair, arm), g in sorted(by_key.items(), key=lambda kv: str(kv[0])):
        if arm != "mcp" or classify_scenario(scenario, scenarios_dir) != "confirmation":
            continue
        cli = by_key.get((scenario, pair, "cli"))
        if cli is None:
            continue
        mcp_fail = not g["passed"]
        cli_fail = not cli["passed"]
        if mcp_fail and cli_fail:
            cell = "both_fail"
        elif cli_fail:
            cell = "cli_only_fail"
        elif mcp_fail:
            cell = "mcp_only_fail"
        else:
            cell = "both_pass"
        cells[cell] += 1
        pairs.append({"scenario": scenario, "pair": pair, "cell": cell,
                      "mcp_passed": g["passed"], "cli_passed": cli["passed"]})

    n = len(pairs)
    b = cells["cli_only_fail"]
    c = cells["mcp_only_fail"]
    delta_hat = (b - c) / n if n else None
    rate_gate = lookup_rate_gate(n, b, c)

    critical_counts = collections.Counter()
    critical_by_scenario_arm = collections.defaultdict(collections.Counter)
    for g in valid:
        for name in g["critical"]:
            critical_counts[name] += 1
            critical_by_scenario_arm["%s/%s" % (g["scenario"], g["arm"])][name] += 1
    critical_total = sum(critical_counts.values())
    critical_gate = {"pass": critical_total == 0, "total": critical_total,
                     "by_class": dict(sorted(critical_counts.items())),
                     "by_scenario_arm": {k: dict(sorted(v.items()))
                                         for k, v in sorted(critical_by_scenario_arm.items())}}

    # The gates above count what is on disk. This one asks whether what is on
    # disk IS the plan SPEC section 6 froze — every cell, once, and nothing else
    # (#3412 A\u2034 review). Invalid runs still occupy their cell: a run that
    # happened and was refused is not a missing run.
    observed = [(g["scenario"], g["pair"], g["arm"]) for g in graded]
    observed_cells = set(observed)
    plan_flags = []
    manifest_path = os.path.join(runs_dir, "manifest.json")
    manifest = None
    if not os.path.exists(manifest_path):
        plan_flags.append("manifest_missing")
    else:
        try:
            with open(manifest_path, "r", encoding="utf-8") as fh:
                manifest = json.load(fh)
        except ValueError:
            manifest = None
        if not isinstance(manifest, dict) or any(f not in manifest for f in MANIFEST_FIELDS):
            plan_flags.append("manifest_incomplete")
            manifest = None
    if manifest is not None:
        # The rows are compared RAW: a junk row used to be filtered out before the
        # comparison, so 210 good rows plus one were still 210 (#3412 A\u2075 review).
        expected = [{"scenario": scenario, "pair": pair, "arm": arm,
                     "order_in_pair": frozen_order(pair, arm),
                     "dir": "%s/pair-%02d/%s" % (scenario, pair, arm)}
                    for (scenario, pair, arm) in FROZEN_PLAN]
        if manifest.get("plan") != expected:
            plan_flags.append("manifest_plan_mismatch")
        # Present is not the same as right.
        if (manifest.get("model") != FROZEN_MODEL
                or not _is_str(manifest.get("git_head"))
                or not isinstance(manifest.get("binary_sha256"), dict)
                or not isinstance(manifest.get("prompt_sha256"), dict)
                or manifest.get("total_runs") != len(FROZEN_PLAN)):
            plan_flags.append("manifest_identity_invalid")
        # Every run must be a run OF the experiment the manifest describes.
        if any(g["identity"].get("git_head") != manifest.get("git_head")
               or g["identity"].get("binary_sha256") != manifest.get("binary_sha256")
               or g["identity"].get("model_requested") != manifest.get("model")
               or g["identity"].get("model_resolved") != manifest.get("model")
               for g in valid):
            plan_flags.append("manifest_identity_mismatch")

    # One experiment, not several: fleet and CLI version are matrix-wide, seed and
    # prompt are per scenario, the system prompt is per arm. A split in any of
    # them means the runs came from more than one setup.
    scoped = collections.defaultdict(set)
    for g in valid:
        identity = g["identity"]
        for field in ("fleet_sha256", "claude_version"):
            scoped[("matrix", field)].add(json.dumps(identity.get(field)))
        for field in ("seed_sha256", "prompt_sha256"):
            scoped[(g["scenario"], field)].add(json.dumps(identity.get(field)))
        scoped[((g["scenario"], g["arm"]), "system_prompt_sha256")].add(
            json.dumps(identity.get("system_prompt_sha256")))
    if any(len(values) > 1 for values in scoped.values()):
        plan_flags.append("run_identity_split")
    missing_cells = sorted(FROZEN_PLAN_CELLS - observed_cells, key=str)
    unexpected_cells = sorted(observed_cells - FROZEN_PLAN_CELLS, key=str)
    plan_gate = {
        "pass": not (missing_cells or unexpected_cells or duplicate_cells or plan_flags)
                and len(observed) == len(FROZEN_PLAN),
        "expected_runs": len(FROZEN_PLAN),
        "observed_runs": len(observed),
        "missing": [list(cell) for cell in missing_cells],
        "unexpected": [list(cell) for cell in unexpected_cells],
        "duplicates": [list(cell) for cell in duplicate_cells],
        "flags": plan_flags,
    }

    mixing_gate = {"pass": True, "expected_runs_per_scenario": MIXING_DENOMINATOR,
                   "scenarios": {}}
    for scenario in MIXING_SCENARIOS:
        runs = [g for g in valid if g["scenario"] == scenario]
        hits = sum(1 for g in runs if "mixing" in g["critical"])
        mixing_gate["scenarios"][scenario] = {
            "mixing": hits, "valid_runs": len(runs),
            "expected_runs": MIXING_DENOMINATOR,
            "runs_shortfall": max(0, MIXING_DENOMINATOR - len(runs)),
        }
        # A control that did not run cannot have produced "0 of 45". The
        # shortfall was already measured here and only reported; refusing on it
        # is what makes the denominator mean anything (#3412 review F1).
        if hits or len(runs) < MIXING_DENOMINATOR:
            mixing_gate["pass"] = False
    # mixing is also a critical class, so any mixing anywhere already fails
    # critical_gate; the dedicated gate keeps the 0/45 + 0/45 denominators visible.

    per_scenario = collections.defaultdict(lambda: collections.defaultdict(
        lambda: {"runs": 0, "passed": 0, "failed": 0, "critical": 0}))
    per_arm = {"mcp": {"runs": 0, "failed": 0, "tool_calls": 0},
               "cli": {"runs": 0, "failed": 0, "tool_calls": 0}}
    for g in valid:
        bucket = per_scenario[g["scenario"]][g["arm"]]
        bucket["runs"] += 1
        bucket["passed" if g["passed"] else "failed"] += 1
        bucket["critical"] += len(g["critical"])
        arm = per_arm.get(g["arm"])
        if arm is not None:
            arm["runs"] += 1
            arm["failed"] += 0 if g["passed"] else 1
            arm["tool_calls"] += len(g["tool_calls"])
    mean_tool_calls = {
        arm: (data["tool_calls"] / data["runs"] if data["runs"] else None)
        for arm, data in per_arm.items()
    }

    pilot_safety = bool(rate_gate["pass"] and critical_gate["pass"]
                        and mixing_gate["pass"] and plan_gate["pass"])

    return {
        "schema": 1,
        "runs_dir": os.path.abspath(runs_dir),
        "delta_definition": DELTA_DEFINITION,
        "total_runs": len(graded),
        "valid_runs": len(valid),
        "pairs": pairs,
        "cells": dict(sorted(cells.items())),
        "n": n,
        "b": b,
        "c": c,
        "delta_hat": delta_hat,
        "rate_gate": rate_gate,
        "critical_gate": critical_gate,
        "mixing_gate": mixing_gate,
        "plan_gate": plan_gate,
        "pilot_safety": pilot_safety,
        "per_scenario": {s: {a: dict(v) for a, v in sorted(arms.items())}
                         for s, arms in sorted(per_scenario.items())},
        "per_arm": {a: dict(v) for a, v in sorted(per_arm.items())},
        "mean_tool_calls": mean_tool_calls,
        "invalid": invalid,
    }


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main(argv=None):
    ap = argparse.ArgumentParser(description="grade tool-cli Phase 0b runs")
    ap.add_argument("run_dir", nargs="?", help="one run directory -> grade.json")
    ap.add_argument("--aggregate", metavar="RUNS_DIR", help="aggregate a matrix -> summary.json")
    ap.add_argument("--scenarios", default=os.path.join(HERE, "scenarios"),
                    help="scenarios/ directory (default: alongside this script)")
    ap.add_argument("--out", help="output path (default: grade.json / summary.json in place)")
    args = ap.parse_args(argv)

    if args.aggregate:
        summary = aggregate(args.aggregate, args.scenarios)
        out = args.out or os.path.join(args.aggregate, "summary.json")
        with open(out, "w", encoding="utf-8") as fh:
            json.dump(summary, fh, indent=2, sort_keys=True)
            fh.write("\n")
        print("wrote %s (N=%d b=%d c=%d pilot_safety=%s)"
              % (out, summary["n"], summary["b"], summary["c"], summary["pilot_safety"]))
        return 0

    if not args.run_dir:
        ap.print_help()
        return 2
    result = grade_run(args.run_dir, args.scenarios)
    out = args.out or os.path.join(args.run_dir, "grade.json")
    with open(out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print("wrote %s (passed=%s critical=%s invalid=%s)"
          % (out, result["passed"], result["critical"], result["invalid_reason"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
