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
#   COVERAGE_DIAG_GRACE_SECS post-cleanup grace before final inventory [2]
#   COVERAGE_DIAG_PROC_ROOT  /proc to consult for PID visibility          [/proc]
set -o pipefail

producer="${COVERAGE_PRODUCER:-cargo llvm-cov -p agend-terminal --tests --features tray --lcov --output-path coverage.lcov}"
clean_cmd="${COVERAGE_CLEAN:-cargo llvm-cov clean --workspace}"
profile_dir="${COVERAGE_PROFILE_DIR:-target/llvm-cov-target}"
max_attempts="${COVERAGE_MAX_ATTEMPTS:-3}"
log="${COVERAGE_LOG:-cov-attempt.log}"
diag_max_files="${COVERAGE_DIAG_MAX_FILES:-10}"
# #3236: bounded post-cleanup grace before the final inventory (corrupt path only).
diag_grace_secs="${COVERAGE_DIAG_GRACE_SECS:-2}"
# Seam: the /proc to consult for PID visibility. Overridable so the hidepid and
# restricted-namespace branches are testable off Linux.
diag_proc_root="${COVERAGE_DIAG_PROC_ROOT:-/proc}"
# Validated like the file cap, and HARD-BOUNDED. Unvalidated it leaked a raw
# `sleep: invalid time interval` and an arbitrarily large value could re-time
# the retry path — neither is acceptable for a diagnostics-only change.
DIAG_GRACE_MAX_SECS=30

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
case "$diag_grace_secs" in
    '' | *[!0-9]*) diag_grace_secs=-1 ;;
esac
if [ "$diag_grace_secs" = "-1" ] || [ "${#diag_grace_secs}" -gt 3 ] \
    || [ "$diag_grace_secs" -gt "$DIAG_GRACE_MAX_SECS" ]; then
    echo "::warning::COVERAGE_DIAG_GRACE_SECS=$(escape_field "${COVERAGE_DIAG_GRACE_SECS-}") is not an integer in 0..$DIAG_GRACE_MAX_SECS; using 2"
    diag_grace_secs=2
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
FACT_HEAD64=""
read_pinned_facts() {
    local path="$1" tmp
    FACT_SIZE=n/a
    FACT_HEADER=n/a
    FACT_MTIME=unknown
    FACT_HEAD64=n/a
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
    # 64 bytes, from the SAME pinned copy — no second open. The magic alone was
    # never the problem: it is valid LLVM raw magic, identical on healthy files.
    # `-v`: without it od collapses repeated lines to `*`, silently truncating the
    # very bytes this field exists to show.
    FACT_HEAD64="$( { od -v -An -tx1 -N64 <"$tmp"; } 2>/dev/null | tr -s ' \n' ' ' | sed 's/^ //;s/ $//')"
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


# ── #3236 late-writer discriminator (DIAGNOSTICS ONLY) ──────────────────────
# None of this changes the failure, the retry policy or the classification. It
# answers one open question: is the writer of a named corrupt profile still
# ALIVE when llvm-profdata runs? Run 31655194748 showed one module's profiles at
# four different page-aligned sizes — the shape of files caught mid-growth — but
# liveness at merge was never observed, so the hypothesis stayed unproven.

# The `%p` field from `<name>-<pid>-<module>_<n>.profraw`; `unknown` otherwise.
pid_token() {
    printf '%s' "$1" | sed -nE 's/.*-([0-9]+)-[0-9]+_[0-9]+\.profraw$/\1/p' | grep . || printf 'unknown'
}

# POSIX liveness of a PID NUMBER — deliberately NOT called writer_alive. PIDs
# are recycled, so a live number is not proof that THE writer still runs; it is
# one correlate, to be read with `writer_start`/`writer_exe`. `kill -0` needs no
# /proc, so this fact holds on Linux, macOS and Git Bash alike.
# ONE boundary parse. Every PID consumer below reads ONLY this result, so a
# non-canonical token can never reach a kill/ps//proc lookup. Guarding each
# consumer separately was the wrong shape: it left `writer_start`/`writer_exe`
# free to look up `007969` and name a real process while liveness said unknown.
# Empty result == not a usable PID.
canonical_pid() {
    case "$1" in
        # A leading zero is not canonical: `007` would be reinterpreted as PID 7,
        # a DIFFERENT process. `0` is never a writer.
        '' | *[!0-9]* | 0*) printf ''; return ;;
    esac
    # Digits alone are not enough — a token too large for a shell integer errors
    # inside `[ … ]` and prints a raw shell diagnostic into the block.
    [ "${#1}" -le 10 ] || { printf ''; return; }
    printf '%s' "$1"
}

