"""matrix.sh authority: the plan is frozen, and resume must recognise what it skips.

Both cases are driven through the real script with `--dry-run`, which builds the
plan and does the resume bookkeeping without executing a single run.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

#: meta_extra sentinel: drop the key rather than set it.
DROP_SENTINEL = object()

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MATRIX = os.path.join(HERE, "matrix.sh")
sys.path.insert(0, HERE)
import grade  # noqa: E402  (the harness under test)

#: The release binaries a real matrix runs. A conforming run records their
#: digests, so the control below can only exist in a built tree.
RELEASE = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(HERE))),
                       "target", "release")
BUILT = all(os.path.exists(os.path.join(RELEASE, name))
            for name in ("agend-terminal", "agend-mcp-bridge"))
NEEDS_BUILD = "needs target/release binaries: a conforming run records their digests"


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
        scenarios = os.path.join(HERE, "scenarios")
        # A run this matrix would accept, so a probe can break exactly one field.
        meta = {"schema": 1, "scenario": "S01", "arm": "mcp", "pair": 1,
                "order_in_pair": "first", "model_requested": "claude-fable-5",
                "model_resolved": "claude-fable-5", "git_head": self._head(),
                "claude_version": "2.0.0-test",
                "binary_sha256": {name: grade.file_sha256(os.path.join(RELEASE, name))
                                  for name in ("agend-terminal", "agend-mcp-bridge")},
                "system_prompt_sha256": grade.frozen_system_prompt_digest("mcp"),
                "prompt_sha256": grade.file_sha256(
                    os.path.join(scenarios, "S01", "prompt.txt")),
                "fence": True, "fleet_sha256": grade.frozen_fleet_digest(),
                "seed_sha256": grade.file_sha256(
                    os.path.join(scenarios, "S01", "seed.sh")),
                "started_at": "2026-08-28T00:00:00Z", "ended_at": "2026-08-28T00:00:01Z",
                "duration_ms": 1000, "exit_code": 0, "turns": 3, "timed_out": False,
                # The execution budget is part of what makes a run THIS matrix's:
                # the grader refuses any other value, so a fixture that omits it
                # stopped modelling a conforming run the moment that became true.
                "max_turns": grade.FROZEN_MAX_TURNS,
                "timeout_secs": grade.FROZEN_TIMEOUT_SECS,
                "invalid_reason": None}
        meta.update(meta_extra)
        for key in [k for k, v in meta_extra.items() if v is DROP_SENTINEL]:
            meta.pop(key, None)
        with open(os.path.join(planned, "metadata.json"), "w", encoding="utf-8") as fh:
            json.dump(meta, fh)
        return planned

    def _plant_with_stream(self, **meta_extra):
        planned = self._plant(**meta_extra)
        # A run that really happened copied its final_state back; the grader now
        # refuses one that did not, so a fixture standing in for a COMPLETE run
        # has to carry the tree as well as the metadata.
        os.makedirs(os.path.join(planned, "final_state"), exist_ok=True)
        with open(os.path.join(planned, "stream.jsonl"), "w", encoding="utf-8") as fh:
            fh.write('{"type": "system", "subtype": "init", "model": "claude-fable-5",'
                     ' "claude_code_version": "2.0.0-test"}\n')
        return planned

    @unittest.skipUnless(BUILT, NEEDS_BUILD)
    def test_a_conforming_run_is_skipped(self):
        """The control the refusals are measured against.

        Without it "resume refused" proves only that something was wrong.
        """
        self._plant_with_stream()
        proc = dry_run(self.out)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("1 already complete", proc.stdout)

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


    def test_resume_validates_the_whole_metadata_contract(self):
        """Identity matched and the stream was there; the record was still partial.

        The planted run is built to pass every identity check resume already
        makes — it even carries the binaries from this matrix's own manifest —
        and differs only by a missing SPEC-required field.
        """
        first = dry_run(self.out)
        self.assertEqual(first.returncode, 0, first.stderr)
        with open(os.path.join(self.out, "manifest.json"), "r", encoding="utf-8") as fh:
            manifest = json.load(fh)
        planned = self._plant(turns=DROP_SENTINEL,
                              binary_sha256=manifest["binary_sha256"])
        with open(os.path.join(planned, "stream.jsonl"), "w", encoding="utf-8") as fh:
            fh.write('{"type": "system", "subtype": "init", "model": "claude-fable-5"}\n')

        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a run missing SPEC-required metadata did not complete:\n"
                            + proc.stdout + proc.stderr)


    def _valid_manifest(self, **overrides):
        """A manifest this matrix would accept, so a probe can break one field."""
        manifest = {
            "schema": 1, "stamp": os.path.basename(self.out),
            "created_at": "2026-08-28T00:00:00Z", "dry_run": False,
            "git_head": self._head(), "model": "claude-fable-5", "jobs": 3,
            "binary_sha256": {"agend-terminal": "0" * 64, "agend-mcp-bridge": "0" * 64},
            "prompt_sha256": grade.frozen_prompt_digests(),
            "fleet_sha256": grade.frozen_fleet_digest(),
            "seed_sha256": grade.frozen_seed_digests(os.path.join(HERE, "scenarios")),
            "missing_scenarios": [], "total_runs": 210,
            "plan": grade.frozen_plan_rows(),
        }
        manifest.update(overrides)
        for key in [k for k, v in overrides.items() if v is DROP_SENTINEL]:
            manifest.pop(key, None)
        with open(os.path.join(self.out, "manifest.json"), "w", encoding="utf-8") as fh:
            json.dump(manifest, fh)

    def test_a_manifest_with_the_wrong_seed_map_is_not_overwritten(self):
        """seed_sha256 is checked when grading and was not checked before writing.

        Everything else in this manifest is what the matrix would write, so the
        seed map is the only thing that can refuse it.
        """
        self._valid_manifest(seed_sha256={"S01": "e" * 64})
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a manifest whose seed map is not the frozen one must not "
                            "be overwritten:\n" + proc.stdout + proc.stderr)

    def test_a_manifest_with_no_seed_map_is_not_overwritten(self):
        self._valid_manifest(seed_sha256=DROP_SENTINEL)
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a manifest that states no seed identity must not be "
                            "overwritten:\n" + proc.stdout + proc.stderr)


    def test_a_manifest_with_other_binaries_is_not_overwritten(self):
        """The digests were checked for shape, never against the binaries here.

        A manifest can name two well-formed digests of somebody else's build and
        be written over as if it were this matrix's.
        """
        self._valid_manifest(binary_sha256={"agend-terminal": "a" * 64,
                                            "agend-mcp-bridge": "b" * 64})
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a manifest naming other binaries must not be overwritten:\n"
                            + proc.stdout + proc.stderr)
        self.assertIn("binary_sha256", proc.stdout + proc.stderr,
                      "the refusal must say which field it could not account for")


    @unittest.skipUnless(BUILT, NEEDS_BUILD)
    def test_resume_refuses_a_run_with_a_foreign_system_prompt(self):
        """Resume runs the grader's contract, so this must refuse there too.

        Everything else in the planted run is what this matrix would accept —
        the control above proves that — so the system prompt is the only thing
        that can refuse it.
        """
        self._plant_with_stream(system_prompt_sha256="f" * 64)
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0,
                            "a run given another system prompt is not this matrix's:\n"
                            + proc.stdout + proc.stderr)


    def test_a_manifest_with_another_jobs_setting_is_not_overwritten(self):
        """`jobs` was checked for being at least 1, never for being THIS matrix's.

        A tree written with a different parallelism is a different run of the
        matrix; overwriting its manifest hides that. The assertions read the
        REFUSAL TEXT rather than the exit code, because in an unbuilt worktree
        the guard also refuses on binary digests — naming the field is what
        distinguishes this check from that one.
        """
        self._valid_manifest(jobs=9)
        proc = dry_run(self.out)
        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("jobs", proc.stdout + proc.stderr,
                      "the refusal must name jobs when the manifest disagrees")

    def test_the_matrix_own_jobs_setting_is_not_a_complaint(self):
        """The control: the default this dry run uses must not be flagged."""
        self._valid_manifest(jobs=3)
        proc = dry_run(self.out)
        self.assertNotIn("jobs", proc.stdout + proc.stderr,
                         "jobs=3 is what this matrix runs; it must not be refused for it")


if __name__ == "__main__":
    unittest.main()
