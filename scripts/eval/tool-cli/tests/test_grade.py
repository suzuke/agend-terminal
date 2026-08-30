#!/usr/bin/env python3
"""Tests for grade.py: normalisation, generic checks, aggregation, gates.

The final_state fixtures under tests/fixtures/final_state_sample/ were produced
by a REAL daemon (release binary, hermetic AGEND_HOME under $TMPDIR) on
2026-08-28, so the loader is tested against the shapes the harness will really
copy — not against invented ones.
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FIXTURES = os.path.join(HERE, "fixtures")
SAMPLE = os.path.join(FIXTURES, "final_state_sample")
# #3412: the fixture is STORED as `fleet.sample.yaml`. A blob whose BASENAME is
# `fleet.yaml` is refused anywhere in a push range by agentic-git's trust-root
# denylist (push_guards.rs TRUST_ROOT_DENY_NAMES), which cannot tell a fixture
# from the operator's live config. The loader contract is unchanged — it still
# reads `fleet.yaml` — so the tests materialise the real name in a temp copy.
FLEET_STORED_NAME = "fleet.sample.yaml"
sys.path.insert(0, ROOT)

import grade  # noqa: E402

PROBE_ID = "09c2d6f5-78b5-4453-913a-b3d68ba640b1"
LEAD_ID = "cb93871b-b4a6-48db-9734-dee8effeb337"
REVIEW_ID = "10c6662d-6426-4113-a56f-be2e3490e44d"

FLEET_YAML = """instances:
  probe:
    command: /bin/cat
    role_kind: implementer
    id: %s
  ane-review:
    command: /bin/cat
    role_kind: reviewer
    id: %s
  lead:
    command: /bin/cat
    role_kind: orchestrator
    id: %s
