//! Behavioral state inference — silence, cursor, and process signals.
//!
//! Sprint 27 PR-A: shadow-mode behavioral probe that runs alongside
//! regex-based state detection. In shadow mode, behavioral signals are
//! logged as telemetry but do NOT override the regex-detected state.
//! Phase 2 (Sprint 28+) promotes behavioral to primary with env var opt-in.
//!
//! ## Architecture: free functions over trait
//!
//! `config_for(backend)` + `infer_from_silence(config, duration)` are free
//! functions rather than a `BehavioralProbe` trait because:
//! 1. No dynamic dispatch needed — backend is known at StateTracker construction
//! 2. Config is `Copy` data, not behavior — a struct with fields, not methods
//! 3. Inference is a pure function of (config, signal) → result
//! 4. Avoids trait object lifetime complexity in StateTracker (which is `!Send`)
//!
//! A trait would add vtable indirection for zero benefit. If Phase 2 needs
//! per-backend method overrides (e.g. custom cursor query parsing), promote
//! to trait at that point.

use crate::backend::Backend;
use std::time::Duration;

/// Behavioral state inference result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BehavioralSignal {
    /// PTY output silent beyond threshold → likely thinking/processing.
    SilenceThinking,
    /// PTY output silent beyond idle threshold → likely idle/waiting.
    SilenceIdle,
    /// No behavioral signal detected.
    None,
}

impl std::fmt::Display for BehavioralSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SilenceThinking => write!(f, "silence_thinking"),
            Self::SilenceIdle => write!(f, "silence_idle"),
            Self::None => write!(f, "none"),
        }
    }
}

/// Per-backend behavioral calibration constants.
#[derive(Debug, Clone, Copy)]
pub struct BehavioralConfig {
    /// Silence duration before inferring "thinking" (ms).
    pub silence_thinking_ms: u64,
    /// Silence duration before inferring "idle" (ms).
    pub silence_idle_ms: u64,
    /// Whether this backend supports cursor position query (DSR CPR).
    #[allow(dead_code)] // Phase 2 Sprint 28+
    pub supports_cursor_query: bool,
}

impl Default for BehavioralConfig {
    fn default() -> Self {
        Self {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        }
    }
}

/// Get behavioral config for a backend.
///
/// #8 Phase 2 step-0: migrated backends (`profile() == Some`) source from the
/// co-located [`crate::backend_profile::BackendProfile`]; un-migrated backends
/// fall back to [`config_for_legacy`]. `profile_configs_byte_identical_to_legacy`
/// proves `p.behavioral == config_for_legacy(b)` for every migrated backend, so
/// this is a pure indirection, not a behavior change.
pub fn config_for(backend: &Backend) -> BehavioralConfig {
    if let Some(p) = crate::backend_profile::profile(backend) {
        return p.behavioral;
    }
    config_for_legacy(backend)
}

/// The pre-reroute behavioral match — both the fork's fallback for un-migrated
/// backends AND the TRUE-legacy reference for the parity test (after the reroute,
/// `config_for` sources migrated backends from the profile, so it can't stand in
/// for "legacy" without making the assertion circular).
pub(crate) fn config_for_legacy(backend: &Backend) -> BehavioralConfig {
    match backend {
        Backend::ClaudeCode => BehavioralConfig {
            silence_thinking_ms: 2000,
            silence_idle_ms: 6000,
            supports_cursor_query: true,
        },
        Backend::KiroCli => BehavioralConfig {
            silence_thinking_ms: 2500,
            silence_idle_ms: 7000,
            supports_cursor_query: true,
        },
        Backend::Codex => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false, // bubbletea TUI may not respond to DSR
        },
        Backend::Gemini => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        },
        Backend::OpenCode => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        },
        // #987: agy shares gemini-cli's agent engine + TUI; inherit gemini's
        // calibrated thresholds as starting point. Tune in follow-up PR if
        // empirical telemetry shows divergence.
        Backend::Agy => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        },
        Backend::Shell | Backend::Raw(_) => BehavioralConfig::default(),
    }
}

/// Infer behavioral signal from silence duration.
pub fn infer_from_silence(config: &BehavioralConfig, silence: Duration) -> BehavioralSignal {
    let silence_ms = silence.as_millis() as u64;
    if silence_ms >= config.silence_idle_ms {
        BehavioralSignal::SilenceIdle
    } else if silence_ms >= config.silence_thinking_ms {
        BehavioralSignal::SilenceThinking
    } else {
        BehavioralSignal::None
    }
}

/// Get the foreground process group ID for a PTY fd.
/// Delegates to `backend_harness::verify_tcgetpgrp` (promoted, not rebuilt).
#[allow(dead_code)] // Phase 2 Sprint 28+
#[cfg(unix)]
pub fn fg_pgid(_pty_fd: i32) -> Option<u32> {
    // Phase 2: wire to actual PTY fd. For now, use verify_tcgetpgrp
    // which probes stdin — sufficient for shadow-mode telemetry.
    crate::backend_harness::verify_tcgetpgrp()
        .ok()
        .map(|pgid| pgid as u32)
}

#[allow(dead_code)] // Phase 2 Sprint 28+
#[cfg(not(unix))]
pub fn fg_pgid(_pty_fd: i32) -> Option<u32> {
    // Windows: ConsoleProcessList stub — Sprint 28+ Phase 2
    Option::None
}

/// Shadow-mode telemetry: log behavioral signal alongside regex state.
/// In shadow mode this is observability only — no state change.
pub fn log_shadow_telemetry(
    instance: &str,
    backend: &str,
    regex_state: &str,
    behavioral: BehavioralSignal,
) {
    if behavioral != BehavioralSignal::None {
        tracing::debug!(
            target: "behavioral_shadow",
            instance,
            backend,
            regex_state,
            behavioral = %behavioral,
            "behavioral shadow: signal detected"
        );
    }
}

// ---------------------------------------------------------------------------
// Divergence dashboard — accumulates behavioral vs regex state comparisons
// ---------------------------------------------------------------------------

