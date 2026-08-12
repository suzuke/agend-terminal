#!/usr/bin/env bash
# #3236 — the coverage producer/retry wrapper, extracted from the inline
# `Run coverage` step in .github/workflows/ci.yml so its failure semantics are
# testable (same pattern as fmt-owned.sh + test_fmt_owned.sh).
#
# SCOPE (decision d-20260812161748154106-7): diagnostics and failure semantics
# ONLY. The corrupt-profraw writer is UNRESOLVED and nothing here claims to cure
# it — this wrapper's job is to stop lying about what failed and to leave behind
# enough evidence to finish that RCA.
#
# Four properties, pinned by scripts/test_coverage_run.sh:
#   1. an observed `test … FAILED` outranks the corruption signature — a real
#      failure is never retried and never relabelled a flake, and the producer's
#      own exit code survives (previously the signature was checked first, so a
#      run containing both was reported as "…signature-gated profraw-flake
#      retries", burying the real failure — see #1735 for the class)
#   2. cleanup failure is surfaced, not swallowed
#   3. a retry starts from a profile directory holding nothing a prior attempt
#      produced, or the wrapper fails rather than merging across attempts
#   4. corrupt/no-profile failure emits bounded, deterministic evidence, and
#      a path the producer NAMED as corrupt is described regardless of where it
#      sorts in the glob-ordered cap
#
# Seams (all default to the production values):
#   COVERAGE_PRODUCER     command that produces coverage        [cargo llvm-cov …]
#   COVERAGE_CLEAN        cleanup command between attempts      [cargo llvm-cov clean --workspace]
#   COVERAGE_PROFILE_DIR  directory holding *.profraw           [target/llvm-cov-target]
#   COVERAGE_MAX_ATTEMPTS total producer executions             [3]
#   COVERAGE_LOG          per-attempt producer log              [cov-attempt.log]
#   COVERAGE_DIAG_MAX_FILES  profraw files described on failure [10]
set -o pipefail

producer="${COVERAGE_PRODUCER:-cargo llvm-cov -p agend-terminal --tests --features tray --lcov --output-path coverage.lcov}"
clean_cmd="${COVERAGE_CLEAN:-cargo llvm-cov clean --workspace}"
profile_dir="${COVERAGE_PROFILE_DIR:-target/llvm-cov-target}"
max_attempts="${COVERAGE_MAX_ATTEMPTS:-3}"
log="${COVERAGE_LOG:-cov-attempt.log}"
diag_max_files="${COVERAGE_DIAG_MAX_FILES:-10}"

# A genuine producer/test failure. Checked BEFORE the corruption signature:
# corruption may accompany a real failure, and when it does the real failure is
# the truthful classification.
REAL_FAILURE_SIGNATURE='^\s*test .+ \.\.\. FAILED|test result: FAILED|error: test failed|^\s*FAIL \['
# llvm-cov profile corruption. Only meaningful once a real failure is ruled out.
CORRUPTION_SIGNATURE='profdata|\.profraw|malformed instrumentation|raw profile|invalid instrumentation profile|failed to (load|merge).*profile|no profile can be merged'

# Bounded evidence for the unresolved corruption RCA: attempt, directory,
# response-file membership, and per-file size + header bytes. Never echoes the
# producer log — that is what made previous failures unreadable.
# #3236: paths the producer EXPLICITLY named as corrupt are cap-exempt and are
# described FIRST. The ordinary listing below is glob-ordered, so the one file
# llvm-profdata named can sort outside the cap — observed in run 31619505420
# job 94190482315, where the block printed ten unrelated valid profiles and
# omitted agend-terminal-56911-14119548425640577428_0.profraw, the only path
# the producer named. This reports what was seen; it makes no claim about the
# writer, which is still unresolved.

# Lexical path normalization: collapse empty and `.` components and resolve a
# component that IS `..`. No filesystem access and no symlink following, so a
# name that merely CONTAINS dots (`weird..name.profraw`) stays a normal
# component — only a real parent reference is resolved away.
normalize_path() {
    local out="" comp
    case "$1" in /*) ;; *) printf '' ; return ;; esac
    while IFS= read -r comp; do
        case "$comp" in
            '' | .) ;;
            ..) out="${out%/*}" ;;
            *) out="$out/$comp" ;;
        esac
    done <<EOF
$(printf '%s\n' "$1" | tr '/' '\n')
EOF
    printf '%s' "${out:-/}"
}

# Follow a symlinked leaf to its final target, bounded. Containment must be
# decided on the path that will actually be OPENED: physicalizing only the
# parent leaves an in-directory symlink pointing anywhere. `readlink -f` is not
# portable to macOS, so the chain is walked one link at a time.
resolve_leaf() {
    local path="$1" i=0 target dir
    while [ -L "$path" ]; do
        i=$((i + 1))
        if [ "$i" -gt 32 ]; then
            printf ''
            return
        fi
        target="$(readlink "$path" 2>/dev/null)"
        if [ -z "$target" ]; then
            printf ''
            return
        fi
        case "$target" in
            /*) path="$target" ;;
            *)
                dir="${path%/*}"
                [ -n "$dir" ] || dir=/
                path="$dir/$target"
                ;;
        esac
    done
    printf '%s' "$path"
}

# Physicalize a path's directory, leaving the leaf name attached.
physicalize_parent() {
    local p="$1" leaf dir phys
    leaf="${p##*/}"
    dir="${p%/*}"
    [ -n "$dir" ] || dir=/
    phys="$(cd "$dir" 2>/dev/null && pwd -P)"
    if [ -n "$phys" ]; then
        printf '%s/%s' "$phys" "$leaf"
    else
        printf '%s' "$p"
    fi
}

