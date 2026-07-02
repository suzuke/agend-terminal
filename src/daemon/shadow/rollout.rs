//! #2413 Phase D — codex rollout-tail observer source.
//!
//! Codex (TUI mode) live-flushes its session to
//! `~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<uuid>.jsonl` DURING a turn (confirm-first
//! verified: `function_call`/`response_item` records appear mid-turn, before
//! `task_complete`). This module is a strictly **READ-ONLY tail** of those files: each
//! appended JSONL record → [`Evidence`] (`authority=Stream`) → the SAME per-agent buffer
//! the reducer already consumes ([`super::push`]). It NEVER writes `~/.codex` and never
//! injects anything — codex produces the file itself, so this is the cleanest plane yet.
//!
//! Parallel to the claude hook plane in `mod.rs`: claude = unix-socket hook ingest
//! (`Authority::Hook`), codex = rollout tail (`Authority::Stream`). The reducer is
//! unchanged — both planes just fill the buffer.
//!
//! Cross-platform (`std::fs` tail; no unix socket), so nothing here is cfg-gated.

use super::evidence::{Evidence, EvidenceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Poll cadence for the tail loop. Codex live-flushes within a turn, so ~1 s gives
/// near-real-time state without busy-spinning the disk.
const TAIL_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// How many LOCAL day-dirs back `discover_rollouts` scans (today + 2 prior). Covers a
/// session that crosses one or two midnights (its file stays in its START-day dir). #2437 r4.
const DISCOVER_DAYS: u32 = 3;

/// Only tail rollout files modified within this window — skips dormant old sessions while
/// still catching a long-running one that crossed midnight (whose file keeps being appended).
const DISCOVER_RECENT: std::time::Duration = std::time::Duration::from_secs(26 * 3600);

/// One codex rollout JSONL record — only the fields we consume (codex emits more).
#[derive(Debug, Deserialize)]
struct RolloutRecord {
    /// ISO-8601 Z record-write time. We prefer it over wall-clock so replayed/lagged
    /// reads still stamp Evidence at the true event time.
    timestamp: Option<String>,
    #[serde(rename = "type")]
    rtype: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// Map one codex rollout JSONL line → [`Evidence`] (`authority=Stream`). `now_ms` is the
/// fallback stamp when the record carries no parseable timestamp. `None` for records that
/// are not an agent-state transition (`session_meta` / `turn_context` / `user_message` /
/// `developer`+`user` messages / a `token_count` with no rate-limit). PURE — unit-tested
/// against real record shapes, no I/O.
pub(crate) fn record_to_evidence(line: &str, now_ms: u64) -> Option<Evidence> {
    let rec: RolloutRecord = serde_json::from_str(line.trim()).ok()?;
    let at_ms = rec
        .timestamp
        .as_deref()
        .and_then(parse_iso_ms)
        .unwrap_or(now_ms);
    let p = &rec.payload;
    let ptype = p.get("type").and_then(|v| v.as_str());
    let kind = match (rec.rtype.as_str(), ptype) {
        // A turn began / ended.
        ("event_msg", Some("task_started")) => EvidenceKind::TurnStarted,
        ("event_msg", Some("task_complete")) => EvidenceKind::TurnEnded { stop_reason: None },
        // Assistant output is streaming (event-level notification).
        ("event_msg", Some("agent_message")) => EvidenceKind::Responding,
        // A tool (codex `exec_command` / MCP call) started / ended.
        ("response_item", Some("function_call")) => EvidenceKind::ToolStarted {
            name: p.get("name").and_then(|v| v.as_str()).map(str::to_string),
        },
        ("response_item", Some("function_call_output")) => EvidenceKind::ToolEnded,
        // A structured assistant message = responding; user/developer messages are the
        // PROMPT side, not agent state (they fall through to the `_` arm below).
        ("response_item", Some("message"))
            if p.get("role").and_then(|v| v.as_str()) == Some("assistant") =>
        {
            EvidenceKind::Responding
        }
        // Token accounting — also the carrier of codex's `rate_limits` (the bonus claude
        // hooks lack). Surface RateLimited only when a window is actually exhausted;
        // otherwise it's just usage accounting.
        ("event_msg", Some("token_count")) => {
            if let Some(retry_at_ms) = rate_limit_retry_at(p, at_ms) {
                EvidenceKind::RateLimited {
                    retry_at_ms: Some(retry_at_ms),
                }
            } else {
                let (input, output) = token_usage(p);
                EvidenceKind::TokenUsage { input, output }
            }
        }
        _ => return None,
    };
    Some(Evidence::stream(kind, at_ms))
}

/// Extract `(input, output)` token totals from a `token_count` payload's `info`.
fn token_usage(payload: &serde_json::Value) -> (u64, u64) {
    let last = payload.get("info").and_then(|i| i.get("last_token_usage"));
    let g = |k: &str| {
        last.and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    (g("input_tokens"), g("output_tokens"))
}

/// If a `token_count` payload's `rate_limits` shows a window at/over capacity, return the
/// absolute epoch-ms instant it resets (best-effort). `None` = not currently throttled.
/// Codex reports `rate_limits.{primary,secondary}.{used_percent,resets_in_seconds}`.
fn rate_limit_retry_at(payload: &serde_json::Value, at_ms: u64) -> Option<u64> {
    let rl = payload.get("info").and_then(|i| i.get("rate_limits"))?;
    for win in ["primary", "secondary"] {
        let w = match rl.get(win) {
            Some(w) => w,
            None => continue,
        };
        let used = w
            .get("used_percent")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if used >= 100.0 {
            let resets = w
                .get("resets_in_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            return Some(at_ms + resets * 1000);
        }
    }
    None
}

/// Parse an ISO-8601 / RFC-3339 instant (e.g. `2026-06-24T02:59:08.844Z`) → epoch ms.
fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Extract the session cwd from a `session_meta` record (rollout line 1), verbatim. `None`
/// if the line isn't a `session_meta` or carries no cwd. Any macOS `/private` reconciliation
/// happens in [`agent_for_cwd`] at comparison time, so the raw cwd is returned here.
pub(crate) fn session_cwd(line: &str) -> Option<String> {
    let rec: RolloutRecord = serde_json::from_str(line.trim()).ok()?;
    if rec.rtype != "session_meta" {
        return None;
    }
    Some(rec.payload.get("cwd")?.as_str()?.to_string())
}

/// Map a session cwd → the agend codex agent that owns it. `candidates` is a
/// (name, expected-cwd) list: for a MANAGED agent, `expected-cwd` is the fixed
/// `<home>/workspace/<name>` convention; for an EPHEMERAL codex worker (#2524 P3a
/// PR-1 — they bypass the registry by design, #1967, and have no fixed workspace
/// convention), it's the worker's own recorded spawn cwd. Compared by path
/// COMPONENTS (`Path` eq handles `\` vs `/`, so it works on Windows — #2437
/// round-1). codex canonicalizes `/tmp`→`/private/tmp` on macOS; [`strip_private`]
/// (unix-only) reconciles that before the comparison.
///
/// #2524 P3a PR-1: FAIL-CLOSED on an ambiguous cwd (≥2 candidates share the same
/// expected cwd — e.g. two ephemeral workers spawned in the same directory, or an
/// ephemeral worker's cwd colliding with a managed agent's workspace path). Do NOT
/// guess: attribute to neither rather than silently mis-crediting Evidence.
fn agent_for_cwd(cwd: &str, candidates: &[(String, PathBuf)]) -> Option<String> {
    let cwd_path = Path::new(strip_private(cwd));
    let mut matches = candidates
        .iter()
        .filter(|(_, expected)| cwd_path == expected.as_path());
    let first = matches.next()?;
    if matches.next().is_some() {
        tracing::warn!(
            tag = "#shadow-observer",
            cwd = %cwd_path.display(),
            "codex rollout: cwd matches MULTIPLE candidate agents — ambiguous, not attributing"
        );
        return None;
    }
    Some(first.0.clone())
}

/// Build the (name, expected-cwd) candidate list `agent_for_cwd` matches against:
/// managed codex agents (from the registry, `<home>/workspace/<name>` convention)
/// PLUS live ephemeral codex workers (#2524 P3a PR-1, their own recorded spawn
/// cwd — read-only `LIVE_CHILDREN` snapshot, doesn't register them anywhere).
fn codex_candidates(registry: &crate::agent::AgentRegistry, home: &Path) -> Vec<(String, PathBuf)> {
    let ws = home.join("workspace");
    let mut out: Vec<(String, PathBuf)> = live_codex_agents(registry)
        .into_iter()
        .map(|name| {
            let expected = ws.join(&name);
            (name, expected)
        })
        .collect();
    out.extend(
        crate::ephemeral_tracking::live_children_snapshot()
            .into_iter()
            .filter(|w| matches!(w.backend, Some(crate::backend::Backend::Codex)))
            .filter_map(|w| w.cwd.map(|cwd| (w.worker_id, cwd))),
    );
    out
}

/// Strip codex's macOS `/private` canonicalization prefix (`/private/tmp/...` → `/tmp/...`)
/// so a `/tmp`-rooted daemon home matches. cfg-guarded to unix — Windows has no such prefix
/// and must not have `/private` semantics applied to its paths (#2437 review). No-op
/// otherwise.
#[cfg(unix)]
fn strip_private(p: &str) -> &str {
    match p.strip_prefix("/private") {
        Some(rest) if rest.starts_with('/') => rest,
        _ => p,
    }
}
#[cfg(not(unix))]
fn strip_private(p: &str) -> &str {
    p
}

/// Today's codex rollout directory root (`<CODEX_HOME|~/.codex>/sessions`).
fn codex_sessions_root() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(h).join("sessions"));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".codex").join("sessions"))
}