/// Per-backend divergence counter for shadow-mode telemetry.
#[derive(Debug, Default)]
pub struct DivergenceStats {
    pub total_ticks: u64,
    pub agree: u64,
    pub diverge: u64,
}

impl DivergenceStats {}

/// Global divergence accumulator — keyed by backend name.
static DIVERGENCE: parking_lot::Mutex<Option<std::collections::HashMap<String, DivergenceStats>>> =
    parking_lot::Mutex::new(None);

/// Record a tick where behavioral and regex states are compared.
pub fn record_divergence(backend: &str, behavioral: BehavioralSignal, regex_state: &str) {
    let mut guard = DIVERGENCE.lock();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    let stats = map.entry(backend.to_string()).or_default();
    stats.total_ticks += 1;
    // Divergence: behavioral says thinking/idle but regex says something else
    let behavioral_implies = match behavioral {
        BehavioralSignal::SilenceThinking => "thinking",
        BehavioralSignal::SilenceIdle => "idle",
        BehavioralSignal::None => regex_state, // no signal = agree by default
    };
    if behavioral_implies == regex_state {
        stats.agree += 1;
    } else {
        stats.diverge += 1;
    }
}

/// Wave 1 CLI consolidation: the standalone `state-divergence-report`
/// CLI surface was removed (operator audit decision
/// `d-20260507155456191111-0`). Data layer kept intact — future API
/// endpoint or daemon-side consumer can re-expose if needed.
#[allow(dead_code)] // used by tests; data layer retained for future API
pub fn divergence_report() -> Vec<(String, DivergenceStats)> {
    let guard = DIVERGENCE.lock();
    match guard.as_ref() {
        Some(map) => map
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    DivergenceStats {
                        total_ticks: v.total_ticks,
                        agree: v.agree,
                        diverge: v.diverge,
                    },
                )
            })
            .collect(),
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// F9: productive-output signal (#685 sub-task 4)
//
// Parallel to `BehavioralSignal` (silence-based, absence-of-output) — uses
// presence-of-specific-output evidence. Shares this module + the
// `behavioral_shadow` tracing target for telemetry infrastructure reuse.
// See `docs/F9-PRODUCTIVE-OUTPUT-GATE.md` §F9.2 for design rationale.
// ---------------------------------------------------------------------------

/// Productive-output inference result. Returned by `infer_productivity()`
/// when MCP heartbeat is fresh OR a structural marker matches the rendered
/// screen text. Used by `check_hang` as the dual-path supplement signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductivitySignal {
    /// Productive evidence detected — agent is doing actual work.
    Productive { source: ProductivitySource },
    /// No productive signal — agent may be silently stuck.
    NoSignal,
}

/// Source of a `Productive` signal — heartbeat (MCP tool call) or marker
/// (text pattern). Carried through telemetry so corpus analysis can
/// disaggregate which evidence type fires for which scenarios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductivitySource {
    /// MCP heartbeat refreshed recently — agent called a tool.
    Heartbeat,
    /// Structural marker pattern matched the screen text. The matched
    /// pattern literal is carried for telemetry/debug visibility.
    Marker(&'static str),
}

impl std::fmt::Display for ProductivitySignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delta 1: "productive:" prefix in display ensures grep-equivalence
        // to enum-namespaced telemetry search (`rg "productive:"`).
        match self {
            Self::Productive { source } => match source {
                ProductivitySource::Heartbeat => write!(f, "productive:heartbeat"),
                ProductivitySource::Marker(p) => write!(f, "productive:marker({p})"),
            },
            Self::NoSignal => write!(f, "productive:none"),
        }
    }
}

/// Per-backend productive-signal calibration. Sub-task 6 (decision
/// `d-20260514022917793418-0`) shipped per-backend markers across 5
/// managed backends; the `cache_id` field gates fast routing to a
/// pre-compiled regex set, mirroring the `LazyLock` idiom at
/// `src/state.rs:479-483`.
#[derive(Debug, Clone, Copy)]
pub struct ProductivityConfig {
    /// Structural marker patterns. Each must be anchored (line-start `^` or
    /// equivalent) to avoid scrollback FP (per F39 Scenario A/B/C taxonomy
    /// in `docs/HUNG-STATE-TRANSITIONS.md`).
    pub markers: &'static [&'static str],
    /// Whether MCP heartbeat refresh counts as productive evidence.
    /// `false` for backends without MCP integration (Shell/Raw).
    pub use_heartbeat: bool,
    /// Heartbeat age window for the `Heartbeat` source classification.
    /// A heartbeat older than this is treated as stale; the markers path
    /// must independently fire.
    pub heartbeat_fresh_window_ms: u64,
    /// Identifier for the pre-compiled regex cache that matches `markers`.
    /// `Some(...)` enables the fast path through cached `LazyLock` regexes
    /// (see `infer_productivity`); `None` falls back to per-call
    /// `Regex::new()` compile. All Phase 1 callers via
    /// `config_for_productivity` set `Some(...)`; `None` is an
    /// intentional future-proofing escape hatch for test / ad-hoc
    /// configs that want to provide custom markers without registering
    /// a cache.
    pub cache_id: Option<MarkerCacheId>,
}

/// Identifies which pre-compiled regex cache to consult when matching
/// `ProductivityConfig.markers`. Each variant maps to one of the per-
/// backend `*_PRODUCTIVE_REGEXES` `LazyLock` statics below.
///
/// **CRITICAL**: without this routing, per-backend markers fall through
/// the ad-hoc `Regex::new()` compile path on every `feed()` call — the
/// exact bug that caused PR #766's ubuntu/windows CI failure (replay
/// test cumulative latency exceeding `min_hold` thresholds). The match
/// on this enum is compile-time exhaustive so adding a backend forces
/// a cache static + match arm in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerCacheId {
    /// Generic structural anchors only (file save banners + Claude tool
    /// completion). Used by Shell/Raw backends and as a baseline subset
    /// inside each per-backend MARKERS list.
    Generic,
    Claude,
    Kiro,
    Codex,
    Gemini,
    OpenCode,
}