# Callers pass ONLY a canonical_pid result.
#
# TRI-STATE, truthfully. `kill -0` fails for BOTH "no such process" (ESRCH) and
# "permission denied" (EPERM), so a bare failure is NOT proof of absence.
#   yes     = existence proven
#   no      = absence proven BY AN AUTHORITY THAT SEES ALL PROCESSES
#   unknown = could not distinguish
# Authority matters: MSYS `ps` lists only MSYS processes, so on Windows it can
# not refute a native PID — measured, it reported PID 1 absent in CI.
# Which authority, if any, can prove ABSENCE of an arbitrary PID on this host?
# A NAMED SEAM, so Windows behaviour is testable from any platform.
#   proc = /proc lists every process (Linux)
#   ps   = `ps -p` lists every process (Darwin/BSD)
#   none = no authority over the native PID space; absence is NOT knowable
# Git Bash/MSYS is `none` even though it ships BOTH a /proc and a ps: each sees
# only MSYS processes, so neither can refute a native Windows PID. Inferring
# authority from `[ -d /proc ]` is exactly how PID 1 was twice declared absent
# on Windows CI.
pid_authority() {
    case "${MSYSTEM:-}" in
        ?*) printf 'none'; return ;;
    esac
    case "${OSTYPE:-}" in
        msys* | cygwin* | win*) printf 'none'; return ;;
    esac
    case "$(uname -s 2>/dev/null)" in
        Linux) printf 'proc' ;;
        Darwin | *BSD) printf 'ps' ;;
        *) printf 'none' ;;
    esac
}

# Callers pass ONLY a canonical_pid result.
#
# TRI-STATE, truthfully. `kill -0` fails for BOTH ESRCH and EPERM, so a bare
# failure is NOT proof of absence.
#   yes     = existence proven
#   no      = absence proven by an authority that sees EVERY process
#   unknown = could not distinguish
pid_liveness() {
    [ -n "$1" ] || { printf 'unknown'; return; }
    if kill -0 "$1" 2>/dev/null; then
        printf 'yes'
        return
    fi
    # The authority must actually ANSWER. A failed query proves nothing: `ps`
    # exiting 127 because it is missing is not evidence that the process is
    # gone, and neither is a /proc that is not mounted.
    local rc
    case "$(pid_authority)" in
        proc)
            # PRESENCE proves existence. ABSENCE proves nothing, ever.
            # hidepid is OWNER-SENSITIVE: PID 1 can be visible merely because it
            # shares our UID while a foreign target stays hidden, so no /proc
            # entry — not `self`, not `1` — can serve as a visibility proxy.
            # After `kill -0` has already failed, a missing entry is
            # indistinguishable from an invisible one, so it is `unknown`.
            if [ -d "$diag_proc_root/$1" ]; then
                printf 'yes'
            else
                printf 'unknown'
            fi
            ;;
        ps)
            ps -p "$1" -o pid= >/dev/null 2>&1
            rc=$?
            case "$rc" in
                0) printf 'yes' ;;
                # POSIX `ps` exits 1 for "no matching process" — the only
                # non-zero status that actually proves absence.
                1) printf 'no' ;;
                *) printf 'unknown' ;;
            esac
            ;;
        *) printf 'unknown' ;;
    esac
}

# Start time and EXECUTABLE IDENTITY, both best-effort and both platform-guarded.
# Only the executable's BASENAME is emitted — never argv, never env — so the
# block answers "which binary was this?" without carrying arbitrary content.
writer_start() {
    [ -n "$1" ] || { printf 'unavailable'; return; }
    if [ -r "/proc/$1/stat" ]; then
        # comm (field 2) is parenthesised and may contain spaces or ')', so
        # counting fields from the start is wrong. Cut after the FINAL ')' and
        # take the 20th remaining field (starttime is field 22 overall).
        sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | awk '{print $20}' | grep . && return
    fi
    ps -o lstart= -p "$1" 2>/dev/null | tr -s ' ' | sed 's/^ //;s/ $//' | grep . && return
    printf 'unavailable'
}

