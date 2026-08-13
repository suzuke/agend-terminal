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
skip=0

# A property this platform cannot exercise is SKIPPED, never silently passed:
# a green count that includes an unverifiable property is a false green.
report_skip() {
    echo "SKIP  $1"
    [ -n "${2:-}" ] && echo "      $2"
    skip=$((skip + 1))
}

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

# ── 6-8. Named-path resolution and membership exactness ──────────────────────
# Helper: run the wrapper over a sandbox whose producer names $1 verbatim.
# $2..: extra files to create in profiles/. Echoes the corrupt= line.
run_named_case() {
    local named="$1" sandbox out
    shift
    sandbox="$(new_sandbox)"
    local f
    for f in "$@"; do
        printf 'partial' >"$sandbox/profiles/$f"
    done
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $named: invalid instrumentation profile data (file header is corrupt)"
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
    echo "$out" | grep -E '^\s*corrupt=' | head -n 1
    rm -rf "$sandbox"
}

# drive_named with an extra PATH prefix (for command seams).
drive_named_with_path() {
    local prefix="$1" profiles="$2" named="$3" sandbox out
    sandbox="$(dirname "$profiles")"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $named: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && PATH="$prefix:$PATH" COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    echo "$out" | grep -E '^\s*corrupt=' | head -n 1
}

# ── 19-20. Delimiter-free transport and pinned reads (R5) ───────────────────

# A path may legitimately contain a tab; encoding two paths into one
# tab-delimited string makes such a profile unreportable.
test_tab_containing_path_is_reported() {
    local sandbox line name
    sandbox="$(new_sandbox)"
    name="$(printf 'odd\tname-1-2_0.profraw')"
    printf 'partial' >"$sandbox/profiles/$name"
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/$name")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=yes'; then
        report 0 "a path containing a tab is still reported"
    else
        report 1 "a path containing a tab is still reported" "got: $line"
    fi
}

# Validation and the metadata reads must not be separately racey: once the
# target is validated, the bytes reported must come from THAT object even if the
# name is swapped immediately afterwards.
test_reads_are_pinned_against_post_validation_swap() {
    local sandbox line outside shim victim
    sandbox="$(new_sandbox)"
    outside="$sandbox/outside-marker.bin"
    printf 'OUTSIDE-SWAPPED' >"$outside"
    victim="$sandbox/profiles/victim-2-3_0.profraw"
    printf 'INNOCENT' >"$victim"
    shim="$sandbox/shim"
    mkdir -p "$shim"
    # `wc` appears only in the READ phase (the size measurement), never during
    # validation, so a swap here lands strictly between the two. `tr` would be
    # too early: normalize_path uses it while validating.
    cat >"$shim/wc" <<SHIM
#!/usr/bin/env bash
if [ ! -e "$sandbox/.swapped" ]; then
    : >"$sandbox/.swapped"
    rm -f "$victim"
    ln -s "$outside" "$victim" 2>/dev/null
fi
exec /usr/bin/wc "\$@"
SHIM
    chmod +x "$shim/wc"
    if ! ln -s "$outside" "$sandbox/.probe" 2>/dev/null || [ ! -L "$sandbox/.probe" ]; then
        rm -rf "$sandbox"
        report_skip "reads are pinned against a post-validation swap" \
            "this platform does not create real symlinks; premise unavailable"
        return
    fi
    line="$(drive_named_with_path "$shim" "$sandbox/profiles" "$victim")"
    rm -rf "$sandbox"
    # 4f 55 54 53 49 44 45 2d = "OUTSIDE-"
    if echo "$line" | grep -q '4f 55 54 53 49 44 45 2d'; then
        report 1 "reads are pinned against a post-validation swap" "outside bytes disclosed: $line"
    else
        report 0 "reads are pinned against a post-validation swap"
    fi
}

# ── 21-23. Fabricated facts, temp-path trust, unparseable names (R6) ────────

# A failed pinned read must never be dressed up as a successful one.
test_failed_pinned_read_is_not_reported_as_success() {
    local sandbox line shim
    sandbox="$(new_sandbox)"
    printf 'REALDATA' >"$sandbox/profiles/fail-1-2_0.profraw"
    shim="$sandbox/shim"
    mkdir -p "$shim"
    cat >"$shim/cat" <<'SHIM'
#!/usr/bin/env bash
exit 1
SHIM
    chmod +x "$shim/cat"
    line="$(drive_named_with_path "$shim" "$sandbox/profiles" "$sandbox/profiles/fail-1-2_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=yes' && echo "$line" | grep -q 'size_bytes=0'; then
        report 1 "a failed pinned read is not reported as success" "fabricated: $line"
    else
        report 0 "a failed pinned read is not reported as success"
    fi
}

# The scratch path must be a real, private regular file — never followed.
test_temp_path_is_not_followed() {
    local sandbox line shim outside
    sandbox="$(new_sandbox)"
    outside="$sandbox/sentinel.bin"
    printf 'SENTINEL' >"$outside"
    printf 'PROFILEBYTES' >"$sandbox/profiles/tmp-3-4_0.profraw"
    shim="$sandbox/shim"
    mkdir -p "$shim"
    ln -s "$outside" "$sandbox/evil-temp" 2>/dev/null
    if [ ! -L "$sandbox/evil-temp" ]; then
        rm -rf "$sandbox"
        report_skip "the scratch path is not followed" \
            "this platform does not create real symlinks; premise unavailable"
        return
    fi
    cat >"$shim/mktemp" <<SHIM
#!/usr/bin/env bash
echo "$sandbox/evil-temp"
SHIM
    chmod +x "$shim/mktemp"
    line="$(drive_named_with_path "$shim" "$sandbox/profiles" "$sandbox/profiles/tmp-3-4_0.profraw")"
    local sentinel
    sentinel="$(cat "$outside" 2>/dev/null)"
    rm -rf "$sandbox"
    if [ "$sentinel" = "SENTINEL" ]; then
        report 0 "the scratch path is not followed"
    else
        report 1 "the scratch path is not followed" "sentinel overwritten; line: $line"
    fi
}

# A named path the parser cannot represent must be DISCLOSED as unparseable,
# never silently dropped.
test_unparseable_named_path_is_disclosed() {
    local sandbox out
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'warning: /tmp/odd\nname-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)\n'
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    rm -rf "$sandbox"
    if echo "$out" | grep -qE 'corrupt=|unparseable'; then
        report 0 "an unparseable named path is disclosed, not dropped"
    else
        report 1 "an unparseable named path is disclosed, not dropped" \
            "block claimed the producer named nothing"
    fi
}

# ── 46-47. A non-regular object must never block the wrapper ────────────────
# Opening a FIFO for reading blocks until a writer appears. Every read here is
# on a producer-controlled path, so a `*.profraw` or `*-profraw-list` FIFO hangs
# the wrapper forever: in CI that is a silent step timeout with the evidence
# block truncated mid-print — strictly worse than a failure, and precisely the
# unreadable failure this wrapper exists to remove.

