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


    def _head(self):
        proc = subprocess.run(["git", "-C", os.path.dirname(os.path.dirname(HERE)),
                               "rev-parse", "HEAD"],
                              capture_output=True, text=True)
        return proc.stdout.strip() or "unknown"

    def test_resume_refuses_a_run_recorded_under_the_wrong_order(self):
        """order_in_pair is planned here and recorded there; nothing compared them."""
        planned = os.path.join(self.out, "S01", "pair-01", "mcp")
        os.makedirs(planned, exist_ok=True)
        with open(os.path.join(planned, "metadata.json"), "w", encoding="utf-8") as fh:
            json.dump({"schema": 1, "scenario": "S01", "arm": "mcp", "pair": 1,
                       "order_in_pair": "second",  # the plan says first
                       "model_requested": "claude-fable-5",
                       "git_head": self._head()}, fh)
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a run recorded under the wrong order is not this cell's run:\n"
                            + proc.stdout + proc.stderr)

    def test_a_partial_run_directory_is_not_silently_overwritten(self):
        """A directory with a stream but no metadata.json went back on the queue.

        The next run writes straight over whatever is there, so a half-finished
        or foreign run disappears without anyone deciding that it should.
        """
        planned = os.path.join(self.out, "S01", "pair-01", "mcp")
        os.makedirs(planned, exist_ok=True)
        with open(os.path.join(planned, "stream.jsonl"), "w", encoding="utf-8") as fh:
            fh.write('{"type": "system", "subtype": "init", "model": "claude-fable-5"}\n')
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a partial run directory must be decided, not overwritten:\n"
                            + proc.stdout + proc.stderr)
        self.assertIn("S01/pair-01/mcp", proc.stdout + proc.stderr)

    def test_an_existing_manifest_that_is_not_this_matrix_is_not_overwritten(self):
        """The manifest is the tree's account of itself; it was rewritten blind."""
        with open(os.path.join(self.out, "manifest.json"), "w", encoding="utf-8") as fh:
            json.dump({"schema": 1, "stamp": "SOMEONE-ELSE", "model": "claude-other",
                       "git_head": "f" * 40, "total_runs": 210, "plan": []}, fh)
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a manifest describing another matrix must not be overwritten:\n"
                            + proc.stdout + proc.stderr)


    def _plant(self, **meta_extra):
        planned = os.path.join(self.out, "S01", "pair-01", "mcp")
        os.makedirs(planned, exist_ok=True)
        meta = {"schema": 1, "scenario": "S01", "arm": "mcp", "pair": 1,
                "order_in_pair": "first", "model_requested": "claude-fable-5",
                "model_resolved": "claude-fable-5", "git_head": self._head(),
                "invalid_reason": None}
        meta.update(meta_extra)
        with open(os.path.join(planned, "metadata.json"), "w", encoding="utf-8") as fh:
            json.dump(meta, fh)
        return planned

    def test_resume_refuses_a_run_with_no_stream(self):
        """Skipping claimed the run happened; nothing checked it produced one."""
        self._plant()
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a run with no stream.jsonl did not happen:\n"
                            + proc.stdout + proc.stderr)

    def test_resume_refuses_a_stream_that_names_another_model(self):
        planned = self._plant()
        with open(os.path.join(planned, "stream.jsonl"), "w", encoding="utf-8") as fh:
            fh.write('{"type": "system", "subtype": "init", "model": "claude-other"}\n')
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "the stream decides which model ran:\n"
                            + proc.stdout + proc.stderr)

    def test_an_existing_manifest_is_validated_in_full_not_by_three_fields(self):
        """stamp, model and head matched; the plan it declared did not."""
        stamp = os.path.basename(self.out)
        with open(os.path.join(self.out, "manifest.json"), "w", encoding="utf-8") as fh:
            json.dump({"schema": 1, "stamp": stamp, "model": "claude-fable-5",
                       "git_head": self._head(), "total_runs": 3,
                       "plan": [{"scenario": "S01", "pair": 1, "arm": "mcp",
                                 "order_in_pair": "first", "dir": "S01/pair-01/mcp"}]}, fh)
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a manifest whose plan is not this matrix's must not be "
                            "overwritten on the strength of three matching fields:\n"
                            + proc.stdout + proc.stderr)


if __name__ == "__main__":
    unittest.main()