writer_exe() {
    local raw=""
    [ -n "$1" ] || { printf 'unavailable'; return; }
    if [ -r "/proc/$1/cmdline" ]; then
        raw="$(tr '\0' '\n' <"/proc/$1/cmdline" 2>/dev/null | head -n 1)"
    fi
    [ -n "$raw" ] || raw="$(ps -o comm= -p "$1" 2>/dev/null | head -n 1)"
    [ -n "$raw" ] || { printf 'unavailable'; return; }
    raw="${raw##*/}"
    printf '%s' "$(escape_field "${raw:0:64}")"
}

# One healthy peer of the SAME module, so an odd size can be read against its
# own module rather than against unrelated ones. Non-regular objects are never
# opened, per the FIFO rule.
# Is this basename one the producer named corrupt?
is_named_corrupt_base() {
    local want="$1" b
    while IFS= read -r b; do
        [ -n "$b" ] || continue
        [ "$b" = "$want" ] && return 0
    done <<EOF
$NAMED_CORRUPT_BASES
EOF
    return 1
}

emit_module_exemplar() {
    local corrupt_base="$1" module="$2" f base
    case "$module" in '' | unknown) printf '    exemplar=none\n'; return ;; esac
    for f in "$profile_dir"/*-"$module"_*.profraw; do
        [ -e "$f" ] || continue
        base="${f##*/}"
        [ "$base" = "$corrupt_base" ] && continue
        is_named_corrupt_base "$base" && continue
        # CONTAINMENT, then ONE SNAPSHOT — the same two steps the named route
        # takes, not a second mechanism. `wc -c` plus `od` were two more opens
        # of a producer-controlled name that `-f` had already followed, so a
        # peer symlinked outside the profile directory was offered as this
        # module's "healthy" exemplar and its first bytes printed.
        #
        # BASENAME, never "$f": resolve_in_profile_dir prepends $profile_dir_abs
        # to any non-absolute candidate, and $profile_dir defaults to the
        # RELATIVE `target/llvm-cov-target` — handing it the glob's own path
        # would double the prefix and silently blind this field in exactly the
        # configuration CI runs.
        resolve_in_profile_dir "$base" || continue
        # Any non-zero is a candidate we could not read as a healthy peer —
        # escaping, non-regular, replaced mid-read, or our own scratch failing.
        # An exemplar is an offer, so the honest move is to offer the next one.
        read_pinned_facts "$RESOLVED_FINAL" || continue
        printf '    exemplar=%s size_bytes=%s header=%s\n' \
            "$(escape_field "$base")" "$FACT_SIZE" "$FACT_HEADER"
        return
    done
    printf '    exemplar=none\n'
}

# PIDs named corrupt in THIS attempt, for the inventories below.
NAMED_WRITER_PIDS=""
# Basenames the producer NAMED corrupt this attempt. A named-corrupt file must
# never be offered as another's "healthy" exemplar — that inverts the field's
# entire purpose.
NAMED_CORRUPT_BASES=""