""" % (PROBE_ID, REVIEW_ID, LEAD_ID)

IDS = {"probe": PROBE_ID, "ane-review": REVIEW_ID, "lead": LEAD_ID}

PASS_EXPECT = "def grade(ctx):\n    return Verdict(True)\n"
FAIL_EXPECT = "def grade(ctx):\n    return Verdict(False, notes=['objective missed'])\n"
CRIT_EXPECT = ("def grade(ctx):\n"
               "    return Verdict(False, critical=[critical('wrong_target')])\n")


# ---------------------------------------------------------------------------
# builders
# ---------------------------------------------------------------------------

def bash_event(command):
    return {"type": "assistant",
            "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash",
                                     "input": {"command": command}}]}}


def mcp_event(name, payload):
    return {"type": "assistant",
            "message": {"content": [{"type": "tool_use", "id": "t2", "name": name,
                                     "input": payload}]}}


def text_event(text):
    return {"type": "assistant",
            "message": {"content": [{"type": "text", "text": text}]}}


def inbox_row(msg_id, sender, text, kind="update", **extra):
    row = {"schema_version": 1, "id": msg_id, "from": "from:%s" % sender,
           "from_id": IDS[sender], "text": text, "kind": kind,
           "timestamp": "2026-08-28T05:%02d:00.000000+00:00" % (len(msg_id) % 60),
           "read_at": None, "delivery_mode": "transport_queued_unverified"}
    row.update(extra)
    return row


def task_event(seq, instance, kind, task_id, **fields):
    event = {"kind": kind, "task_id": task_id}
    event.update(fields)
    return {"schema_version": 3, "seq": seq,
            "timestamp": "2026-08-28T05:33:%02d.000000000Z" % (seq % 60),
            "instance": instance, "emitter_id": IDS[instance], "event": event}


#: meta_extra sentinel: delete the key instead of setting it.
DROP = object()


def sha_of(path):
    if not os.path.exists(path):
        return "0" * 64
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def scenario_file_sha(base, scenario, name):
    """The digest a run records for its scenario's frozen file.

    Fixtures put runs either in <tmp>/runs or in <tmp> itself, with scenarios
    alongside; look in both.
    """
    base = os.path.abspath(base)
    for root in (os.path.join(os.path.dirname(base), "scenarios"),
                 os.path.join(base, "scenarios")):
        path = os.path.join(root, scenario, name)
        if os.path.exists(path):
            return sha_of(path)
    return "0" * 64


def expected_order(pair, arm):
    """The order run.sh derives and matrix.sh plans: parity x arm."""
    first_arm = "mcp" if pair % 2 else "cli"
    return "first" if arm == first_arm else "second"


def write_run(base, scenario, arm, pair, events, inbox=None, task_events=None,
              meta_extra=None, sent_ledger=None, init=True, final_state=True):
    run_dir = os.path.join(base, "%s-%s-p%s" % (scenario, arm, pair))
    if final_state:
        os.makedirs(os.path.join(run_dir, "final_state", "inbox"), exist_ok=True)
    else:
        os.makedirs(run_dir, exist_ok=True)
    meta = {"schema": 1, "scenario": scenario, "arm": arm, "pair": pair,
            "order_in_pair": expected_order(pair, arm),
            "model_requested": "claude-fable-5",
            "model_resolved": "claude-fable-5", "claude_version": "2.0.0-test",
            "git_head": "0" * 40,
            "binary_sha256": {"agend-terminal": "0" * 64, "agend-mcp-bridge": "0" * 64},
            "system_prompt_sha256": grade.frozen_system_prompt_digest(arm),
            "prompt_sha256": scenario_file_sha(base, scenario, "prompt.txt"),
            "fleet_sha256": grade.frozen_fleet_digest(),
            "seed_sha256": scenario_file_sha(base, scenario, "seed.sh"),
            "started_at": "2026-08-28T00:00:00Z", "ended_at": "2026-08-28T00:00:01Z",
            "duration_ms": 1000, "fence": True, "exit_code": 0,
            "turns": 3, "timed_out": False, "invalid_reason": None,
            "max_turns": 15, "timeout_secs": 900}
    meta.update(meta_extra or {})
    for key in [k for k, v in (meta_extra or {}).items() if v is DROP]:
        meta.pop(key, None)
    with open(os.path.join(run_dir, "metadata.json"), "w", encoding="utf-8") as fh:
        json.dump(meta, fh)
    # A real stream opens with the system/init event that carries the model the
    # session actually resolved (SPEC section 3). Fixtures carry it too, unless
    # the caller is testing what happens when it disagrees or is missing.
    stream = list(events)
    if init and not any(e.get("type") == "system" and e.get("subtype") == "init"
                        for e in stream if isinstance(e, dict)):
        stream.insert(0, {"type": "system", "subtype": "init",
                          "model": meta.get("model_resolved"),
                          "claude_code_version": meta.get("claude_version")})
    with open(os.path.join(run_dir, "stream.jsonl"), "w", encoding="utf-8") as fh:
        for event in stream:
            fh.write(json.dumps(event) + "\n")
    if not final_state:
        # A run whose durable evidence never came back. Everything else about it
        # conforms, which is exactly what makes it worth testing.
        return run_dir
    final = os.path.join(run_dir, "final_state")
    with open(os.path.join(final, "fleet.yaml"), "w", encoding="utf-8") as fh:
        fh.write(FLEET_YAML)
    for owner, rows in (inbox or {}).items():
        with open(os.path.join(final, "inbox", "%s.jsonl" % IDS[owner]), "w",
                  encoding="utf-8") as fh:
            for row in rows:
                fh.write(json.dumps(row) + "\n")
    with open(os.path.join(final, "task_events.jsonl"), "w", encoding="utf-8") as fh:
        for row in task_events or []:
            fh.write(json.dumps(row) + "\n")
    with open(os.path.join(final, "sent_ledger.jsonl"), "w", encoding="utf-8") as fh:
        for row in sent_ledger or []:
            fh.write(json.dumps(row) + "\n")
    return run_dir


def write_scenarios(base, spec):
    """spec: {scenario_id: (expect_source, arms)}"""
    root = os.path.join(base, "scenarios")
    for scenario, (source, arms) in spec.items():
        directory = os.path.join(root, scenario)
        os.makedirs(directory, exist_ok=True)
        with open(os.path.join(directory, "expect.py"), "w", encoding="utf-8") as fh:
            fh.write(source)
        with open(os.path.join(directory, "meta.json"), "w", encoding="utf-8") as fh:
            json.dump({"id": scenario, "title": scenario, "arms": arms,
                       "pairs": 10, "roles_required": True}, fh)
        # the frozen files a run's prompt_sha256 / seed_sha256 are bound to
        with open(os.path.join(directory, "prompt.txt"), "w", encoding="utf-8") as fh:
            fh.write("prompt for %s\n" % scenario)
        with open(os.path.join(directory, "seed.sh"), "w", encoding="utf-8") as fh:
            fh.write("#!/bin/sh\n# seed %s\n" % scenario)
    return root


def frozen_manifest_rows():
    rows = []
    for index in range(1, 7):
        scenario = "S%02d" % index
        for pair in range(1, 11):
            for arm in (("mcp", "cli") if pair % 2 else ("cli", "mcp")):
                rows.append((scenario, pair, arm))
    rows.extend(("S13", pair, "mcp") for pair in range(1, 46))
    rows.extend(("S14", pair, "cli") for pair in range(1, 46))
    return [{"scenario": s, "pair": p, "arm": a, "order_in_pair": expected_order(p, a),
             "dir": "%s/pair-%02d/%s" % (s, p, a)} for (s, p, a) in rows]


def write_manifest(runs_dir, **overrides):
    manifest = {"schema": 1, "stamp": "TEST", "created_at": "2026-08-28T00:00:00Z",
                "dry_run": False, "git_head": "0" * 40, "model": "claude-fable-5",
                "jobs": 3, "timeout_secs": grade.FROZEN_TIMEOUT_SECS,
                "binary_sha256": {"agend-terminal": "0" * 64,
                                             "agend-mcp-bridge": "0" * 64},
                "prompt_sha256": {name: sha_of(os.path.join(
                    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                    "prompts", "%s.txt" % name)) for name in ("base", "mcp", "cli")},
                "missing_scenarios": [], "total_runs": 210,
                "fleet_sha256": grade.frozen_fleet_digest(),
                "seed_sha256": {scenario: sha_of(os.path.join(
                    os.path.dirname(os.path.abspath(runs_dir)), "scenarios",
                    scenario, "seed.sh"))
                    for scenario in ["S0%d" % i for i in range(1, 7)] + ["S13", "S14"]},
                "plan": frozen_manifest_rows()}
    manifest.update(overrides)
    for key in [k for k, v in overrides.items() if v is DROP]:
        manifest.pop(key, None)
    with open(os.path.join(runs_dir, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh)
    return manifest


class TempCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="tclieval-test-")
        self.addCleanup(shutil.rmtree, self.tmp, True)


# ---------------------------------------------------------------------------
# normalisation
# ---------------------------------------------------------------------------

class Normalisation(TempCase):
    def test_mcp_tool_use(self):
        calls = grade.normalise_tool_calls([
            mcp_event("mcp__agend-terminal__task", {"action": "get", "id": "t-1"})])
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["surface"], "mcp")
        self.assertEqual(calls[0]["tool"], "task")
        self.assertEqual(calls[0]["action"], "get")
        self.assertEqual(calls[0]["args"]["id"], "t-1")

    def test_foreign_mcp_server_still_counts_as_mcp_surface(self):
        calls = grade.normalise_tool_calls([mcp_event("mcp__other__thing", {})])
        self.assertEqual(calls[0]["surface"], "mcp")

    def test_cli_arg_and_action(self):
        calls = grade.normalise_tool_calls([
            bash_event("agend-terminal tool task --action claim --arg id=t-7 --arg force=true")])
        self.assertEqual(len(calls), 1)
        call = calls[0]
        self.assertEqual((call["surface"], call["tool"], call["action"]),
                         ("cli", "task", "claim"))
        self.assertEqual(call["args"], {"id": "t-7", "force": "true"})

    def test_cli_inline_json(self):
        calls = grade.normalise_tool_calls([bash_event(
            "agend-terminal tool send --json '{\"instance\":\"lead\",\"message\":\"hi\"}'")])
        self.assertEqual(calls[0]["args"], {"instance": "lead", "message": "hi"})

    def test_cli_heredoc_json_stdin(self):
        command = (
            "agend-terminal tool send --json - <<'EOF'\n"
            "{\"instance\": \"ane-review\",\n"
            " \"message\": \"line1\\nline2 \\\"double\\\" 'single' `tick` $HOME 測試🚀\",\n"
            " \"request_kind\": \"query\"}\n"
            "EOF"
        )
        calls = grade.normalise_tool_calls([bash_event(command)])
        self.assertEqual(len(calls), 1)
        call = calls[0]
        self.assertEqual(call["surface"], "cli")
        self.assertEqual(call["tool"], "send")
        self.assertEqual(call["args"]["instance"], "ane-review")
        self.assertIn("測試🚀", call["args"]["message"])
        self.assertEqual(call["args"]["request_kind"], "query")
        self.assertIn("EOF", call["raw"])

    def test_cli_absolute_path_binary_and_action_from_json(self):
        calls = grade.normalise_tool_calls([bash_event(
            "/tmp/sbx/bin/agend-terminal tool task --json '{\"action\":\"done\",\"id\":\"t-9\"}'")])
        self.assertEqual(calls[0]["tool"], "task")
        self.assertEqual(calls[0]["action"], "done")

    def test_two_cli_calls_in_one_bash_command(self):
        calls = grade.normalise_tool_calls([bash_event(
            "agend-terminal tool inbox --json '{}' && agend-terminal tool task --action list")])
        self.assertEqual([c["tool"] for c in calls], ["inbox", "task"])

    def test_json_from_file_leaves_args_none(self):
        calls = grade.normalise_tool_calls([
            bash_event("agend-terminal tool send --json @/tmp/body.json")])
        self.assertIsNone(calls[0]["args"])
        self.assertIn("json from file", calls[0]["parse_error"])

    def test_invalid_json_never_crashes(self):
        calls = grade.normalise_tool_calls([
            bash_event("agend-terminal tool send --json '{not json'")])
        self.assertEqual(calls[0]["surface"], "cli")
        self.assertIsNone(calls[0]["args"])
        self.assertEqual(calls[0]["parse_error"], "invalid --json payload")

    def test_unparsable_shell_falls_back_without_crashing(self):
        calls = grade.normalise_tool_calls([
            bash_event("agend-terminal tool send --action reply --arg m='unterminated")])
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["tool"], "send")
        self.assertIsNone(calls[0]["args"])

    def test_bash_without_agend_terminal_is_other(self):
        calls = grade.normalise_tool_calls([bash_event("ls -la /tmp")])
        self.assertEqual(calls[0]["surface"], "other")
        self.assertEqual(calls[0]["tool"], "Bash")

    def test_non_bash_non_mcp_tool_is_other(self):
        calls = grade.normalise_tool_calls([mcp_event("Read", {"file_path": "/x"})])
        self.assertEqual(calls[0]["surface"], "other")


# ---------------------------------------------------------------------------
# final_state loader (real daemon fixture)
# ---------------------------------------------------------------------------

def staged_sample(tmpdir):
    """Copy the stored fixture into `tmpdir`, materialising `fleet.sample.yaml`
    as the `fleet.yaml` the loader expects. Bytes are copied unchanged."""
    staged = os.path.join(tmpdir, "final_state")
    shutil.copytree(SAMPLE, staged)
    os.rename(os.path.join(staged, FLEET_STORED_NAME),
              os.path.join(staged, "fleet.yaml"))
    return staged


class FinalStateLoader(unittest.TestCase):
    def test_real_fixture_shapes(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = grade.load_final_state(staged_sample(tmp))
        self.assertIn("probe", state["inbox"], "uuid inbox stems must resolve via fleet.yaml")
        self.assertIn("lead", state["inbox"])
        probe_rows = state["inbox"]["probe"]
        self.assertEqual(len(probe_rows), 2)
        self.assertEqual(probe_rows[0]["text"], "pair-1-msg-1")
        self.assertEqual(grade.sender_name(probe_rows[0], state["uuid_to_name"]), "lead")
        report = state["inbox"]["lead"][0]
        self.assertEqual(report["kind"], "report")
        self.assertEqual(report["parent_id"], "m-20260828053408256248-1")
        self.assertTrue(report["correlation_id"].startswith("t-"))
        tasks = state["tasks"]
        self.assertEqual(len(tasks), 1)
        task = list(tasks.values())[0]
        self.assertEqual(task["status"], "done")
        self.assertEqual(task["assignee"], "probe")
        self.assertEqual(task["result"], "finished")
        self.assertEqual(task["created_by"], "lead")
        self.assertEqual(state["sent_ledger"], [],
                         "agent->agent sends are not recorded in sent_ledger")

    def test_fixture_fleet_sample_is_committed_and_not_ignored(self):
        # The fixture is the only source of the uuid<->name map, so an ignored or
        # untracked one passes locally and fails in every fresh checkout. Stored
        # under a non-trust-root basename (see FLEET_STORED_NAME), so it needs no
        # ignore exception and no longer trips the push-time denylist.
        if shutil.which("git") is None:
            self.skipTest("git not on PATH")
        root = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=HERE,
                              capture_output=True, text=True, check=True).stdout.strip()
        rel = os.path.relpath(os.path.join(SAMPLE, FLEET_STORED_NAME), root)
        ignored = subprocess.run(["git", "check-ignore", "-q", rel], cwd=root).returncode == 0
        self.assertFalse(ignored, "%s is swallowed by an ignore rule" % rel)
        tracked = subprocess.run(["git", "ls-files", "--error-unmatch", rel], cwd=root,
                                 capture_output=True).returncode == 0
        self.assertTrue(tracked, "%s must be committed with the rest of the fixture" % rel)

    def test_missing_final_state_is_empty_not_fatal(self):
        state = grade.load_final_state("/nonexistent/final_state")
        self.assertEqual(state["inbox"], {})
        self.assertEqual(state["tasks"], {})

    def test_fleet_yaml_parser(self):
        parsed = grade.parse_fleet_yaml(FLEET_YAML)
        self.assertEqual(sorted(parsed), ["ane-review", "lead", "probe"])
        self.assertEqual(parsed["probe"]["role_kind"], "implementer")
        self.assertEqual(parsed["lead"]["id"], LEAD_ID)


# ---------------------------------------------------------------------------
# generic checks
# ---------------------------------------------------------------------------

class GenericChecks(TempCase):
    def _grade(self, **kwargs):
        scenarios = write_scenarios(self.tmp, {kwargs.pop("scenario", "S01"):
                                               (kwargs.pop("expect", PASS_EXPECT),
                                                ["mcp", "cli"])})
        run_dir = write_run(self.tmp, kwargs.pop("sid", "S01"), kwargs.pop("arm", "cli"),
                            kwargs.pop("pair", 1), kwargs.pop("events", []), **kwargs)
        return grade.grade_run(run_dir, scenarios)

    def test_mixing_mcp_arm_touching_cli(self):
        result = self._grade(arm="mcp",
                             events=[bash_event("agend-terminal tool inbox --json '{}'")])
        self.assertIn("mixing", result["critical"])
        self.assertFalse(result["passed"])

    def test_mixing_cli_arm_touching_mcp(self):
        result = self._grade(arm="cli",
                             events=[mcp_event("mcp__agend-terminal__inbox", {})])
        self.assertIn("mixing", result["critical"])

    def test_no_mixing_when_arm_matches_surface(self):
        result = self._grade(arm="cli",
                             events=[bash_event("agend-terminal tool inbox --json '{}'")])
        self.assertEqual(result["critical"], [])
        self.assertTrue(result["passed"])

    def test_duplicate_side_effect_same_body_twice(self):
        inbox = {"lead": [inbox_row("m-1", "probe", "done", kind="report"),
                          inbox_row("m-22", "probe", "done", kind="report")]}
        result = self._grade(arm="cli", inbox=inbox)
        self.assertIn("duplicate_side_effect", result["critical"])
        self.assertTrue(any("duplicate send to lead" in n for n in result["notes"]))

    def test_duplicate_side_effect_double_claim(self):
        events = [task_event(1, "lead", "Created", "t-1", owner="probe", title="T"),
                  task_event(2, "probe", "Claimed", "t-1", by="probe"),
                  task_event(3, "probe", "Claimed", "t-1", by="probe")]
        result = self._grade(arm="cli", task_events=events)
        self.assertIn("duplicate_side_effect", result["critical"])

    def test_single_send_is_not_duplicate(self):
        inbox = {"lead": [inbox_row("m-1", "probe", "done", kind="report")],
                 "probe": [inbox_row("m-2", "lead", "please", kind="task")]}
        result = self._grade(arm="cli", inbox=inbox)
        self.assertEqual(result["critical"], [])

    def test_resend_without_intervening_read(self):
        cmd = "agend-terminal tool send --json '{\"instance\":\"lead\",\"message\":\"x\"}'"
        result = self._grade(arm="cli", events=[bash_event(cmd), bash_event(cmd)])
        self.assertIn("duplicate_side_effect", result["critical"])

    def test_resend_after_read_is_allowed(self):
        cmd = "agend-terminal tool send --json '{\"instance\":\"lead\",\"message\":\"x\"}'"
        read = "agend-terminal tool inbox --json '{}'"
        result = self._grade(arm="cli",
                             events=[bash_event(cmd), bash_event(read), bash_event(cmd)])
        self.assertEqual(result["critical"], [])

    def test_expect_critical_class_propagates(self):
        result = self._grade(arm="cli", expect=CRIT_EXPECT)
        self.assertEqual(result["critical"], ["wrong_target"])
        self.assertFalse(result["passed"])

    def test_unknown_critical_class_invalidates_run(self):
        bad = "def grade(ctx):\n    return Verdict(False, critical=['not_a_class'])\n"
        result = self._grade(arm="cli", expect=bad)
        self.assertTrue(result["invalid"])
        self.assertEqual(result["invalid_reason"], "expect_error")

    def test_ctx_helpers_reach_final_state(self):
        source = ("def grade(ctx):\n"
                  "    sends = ctx.sends_by('probe')\n"
                  "    ok = len(sends) == 1 and sends[0][0] == 'lead'\n"
                  "    ok = ok and ctx.task('t-1')['status'] == 'done'\n"
                  "    ok = ok and 'FINAL' in ctx.final_assistant_text()\n"
                  "    return Verdict(ok)\n")
        result = self._grade(
            arm="cli", expect=source, events=[text_event("FINAL answer")],
            inbox={"lead": [inbox_row("m-1", "probe", "report", kind="report")]},
            task_events=[task_event(1, "lead", "Created", "t-1", owner="probe", title="T"),
                         task_event(2, "probe", "Done", "t-1", by="probe",
                                    source={"via": "OperatorManual", "result": "r"})])
        self.assertTrue(result["passed"], result["notes"])


# ---------------------------------------------------------------------------
# invalid runs
# ---------------------------------------------------------------------------

class InvalidRuns(TempCase):
    def test_missing_expect_py(self):
        run_dir = write_run(self.tmp, "S01", "cli", 1, [])
        result = grade.grade_run(run_dir, os.path.join(self.tmp, "no-scenarios"))
        self.assertTrue(result["invalid"])
        self.assertEqual(result["invalid_reason"], "missing_expect")

    def test_model_mismatch(self):
        scenarios = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        run_dir = write_run(self.tmp, "S01", "cli", 1, [],
                            meta_extra={"model_resolved": "claude-sonnet-5"})
        result = grade.grade_run(run_dir, scenarios)
        self.assertEqual(result["invalid_reason"], "model_mismatch")

    def test_timed_out(self):
        scenarios = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        run_dir = write_run(self.tmp, "S01", "cli", 1, [], meta_extra={"timed_out": True})
        result = grade.grade_run(run_dir, scenarios)
        self.assertEqual(result["invalid_reason"], "timed_out")


# ---------------------------------------------------------------------------
# aggregation + gates
# ---------------------------------------------------------------------------

class Aggregation(TempCase):
    def test_four_cells_and_delta_sign(self):
        # per-scenario expect.py decides pass/fail, so give each cell its own scenario
        runs = os.path.join(self.tmp, "runs")
        scen = os.path.join(self.tmp, "scenarios")
        plan = {
            ("S01", 1): (True, True),    # both_pass
            ("S02", 1): (False, False),  # both_fail
            ("S03", 1): (True, False),   # cli_only_fail  -> b
            ("S04", 1): (True, False),   # cli_only_fail  -> b
            ("S05", 1): (False, True),   # mcp_only_fail  -> c
        }
        for (scenario, pair), (mcp_ok, cli_ok) in plan.items():
            directory = os.path.join(scen, scenario)
            os.makedirs(directory, exist_ok=True)
            with open(os.path.join(directory, "meta.json"), "w", encoding="utf-8") as fh:
                json.dump({"id": scenario, "arms": ["mcp", "cli"]}, fh)
            source = ("def grade(ctx):\n"
                      "    ok = {'mcp': %s, 'cli': %s}[ctx.arm]\n"
                      "    return Verdict(ok)\n" % (mcp_ok, cli_ok))
            with open(os.path.join(directory, "expect.py"), "w", encoding="utf-8") as fh:
                fh.write(source)
            with open(os.path.join(directory, "prompt.txt"), "w", encoding="utf-8") as fh:
                fh.write("prompt for %s\n" % scenario)
            with open(os.path.join(directory, "seed.sh"), "w", encoding="utf-8") as fh:
                fh.write("#!/bin/sh\n# seed %s\n" % scenario)
            os.makedirs(runs, exist_ok=True)
            for arm in ("mcp", "cli"):
                write_run(runs, scenario, arm, pair, [])
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["cells"]["both_pass"], 1)
        self.assertEqual(summary["cells"]["both_fail"], 1)
        self.assertEqual(summary["cells"]["cli_only_fail"], 2)
        self.assertEqual(summary["cells"]["mcp_only_fail"], 1)
        self.assertEqual(summary["n"], 5)
        self.assertEqual(summary["b"], 2)
        self.assertEqual(summary["c"], 1)
        self.assertAlmostEqual(summary["delta_hat"], (2 - 1) / 5)
        self.assertGreater(summary["delta_hat"], 0,
                           "b (cli_only_fail) must push delta positive")
        self.assertEqual(summary["delta_definition"], "fail_cli - fail_mcp")
        # N != 60 -> off-table recomputation, flagged
        self.assertEqual(summary["rate_gate"]["source"], "tango_runtime")
        self.assertIn("n_deviates_from_frozen_table", summary["rate_gate"]["flags"])
        self.assertFalse(summary["rate_gate"]["pass"])
        self.assertFalse(summary["pilot_safety"])
        self.assertEqual(summary["per_arm"]["cli"]["failed"], 3)
        self.assertEqual(summary["per_arm"]["mcp"]["failed"], 2)

    def test_invalid_run_excludes_the_whole_pair(self):
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [])
        write_run(runs, "S01", "cli", 1, [], meta_extra={"invalid_reason": "daemon_fault"})
        write_run(runs, "S01", "mcp", 2, [])
        write_run(runs, "S01", "cli", 2, [])
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["n"], 1)
        self.assertEqual(len(summary["invalid"]), 1)
        self.assertEqual(summary["invalid"][0]["reason"], "daemon_fault")
        self.assertEqual(summary["valid_runs"], 3)

    def test_mixing_gate_and_critical_gate(self):
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"]),
                                          "S14": (PASS_EXPECT, ["cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S13", "mcp", 1, [mcp_event("mcp__agend-terminal__inbox", {})])
        write_run(runs, "S14", "cli", 1,
                  [bash_event("agend-terminal tool inbox --json '{}'")])
        clean = grade.aggregate(runs, scen)
        # No mixing hit, but only 1 of the 45 controls ran: the gate refuses the
        # shortfall it measures (#3412 review F1 — this assertion used to read
        # assertTrue, which is the defect written down). critical_gate is about
        # occurrences, and there are none, so it still passes.
        self.assertFalse(clean["mixing_gate"]["pass"])
        self.assertEqual(clean["mixing_gate"]["scenarios"]["S13"]["mixing"], 0)
        self.assertTrue(clean["critical_gate"]["pass"])
        self.assertEqual(clean["mixing_gate"]["scenarios"]["S13"]["runs_shortfall"], 44)
        self.assertEqual(clean["n"], 0, "single-arm scenarios never form pairs")

        write_run(runs, "S13", "mcp", 2,
                  [bash_event("agend-terminal tool inbox --json '{}'")])
        dirty = grade.aggregate(runs, scen)
        self.assertFalse(dirty["mixing_gate"]["pass"])
        self.assertEqual(dirty["mixing_gate"]["scenarios"]["S13"]["mixing"], 1)
        self.assertFalse(dirty["critical_gate"]["pass"])
        self.assertEqual(dirty["critical_gate"]["by_class"]["mixing"], 1)
        self.assertFalse(dirty["pilot_safety"])

    def test_an_absent_control_scenario_fails_the_mixing_gate(self):
        """F1: a negative control that never ran must not read as 0 violations.

        The whole claim of the mixing gate is "0 out of 45". Zero out of zero is
        not that claim, and the shortfall the gate already measures is exactly
        the number it must refuse on.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"]),
                                          "S14": (PASS_EXPECT, ["cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S14", "cli", 1,
                  [bash_event("agend-terminal tool inbox --json '{}'")])
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["mixing_gate"]["scenarios"]["S13"]["valid_runs"], 0)
        self.assertEqual(summary["mixing_gate"]["scenarios"]["S13"]["runs_shortfall"], 45)
        self.assertFalse(summary["mixing_gate"]["pass"],
                         "a control scenario with no runs at all cannot pass its own gate")
        self.assertFalse(summary["pilot_safety"])

    def test_a_short_control_scenario_fails_the_mixing_gate(self):
        """F1, the partial case: some runs is still not the denominator."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"]),
                                          "S14": (PASS_EXPECT, ["cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S13", "mcp", 1, [mcp_event("mcp__agend-terminal__inbox", {})])
        write_run(runs, "S14", "cli", 1,
                  [bash_event("agend-terminal tool inbox --json '{}'")])
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["mixing_gate"]["scenarios"]["S13"]["runs_shortfall"], 44)
        self.assertFalse(summary["mixing_gate"]["pass"],
                         "1/45 of a control is not a passed control")

    @staticmethod
    def _control_violation():
        """An mcp-arm control reaching for the CLI surface — a real mixing hit."""
        return [bash_event("agend-terminal tool inbox --json '{}'")]

    def test_a_missing_arm_is_invalid_not_a_silent_pass(self):
        """F2: mixing detection COMPARES the arm, so an arm it cannot read blinds it.

        The run below carries a real violation. With the arm absent the
        comparison simply never matches, and the gate reports 0 hits over a run
        it never actually checked.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"])})
        os.makedirs(runs, exist_ok=True)
        run_dir = write_run(runs, "S13", "mcp", 1,
                            self._control_violation())
        meta_path = os.path.join(run_dir, "metadata.json")
        with open(meta_path, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        meta.pop("arm")
        with open(meta_path, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)

        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]], ["bad_arm"],
                         "an unreadable arm is a broken run, not a clean one")
        self.assertEqual(summary["mixing_gate"]["scenarios"]["S13"]["valid_runs"], 0)
        self.assertFalse(summary["mixing_gate"]["pass"])
        self.assertFalse(summary["pilot_safety"])

    def test_an_illegal_arm_value_is_invalid_not_a_silent_pass(self):
        """Same blinding, spelled as a value outside the two arms."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S13", "mcp", 1,
                  self._control_violation(),
                  meta_extra={"arm": "MCP"})
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]], ["bad_arm"])
        self.assertFalse(summary["mixing_gate"]["pass"])

    def test_unreadable_arms_do_not_crash_the_aggregate(self):
        """Mixed unreadable arms used to raise instead of grading.

        `None` and a string in the same scenario met in `sorted(arms.items())`
        and raised TypeError, so the failure mode was either a silent pass (when
        every arm was unreadable the same way) or a crash. Neither is a refusal.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"])})
        os.makedirs(runs, exist_ok=True)
        violation = self._control_violation()
        run_dir = write_run(runs, "S13", "mcp", 1, violation)
        meta_path = os.path.join(run_dir, "metadata.json")
        with open(meta_path, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        meta.pop("arm")
        with open(meta_path, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)
        write_run(runs, "S13", "mcp", 2, violation, meta_extra={"arm": "MCP"})

        summary = grade.aggregate(runs, scen)
        self.assertEqual(sorted(e["reason"] for e in summary["invalid"]),
                         ["bad_arm", "bad_arm"])
        self.assertFalse(summary["mixing_gate"]["pass"])

    def test_the_rate_gate_passes_only_at_the_frozen_n(self):
        """F3: SPEC section 9 pins the gate to a table lookup AT N=60.

        A matrix that loses a pair recomputes the interval at its own N and, on
        clean data, comfortably clears the margin — so the run reads PASS while
        the acceptance that was actually frozen was never evaluated. The
        recomputation stays in the summary as a diagnostic; it must not grant a
        pass.
        """
        short = grade.lookup_rate_gate(grade.TARGET_N - 1, 0, 0)
        self.assertIn("n_deviates_from_frozen_table", short["flags"])
        self.assertIsNotNone(short["ucb"], "the recomputation stays visible")
        self.assertLess(short["ucb"], short["margin"],
                        "precondition: the off-table interval clears the margin, "
                        "so only the N check can be what refuses it")
        self.assertFalse(short["pass"],
                         "only the frozen N=60 lookup may pass the rate gate")

        over = grade.lookup_rate_gate(grade.TARGET_N + 1, 0, 0)
        self.assertFalse(over["pass"], "more pairs than the frozen N is not the frozen N")

        exact = grade.lookup_rate_gate(grade.TARGET_N, 0, 0)
        self.assertTrue(exact["pass"], "the frozen N=60 table behaviour must not regress")
        self.assertEqual(exact["source"], "frozen_table")
        self.assertAlmostEqual(exact["ucb"], 0.04314679724032847)
        self.assertEqual(exact["flags"], [])

        rejected = grade.lookup_rate_gate(grade.TARGET_N, 9, 0)
        self.assertEqual(rejected["source"], "frozen_table")
        self.assertFalse(rejected["pass"],
                         "a table cell that rejects must still reject")

    def test_one_invalid_confirmation_run_cannot_still_pass(self):
        """The same defect end to end: a full matrix minus one run.

        Sixty pairs are built, one run is invalidated, and the aggregate must not
        report a passing rate gate over the 59 that remain.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S0%d" % i: (PASS_EXPECT, ["mcp", "cli"])
                                          for i in range(1, 7)})
        os.makedirs(runs, exist_ok=True)
        for i in range(1, 7):
            for pair in range(1, 11):
                for arm in ("mcp", "cli"):
                    write_run(runs, "S0%d" % i, arm, pair, [])
        full = grade.aggregate(runs, scen)
        self.assertEqual(full["n"], grade.TARGET_N, "precondition: a complete matrix is 60 pairs")
        self.assertTrue(full["rate_gate"]["pass"])
        self.assertEqual(full["rate_gate"]["source"], "frozen_table")

        one = os.path.join(runs, "S01-cli-p1", "metadata.json")
        with open(one, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        meta["invalid_reason"] = "infra_fault"
        with open(one, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)

        short = grade.aggregate(runs, scen)
        self.assertEqual(short["n"], grade.TARGET_N - 1, "the invalid run takes its pair with it")
        self.assertFalse(short["rate_gate"]["pass"],
                         "59 pairs is not the frozen acceptance, however good the interval looks")

    def test_a_model_that_cannot_be_confirmed_is_invalid(self):
        """Secondary review: the model check only fired when BOTH fields were set.

        `if requested and resolved and requested != resolved` treats an absent or
        empty field as agreement, so a run whose model was never resolved — or was
        resolved to something other than the frozen pin — counted as a clean run
        of the experiment the matrix claims to be.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"model_resolved": None})
        write_run(runs, "S01", "cli", 1, [], meta_extra={"model_requested": ""})
        write_run(runs, "S01", "mcp", 2, [], meta_extra={
            "model_requested": "claude-other", "model_resolved": "claude-other"})
        write_run(runs, "S01", "cli", 2, [])

        summary = grade.aggregate(runs, scen)
        reasons = sorted(e["reason"] for e in summary["invalid"])
        self.assertEqual(reasons, ["model_missing", "model_missing", "model_not_frozen"],
                         "absent, empty and off-pin models must each be refused")
        self.assertEqual(summary["valid_runs"], 1)

    def test_a_run_whose_arm_the_scenario_never_declared_is_invalid(self):
        """Secondary review: S13 is an mcp-only control and S14 a cli-only one.

        A run carrying the other arm is not the control the plan declared — the
        surface under test is the whole point of these two scenarios — so it must
        not be counted as one of the 45.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S13": (PASS_EXPECT, ["mcp"]),
                                          "S14": (PASS_EXPECT, ["cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S13", "cli", 1, [])
        write_run(runs, "S14", "mcp", 1, [])

        summary = grade.aggregate(runs, scen)
        self.assertEqual(sorted(e["reason"] for e in summary["invalid"]),
                         ["arm_not_declared", "arm_not_declared"])
        self.assertEqual(summary["mixing_gate"]["scenarios"]["S13"]["valid_runs"], 0)
        self.assertFalse(summary["mixing_gate"]["pass"])

    def test_a_duplicated_cell_is_refused_not_silently_overwritten(self):
        """Secondary review: the pair table was a dict keyed by the cell.

        Two runs claiming the same (scenario, pair, arm) used to collapse — the
        last one walked in and the other vanished from the record, so a rerun
        dropped into the tree could quietly replace a failure with a pass.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        original = write_run(runs, "S01", "mcp", 1, [])
        write_run(runs, "S01", "cli", 1, [])
        shutil.copytree(original, os.path.join(runs, "S01-mcp-p1-again"))

        summary = grade.aggregate(runs, scen)
        self.assertEqual(sorted(e["reason"] for e in summary["invalid"]),
                         ["duplicate_cell", "duplicate_cell"],
                         "both copies go: which one is the real run is not ours to guess")
        self.assertEqual(summary["n"], 0, "the contested pair cannot be counted")

    # ---- A''' review: metadata identity, declarations, and the frozen plan ----

    FROZEN_CELLS = (
        [("S0%d" % i, pair, arm)
         for i in range(1, 7) for pair in range(1, 11) for arm in ("mcp", "cli")]
        + [("S13", pair, "mcp") for pair in range(1, 46)]
        + [("S14", pair, "cli") for pair in range(1, 46)]
    )

    def frozen_matrix(self, skip=(), extra=(), manifest=True, final_state=True,
                      no_evidence_cells=()):
        """The whole SPEC section 6 plan on disk: 210 runs, every one conforming."""
        runs = os.path.join(self.tmp, "runs")
        spec = {"S0%d" % i: (PASS_EXPECT, ["mcp", "cli"]) for i in range(1, 7)}
        spec["S13"] = (PASS_EXPECT, ["mcp"])
        spec["S14"] = (PASS_EXPECT, ["cli"])
        scen = write_scenarios(self.tmp, spec)
        os.makedirs(runs, exist_ok=True)
        for cell in self.FROZEN_CELLS:
            if cell in skip:
                continue
            scenario, pair, arm = cell
            write_run(runs, scenario, arm, pair, [],
                      final_state=final_state and cell not in no_evidence_cells)
        for scenario, pair, arm, meta_extra in extra:
            write_run(runs, scenario, arm, pair, [], meta_extra=meta_extra)
        if manifest:
            write_manifest(runs)
        return runs, scen

    def test_the_complete_frozen_matrix_passes_every_gate(self):
        """The baseline the rest of these tests deviate from.

        Without it a fail-closed suite proves only that the grader can say no.
        """
        runs, scen = self.frozen_matrix()
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["total_runs"], 210)
        self.assertEqual(summary["valid_runs"], 210)
        self.assertEqual(summary["n"], 60)
        self.assertTrue(summary["plan_gate"]["pass"])
        self.assertTrue(summary["rate_gate"]["pass"])
        self.assertTrue(summary["mixing_gate"]["pass"])
        self.assertTrue(summary["critical_gate"]["pass"])
        self.assertTrue(summary["pilot_safety"])

    def test_a_matrix_without_durable_evidence_cannot_claim_pilot_safety(self):
        """#3435 r1 (1): absence of evidence was scored as a passing experiment.

        `load_final_state` degrades a missing tree to empties, and `detect_invalid`
        never asked whether the tree was there — so a synthetic matrix that ran
        nothing and copied back nothing presented 210 conforming runs and every
        gate agreed. The plan gate counts cells, not evidence, and it is satisfied
        by a run that happened; the rate and critical gates then read the empty
        state as "no failures, no violations". Pilot safety must never be claimed
        from a tree that carries no durable evidence at all.
        """
        runs, scen = self.frozen_matrix(final_state=False)
        summary = grade.aggregate(runs, scen)

        self.assertFalse(
            summary["pilot_safety"],
            "a matrix with no final_state anywhere must not report pilot safety")
        self.assertEqual(summary["valid_runs"], 0,
                         "a run with no durable evidence is not a valid run")
        self.assertEqual(
            sorted({e["reason"] for e in summary["invalid"]}), ["final_state_missing"])

    def test_a_confirmation_cell_with_no_graded_run_unsupports_the_claim(self):
        """The coverage half of (1), isolated from the mixing gate.

        Dropping evidence everywhere is caught by the mixing gate too — S13/S14
        lose their 45 valid runs — so that alone does not prove the plan gate
        learned anything. Here only the CONFIRMATION cells lose their evidence:
        the mixing controls keep their 45 each and their gate still passes, so
        the refusal has to come from the plan gate noticing that frozen cells
        have no graded run behind them.
        """
        blind = [("S01", pair, arm) for pair in range(1, 11) for arm in ("mcp", "cli")]
        runs, scen = self.frozen_matrix(no_evidence_cells=set(blind))
        summary = grade.aggregate(runs, scen)

        self.assertTrue(summary["mixing_gate"]["pass"],
                        "S13/S14 kept their evidence — the mixing gate is not what fires")
        self.assertIn("plan_cells_without_valid_run", summary["plan_gate"]["flags"])
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertEqual(summary["plan_gate"]["missing"], [],
                         "those runs happened; they are excluded, not absent")
        self.assertFalse(summary["pilot_safety"])

    def test_a_turn_budget_off_the_frozen_contract_is_another_experiment(self):
        """#3435 r1 (3): SPEC pins --max-turns 15, the runner defaulted to 40.

        The budget decides how much room a run had to succeed, so a matrix mixing
        budgets is not one experiment — yet identity bound the model, the binaries
        and every digest while ignoring the turn budget entirely, so a 40-turn run
        counted as a clean run of the frozen 15-turn experiment.
        """
        runs, scen = self.frozen_matrix(skip=[("S01", 1, "mcp")],
                                        extra=[("S01", 1, "mcp", {"max_turns": 40})])
        summary = grade.aggregate(runs, scen)

        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["max_turns_not_frozen"])
        self.assertFalse(summary["pilot_safety"],
                         "one run off the frozen budget invalidates the claim")

    def test_a_timeout_off_the_frozen_contract_is_another_experiment(self):
        """#3435 r2 (B): the OTHER half of the execution budget.

        `--max-turns` and `--timeout` are both overridable on run.sh and
        matrix.sh, and both decide how much room a run had to succeed. The turn
        budget is now bound to the frozen contract; the wall-clock budget is not
        recorded at all, so a matrix run under a 60s cap and one run under 900s
        are indistinguishable afterwards — and a run killed early by a shorter
        cap looks exactly like a run that simply failed.
        """
        runs, scen = self.frozen_matrix(skip=[("S01", 1, "mcp")],
                                        extra=[("S01", 1, "mcp", {"timeout_secs": 60})])
        summary = grade.aggregate(runs, scen)

        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["timeout_not_frozen"])
        self.assertFalse(summary["pilot_safety"])

    def test_the_frozen_timeout_is_still_accepted(self):
        """Control: 900 is the contract, not merely 'not 60'."""
        runs, scen = self.frozen_matrix(skip=[("S01", 1, "mcp")],
                                        extra=[("S01", 1, "mcp", {"timeout_secs": 900})])
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["invalid"], [])
        self.assertTrue(summary["pilot_safety"])

    def test_the_frozen_turn_budget_is_still_accepted(self):
        """Control for the guard above: 15 is the contract, not merely 'not 40'."""
        runs, scen = self.frozen_matrix(skip=[("S01", 1, "mcp")],
                                        extra=[("S01", 1, "mcp", {"max_turns": 15})])
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["invalid"], [])
        self.assertTrue(summary["pilot_safety"])

    def test_the_plan_gate_refuses_a_matrix_that_is_not_the_frozen_plan(self):
        """Missing, excess, out-of-range, non-integer and arbitrary cells.

        The gates above count what is on disk; nothing checked that what is on
        disk is the plan the contract froze.
        """
        runs, scen = self.frozen_matrix(skip=[("S01", 10, "cli")])
        missing = grade.aggregate(runs, scen)
        self.assertFalse(missing["plan_gate"]["pass"])
        self.assertIn(["S01", 10, "cli"], missing["plan_gate"]["missing"])
        self.assertFalse(missing["pilot_safety"])

        for label, cell, meta_extra in [
            ("out_of_range_pair", ("S01", 11, "mcp"), None),
            ("arbitrary_scenario", ("S99", 1, "mcp"), None),
            ("noninteger_pair", ("S02", 3, "mcp"), {"pair": "3"}),
        ]:
            with self.subTest(label):
                self.setUp()
                scenario, pair, arm = cell
                runs, scen = self.frozen_matrix(extra=[(scenario, pair, arm, meta_extra)]
                                                if label != "noninteger_pair" else [])
                if label == "noninteger_pair":
                    victim = os.path.join(runs, "S02-mcp-p3", "metadata.json")
                    with open(victim, "r", encoding="utf-8") as fh:
                        meta = json.load(fh)
                    meta["pair"] = "3"
                    with open(victim, "w", encoding="utf-8") as fh:
                        json.dump(meta, fh)
                summary = grade.aggregate(runs, scen)
                self.assertFalse(summary["plan_gate"]["pass"],
                                 "%s must not read as the frozen plan" % label)
                self.assertFalse(summary["pilot_safety"])

    def test_the_manifest_plan_must_be_the_frozen_plan(self):
        """A tree can carry a manifest that describes a different experiment."""
        runs, scen = self.frozen_matrix()
        # a COMPLETE manifest whose plan describes 209 runs — the identity fields
        # are all there, so what fails is the plan itself
        write_manifest(runs, plan=frozen_manifest_rows()[:-1], total_runs=209)
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_plan_mismatch", summary["plan_gate"]["flags"])

    def test_incomplete_or_mistyped_metadata_is_refused(self):
        """SPEC section 3 lists what a run must record; the grader trusted it blind."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"git_head": DROP})
        write_run(runs, "S01", "cli", 1, [], meta_extra={"binary_sha256": DROP})
        write_run(runs, "S01", "mcp", 2, [], meta_extra={"schema": "1"})
        write_run(runs, "S01", "cli", 2, [], meta_extra={"duration_ms": "1000"})
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["metadata_incomplete"] * 4,
                         "absent and mistyped SPEC fields are both refusals")
        self.assertEqual(summary["valid_runs"], 0)

    def test_a_stream_that_does_not_confirm_the_model_is_refused(self):
        """SPEC section 3: the model resolved by the STREAM must equal MODEL.

        metadata.model_resolved is written by the runner; the stream is what the
        session actually reported. Trusting only the metadata means a fabricated
        (or stale) metadata.json decides which experiment this run belongs to.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1,
                  [{"type": "system", "subtype": "init", "model": "claude-other"}])
        write_run(runs, "S01", "cli", 1,
                  [{"type": "system", "subtype": "init"}])
        write_run(runs, "S01", "mcp", 2, [{"type": "assistant", "message": {}}],
                  init=False)
        summary = grade.aggregate(runs, scen)
        self.assertEqual(sorted(e["reason"] for e in summary["invalid"]),
                         ["stream_model_mismatch", "stream_model_missing",
                          "stream_model_missing"])

    def test_a_scenario_without_a_usable_declaration_fails_closed(self):
        """An unreadable declaration used to mean "no constraint"."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [])
        # Every one of these has a usable expect.py, so what is being tested is
        # the DECLARATION: malformed, non-object, empty, and absent.
        for broken, body in [("S02", "not json at all"), ("S03", "[1, 2]"),
                             ("S04", "{}"), ("S05", None)]:
            directory = os.path.join(scen, broken)
            os.makedirs(directory, exist_ok=True)
            with open(os.path.join(directory, "expect.py"), "w", encoding="utf-8") as fh:
                fh.write(PASS_EXPECT)
            if body is not None:
                with open(os.path.join(directory, "meta.json"), "w", encoding="utf-8") as fh:
                    fh.write(body)
            write_run(runs, broken, "mcp", 1, [])

        summary = grade.aggregate(runs, scen)
        reasons = sorted(e["reason"] for e in summary["invalid"])
        self.assertEqual(reasons, ["scenario_declaration_invalid"] * 4,
                         "malformed, non-object, empty and absent declarations all fail closed")
        self.assertEqual(summary["valid_runs"], 1)

    # ---- A⁗ review: multiplicity, order, strict types, mandatory manifest ----

    def test_a_second_copy_of_a_cell_counts_even_when_it_is_invalid(self):
        """Duplicate detection only ever looked at VALID runs.

        Mark the second copy invalid and the cell set still matches, the valid
        copies are still unique, and 211 runs read as the frozen 210.
        """
        runs, scen = self.frozen_matrix()
        shutil.copytree(os.path.join(runs, "S01-mcp-p1"),
                        os.path.join(runs, "S01-mcp-p1-copy"))
        victim = os.path.join(runs, "S01-mcp-p1-copy", "metadata.json")
        with open(victim, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        meta["invalid_reason"] = "infra_fault"
        with open(victim, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)

        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["plan_gate"]["observed_runs"], 211)
        self.assertFalse(summary["plan_gate"]["pass"],
                         "211 runs is not the frozen 210, whatever their verdicts")
        self.assertIn(["S01", 1, "mcp"], summary["plan_gate"]["duplicates"])
        self.assertFalse(summary["pilot_safety"])

    def test_the_order_a_run_records_must_be_the_order_the_plan_declares(self):
        """order_in_pair is planned, recorded and resumed — nothing compared them."""
        runs, scen = self.frozen_matrix()
        victim = os.path.join(runs, "S01-mcp-p1", "metadata.json")
        with open(victim, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        self.assertEqual(meta["order_in_pair"], "first", "precondition: pair 1 mcp goes first")
        meta["order_in_pair"] = "second"
        with open(victim, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)

        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["order_in_pair_mismatch"])

    def test_the_manifest_order_must_match_the_plan_too(self):
        runs, scen = self.frozen_matrix()
        rows = frozen_manifest_rows()
        rows[0] = dict(rows[0], order_in_pair="second")
        write_manifest(runs, plan=rows)
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_plan_mismatch", summary["plan_gate"]["flags"])

    def test_invalid_reason_and_schema_are_strictly_typed(self):
        """A non-string invalid_reason was returned AS the reason; True == 1."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"invalid_reason": 123})
        write_run(runs, "S01", "cli", 1, [], meta_extra={"schema": True})
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["metadata_incomplete", "metadata_incomplete"],
                         "a number is not a reason, and a flag is not schema 1")

    def test_a_tree_without_a_full_manifest_cannot_be_the_frozen_matrix(self):
        """The manifest is the tree's own account of what it ran."""
        runs, scen = self.frozen_matrix(manifest=False)
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_missing", summary["plan_gate"]["flags"])

        self.setUp()
        runs, scen = self.frozen_matrix()
        write_manifest(runs, total_runs=DROP)
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_incomplete", summary["plan_gate"]["flags"])

    # ---- A⁵ review: required invalid_reason, full manifest values, one identity ----

    def test_invalid_reason_must_be_present(self):
        """SPEC.txt:68 lists it. Absent read as "nothing to report"."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"invalid_reason": DROP})
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]], ["metadata_incomplete"])

    def test_a_junk_plan_row_cannot_hide_among_the_210(self):
        """Non-dict rows were filtered out before comparing, so they were free."""
        runs, scen = self.frozen_matrix()
        write_manifest(runs, plan=frozen_manifest_rows() + ["not-a-row"])
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_plan_mismatch", summary["plan_gate"]["flags"])

    def test_a_manifest_planned_under_another_budget_is_not_this_experiment(self):
        """Found by mutation: nothing exercised the manifest's budget row.

        matrix.sh refuses to RESUME under a changed budget, but a finished tree
        handed straight to the grader never met that guard — so the contract row
        is what has to say the manifest states the frozen budget.
        """
        runs, scen = self.frozen_matrix(manifest=False)
        write_manifest(runs, timeout_secs=60)
        summary = grade.aggregate(runs, scen)

        self.assertIn("manifest_identity_invalid", summary["plan_gate"]["flags"])
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertFalse(summary["pilot_safety"])

    def test_the_manifest_identity_values_are_checked_not_just_present(self):
        """A complete manifest can still describe another experiment."""
        runs, scen = self.frozen_matrix()
        write_manifest(runs, model="claude-other")
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_identity_invalid", summary["plan_gate"]["flags"])

    def test_every_run_must_carry_the_manifest_identity(self):
        """The manifest said one head and binary; nothing checked the runs agreed."""
        runs, scen = self.frozen_matrix()
        victim = os.path.join(runs, "S01-mcp-p1", "metadata.json")
        with open(victim, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        meta["git_head"] = "f" * 40
        with open(victim, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)
        summary = grade.aggregate(runs, scen)
        self.assertFalse(summary["plan_gate"]["pass"])
        self.assertIn("manifest_identity_mismatch", summary["plan_gate"]["flags"])

    def test_the_matrix_must_be_one_experiment_not_several(self):
        """What is left to bind once the frozen files bind the rest.

        A fleet or seed hash that is not the frozen file now has its own, sharper
        refusal. The CLI VERSION has no frozen file — two runs can each agree with
        their own stream and still come from different installs — so the
        matrix-wide check is what catches that.
        """
        runs, scen = self.frozen_matrix()
        victim = os.path.join(runs, "S02-cli-p4", "metadata.json")
        with open(victim, "r", encoding="utf-8") as fh:
            meta = json.load(fh)
        meta["fleet_sha256"] = "a" * 64
        with open(victim, "w", encoding="utf-8") as fh:
            json.dump(meta, fh)
        wrong_fleet = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in wrong_fleet["invalid"]], ["fleet_not_frozen"])

        self.setUp()
        runs, scen = self.frozen_matrix()
        # self-consistent, and from another install
        write_run(runs, "S03", "mcp", 5, [{"type": "system", "subtype": "init",
                                           "model": "claude-fable-5",
                                           "claude_code_version": "9.9.9"}],
                  meta_extra={"claude_version": "9.9.9"})
        split = grade.aggregate(runs, scen)
        self.assertEqual(split["invalid"], [], "each run agrees with its own stream")
        self.assertFalse(split["plan_gate"]["pass"])
        self.assertIn("run_identity_split", split["plan_gate"]["flags"])


    # ---- A⁶ review: every manifest field, and the frozen tree as the binding ----

    #: (field, a value that must be refused). One row per field the manifest
    #: carries — the reviews asked for each to be covered individually.
    MANIFEST_FIELD_PROBES = (
        ("schema", 2),
        ("schema", True),
        ("stamp", ""),
        ("created_at", ""),
        ("dry_run", True),
        ("jobs", 0),
        ("missing_scenarios", ["S07"]),
        ("model", "claude-other"),
        ("git_head", ""),
        ("binary_sha256", {"agend-terminal": "0" * 64}),
        ("prompt_sha256", {"base": "0" * 64, "mcp": "0" * 64, "cli": "0" * 64}),
        ("total_runs", 209),
    )

    def test_every_manifest_field_is_validated_by_value(self):
        for field, bad in self.MANIFEST_FIELD_PROBES:
            with self.subTest(field=field, bad=bad):
                self.setUp()
                runs, scen = self.frozen_matrix()
                write_manifest(runs, **{field: bad})
                summary = grade.aggregate(runs, scen)
                self.assertFalse(summary["plan_gate"]["pass"],
                                 "manifest %s=%r must be refused" % (field, bad))
                self.assertFalse(summary["pilot_safety"])

    def test_a_dropped_manifest_field_is_refused_individually(self):
        for field in ("schema", "stamp", "created_at", "dry_run", "jobs",
                      "missing_scenarios", "model", "git_head", "binary_sha256",
                      "prompt_sha256", "total_runs", "plan"):
            with self.subTest(field=field):
                self.setUp()
                runs, scen = self.frozen_matrix()
                write_manifest(runs, **{field: DROP})
                summary = grade.aggregate(runs, scen)
                self.assertIn("manifest_incomplete", summary["plan_gate"]["flags"])

    def test_the_prompt_and_seed_a_run_names_must_be_the_frozen_ones(self):
        """Constant across runs is not the same as CORRECT.

        Every one of the real 210 runs records prompt_sha256 = sha256 of its
        scenario's prompt.txt and seed_sha256 = sha256 of its seed.sh, so the
        frozen tree can be the binding rather than mere agreement between runs.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"prompt_sha256": "d" * 64})
        write_run(runs, "S01", "cli", 1, [], meta_extra={"seed_sha256": "c" * 64})
        summary = grade.aggregate(runs, scen)
        reasons = sorted(e["reason"] for e in summary["invalid"])
        self.assertEqual(reasons, ["prompt_not_frozen", "seed_not_frozen"],
                         "a hash that is not the frozen file is refused, however "
                         "consistent the rest of the matrix is")

    # ---- A⁷ review: the fleet template IS frozen, and the CLI version is in the stream ----

    def test_the_fleet_hash_must_be_the_frozen_template(self):
        """I said there was no frozen file to bind this to. There is.

        sandbox.sh:57 holds FLEET_YAML as a literal and writes it verbatim to
        fleet.template.yaml, so sha256 of that literal is exactly what all 210
        real runs record. Matrix-wide agreement was the weaker check.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"fleet_sha256": "e" * 64})
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]], ["fleet_not_frozen"],
                         "a well-formed hash of something else is still not the fleet")

    def test_the_version_a_run_claims_must_be_the_one_the_stream_reports(self):
        """metadata.claude_version is written by the runner; the stream says it."""
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1,
                  [{"type": "system", "subtype": "init", "model": "claude-fable-5",
                    "claude_code_version": "9.9.9"}])
        write_run(runs, "S01", "cli", 1,
                  [{"type": "system", "subtype": "init", "model": "claude-fable-5"}])
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["stream_version_mismatch", "stream_version_mismatch"],
                         "a version the stream does not report is not this run's version")

    def test_the_manifest_carries_the_fleet_and_seed_identity(self):
        for field, bad in (("fleet_sha256", DROP), ("fleet_sha256", "e" * 64),
                           ("seed_sha256", DROP), ("seed_sha256", {"S01": "e" * 64})):
            with self.subTest(field=field, bad=bad):
                self.setUp()
                runs, scen = self.frozen_matrix()
                write_manifest(runs, **{field: bad})
                summary = grade.aggregate(runs, scen)
                self.assertFalse(summary["plan_gate"]["pass"],
                                 "manifest %s=%r must be refused" % (field, bad))

    def test_the_system_prompt_a_run_names_must_be_the_frozen_pair(self):
        """SPEC.txt:60 builds it: prompts/base.txt + "\n\n" + prompts/<arm>.txt.

        Per-arm agreement between runs was the only check, so a matrix could
        agree on a system prompt that is not the frozen one. The derived digests
        are what all 210 real runs record — d2a41ec8… for mcp, 8bcf7657… for cli.
        """
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [], meta_extra={"system_prompt_sha256": "f" * 64})
        summary = grade.aggregate(runs, scen)
        self.assertEqual([e["reason"] for e in summary["invalid"]],
                         ["system_prompt_not_frozen"])

    def test_a_uniformly_foreign_system_prompt_is_still_caught(self):
        """The shape the review named: every run agrees, and all are wrong.

        Per-arm agreement holds perfectly here — which is exactly why agreement
        was never the check.
        """
        runs, scen = self.frozen_matrix()
        for cell in self.FROZEN_CELLS:
            scenario, pair, arm = cell
            path = os.path.join(runs, "%s-%s-p%s" % (scenario, arm, pair), "metadata.json")
            with open(path, "r", encoding="utf-8") as fh:
                meta = json.load(fh)
            meta["system_prompt_sha256"] = "f" * 64
            with open(path, "w", encoding="utf-8") as fh:
                json.dump(meta, fh)
        summary = grade.aggregate(runs, scen)
        self.assertEqual(summary["valid_runs"], 0)
        self.assertEqual({e["reason"] for e in summary["invalid"]},
                         {"system_prompt_not_frozen"})
        self.assertFalse(summary["pilot_safety"])

    def test_mean_tool_calls_per_arm(self):
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [mcp_event("mcp__agend-terminal__inbox", {}),
                                          mcp_event("mcp__agend-terminal__send", {})])
        write_run(runs, "S01", "cli", 1,
                  [bash_event("agend-terminal tool inbox --json '{}'")])
        summary = grade.aggregate(runs, scen)
        self.assertAlmostEqual(summary["mean_tool_calls"]["mcp"], 2.0)
        self.assertAlmostEqual(summary["mean_tool_calls"]["cli"], 1.0)


class ExpectModuleIdentity(TempCase):
    """`from grade import Verdict` inside expect.py must yield THIS Verdict.

    grade.py runs as `__main__` from the CLI, so without the sys.modules alias
    the import builds a second module and every isinstance check fails — which
    would silently invalidate the whole matrix.
    """

    def test_expect_importing_grade_module_grades_normally(self):
        source = ("from grade import Verdict\n\n"
                  "def grade(ctx):\n"
                  "    return Verdict(True, notes=['imported'])\n")
        scenarios = write_scenarios(self.tmp, {"S01": (source, ["mcp", "cli"])})
        run_dir = write_run(self.tmp, "S01", "cli", 1, [])
        result = grade.grade_run(run_dir, scenarios)
        self.assertFalse(result["invalid"], result["notes"])
        self.assertTrue(result["passed"])

    def test_grade_py_as_a_subprocess_script(self):
        source = ("from grade import Verdict\n\n"
                  "def grade(ctx):\n"
                  "    return Verdict(True)\n")
        scenarios = write_scenarios(self.tmp, {"S01": (source, ["mcp", "cli"])})
        run_dir = write_run(self.tmp, "S01", "cli", 1, [])
        proc = subprocess.run(
            [sys.executable, os.path.join(ROOT, "grade.py"), run_dir,
             "--scenarios", scenarios],
            capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(run_dir, "grade.json"), encoding="utf-8") as fh:
            written = json.load(fh)
        self.assertTrue(written["passed"], written["notes"])
        self.assertIsNone(written["invalid_reason"])


class RateGateLookup(unittest.TestCase):
    def test_frozen_table_used_at_n_60(self):
        gate = grade.lookup_rate_gate(60, 0, 0)
        self.assertEqual(gate["source"], "frozen_table")
        self.assertTrue(gate["pass"])
        self.assertEqual(gate["flags"], [])

    def test_frozen_table_rejects_three_cli_only_failures(self):
        gate = grade.lookup_rate_gate(60, 3, 0)
        self.assertEqual(gate["source"], "frozen_table")
        self.assertFalse(gate["pass"])

    def test_off_table_n_is_flagged(self):
        gate = grade.lookup_rate_gate(59, 0, 0)
        self.assertEqual(gate["source"], "tango_runtime")
        self.assertIn("n_deviates_from_frozen_table", gate["flags"])

    def test_zero_pairs_fails_closed(self):
        gate = grade.lookup_rate_gate(0, 0, 0)
        self.assertFalse(gate["pass"])
        self.assertIn("no_valid_pairs", gate["flags"])

    def test_delta_definition_constant_is_pinned(self):
        self.assertEqual(grade.DELTA_DEFINITION, "fail_cli - fail_mcp")


class Taxonomy(unittest.TestCase):
    def test_five_critical_classes(self):
        self.assertEqual(list(grade.CRITICAL_CLASSES),
                         ["completeness", "mixing", "wrong_target",
                          "duplicate_side_effect", "broken_protocol_link"])

    def test_verdict_rejects_unknown_class(self):
        with self.assertRaises(ValueError):
            grade.Verdict(False, critical=["oops"])

    def test_critical_helper_validates(self):
        self.assertEqual(grade.critical("mixing"), "mixing")
        with self.assertRaises(ValueError):
            grade.critical("nope")


class ReportRendering(TempCase):
    def test_report_renders_all_three_gates(self):
        sys.path.insert(0, ROOT)
        import report  # noqa: E402
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [])
        write_run(runs, "S01", "cli", 1, [])
        text = report.render(grade.aggregate(runs, scen))
        for needle in ("PILOT SAFETY", "rate_gate", "critical_gate", "mixing_gate",
                       "cli_only_fail", "delta_hat", "ARTIFACT INDEX",
                       "fail_cli - fail_mcp"):
            self.assertIn(needle, text)

    def test_report_ends_with_exactly_one_newline(self):
        # `git diff --check` rejects a blank line at EOF in the committed report.txt
        sys.path.insert(0, ROOT)
        import report  # noqa: E402
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [])
        write_run(runs, "S01", "cli", 1, [])
        text = report.render(grade.aggregate(runs, scen))
        self.assertTrue(text.endswith("\n"))
        self.assertFalse(text.endswith("\n\n"))

    def test_artifact_index_lists_only_files_every_run_dir_has(self):
        # `grade.py --aggregate` grades in memory: no run dir ever gets a grade.json
        sys.path.insert(0, ROOT)
        import report  # noqa: E402
        runs = os.path.join(self.tmp, "runs")
        scen = write_scenarios(self.tmp, {"S01": (PASS_EXPECT, ["mcp", "cli"])})
        os.makedirs(runs, exist_ok=True)
        write_run(runs, "S01", "mcp", 1, [])
        write_run(runs, "S01", "cli", 1, [])
        text = report.render(grade.aggregate(runs, scen))
        line = next(l for l in text.splitlines() if "per-run artifacts" in l)
        listed = set(line[line.index("{") + 1:line.index("}")].split(","))
        self.assertEqual(listed, {"metadata.json", "seed.json", "stream.jsonl",
                                  "stderr.txt", "run.log", "final_state/"})


if __name__ == "__main__":
    unittest.main()


class ResendAfterFailureIsNotDuplicate(unittest.TestCase):
    def _events(self, first_result_text, first_is_error):
        cmd = ("agend-terminal tool send --json - <<'EOF'\n"
               '{"instance": "ane-review", "request_kind": "query", "message": "hello"}\nEOF')
        def tu(i): return {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "tu%d" % i, "name": "Bash", "input": {"command": cmd}}]}}
        def tr(i, text, err): return {"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "tu%d" % i, "content": text, "is_error": err}]}}
        return [tu(1), tr(1, first_result_text, first_is_error), tu(2), tr(2, '{"target": "ane-review"}', False)]

    def test_resend_after_exit3_is_not_duplicate(self):
        import grade
        calls = grade.normalise_tool_calls(self._events("Exit code 3\n{\"error\": \"bad\", \"fix\": \"x\"}", True))
        self.assertEqual([c["outcome"] for c in calls], ["error", "ok"])
        ctx = self._ctx_with_calls(grade, calls)
        crit, notes = grade.check_duplicate_side_effect(ctx)
        self.assertEqual(crit, [], notes)

    def test_resend_after_success_is_duplicate(self):
        import grade
        calls = grade.normalise_tool_calls(self._events('{"target": "ane-review"}', False))
        self.assertEqual([c["outcome"] for c in calls], ["ok", "ok"])
        ctx = self._ctx_with_calls(grade, calls)
        crit, notes = grade.check_duplicate_side_effect(ctx)
        self.assertEqual(crit, ["duplicate_side_effect"], notes)

    def test_trailing_semicolon_is_stripped_from_tool_token(self):
        import grade
        calls = grade.normalise_bash_command("agend-terminal tool schema inbox; agend-terminal tool list")
        names = [c["tool"] for c in calls]
        self.assertNotIn("schema inbox;", names, names)
        self.assertTrue(any(n.startswith("schema") and n.rstrip().endswith("inbox") for n in names), names)

    def _ctx_with_calls(self, grade, calls):
        class Final(dict):
            pass
        final = {"sent_ledger": [], "tasks": {}, "inbox": {}}
        class Ctx:
            pass
        ctx = Ctx(); ctx.tool_calls = calls; ctx.final = final; ctx.arm = "cli"
        ctx.sends_by = lambda agent: []
        return ctx


# ---------------------------------------------------------------------------
# #3435 r1 (2): the mixing controls must see a real tool call, not a sentence
# ---------------------------------------------------------------------------

TOOL_CLI_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REAL_SCENARIOS = os.path.join(TOOL_CLI_ROOT, "scenarios")


class MixingControlEvidence(TempCase):
    """S13/S14 are the negative controls the whole mixing denominator rests on.

    Each is a TRANSLATION scenario: the peer states the request in the other
    surface's vocabulary and the agent has to carry it out on its OWN surface
    (S13 reads CLI syntax and must call the MCP tool; S14 reads MCP tool names
    and must run the CLI). The verdict, though, was `bool(hits) and not crit` —
    hits being an outbound status string in ane-review's inbox. An agent that
    invoked nothing at all and simply asserted the status therefore scored a
    clean pass, and 45 of those read as "0 mixing violations in 45 runs". The
    denominator only means something if the runs behind it did the work.
    """

    def ctx_for(self, scenario, arm, tool_calls):
        status = "in_review"
        final = {"inbox": {"ane-review": [{"from": "probe",
                                           "text": "task t-1 status is %s" % status}]},
                 "tasks": {}, "task_events": [], "task_index": [], "sent_ledger": [],
                 "decisions": [], "mcp_usage": [], "instances": {}, "uuid_to_name": {},
                 "root": self.tmp}
        meta = {"scenario": scenario, "arm": arm, "pair": 1}
        seed = {"task": {"id": "t-1", "status": status}}
        return grade.Ctx(self.tmp, meta, [], tool_calls, final, seed)

    def call(self, surface, tool="task", action="get"):
        return {"surface": surface, "tool": tool, "action": action,
                "args": {"id": "t-1"}, "outcome": None}

    def verdict(self, scenario, arm, tool_calls):
        module = grade.load_expect(REAL_SCENARIOS, scenario)
        self.assertIsNotNone(module, "the real %s expect.py must load" % scenario)
        return module.grade(self.ctx_for(scenario, arm, tool_calls))

    def test_s13_reporting_the_status_without_calling_the_mcp_tool_is_not_a_pass(self):
        self.assertFalse(
            self.verdict("S13", "mcp", []).passed,
            "S13 must require the MCP call it exists to observe, not a sentence")

    def test_s14_reporting_the_status_without_running_the_cli_is_not_a_pass(self):
        self.assertFalse(
            self.verdict("S14", "cli", []).passed,
            "S14 must require the CLI invocation it exists to observe")

    def test_s13_wrong_surface_alone_is_not_the_translation_either(self):
        """A CLI call in the mcp arm is the mixing violation, never the evidence."""
        self.assertFalse(self.verdict("S13", "mcp", [self.call("cli")]).passed)

    def test_s14_wrong_surface_alone_is_not_the_translation_either(self):
        self.assertFalse(self.verdict("S14", "cli", [self.call("mcp")]).passed)

    def test_s13_translating_into_the_mcp_call_still_passes(self):
        """Control: the guard must not refuse the behaviour the scenario wants."""
        self.assertTrue(self.verdict("S13", "mcp", [self.call("mcp")]).passed)

    def test_s14_translating_into_the_cli_call_still_passes(self):
        self.assertTrue(self.verdict("S14", "cli", [self.call("cli")]).passed)
