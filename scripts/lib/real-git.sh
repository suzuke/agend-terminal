# scripts/lib/real-git.sh — resolve the REAL git binary under the agend-git shim
# (task122, fixture-only shell counterpart of tests/common/git_isolated.rs).
#
# On an agent's PATH, `git` is the agend-git SHIM, which denies `git -c … <sub>`
# forms and reroutes fixtures into the bound worktree; child git procs inherit it.
# SOURCE this file, then call `assert_real_git_or_die` so this shell AND its child
# git processes use REAL git (real-git-first PATH). FAIL LOUD (exit) when only the
# shim resolves — never a silent SKIP.
#
# Resolution mirrors the Rust seam: `$AGEND_REAL_GIT` when it is a real git OUTSIDE
# the shim dir(s); else a PATH scan EXCLUDING `$AGEND_HOME/bin` and
# `~/.agend-terminal/bin`, proven via `git version`.
#
# Portable to bash 3.2 / Git Bash. This is a fixture helper only — never sourced by
# production runtime code.

# Shim dirs, one per line.
_rg_shim_dirs() {
    [ -n "${AGEND_HOME:-}" ] && printf '%s\n' "${AGEND_HOME}/bin"
    [ -n "${HOME:-}" ] && printf '%s\n' "${HOME}/.agend-terminal/bin"
    return 0
}

# Best-effort directory canonicalization for comparison.
_rg_canon() { (CDPATH= cd -- "$1" 2>/dev/null && pwd -P) || printf '%s' "$1"; }

# True if $1 is an executable that answers `git version`.
_rg_is_git() {
    [ -x "$1" ] || return 1
    "$1" version 2>/dev/null | grep -q '^git version'
}

# True if canonical dir $1 is one of the shim dirs.
_rg_is_shim_dir() {
    local want shim
    want="$(_rg_canon "$1")"
    while IFS= read -r shim; do
        [ "$(_rg_canon "$shim")" = "$want" ] && return 0
    done <<EOF
$(_rg_shim_dirs)
EOF
    return 1
}

# Echo the resolved REAL git path, or return 1 (only shim / none).
real_git() {
    # 1. explicit AGEND_REAL_GIT, if a real git outside the shim.
    if [ -n "${AGEND_REAL_GIT:-}" ] && _rg_is_git "${AGEND_REAL_GIT}"; then
        if ! _rg_is_shim_dir "$(dirname "${AGEND_REAL_GIT}")"; then
            printf '%s\n' "${AGEND_REAL_GIT}"
            return 0
        fi
    fi
    # 2. PATH scan excluding the shim dir(s).
    local entry
    local oldifs="$IFS"
    IFS=:
    for entry in $PATH; do
        IFS="$oldifs"
        [ -n "$entry" ] || continue
        if ! _rg_is_shim_dir "$entry" && _rg_is_git "${entry}/git"; then
            printf '%s\n' "${entry}/git"
            return 0
        fi
        IFS=:
    done
    IFS="$oldifs"
    return 1
}

_rg_die() {
    echo "real-git provenance FAILED — no real git on PATH after excluding the agend-git shim \
dir(s). Set AGEND_REAL_GIT or put a real git on PATH. Fail-loud (task122), never a SKIP." >&2
    exit 3
}

# Echo the DIRECTORY of the resolved real git, or die.
real_git_dir() {
    local g
    g="$(real_git)" || _rg_die
    dirname "$g"
}

# Assert real git is resolvable and PREPEND its dir to PATH (this shell + children).
# Dies fail-loud if only the shim resolves.
assert_real_git_or_die() {
    local dir
    dir="$(real_git_dir)" || exit 3
    PATH="${dir}:${PATH}"
    export PATH
}