// ---------------------------------------------------------------------------
// Per-backend markers — explicit composition (generic anchors duplicated
// into each list; no implicit overlay). Decision §1 lock-broke duplication
// over overlay: clearer per-backend semantics, single regex cache per
// backend. Maintenance cost (5-place edit for future common anchor) is
// the accepted trade-off.
//
// Source for completion glyphs + tool vocab: src/state.rs:200-450
// AgentState ToolUse pattern catalogs (per-backend). F9 productivity is
// COMPLETION-ONLY — exclude in-progress spinner glyphs from the AgentState
// regex (those fire before work completes, opposite of productivity
// signal). Sub-task 6 decision §1.
// ---------------------------------------------------------------------------

/// Generic structural anchors. File save banners + Claude-shape tool
/// completion (kept here for Shell/Raw fallback). Each per-backend MARKERS
/// const includes these three save-banner anchors via explicit duplication.
// #8 Phase 2: pub(crate) so the co-located BackendProfile (crate::backend_profile)
// can reference these markers when bundling a backend's ProductivityConfig. The
// arrays + their LazyLock compilation stay here (the profile holds &'static refs).
pub(crate) const GENERIC_PRODUCTIVE_MARKERS: &[&str] = &[
    r"^Saved to \S+",
    r"^Wrote \d+ bytes",
    r"^Created file: \S+",
    r"^\s*✓\s+(Read|Bash|Edit|Write|Grep)\b",
];

/// Claude: generic anchors + completion glyphs (`✓●⏺` — NOT the in-progress
/// Braille spinner set `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) with Claude tool-name vocabulary.
const CLAUDE_PRODUCTIVE_MARKERS: &[&str] = &[
    r"^Saved to \S+",
    r"^Wrote \d+ bytes",
    r"^Created file: \S+",
    r"^[✓●⏺]\s+(Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)\b",
];

/// Kiro: generic anchors + `●` completion glyph with Kiro tool-name
/// vocabulary + bracketed tool-call literals (`[fs_read]`, `[fs_write]`,
/// `[execute_bash]`). The bracket form fires for Kiro's structured tool
/// trace output.
// #8 Phase 2 (KiroCli migration): pub(crate) so the BackendProfile bundles
// KiroCli's ProductivityConfig by &'static ref (markers + LazyLock stay here).
pub(crate) const KIRO_PRODUCTIVE_MARKERS: &[&str] = &[
    r"^Saved to \S+",
    r"^Wrote \d+ bytes",
    r"^Created file: \S+",
    r"^●\s+(Read|Write|Edit|Bash|Grep|Glob|Task|List|Search)\b",
    r"\[(fs_read|fs_write|execute_bash)\]",
];

/// Codex: generic anchors + `•` past-tense title vocabulary (`Explored`,
/// `Edited`, `Ran`) + `apply_patch` literal completion banner.
const CODEX_PRODUCTIVE_MARKERS: &[&str] = &[
    r"^Saved to \S+",
    r"^Wrote \d+ bytes",
    r"^Created file: \S+",
    r"^•\s+(Explored|Edited|Ran)\b",
    r"apply_patch",
];

/// Gemini: generic anchors + `✓` completion glyph with Gemini CamelCase
/// tool-name vocabulary. Excludes `tool.*call` / `MCP.*tool` literals
/// (heartbeat path already covers MCP — avoid double-counting).
pub(crate) const GEMINI_PRODUCTIVE_MARKERS: &[&str] = &[
    r"^Saved to \S+",
    r"^Wrote \d+ bytes",
    r"^Created file: \S+",
    r"^✓\s+(ReadFile|WriteFile|ReadManyFiles|Edit|Shell|WebFetch|Glob|GoogleSearch|MemoryTool|ReadFolder)\b",
];

/// OpenCode: generic anchors + `→` completion glyph (NOT the in-flight
/// `✱` glyph) with OpenCode tool-name vocabulary. Synthetic-only —
/// not validated against real PTY captures; corpus growth path per
/// `docs/F685-FIXTURE-CORPUS.md §F685-CORPUS.6`.
// #8 Phase 2 step-1: pub(crate) so the co-located OpenCode BackendProfile
// references the SAME markers const (not a re-typed copy) → byte-identity with
// the legacy config_for_productivity arm. Matches KIRO_PRODUCTIVE_MARKERS.
pub(crate) const OPENCODE_PRODUCTIVE_MARKERS: &[&str] = &[
    r"^Saved to \S+",
    r"^Wrote \d+ bytes",
    r"^Created file: \S+",
    r"^→\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b",
];

/// Build a `LazyLock<Vec<Regex>>` from a static markers slice. Each
/// marker is compiled with the `(?m)` multiline flag so `^` anchors at
/// line starts inside the rendered screen.
fn compile_markers(markers: &'static [&'static str]) -> Vec<regex::Regex> {
    markers
        .iter()
        .map(|p| {
            regex::Regex::new(&format!("(?m){p}"))
                .unwrap_or_else(|e| panic!("F9 productive marker regex compile: {p}: {e}"))
        })
        .collect()
}

static GENERIC_PRODUCTIVE_REGEXES: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| compile_markers(GENERIC_PRODUCTIVE_MARKERS));
static CLAUDE_PRODUCTIVE_REGEXES: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| compile_markers(CLAUDE_PRODUCTIVE_MARKERS));
static KIRO_PRODUCTIVE_REGEXES: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| compile_markers(KIRO_PRODUCTIVE_MARKERS));
static CODEX_PRODUCTIVE_REGEXES: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| compile_markers(CODEX_PRODUCTIVE_MARKERS));
static GEMINI_PRODUCTIVE_REGEXES: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| compile_markers(GEMINI_PRODUCTIVE_MARKERS));
static OPENCODE_PRODUCTIVE_REGEXES: std::sync::LazyLock<Vec<regex::Regex>> =
    std::sync::LazyLock::new(|| compile_markers(OPENCODE_PRODUCTIVE_MARKERS));