in_profile_scope() {
    case "$1" in
        "$profile_dir_abs" | "$profile_dir_abs"/*) return 0 ;;
        *) return 1 ;;
    esac
}

# Absolutize against the profile directory, normalize lexically, then judge
# containment on the physical path that would actually be opened. Empty result
# means "not a path we may touch".
resolve_in_profile_dir() {
    local candidate="$1" norm final
    case "$candidate" in
        /*) ;;
        *) candidate="$profile_dir_abs/$candidate" ;;
    esac
    norm="$(physicalize_parent "$(normalize_path "$candidate")")"
    in_profile_scope "$norm" || { printf ''; return; }
    # A symlinked LEAF inside the directory must not escape it. A link whose
    # target is still in scope stays usable, so genuinely in-scope symlinked
    # paths keep working.
    final="$(resolve_leaf "$norm")"
    [ -n "$final" ] || { printf ''; return; }
    if [ "$final" != "$norm" ]; then
        final="$(physicalize_parent "$(normalize_path "$final")")"
        in_profile_scope "$final" || { printf ''; return; }
    fi
    printf '%s' "$norm"
}

# The profile directory as an absolute, physical path — the scope boundary every
# named token is judged against. When it does not exist yet, an already-absolute
# setting keeps its own boundary (prepending $PWD would move it).
resolve_profile_dir_abs() {
    local abs
    abs="$(cd "$profile_dir" 2>/dev/null && pwd -P)"
    if [ -z "$abs" ]; then
        case "$profile_dir" in
            /*) abs="$profile_dir" ;;
            *) abs="$PWD/$profile_dir" ;;
        esac
        abs="$(normalize_path "$abs")"
    fi
    printf '%s' "$abs"
}
profile_dir_abs="$(resolve_profile_dir_abs)"

# Strip surrounding quotes and a trailing CR (a response file is not guaranteed
# to be LF-separated).
clean_token() {
    local t="$1"
    t="${t%$'\r'}"
    t="${t#\"}"
    t="${t%\"}"
    t="${t#\'}"
    t="${t%\'}"
    printf '%s' "$t"
}

# Extract named paths from the KNOWN diagnostic line structure
# (`warning: <path>: <corruption phrase>`) rather than by scanning for anything
# ending in .profraw — the anchor is what lets a path containing spaces survive
# without broadening the match to unrelated text.
named_corrupt_paths() {
    [ -f "$log" ] || return 0
    sed -nE 's/^[[:space:]]*warning:[[:space:]]*"?(.+\.profraw)"?:[[:space:]]*(invalid instrumentation profile data|malformed instrumentation profile data|truncated profile data|raw profile).*/\1/p' \
        "$log" 2>/dev/null | awk '!seen[$0]++'
}

# `date -r FILE` is the one mtime spelling BSD and GNU agree on (`stat` is not).
file_mtime() {
    date -u -r "$1" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo unknown
}

# The `%m` module token from `<name>-<pid>-<module>_<n>.profraw`; `unknown` when
# the name does not carry one.
module_token() {
    printf '%s' "$1" | sed -nE 's/.*-([0-9]+)_[0-9]+\.profraw$/\1/p' | grep . || printf 'unknown'
}