/// Per-file tail cursor: byte offset consumed + the agent the rollout is attributed to
/// (resolved once from the session_meta header; `None` until resolved / if not ours).
struct Cursor {
    offset: u64,
    agent: Option<String>,
}

/// Spawn the codex rollout tailer — a fire-and-forget daemon thread (mirrors
/// `api_activity_probe::spawn`). No-op unless [`super::enabled`]. Wired into BOTH
/// `run_core` AND `run_app` (the #2434 lesson: the live fleet daemon is app mode).
pub fn spawn(registry: crate::agent::AgentRegistry, home: PathBuf) {
    if !super::enabled() {
        return;
    }
    // fire-and-forget: a detached read-only tail of codex's own session files. It owns no
    // daemon state, holds no lock across I/O, and exits when the process does. Losing it on
    // shutdown is harmless (next boot re-discovers from the live rollout tail). (§10.5)
    let _ = std::thread::Builder::new()
        .name("shadow-codex-rollout".into())
        .spawn(move || {
            let Some(root) = codex_sessions_root() else {
                tracing::info!(
                    tag = "#shadow-observer",
                    "codex rollout tailer: no HOME/CODEX_HOME — disabled"
                );
                return;
            };
            tracing::info!(tag = "#shadow-observer", root = %root.display(),
                "codex rollout tailer listening (stream plane)");
            let mut cursors: HashMap<PathBuf, Cursor> = HashMap::new();
            loop {
                tail_once(&root, &registry, &home, &mut cursors);
                std::thread::sleep(TAIL_TICK);
            }
        });
}

