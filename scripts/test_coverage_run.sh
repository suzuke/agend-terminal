#!/usr/bin/env bash
# #3236 — behavioural contract for scripts/coverage-run.sh.
#
# Scope (decision d-20260812161748154106-7): failure precedence, cleanup
# truthfulness, attempt isolation, bounded diagnostics. These tests say NOTHING
# about the corrupt-profraw writer itself, which is unresolved — they only pin
# how the wrapper must BEHAVE when corruption is present.
#
# Usage: ./scripts/test_coverage_run.sh   (exit 0 on all-pass, 1 otherwise)

set -uo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
wrapper="$script_dir/coverage-run.sh"

pass=0
fail=0

report() {
    local ok="$1" label="$2" detail="${3:-}"
    if [ "$ok" -eq 0 ]; then
        echo "PASS  $label"
        pass=$((pass + 1))
    else
        echo "FAIL  $label"
        [ -n "$detail" ] && echo "      $detail"
        fail=$((fail + 1))
    fi
}

new_sandbox() {
    local dir
    dir="$(mktemp -d "${TMPDIR:-/tmp}/cov-run-test.XXXXXX")"
    mkdir -p "$dir/profiles"
    echo "$dir"
}

# ── 1. Failure precedence ────────────────────────────────────────────────────
# A producer that fails with BOTH a real `test … FAILED` and a profraw mention
# is a REAL failure. It must not be retried and must not be labelled a flake:
# the corruption signature must never outrank an observed test failure.
test_real_failure_wins_over_corruption_signature() {
    local sandbox out rc
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
echo "attempt-marker" >> "$COV_TEST_ATTEMPTS"
echo "test some::real::case ... FAILED"
echo "warning: /t/agend-terminal-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)"
exit 101
PRODUCER
    chmod +x "$sandbox/producer.sh"
    : >"$sandbox/attempts"
    out="$(cd "$sandbox" && COV_TEST_ATTEMPTS="$sandbox/attempts" \
        COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" \
        COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" \
        "$wrapper" 2>&1)"
    rc=$?
    local attempts
    attempts="$(wc -l <"$sandbox/attempts" | tr -d ' ')"

    # Match a POSITIVE flake claim only — the truthful message legitimately
    # contains the word "flake" in "NOT a flake".
    if [ "$attempts" -ne 1 ]; then
        report 1 "real failure is not retried" "producer ran $attempts times, expected 1"
    elif echo "$out" | grep -qiE "hit .*flake|flake retries|is a flake|corruption flake"; then
        report 1 "real failure is not relabelled as a flake" "output claimed a flake: $(echo "$out" | grep -iE 'hit .*flake|flake retries|is a flake|corruption flake' | head -1)"
    elif [ "$rc" -ne 101 ]; then
        report 1 "real failure preserves the producer exit code" "wrapper exited $rc, producer exited 101"
    else
        report 0 "real failure wins over the corruption signature"
    fi
    rm -rf "$sandbox"
}

# ── 2. Cleanup truthfulness ──────────────────────────────────────────────────
# A cleanup that fails must be surfaced. Swallowing it (`|| true`) leaves the
# next attempt running against state nobody verified was cleaned.
test_cleanup_failure_is_surfaced() {
    local sandbox out rc
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="false" \
        COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" \
        "$wrapper" 2>&1)"
    rc=$?

    if [ "$rc" -eq 0 ]; then
        report 1 "cleanup failure is surfaced" "wrapper exited 0 despite a failing cleanup"
    elif ! echo "$out" | grep -qi "cleanup"; then
        report 1 "cleanup failure is surfaced" "no cleanup failure reported in output"
    else
        report 0 "cleanup failure is surfaced, not swallowed"
    fi
    rm -rf "$sandbox"
}

# ── 3. Attempt isolation ─────────────────────────────────────────────────────
# A retry must not be able to consume a prior attempt's profraw. The wrapper is
# responsible for ensuring attempt N+1 starts from a profile directory that
# contains nothing attempt N produced.
test_retry_cannot_consume_prior_attempt_profraw() {
    local sandbox out
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
n="$(cat "$COV_TEST_ATTEMPTS")"
n=$((n + 1)); echo "$n" > "$COV_TEST_ATTEMPTS"
if [ "$n" -gt 1 ]; then
  # Record whatever the previous attempt left behind.
  ls "$COVERAGE_PROFILE_DIR" > "$COV_TEST_SEEN" 2>/dev/null || true
fi
printf 'stale' > "$COVERAGE_PROFILE_DIR/agend-terminal-$n-deadbeef_0.profraw"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    echo 0 >"$sandbox/attempts"
    : >"$sandbox/seen"
    out="$(cd "$sandbox" && COV_TEST_ATTEMPTS="$sandbox/attempts" COV_TEST_SEEN="$sandbox/seen" \
        COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" \
        COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" \
        "$wrapper" 2>&1)"

    if [ -s "$sandbox/seen" ]; then
        report 1 "retry cannot consume prior-attempt profraw" \
            "attempt 2 saw: $(tr '\n' ' ' <"$sandbox/seen")"
    else
        report 0 "retry cannot consume prior-attempt profraw"
    fi
    rm -rf "$sandbox"
}

