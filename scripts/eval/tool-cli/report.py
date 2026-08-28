#!/usr/bin/env python3
"""summary.json -> report.txt, the human-readable Phase 0b pilot-safety sheet.

SPEC.txt section 4/9.  Reads only what grade.py --aggregate already decided;
this file never recomputes a gate.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

RULE = "=" * 78
THIN = "-" * 78


def _verdict(ok):
    return "PASS" if ok else "FAIL"


def _fmt(value, spec="%.6f"):
    return "n/a" if value is None else spec % value


def render(summary):
    out = []
    add = out.append
    add(RULE)
    add("tool-cli Phase 0b — PILOT SAFETY report")
    add("(pilot safety only — never a Phase 1 authorization; SPEC.txt section 9)")
    add(RULE)
    add("")
    add("OVERALL          %s" % _verdict(summary["pilot_safety"]))
    add("  rate_gate      %s" % _verdict(summary["rate_gate"]["pass"]))
    add("  critical_gate  %s" % _verdict(summary["critical_gate"]["pass"]))
    add("  mixing_gate    %s" % _verdict(summary["mixing_gate"]["pass"]))
    add("")
    add(THIN)
    add("PAIRED TABLE   delta = %s   (positive => CLI worse)" % summary["delta_definition"])
    add(THIN)
    cells = summary["cells"]
    add("  both_pass      %4d" % cells.get("both_pass", 0))
    add("  both_fail      %4d" % cells.get("both_fail", 0))
    add("  cli_only_fail  %4d   (b)" % summary["b"])
    add("  mcp_only_fail  %4d   (c)" % summary["c"])
    add("  N (valid pairs)%4d" % summary["n"])
    add("  delta_hat      %s" % _fmt(summary["delta_hat"], "%+.6f"))
    rate = summary["rate_gate"]
    add("  UCB(one-sided 95%%)  %s   margin %s   source %s"
        % (_fmt(rate["ucb"]), _fmt(rate["margin"], "%.2f"), rate["source"]))
    for flag in rate.get("flags", []):
        add("  !! %s" % flag)
    add("")
    add(THIN)
    add("CRITICAL CLASSES (zero tolerance, both arms, all valid runs)")
    add(THIN)
    crit = summary["critical_gate"]
    add("  total occurrences  %d" % crit["total"])
    if crit["by_class"]:
        for name, count in sorted(crit["by_class"].items()):
            add("    %-24s %d" % (name, count))
        for key, classes in sorted(crit["by_scenario_arm"].items()):
            add("    %-24s %s" % (key, json.dumps(classes, sort_keys=True)))
    else:
        add("    none")
    add("")
    add(THIN)
    add("MIXING GATE (target 0/45 per scenario)")
    add(THIN)
    for scenario, data in sorted(summary["mixing_gate"]["scenarios"].items()):
        add("  %-6s %d/%d mixing   valid_runs=%d   shortfall=%d"
            % (scenario, data["mixing"], data["expected_runs"],
               data["valid_runs"], data["runs_shortfall"]))
    add("")
    add(THIN)
    add("PER SCENARIO / ARM")
    add(THIN)
    add("  %-8s %-5s %6s %7s %7s %9s" % ("scenario", "arm", "runs", "passed", "failed", "critical"))
    for scenario, arms in sorted(summary["per_scenario"].items()):
        for arm, data in sorted(arms.items()):
            add("  %-8s %-5s %6d %7d %7d %9d"
                % (scenario, arm, data["runs"], data["passed"],
                   data["failed"], data["critical"]))
    add("")
    add(THIN)
    add("PER ARM")
    add(THIN)
    for arm, data in sorted(summary["per_arm"].items()):
        mean = summary["mean_tool_calls"].get(arm)
        add("  %-5s runs=%d failed=%d mean_tool_calls=%s"
            % (arm, data["runs"], data["failed"], _fmt(mean, "%.2f")))
    add("")
    add(THIN)
    add("INVALID RUNS (excluded from pairs)")
    add(THIN)
    if summary["invalid"]:
        for row in summary["invalid"]:
            add("  %-6s %-4s pair=%s  %-18s %s"
                % (row["scenario"], row["arm"], row["pair"], row["reason"], row["run_dir"]))
    else:
        add("  none")
    add("")
    add(THIN)
    add("ARTIFACT INDEX")
    add(THIN)
    add("  runs_dir      %s" % summary["runs_dir"])
    add("  summary.json  %s" % os.path.join(summary["runs_dir"], "summary.json"))
    add("  report.txt    %s" % os.path.join(summary["runs_dir"], "report.txt"))
    add("  manifest.json %s" % os.path.join(summary["runs_dir"], "manifest.json"))
    add("  per-run artifacts: <run_dir>/{metadata.json,seed.json,stream.jsonl,stderr.txt,run.log,final_state/}")
    add("  (no per-run grade.json: --aggregate grades in memory; grade.py <run_dir> writes one on demand)")
    add("  total_runs=%d  valid_runs=%d" % (summary["total_runs"], summary["valid_runs"]))
    return "\n".join(out) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description="render summary.json as report.txt")
    ap.add_argument("summary", help="path to summary.json")
    ap.add_argument("--out", help="output path (default: report.txt beside summary.json)")
    args = ap.parse_args(argv)

    with open(args.summary, "r", encoding="utf-8") as fh:
        summary = json.load(fh)
    text = render(summary)
    out = args.out or os.path.join(os.path.dirname(os.path.abspath(args.summary)), "report.txt")
    with open(out, "w", encoding="utf-8") as fh:
        fh.write(text)
    print("wrote %s" % out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