/// One tail cycle: (re)discover today's rollout files, attribute each to a codex agent via
/// its session_meta cwd, and drain newly-appended records → Evidence → the per-agent
/// buffer. Bounded work; reads only, never under a held lock.
fn tail_once(
    root: &Path,
    registry: &crate::agent::AgentRegistry,
    home: &Path,
    cursors: &mut HashMap<PathBuf, Cursor>,
) {
    let candidates = codex_candidates(registry, home);
    if candidates.is_empty() {
        return;
    }
    for file in discover_rollouts(root) {
        let cur = cursors.entry(file.clone()).or_insert(Cursor {
            offset: 0,
            agent: None,
        });
        drain_file(&file, cur, &candidates);
    }
}

/// Snapshot the live CODEX agent names (brief registry lock, released before any I/O).
fn live_codex_agents(registry: &crate::agent::AgentRegistry) -> Vec<String> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|h| h.backend_command.contains("codex"))
        .map(|h| h.name.to_string())
        .collect()
}

/// Recently-modified rollout files under the last [`DISCOVER_DAYS`] day-dirs
/// (`<root>/<Y>/<M>/<D>/`). #2437 r4: a codex session is named by — and filed under — its
/// START day (LOCAL date, matching the `rollout-<local-ts>` filename, NOT the UTC `Z`
/// session_meta stamp). A long-running fleet agent's session crosses midnight and keeps
/// appending to YESTERDAY's file, which a today-only scan would miss — leaving that agent
/// OBSERVE-BLIND after every midnight until it restarts. So scan the last few LOCAL
/// day-dirs and keep only files touched within [`DISCOVER_RECENT`] (bounds the set + skips
/// dormant prior-day sessions).
fn discover_rollouts(root: &Path) -> Vec<PathBuf> {
    let now = chrono::Local::now();
    let recent_cutoff = std::time::SystemTime::now()
        .checked_sub(DISCOVER_RECENT)
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut out = Vec::new();
    for back in 0..DISCOVER_DAYS {
        let day = now - chrono::Duration::days(i64::from(back));
        let dir = root
            .join(day.format("%Y").to_string())
            .join(day.format("%m").to_string())
            .join(day.format("%d").to_string());
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_rollout = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
            if !is_rollout {
                continue;
            }
            // Recency: skip dormant old sessions; keep a long-running one still being
            // written (its file mtime stays fresh even though its day-dir is older).
            let fresh = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| m >= recent_cutoff)
                .unwrap_or(true);
            if fresh {
                out.push(p);
            }
        }
    }
    out
}