# Run the wrapper under a hard deadline. A test for a hang must not itself hang:
# if the process is still alive at the deadline it is BLOCKED, and we release it
# by OPENING each FIFO FOR WRITING — that completes the pending open-for-read, so
# the wrapper finishes and no orphan is left behind. Safe precisely because we
# only do it while a blocked reader is known to exist.
run_wrapper_with_deadline() {
    local sandbox="$1" secs="$2" pid waited=0 blocked=0 p
    (
        cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
            COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
            COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 \
            "$wrapper" >"$sandbox/out" 2>&1
    ) &
    pid=$!
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt "$secs" ]; do
        sleep 1
        waited=$((waited + 1))
    done
    while kill -0 "$pid" 2>/dev/null; do
        blocked=1
        for p in "$sandbox/profiles"/*; do
            [ -p "$p" ] || continue
            ( exec 3>"$p"; exec 3>&- ) 2>/dev/null
        done
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -gt $((secs * 4)) ]; then
            kill -9 "$pid" 2>/dev/null
            break
        fi
    done
    wait "$pid" 2>/dev/null
    printf '%s' "$blocked"
}

# `mkfifo` is absent or a no-op on some platforms; then the premise, not the
# property, is unavailable.
fifos_unavailable() {
    mkfifo "$1/fifo-probe" 2>/dev/null || return 0
    [ -p "$1/fifo-probe" ] || { rm -f "$1/fifo-probe"; return 0; }
    rm -f "$1/fifo-probe"
    return 1
}

test_named_fifo_does_not_block_the_wrapper() {
    local sandbox blocked out bad=""
    sandbox="$(new_sandbox)"
    if fifos_unavailable "$sandbox"; then
        rm -rf "$sandbox"
        report_skip "a producer-named FIFO does not block the wrapper" \
            "this platform does not create FIFOs; premise unavailable"
        return
    fi
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
mkfifo "$COVERAGE_PROFILE_DIR/fifo-1-1_0.profraw"
printf 'warning: %s/fifo-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    blocked="$(run_wrapper_with_deadline "$sandbox" 5)"
    out="$(cat "$sandbox/out" 2>/dev/null)"
    rm -f "$sandbox/profiles"/*.profraw 2>/dev/null
    rm -rf "$sandbox"
    [ "$blocked" = "0" ] || bad="$bad blocked-on-open"
    # It must still finish the block it started — a truncated group is the CI
    # symptom that makes a hang worse than a failure.
    echo "$out" | grep -q '::endgroup::' || bad="$bad block-truncated"
    # And describe the object honestly rather than claiming bytes it never read.
    echo "$out" | grep -qE '^[[:space:]]*corrupt=fifo-1-1_0\.profraw' || bad="$bad not-described"
    echo "$out" | grep -q 'exists=not-a-regular-file' || bad="$bad not-labelled-non-regular"
    if [ -n "$bad" ]; then
        report 1 "a producer-named FIFO does not block the wrapper" \
            "issues:$bad; got: $(echo "$out" | grep -E '^[[:space:]]*corrupt=' | head -1)"
    else
        report 0 "a producer-named FIFO does not block the wrapper"
    fi
}

# The same hazard on the response file, whose NAME is producer-controlled by
# glob: the count, the fragment listing and the membership read all open it.
test_fifo_response_file_does_not_block_the_wrapper() {
    local sandbox blocked out bad=""
    sandbox="$(new_sandbox)"
    if fifos_unavailable "$sandbox"; then
        rm -rf "$sandbox"
        report_skip "a FIFO response file does not block the wrapper" \
            "this platform does not create FIFOs; premise unavailable"
        return
    fi
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/real-1-1_0.profraw"
mkfifo "$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
printf 'warning: %s/real-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    blocked="$(run_wrapper_with_deadline "$sandbox" 5)"
    out="$(cat "$sandbox/out" 2>/dev/null)"
    rm -f "$sandbox/profiles"/* 2>/dev/null
    rm -rf "$sandbox"
    [ "$blocked" = "0" ] || bad="$bad blocked-on-open"
    echo "$out" | grep -q '::endgroup::' || bad="$bad block-truncated"
    # The real profile alongside it must still be described.
    echo "$out" | grep -qE '^[[:space:]]*corrupt=real-1-1_0\.profraw' || bad="$bad real-path-lost"
    if [ -n "$bad" ]; then
        report 1 "a FIFO response file does not block the wrapper" "issues:$bad"
    else
        report 0 "a FIFO response file does not block the wrapper"
    fi
}

# ── 44-45. Isolation must fail closed when its own commands fail ────────────
# The permission precheck only covers ONE way enumeration can fail. `rm`'s exit
# status was ignored outright, and `find … | wc -l` discards find's status: a
# failed enumeration produced a numeric 0, indistinguishable from "nothing
# left", so the wrapper retried against state nobody verified. Each command is
# failed INDEPENDENTLY as well as together, so the status attribution is real
# rather than one check masking the other.
#
# `PATH` shims, not permissions: this is about command failure, and it must hold
# on platforms where mode bits do not stop `rm`.
drive_isolation_with_failing() {
    local which="$1" sandbox out rc attempts seen
    sandbox="$(new_sandbox)"
    mkdir -p "$sandbox/shim"
    case "$which" in
        rm | both) printf '#!/usr/bin/env bash\nexit 1\n' >"$sandbox/shim/rm" ;;
    esac
    case "$which" in
        find | both) printf '#!/usr/bin/env bash\nexit 1\n' >"$sandbox/shim/find" ;;
    esac
    chmod +x "$sandbox/shim"/* 2>/dev/null
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
n=$(cat "$COV_ATT" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" >"$COV_ATT"
if [ "$n" -gt 1 ]; then ls "$COVERAGE_PROFILE_DIR" >"$COV_SEEN" 2>/dev/null; fi
printf 'stale' >"$COVERAGE_PROFILE_DIR/stale-1-1_0.profraw"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    echo 0 >"$sandbox/att"
    : >"$sandbox/seen"
    out="$(cd "$sandbox" && PATH="$sandbox/shim:$PATH" \
        COV_ATT="$sandbox/att" COV_SEEN="$sandbox/seen" \
        COVERAGE_PRODUCER="$sandbox/producer.sh" COVERAGE_CLEAN="true" \
        COVERAGE_PROFILE_DIR="$sandbox/profiles" COVERAGE_LOG="$sandbox/cov.log" \
        COVERAGE_MAX_ATTEMPTS=2 "$wrapper" 2>&1)"
    rc=$?
    attempts="$(cat "$sandbox/att")"
    seen="$(tr '\n' ' ' <"$sandbox/seen")"
    rm -rf "$sandbox"
    printf '%s|%s|%s|%s' "$attempts" "$rc" "$seen" "$(echo "$out" | grep -c 'cannot isolate')"
}

test_isolation_fails_closed_when_its_commands_fail() {
    local which res attempts rc seen isolated bad=""
    for which in rm find both; do
        res="$(drive_isolation_with_failing "$which")"
        attempts="${res%%|*}"; res="${res#*|}"
        rc="${res%%|*}"; res="${res#*|}"
        seen="${res%%|*}"; isolated="${res#*|}"
        # It must stop at isolation: attempt 2 must never run, so it can never
        # observe what attempt 1 left behind.
        [ "$attempts" = "1" ] || bad="$bad [$which]ran-$attempts-attempts"
        [ -n "$seen" ] && bad="$bad [$which]attempt2-saw($seen)"
        [ "$rc" -ne 0 ] || bad="$bad [$which]exited-zero"
        [ "$isolated" != "0" ] || bad="$bad [$which]no-isolation-error"
    done
    if [ -n "$bad" ]; then
        report 1 "isolation fails closed when rm or find fails" "issues:$bad"
    else
        report 0 "isolation fails closed when rm or find fails"
    fi
}

# ── 41-43. The corruption-phrase population (d-…211213152750-12) ────────────
# The parser anchors on a `warning:` line; the warning COUNT deliberately does
# not, so that a name containing a newline — which splits the producer's line
# and leaves the phrase stranded — is still counted and disclosed. That only
# works if the phrases themselves are exact llvm-profdata messages. The bare
# two-word alternative `raw profile` matched ordinary prose, so the two
# populations disagreed and the accounting invented a path.
#
# Message set verified against the shipped binary, not from memory:
#   strings "$(xcrun --find llvm-profdata)" | grep -E '^(raw|empty raw|malformed|invalid|truncated) '
#     empty raw profile file
#     invalid instrumentation profile data (bad magic)
#     invalid instrumentation profile data (file header is corrupt)
#     malformed instrumentation profile data
#     raw profile version mismatch
#     truncated profile data

# A newline-bearing pathname is arbitrary bytes, so a path FRAGMENT may itself
# contain an exact corruption phrase. The phrase then matches on two lines — the
# path fragment and the real message — for ONE logical warning, and any exact
# tally derived from line counts is wrong. Line framing cannot tell pathname
# bytes from message bytes, so the honest disclosure is boolean: something was
# unattributable, without claiming how many.
test_phrase_bearing_path_fragment_claims_no_count() {
    local sandbox out block bad="" markers
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'warning: /tmp/odd-invalid instrumentation profile data-\nname-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)\n'
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    markers="$(echo "$block" | grep -c 'corrupt=(unparseable)')"
    rm -rf "$sandbox"
    # Exactly ONE bounded generic marker for the one logical warning.
    [ "$markers" -eq 1 ] || bad="$bad expected-1-marker-got-$markers"
    echo "$block" | grep -q 'count=' && bad="$bad asserts-exact-count"
    if [ -n "$bad" ]; then
        report 1 "a phrase-bearing path fragment claims no count" \
            "issues:$bad; got: $(echo "$block" | grep 'unparseable' | head -1)"
    else
        report 0 "a phrase-bearing path fragment claims no count"
    fi
}

# Benign prose that merely mentions raw profiles, beside a real parsed warning.
test_benign_raw_profile_prose_fabricates_no_record() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'x' >"$COVERAGE_PROFILE_DIR/ok-1-1_0.profraw"
printf 'warning: %s/ok-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "note: merging raw profile data from 4 inputs"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    # Premise: the one real warning must have been parsed and described.
    echo "$block" | grep -q 'corrupt=ok-1-1_0.profraw' || bad="$bad premise-not-described"
    echo "$block" | grep -q 'unparseable' && bad="$bad fabricated-unparseable"
    if [ -n "$bad" ]; then
        report 1 "benign raw-profile prose fabricates no record" "issues:$bad"
    else
        report 0 "benign raw-profile prose fabricates no record"
    fi
}

# The exact raw-profile messages must still be recognised and described — the
# narrowing must not cost coverage of real llvm output.
test_exact_raw_profile_messages_are_parsed() {
    local sandbox out line bad="" msg name
    for msg in "raw profile version mismatch" "empty raw profile file"; do
        sandbox="$(new_sandbox)"
        name="exact-1-1_0.profraw"
        cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
printf 'x' >"\$COVERAGE_PROFILE_DIR/$name"
printf 'warning: %s/$name: $msg\n' "\$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
        chmod +x "$sandbox/producer.sh"
        out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
            COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
            COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
        line="$(echo "$out" | grep -E '^[[:space:]]*corrupt=' | head -n 1)"
        rm -rf "$sandbox"
        echo "$line" | grep -q "corrupt=$name" || bad="$bad [$msg]not-parsed"
        echo "$line" | grep -q 'exists=yes' || bad="$bad [$msg]not-described"
        echo "$out" | grep -q 'unparseable' && bad="$bad [$msg]spurious-unparseable"
    done
    if [ -n "$bad" ]; then
        report 1 "exact raw-profile messages are parsed and described" "issues:$bad"
    else
        report 0 "exact raw-profile messages are parsed and described"
    fi
}

# A real warning carrying an exact raw-profile message, whose PATH contains a
# newline, still cannot be attributed — and must still be disclosed generically,
# with no named facts.
test_split_exact_raw_profile_warning_stays_unparseable() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'warning: /tmp/odd\nname-1-2_0.profraw: raw profile version mismatch\n'
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep 'unparseable' | head -n 1)"
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad not-disclosed"
    # Generic: it must carry no facts attributed to a path it never resolved,
    # and no exact tally — see the overcount test below for why.
    echo "$line" | grep -q 'corrupt=(unparseable)' || bad="$bad not-generic"
    echo "$line" | grep -q 'count=' && bad="$bad asserts-exact-count"
    echo "$line" | grep -q 'name-1-2_0' && bad="$bad leaked-partial-name"
    if [ -n "$bad" ]; then
        report 1 "a split exact raw-profile warning stays generically unparseable" \
            "issues:$bad; got: $line"
    else
        report 0 "a split exact raw-profile warning stays generically unparseable"
    fi
}

# ── 35-39. Facts the block states about reads it could not make ─────────────
# The rule 3e4a52b2 exists to enforce is that a read which did not happen
# produces no fact. A BLANK field breaks it; so does an affirmative wrong one.

# `read_pinned_facts` returns 1 both for "not there" and "could not open it",
# and the caller rendered both as `exists=no` — asserting absence for a file
# that is present. The survey two lines below says `file=<same name>`, so the
# block contradicts itself.
test_unreadable_named_path_is_not_reported_absent() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    if cannot_make_unreadable "$sandbox"; then
        rm -rf "$sandbox"
        report_skip "an unreadable named path is not reported absent" \
            "this user can read a mode-000 file; premise unavailable"
        return
    fi
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'REALBYTES' >"$COVERAGE_PROFILE_DIR/named-1-2_0.profraw"
chmod 000 "$COVERAGE_PROFILE_DIR/named-1-2_0.profraw"
printf 'warning: %s/named-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -E '^[[:space:]]*corrupt=' | head -n 1)"
    chmod -R u+rwX "$sandbox" 2>/dev/null
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad premise-not-described"
    echo "$line" | grep -q 'exists=no' && bad="$bad claims-absent"
    echo "$line" | grep -q 'exists=unreadable' || bad="$bad not-labelled-unreadable"
    if [ -n "$bad" ]; then
        report 1 "an unreadable named path is not reported absent" "issues:$bad; got: $line"
    else
        report 0 "an unreadable named path is not reported absent"
    fi
}

# `od` prints nothing for an empty file, so the named route emitted `header=`
# blank while the survey reported `header=n/a` for the very same file.
test_zero_byte_named_path_reports_na_header() {
    local sandbox out line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
: >"$COVERAGE_PROFILE_DIR/empty-1-1_0.profraw"
printf 'warning: %s/empty-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    line="$(echo "$out" | grep -E '^[[:space:]]*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    echo "$line" | grep -q 'size_bytes=0' || bad="$bad premise-not-zero-byte"
    echo "$line" | grep -qE 'header=[[:space:]]*mtime=' && bad="$bad blank-header"
    echo "$line" | grep -q 'header=n/a' || bad="$bad header-not-na"
    if [ -n "$bad" ]; then
        report 1 "a zero-byte named path reports header=n/a, not blank" "issues:$bad; got: $line"
    else
        report 0 "a zero-byte named path reports header=n/a, not blank"
    fi
}

# The named list is DEDUPED but the warning count is not, so a producer that
# names the same path twice made the accounting invent a second, unparseable
# path that never existed.
test_duplicate_warning_fabricates_no_unparseable_record() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'x' >"$COVERAGE_PROFILE_DIR/dup-1-1_0.profraw"
printf 'warning: %s/dup-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
printf 'warning: %s/dup-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    # Premise: the one real path must have been described.
    echo "$block" | grep -q 'corrupt=dup-1-1_0.profraw' || bad="$bad premise-not-described"
    echo "$block" | grep -q 'unparseable' && bad="$bad fabricated-unparseable"
    if [ -n "$bad" ]; then
        report 1 "a duplicated warning fabricates no unparseable record" "issues:$bad"
    else
        report 0 "a duplicated warning fabricates no unparseable record"
    fi
}

# Isolation is a SAFETY gate (property 3). `find` failing and `find` matching
# nothing both yield a count of zero, so an unreadable profile directory —
# where `rm` silently did nothing — was reported as successfully isolated and
# the wrapper retried against state nobody verified.
test_unreadable_profile_dir_fails_isolation_closed() {
    local sandbox out rc bad=""
    sandbox="$(new_sandbox)"
    if cannot_make_unreadable "$sandbox"; then
        rm -rf "$sandbox"
        report_skip "an unreadable profile dir fails isolation closed" \
            "this user can read a mode-000 file; premise unavailable"
        return
    fi
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'stale' >"$COVERAGE_PROFILE_DIR/stale-1-1_0.profraw"
chmod 000 "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=2 "$wrapper" 2>&1)"
    rc=$?
    chmod -R u+rwX "$sandbox" 2>/dev/null
    rm -rf "$sandbox"
    echo "$out" | grep -qi 'cannot isolate' || bad="$bad no-isolation-error"
    [ "$rc" -ne 0 ] || bad="$bad exited-zero"
    if [ -n "$bad" ]; then
        report 1 "an unreadable profile dir fails isolation closed" \
            "issues:$bad; rc=$rc"
    else
        report 0 "an unreadable profile dir fails isolation closed"
    fi
}

# `diag_max_files` is the one seam never validated, and it is used as a bare
# integer in `[ … -ge … ]` and as `head -n`. A non-numeric value therefore
# printed bash's own errors, unframed, INSIDE the group.
test_non_numeric_diag_cap_emits_no_raw_shell_error() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/cap-1-1_0.profraw"
printf '%s/cap-1-1_0.profraw\n' "$COVERAGE_PROFILE_DIR" >"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 \
        COVERAGE_DIAG_MAX_FILES=abc "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    echo "$block" | grep -q 'profile_dir=' || bad="$bad premise-no-block"
    echo "$block" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    # A bad cap must not silently swallow the evidence either.
    echo "$block" | grep -q 'response_line=' || bad="$bad lost-response-lines"
    echo "$block" | grep -q 'file=cap-1-1_0.profraw' || bad="$bad lost-survey"
    if [ -n "$bad" ]; then
        report 1 "a non-numeric diagnostic cap emits no raw shell error" \
            "issues:$bad; block: $(echo "$block" | grep -E ': line [0-9]+:' | head -1)"
    else
        report 0 "a non-numeric diagnostic cap emits no raw shell error"
    fi
}

# Digits alone are not enough. A value that does not fit a shell integer passes
# a digits-only guard and then fails in `[ … -ge … ]` AND in `head -n`, which
# reproduces both defects the validation exists to prevent: unframed shell
# errors inside the group, once per file, and every response line silently
# dropped while `lines=` still claims they exist.
test_out_of_range_diag_cap_emits_no_raw_shell_error() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/cap-2-2_0.profraw"
printf '%s/cap-2-2_0.profraw\n' "$COVERAGE_PROFILE_DIR" >"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 \
        COVERAGE_DIAG_MAX_FILES=99999999999999999999 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    echo "$block" | grep -q 'profile_dir=' || bad="$bad premise-no-block"
    echo "$block" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    echo "$block" | grep -q 'response_line=' || bad="$bad lost-response-lines"
    echo "$block" | grep -q 'file=cap-2-2_0.profraw' || bad="$bad lost-survey"
    if [ -n "$bad" ]; then
        report 1 "an out-of-range diagnostic cap emits no raw shell error" \
            "issues:$bad; block: $(echo "$block" | grep -E ': line [0-9]+:' | head -1)"
    else
        report 0 "an out-of-range diagnostic cap emits no raw shell error"
    fi
}

# ── 30-34. The reads BEHIND the fields (adversarial review of the framing) ───
# Framing the field is only half of it. The commands that produce the numbers
# printed beside that field open producer-controlled paths too, and a failed
# open leaks the shell's own path-bearing message into the group — the same
# defect as the `exec` one, in the survey rather than the named route.

# Skip helper: a mode-000 file is readable anyway as root, and on a platform
# without POSIX mode bits. Then the premise, not the property, is unavailable.
cannot_make_unreadable() {
    local probe="$1/unreadable-probe.bin"
    printf 'probe' >"$probe"
    chmod 000 "$probe" 2>/dev/null
    if cat "$probe" >/dev/null 2>&1; then
        chmod 644 "$probe" 2>/dev/null
        rm -f "$probe"
        return 0
    fi
    chmod 644 "$probe" 2>/dev/null
    rm -f "$probe"
    return 1
}

# `wc -c <"$f"` has NO stderr redirect and `od … <"$f" 2>/dev/null` applies its
# redirect too late, so an unopenable .profraw prints bash's own message —
# carrying the path, unframed — into the block. It also leaves `size_bytes=`
# EMPTY, fabricating a blank fact from a read that did not happen.
test_unreadable_profraw_emits_no_raw_shell_error() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    if cannot_make_unreadable "$sandbox"; then
        rm -rf "$sandbox"
        report_skip "an unreadable profraw emits no raw shell error" \
            "this user can read a mode-000 file; premise unavailable"
        return
    fi
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
name="$(printf 'boom-1-1_0.profraw\n  corrupt=FORGEDCORRUPT exists=yes in_response=yes\ntail-9-9_0.profraw')"
printf 'x' >"$COVERAGE_PROFILE_DIR/$name"
chmod 000 "$COVERAGE_PROFILE_DIR/$name"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -E '^[[:space:]]*file=boom-1-1_0' | head -n 1)"
    chmod -R u+rwX "$sandbox" 2>/dev/null
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad premise-not-surveyed"
    echo "$block" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    echo "$block" | grep -qE '^[[:space:]]*corrupt=FORGEDCORRUPT' && bad="$bad forged-record"
    # A read that did not happen is `n/a`, never an empty field.
    echo "$line" | grep -qE 'size_bytes=[[:space:]]*$|size_bytes= ' && bad="$bad blank-size"
    echo "$line" | grep -q 'size_bytes=n/a' || bad="$bad size-not-na"
    if [ -n "$bad" ]; then
        report 1 "an unreadable profraw emits no raw shell error" "issues:$bad; got: $line"
    else
        report 0 "an unreadable profraw emits no raw shell error"
    fi
}

# Same class on the response file: `awk … <"$list"` for the count, `head` for
# the fragments, and `response_contains_path` reads it again for membership.
test_unreadable_response_file_emits_no_raw_shell_error() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    if cannot_make_unreadable "$sandbox"; then
        rm -rf "$sandbox"
        report_skip "an unreadable response file emits no raw shell error" \
            "this user can read a mode-000 file; premise unavailable"
        return
    fi
    # The named warning makes membership run against the unreadable list too.
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/named-1-1_0.profraw"
printf 'anything\n' >"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
chmod 000 "$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
printf 'warning: %s/named-1-1_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -E '^response_file=' | head -n 1)"
    chmod -R u+rwX "$sandbox" 2>/dev/null
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad premise-no-response-line"
    echo "$block" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    echo "$line" | grep -qE 'lines=[[:space:]]*$' && bad="$bad blank-count"
    echo "$line" | grep -q 'lines=n/a' || bad="$bad count-not-na"
    if [ -n "$bad" ]; then
        report 1 "an unreadable response file emits no raw shell error" \
            "issues:$bad; got: $line"
    else
        report 0 "an unreadable response file emits no raw shell error"
    fi
}

# `find … | wc -l` counts LINES and calls them FILES — the same untruthful
# labelling as `entries=`, in the isolation error. One surviving profraw whose
# name holds two newlines is reported as three files.
test_isolation_counts_files_not_lines() {
    local sandbox out bad=""
    sandbox="$(new_sandbox)"
    # An `rm` that reports success but removes nothing. Mode bits would make the
    # real `rm` FAIL, which now fails closed BEFORE the count is reached — a
    # different property, pinned separately. To exercise the COUNT the cleanup
    # must look like it worked while a file survives. This is also portable, so
    # the case no longer needs a mode-bit SKIP.
    mkdir -p "$sandbox/shim"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$sandbox/shim/rm"
    chmod +x "$sandbox/shim/rm"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
name="$(printf 'leftover-1-1_0.profraw\nsecond-line\nthird-9-9_0.profraw')"
printf 'x' >"$COVERAGE_PROFILE_DIR/$name"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && PATH="$sandbox/shim:$PATH" \
        COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=2 "$wrapper" 2>&1)"
    rm -rf "$sandbox"
    # Premise: the survivor must have been detected at all.
    if ! echo "$out" | grep -q 'cannot isolate attempts'; then
        report 1 "the isolation error counts files, not lines" \
            "premise: no isolation error fired despite a surviving profraw"
        return
    fi
    # One file whose name spans three lines is ONE file.
    echo "$out" | grep -qE 'cannot isolate attempts: 1 \.profraw' || bad="$bad wrong-count"
    if [ -n "$bad" ]; then
        report 1 "the isolation error counts files, not lines" \
            "issues:$bad; got: $(echo "$out" | grep 'cannot isolate attempts' | head -1)"
    else
        report 0 "the isolation error counts files, not lines"
    fi
}

# The profraw survey discloses its cap; the response listing silently dropped
# everything past it, so `lines=` and the rendered fragments disagreed with no
# explanation.
test_response_truncation_is_disclosed() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
i=1
: >"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
while [ "$i" -le 5 ]; do
  printf '%s/entry-%s-1_0.profraw\n' "$COVERAGE_PROFILE_DIR" "$i" \
      >>"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
  i=$((i + 1))
done
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 \
        COVERAGE_DIAG_MAX_FILES=2 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    local shown
    shown="$(echo "$block" | grep -c 'response_line=')"
    echo "$block" | grep -q 'lines=5' || bad="$bad premise-wrong-count"
    [ "$shown" -eq 2 ] || bad="$bad expected-2-shown-got-$shown"
    echo "$block" | grep -qi 'more response lines not shown' || bad="$bad no-truncation-marker"
    if [ -n "$bad" ]; then
        report 1 "response-line truncation is disclosed" "issues:$bad"
    else
        report 0 "response-line truncation is disclosed"
    fi
}

# A control byte the escaper cannot name must not be rendered as a character a
# real path can also contain: `?` for ESC is indistinguishable from a literal
# `?`. Backslash is escaped first, so `\?` can only mean "a control byte was
# here" and a literal `?` stays itself.
test_residual_control_byte_is_unambiguous() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
name="$(printf 'esc-1-1\033x-and-?-literal_0.profraw')"
printf 'x' >"$COVERAGE_PROFILE_DIR/$name"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -E '^[[:space:]]*file=esc-1-1' | head -n 1)"
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad premise-not-surveyed"
    echo "$line" | grep -qF 'esc-1-1\?x-and-?-literal_0.profraw' || bad="$bad not-distinguishable"
    if [ -n "$bad" ]; then
        report 1 "a residual control byte is escaped unambiguously" "issues:$bad; got: $line"
    else
        report 0 "a residual control byte is escaped unambiguously"
    fi
}

# ── 24-27. Record framing: no field may forge a record (d-…191530082822-11) ──
# The evidence block is LINE-ORIENTED, so any path- or command-bearing field
# emitted raw can carry a newline and print further lines that read as further
# records. llvm-profdata really does emit raw newline filenames, so this is a
# production shape, not a contrived one.
#
# Shared fixture notes — three traps already paid for, do not re-discover them:
#   1. The forged payload must contain NO `/`. A `/` makes the name a path into
#      a directory that does not exist, the file is never created, and the block
#      truthfully prints `(no .profraw files present)` — a test that then passes
#      for the wrong reason.
#   2. An unquoted heredoc interpolating a newline-bearing name puts a REAL
#      newline in the generated producer's SOURCE and breaks it. The name is
#      built INSIDE the producer with printf escapes, under a quoted heredoc.
#   3. The PRODUCER must create the file, during the run. A file the harness
#      creates beforehand is removed by the wrapper's cleanup before the survey
#      runs, and the assertions then hold vacuously.

# The survey and the response file both render a producer-controlled filename.
# One newline-bearing name must not be able to forge corrupt=, file= or member:
# records out of its own bytes.
test_newline_field_cannot_forge_records() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
name="$(printf 'forge-1-1_0.profraw\n  corrupt=FORGEDCORRUPT exists=yes in_response=yes size_bytes=1\n  file=FORGEDFILE size_bytes=1 header=be ef\n  member: FORGEDMEMBER\ntail-9-9_0.profraw')"
printf 'x' >"$COVERAGE_PROFILE_DIR/$name"
printf '%s\n' "$COVERAGE_PROFILE_DIR/$name" >"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    # Premise: the name must actually have reached the block. Without this the
    # forgery assertions hold for a file that was never surveyed.
    echo "$block" | grep -q 'forge-1-1_0.profraw' || bad="$bad premise-not-rendered"
    echo "$block" | grep -qE '^[[:space:]]*corrupt=FORGEDCORRUPT' && bad="$bad forged-corrupt-record"
    echo "$block" | grep -qE '^[[:space:]]*file=FORGEDFILE' && bad="$bad forged-file-record"
    echo "$block" | grep -qE '^[[:space:]]*member: FORGEDMEMBER' && bad="$bad forged-member-record"
    if [ -n "$bad" ]; then
        report 1 "no diagnostic field can forge a record" "issues:$bad"
    else
        report 0 "no diagnostic field can forge a record"
    fi
}

# The response file is read line by line, so its count is a count of LINES. One
# pathname spanning five lines is not five entries, and labelling it `entries`
# claims a reconstructable path count the reader cannot get back.
test_response_count_is_labelled_by_lines() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
name="$(printf 'multi-1-1_0.profraw\nsecond-line\nthird-line\nfourth-9-9_0.profraw')"
printf 'x' >"$COVERAGE_PROFILE_DIR/$name"
printf '%s\n' "$COVERAGE_PROFILE_DIR/$name" >"$COVERAGE_PROFILE_DIR/agend-terminal-profraw-list"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -E '^response_file=' | head -n 1)"
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad premise-no-response-line"
    echo "$line" | grep -q 'entries=' && bad="$bad claims-path-entries"
    echo "$line" | grep -q 'lines=' || bad="$bad no-line-count"
    if [ -n "$bad" ]; then
        report 1 "the response count is labelled by lines, not path entries" \
            "issues:$bad; got: $line"
    else
        report 0 "the response count is labelled by lines, not path entries"
    fi
}

# The response file's own NAME is producer-controlled too: the wrapper finds it
# by glob, so a newline in the filename forges records through `response_file=`.
test_response_file_name_cannot_forge_records() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
name="$(printf 'evil-1-1\n  corrupt=FORGEDLIST exists=yes in_response=yes\nrest-profraw-list')"
printf 'nothing\n' >"$COVERAGE_PROFILE_DIR/$name"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    echo "$block" | grep -q 'evil-1-1' || bad="$bad premise-not-rendered"
    echo "$block" | grep -qE '^[[:space:]]*corrupt=FORGEDLIST' && bad="$bad forged-corrupt-record"
    if [ -n "$bad" ]; then
        report 1 "a response-file name cannot forge a record" "issues:$bad"
    else
        report 0 "a response-file name cannot forge a record"
    fi
}

# `profile_dir` is echoed verbatim at the top of the block and again in the
# isolation error. It is configuration rather than producer output, but it is
# path-bearing and goes through the same framing.
test_profile_dir_field_cannot_forge_records() {
    local sandbox out block dir bad=""
    sandbox="$(new_sandbox)"
    dir="$(printf '%s/p1\n  corrupt=FORGEDDIR exists=yes in_response=yes\np2' "$sandbox")"
    mkdir -p "$dir" || {
        rm -rf "$sandbox"
        report_skip "a profile_dir field cannot forge a record" \
            "this platform cannot create a newline-bearing directory; premise unavailable"
        return
    }
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/dir-1-1_0.profraw"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$dir" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    echo "$block" | grep -q 'p1' || bad="$bad premise-not-rendered"
    echo "$block" | grep -qE '^[[:space:]]*corrupt=FORGEDDIR' && bad="$bad forged-corrupt-record"
    if [ -n "$bad" ]; then
        report 1 "a profile_dir field cannot forge a record" "issues:$bad"
    else
        report 0 "a profile_dir field cannot forge a record"
    fi
}

# ── 28-29. Unframed shell errors inside the block ────────────────────────────
# The shell reports its own diagnostics on stderr in a format the wrapper does
# not control, and inside the group they read as evidence. They are also
# path-bearing, so they carry exactly the content the framing rules govern.
# `grep -F 'line '` would be too loose; the bash prefix is `<script>: line N:`.

# `exec 9<"$path" 2>/dev/null` does NOT suppress the message in bash 3.2: the
# shell reports a failed redirection itself, before the `2>` takes effect. Any
# named path that does not exist therefore prints a raw error into the block.
test_missing_named_path_emits_no_raw_shell_error() {
    local sandbox out block line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $sandbox/profiles/gone-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    line="$(echo "$block" | grep -E '^[[:space:]]*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    # Premise: the missing path must actually have been described.
    echo "$line" | grep -q 'exists=no' || bad="$bad premise-not-described"
    echo "$block" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    if [ -n "$bad" ]; then
        report 1 "a missing named path emits no raw shell error" \
            "issues:$bad; block: $(echo "$block" | grep -E ': line [0-9]+:' | head -1)"
    else
        report 0 "a missing named path emits no raw shell error"
    fi
}

# `grep -c` prints 0 AND exits 1 when nothing matches, so `grep -c … || printf 0`
# emits TWO counts. The wrapper then compares a two-line string as an integer:
# the shell errors into the block, and the unparseable accounting — which must
# disclose every warning the parser could not represent — silently stops working.
test_unmatched_warning_count_emits_no_raw_shell_error() {
    local sandbox out block bad=""
    sandbox="$(new_sandbox)"
    # A corruption signature that is NOT one of the named CORRUPTION_PHRASES:
    # the wrapper reaches diagnostics, but the phrase counter matches nothing.
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/count-1-1_0.profraw"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    block="$(echo "$out" | sed -n '/::group::coverage corruption evidence/,/::endgroup::/p')"
    rm -rf "$sandbox"
    # Premise: the block must have been emitted at all.
    echo "$block" | grep -q 'profile_dir=' || bad="$bad premise-no-block"
    echo "$block" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    # No warning was parseable and none was named: the honest answer is the
    # "named nothing" line, not an unparseable count.
    echo "$block" | grep -q 'producer named no corrupt profile path' || bad="$bad no-named-nothing-line"
    if [ -n "$bad" ]; then
        report 1 "an unmatched warning count emits no raw shell error" \
            "issues:$bad; block: $(echo "$block" | grep -E ': line [0-9]+:' | head -1)"
    else
        report 0 "an unmatched warning count emits no raw shell error"
    fi
}

# Membership must be an EXACT match: foo.profraw is not a member merely because
# the response file lists otherfoo.profraw.
test_membership_is_exact_not_substring() {
    local sandbox out line
    sandbox="$(new_sandbox)"
    printf 'partial' >"$sandbox/profiles/foo-1-2_0.profraw"
    printf '%s\n' "$sandbox/profiles/otherfoo-1-2_0.profraw" \
        >"$sandbox/profiles/agend-terminal-profraw-list"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $sandbox/profiles/foo-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    line="$(echo "$out" | grep -E '^\s*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'in_response=no'; then
        report 0 "response membership is exact, not substring"
    else
        report 1 "response membership is exact, not substring" "got: $line"
    fi
}

# A quoted absolute path must still resolve to the real file.
test_quoted_absolute_named_path_resolves() {
    local sandbox out line
    sandbox="$(new_sandbox)"
    printf 'partial' >"$sandbox/profiles/quoted-9-8_0.profraw"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: \"$sandbox/profiles/quoted-9-8_0.profraw\": invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    line="$(echo "$out" | grep -E '^\s*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=yes'; then
        report 0 "quoted absolute named path resolves"
    else
        report 1 "quoted absolute named path resolves" "got: $line"
    fi
}

# A bare relative name must resolve against the profile directory, not the CWD.
test_bare_relative_named_path_resolves() {
    local line
    line="$(run_named_case "bare-7-6_0.profraw" "bare-7-6_0.profraw")"
    if echo "$line" | grep -q 'exists=yes'; then
        report 0 "bare relative named path resolves against the profile dir"
    else
        report 1 "bare relative named path resolves against the profile dir" "got: $line"
    fi
}

# ── 9-13. Named-path scope, normalization and membership (primary R2) ────────
# Shared driver: producer names $2 verbatim; $1 is the profile dir to use.
# Echoes the first corrupt= line.
drive_named() {
    local profiles="$1" named="$2" sandbox out
    sandbox="$(dirname "$profiles")"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $named: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    echo "$out" | grep -E '^\s*corrupt=' | head -n 1
}

# A filename that merely CONTAINS dots is a normal component; only a component
# that IS `..` is a parent reference.
test_benign_dots_in_filename_are_not_traversal() {
    local sandbox line
    sandbox="$(new_sandbox)"
    printf 'partial' >"$sandbox/profiles/weird..name-1-2_0.profraw"
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/weird..name-1-2_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=yes'; then
        report 0 "a filename containing dots is not treated as traversal"
    else
        report 1 "a filename containing dots is not treated as traversal" "got: $line"
    fi
}

# Response entries may carry CRLF and may lack a final newline.
test_response_entries_tolerate_crlf_and_no_final_newline() {
    local sandbox line
    sandbox="$(new_sandbox)"
    printf 'partial' >"$sandbox/profiles/crlf-3-4_0.profraw"
    printf '%s\r' "$sandbox/profiles/crlf-3-4_0.profraw" \
        >"$sandbox/profiles/agend-terminal-profraw-list"
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/crlf-3-4_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'in_response=yes'; then
        report 0 "response entries tolerate CRLF and a missing final newline"
    else
        report 1 "response entries tolerate CRLF and a missing final newline" "got: $line"
    fi
}

# Same basename in a different directory is NOT the same member.
test_membership_compares_full_paths_not_basenames() {
    local sandbox line
    sandbox="$(new_sandbox)"
    mkdir -p "$sandbox/elsewhere"
    printf 'partial' >"$sandbox/profiles/dup-5-6_0.profraw"
    printf '%s\n' "$sandbox/elsewhere/dup-5-6_0.profraw" \
        >"$sandbox/profiles/agend-terminal-profraw-list"
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/dup-5-6_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'in_response=no'; then
        report 0 "membership compares full paths, not basenames"
    else
        report 1 "membership compares full paths, not basenames" "got: $line"
    fi
}

# A named path containing spaces must survive extraction intact.
test_named_path_with_spaces_is_extracted() {
    local sandbox line
    sandbox="$(new_sandbox)"
    printf 'partial' >"$sandbox/profiles/has space-7-8_0.profraw"
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/has space-7-8_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'corrupt=has space-7-8_0.profraw' && echo "$line" | grep -q 'exists=yes'; then
        report 0 "a named path containing spaces is extracted intact"
    else
        report 1 "a named path containing spaces is extracted intact" "got: $line"
    fi
}

# An absolute token outside the profile directory is never stat'd or read.
test_absolute_token_outside_profile_dir_is_out_of_scope() {
    local sandbox line outside
    sandbox="$(new_sandbox)"
    outside="$sandbox/outside-9-1_0.profraw"
    printf 'OUTSIDEDATA' >"$outside"
    line="$(drive_named "$sandbox/profiles" "$outside")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=out-of-scope'; then
        report 0 "an absolute token outside the profile dir is out-of-scope"
    else
        report 1 "an absolute token outside the profile dir is out-of-scope" "got: $line"
    fi
}

# ── 14-15. Leaf-symlink containment and the absolute-dir boundary (R3) ───────

# A .profraw INSIDE the profile dir that is a SYMLINK to an outside file must
# not be read: physicalizing only the parent leaves the leaf unresolved.
test_symlink_leaf_cannot_escape_containment() {
    local sandbox line outside
    sandbox="$(new_sandbox)"
    outside="$sandbox/secret.bin"
    printf 'OUTSIDEBYTES' >"$outside"
    ln -s "$outside" "$sandbox/profiles/leaf-1-2_0.profraw" 2>/dev/null
    # Git Bash on Windows defaults to MSYS winsymlinks=copy, so `ln -s` yields a
    # regular file. There is then no symlink to contain and `exists=yes` is the
    # truthful answer — the premise, not the property, is unavailable.
    if [ ! -L "$sandbox/profiles/leaf-1-2_0.profraw" ]; then
        rm -rf "$sandbox"
        report_skip "a symlinked leaf cannot escape containment" \
            "this platform does not create real symlinks; premise unavailable"
        return
    fi
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/leaf-1-2_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=out-of-scope' && ! echo "$line" | grep -qi '4f 55 54 53 49 44 45'; then
        report 0 "a symlinked leaf cannot escape containment"
    else
        report 1 "a symlinked leaf cannot escape containment" "got: $line"
    fi
}

# When COVERAGE_PROFILE_DIR is absolute but does not exist, a child of it is
# still IN scope — it is simply missing. The cd-failure fallback must not
# prepend $PWD to an already-absolute directory.
test_absolute_missing_profile_dir_keeps_its_boundary() {
    local sandbox line missing out
    sandbox="$(new_sandbox)"
    missing="$sandbox/not-created-yet"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $missing/child-3-4_0.profraw: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd / && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$missing" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    line="$(echo "$out" | grep -E '^\s*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=no'; then
        report 0 "an absolute missing profile dir keeps its own boundary"
    else
        report 1 "an absolute missing profile dir keeps its own boundary" "got: $line"
    fi
}

# ── 16-17. Lazy boundary and check/open identity (R4) ────────────────────────

# Production shape: a RELATIVE profile_dir that does not exist when the wrapper
# loads, under a symlinked CWD. A lexical boundary recorded at load time cannot
# match tokens that physicalize later, and every named path silently becomes
# out-of-scope — the diagnostics go blind exactly when they are needed.
test_relative_missing_profile_dir_under_symlinked_cwd() {
    local sandbox out line
    sandbox="$(new_sandbox)"
    mkdir -p "$sandbox/realdir"
    ln -s "$sandbox/realdir" "$sandbox/linkdir" 2>/dev/null
    if [ ! -L "$sandbox/linkdir" ]; then
        rm -rf "$sandbox"
        report_skip "relative missing profile_dir under a symlinked CWD" \
            "this platform does not create real symlinks; premise unavailable"
        return
    fi
    # cargo-llvm-cov CREATES the profile directory during the run, so it is
    # absent when the wrapper loads and present (and physicalizable) by the time
    # diagnostics run. That gap is the defect.
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
mkdir -p "$PWD/target/llvm-cov-target"
printf 'partial' >"$PWD/target/llvm-cov-target/child-1-2_0.profraw"
echo "warning: $PWD/target/llvm-cov-target/child-1-2_0.profraw: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox/linkdir" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="target/llvm-cov-target" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    line="$(echo "$out" | grep -E '^\s*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    if echo "$line" | grep -q 'exists=yes'; then
        report 0 "a relative missing profile_dir keeps its boundary under a symlinked CWD"
    else
        report 1 "a relative missing profile_dir keeps its boundary under a symlinked CWD" "got: $line"
    fi
}

# The bytes reported must come from the path that was VALIDATED, and naming the
# link must not cost membership.
test_in_scope_symlink_opens_the_validated_target() {
    local sandbox line
    sandbox="$(new_sandbox)"
    printf 'TARGETOK' >"$sandbox/profiles/target-1-2_0.profraw"
    ln -s "$sandbox/profiles/target-1-2_0.profraw" "$sandbox/profiles/alias-3-4_0.profraw" 2>/dev/null
    if [ ! -L "$sandbox/profiles/alias-3-4_0.profraw" ]; then
        rm -rf "$sandbox"
        report_skip "an in-scope symlink opens the validated target" \
            "this platform does not create real symlinks; premise unavailable"
        return
    fi
    printf '%s\n' "$sandbox/profiles/alias-3-4_0.profraw" \
        >"$sandbox/profiles/agend-terminal-profraw-list"
    line="$(drive_named "$sandbox/profiles" "$sandbox/profiles/alias-3-4_0.profraw")"
    rm -rf "$sandbox"
    if echo "$line" | grep -q '54 41 52 47 45 54 4f 4b' && echo "$line" | grep -q 'in_response=yes'; then
        report 0 "an in-scope symlink opens the validated target without losing membership"
    else
        report 1 "an in-scope symlink opens the validated target without losing membership" "got: $line"
    fi
}

# ── 18. check/open identity (R4 primary, proven blocker) ────────────────────
# Validation follows the link and approves a target; the bytes must then come
# from THAT target. A controlled readlink seam reports an in-scope target while
# the real link points outside: if the code validates `final` but opens `norm`,
# the outside marker bytes are read and disclosed.
test_validated_target_is_the_one_opened() {
    local sandbox line outside shim
    sandbox="$(new_sandbox)"
    outside="$sandbox/outside-marker.bin"
    printf 'OUTSIDEMARKER' >"$outside"
    printf 'INSIDEDECOY' >"$sandbox/profiles/decoy-9-9_0.profraw"
    ln -s "$outside" "$sandbox/profiles/leaf-5-5_0.profraw" 2>/dev/null
    if [ ! -L "$sandbox/profiles/leaf-5-5_0.profraw" ]; then
        rm -rf "$sandbox"
        report_skip "the validated target is the one opened" \
            "this platform does not create real symlinks; premise unavailable"
        return
    fi
    shim="$sandbox/shim"
    mkdir -p "$shim"
    cat >"$shim/readlink" <<SHIM
#!/usr/bin/env bash
# Lie: report an in-scope target regardless of the real link.
echo "$sandbox/profiles/decoy-9-9_0.profraw"
SHIM
    chmod +x "$shim/readlink"
    cat >"$sandbox/producer.sh" <<PRODUCER
#!/usr/bin/env bash
echo "warning: $sandbox/profiles/leaf-5-5_0.profraw: invalid instrumentation profile data (file header is corrupt)"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    line="$(cd "$sandbox" && PATH="$shim:$PATH" COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1 |
        grep -E '^\s*corrupt=' | head -n 1)"
    rm -rf "$sandbox"
    # 4f 55 54 53 49 44 45 4d = "OUTSIDEM"
    if echo "$line" | grep -q '4f 55 54 53 49 44 45 4d'; then
        report 1 "the validated target is the one opened" "outside marker bytes disclosed: $line"
    else
        report 0 "the validated target is the one opened"
    fi
}

# ── 48-54. Late-writer discriminator (#3236, d-…011320993265-24) ────────────
# DIAGNOSTICS ONLY. These pin evidence, not a cure: nothing here may change the
# failure, the retry policy or the classification. The open question they exist
# to answer is whether the writer of a named corrupt profile is STILL ALIVE when
# llvm-profdata runs.

# Drive a corrupt run whose producer names $2 and optionally pre-creates files.
drive_writer_case() {
    local sandbox="$1" named="$2" attempts="${3:-1}"
    (cd "$sandbox" && COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS="$attempts" \
        "$wrapper" 2>&1)
}

# A profile whose writer is still running must be reported alive — the decisive
# datum separating "late writer" from "died mid-merge".
test_live_writer_is_reported_alive() {
    local sandbox out bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
sleep 30 &
live=$!
echo "$live" >"$COV_LIVE_PID"
printf 'partial' >"$COVERAGE_PROFILE_DIR/agend-terminal-$live-777_0.profraw"
printf 'warning: %s/agend-terminal-%s-777_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR" "$live"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(COV_LIVE_PID="$sandbox/livepid" drive_writer_case "$sandbox" "" 1)"
    local live; live="$(cat "$sandbox/livepid" 2>/dev/null)"
    kill "$live" 2>/dev/null
    rm -rf "$sandbox"
    echo "$out" | grep -q "writer_pid=$live" || bad="$bad pid-not-reported"
    echo "$out" | grep -q 'writer_alive=yes' || bad="$bad not-alive"
    if [ -n "$bad" ]; then
        report 1 "a live writer is reported alive" "issues:$bad; got: $(echo "$out" | grep -o 'writer_[a-z]*=[^ ]*' | tr '\n' ' ')"
    else
        report 0 "a live writer is reported alive"
    fi
}

# A writer that has exited must be reported dead, not alive.
test_dead_writer_is_reported_dead() {
    local sandbox out bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
sh -c 'exit 0' & dead=$!; wait "$dead" 2>/dev/null
printf 'partial' >"$COVERAGE_PROFILE_DIR/agend-terminal-$dead-778_0.profraw"
printf 'warning: %s/agend-terminal-%s-778_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR" "$dead"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(drive_writer_case "$sandbox" "" 1)"
    rm -rf "$sandbox"
    echo "$out" | grep -q 'writer_alive=' || bad="$bad no-liveness-field"
    echo "$out" | grep -q 'writer_alive=yes' && bad="$bad claims-alive"
    if [ -n "$bad" ]; then
        report 1 "an exited writer is not reported alive" "issues:$bad"
    else
        report 0 "an exited writer is not reported alive"
    fi
}

# A name carrying no parseable pid must degrade, never error.
test_unparseable_writer_pid_is_unknown() {
    local sandbox out bad=""
    sandbox="$(new_sandbox)"
    printf 'partial' >"$sandbox/profiles/nopid.profraw"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'warning: %s/nopid.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(drive_writer_case "$sandbox" "" 1)"
    rm -rf "$sandbox"
    echo "$out" | grep -q 'writer_pid=unknown' || bad="$bad not-unknown"
    echo "$out" | grep -qE ': line [0-9]+:' && bad="$bad raw-shell-error"
    if [ -n "$bad" ]; then
        report 1 "an unparseable writer pid is reported unknown" "issues:$bad"
    else
        report 0 "an unparseable writer pid is reported unknown"
    fi
}

# The ownership timeline: three bounded inventories around cleanup.
test_three_inventories_bracket_cleanup() {
    local sandbox out bad="" order
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/agend-terminal-4242-779_0.profraw"
printf 'warning: %s/agend-terminal-4242-779_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(drive_writer_case "$sandbox" "" 2)"
    rm -rf "$sandbox"
    order="$(echo "$out" | grep -o 'inventory=[a-z-]*' | tr '\n' ',')"
    [ "$order" = "inventory=pre-clean,inventory=post-clean,inventory=post-grace," ] \
        || bad="$bad wrong-order($order)"
    echo "$out" | grep -q 'profraw_files=' || bad="$bad no-file-count"
    echo "$out" | grep -q 'live_writers=' || bad="$bad no-live-count"
    if [ -n "$bad" ]; then
        report 1 "three inventories bracket cleanup" "issues:$bad"
    else
        report 0 "three inventories bracket cleanup"
    fi
}

# The grace is diagnostic-only: a PASSING run must not pay it, and must emit no
# inventory at all.
test_grace_and_inventory_only_on_corrupt_path() {
    local sandbox out bad="" t0 t1
    sandbox="$(new_sandbox)"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$sandbox/producer.sh"
    chmod +x "$sandbox/producer.sh"
    t0=$(date +%s)
    out="$(drive_writer_case "$sandbox" "" 1)"
    t1=$(date +%s)
    rm -rf "$sandbox"
    echo "$out" | grep -q 'inventory=' && bad="$bad inventory-on-success"
    [ $((t1 - t0)) -le 1 ] || bad="$bad success-path-delayed($((t1-t0))s)"
    if [ -n "$bad" ]; then
        report 1 "grace and inventory are corrupt-path only" "issues:$bad"
    else
        report 0 "grace and inventory are corrupt-path only"
    fi
}

# 64 bytes, not just the magic — and still one line for a newline-bearing name.
test_first_64_bytes_are_captured() {
    local sandbox out line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
i=0; while [ "$i" -lt 80 ]; do printf 'A'; i=$((i+1)); done >"$COVERAGE_PROFILE_DIR/agend-terminal-4243-780_0.profraw"
printf 'warning: %s/agend-terminal-4243-780_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(drive_writer_case "$sandbox" "" 1)"
    line="$(echo "$out" | grep -o 'head64=.*' | head -n 1)"
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad no-head64"
    # 64 bytes of 0x41 => 64 "41" tokens.
    [ "$(echo "$line" | grep -o '41' | wc -l | tr -d ' ')" = "64" ] || bad="$bad not-64-bytes"
    if [ -n "$bad" ]; then
        report 1 "the first 64 bytes are captured" "issues:$bad; got: ${line:0:60}"
    else
        report 0 "the first 64 bytes are captured"
    fi
}

# Tool versions and LLVM_COV/LLVM_PROFDATA PRESENCE — never their values.
test_toolchain_presence_without_values() {
    local sandbox out line bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/agend-terminal-4244-781_0.profraw"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(cd "$sandbox" && LLVM_COV=/secret/path/llvm-cov COVERAGE_PRODUCER="$sandbox/producer.sh" \
        COVERAGE_CLEAN="true" COVERAGE_PROFILE_DIR="$sandbox/profiles" \
        COVERAGE_LOG="$sandbox/cov.log" COVERAGE_MAX_ATTEMPTS=1 "$wrapper" 2>&1)"
    line="$(echo "$out" | grep -o 'toolchain .*' | head -n 1)"
    rm -rf "$sandbox"
    [ -n "$line" ] || bad="$bad no-toolchain-line"
    echo "$line" | grep -q 'LLVM_COV=set' || bad="$bad presence-not-reported"
    # The VALUE must never appear anywhere in the output.
    echo "$out" | grep -q '/secret/path' && bad="$bad LEAKED-ENV-VALUE"
    if [ -n "$bad" ]; then
        report 1 "toolchain presence is reported without env values" "issues:$bad"
    else
        report 0 "toolchain presence is reported without env values"
    fi
}

# A healthy exemplar of the SAME module settles whether an odd size is a module
# property or a corruption signal — the hole left open by the run-31655194748
# analysis.
test_healthy_same_module_exemplar() {
    local sandbox out bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/agend-terminal-4245-782_0.profraw"
printf 'healthy-exemplar-bytes' >"$COVERAGE_PROFILE_DIR/agend-terminal-9999-782_0.profraw"
printf 'warning: %s/agend-terminal-4245-782_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(drive_writer_case "$sandbox" "" 1)"
    rm -rf "$sandbox"
    echo "$out" | grep -q 'exemplar=agend-terminal-9999-782_0.profraw' || bad="$bad no-exemplar"
    echo "$out" | grep -qE 'exemplar=[^ ]+ size_bytes=22' || bad="$bad no-exemplar-size"
    if [ -n "$bad" ]; then
        report 1 "a healthy same-module exemplar is emitted" "issues:$bad; got: $(echo "$out" | grep -o 'exemplar=.*' | head -1)"
    else
        report 0 "a healthy same-module exemplar is emitted"
    fi
}

# No same-module peer -> say so explicitly rather than omit the field.
test_missing_exemplar_is_explicit() {
    local sandbox out bad=""
    sandbox="$(new_sandbox)"
    cat >"$sandbox/producer.sh" <<'PRODUCER'
#!/usr/bin/env bash
printf 'partial' >"$COVERAGE_PROFILE_DIR/agend-terminal-4246-783_0.profraw"
printf 'warning: %s/agend-terminal-4246-783_0.profraw: invalid instrumentation profile data (file header is corrupt)\n' "$COVERAGE_PROFILE_DIR"
echo "error: no profile can be merged"
exit 1
PRODUCER
    chmod +x "$sandbox/producer.sh"
    out="$(drive_writer_case "$sandbox" "" 1)"
    rm -rf "$sandbox"
    echo "$out" | grep -q 'exemplar=none' || bad="$bad not-explicit"
    if [ -n "$bad" ]; then
        report 1 "a missing exemplar is stated explicitly" "issues:$bad"
    else
        report 0 "a missing exemplar is stated explicitly"
    fi
}

test_real_failure_wins_over_corruption_signature
test_cleanup_failure_is_surfaced
test_retry_cannot_consume_prior_attempt_profraw
test_corrupt_failure_emits_bounded_diagnostics
test_named_corrupt_path_is_cap_exempt
test_membership_is_exact_not_substring
test_quoted_absolute_named_path_resolves
test_bare_relative_named_path_resolves
test_benign_dots_in_filename_are_not_traversal
test_response_entries_tolerate_crlf_and_no_final_newline
test_membership_compares_full_paths_not_basenames
test_named_path_with_spaces_is_extracted
test_absolute_token_outside_profile_dir_is_out_of_scope
test_symlink_leaf_cannot_escape_containment
test_absolute_missing_profile_dir_keeps_its_boundary
test_relative_missing_profile_dir_under_symlinked_cwd
test_in_scope_symlink_opens_the_validated_target
test_validated_target_is_the_one_opened
test_tab_containing_path_is_reported
test_reads_are_pinned_against_post_validation_swap
test_failed_pinned_read_is_not_reported_as_success
test_temp_path_is_not_followed
test_unparseable_named_path_is_disclosed
test_live_writer_is_reported_alive
test_dead_writer_is_reported_dead
test_unparseable_writer_pid_is_unknown
test_three_inventories_bracket_cleanup
test_grace_and_inventory_only_on_corrupt_path
test_first_64_bytes_are_captured
test_toolchain_presence_without_values
test_healthy_same_module_exemplar
test_missing_exemplar_is_explicit
test_isolation_fails_closed_when_its_commands_fail
test_named_fifo_does_not_block_the_wrapper
test_fifo_response_file_does_not_block_the_wrapper
test_phrase_bearing_path_fragment_claims_no_count
test_benign_raw_profile_prose_fabricates_no_record
test_exact_raw_profile_messages_are_parsed
test_split_exact_raw_profile_warning_stays_unparseable
test_unreadable_named_path_is_not_reported_absent
test_zero_byte_named_path_reports_na_header
test_duplicate_warning_fabricates_no_unparseable_record
test_unreadable_profile_dir_fails_isolation_closed
test_non_numeric_diag_cap_emits_no_raw_shell_error
test_out_of_range_diag_cap_emits_no_raw_shell_error
test_unreadable_profraw_emits_no_raw_shell_error
test_unreadable_response_file_emits_no_raw_shell_error
test_isolation_counts_files_not_lines
test_response_truncation_is_disclosed
test_residual_control_byte_is_unambiguous
test_newline_field_cannot_forge_records
test_response_count_is_labelled_by_lines
test_response_file_name_cannot_forge_records
test_profile_dir_field_cannot_forge_records
test_missing_named_path_emits_no_raw_shell_error
test_unmatched_warning_count_emits_no_raw_shell_error

echo
echo "coverage-run contract: $pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
