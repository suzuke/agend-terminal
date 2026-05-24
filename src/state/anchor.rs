use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ── #919 Phase A: raw-chunk ring + red-ANSI anchor ──────────────────────
//
// Architectural insight (confirmed in spike `/tmp/dialectic-919-dev-primary.md`):
// `crate::agent::inject_to_agent` strips ANSI from daemon-injected text
// at `src/agent.rs:1486` BEFORE writing to the PTY. So daemon-injected
// prose (e.g. `[AGEND-MSG]` content quoting another agent's discussion)
// arrives in the PTY byte stream as PLAIN TEXT — zero color escapes.
// Backend-emitted error messages, by contrast, almost always carry red
// SGR for visual emphasis (Claude / Codex / Gemini / OpenCode).
//
// The red-SGR-nearby predicate is therefore a STRUCTURAL discriminator
// (not heuristic): if `has_red_ansi_anchor(phrase, now) == false`, the
// matched phrase is almost certainly daemon-injected prose, not a real
// backend error. Phase A applies this gate to HIGH_FP patterns
// (ServerRateLimit / RateLimit / ContextFull) to suppress the
// recursive-dogfood false-positive class (#841/#846 RCAs).
//
// Ring sizing (per dev-2 cross-audit sharpening — trim from 20×4KB to
// 10×4KB): 40KB per agent, ~400KB for a 10-agent fleet. Acceptable.

/// One raw PTY chunk + monotonic timestamp. Bounded to `RAW_CHUNK_MAX`
/// bytes (oversized chunks are truncated at push).
#[derive(Debug, Clone)]
pub(crate) struct RawChunk {
    pub bytes: Vec<u8>,
    pub at: Instant,
}

/// Ring buffer size (chunks). 10 chunks × 4KB = 40KB per agent.
pub(super) const RAW_RING_CHUNKS: usize = 10;

/// Max bytes per chunk. Larger inputs are truncated at push.
pub(super) const RAW_CHUNK_MAX: usize = 4096;

/// Anchor proximity window (bytes) — red SGR must be within N bytes of
/// the matched phrase to count as an anchor.
const ANCHOR_WINDOW_BYTES: usize = 200;

/// Anchor freshness window (Duration) — chunks older than this are
/// ignored by the anchor check.
pub(super) const ANCHOR_WINDOW_MS: Duration = Duration::from_secs(30);

/// Returns true if the most-recent ring entries contain ANY occurrence
/// of `phrase` AND a red SGR escape within `ANCHOR_WINDOW_BYTES` of the
/// phrase start, all within `ANCHOR_WINDOW_MS` of `now`.
///
/// Red SGR variants matched (per dev-2 sharpening — 256-color
/// `\x1b[38;5;{160..200}m` deliberately SKIPPED to reduce FP surface):
/// - `\x1b[31m` standard red foreground
/// - `\x1b[91m` bright red foreground
/// - `\x1b[1;31m` bold + standard red
/// - `\x1b[31;1m` standard red + bold
///
/// Anchor algorithm: for each chunk within the freshness window,
/// search for the phrase as a UTF-8 substring (chunks are lossy-
/// converted from raw bytes since SGR escapes are pure ASCII and
/// don't span UTF-8 boundaries). If found, scan a `±ANCHOR_WINDOW_BYTES`
/// window around the phrase for any of the red-SGR substrings.
///
/// Performance: O(ring_size × phrase_len) per call. With 10 chunks of
/// 4KB each and a 50-char phrase, ~2ms worst case. Only invoked on
/// pattern match for HIGH_FP states, so total cost is bounded by the
/// detect rate (typically <1/sec under normal load).
pub(crate) fn has_red_ansi_anchor(ring: &VecDeque<RawChunk>, phrase: &str, now: Instant) -> bool {
    if phrase.is_empty() {
        return false;
    }
    // Red SGR variants. Stored as static byte slices for cheap
    // substring search. The leading `\x1b[` (CSI) + parameter bytes +
    // trailing `m` is a complete SGR sequence; partial matches inside
    // longer sequences (e.g. `\x1b[1;31;40m` where 40 is bg-color) are
    // still detected because we search the prefix substring.
    const RED_SGR_VARIANTS: &[&[u8]] = &[b"\x1b[31m", b"\x1b[91m", b"\x1b[1;31m", b"\x1b[31;1m"];
    let phrase_bytes = phrase.as_bytes();
    for chunk in ring.iter() {
        if now.duration_since(chunk.at) > ANCHOR_WINDOW_MS {
            continue;
        }
        let mut search_from = 0usize;
        while search_from < chunk.bytes.len() {
            let Some(rel) = find_subslice(&chunk.bytes[search_from..], phrase_bytes) else {
                break;
            };
            let phrase_start = search_from + rel;
            let phrase_end = phrase_start + phrase_bytes.len();
            // Window: phrase_start - W .. phrase_end + W
            let win_start = phrase_start.saturating_sub(ANCHOR_WINDOW_BYTES);
            let win_end = (phrase_end + ANCHOR_WINDOW_BYTES).min(chunk.bytes.len());
            let window = &chunk.bytes[win_start..win_end];
            if RED_SGR_VARIANTS
                .iter()
                .any(|sgr| find_subslice(window, sgr).is_some())
            {
                return true;
            }
            search_from = phrase_start + 1; // search further occurrences
        }
    }
    false
}

pub(super) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
}
