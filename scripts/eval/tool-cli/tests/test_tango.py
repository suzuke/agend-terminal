#!/usr/bin/env python3
"""Property + cross-check tests for tango.py (SPEC.txt section 5).

The cross-check is deliberately an INDEPENDENT implementation: instead of the
closed-form restricted MLE it maximises the profile log-likelihood in p21 with
a 1-D golden-section search, then feeds that p21 into the same score formula.
If the algebra behind the closed form were wrong, the two would disagree.
"""

import json
import math
import os
import random
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, ROOT)

import tango  # noqa: E402

N = tango.DEFAULT_N
Z = tango.Z_ALPHA


# --------------------------------------------------------------------------
# independent numerical reference
# --------------------------------------------------------------------------

def _loglik(n, b, c, d, q):
    m = n - b - c
    p12 = q + d
    rest = 1.0 - 2.0 * q - d
    if p12 <= 0.0 or q <= 0.0 or rest <= 0.0:
        # terms with zero count contribute nothing; a zero probability with a
        # positive count is -inf.
        total = 0.0
        for count, p in ((b, p12), (c, q), (m, rest)):
            if count == 0:
                continue
            if p <= 0.0:
                return -math.inf
            total += count * math.log(p)
        return total
    return b * math.log(p12) + c * math.log(q) + m * math.log(rest)


def numeric_p21(n, b, c, d):
    """Golden-section maximisation of the profile log-likelihood in q = p21."""
    lo = max(0.0, -d)
    hi = (1.0 - d) / 2.0
    if hi <= lo:
        return max(lo, 0.0)
    invphi = (math.sqrt(5.0) - 1.0) / 2.0
    a, bnd = lo, hi
    x1 = bnd - invphi * (bnd - a)
    x2 = a + invphi * (bnd - a)
    f1, f2 = _loglik(n, b, c, d, x1), _loglik(n, b, c, d, x2)
    for _ in range(400):
        if f1 < f2:
            a, x1, f1 = x1, x2, f2
            x2 = a + invphi * (bnd - a)
            f2 = _loglik(n, b, c, d, x2)
        else:
            bnd, x2, f2 = x2, x1, f1
            x1 = bnd - invphi * (bnd - a)
            f1 = _loglik(n, b, c, d, x1)
        if bnd - a < 1e-14:
            break
    return (a + bnd) / 2.0


def numeric_z(n, b, c, d):
    q = numeric_p21(n, b, c, d)
    var = n * (2.0 * q + d * (1.0 - d))
    num = b - c - n * d
    if var <= 0.0:
        if abs(num) < 1e-12:
            return 0.0
        return math.inf if num > 0 else -math.inf
    return num / math.sqrt(var)


def numeric_ucb(n, b, c):
    lo = (b - c) / n
    hi = 1.0
    if numeric_z(n, b, c, hi) >= -Z:
        return hi
    for _ in range(200):
        mid = (lo + hi) / 2.0
        if mid <= lo or mid >= hi:
            break
        if numeric_z(n, b, c, mid) >= -Z:
            lo = mid
        else:
            hi = mid
    return lo


# --------------------------------------------------------------------------


