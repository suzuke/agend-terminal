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

# Render an arbitrary byte string as exactly ONE line. Every path- or
# command-bearing field in the block and in the error paths goes through this.
# The block is line-oriented, so a field emitted raw can carry a newline and
# print further lines that read as further records: llvm-profdata really does
# emit raw newline filenames, and one such name was able to forge `corrupt=`,
# `file=` and `member:` records out of its own bytes.
#
# Backslash is escaped FIRST so the escapes introduced after it are unambiguous
# and the original bytes stay recoverable. Only backslash and C0/DEL control
# bytes are touched: `printf %q` would also quote spaces, and a profraw name
# containing a space is legal and must stay readable (bash 3.2 has no `${x@Q}`).
# Non-ASCII bytes are left alone — mangling a UTF-8 filename would lose the very
# evidence this block exists to carry.
escape_field() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    # Anything still a control byte (ESC, BEL, VT, …) cannot be rendered
    # faithfully on one line and is not a path character worth preserving. It
    # becomes `\?`, not `?`: backslash is already escaped above, so `\?` can
    # only mean "a control byte was here" while a literal `?` — legal in a
    # path — stays itself. A bare `?` would be indistinguishable from both.
    case "$s" in
        *[[:cntrl:]]*) s="$(printf '%s' "$s" | LC_ALL=C sed 's/[[:cntrl:]]/\\?/g')" ;;
    esac
    printf '%s' "$s"
}

# `diag_max_files` is used bare as an integer (`[ … -ge … ]`) and as `head -n`,
# so an operator typo used to print bash's own errors — unframed — inside the
# evidence group, once per file, and silently drop every response line. A cap
# below 1 is meaningless here: it would disclose a truncation marker for a
# listing it never started.
case "$diag_max_files" in
    '' | *[!0-9]*) diag_max_files=0 ;;
esac
# Digits alone are not enough. A value too large for a shell integer passes a
# digits-only guard and then fails in BOTH `[ … -ge … ]` and `head -n`, which
# reproduces the very defects this validation exists to prevent. Six digits is
# already far beyond any real profile count, so anything longer is a typo.
if [ "${#diag_max_files}" -gt 6 ]; then
    diag_max_files=0
fi
if [ "$diag_max_files" -lt 1 ]; then
    echo "::warning::COVERAGE_DIAG_MAX_FILES=$(escape_field "${COVERAGE_DIAG_MAX_FILES-}") is not a positive integer; using 10"
    diag_max_files=10
