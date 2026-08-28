"""matrix.sh authority: the plan is frozen, and resume must recognise what it skips.

Both cases are driven through the real script with `--dry-run`, which builds the
plan and does the resume bookkeeping without executing a single run.
"""
import json
import os
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MATRIX = os.path.join(HERE, "matrix.sh")


def dry_run(out_dir, env_extra=None):
    env = dict(os.environ)
    env.update(env_extra or {})
    return subprocess.run(["bash", MATRIX, "--dry-run", out_dir],
                          capture_output=True, text=True, env=env, cwd=HERE)


class MatrixAuthority(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="tclieval-matrix-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.out = os.path.join(self.tmp, "runs")
        os.makedirs(self.out, exist_ok=True)

    def test_the_plan_is_the_frozen_one_whatever_the_environment_says(self):
        """MATRIX_CONFIRMATION / MATRIX_MIXING used to replace the SPEC plan.

        Set them and the matrix plans whatever they say — including nothing at
        all, which still exits 0 and reports a complete-looking run of zero
        runs. A frozen plan that an environment variable can empty is not frozen.
        """
        proc = dry_run(self.out, {"MATRIX_CONFIRMATION": "S00-smoke",
                                  "MATRIX_MIXING": ""})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("total runs: 210", proc.stdout,
                      "the environment must not be able to change the frozen plan")

    def test_resume_refuses_a_run_directory_it_cannot_account_for(self):
        """A run dir with metadata.json was skipped without ever being read.

        Anything that puts a metadata.json in the right place — a stale tree from
        an older head, a copied directory, a hand-written file — is counted as a
        completed run of THIS matrix.
        """
        planned = os.path.join(self.out, "S01", "pair-01", "mcp")
        os.makedirs(planned, exist_ok=True)
        with open(os.path.join(planned, "metadata.json"), "w", encoding="utf-8") as fh:
            json.dump({"schema": 1, "scenario": "S01", "arm": "mcp", "pair": 1,
                       "model_requested": "claude-other",
                       "git_head": "f" * 40}, fh)

        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "resume must refuse a run dir that is not this matrix's:\n"
                            + proc.stdout + proc.stderr)
        self.assertIn("S01/pair-01/mcp", proc.stdout + proc.stderr,
                      "the refusal must name the directory it could not account for")


if __name__ == "__main__":
    unittest.main()