# Membership is exact and by NORMALIZED FULL PATH: a same-named file in another
# directory is a different member, and a substring test would call foo.profraw a
# member because the list happens to hold otherfoo.profraw.
response_contains_path() {
    local want="$1" list entry resolved
    for list in "$profile_dir"/*-profraw-list; do
        [ -e "$list" ] || continue
        # `|| [ -n "$entry" ]` so a final entry without a trailing newline is
        # still read.
        while IFS= read -r entry || [ -n "$entry" ]; do
            entry="$(clean_token "$entry")"
            [ -n "$entry" ] || continue
            resolved="$(resolve_in_profile_dir "$entry")"
            [ -n "$resolved" ] || continue
            if [ "$resolved" = "$want" ]; then
                printf 'yes'
                return 0
            fi
        done <"$list"
    done
    printf 'no'
}

describe_named_corrupt() {
    local token base path exists size header mtime in_response
    token="$(clean_token "$1")"
    base="${token##*/}"
    path="$(resolve_in_profile_dir "$token")"
    if [ -z "$path" ]; then
        # Named, disclosed, and deliberately NOT touched: outside the profile
        # directory (or a parent reference) is not ours to stat or read.
        exists="out-of-scope"
        size=n/a
        header=n/a
        mtime=n/a
        in_response=no
    else
        if [ -e "$path" ]; then
            exists=yes
            size="$(wc -c <"$path" | tr -d ' ')"
            header="$(od -An -tx1 -N8 <"$path" 2>/dev/null | tr -s ' ' | sed 's/^ //;s/ $//')"
            mtime="$(file_mtime "$path")"
        else
            exists=no
            size=n/a
            header=n/a
            mtime=n/a
        fi
        in_response="$(response_contains_path "$path")"
    fi
    printf '  corrupt=%s exists=%s in_response=%s size_bytes=%s header=%s mtime=%s module=%s\n' \
        "$base" "$exists" "$in_response" "$size" "$header" "$mtime" "$(module_token "$base")"
}

emit_diagnostics() {
    local attempt="$1"
    echo "::group::coverage corruption evidence (attempt $attempt)"
    echo "profile_dir=$profile_dir"
    # (a) cap-exempt: every path the producer itself named, in the order named.
    local named=0 p
    while IFS= read -r p; do
        [ -n "$p" ] || continue
        describe_named_corrupt "$p"
        named=$((named + 1))
    done <<EOF
$(named_corrupt_paths)
EOF
    [ "$named" -eq 0 ] && echo "  (producer named no corrupt profile path)"
    # (b) the ordinary, bounded survey.
    local list
    for list in "$profile_dir"/*-profraw-list; do
        [ -e "$list" ] || continue
        echo "response_file=$list entries=$(wc -l <"$list" | tr -d ' ')"
        head -n "$diag_max_files" "$list" | sed 's/^/  member: /'
    done
    local shown=0 f
    for f in "$profile_dir"/*.profraw; do
        [ -e "$f" ] || continue
        if [ "$shown" -ge "$diag_max_files" ]; then
            echo "  … more profraw files not shown (cap=$diag_max_files)"
            break
        fi
        printf '  file=%s size_bytes=%s header=%s\n' \
            "$(basename "$f")" \
            "$(wc -c <"$f" | tr -d ' ')" \
            "$(od -An -tx1 -N8 <"$f" 2>/dev/null | tr -s ' ' | sed 's/^ //;s/ $//')"
        shown=$((shown + 1))
    done
    [ "$shown" -eq 0 ] && echo "  (no .profraw files present)"
    echo "::endgroup::"
}

# Narrowly scoped: only this run's raw profiles, only in the profile directory.
# Verified afterwards — an unverified clean is how a retry ends up merging a
# previous attempt's output.
isolate_attempt_outputs() {
    rm -f "$profile_dir"/*.profraw 2>/dev/null
    local leftover
    leftover="$(find "$profile_dir" -maxdepth 1 -name '*.profraw' 2>/dev/null | wc -l | tr -d ' ')"
    if [ "$leftover" != "0" ]; then
        echo "::error::coverage cannot isolate attempts: $leftover .profraw file(s) survived cleanup in $profile_dir"
        return 1
    fi
    return 0
}

attempt=1
while :; do
    eval "$producer" 2>&1 | tee "$log"
    producer_rc="${PIPESTATUS[0]}"
    if [ "$producer_rc" -eq 0 ]; then
        exit 0
    fi

    # (1) precedence: a real failure outranks corruption, always.
    if grep -qEi "$REAL_FAILURE_SIGNATURE" "$log"; then
        echo "::error::coverage failed with a REAL test failure (exit $producer_rc) — NOT a flake; failing fast. See the 'test ... FAILED' above."
        exit "$producer_rc"
    fi

    if ! grep -qiE "$CORRUPTION_SIGNATURE" "$log"; then
        echo "::error::coverage failed with an unclassified producer error (exit $producer_rc); not retrying"
        exit "$producer_rc"
    fi

    # (4) evidence for the still-open corruption RCA, on every corrupt attempt.
    emit_diagnostics "$attempt"

    if [ "$attempt" -ge "$max_attempts" ]; then
        echo "::error::coverage failed with llvm-cov profile corruption after $attempt attempt(s) (exit $producer_rc); the corrupt-writer RCA (#3236) is unresolved — this is NOT a known-good flake"
        exit "$producer_rc"
    fi

    echo "::warning::coverage attempt $attempt hit llvm-cov profile corruption; isolating outputs and retrying"
    # (2) cleanup truthfully.
    if ! eval "$clean_cmd"; then
        echo "::error::coverage cleanup command failed (\`$clean_cmd\`); refusing to retry against unverified state"
        exit 1
    fi
    # (3) isolation, verified.
    isolate_attempt_outputs || exit 1
    attempt=$((attempt + 1))
done