fi

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
#
# TRUTH CLAIM — an opened-descriptor snapshot (decision d-…191530082822-11).
# What this block asserts, exactly: containment was validated, the path was
# opened ONCE, and every fact below describes the object that descriptor
# referred to AT THAT MOMENT. Nothing more is claimed.
#   * A concurrent replacement of the same canonical name AFTER the open is
#     explicitly OUT OF SCOPE. A same-path replacement that we do detect is
#     still reported (`changed-during-read`), but detection is best-effort and
#     is not promised: no identity anchor is taken. Portable Bash cannot bind
#     validation, the opened bytes and the final pathname atomically —
#     /dev/fd is not identity-portable (macOS devfs gives it its own dev/inode,
#     so `-ef` reports DIFFERENT for a descriptor on the file itself),
#     stat-before/open/stat-after still races, and hard-link anchoring moves the
#     selection race while adding EXDEV, MSYS and cleanup failures.
#   * Hostile same-UID control of the profile directory after the synchronous
#     producer has exited is NOT a boundary this diagnostic block promises.
#     These are RCA breadcrumbs for a corrupt-writer investigation, not an
#     authentication decision.
FACT_SIZE=""
FACT_HEADER=""
FACT_MTIME=""
read_pinned_facts() {
    local path="$1" tmp
    FACT_SIZE=n/a
    FACT_HEADER=n/a
    FACT_MTIME=unknown
    # ONLY REGULAR FILES ARE OPENED. Opening a FIFO for reading blocks until a
    # writer appears, so a producer-named `*.profraw` FIFO hung the wrapper
    # forever — in CI a silent step timeout with the evidence block truncated
    # mid-print, which is strictly worse than a failure and is exactly the
    # unreadable failure this wrapper exists to remove. `-e` first, so a path
    # that simply does not exist still falls through to the open and is reported
    # absent rather than mislabelled. The scratch file below is guarded the same
    # way, for the same reason.
    if [ -e "$path" ] && [ ! -f "$path" ]; then
        return 5
    fi
    # The redirection is grouped, not suffixed: bash reports a FAILED
    # redirection itself, before a trailing `2>/dev/null` on the same `exec`
    # takes effect, so a missing named path used to print a raw shell error —
    # carrying the path, unescaped — straight into the evidence block.
    { exec 9<"$path"; } 2>/dev/null || return 1
    tmp="$(mktemp "${TMPDIR:-/tmp}/coverage-diag.XXXXXX" 2>/dev/null)"
    # The scratch pathname is not trusted: a redirect through a symlink would
    # write profile bytes into whatever it points at.
    if [ -z "$tmp" ] || [ -L "$tmp" ] || [ ! -f "$tmp" ]; then
        exec 9<&-
        [ -n "$tmp" ] && [ ! -L "$tmp" ] && rm -f "$tmp"
        # 4, not 1: OUR scratch failed. That says nothing whatever about the
        # target, and reporting it as a property of the target would be a fact
        # invented from a read that never happened.
        return 4
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
    FACT_SIZE="$( { wc -c <"$tmp"; } 2>/dev/null | tr -d ' ')"
    FACT_HEADER="$( { od -An -tx1 -N8 <"$tmp"; } 2>/dev/null | tr -s ' ' | sed 's/^ //;s/ $//')"
    rm -f "$tmp"
    # `od` prints nothing at all for an empty file, so a 0-byte profile used to
    # report `header=` blank here while the survey reported `header=n/a` for the
    # same file. An empty field is a fabricated fact on either route.
    case "$FACT_SIZE" in
        '' | *[!0-9]*) FACT_SIZE=n/a ;;
    esac
    [ -n "$FACT_HEADER" ] || FACT_HEADER=n/a
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
# EXACT llvm-profdata messages only. The parser anchors on a `warning:` line;
# the warning COUNT deliberately does not, so a name containing a newline —
# which splits the producer's line and strands the phrase — is still counted and
# disclosed. That only holds if the phrases cannot match anything else, and the
# bare two-word alternative `raw profile` matched ordinary prose: a log line
# reading `note: merging raw profile data from 4 inputs` was counted as a
# corruption warning, so the two populations disagreed and the accounting
# invented an unparseable path. It also failed to parse `empty raw profile
# file`, because the alternative had to match at the start of the message.
# Verified against the shipped binary rather than from memory:
#   strings "$(xcrun --find llvm-profdata)" | grep -E '^(empty raw|raw profile|malformed|invalid|truncated) '
CORRUPTION_PHRASES='invalid instrumentation profile data|malformed instrumentation profile data|truncated profile data|empty raw profile file|raw profile version mismatch'

named_corrupt_paths_raw() {
    [ -f "$log" ] || return 0
    sed -nE "s/^[[:space:]]*warning:[[:space:]]*\"?(.+\.profraw)\"?:[[:space:]]*($CORRUPTION_PHRASES).*/\\1/p" \
        "$log" 2>/dev/null
}

named_corrupt_paths() {
    named_corrupt_paths_raw | awk '!seen[$0]++'
}

# How many warning lines the parser actually understood — counted BEFORE the
# dedupe. The unparseable accounting compares warnings against this, not
# against the number of distinct paths: a producer that names the same path
# twice yields two warnings and one path, and subtracting those invented a
# second, unparseable path that never existed.
count_parsed_warnings() {
    local n
    n="$(named_corrupt_paths_raw | awk 'END{print NR}')"
    case "$n" in
        '' | *[!0-9]*) n=0 ;;
    esac
    printf '%s' "$n"
}

# How many corruption warnings the producer emitted, so a path the line-oriented
# parser cannot represent (a name containing a newline) is DISCLOSED as
# unparseable rather than silently dropped.
count_corruption_warnings() {
    local n
    [ -f "$log" ] || { printf '0'; return; }
    # Counted by PHRASE, not by a `warning:` prefix: a name containing a newline
    # splits the producer's line, leaving the phrase on a line of its own.
    #
    # `grep -c` prints 0 AND exits 1 when nothing matches, so a `|| printf '0'`
    # fallback emitted a SECOND count. The caller then compared the two-line
    # string as an integer: the shell errored into the evidence block and the
    # unparseable accounting below silently stopped working.
    n="$(grep -cE "($CORRUPTION_PHRASES)" "$log" 2>/dev/null)"
    case "$n" in
        '' | *[!0-9]*) n=0 ;;
    esac
    printf '%s' "$n"
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
        # `-f`, not `-e`: a FIFO response file would block this read forever.
        [ -f "$list" ] || continue
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
        # Grouped: an unreadable response file must not report its own path
        # into the evidence block through the shell's redirection error.
        done 2>/dev/null <"$list"
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
            5)
                # A FIFO, device or directory. It is there, it is not a profile,
                # and it was deliberately NOT opened — reading it could block
                # forever. Reporting what it IS beats reporting bytes it does
                # not have.
                exists="not-a-regular-file"
                size=n/a
                header=n/a
                mtime=n/a
                ;;
            4)
                # Our scratch file could not be created. We learned nothing
                # about the target, so we assert nothing about it.
                exists="undetermined"
                size=n/a
                header=n/a
                mtime=n/a
                ;;
            *)
                # The open failed. "Not there" and "there but not openable" are
                # different facts, and reporting the second as the first
                # asserts absence for a file the survey below lists by name.
                if [ -e "$path" ]; then
                    exists="unreadable"
                else
                    exists=no
                fi
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
    # The module token is derived from the RAW basename, then the basename is
    # rendered: deriving it from the escaped form would read `\n` as characters.
    printf '  corrupt=%s exists=%s in_response=%s size_bytes=%s header=%s mtime=%s module=%s\n' \
        "$(escape_field "$base")" "$exists" "$in_response" "$size" "$header" "$mtime" \
        "$(module_token "$base")"
}