# ── 4. Bounded diagnostics ───────────────────────────────────────────────────
# On a corrupt/no-profile failure the wrapper must emit deterministic evidence
# adequate to continue the RCA — attempt number, the profile path, per-file size
# and header bytes — and must NOT dump the producer log wholesale.
test_corrupt_failure_emits_bounded_diagnostics() {
    local sandbox out
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'CORRUPTHEADER' > "$COVERAGE_PROFILE_DIR/agend-terminal-4242-cafebabe_0.profraw"
# a deliberately noisy log the wrapper must not echo back in full
i=0; while [ "$i" -lt 400 ]; do echo "noise line $i"; i=$((i + 1)); done
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" \
        COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" \
        COVERAGE_MAX_ATTEMPTS=1 \
        "$wrapper" 2>&1)"

    local missing=""
    echo "$out" | grep -q "agend-terminal-4242-cafebabe_0.profraw" || missing="$missing filename"
    echo "$out" | grep -qiE "size|bytes" || missing="$missing size"
    echo "$out" | grep -qiE "header" || missing="$missing header"
    echo "$out" | grep -qiE "attempt" || missing="$missing attempt"
    # The producer's own output is streamed once by design (CI needs progress).
    # What must stay bounded is the DIAGNOSTIC block: it must not re-dump the log.
    local diag_noise
    diag_noise="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p' | grep -c "noise line")"

    if [ -n "$missing" ]; then
        report 1 "corrupt failure emits bounded diagnostics" "missing evidence:$missing"
    elif [ "$diag_noise" -gt 0 ]; then
        report 1 "diagnostics stay bounded" "diagnostic block re-dumped $diag_noise producer log lines"
    else
        report 0 "corrupt failure emits bounded, deterministic diagnostics"
    fi
    rm -rf "$sandbox"
}

# ── 5. Named-corrupt relevance ───────────────────────────────────────────────
# #3236 (run 31619505420): the ordinary listing is capped in GLOB order, so the
# one path llvm-profdata explicitly named as corrupt fell outside the cap and
# the evidence block showed ten unrelated valid files instead. A path the
# producer NAMES must be described regardless of where it sorts.
test_named_corrupt_path_is_cap_exempt() {
    local sandbox out rc corrupt
    sandbox="$(new_sandbox)"
    corrupt="agend-terminal-56911-14119548425640577428_0.profraw"
    # Twelve valid files that all sort BEFORE the named one, so a glob-ordered
    # cap of 10 can never reach it.
    local i
    for i in 01 02 03 04 05 06 07 08 09 10 11 12; do
        printf 'valid-%s' "$i" >"$sandbox/profiles/agend-terminal-1$i-999$i""_0.profraw"
    done
    printf 'partial' >"$sandbox/profiles/$corrupt"
    printf '%s\n' "$sandbox/profiles/$corrupt" >"$sandbox/profiles/agend-terminal-profraw-list"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $sandbox/profiles/$corrupt: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" \
        COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" \
        COVERAGE_MAX_ATTEMPTS=1 \
        "$wrapper" 2>&1)"
    rc=$?

    local block line missing=""
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -F "$corrupt" | grep -E '^\s*corrupt=' | head -n 1)"
    [ -n "$line" ] || missing="$missing named-path"
    if [ -n "$line" ]; then
        echo "$line" | grep -q 'exists=' || missing="$missing exists"
        echo "$line" | grep -q 'in_response=' || missing="$missing response-membership"
        echo "$line" | grep -q 'size_bytes=' || missing="$missing size"
        echo "$line" | grep -q 'header=' || missing="$missing header"
        echo "$line" | grep -q 'mtime=' || missing="$missing mtime"
        echo "$line" | grep -q 'module=' || missing="$missing module-token"
    fi
    # It must come BEFORE the ordinary capped listing, not after it.
    local corrupt_at file_at
    corrupt_at="$(echo "$block" | grep -nE '^\s*corrupt=' | head -n 1 | cut -d: -f1)"
    file_at="$(echo "$block" | grep -nE '^\s*file=' | head -n 1 | cut -d: -f1)"
    if [ -n "$corrupt_at" ] && [ -n "$file_at" ] && [ "$corrupt_at" -gt "$file_at" ]; then
        missing="$missing precedes-cap"
    fi

    if [ "$rc" -eq 0 ]; then
        report 1 "named corrupt path is cap-exempt" "wrapper exited 0 on a corrupt failure"
    elif [ -n "$missing" ]; then
        report 1 "named corrupt path is cap-exempt" "missing:$missing"
    else
        report 0 "named corrupt path is described despite the glob-ordered cap"
    fi
    rm -rf "$sandbox"
}

test_real_failure_wins_over_corruption_signature
test_cleanup_failure_is_surfaced
test_retry_cannot_consume_prior_attempt_profraw
test_corrupt_failure_emits_bounded_diagnostics
test_named_corrupt_path_is_cap_exempt

echo
echo "coverage-run contract: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
