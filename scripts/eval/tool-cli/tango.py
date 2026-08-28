#!/usr/bin/env python3
"""Tango (1998) score interval for the paired difference of failure rates.

SPEC.txt section 5.  Standard library only (no numpy / scipy) — the frozen
acceptance table must be reproducible from a bare python3.

Model
-----
Each pair contributes one cell of the 2x2 table

    n11 = both arms pass      n12 = b = CLI fails, MCP passes
    n21 = c = MCP fails, CLI passes   n22 = both arms fail

with N pairs total.  The estimand is

    delta = fail_CLI - fail_MCP = p12 - p21,   delta_hat = (b - c) / N

so a POSITIVE delta means the CLI arm is worse.  The sign is pinned by
``tests/test_tango.py`` and by ``grade.py``'s ``DELTA_DEFINITION``.

Restricted MLE
--------------
Profile the multinomial log-likelihood in q = p21 under H0: p12 - p21 = d
(p11 and p22 only enter through their sum, so they profile out):

    L(q) = b log(q + d) + c log(q) + (N - b - c) log(1 - 2q - d)

Setting L'(q) = 0 and clearing denominators gives the quadratic

    2 q^2 - B q - gamma d (1 - d) = 0
    B = alpha (1 - d) + gamma (1 - 3 d) - 2 mu d

with alpha = b/N, gamma = c/N, mu = 1 - alpha - gamma, whose admissible root is
the closed form of SPEC section 5:

    p21~ = (B + sqrt(B^2 + 8 gamma d (1 - d))) / 4

The score statistic is

    z(d) = (b - c - N d) / sqrt(N (2 p21~ + d (1 - d)))

and the one-sided upper bound is UCB = sup{ d >= delta_hat : z(d) >= -z_alpha },
found by bisection because z(d) is decreasing in d.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import sys

Z_ALPHA = 1.6448536
MARGIN = 0.10
DEFAULT_N = 60
METHOD = "tango1998-score"
DELTA_DEFINITION = "fail_cli - fail_mcp"

HERE = os.path.dirname(os.path.abspath(__file__))
TABLE_PATH = os.path.join(HERE, "acceptance_table.json")


def restricted_mle_p21(n: int, b: int, c: int, d: float) -> float:
    """Closed-form restricted MLE of p21 under H0: delta = d."""
    alpha = b / n
    gamma = c / n
    mu = 1.0 - alpha - gamma
    bb = alpha * (1.0 - d) + gamma * (1.0 - 3.0 * d) - 2.0 * mu * d
    disc = bb * bb + 8.0 * gamma * d * (1.0 - d)
    if disc < 0.0:
        disc = 0.0  # only reachable through float noise at the boundary
    q = (bb + math.sqrt(disc)) / 4.0
    if q < 0.0:
        q = 0.0
    # feasibility: p12 = q + d and p11 + p22 = 1 - 2q - d must stay in [0, 1]
    upper = (1.0 - d) / 2.0
    if q > upper:
        q = max(0.0, upper)
    return q


def score_z(n: int, b: int, c: int, d: float) -> float:
    """Tango score statistic at delta = d.  +/-inf for a degenerate variance."""
    q = restricted_mle_p21(n, b, c, d)
    var = n * (2.0 * q + d * (1.0 - d))
    num = b - c - n * d
    if var <= 0.0:
        if abs(num) < 1e-12:
            return 0.0
        return math.inf if num > 0 else -math.inf
    return num / math.sqrt(var)


def upper_bound(n: int, b: int, c: int, z_alpha: float = Z_ALPHA) -> float:
    """One-sided upper confidence bound on delta = fail_CLI - fail_MCP."""
    lo = (b - c) / n  # z(delta_hat) == 0 >= -z_alpha
    hi = 1.0
    if score_z(n, b, c, hi) >= -z_alpha:
        return hi
    for _ in range(200):
        mid = (lo + hi) / 2.0
        if mid <= lo or mid >= hi:
            break
        if score_z(n, b, c, mid) >= -z_alpha:
            lo = mid
        else:
            hi = mid
    return lo


def lower_bound(n: int, b: int, c: int, z_alpha: float = Z_ALPHA) -> float:
    """One-sided lower confidence bound; used only by the symmetry property."""
    hi = (b - c) / n
    lo = -1.0
    if score_z(n, b, c, lo) <= z_alpha:
        return lo
    for _ in range(200):
        mid = (lo + hi) / 2.0
        if mid <= lo or mid >= hi:
            break
        if score_z(n, b, c, mid) <= z_alpha:
            hi = mid
        else:
            lo = mid
    return hi


def cell(n: int, b: int, c: int, margin: float = MARGIN) -> dict:
    ucb = upper_bound(n, b, c)
    return {"ucb": ucb, "accept": ucb <= margin}


def build_table(n: int = DEFAULT_N, margin: float = MARGIN) -> dict:
    cells = {}
    for b in range(n + 1):
        for c in range(n + 1 - b):
            cells["%d,%d" % (b, c)] = cell(n, b, c, margin)
    return {
        "n": n,
        "margin": margin,
        "alpha_one_sided": 0.05,
        "z": Z_ALPHA,
        "method": METHOD,
        "delta_definition": DELTA_DEFINITION,
        "cells": cells,
        "generator_sha256": sha256_file(os.path.join(HERE, "tango.py")),
    }


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def serialise(table: dict) -> str:
    """Canonical, byte-stable encoding of the table."""
    return json.dumps(table, sort_keys=True, separators=(",", ":")) + "\n"


def load_table(path: str = TABLE_PATH) -> dict:
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def selftest() -> int:
    n = DEFAULT_N
    problems = []
    for b, c in [(0, 0), (3, 0), (6, 0), (6, 3), (12, 6), (60, 0)]:
        ucb = upper_bound(n, b, c)
        if ucb < 1.0:
            z = score_z(n, b, c, ucb)
            if abs(z + Z_ALPHA) > 1e-6:
                problems.append("z(UCB) off for (%d,%d): %.9f" % (b, c, z))
        if ucb < (b - c) / n - 1e-12:
            problems.append("UCB below delta_hat for (%d,%d)" % (b, c))
        print("b=%2d c=%2d  delta_hat=%+.6f  ucb=%.6f  accept=%s"
              % (b, c, (b - c) / n, ucb, ucb <= MARGIN))
    if problems:
        for p in problems:
            print("FAIL: " + p, file=sys.stderr)
        return 1
    print("selftest OK")
    return 0


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--generate", action="store_true",
                    help="write acceptance_table.json for N=%d" % DEFAULT_N)
    ap.add_argument("--selftest", action="store_true", help="run internal sanity checks")
    ap.add_argument("--n", type=int, default=DEFAULT_N)
    ap.add_argument("--b", type=int)
    ap.add_argument("--c", type=int)
    args = ap.parse_args(argv)

    if args.generate:
        text = serialise(build_table(args.n))
        with open(TABLE_PATH, "w", encoding="utf-8") as fh:
            fh.write(text)
        print("wrote %s (%d cells)" % (TABLE_PATH, (args.n + 1) * (args.n + 2) // 2))
        return 0
    if args.selftest:
        return selftest()
    if args.b is not None and args.c is not None:
        out = cell(args.n, args.b, args.c)
        out["n"] = args.n
        out["b"] = args.b
        out["c"] = args.c
        out["delta_hat"] = (args.b - args.c) / args.n
        print(json.dumps(out, sort_keys=True))
        return 0
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
