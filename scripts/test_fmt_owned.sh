#!/usr/bin/env bash
# scripts/test_fmt_owned.sh — hermetic tests for scripts/fmt-owned.sh (task83,
# decision d-20260713150435301072-46).
#
# Builds a throwaway git superproject with a RECURSIVE (2-level) submodule and
# asserts the owned-source formatter boundary:
#   - a malformed OWNED *.rs is formatted (write mode);
#   - a filename with a SPACE is handled (NUL-safe enumeration);
#   - the vendored submodule's content, gitlink and status stay BYTE-IDENTICAL
#     (recursively), and a super-tracked vendor/** file is never touched;
#   - --check DETECTS owned drift (non-zero) and passes once clean;
#   - a rustfmt PARSE FAILURE propagates (non-zero);
#   - the recursive parent/submodule tree ends CLEAN.
#
# Usage: scripts/test_fmt_owned.sh    # exits 0 all-pass, 1 on any failure, 2 setup.
set -uo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
fmt_owned="$script_dir/fmt-owned.sh"

pass=0
fail=0
ok()  { echo "PASS  $1"; pass=$((pass + 1)); }
bad() { echo "FAIL  $1"; fail=$((fail + 1)); }

command -v rustfmt >/dev/null 2>&1 || { echo "test_fmt_owned: rustfmt not found" >&2; exit 2; }

# git that permits local file:// submodules (git >=2.38 blocks them by default)
# and a deterministic default branch, hermetic identity.
git_h() {
    git -c protocol.file.allow=always -c init.defaultBranch=main \
        -c user.email=t@example.invalid -c user.name=t \
        -c commit.gpgsign=false "$@"
}

sha() { git hash-object "$1"; }   # content hash independent of the working repo

work="$(mktemp -d -t fmt-owned-test.XXXXXX)"
trap 'rm -rf "$work"' EXIT

mk_repo() { mkdir -p "$1"; git_h -C "$1" init -q; }
commit_all() { git_h -C "$1" add -A; git_h -C "$1" commit -q -m "$2"; }

# ── Build super → vendor/dep → nested/inner (2-level recursive submodule) ──────
inner="$work/inner"; mk_repo "$inner"
printf 'fn inner() {}\n' > "$inner/inner.rs"
commit_all "$inner" inner

dep="$work/dep"; mk_repo "$dep"
git_h -C "$dep" submodule add -q "$inner" nested >/dev/null 2>&1
printf 'fn dep() {}\n' > "$dep/dep.rs"
commit_all "$dep" dep

super="$work/super"; mk_repo "$super"
git_h -C "$super" submodule add -q "$dep" vendor/dep >/dev/null 2>&1
git_h -C "$super" submodule update -q --init --recursive

# Owned sources (deliberately MISformatted) + a super-tracked vendor/** file that
# the pathspec must exclude even though it is not itself a submodule.
mkdir -p "$super/src"
printf 'fn   main( ){  }\n'      > "$super/src/main.rs"     # owned, malformed
printf 'fn   spaced( ){  }\n'    > "$super/src/a b.rs"      # owned, malformed, SPACE in name
printf 'fn   vendored( ){  }\n'  > "$super/vendor/excluded.rs"  # super-tracked under vendor/**
commit_all "$super" super

# Snapshot everything the formatter must NOT change.
excluded_before="$(sha "$super/vendor/excluded.rs")"
dep_rs_before="$(sha "$dep/dep.rs")"
inner_rs_before="$(sha "$inner/inner.rs")"
submod_status_before="$(git_h -C "$super" submodule status --recursive)"

run_owned() { ( cd "$super" && "$fmt_owned" "$@" ); }
rc() { local c=0; "$@" >/dev/null 2>&1 || c=$?; echo "$c"; }

# ── 1. --check DETECTS owned drift (malformed files present) ──────────────────
[ "$(rc run_owned --check)" -ne 0 ] \
    && ok "--check detects owned drift (non-zero)" \
    || bad "--check should be non-zero on malformed owned files"

# ── 2/3. write mode FORMATS owned files, incl. the SPACE-named one ────────────
run_owned >/dev/null 2>&1 || true
if rustfmt --edition 2021 --check "$super/src/main.rs" >/dev/null 2>&1; then
    ok "owned malformed file is formatted (src/main.rs)"
else
    bad "src/main.rs not formatted by write mode"
fi
if rustfmt --edition 2021 --check "$super/src/a b.rs" >/dev/null 2>&1; then
    ok "space-named owned file is formatted (NUL/space path works)"
else
    bad "src/a b.rs (space) not formatted — NUL-safe enumeration broken"
fi

# ── 4. vendor content + gitlink + status BYTE-IDENTICAL (recursive) ───────────
excluded_after="$(sha "$super/vendor/excluded.rs")"
dep_rs_after="$(sha "$dep/dep.rs")"
inner_rs_after="$(sha "$inner/inner.rs")"
submod_status_after="$(git_h -C "$super" submodule status --recursive)"
if [ "$excluded_before" = "$excluded_after" ] \
    && [ "$dep_rs_before" = "$dep_rs_after" ] \
    && [ "$inner_rs_before" = "$inner_rs_after" ] \
    && [ "$submod_status_before" = "$submod_status_after" ]; then
    ok "vendor/** + recursive submodule content/gitlink/status byte-identical"
else
    bad "formatter mutated vendored/excluded content or submodule state"
fi

# super working tree shows ONLY owned src/ changes, never a vendor/ entry.
if git_h -C "$super" status --porcelain | grep -q '^.* vendor/'; then
    bad "super status reports a vendor/ modification after formatting"
else
    ok "super status has no vendor/ modification"
fi

# ── 5. --check PASSES once the owned tree is clean ────────────────────────────
[ "$(rc run_owned --check)" -eq 0 ] \
    && ok "--check passes after formatting (clean)" \
    || bad "--check should be zero once owned files are formatted"

# ── 6. recursive cleanliness: commit owned changes → whole tree clean ─────────
commit_all "$super" formatted
super_dirty="$(git_h -C "$super" status --porcelain)"
recurse_dirty="$(git_h -C "$super" submodule foreach --recursive 'git status --porcelain' 2>/dev/null | grep -v '^Entering' || true)"
if [ -z "$super_dirty" ] && [ -z "$recurse_dirty" ]; then
    ok "recursive parent/submodule cleanliness after commit"
else
    bad "tree not clean recursively (super='$super_dirty' sub='$recurse_dirty')"
fi

# ── 7. rustfmt PARSE FAILURE propagates (non-zero) ────────────────────────────
printf 'fn broken( {\n' > "$super/src/broken.rs"   # unparseable Rust
git_h -C "$super" add -A >/dev/null 2>&1
[ "$(rc run_owned)" -ne 0 ] \
    && ok "rustfmt parse failure propagates (non-zero)" \
    || bad "unparseable owned file should make write mode non-zero"

# ── 8. every production fmt caller invokes the shared surface ─────────────────
# (pre-push converges transitively via preflight, so the direct callers suffice.)
repo_root="$(cd "$script_dir/.." && pwd)"
callers_ok=1
for f in .github/workflows/ci.yml .gitlab-ci.yml scripts/preflight.sh; do
    if ! grep -q 'fmt-owned\.sh' "$repo_root/$f"; then
        echo "  (no fmt-owned.sh reference in $f)" >&2
        callers_ok=0
    fi
done
[ "$callers_ok" -eq 1 ] \
    && ok "all production fmt callers invoke scripts/fmt-owned.sh" \
    || bad "a production fmt caller does not invoke the shared surface"

echo
echo "summary: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
exit 0
