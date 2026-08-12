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

# Physicalize a path by walking down to its nearest EXISTING ancestor and
# re-appending the missing components. `pwd -P` needs a directory that exists,
# and the profile directory does not exist yet on the first attempt — a purely
# lexical fallback would record a boundary that later, physicalized tokens can
# never match.
physicalize_best_effort() {
    local p="$1" missing="" dir phys
    case "$p" in /*) ;; *) p="$PWD/$p" ;; esac
    p="$(normalize_path "$p")"
    dir="$p"
    while [ "$dir" != "/" ] && [ -n "$dir" ] && [ ! -d "$dir" ]; do
        missing="${dir##*/}${missing:+/$missing}"
        dir="${dir%/*}"
        [ -n "$dir" ] || dir=/
    done
    phys="$(cd "$dir" 2>/dev/null && pwd -P)"
    [ -n "$phys" ] || phys="$dir"
    if [ -n "$missing" ]; then
        printf '%s/%s' "${phys%/}" "$missing"
    else
        printf '%s' "$phys"
    fi
}

in_profile_scope() {
    case "$1" in
        "$profile_dir_abs" | "$profile_dir_abs"/*) return 0 ;;
        *) return 1 ;;
    esac
}

# Absolutize against the profile directory, normalize, then judge containment on
# the physical path that would actually be opened. Results are returned in
# globals, NOT as a delimited string: a command substitution cannot carry NUL and
# any printable delimiter can occur in a valid path (a TAB-containing basename is
# legal), so nothing is encoded.
#   RESOLVED_NAMED — the path as the producer referred to it (membership)
#   RESOLVED_FINAL — the validated target callers must open
# Returns 0 when in scope, 1 otherwise.
RESOLVED_NAMED=""
RESOLVED_FINAL=""
resolve_in_profile_dir() {
    local candidate="$1" norm final
    RESOLVED_NAMED=""
    RESOLVED_FINAL=""
    case "$candidate" in
        /*) ;;
        *) candidate="$profile_dir_abs/$candidate" ;;
    esac
    norm="$(physicalize_best_effort "$candidate")"
    in_profile_scope "$norm" || return 1
    # A symlinked LEAF must not escape. The target is validated AND handed back:
    # validating one path and then opening another is the gap this closes.
    final="$(resolve_leaf "$norm")"
    [ -n "$final" ] || return 1
    if [ "$final" != "$norm" ]; then
        final="$(physicalize_best_effort "$final")"
        in_profile_scope "$final" || return 1
    fi
    RESOLVED_NAMED="$norm"
    RESOLVED_FINAL="$final"
    return 0
}

# All metadata comes from ONE descriptor opened on the validated target, and the
# bytes are emitted only after re-confirming that the path still resolves to that
# same in-scope object. Re-opening the path once per fact is what let a
# replacement between validation and the reads disclose an outside file.
# Returns 0 = facts collected, 1 = could not open, 2 = changed during the read.
FACT_SIZE=""
FACT_HEADER=""
FACT_MTIME=""
read_pinned_facts() {
    local path="$1" tmp
    FACT_SIZE=n/a
    FACT_HEADER=n/a
    FACT_MTIME=unknown
    exec 9<"$path" 2>/dev/null || return 1
    tmp="$(mktemp "${TMPDIR:-/tmp}/coverage-diag.XXXXXX" 2>/dev/null)"
    # The scratch pathname is not trusted: a redirect through a symlink would
    # write profile bytes into whatever it points at.
    if [ -z "$tmp" ] || [ -L "$tmp" ] || [ ! -f "$tmp" ]; then
        exec 9<&-
        [ -n "$tmp" ] && [ ! -L "$tmp" ] && rm -f "$tmp"
        return 1
    fi
    # Content is taken through the descriptor, so a later rename/replace of the
    # NAME cannot change what was read. A failed copy is a FAILED read — never
    # report zero bytes as though the file were empty.
    if ! cat <&9 >"$tmp" 2>/dev/null; then
        exec 9<&-
        rm -f "$tmp"
        return 3
    fi
    # mtime from the descriptor where the platform exposes it; otherwise report
    # `unknown` rather than re-opening a path that may have been replaced.
    FACT_MTIME="$(date -u -r /dev/fd/9 '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo unknown)"
    if ! resolve_in_profile_dir "$path" || [ "$RESOLVED_FINAL" != "$path" ]; then
        exec 9<&-
        rm -f "$tmp"
        return 2
    fi
    exec 9<&-
    FACT_SIZE="$(wc -c <"$tmp" | tr -d ' ')"
    FACT_HEADER="$(od -An -tx1 -N8 <"$tmp" 2>/dev/null | tr -s ' ' | sed 's/^ //;s/ $//')"
    rm -f "$tmp"
    return 0
}

# The profile directory as an absolute, physical path — the scope boundary every
# named token is judged against. When it does not exist yet, an already-absolute
# setting keeps its own boundary (prepending $PWD would move it).
resolve_profile_dir_abs() {
    physicalize_best_effort "$profile_dir"
}
# Deliberately NOT computed at load time: the producer creates the profile
# directory during the run, so the boundary must be derived when diagnostics
# run or it is recorded before the directory it describes exists.
profile_dir_abs=""

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
CORRUPTION_PHRASES='invalid instrumentation profile data|malformed instrumentation profile data|truncated profile data|raw profile'

named_corrupt_paths() {
    [ -f "$log" ] || return 0
    sed -nE "s/^[[:space:]]*warning:[[:space:]]*\"?(.+\.profraw)\"?:[[:space:]]*($CORRUPTION_PHRASES).*/\\1/p" \
        "$log" 2>/dev/null | awk '!seen[$0]++'
}