# A bounded inventory: how many profraw files remain, and how many named writers
# are still alive. NOT a process table — that would be unbounded and would carry
# arbitrary cmdlines.
emit_inventory() {
    local label="$1" files=0 live=0 f pid
    for f in "$profile_dir"/*.profraw; do
        # `-e` FOLLOWS the link, so a DANGLING symlink is false and used to
        # vanish from this count. A count is the one field a reader trusts
        # without checking, and an entry the producer created is present
        # whether or not its target is. Widened, never followed: the entry is
        # counted, nothing is opened.
        [ -e "$f" ] || [ -L "$f" ] || continue
        files=$((files + 1))
    done
    for pid in $NAMED_WRITER_PIDS; do
        [ "$(pid_liveness "$pid")" = "yes" ] && live=$((live + 1))
    done
    printf '  inventory=%s profraw_files=%s live_writers=%s\n' "$label" "$files" "$live"
}

# Resolve the llvm-profdata that cargo-llvm-cov ACTUALLY uses. Plain
# `llvm-profdata` is not on PATH in CI — the real one ships with the rustup
# llvm-tools component beside the target libdir, which is why the first version
# of this reported `unavailable` for a tool that had just run.
resolve_llvm_profdata() {
    local libdir cand
    if [ -n "${LLVM_PROFDATA:-}" ]; then
        printf '%s' "$LLVM_PROFDATA"
        return
    fi
    libdir="$( { rustc --print target-libdir; } 2>/dev/null )"
    if [ -n "$libdir" ]; then
        cand="${libdir%/lib}/bin/llvm-profdata"
        if [ -x "$cand" ]; then
            printf '%s' "$cand"
            return
        fi
    fi
    command -v llvm-profdata 2>/dev/null || printf ''
}

# cargo-llvm-cov answers to both the direct binary and the cargo subcommand;
# try both before concluding it is absent.
resolve_cargo_llvm_cov_version() {
    local v
    v="$( { cargo-llvm-cov --version; } 2>/dev/null | head -n 1)"
    [ -n "$v" ] || v="$( { cargo llvm-cov --version; } 2>/dev/null | head -n 1)"
    printf '%s' "$v"
}

# Versions of the tools that actually ran, plus whether the two LLVM overrides
# are set. PRESENCE ONLY — the values are paths and are deliberately not printed.
emit_toolchain() {
    local rustc_v cov_v pd_v pd_bin cov_present=unset pd_present=unset
    [ -n "${LLVM_COV:-}" ] && cov_present="set"
    [ -n "${LLVM_PROFDATA:-}" ] && pd_present="set"
    rustc_v="$( { rustc --version; } 2>/dev/null | head -n 1)"
    cov_v="$(resolve_cargo_llvm_cov_version)"
    pd_bin="$(resolve_llvm_profdata)"
    [ -n "$pd_bin" ] && pd_v="$( { "$pd_bin" --version; } 2>/dev/null | head -n 1 | tr -s ' ')"
    printf '  toolchain rustc=%s cargo_llvm_cov=%s llvm_profdata=%s LLVM_COV=%s LLVM_PROFDATA=%s\n' \
        "$(escape_field "${rustc_v:-unavailable}")" \
        "$(escape_field "${cov_v:-unavailable}")" \
        "$(escape_field "${pd_v:-unavailable}")" \
        "$cov_present" "$pd_present"
}

# The `%m` module token from `<name>-<pid>-<module>_<n>.profraw`; `unknown` when
# the name does not carry one.
module_token() {
    printf '%s' "$1" | sed -nE 's/.*-([0-9]+)_[0-9]+\.profraw$/\1/p' | grep . || printf 'unknown'
}

# Membership is exact and by NORMALIZED FULL PATH: a same-named file in another
# directory is a different member, and a substring test would call foo.profraw a
# member because the list happens to hold otherfoo.profraw.
#
# TRI-STATE, because `no` is the strongest claim in this record and it needs an
# authority. `yes` needs one matching entry. `no` needs the far stronger fact
# that EVERY present list was read to its end — so a list we could not examine
# (a directory or FIFO the producer's glob name landed on; a regular file whose
# open failed) yields `unknown`, never `no`. The survey below discloses those
# same lists as `lines=n/a`: emitting `in_response=no` beside `lines=n/a` made
# one record assert absence and admit ignorance at the same time, and the
# assertion was the false half. An empty glob is `unknown` too — no list to read
# is not a list that excludes us, and it is indistinguishable from a profile
# directory that could not be enumerated.
#
# This is the `ps` exit-127 rule and the `/proc`-absence rule at their fourth
# site: a query that did not answer is not a `no`. Membership was the last field
# in the record without a third state.
response_contains_path() {
    local want="$1" list entry examined=0 unexamined=0
    for list in "$profile_dir"/*-profraw-list; do
        # An unmatched glob leaves the pattern itself, and neither test matches
        # it. `-e` alone also dropped a DANGLING symlink — it follows the link,
        # so a broken one was skipped before the refusal below could class it
        # as unexamined. Widened so the entry reaches that refusal; the link is
        # still never followed.
        [ -e "$list" ] || [ -L "$list" ] || continue
        # A SYMLINKED LIST IS REFUSED OUTRIGHT, in scope or not. The name is
        # producer-controlled by glob, and cargo-llvm-cov writes this file with
        # `fs::write` — a legitimate producer never presents a link here, so
        # supporting one buys nothing and costs a resolved second path, which is
        # the validate-one/open-another shape this script has already been bitten
        # by. Judging the SHAPE needs no resolution at all.
        #
        # HONEST BOUND: this does not eliminate TOCTOU. `[ -L ]` and the open
        # below are separate syscalls and nothing anchors the object between
        # them; refusing outright removes the resolved detour and fails closed,
        # nothing more. A HARD link to an outside file defeats this test and the
        # resolving alternative alike — `-L` is false and the path genuinely is
        # inside the directory — and that requires same-UID write access to the
        # profile directory, which this block already declares out of scope.
        if [ -L "$list" ]; then
            unexamined=1
            continue
        fi
        # `-f`, not `-e`: a FIFO response file would block this read forever.
        # Refusing to read it is exactly why membership cannot then be denied.
        if [ ! -f "$list" ]; then
            unexamined=1
            continue
        fi
        # The open we TEST is the open we READ FROM. A separate `: <"$list"`
        # probe would validate one open and consume another — the
        # validate-one-path-open-another gap this script has already closed
        # once. The loop's own status is that of its last body command, so `:`
        # forces zero; a non-zero status here can then ONLY mean the redirection
        # itself failed, which is the difference between a list that omits us
        # and a list we never read.
        #
        # `2>/dev/null` BEFORE `<"$list"`: redirections apply left to right, so
        # ordering them the other way lets a failed open print bash's own
        # message — carrying the path, unframed — into the evidence block.
        if { while IFS= read -r entry || [ -n "$entry" ]; do
                 # `|| [ -n "$entry" ]` above so a final entry without a
                 # trailing newline is still read.
                 entry="$(clean_token "$entry")"
                 [ -n "$entry" ] || continue
                 resolve_in_profile_dir "$entry" || continue
                 # Compare the NAMED path: membership is about the path the
                 # response file refers to, not the target a link happens to
                 # point at.
                 if [ "$RESOLVED_NAMED" = "$want" ]; then
                     printf 'yes'
                     return 0
                 fi
             done
             :; } 2>/dev/null <"$list"; then
            examined=1
        else
            unexamined=1
        fi
    done
    if [ "$unexamined" -eq 1 ] || [ "$examined" -eq 0 ]; then
        printf 'unknown'
    else
        printf 'no'
    fi
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
        # `unknown`, not `no`, and not by analogy — by the same defect: the
        # membership walk SKIPS every entry that resolves outside the profile
        # directory, so a response list may name this exact path while we
        # deliberately ignore it. `no` claimed an exclusion that the comparison
        # is structurally unable to establish for an out-of-scope path.
        in_response=unknown
    fi
    # The module token is derived from the RAW basename, then the basename is
    # rendered: deriving it from the escaped form would read `\n` as characters.
    printf '  corrupt=%s exists=%s in_response=%s size_bytes=%s header=%s mtime=%s module=%s\n' \
        "$(escape_field "$base")" "$exists" "$in_response" "$size" "$header" "$mtime" \
        "$(module_token "$base")"
    # Writer evidence on its own lines, so the corrupt= record above keeps the
    # exact shape every prior contract test pins.
    # Parse ONCE; an invalid token yields the coherent tuple
    # (unknown, unavailable, unavailable) with ZERO process lookups.
    local wpid cpid
    wpid="$(pid_token "$base")"
    cpid="$(canonical_pid "$wpid")"
    NAMED_WRITER_PIDS="$NAMED_WRITER_PIDS $cpid"
    printf '    writer_pid=%s pid_alive=%s writer_start=%s writer_exe=%s\n' \
        "$(escape_field "$wpid")" "$(pid_liveness "$cpid")" \
        "$(escape_field "$(writer_start "$cpid")")" "$(writer_exe "$cpid")"
    printf '    head64=%s\n' "$FACT_HEAD64"
    emit_module_exemplar "$base" "$(module_token "$base")"
}

emit_diagnostics() {
    local attempt="$1"
    NAMED_WRITER_PIDS=""
    profile_dir_abs="$(resolve_profile_dir_abs)"
    echo "::group::coverage corruption evidence (attempt $attempt)"
    # `printf`, not `echo`, wherever an escaped field is emitted: the escaped
    # form contains backslashes by construction, and `echo` re-interprets them
    # under `xpg_echo` — undoing the framing at the last step. That option is
    # off on all three CI platforms, so this is hardening rather than a fix for
    # anything reachable here; the invariant simply should not rest on it.
    printf 'profile_dir=%s\n' "$(escape_field "$profile_dir")"
    emit_toolchain
    # Collect every named-corrupt basename FIRST, so the exemplar search below
    # can exclude all of them rather than only the file being described.
    local ncb
    while IFS= read -r ncb; do
        [ -n "$ncb" ] || continue
        ncb="$(clean_token "$ncb")"
        NAMED_CORRUPT_BASES="$NAMED_CORRUPT_BASES
${ncb##*/}"
    done <<EOF