/// Read newly-appended bytes of one rollout file from the cursor offset, attribute it (via
/// the first `session_meta` line) to an agend codex agent, and push each transition as
/// Evidence. A file not owned by any live codex agent is consumed-and-ignored (cursor
/// advances so we don't re-scan it).
fn drain_file(file: &Path, cur: &mut Cursor, candidates: &[(String, PathBuf)]) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let Ok(f) = std::fs::File::open(file) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // #2437 r6: a same-path REBUILD (codex reusing a path, or any truncation) shrinks the
    // file below our cursor. Detect it and RESET — re-resolve the agent from the new header
    // and re-drain from 0. Otherwise a shorter rewrite is never read (the observer goes
    // DEAF for that agent) and a longer one would seek into the middle of a record.
    if len < cur.offset {
        cur.offset = 0;
        cur.agent = None;
    }
    if len <= cur.offset {
        return; // nothing new
    }
    let mut reader = BufReader::new(f);
    if reader.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut consumed = cur.offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        // Only act on a COMPLETE line (ends with '\n'); a partial trailing write is left
        // for the next tick (don't advance the cursor past it).
        if !line.ends_with('\n') {
            break;
        }
        consumed += n as u64;
        // Resolve the owning agent from the session_meta header (first line).
        if cur.agent.is_none() {
            if let Some(cwd) = session_cwd(&line) {
                cur.agent = agent_for_cwd(&cwd, candidates);
            }
            // Either way the header line itself is not a transition.
            continue;
        }
        if let Some(agent) = cur.agent.as_deref() {
            if let Some(ev) = record_to_evidence(&line, now_ms) {
                super::push(agent, ev);
            }
        }
    }
    cur.offset = consumed;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::evidence::{Authority, EvidenceKind};
    use super::*;
    use serial_test::serial;

    fn kind_of(line: &str) -> Option<EvidenceKind> {
        record_to_evidence(line, 1_000).map(|e| e.kind)
    }

    /// Build the (name, expected-cwd) candidate list for a set of MANAGED agent
    /// names under `home` — the `<home>/workspace/<name>` convention `codex_candidates`
    /// derives for registry members. Test-only shorthand so existing fixtures don't
    /// need to spell out the join at every call site.
    fn ws_candidates(home: &Path, names: &[&str]) -> Vec<(String, PathBuf)> {
        let ws = home.join("workspace");
        names.iter().map(|n| (n.to_string(), ws.join(n))).collect()
    }

    #[test]
    fn maps_turn_lifecycle_and_tools() {
        assert_eq!(
            kind_of(
                r#"{"timestamp":"2026-06-24T02:59:00.000Z","type":"event_msg","payload":{"type":"task_started"}}"#
            ),
            Some(EvidenceKind::TurnStarted)
        );
        assert_eq!(
            kind_of(
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","duration_ms":1200}}"#
            ),
            Some(EvidenceKind::TurnEnded { stop_reason: None })
        );
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1"}}"#
            ),
            Some(EvidenceKind::ToolStarted {
                name: Some("exec_command".into())
            })
        );
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1"}}"#
            ),
            Some(EvidenceKind::ToolEnded)
        );
    }

    #[test]
    fn assistant_message_is_responding_user_is_not() {
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#
            ),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hi"}}"#),
            Some(EvidenceKind::Responding)
        );
        // User / developer prompt side is NOT agent state.
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#
            ),
            None
        );
        // session_meta / turn_context / user_message → not a transition.
        assert_eq!(kind_of(r#"{"type":"turn_context","payload":{}}"#), None);
        assert_eq!(
            kind_of(r#"{"type":"event_msg","payload":{"type":"user_message","message":"go"}}"#),
            None
        );
    }

    #[test]
    fn token_count_yields_usage_then_ratelimit_when_exhausted() {
        // Normal token_count → TokenUsage with the last-turn totals.
        let u = kind_of(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":15797,"output_tokens":36}}}}"#,
        );
        assert_eq!(
            u,
            Some(EvidenceKind::TokenUsage {
                input: 15797,
                output: 36
            })
        );
        // A window at 100% used → RateLimited (the codex-only bonus).
        let r = kind_of(
            r#"{"timestamp":"2026-06-24T03:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1,"output_tokens":1},"rate_limits":{"primary":{"used_percent":100.0,"resets_in_seconds":60}}}}}"#,
        );
        match r {
            Some(EvidenceKind::RateLimited {
                retry_at_ms: Some(ms),
            }) => {
                // 03:00:00.000Z + 60s
                assert_eq!(
                    ms,
                    parse_iso_ms("2026-06-24T03:00:00.000Z").unwrap() + 60_000
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn evidence_is_stream_authority_with_record_timestamp() {
        let ev = record_to_evidence(
            r#"{"timestamp":"2026-06-24T02:59:08.844Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            9_999,
        )
        .unwrap();
        assert_eq!(ev.authority, Authority::Stream);
        // Stamped at the RECORD time, not the fallback now_ms.
        assert_eq!(ev.at_ms, parse_iso_ms("2026-06-24T02:59:08.844Z").unwrap());
        assert_ne!(ev.at_ms, 9_999);
    }

    #[test]
    fn malformed_or_partial_line_is_none() {
        assert_eq!(record_to_evidence("not json", 1), None);
        assert_eq!(record_to_evidence("", 1), None);
        assert_eq!(record_to_evidence("{}", 1), None);
    }

    #[test]
    fn session_cwd_extracts_verbatim() {
        let line = r#"{"type":"session_meta","payload":{"id":"u1","cwd":"/private/tmp/svcx/workspace/cx","originator":"codex-tui"}}"#;
        // Returned verbatim — no normalization (attribution is by trailing components).
        assert_eq!(
            session_cwd(line).as_deref(),
            Some("/private/tmp/svcx/workspace/cx")
        );
        // A non-session_meta line yields no cwd.
        assert_eq!(session_cwd(r#"{"type":"event_msg","payload":{}}"#), None);
    }

    /// Attribution is SCOPED to this daemon's `home` AND separator-agnostic (#2437): a cwd
    /// attributes only if it equals `<home>/workspace/<live-name>`. Fixtures are built with
    /// the platform `Path` API (the cwd the SAME way production derives it), so the assert
    /// holds on macOS AND Windows (`\` separators).
    #[test]
    fn agent_for_cwd_matches_only_fleet_codex_workspace() {
        let home = std::env::temp_dir().join("svcx_test_home");
        let candidates = ws_candidates(&home, &["cx", "cx2"]);
        // The agent's own workspace (built like production) → match, on any platform.
        let ws_cx = home.join("workspace").join("cx");
        assert_eq!(
            agent_for_cwd(&ws_cx.to_string_lossy(), &candidates).as_deref(),
            Some("cx")
        );
        // Unknown agent name UNDER the fleet workspace → None.
        let ghost = home.join("workspace").join("ghost");
        assert_eq!(agent_for_cwd(&ghost.to_string_lossy(), &candidates), None);
        // A cwd not ending in `workspace/<name>` → None.
        assert_eq!(agent_for_cwd("/some/other/dir", &candidates), None);
    }

    /// #2524 P3a PR-1: an ephemeral codex worker's OWN recorded cwd (not the
    /// `<home>/workspace/<name>` convention) also attributes correctly — it's just
    /// another candidate in the merged list.
    #[test]
    fn agent_for_cwd_matches_ephemeral_worker_explicit_cwd() {
        let candidates = vec![(
            "eph-1".to_string(),
            PathBuf::from("/tmp/some/ephemeral/dir"),
        )];
        assert_eq!(
            agent_for_cwd("/tmp/some/ephemeral/dir", &candidates).as_deref(),
            Some("eph-1")
        );
        assert_eq!(agent_for_cwd("/tmp/some/other/dir", &candidates), None);
    }

    /// #2524 P3a PR-1: two candidates (e.g. two ephemeral workers, or an ephemeral
    /// worker colliding with a managed agent's workspace) sharing the SAME expected
    /// cwd is FAIL-CLOSED — attribute to neither rather than guess.
    #[test]
    fn agent_for_cwd_ambiguous_when_two_candidates_share_a_cwd() {
        let shared = PathBuf::from("/tmp/shared/workdir");
        let candidates = vec![
            ("eph-1".to_string(), shared.clone()),
            ("eph-2".to_string(), shared.clone()),
        ];
        assert_eq!(
            agent_for_cwd(&shared.to_string_lossy(), &candidates),
            None,
            "an ambiguous cwd must not attribute to either candidate"
        );
    }

    /// #2437 r6 (regression, verbatim): a SAME-NAMED `workspace/<name>` OUTSIDE this daemon's
    /// home must NOT attribute — the tail-component-only match (round-3) wrongly did.
    #[test]
    fn agent_for_cwd_does_not_attribute_same_named_stray_workspace() {
        let candidates = ws_candidates(Path::new("/tmp/svcx"), &["cx"]);
        assert_eq!(
            agent_for_cwd("/tmp/operator/workspace/cx", &candidates),
            None
        );
    }

    /// codex canonicalizes `/tmp`→`/private/tmp` on macOS; `strip_private` (unix-only)
    /// reconciles it so a `/tmp`-rooted home still matches. (Windows has no such prefix.)
    #[cfg(unix)]
    #[test]
    fn agent_for_cwd_reconciles_macos_private_prefix() {
        let candidates = ws_candidates(Path::new("/tmp/svcx"), &["cx"]);
        assert_eq!(
            agent_for_cwd("/private/tmp/svcx/workspace/cx", &candidates).as_deref(),
            Some("cx")
        );
    }

    /// Integration: a real on-disk rollout file (session_meta header + a turn) tailed by
    /// `drain_file` resolves the owning agent from the cwd and pushes each transition as
    /// Stream Evidence into the shared buffer. Also pins the partial-trailing-line safety
    /// (a line without a newline is NOT consumed until completed).
    #[test]
    #[serial(shadow_observer)]
    fn drain_file_tails_real_rollout_into_buffer() {
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("agend_rollout_{}", std::process::id()));
        let ws = home.join("workspace").join("cxt");
        std::fs::create_dir_all(&ws).unwrap();
        let cwd = ws.to_string_lossy().to_string();
        let roll = home.join("rollout-test.jsonl");
        let mut f = std::fs::File::create(&roll).unwrap();
        // serde_json so the cwd is JSON-escaped — a real path has `\` on Windows, which is
        // an invalid raw JSON escape (#2437 windows-latest). Production codex writes valid
        // escaped JSON; the test must too.
        writeln!(
            f,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"cwd": &cwd, "originator":"codex-tui"}})
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"event_msg","payload":{{"type":"task_started"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec_command"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"t1"}}}}"#
        )
        .unwrap();
        f.flush().unwrap();

        let mut cur = Cursor {
            offset: 0,
            agent: None,
        };
        let candidates = ws_candidates(&home, &["cxt"]);
        drain_file(&roll, &mut cur, &candidates);

        assert_eq!(
            cur.agent.as_deref(),
            Some("cxt"),
            "cwd resolved to the agent"
        );
        let evs = super::super::peek("cxt");
        let kinds: Vec<&EvidenceKind> = evs.iter().map(|e| &e.kind).collect();
        assert!(kinds.contains(&&EvidenceKind::TurnStarted), "{kinds:?}");
        assert!(
            kinds.iter().any(|k| matches!(
                k,
                EvidenceKind::ToolStarted { name } if name.as_deref() == Some("exec_command")
            )),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&&EvidenceKind::TurnEnded { stop_reason: None }),
            "{kinds:?}"
        );
        assert!(
            evs.iter().all(|e| e.authority == Authority::Stream),
            "all Stream authority"
        );
        let off_after = cur.offset;

        // Partial-trailing-line safety: append bytes WITHOUT a newline → not consumed.
        let mut f2 = std::fs::OpenOptions::new()
            .append(true)
            .open(&roll)
            .unwrap();
        write!(
            f2,
            r#"{{"type":"event_msg","payload":{{"type":"task_started"#
        )
        .unwrap();
        f2.flush().unwrap();
        drain_file(&roll, &mut cur, &candidates);
        assert_eq!(
            cur.offset, off_after,
            "partial line must not advance the cursor"
        );

        super::super::drain("cxt");
        super::super::forget_agent("cxt");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #2437 r6 (regression): a same-path REBUILD (codex truncates/reuses a rollout path)
    /// must REWIND the cursor so the observer keeps seeing the new session — not go DEAF
    /// because the rebuilt file is shorter than the prior cursor offset.
    #[test]
    #[serial(shadow_observer)]
    fn truncated_rollout_same_path_rewinds_cursor() {
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("agend_roll_trunc_{}", std::process::id()));
        let ws = home.join("workspace").join("cxr");
        std::fs::create_dir_all(&ws).unwrap();
        let cwd = ws.to_string_lossy().to_string();
        let roll = home.join("rollout-trunc.jsonl");

        // First session: header + a turn + padding so the cursor advances well past a short
        // rebuild. Drain it.
        let mut f = std::fs::File::create(&roll).unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"cwd": &cwd}})
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"event_msg","payload":{{"type":"task_started"}}}}"#
        )
        .unwrap();
        for _ in 0..6 {
            writeln!(
                f,
                r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1,"output_tokens":1}}}}}}}}"#
            )
            .unwrap();
        }
        f.flush().unwrap();
        drop(f);
        let mut cur = Cursor {
            offset: 0,
            agent: None,
        };
        let candidates = ws_candidates(&home, &["cxr"]);
        drain_file(&roll, &mut cur, &candidates);
        assert!(
            cur.offset > 0 && cur.agent.is_some(),
            "first drain advanced"
        );
        let big_offset = cur.offset;
        super::super::drain("cxr"); // clear the buffer

        // REBUILD: truncate (File::create) + a SHORTER new session. File now < big_offset.
        let mut f2 = std::fs::File::create(&roll).unwrap();
        writeln!(
            f2,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"cwd": &cwd}})
        )
        .unwrap();
        writeln!(
            f2,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec_command"}}}}"#
        )
        .unwrap();
        f2.flush().unwrap();
        drop(f2);
        assert!(
            std::fs::metadata(&roll).unwrap().len() < big_offset,
            "rebuilt session is shorter than the prior cursor"
        );

        drain_file(&roll, &mut cur, &candidates);
        // Recovered: agent re-resolved from the new header + the new ToolStarted seen (the
        // pre-fix code would have left the cursor past EOF → observer DEAF → empty).
        let evs = super::super::peek("cxr");
        assert!(
            evs.iter()
                .any(|e| matches!(&e.kind, EvidenceKind::ToolStarted { .. })),
            "after truncation the observer must re-drain the rebuilt session, got {evs:?}"
        );
        super::super::drain("cxr");
        super::super::forget_agent("cxr");
        let _ = std::fs::remove_dir_all(&home);
    }
}