# How many corruption warnings the producer emitted, so a path the line-oriented
# parser cannot represent (a name containing a newline) is DISCLOSED as
# unparseable rather than silently dropped.
count_corruption_warnings() {
    [ -f "$log" ] || { printf '0'; return; }
    # Counted by PHRASE, not by a `warning:` prefix: a name containing a newline
    # splits the producer's line, leaving the phrase on a line of its own.
    grep -cE "($CORRUPTION_PHRASES)" "$log" 2>/dev/null || printf '0'
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
    local want="$1" list entry
    for list in "$profile_dir"/*-profraw-list; do
        [ -e "$list" ] || continue
        # `|| [ -n "$entry" ]` so a final entry without a trailing newline is
        # still read.
        while IFS= read -r entry || [ -n "$entry" ]; do
            entry="$(clean_token "$entry")"
            [ -n "$entry" ] || continue
            resolve_in_profile_dir "$entry" || continue
            # Compare the NAMED path: membership is about the path the response
            # file refers to, not the target a link happens to point at.
            if [ "$RESOLVED_NAMED" = "$want" ]; then
                printf 'yes'
                return 0
            fi
        done <"$list"
    done
    printf 'no'
}

describe_named_corrupt() {
    local token base named path exists size header mtime in_response rc
    token="$(clean_token "$1")"
    base="${token##*/}"
    if resolve_in_profile_dir "$token"; then
        named="$RESOLVED_NAMED"
        path="$RESOLVED_FINAL"
        read_pinned_facts "$path"
        rc=$?
        case "$rc" in
            0)
                exists=yes
                size="$FACT_SIZE"
                header="$FACT_HEADER"
                mtime="$FACT_MTIME"
                ;;
            3)
                # The copy through the descriptor failed. Reporting zero bytes
                # here would fabricate a fact from a read that did not happen.
                exists="read-failed"
                size=n/a
                header=n/a
                mtime=n/a
                ;;
            2)
                # The object was replaced between validation and the read; the
                # honest answer is that we will not attribute bytes to it.
                exists="changed-during-read"
                size=n/a
                header=n/a
                mtime=n/a
                ;;
            *)
                exists=no
                size=n/a
                header=n/a
                mtime=n/a
                ;;
        esac
        in_response="$(response_contains_path "$named")"
    else
        # Named, disclosed, and deliberately NOT touched: outside the profile
        # directory (or a parent reference) is not ours to stat or read.
        exists="out-of-scope"
        size=n/a
        header=n/a
        mtime=n/a
        in_response=no
    fi
    printf '  corrupt=%s exists=%s in_response=%s size_bytes=%s header=%s mtime=%s module=%s\n' \
        "$base" "$exists" "$in_response" "$size" "$header" "$mtime" "$(module_token "$base")"
}

emit_diagnostics() {
    local attempt="$1"
    profile_dir_abs="$(resolve_profile_dir_abs)"
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
    local warned
    warned="$(count_corruption_warnings)"
    if [ "$warned" -gt "$named" ]; then
        # Every corruption warning must be accounted for. A name the parser
        # cannot represent is disclosed as unparseable, never dropped.
        echo "  corrupt=(unparseable) exists=unparseable in_response=no size_bytes=n/a header=n/a mtime=n/a module=unknown count=$((warned - named))"
    elif [ "$named" -eq 0 ]; then
        echo "  (producer named no corrupt profile path)"
    fi
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