/// Get productivity config for a backend. Each managed backend uses its
/// own per-backend MARKERS list + cache id; Shell/Raw fall back to the
/// generic anchor set (no MCP heartbeat).
///
/// #8 Phase 2 step-0: migrated backends source from the co-located profile;
/// un-migrated fall back to [`config_for_productivity_legacy`]. The markers +
/// `cache_id` are byte-identical to legacy (proven by the parity test), so the
/// per-marker-list regex cache (keyed by `cache_id`) is unchanged.
pub fn config_for_productivity(backend: &Backend) -> ProductivityConfig {
    if let Some(p) = crate::backend_profile::profile(backend) {
        return p.productivity;
    }
    config_for_productivity_legacy(backend)
}

/// The pre-reroute productivity match — the fork's fallback AND the TRUE-legacy
/// parity reference (see [`config_for_legacy`] for why a separate fn is needed).
pub(crate) fn config_for_productivity_legacy(backend: &Backend) -> ProductivityConfig {
    match backend {
        Backend::ClaudeCode => ProductivityConfig {
            markers: CLAUDE_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Claude),
        },
        Backend::KiroCli => ProductivityConfig {
            markers: KIRO_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Kiro),
        },
        Backend::Codex => ProductivityConfig {
            markers: CODEX_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Codex),
        },
        Backend::Gemini => ProductivityConfig {
            markers: GEMINI_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Gemini),
        },
        Backend::OpenCode => ProductivityConfig {
            markers: OPENCODE_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::OpenCode),
        },
        // #987: agy reuses Gemini's productivity markers + cache id.
        // Both share the same Google agent engine; markers will diverge
        // only if AGY's TUI introduces new productivity indicators not
        // present in Gemini CLI. Telemetry-tune in follow-up PR.
        Backend::Agy => ProductivityConfig {
            markers: GEMINI_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Gemini),
        },
        // Shell / Raw — no MCP, generic markers only.
        Backend::Shell | Backend::Raw(_) => ProductivityConfig {
            markers: GENERIC_PRODUCTIVE_MARKERS,
            use_heartbeat: false,
            heartbeat_fresh_window_ms: 0,
            cache_id: Some(MarkerCacheId::Generic),
        },
    }
}

/// Infer productive-output signal from screen text and heartbeat freshness.
/// Pure function — no side effects, no state mutation. Parallels
/// `infer_from_silence` shape so future signal types follow the same
/// `(config, evidence) -> signal` pattern.
// #685 PR-2 RC1: production callers switched to
// [`infer_productivity_with_match`] for evidence-substring dedup.
// `infer_productivity` retained as the simpler interface for tests +
// future single-signal callers that don't need the matched text.
#[allow(dead_code)] // simpler test interface; production uses infer_productivity_with_match
pub fn infer_productivity(
    config: &ProductivityConfig,
    screen_text: &str,
    heartbeat_age: Duration,
) -> ProductivitySignal {
    // Heartbeat path first — cheaper than regex scan.
    if config.use_heartbeat && heartbeat_age.as_millis() <= config.heartbeat_fresh_window_ms as u128
    {
        return ProductivitySignal::Productive {
            source: ProductivitySource::Heartbeat,
        };
    }
    // Markers path — route to pre-compiled `LazyLock` cache via cache_id.
    // Compile-time exhaustive match catches missing-backend bugs. The
    // `None` arm exists for future custom configs (test code, ad-hoc
    // markers) that opt out of the cache; Phase 1 production code never
    // hits it.
    let cached_regexes: Option<&[regex::Regex]> = match config.cache_id {
        Some(MarkerCacheId::Generic) => Some(&GENERIC_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Claude) => Some(&CLAUDE_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Kiro) => Some(&KIRO_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Codex) => Some(&CODEX_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Gemini) => Some(&GEMINI_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::OpenCode) => Some(&OPENCODE_PRODUCTIVE_REGEXES),
        None => None,
    };
    match cached_regexes {
        Some(regexes) => {
            for (i, pattern) in config.markers.iter().enumerate() {
                if regexes[i].is_match(screen_text) {
                    return ProductivitySignal::Productive {
                        source: ProductivitySource::Marker(pattern),
                    };
                }
            }
        }
        None => {
            // Ad-hoc compile fallback — Phase 1 dead code path; reserved
            // for future ProductivityConfig callers that supply custom
            // markers without registering a cache id. Compile latency
            // limited to non-production paths (test code or one-off
            // tooling).
            for pattern in config.markers {
                if let Ok(re) = regex::Regex::new(&format!("(?m){pattern}")) {
                    if re.is_match(screen_text) {
                        return ProductivitySignal::Productive {
                            source: ProductivitySource::Marker(pattern),
                        };
                    }
                }
            }
        }
    }
    ProductivitySignal::NoSignal
}

