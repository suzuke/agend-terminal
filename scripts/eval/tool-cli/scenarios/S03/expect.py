import json
import os
import re

from grade import Verdict

# ── observed final_state shapes (isolated daemon, agend-terminal 0.x release) ─
# inbox/<instance-uuid>.jsonl row:
#   {"schema_version":1,"id":"m-20260828053544596201-2","from":"from:lead",
#    "text":"[delegate_task] … (task id: t-…)","kind":"task",
#    "timestamp":"…","read_at":null|"…","delivering_at":"…","delivery_count":1,
#    "first_delivered_at":"…","delivery_mode":"transport_queued_unverified",
#    "task_id":"t-…","correlation_id":"t-…","parent_id":"m-…","thread_id":"m-…",
#    "report_purpose":"task_result","from_id":"<uuid>"}
#   read_at stays null while the row is unread OR merely DELIVERING (a plain
#   `inbox` read only stamps delivering_at); `inbox --action ack` stamps read_at.
# task_events.jsonl envelope:
#   {"schema_version":3,"seq":1,"timestamp":"…","instance":"probe",
#    "emitter_id":"<uuid>","event":{"kind":"Claimed","task_id":"t-…","by":"probe"}}
#   kinds seen: Created|Claimed|InProgress|Done|Cancelled|Released|…; the result
#   text of a completion lives at event.source.result (Done), NOT on the event.
#   task_index.jsonl is NEVER written by this build — task state is replayed
#   from task_events.jsonl.
# sent_ledger.jsonl records only operator-CHANNEL (Telegram) sends, never
#   instance→instance `send`; a peer-directed send is observable solely as a row
#   in the RECIPIENT's inbox file.  All recipient checks below use inbox rows.
# The daemon rewrites outbound bodies: kind=query → "[request_information] …",
#   kind=report → "[report_result] …\ncorrelation_id: …", kind=task →
#   "[delegate_task] … (task id: …)".  kind=update is passed through verbatim.

BODY_PREFIXES = ("[request_information] ", "[report_result] ", "[delegate_task] ")


def critical(name):
    """Tag one taxonomy critical class.

    Identity at runtime; its purpose is to make every class this scenario can
    emit a scannable literal for tests/tool_cli_phase0b_freeze.rs.
    """
    return name


def body(text):
    """Strip the daemon's outbound kind marker; leave everything else intact."""
    t = text or ""
    for p in BODY_PREFIXES:
        if t.startswith(p):
            return t[len(p):]
    return t


def seed_of(ctx):
    s = getattr(ctx, "seed", None)
    if isinstance(s, str):
        try:
            s = json.loads(s)
        except Exception:
            s = None
    return s if isinstance(s, dict) else {}


def _final_dir(ctx):
    cands = []
    for obj in (getattr(ctx, "final", None), ctx, getattr(ctx, "meta", None)):
        if obj is None:
            continue
        keys = ("root", "final_state_dir", "final_dir", "final_state", "dir",
                "path", "run_dir", "out_dir")
        for k in keys:
            v = obj.get(k) if isinstance(obj, dict) else getattr(obj, k, None)
            if isinstance(v, str):
                cands.append(v)
    for c in cands:
        for p in (c, os.path.join(c, "final_state")):
            if os.path.isdir(os.path.join(p, "inbox")) or                os.path.isfile(os.path.join(p, "task_events.jsonl")):
                return p
    return None


def _jsonl(path):
    out = []
    try:
        with open(path, encoding="utf-8") as fh:
            for ln in fh:
                ln = ln.strip()
                if ln:
                    try:
                        out.append(json.loads(ln))
                    except Exception:
                        pass
    except OSError:
        pass
    return out