$(named_corrupt_paths)
EOF
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
        # `in_response=unknown`: there is no path here to look up. Membership was
        # never queried, and a record that cannot even name its subject is the
        # last place to assert that subject is absent from a list.
        echo "  corrupt=(unparseable) exists=unparseable in_response=unknown size_bytes=n/a header=n/a mtime=n/a module=unknown"
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
        # Widened for the same reason as membership: a dangling list symlink is
        # a present entry, and dropping it left the block silent about a file
        # the producer created. It falls into the `lines=n/a` branch below,
        # disclosed by name and never opened.
        [ -e "$list" ] || [ -L "$list" ] || continue
        # The response file's NAME is producer-controlled by glob, so it can be
        # a FIFO too — and the count, the listing and the membership read all
        # open it. Disclosed, never read, never dropped.
        #
        # `-L` first, and for the same reason membership refuses it: a symlink
        # under this name is not a shape any legitimate producer creates, and
        # following one disclosed the CONTENT of a file outside the profile
        # directory as `response_line=` records. The NAME is producer-controlled
        # data this block already prints; the bytes behind a link are not ours.
        if [ -L "$list" ] || [ ! -f "$list" ]; then
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
    local shown=0 f fbase fsize fheader
    for f in "$profile_dir"/*.profraw; do
        # Widened, NOT turned into a refusal: an IN-SCOPE symlinked profile is
        # legitimate here and is still read through. A dangling one simply
        # reaches the containment-and-snapshot path below and fails there, so
        # it is disclosed by name with n/a facts instead of disappearing.
        [ -e "$f" ] || [ -L "$f" ] || continue
        if [ "$shown" -ge "$diag_max_files" ]; then
            echo "  … more profraw files not shown (cap=$diag_max_files)"
            break
        fi
        fbase="${f##*/}"
        # CONTAINMENT, then ONE SNAPSHOT, exactly as the named route does it.
        # `-f` FOLLOWS a link, so `wc -c` and `od` used to read — and print the
        # size and first bytes of — a file outside the profile directory that a
        # producer had linked to under an in-directory name. The two reads were
        # also two separate opens, so the size and the header could describe
        # different objects; one pinned copy answers both.
        # BASENAME for the same prefix-doubling reason as the exemplar above.
        if resolve_in_profile_dir "$fbase" && read_pinned_facts "$RESOLVED_FINAL"; then
            fsize="$FACT_SIZE"
            fheader="$FACT_HEADER"
        else
            # Escaping, non-regular, replaced mid-read, or our own scratch
            # failed. The NAME is still disclosed — never dropped — and no fact
            # is invented for a read that did not happen.
            fsize=n/a
            fheader=n/a
        fi
        printf '  file=%s size_bytes=%s header=%s\n' \
            "$(escape_field "$fbase")" "$fsize" "$fheader"
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
    # #3236 ownership timeline. Corrupt path ONLY — a passing run never reaches
    # here, so neither the inventories nor the grace touch the success path.
    emit_inventory pre-clean
    # (2) cleanup truthfully.
    if ! eval "$clean_cmd"; then
        # shellcheck disable=SC2016  # the backticks are literal message text,
        # not command substitution: the format string must not expand anything.
        printf '::error::coverage cleanup command failed (`%s`); refusing to retry against unverified state\n' \
            "$(escape_field "$clean_cmd")"
        exit 1
    fi
    emit_inventory post-clean
    # A short bounded grace, then look again: a writer that survives cleanup and
    # is still alive here is the late writer the RCA is hunting.
    sleep "$diag_grace_secs"
    emit_inventory post-grace
    # (3) isolation, verified.
    isolate_attempt_outputs || exit 1
    attempt=$((attempt + 1))
done