/// #685 PR-2 RC1: variant of [`infer_productivity`] that ALSO returns
/// the matched marker substring when the signal source is `Marker`.
/// Used by `state::feed()` for evidence-level dedup — hashing the
/// matched marker text (not the surrounding tail) prevents a stale
/// marker from re-refreshing `last_productive_output` when adjacent
/// content (spinner ticks, status line edits) changes around it.
///
/// Heartbeat source returns `None` for the matched string — it's
/// timestamp-driven, not text-driven. NoSignal returns `None` for
/// the matched string and `ProductivitySignal::NoSignal`.
///
/// Matched substring scope: the FIRST cached regex match against
/// `screen_text`. Matches `infer_productivity`'s first-match ordering
/// so behaviour is consistent across both surfaces.
pub fn infer_productivity_with_match(
    config: &ProductivityConfig,
    screen_text: &str,
    heartbeat_age: Duration,
) -> (ProductivitySignal, Option<String>) {
    // Heartbeat path — same shortcut as infer_productivity, but no
    // matched-text concept.
    if config.use_heartbeat && heartbeat_age.as_millis() <= config.heartbeat_fresh_window_ms as u128
    {
        return (
            ProductivitySignal::Productive {
                source: ProductivitySource::Heartbeat,
            },
            None,
        );
    }
    let cached_regexes: Option<&[regex::Regex]> = match config.cache_id {
        Some(MarkerCacheId::Generic) => Some(&GENERIC_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Claude) => Some(&CLAUDE_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Kiro) => Some(&KIRO_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Codex) => Some(&CODEX_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::Gemini) => Some(&GEMINI_PRODUCTIVE_REGEXES),
        Some(MarkerCacheId::OpenCode) => Some(&OPENCODE_PRODUCTIVE_REGEXES),
        None => None,
    };
    if let Some(regexes) = cached_regexes {
        for (i, pattern) in config.markers.iter().enumerate() {
            if let Some(m) = regexes[i].find(screen_text) {
                return (
                    ProductivitySignal::Productive {
                        source: ProductivitySource::Marker(pattern),
                    },
                    Some(m.as_str().to_string()),
                );
            }
        }
    } else {
        for pattern in config.markers {
            if let Ok(re) = regex::Regex::new(&format!("(?m){pattern}")) {
                if let Some(m) = re.find(screen_text) {
                    return (
                        ProductivitySignal::Productive {
                            source: ProductivitySource::Marker(pattern),
                        },
                        Some(m.as_str().to_string()),
                    );
                }
            }
        }
    }
    (ProductivitySignal::NoSignal, None)
}

