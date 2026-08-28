#!/usr/bin/env python3
"""Freeze / verify the sha256 of every FROZEN file in SPEC.txt section 1.

`--write` records the digests into freeze.json; `--check` compares and exits
non-zero on ANY drift, including a frozen file that appeared or disappeared.
Globs are resolved at run time, so scenario files that do not exist yet are
picked up by the next `--write` (SPEC section 6 freeze order: commit A pins the
whole set before a single confirmation run).
"""

from __future__ import annotations

import argparse
import glob
import hashlib
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
FREEZE_PATH = os.path.join(HERE, "freeze.json")

# Relative to this directory.  Order is irrelevant (the manifest is sorted).
FROZEN_PATTERNS = (
    "SPEC.txt",
    "taxonomy.json",
    "acceptance_table.json",
    "tango.py",
    "grade.py",
    "report.py",
    "run.sh",
    "sandbox.sh",
    "matrix.sh",
    "prompts/*.txt",
    "scenarios/*/prompt.txt",
    "scenarios/*/expect.py",
    "scenarios/*/seed.sh",
    "scenarios/*/meta.json",
)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def collect():
    """Map relative path -> sha256 for every currently existing frozen file."""
    manifest = {}
    for pattern in FROZEN_PATTERNS:
        for path in glob.glob(os.path.join(HERE, pattern)):
            if not os.path.isfile(path):
                continue
            rel = os.path.relpath(path, HERE)
            manifest[rel] = sha256_file(path)
    return dict(sorted(manifest.items()))


def write():
    payload = {"schema": 1, "patterns": list(FROZEN_PATTERNS), "files": collect()}
    with open(FREEZE_PATH, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print("wrote %s (%d files)" % (FREEZE_PATH, len(payload["files"])))
    return 0


def check():
    if not os.path.exists(FREEZE_PATH):
        print("freeze.json missing — run freeze.py --write", file=sys.stderr)
        return 1
    with open(FREEZE_PATH, "r", encoding="utf-8") as fh:
        recorded = json.load(fh)["files"]
    current = collect()
    problems = []
    for rel in sorted(set(recorded) | set(current)):
        want = recorded.get(rel)
        have = current.get(rel)
        if want is None:
            problems.append("NEW (not frozen):     %s" % rel)
        elif have is None:
            problems.append("MISSING:              %s" % rel)
        elif want != have:
            problems.append("CHANGED:              %s\n    frozen  %s\n    current %s"
                            % (rel, want, have))
    if problems:
        print("freeze check FAILED (%d problem(s)):" % len(problems), file=sys.stderr)
        for problem in problems:
            print("  " + problem, file=sys.stderr)
        print("\nIf the change is intentional, SPEC section 6 requires a new commit A'"
              " and a matrix restart from zero.", file=sys.stderr)
        return 1
    print("freeze check OK (%d files)" % len(current))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--write", action="store_true", help="record digests")
    group.add_argument("--check", action="store_true", help="verify digests")
    args = ap.parse_args(argv)
    return write() if args.write else check()


if __name__ == "__main__":
    sys.exit(main())