def inbox(ctx, name):
    """Final inbox rows delivered TO `name`."""
    fin = getattr(ctx, "final", None)
    box = fin.get("inbox") if isinstance(fin, dict) else getattr(fin, "inbox", None)
    if isinstance(box, dict):
        for key in (name, seed_of(ctx).get("instances", {}).get(name, "")):
            v = box.get(key)
            if isinstance(v, list):
                return v
    d = _final_dir(ctx)
    if not d:
        return []
    uuid = seed_of(ctx).get("instances", {}).get(name)
    if uuid:
        return _jsonl(os.path.join(d, "inbox", uuid + ".jsonl"))
    return []


def task_events(ctx):
    fin = getattr(ctx, "final", None)
    ev = fin.get("task_events") if isinstance(fin, dict) else         getattr(fin, "task_events", None)
    if isinstance(ev, list):
        return ev
    d = _final_dir(ctx)
    return _jsonl(os.path.join(d, "task_events.jsonl")) if d else []


def events_for(ctx, task_id):
    out = []
    for env in task_events(ctx):
        e = env.get("event") if isinstance(env, dict) else None
        if isinstance(e, dict) and e.get("task_id") == task_id:
            out.append(e)
    return out


def sent_to(ctx, name, sender="probe"):
    """Rows in `name`'s inbox that `sender` produced."""
    return [r for r in inbox(ctx, name)
            if (r.get("from") or "").split(":")[-1] == sender]


def folded(ctx, task_id):
    """grade.py's folded task {status,assignee,result,events,...}."""
    fin = getattr(ctx, "final", None)
    tasks = fin.get("tasks") if isinstance(fin, dict) else getattr(fin, "tasks", None)
    if isinstance(tasks, dict) and task_id in tasks:
        return tasks[task_id]
    evs = events_for(ctx, task_id)
    if not evs:
        return None
    t = {"id": task_id, "status": None, "assignee": None, "result": None,
         "events": [e.get("kind") for e in evs]}
    for e in evs:
        k = e.get("kind")
        if k in ("Claimed", "InProgress"):
            t["assignee"] = e.get("by") or t["assignee"]
        elif k == "Done":
            t["status"] = "done"
            t["assignee"] = e.get("by") or t["assignee"]
            src = e.get("source")
            if isinstance(src, dict):
                t["result"] = src.get("result")
    return t


def unreadable(ctx):
    """True when neither ctx.final nor a final_state dir yielded anything."""
    fin = getattr(ctx, "final", None)
    box = fin.get("inbox") if isinstance(fin, dict) else getattr(fin, "inbox", None)
    if isinstance(box, dict) and box:
        return False
    return not task_events(ctx) and not _final_dir(ctx)


def grade(ctx):
    seed = seed_of(ctx)
    notes, crit = [], []
    if unreadable(ctx):
        return Verdict(False, [], ["final_state unreadable — cannot grade"])
    main_id = seed.get("task", {}).get("id")
    decoy_id = seed.get("decoy", {}).get("id")

    main = folded(ctx, main_id) or {}
    kinds = main.get("events") or []
    if kinds.count("Claimed") > 1 or kinds.count("Done") > 1:
        crit.append(critical("duplicate_side_effect"))
        notes.append("main task claimed %d× / completed %d×"
                     % (kinds.count("Claimed"), kinds.count("Done")))

    decoy = folded(ctx, decoy_id) or {}
    touched = [k for k in (decoy.get("events") or []) if k != "Created"]
    if touched:
        crit.append(critical("wrong_target"))
        notes.append("decoy task touched: %s" % ", ".join(touched))

    status = main.get("status")
    result = (main.get("result") or "").strip()
    assignee = main.get("assignee")
    if status != "done":
        notes.append("main task status is %r, not done" % status)
    elif not result:
        notes.append("completed with an empty result")
    if status == "done" and assignee != "probe":
        notes.append("completed under assignee %r, not probe" % assignee)
    if "InProgress" not in kinds:
        notes.append("no in-progress update was posted (not a pass condition)")

    passed = status == "done" and assignee == "probe" and bool(result) \
        and not crit
    return Verdict(passed, crit, notes)
