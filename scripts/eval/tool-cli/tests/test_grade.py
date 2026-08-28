#!/usr/bin/env python3
"""Tests for grade.py: normalisation, generic checks, aggregation, gates.

The final_state fixtures under tests/fixtures/final_state_sample/ were produced
by a REAL daemon (release binary, hermetic AGEND_HOME under $TMPDIR) on
2026-08-28, so the loader is tested against the shapes the harness will really
copy — not against invented ones.
"""

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


def write_run(base, scenario, arm, pair, events, inbox=None, task_events=None,
              meta_extra=None, sent_ledger=None):
    run_dir = os.path.join(base, "%s-%s-p%s" % (scenario, arm, pair))
    os.makedirs(os.path.join(run_dir, "final_state", "inbox"), exist_ok=True)
    meta = {"schema": 1, "scenario": scenario, "arm": arm, "pair": pair,
            "order_in_pair": "first", "model_requested": "claude-fable-5",
            "model_resolved": "claude-fable-5", "fence": True, "exit_code": 0,
            "turns": 3, "timed_out": False, "invalid_reason": None}
    meta.update(meta_extra or {})
    with open(os.path.join(run_dir, "metadata.json"), "w", encoding="utf-8") as fh:
        json.dump(meta, fh)
    with open(os.path.join(run_dir, "stream.jsonl"), "w", encoding="utf-8") as fh:
        for event in events:
            fh.write(json.dumps(event) + "\n")
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
    return root


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

class FinalStateLoader(unittest.TestCase):
    def test_real_fixture_shapes(self):
        state = grade.load_final_state(SAMPLE)
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

    def test_fixture_fleet_yaml_is_committed_and_not_ignored(self):
        # The repo-level .gitignore excludes every `fleet.yaml` (the operator's live
        # one); the fixture's copy is the only source of the uuid<->name map, so an
        # ignored, untracked fixture passes locally and fails in every fresh checkout.
        if shutil.which("git") is None:
            self.skipTest("git not on PATH")
        root = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=HERE,
                              capture_output=True, text=True, check=True).stdout.strip()
        rel = os.path.relpath(os.path.join(SAMPLE, "fleet.yaml"), root)
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
        self.assertTrue(clean["mixing_gate"]["pass"])
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