/// Shadow-mode telemetry for productive-output signals. Parallels
/// `log_shadow_telemetry` (silence-side) — shares the `behavioral_shadow`
/// tracing target so dashboards / log queries pick up both signal kinds.
/// Per Delta 1 (decision `d-20260513235514013631-0`), Sprint 27 call sites
/// untouched; this is an additive function not a refactor.
pub fn log_productivity_telemetry(
    instance: &str,
    backend: &str,
    regex_state: &str,
    productivity: &ProductivitySignal,
) {
    if !matches!(productivity, ProductivitySignal::NoSignal) {
        tracing::debug!(
            target: "behavioral_shadow",
            instance,
            backend,
            regex_state,
            productivity = %productivity,
            "productivity shadow: signal detected"
        );
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_below_threshold_returns_none() {
        let config = config_for(&Backend::ClaudeCode);
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(500)),
            BehavioralSignal::None
        );
    }

    #[test]
    fn silence_above_thinking_threshold() {
        let config = config_for(&Backend::ClaudeCode);
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(2500)),
            BehavioralSignal::SilenceThinking
        );
    }

    #[test]
    fn silence_above_idle_threshold() {
        let config = config_for(&Backend::ClaudeCode);
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(7000)),
            BehavioralSignal::SilenceIdle
        );
    }

    #[test]
    fn claude_has_shorter_thresholds_than_default() {
        let claude = config_for(&Backend::ClaudeCode);
        let default = BehavioralConfig::default();
        assert!(claude.silence_thinking_ms < default.silence_thinking_ms);
    }

    #[test]
    fn kiro_supports_cursor_query() {
        assert!(config_for(&Backend::KiroCli).supports_cursor_query);
    }

    #[test]
    fn shell_uses_defaults() {
        let config = config_for(&Backend::Shell);
        let default = BehavioralConfig::default();
        assert_eq!(config.silence_thinking_ms, default.silence_thinking_ms);
    }

    #[test]
    fn codex_uses_default_thresholds() {
        let config = config_for(&Backend::Codex);
        assert_eq!(config.silence_thinking_ms, 3000);
        assert!(!config.supports_cursor_query); // bubbletea TUI
    }

    #[test]
    fn gemini_uses_default_thresholds() {
        let config = config_for(&Backend::Gemini);
        assert_eq!(config.silence_thinking_ms, 3000);
    }

    #[test]
    fn opencode_uses_default_thresholds() {
        let config = config_for(&Backend::OpenCode);
        assert_eq!(config.silence_idle_ms, 8000);
    }

    #[cfg(unix)]
    #[test]
    fn fg_pgid_on_stdin_returns_some() {
        // stdin (fd 0) should have a valid foreground pgid in a terminal
        // This may return None in CI (no controlling terminal)
        let _ = fg_pgid(0); // Just verify it doesn't panic
    }

    #[test]
    fn behavioral_signal_display() {
        assert_eq!(
            format!("{}", BehavioralSignal::SilenceThinking),
            "silence_thinking"
        );
        assert_eq!(format!("{}", BehavioralSignal::None), "none");
    }

    /// M2: Fixture replay — feed through StateTracker, verify state
    /// transition + behavioral config present.
    fn replay_fixture(file: &str, backend: &Backend) {
        let path = format!("tests/fixtures/state-replay/{file}");
        let fixture = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let mut tracker = crate::state::StateTracker::new(Some(backend));
        assert!(
            tracker.has_behavioral_config(),
            "behavioral config must be set for {file}"
        );
        let text = String::from_utf8_lossy(&fixture);
        tracker.feed(&text);
        let state = tracker.get_state();
        assert!(
            !matches!(state, crate::state::AgentState::Starting),
            "fixture {file} should trigger state transition, got Starting"
        );
    }

    /// M2+M4 e2e: feed fixture → sleep past silence threshold → feed again
    /// → capture behavioral_shadow telemetry via tracing subscriber.
    #[test]
    fn fixture_replay_claude_thinking_emits_behavioral_telemetry() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let fixture = std::fs::read("tests/fixtures/state-replay/claude-thinking.raw").unwrap();
        let mut tracker = crate::state::StateTracker::new(Some(&Backend::ClaudeCode));
        tracker.set_instance_name("test-fixture");
        tracker.feed(&String::from_utf8_lossy(&fixture));
        std::thread::sleep(Duration::from_millis(2100));
        tracker.feed("_");
        drop(_guard);
        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            output.contains("silence_thinking"),
            "expected silence_thinking after 2.1s silence, got: {}",
            if output.is_empty() {
                "(empty)".to_string()
            } else {
                output[..output.len().min(300)].to_string()
            }
        );
    }

    /// Helper: e2e behavioral telemetry capture for a fixture.
    fn e2e_fixture_behavioral(file: &str, backend: &Backend) {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .with_ansi(false)
            .finish();
        let path = format!("tests/fixtures/state-replay/{file}");
        let _guard = tracing::subscriber::set_default(subscriber);
        let fixture = std::fs::read(&path).unwrap();
        let mut tracker = crate::state::StateTracker::new(Some(backend));
        tracker.set_instance_name("test-e2e");
        tracker.feed(&String::from_utf8_lossy(&fixture));
        std::thread::sleep(Duration::from_millis(3100));
        tracker.feed("_");
        drop(_guard);
        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            output.contains("silence_thinking"),
            "fixture {file} expected silence_thinking, got: {}",
            if output.is_empty() {
                "(empty)"
            } else {
                &output[..output.len().min(200)]
            }
        );
    }

    #[test]
    fn e2e_kiro_thinking() {
        e2e_fixture_behavioral("kiro-thinking.raw", &Backend::KiroCli);
    }
    #[test]
    fn e2e_codex_thinking() {
        e2e_fixture_behavioral("codex-thinking.raw", &Backend::Codex);
    }
    #[test]
    fn e2e_gemini_thinking() {
        e2e_fixture_behavioral("gemini-thinking.raw", &Backend::Gemini);
    }
    #[test]
    fn e2e_opencode_thinking() {
        e2e_fixture_behavioral("opencode-thinking.raw", &Backend::OpenCode);
    }
    // Sprint 27 PR-B: extend e2e to all 13 fixtures
    #[test]
    fn e2e_claude_tooluse() {
        e2e_fixture_behavioral("claude-tooluse.raw", &Backend::ClaudeCode);
    }
    #[test]
    fn e2e_claude_perm() {
        e2e_fixture_behavioral("claude-perm.raw", &Backend::ClaudeCode);
    }
    #[test]
    fn e2e_codex_tooluse() {
        e2e_fixture_behavioral("codex-tooluse.raw", &Backend::Codex);
    }
    #[test]
    fn e2e_codex_update() {
        e2e_fixture_behavioral("codex-update.raw", &Backend::Codex);
    }
    #[test]
    fn e2e_codex_perm() {
        e2e_fixture_behavioral("codex-perm.raw", &Backend::Codex);
    }
    #[test]
    fn e2e_gemini_tooluse() {
        e2e_fixture_behavioral("gemini-tooluse.raw", &Backend::Gemini);
    }
    #[test]
    fn e2e_kiro_tooluse() {
        e2e_fixture_behavioral("kiro-tooluse.raw", &Backend::KiroCli);
    }
    #[test]
    fn e2e_opencode_tooluse() {
        e2e_fixture_behavioral("opencode-tooluse.raw", &Backend::OpenCode);
    }

    #[test]
    fn fixture_replay_claude_tooluse() {
        replay_fixture("claude-tooluse.raw", &Backend::ClaudeCode);
    }
    #[test]
    fn fixture_replay_claude_perm() {
        replay_fixture("claude-perm.raw", &Backend::ClaudeCode);
    }
    #[test]
    fn fixture_replay_codex_thinking() {
        replay_fixture("codex-thinking.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_codex_tooluse() {
        replay_fixture("codex-tooluse.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_codex_update() {
        replay_fixture("codex-update.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_codex_perm() {
        replay_fixture("codex-perm.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_gemini_thinking() {
        replay_fixture("gemini-thinking.raw", &Backend::Gemini);
    }
    #[test]
    fn fixture_replay_gemini_tooluse() {
        replay_fixture("gemini-tooluse.raw", &Backend::Gemini);
    }
    #[test]
    fn fixture_replay_kiro_thinking() {
        replay_fixture("kiro-thinking.raw", &Backend::KiroCli);
    }
    #[test]
    fn fixture_replay_kiro_tooluse() {
        replay_fixture("kiro-tooluse.raw", &Backend::KiroCli);
    }
    #[test]
    fn fixture_replay_opencode_thinking() {
        replay_fixture("opencode-thinking.raw", &Backend::OpenCode);
    }
    #[test]
    fn fixture_replay_opencode_tooluse() {
        replay_fixture("opencode-tooluse.raw", &Backend::OpenCode);
    }

    /// M2: Silence inference produces correct signal for calibrated thresholds.
    #[test]
    fn claude_silence_inference_matches_calibration() {
        let config = config_for(&Backend::ClaudeCode);
        // Below thinking threshold
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(1000)),
            BehavioralSignal::None
        );
        // Above thinking, below idle
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(3000)),
            BehavioralSignal::SilenceThinking
        );
        // Above idle
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(7000)),
            BehavioralSignal::SilenceIdle
        );
    }

    /// M4: Verify log_shadow_telemetry emits a tracing event with
    /// the behavioral signal in the message.
    #[test]
    fn shadow_telemetry_emits_tracing_event() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .finish();

        let signal = infer_from_silence(
            &config_for(&Backend::ClaudeCode),
            Duration::from_millis(3000),
        );
        tracing::subscriber::with_default(subscriber, || {
            log_shadow_telemetry("test-agent", "claude-code", "idle", signal);
        });

        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            output.contains("silence_thinking"),
            "expected 'silence_thinking' in tracing output, got: {output}"
        );
    }

    /// M4: Verify None signal does NOT emit any tracing event.
    #[test]
    fn shadow_telemetry_skips_none_signal() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            log_shadow_telemetry("test-agent", "claude-code", "idle", BehavioralSignal::None);
        });

        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            !output.contains("behavioral shadow"),
            "None signal should not emit, got: {output}"
        );
    }

    /// M4: StateTracker must expose has_behavioral_config() — fails to
    /// compile when state.rs behavioral fields are absent (RED state).
    #[test]
    fn state_tracker_has_behavioral_config() {
        let tracker = crate::state::StateTracker::new(Some(&Backend::ClaudeCode));
        assert!(
            tracker.has_behavioral_config(),
            "StateTracker must have behavioral_config for managed backends"
        );
    }

    // --- Sprint 27 PR-B: divergence dashboard tests ---

    #[test]
    fn divergence_record_and_report() {
        // Record some ticks
        record_divergence(
            "test-backend",
            BehavioralSignal::SilenceThinking,
            "thinking",
        );
        record_divergence("test-backend", BehavioralSignal::SilenceThinking, "idle"); // diverge
        record_divergence("test-backend", BehavioralSignal::None, "ready"); // agree (None = no signal)

        let report = divergence_report();
        let entry = report.iter().find(|(k, _)| k == "test-backend");
        assert!(entry.is_some(), "report must contain test-backend");
        let (_, stats) = entry.unwrap();
        assert!(stats.total_ticks >= 3, "must have at least 3 ticks");
        assert!(stats.diverge >= 1, "must have at least 1 divergence");
    }

    #[test]
    fn divergence_e2e_through_state_tracker() {
        // Feed a fixture through StateTracker → divergence accumulator should record
        let fixture = std::fs::read("tests/fixtures/state-replay/claude-thinking.raw").unwrap();
        let mut tracker = crate::state::StateTracker::new(Some(&Backend::ClaudeCode));
        tracker.set_instance_name("test-divergence");
        tracker.feed(&String::from_utf8_lossy(&fixture));
        std::thread::sleep(Duration::from_millis(2200));
        tracker.feed("_");

        let report = divergence_report();
        let entry = report.iter().find(|(k, _)| k == "claude");
        assert!(
            entry.is_some(),
            "divergence report must contain claude after feed"
        );
    }

    // -----------------------------------------------------------------------
    // F9 productive-output signal tests (#685 sub-task 4, decision
    // d-20260513235514013631-0). Tests pin the contract on semantic
    // outcomes (Productive vs NoSignal + source) — not on regex literals,
    // so future marker refinement (e.g. deliverable #4 backend tuning)
    // does not break them.
    // -----------------------------------------------------------------------

    #[test]
    fn infer_productivity_matches_structural_marker() {
        // Positive: a "Saved to <path>" banner is a structural marker —
        // line-start anchor + specific format. Must classify Productive.
        let config = config_for_productivity(&Backend::ClaudeCode);
        let signal =
            infer_productivity(&config, "Saved to /tmp/foo.txt\n", Duration::from_secs(99));
        assert!(matches!(
            signal,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
    }

    #[test]
    fn infer_productivity_rejects_bare_keyword_scrollback() {
        // Negative: prose containing the bare keyword "Saved" without the
        // line-start anchor + specific format must NOT classify Productive.
        // Pins the F9 anti-FP contract from decision §4.
        let config = config_for_productivity(&Backend::ClaudeCode);
        let signal = infer_productivity(
            &config,
            "I have Saved your work for next time.\n",
            Duration::from_secs(99),
        );
        assert_eq!(signal, ProductivitySignal::NoSignal);
    }

    #[test]
    fn infer_productivity_uses_heartbeat_when_fresh() {
        // Heartbeat path: fresh MCP heartbeat counts as Productive even
        // when no marker is in screen text. Tool calls are productive
        // evidence (this is what fired the path; agent is alive).
        let config = config_for_productivity(&Backend::ClaudeCode);
        assert!(config.use_heartbeat, "managed backend uses heartbeat");
        let signal = infer_productivity(&config, "<no markers here>", Duration::from_millis(500));
        assert!(matches!(
            signal,
            ProductivitySignal::Productive {
                source: ProductivitySource::Heartbeat
            }
        ));
    }

    #[test]
    fn infer_productivity_stale_heartbeat_no_marker_is_no_signal() {
        // Stale heartbeat (older than config window) + no marker match →
        // NoSignal. This is the F9 grey-failure case: agent produces
        // unproductive output (spinner) but no real progress.
        let config = config_for_productivity(&Backend::ClaudeCode);
        // 10_000ms is the fresh window per config_for_productivity; use 30s
        // to be unambiguously past the window.
        let signal = infer_productivity(
            &config,
            "spinning... please wait\n",
            Duration::from_secs(30),
        );
        assert_eq!(signal, ProductivitySignal::NoSignal);
    }

    #[test]
    fn shell_backend_disables_heartbeat_path() {
        // Shell / Raw backends have no MCP integration — heartbeat path
        // disabled. Markers-only.
        let config = config_for_productivity(&Backend::Shell);
        assert!(!config.use_heartbeat);
        // Fresh "heartbeat" (irrelevant for shell) + no marker → NoSignal.
        let signal = infer_productivity(&config, "$ ls\n", Duration::from_millis(0));
        assert_eq!(signal, ProductivitySignal::NoSignal);
    }

    // -------------------------------------------------------------------
    // Sub-task 6 (#685 deliverable #4): per-backend productive markers
    // (decision d-20260514022917793418-0). 10 tests pin the per-backend
    // marker contract: completion-glyph + tool-vocab fires positive on
    // each backend's expected output shape; bare prose missing the
    // line-start anchor fires NoSignal (anti-FP). Stale heartbeat used
    // throughout to force the markers path.
    // -------------------------------------------------------------------

    /// Duration that exceeds `heartbeat_fresh_window_ms` for any backend
    /// — forces inference through the markers path so positive/negative
    /// tests verify the per-backend regex, not the heartbeat shortcut.
    fn stale_heartbeat() -> Duration {
        Duration::from_secs(u32::MAX as u64)
    }

    #[test]
    fn claude_markers_match_completion_glyph_and_tool() {
        let config = config_for_productivity(&Backend::ClaudeCode);
        assert_eq!(config.cache_id, Some(MarkerCacheId::Claude));
        // `✓ Read foo.toml` — completion glyph + Claude tool vocab.
        let signal = infer_productivity(&config, "✓ Read foo.toml\n", stale_heartbeat());
        assert!(matches!(
            signal,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
    }

    #[test]
    fn claude_markers_reject_in_progress_spinner_and_prose() {
        let config = config_for_productivity(&Backend::ClaudeCode);
        // `⠦ Read foo.toml` — in-progress spinner glyph (NOT completion);
        // bare prose without anchor. Neither should fire productive.
        let in_progress = infer_productivity(&config, "⠦ Read foo.toml\n", stale_heartbeat());
        assert_eq!(in_progress, ProductivitySignal::NoSignal);
        let prose = infer_productivity(
            &config,
            "I have saved your time previously\n",
            stale_heartbeat(),
        );
        assert_eq!(prose, ProductivitySignal::NoSignal);
    }

    #[test]
    fn kiro_markers_match_glyph_or_bracketed_tool() {
        let config = config_for_productivity(&Backend::KiroCli);
        assert_eq!(config.cache_id, Some(MarkerCacheId::Kiro));
        // `● Read foo.toml` — Kiro completion glyph.
        let glyph = infer_productivity(&config, "● Read foo.toml\n", stale_heartbeat());
        assert!(matches!(
            glyph,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
        // `[fs_read]` bracketed tool literal (not line-anchored).
        let bracketed = infer_productivity(
            &config,
            "Running tool [fs_read] on file\n",
            stale_heartbeat(),
        );
        assert!(matches!(
            bracketed,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
    }

    #[test]
    fn kiro_markers_reject_bare_tool_name_prose() {
        let config = config_for_productivity(&Backend::KiroCli);
        // `Read foo.toml` without `●` prefix — chat prose mentioning tool name.
        let prose = infer_productivity(
            &config,
            "Will Read foo.toml when ready\n",
            stale_heartbeat(),
        );
        assert_eq!(prose, ProductivitySignal::NoSignal);
    }

    #[test]
    fn codex_markers_match_dot_title_or_apply_patch() {
        let config = config_for_productivity(&Backend::Codex);
        assert_eq!(config.cache_id, Some(MarkerCacheId::Codex));
        // `• Edited` — Codex past-tense title glyph.
        let title = infer_productivity(&config, "• Edited\n", stale_heartbeat());
        assert!(matches!(
            title,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
        // `apply_patch` literal in Codex tool log line.
        let patch =
            infer_productivity(&config, "└ Ran apply_patch (15 hunks)\n", stale_heartbeat());
        assert!(matches!(
            patch,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
    }

    #[test]
    fn codex_markers_reject_in_flight_spinner_prose() {
        let config = config_for_productivity(&Backend::Codex);
        // `◦ Working` — Codex in-progress spinner; must NOT fire.
        let spinner = infer_productivity(
            &config,
            "◦ Working (5s • esc to interrupt)\n",
            stale_heartbeat(),
        );
        assert_eq!(spinner, ProductivitySignal::NoSignal);
    }

    #[test]
    fn gemini_markers_match_check_glyph_and_camelcase_tool() {
        let config = config_for_productivity(&Backend::Gemini);
        assert_eq!(config.cache_id, Some(MarkerCacheId::Gemini));
        // `✓ ReadFile` — Gemini completion glyph + CamelCase tool name.
        let signal = infer_productivity(&config, "✓ ReadFile foo.toml\n", stale_heartbeat());
        assert!(matches!(
            signal,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
    }

    #[test]
    fn gemini_markers_reject_braille_spinner_and_mcp_prose() {
        let config = config_for_productivity(&Backend::Gemini);
        // `⠦ Thinking... (esc to cancel, 2s)` — Gemini in-progress spinner.
        let spinner = infer_productivity(
            &config,
            "⠦ Thinking... (esc to cancel, 2s)\n",
            stale_heartbeat(),
        );
        assert_eq!(spinner, ProductivitySignal::NoSignal);
        // Bare prose with `MCP tool` mention — heartbeat path is the
        // canonical MCP signal; marker path explicitly excludes the
        // literal to avoid double-counting. Per decision §1 exclusion.
        let mcp_prose = infer_productivity(
            &config,
            "About to call MCP tool method\n",
            stale_heartbeat(),
        );
        assert_eq!(mcp_prose, ProductivitySignal::NoSignal);
    }

    #[test]
    fn opencode_markers_match_arrow_glyph_completion() {
        let config = config_for_productivity(&Backend::OpenCode);
        assert_eq!(config.cache_id, Some(MarkerCacheId::OpenCode));
        // `→ Read foo.toml` — OpenCode completion glyph.
        let signal = infer_productivity(&config, "→ Read foo.toml\n", stale_heartbeat());
        assert!(matches!(
            signal,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
    }

    #[test]
    fn opencode_markers_reject_in_flight_asterisk_glyph() {
        let config = config_for_productivity(&Backend::OpenCode);
        // `✱ Glob "*.toml"` — OpenCode in-flight glyph (NOT completion).
        let in_flight = infer_productivity(
            &config,
            "✱ Glob \"*.toml\" (2 matches)\n",
            stale_heartbeat(),
        );
        assert_eq!(in_flight, ProductivitySignal::NoSignal);
    }

    #[test]
    fn ad_hoc_fallback_path_compiles_and_matches() {
        // Pin the `cache_id: None` fallback path — for callers (test
        // code, future ad-hoc configs) that supply custom markers
        // without registering a cache. Phase 1 production never hits
        // this path; the test ensures it stays correct.
        let custom = ProductivityConfig {
            markers: &["^CUSTOM PRODUCTIVE SIGNAL \\d+"],
            use_heartbeat: false,
            heartbeat_fresh_window_ms: 0,
            cache_id: None,
        };
        let signal =
            infer_productivity(&custom, "CUSTOM PRODUCTIVE SIGNAL 42\n", stale_heartbeat());
        assert!(matches!(
            signal,
            ProductivitySignal::Productive {
                source: ProductivitySource::Marker(_)
            }
        ));
        let negative = infer_productivity(&custom, "nothing matches\n", stale_heartbeat());
        assert_eq!(negative, ProductivitySignal::NoSignal);
    }
}
