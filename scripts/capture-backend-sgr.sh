#!/usr/bin/env bash
# #1450: capture a REAL backend PTY session — raw bytes WITH SGR escapes —
# under the daemon's exact color environment, for use as a state-replay
# fixture (tests/fixtures/state-replay/) that exercises the VTerm cell-color
# anchor against genuine truecolor/256-color reds and Ink redraw framing.
#
# Why this exists: the #1450 RCA was that the #919 anchor's sole fixture
# hand-wrote an idealized `\x1b[31m`+contiguous-phrase shape that the
# production PTY never produces — so truecolor reds and Ink fragmentation
# went untested and a real rate-limit error was silently suppressed. Fixtures
# for the color anchor MUST be real captures, never hand-written escapes.
#
# The daemon spawns every backend PTY with these env vars (src/agent/mod.rs):
#   TERM=xterm-256color  COLORTERM=truecolor  FORCE_COLOR=1
# so the capture must replicate them — that combination is what makes a
# chalk/Ink backend emit the truecolor/256 reds the anchor must recognize.
#
# Usage:
#   scripts/capture-backend-sgr.sh <out.raw> <backend-command...>
#
# Example (capture a Claude rate-limit error — run while actually throttled,
# or against a proxy that injects the 429 wording):
#   scripts/capture-backend-sgr.sh /tmp/claude-rate-limit.raw claude
#   # interact: drive the agent until the rate-limit error renders, then exit
#
# Then inspect the SGR encoding the backend actually used:
#   scripts/capture-backend-sgr.sh --inspect /tmp/claude-rate-limit.raw
#
# Finally copy into tests/fixtures/state-replay/ and add a MANIFEST.yaml
# entry (cli_version + recorded_on), per the recording protocol there.
set -euo pipefail

if [[ "${1:-}" == "--inspect" ]]; then
  out="${2:?usage: --inspect <file.raw>}"
  python3 - "$out" <<'PY'
import re, sys
d = open(sys.argv[1], "rb").read()
sgrs = sorted(set(re.findall(rb"\x1b\[[0-9;]*m", d)))
print(f"bytes={len(d)} distinct_sgr={len(sgrs)}")
print("  truecolor (38;2):", any(b"38;2" in s for s in sgrs))
print("  256-color (38;5):", any(b"38;5" in s for s in sgrs))
print("  16-color red 31m:", any(s == b"\x1b[31m" for s in sgrs))
for s in sgrs:
    print("   ", repr(s))
PY
  exit 0
fi

out="${1:?usage: capture-backend-sgr.sh <out.raw> <backend-command...>}"
shift
[[ $# -ge 1 ]] || { echo "error: missing backend command" >&2; exit 2; }

echo "capturing '$*' → $out (daemon color env)" >&2
TERM=xterm-256color COLORTERM=truecolor FORCE_COLOR=1 script -q "$out" "$@"
echo "captured $(wc -c < "$out") bytes. Inspect with: $0 --inspect $out" >&2