class TangoProperties(unittest.TestCase):
    def test_z_at_ucb_hits_minus_z_alpha(self):
        for b in range(0, 40, 3):
            for c in range(0, 20, 3):
                ucb = tango.upper_bound(N, b, c)
                if ucb >= 1.0:
                    continue  # boundary cell: the root is not interior
                z = tango.score_z(N, b, c, ucb)
                self.assertAlmostEqual(z, -Z, delta=1e-6,
                                       msg="z(UCB) at (%d,%d) = %r" % (b, c, z))

    def test_ucb_at_least_delta_hat(self):
        for b in range(0, 30, 2):
            for c in range(0, 30, 2):
                self.assertGreaterEqual(tango.upper_bound(N, b, c) + 1e-12,
                                        (b - c) / N)

    def test_monotone_in_b_and_c(self):
        table = tango.load_table()
        cells = table["cells"]

        def accept(b, c):
            return cells["%d,%d" % (b, c)]["accept"]

        for b in range(0, N + 1):
            for c in range(0, N + 1 - b):
                if accept(b, c):
                    for bp in range(0, b):
                        self.assertTrue(accept(bp, c),
                                        "accept(%d,%d) but not (%d,%d)" % (b, c, bp, c))
                    for cp in range(c + 1, N + 1 - b):
                        self.assertTrue(accept(b, cp),
                                        "accept(%d,%d) but not (%d,%d)" % (b, c, b, cp))

    def test_ucb_monotone_increasing_in_b(self):
        for c in (0, 2, 5):
            prev = -2.0
            for b in range(0, 30):
                ucb = tango.upper_bound(N, b, c)
                self.assertGreater(ucb, prev - 1e-12)
                prev = ucb

    def test_symmetry_ucb_equals_negative_lcb_transposed(self):
        rng = random.Random(20260828)
        for _ in range(40):
            b = rng.randint(0, N)
            c = rng.randint(0, N - b)
            self.assertAlmostEqual(tango.upper_bound(N, b, c),
                                   -tango.lower_bound(N, c, b), places=9,
                                   msg="asymmetry at (%d,%d)" % (b, c))

    def test_cross_check_against_golden_section_mle(self):
        rng = random.Random(4242)
        cells = []
        while len(cells) < 25:
            b = rng.randint(0, N)
            c = rng.randint(0, N - b)
            if (b, c) not in cells:
                cells.append((b, c))
        for b, c in cells:
            for d in (0.0, 0.05, 0.1, 0.3):
                if d >= 1.0:
                    continue
                closed = tango.restricted_mle_p21(N, b, c, d)
                numeric = numeric_p21(N, b, c, d)
                self.assertAlmostEqual(closed, numeric, delta=1e-7,
                                       msg="p21~ mismatch at (%d,%d,d=%s)" % (b, c, d))
            self.assertAlmostEqual(tango.upper_bound(N, b, c), numeric_ucb(N, b, c),
                                   delta=1e-6, msg="UCB mismatch at (%d,%d)" % (b, c))

    def test_sanity_cells(self):
        table = tango.load_table()
        self.assertTrue(table["cells"]["0,0"]["accept"])
        self.assertFalse(table["cells"]["60,0"]["accept"])
        self.assertLessEqual(table["cells"]["0,0"]["ucb"], 0.10)
        self.assertAlmostEqual(table["cells"]["60,0"]["ucb"], 1.0, places=9)

    def test_delta_sign_convention_positive_means_cli_worse(self):
        # b = cli_only_fail pushes the bound up; c = mcp_only_fail pulls it down.
        self.assertGreater(tango.upper_bound(N, 5, 0), tango.upper_bound(N, 0, 5))
        self.assertLess(tango.upper_bound(N, 0, 5), 0.0)

    def test_table_metadata(self):
        table = tango.load_table()
        self.assertEqual(table["n"], 60)
        self.assertEqual(table["margin"], 0.10)
        self.assertEqual(table["alpha_one_sided"], 0.05)
        self.assertEqual(table["z"], 1.6448536)
        self.assertEqual(table["method"], "tango1998-score")
        self.assertEqual(table["delta_definition"], "fail_cli - fail_mcp")
        self.assertEqual(len(table["cells"]), (60 + 1) * (60 + 2) // 2)

    def test_table_file_matches_regeneration_byte_for_byte(self):
        with open(tango.TABLE_PATH, "rb") as fh:
            on_disk = fh.read()
        regenerated = tango.serialise(tango.build_table()).encode("utf-8")
        self.assertEqual(on_disk, regenerated,
                         "acceptance_table.json is stale — rerun tango.py --generate")

    def test_generator_sha256_matches_tango_py(self):
        table = tango.load_table()
        self.assertEqual(table["generator_sha256"],
                         tango.sha256_file(os.path.join(ROOT, "tango.py")))

    def test_selftest_entrypoint_exits_zero(self):
        proc = subprocess.run([sys.executable, os.path.join(ROOT, "tango.py"), "--selftest"],
                              capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_cli_single_cell_query(self):
        proc = subprocess.run(
            [sys.executable, os.path.join(ROOT, "tango.py"), "--b", "6", "--c", "3"],
            capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        out = json.loads(proc.stdout)
        self.assertAlmostEqual(out["ucb"], tango.upper_bound(60, 6, 3), places=12)
        self.assertAlmostEqual(out["delta_hat"], 0.05, places=12)


if __name__ == "__main__":
    unittest.main()
