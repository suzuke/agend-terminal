#!/usr/bin/env bash
# #3236 — the coverage producer/retry wrapper, extracted from the inline
# `Run coverage` step in .github/workflows/ci.yml so its failure semantics are
# testable (same pattern as fmt-owned.sh + test_fmt_owned.sh).
#
# THIS COMMIT IS THE RED CHECKPOINT: the body below is a faithful extraction of
# the behaviour as it shipped, so scripts/test_coverage_run.sh fails against it.
# The GREEN commit corrects the semantics; the extraction itself must not change
# behaviour, or the RED would be measuring the wrong thing.
#
# Seams (all default to the production values, so CI behaviour is unchanged):
#   COVERAGE_PRODUCER     command that produces coverage        [cargo llvm-cov …]
#   COVERAGE_CLEAN        cleanup command between attempts      [cargo llvm-cov clean --workspace]
#   COVERAGE_PROFILE_DIR  directory holding *.profraw           [target/llvm-cov-target]
#   COVERAGE_MAX_ATTEMPTS total producer executions             [3]
#   COVERAGE_LOG          per-attempt producer log              [cov-attempt.log]
set -o pipefail

producer="${COVERAGE_PRODUCER:-cargo llvm-cov -p agend-terminal --tests --features tray --lcov --output-path coverage.lcov}"
clean_cmd="${COVERAGE_CLEAN:-cargo llvm-cov clean --workspace}"
max_attempts="${COVERAGE_MAX_ATTEMPTS:-3}"
log="${COVERAGE_LOG:-cov-attempt.log}"

CORRUPTION_SIGNATURE='profdata|\.profraw|malformed instrumentation|raw profile|invalid instrumentation profile|failed to (load|merge).*profile|no profile can be merged'

attempt=1
while :; do
    if eval "$producer" 2>&1 | tee "$log"; then
        exit 0
    fi
    if [ "$attempt" -ge "$max_attempts" ]; then
        echo "::error::coverage failed after $((max_attempts - 1)) signature-gated profraw-flake retries"
        exit 1
    fi
    if grep -qiE "$CORRUPTION_SIGNATURE" "$log"; then
        echo "::warning::coverage attempt $attempt hit an llvm-cov profraw/profdata corruption flake; cleaning + retrying"
        eval "$clean_cmd" || true
        attempt=$((attempt + 1))
        continue
    fi
    echo "::error::coverage failed with a REAL test failure (no profraw-corruption signature) — NOT a flake; failing fast. See the 'test ... FAILED' above."
    exit 1
done