emit_diagnostics() {
    local attempt="$1"
    profile_dir_abs="$(resolve_profile_dir_abs)"
    echo "::group::coverage corruption evidence (attempt $attempt)"
    # `printf`, not `echo`, wherever an escaped field is emitted: the escaped
    # form contains backslashes by construction, and `echo` re-interprets them
    # under `xpg_echo` — undoing the framing at the last step. That option is
    # off on all three CI platforms, so this is hardening rather than a fix for
    # anything reachable here; the invariant simply should not rest on it.
    printf 'profile_dir=%s\n' "$(escape_field "$profile_dir")"
    # (a) cap-exempt: every path the producer itself named, in the order named.
    local named=0 p
    while IFS= read -r p; do
        [ -n "$p" ] || continue
        describe_named_corrupt "$p"
        named=$((named + 1))
    done <<EOF
$(named_corrupt_paths)
EOF
    local warned parsed
    warned="$(count_corruption_warnings)"
    parsed="$(count_parsed_warnings)"
    if [ "$warned" -gt "$parsed" ]; then
        # Every corruption warning must be accounted for. A name the parser
        # cannot represent is disclosed as unparseable, never dropped. The
        # comparison is against PARSED warning lines, not distinct paths —
        # `named` is deduped, so using it invented an unparseable path whenever
        # the producer named the same file twice.
        #
        # BOOLEAN, not a tally. A newline-bearing pathname is arbitrary bytes,
        # so a path FRAGMENT may itself contain an exact corruption phrase; the
        # phrase then matches on two lines for ONE logical warning and any count
        # derived from line matches overstates it. Line framing cannot tell
        # pathname bytes from message bytes, and exact reconstruction is
        # impossible by decision — so this discloses THAT something was
        # unattributable, and claims nothing about how much.
        echo "  corrupt=(unparseable) exists=unparseable in_response=no size_bytes=n/a header=n/a mtime=n/a module=unknown"
    elif [ "$named" -eq 0 ]; then
        echo "  (producer named no corrupt profile path)"
    fi
    # (b) the ordinary, bounded survey.
    #
    # The response file is read LINE by line, so what is reported is a count of
    # lines and a set of line fragments — NOT path entries. One pathname holding
    # a newline occupies several lines, so `entries=` claimed a path count the
    # reader could not reconstruct, and each fragment rendered as its own
    # `member:` record. `awk END{NR}` rather than `wc -l`: a response file is not
    # guaranteed a trailing newline, and `wc -l` counts newlines, so a single
    # unterminated entry was reported as zero.
    # EVERY open below is grouped with its own `2>/dev/null`. A redirection
    # that fails is reported by the shell itself, on the live stderr, carrying
    # the path unframed — the `exec` defect, repeated once per read command.
    # And a read that did not happen reports `n/a`: an empty field is a
    # fabricated fact, which is the rule the named route already follows.
    local list line count
    for list in "$profile_dir"/*-profraw-list; do
        [ -e "$list" ] || continue
        # The response file's NAME is producer-controlled by glob, so it can be
        # a FIFO too — and the count, the listing and the membership read all
        # open it. Disclosed, never read, never dropped.
        if [ ! -f "$list" ]; then
            printf 'response_file=%s lines=n/a\n' "$(escape_field "$list")"
            continue
        fi
        count="$( { awk 'END{print NR}' <"$list"; } 2>/dev/null )"
        case "$count" in
            '' | *[!0-9]*) count=n/a ;;
        esac
        printf 'response_file=%s lines=%s\n' "$(escape_field "$list")" "$count"
        # A pipeline, not process substitution: nothing here mutates state that
        # has to survive the subshell, and `< <(…)` needs a working /dev/fd,
        # which is exactly what this script has already been bitten by.
        { head -n "$diag_max_files" <"$list"; } 2>/dev/null |
            while IFS= read -r line || [ -n "$line" ]; do
                printf '  response_line=%s\n' "$(escape_field "$line")"
            done
        # The profraw survey discloses its cap; this must too, or `lines=` and
        # the fragments below it disagree with no explanation.
        if [ "$count" != "n/a" ] && [ "$count" -gt "$diag_max_files" ]; then
            printf '  … more response lines not shown (cap=%s)\n' "$diag_max_files"
        fi
    done
    local shown=0 f fsize fheader
    for f in "$profile_dir"/*.profraw; do
        [ -e "$f" ] || continue
        if [ "$shown" -ge "$diag_max_files" ]; then
            echo "  … more profraw files not shown (cap=$diag_max_files)"
            break
        fi
        # Same rule as the named route: never open a non-regular object.
        if [ ! -f "$f" ]; then
            fsize=""
        else
            fsize="$( { wc -c <"$f"; } 2>/dev/null | tr -d ' ')"
        fi
        case "$fsize" in
            '' | *[!0-9]*)
                fsize=n/a
                fheader=n/a
                ;;
            *)
                fheader="$( { od -An -tx1 -N8 <"$f"; } 2>/dev/null | tr -s ' ' | sed 's/^ //;s/ $//')"
                [ -n "$fheader" ] || fheader=n/a
                ;;
        esac
        printf '  file=%s size_bytes=%s header=%s\n' \
            "$(escape_field "$(basename "$f")")" "$fsize" "$fheader"
        shown=$((shown + 1))
    done
    [ "$shown" -eq 0 ] && echo "  (no .profraw files present)"
    echo "::endgroup::"
}

# Narrowly scoped: only this run's raw profiles, only in the profile directory.
# Verified afterwards — an unverified clean is how a retry ends up merging a
# previous attempt's output.
isolate_attempt_outputs() {
    # Nothing produced yet is nothing to isolate — the producer creates this
    # directory during the run.
    [ -d "$profile_dir" ] || return 0
    # FAIL CLOSED when the directory cannot be listed. `find` failing and `find`
    # matching nothing both count zero, so an unlistable directory — where the
    # `rm` below silently does nothing — used to report successful isolation and
    # the wrapper retried against state nobody had verified. This is the
    # cross-attempt safety gate; an unverifiable clean is not a clean.
    if [ ! -r "$profile_dir" ] || [ ! -x "$profile_dir" ]; then
        printf '::error::coverage cannot isolate attempts: profile directory %s cannot be listed\n' \
            "$(escape_field "$profile_dir")"
        return 1
    fi
    # The cleanup's own exit status is part of the gate. It was discarded, so a
    # cleanup that removed nothing counted as a cleanup that had nothing to do.
    if ! rm -f "$profile_dir"/*.profraw 2>/dev/null; then
        printf '::error::coverage cannot isolate attempts: cleanup of %s failed\n' \
            "$(escape_field "$profile_dir")"
        return 1
    fi
    # ONE FIXED BYTE per match, captured as a string — never the names, and
    # never a pipeline. `find … | wc -l` discards find's exit status (and
    # `local x="$(…)"` would mask it too, because `local` returns 0), so a
    # failed enumeration produced a numeric 0 that is indistinguishable from
    # "nothing left" and the wrapper retried against unverified state. Counting
    # bytes rather than lines also keeps a name containing newlines from
    # inflating the total. `-exec printf` is the portable spelling: `-printf` is
    # GNU only and `-print0` is not guaranteed under MSYS.
    local marks leftover
    if ! marks="$(find "$profile_dir" -maxdepth 1 -name '*.profraw' \
        -exec printf 'x' \; 2>/dev/null)"; then
        printf '::error::coverage cannot isolate attempts: enumerating %s failed\n' \
            "$(escape_field "$profile_dir")"
        return 1
    fi
    leftover="${#marks}"
    if [ "$leftover" != "0" ]; then
        printf '::error::coverage cannot isolate attempts: %s .profraw file(s) survived cleanup in %s\n' \
            "$leftover" "$(escape_field "$profile_dir")"
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
        # shellcheck disable=SC2016  # the backticks are literal message text,
        # not command substitution: the format string must not expand anything.
        printf '::error::coverage cleanup command failed (`%s`); refusing to retry against unverified state\n' \
            "$(escape_field "$clean_cmd")"
        exit 1
    fi
    # (3) isolation, verified.
    isolate_attempt_outputs || exit 1
    attempt=$((attempt + 1))
done
