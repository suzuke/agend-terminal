//! #2413 Phase D — kiro session-tail observer source.
//!
//! `kiro-cli` (the Amazon Q CLI) writes its session transcript to
//! `~/.kiro/sessions/cli/<uuid>.jsonl` (append) alongside a `<uuid>.json` full-state
//! SNAPSHOT (which carries the session `cwd`). This module is a strictly **READ-ONLY
//! tail** of the `.jsonl`: each appended line → [`Evidence`] (`authority=Stream`) → the
//! SAME per-agent buffer the reducer consumes ([`super::push`]). It NEVER writes `~/.kiro`
//! and never injects anything — kiro produces the files itself.
//!
//! Mirrors the codex `rollout` plane (`rollout.rs`); the reducer + Evidence schema are
//! unchanged. Two kiro-specific deltas (confirm-first spike, 2026-06-24,
//! `KIRO-OBSERVER-SPIKE-2026-06-24.md`):
//! - **flat uuid dir** (`sessions/cli/<uuid>.jsonl`), not codex's `Y/M/D/` partition.
//! - **attribution via the `<uuid>.json` sidecar `cwd`** (codex carried cwd in the jsonl
//!   `session_meta` header; kiro's `.jsonl` has no header line).
//!
//! ⚠ CAVEAT (documented residual): kiro flushes the `.jsonl` at **tool-round boundaries +
//! turn-end**, NOT at prompt submit — so a **pure-thinking (no-tool) turn is not caught
//! mid-turn** (its lines land only when the turn completes), and there is no submit-time
//! turn-start edge (codex has `task_started`; kiro does not). Tool-using turns DO flush
//! mid-turn (the `toolUse` line lands while the tool runs). The reducer's liveness backstop
//! and the Screen plane cover the tool-less-thinking gap; agent work is overwhelmingly
//! tool-driven, so practical coverage is good.
//!
//! Cross-platform (`std::fs` tail; no unix socket), so nothing here is cfg-gated except the
//! macOS `/private` reconciliation (unix-only), mirroring `rollout.rs`.

use super::evidence::{Evidence, EvidenceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Poll cadence for the tail loop. kiro flushes within a tool-using turn, so ~1 s gives
/// near-real-time state without busy-spinning the disk. (Same as codex `rollout`.)
const TAIL_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// Only tail session files modified within this window — skips dormant old sessions while
/// still catching a live one being appended. kiro's dir is flat (all sessions in one dir),
/// so this recency filter is what bounds the scanned set.
const DISCOVER_RECENT: std::time::Duration = std::time::Duration::from_secs(26 * 3600);

/// One kiro `.jsonl` line — only the fields we consume. Shape: `{version, kind, data}`.
#[derive(Debug, Deserialize)]
struct KiroRecord {
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// Map one kiro `.jsonl` line → [`Evidence`] (`authority=Stream`). `now_ms` is the fallback
/// stamp when the line carries no `data.meta.timestamp`. `None` for a line that is not an
/// agent-state transition. PURE — unit-tested against real line shapes, no I/O.
///
/// `kind` mapping:
/// - `Prompt` → `TurnStarted`.
/// - `AssistantMessage` → `ToolStarted{name}` if its `content[]` has a `toolUse` block
///   (the model called a tool), else `Responding` (it produced text).
/// - `ToolResults` → `ToolEnded`.
pub(crate) fn record_to_evidence(line: &str, now_ms: u64) -> Option<Evidence> {
    let rec: KiroRecord = serde_json::from_str(line.trim()).ok()?;
    // kiro stamps `data.meta.timestamp` as epoch SECONDS (Prompt lines carry it; others may
    // not). Prefer it so a lagged read still stamps at the true event time.
    let at_ms = rec
        .data
        .get("meta")
        .and_then(|m| m.get("timestamp"))
        .and_then(|v| v.as_u64())
        .map(|secs| secs.saturating_mul(1000))
        .unwrap_or(now_ms);
    let kind = match rec.kind.as_str() {
        "Prompt" => EvidenceKind::TurnStarted,
        "AssistantMessage" => assistant_kind(&rec.data),
        "ToolResults" => EvidenceKind::ToolEnded,
        _ => return None,
    };
    Some(Evidence::stream(kind, at_ms))
}

/// An `AssistantMessage` is a tool call iff any `content[]` block is `kind=="toolUse"`
/// (→ `ToolStarted{name}`); otherwise it is assistant output (→ `Responding`).
fn assistant_kind(data: &serde_json::Value) -> EvidenceKind {
    let content = data.get("content").and_then(|c| c.as_array());
    if let Some(arr) = content {
        if let Some(tu) = arr
            .iter()
            .find(|b| b.get("kind").and_then(|k| k.as_str()) == Some("toolUse"))
        {
            return EvidenceKind::ToolStarted {
                name: tu
                    .get("data")
                    .and_then(|d| d.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_string),
            };
        }
    }
    EvidenceKind::Responding
}

/// Read the `cwd` from a `.jsonl`'s sibling `<uuid>.json` snapshot. `None` if the sidecar
/// is missing (it may briefly lag the `.jsonl`) or carries no cwd — the caller retries.
pub(crate) fn sidecar_cwd(jsonl: &Path) -> Option<String> {
    let json = jsonl.with_extension("json");
    let txt = std::fs::read_to_string(json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    Some(v.get("cwd")?.as_str()?.to_string())
}

/// Map a session cwd → the agend kiro agent that owns it. SCOPED + separator-agnostic:
/// the cwd must EQUAL `<home>/workspace/<name>` for a LIVE kiro agent, compared by path
/// COMPONENTS (`Path` eq handles `\` vs `/` for Windows) AND rooted at THIS daemon's `home`
/// (a stray `*/workspace/<name>` outside the fleet is NOT attributed). Identical to codex
/// `rollout::agent_for_cwd`. `/tmp`→`/private/tmp` macOS canonicalization reconciled by
/// [`strip_private`].
fn agent_for_cwd(cwd: &str, home: &Path, kiro_agents: &[String]) -> Option<String> {
    let cwd_path = Path::new(strip_private(cwd));
    let ws = home.join("workspace");
    kiro_agents
        .iter()
        .find(|name| cwd_path == ws.join(name))
        .cloned()
}

/// Strip macOS `/private` canonicalization (`/private/tmp/...` → `/tmp/...`) so a
/// `/tmp`-rooted daemon home matches. Unix-only (Windows has no such prefix). No-op else.
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

/// kiro's flat session dir (`<HOME>/.kiro/sessions/cli`).
fn kiro_sessions_root() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".kiro").join("sessions").join("cli"))
}

/// Per-file tail cursor: byte offset consumed + the agent the session is attributed to
/// (resolved once from the `.json` sidecar cwd; `None` until resolved / not ours).
struct Cursor {
    offset: u64,
    agent: Option<String>,
}

/// Spawn the kiro session tailer — a fire-and-forget daemon thread (mirrors
/// `rollout::spawn` / `api_activity_probe::spawn`). No-op unless [`super::enabled`]. Wired
/// into BOTH `run_core` AND `run_app` (the #2434 lesson: the live fleet daemon is app mode).
pub fn spawn(registry: crate::agent::AgentRegistry, home: PathBuf) {
    if !super::enabled() {
        return;
    }
    // fire-and-forget: a detached read-only tail of kiro's own session files. It owns no
    // daemon state, holds no lock across I/O, and exits when the process does. Losing it on
    // shutdown is harmless (next boot re-discovers from the live session tail). (§10.5)
    let _ = std::thread::Builder::new()
        .name("shadow-kiro-tail".into())
        .spawn(move || {
            let Some(root) = kiro_sessions_root() else {
                tracing::info!(
                    tag = "#shadow-observer",
                    "kiro session tailer: no HOME — disabled"
                );
                return;
            };
            tracing::info!(tag = "#shadow-observer", root = %root.display(),
                "kiro session tailer listening (stream plane)");
            let mut cursors: HashMap<PathBuf, Cursor> = HashMap::new();
            loop {
                tail_once(&root, &registry, &home, &mut cursors);
                std::thread::sleep(TAIL_TICK);
            }
        });
}

/// One tail cycle: (re)discover recent session files, attribute each via its `.json`
/// sidecar cwd, and drain newly-appended lines → Evidence → the per-agent buffer.
fn tail_once(
    root: &Path,
    registry: &crate::agent::AgentRegistry,
    home: &Path,
    cursors: &mut HashMap<PathBuf, Cursor>,
) {
    let kiro_agents = live_kiro_agents(registry);
    if kiro_agents.is_empty() {
        return;
    }
    for file in discover_sessions(root) {
        let cur = cursors.entry(file.clone()).or_insert(Cursor {
            offset: 0,
            agent: None,
        });
        drain_file(&file, cur, home, &kiro_agents);
    }
}

/// Snapshot the live KIRO agent names (brief registry lock, released before any I/O).
fn live_kiro_agents(registry: &crate::agent::AgentRegistry) -> Vec<String> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|h| h.backend_command.contains("kiro"))
        .map(|h| h.name.to_string())
        .collect()
}

/// Recently-modified `.jsonl` session files under the flat `cli/` dir. kiro names each
/// session by a uuid in ONE dir (no date partition), so the recency filter bounds the set
/// (skips dormant old sessions; keeps a live one being appended).
fn discover_sessions(root: &Path) -> Vec<PathBuf> {
    let recent_cutoff = std::time::SystemTime::now()
        .checked_sub(DISCOVER_RECENT)
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        let is_jsonl = p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x == "jsonl");
        if !is_jsonl {
            continue;
        }
        let fresh = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| m >= recent_cutoff)
            .unwrap_or(true);
        if fresh {
            out.push(p);
        }
    }
    out
}

/// Read newly-appended lines of one session `.jsonl` from the cursor, attribute it (via the
/// `.json` sidecar cwd) to a kiro agent, and push each transition as Evidence. Attribution
/// is retried each tick until the sidecar resolves — and the cursor is NOT advanced until an
/// owning agent is resolved, so no lines are lost to a sidecar/registration race (a session
/// whose cwd is not a live fleet kiro agent simply re-checks cheaply and never advances).
fn drain_file(file: &Path, cur: &mut Cursor, home: &Path, kiro_agents: &[String]) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    // Resolve the owning agent from the sidecar before consuming anything.
    if cur.agent.is_none() {
        match sidecar_cwd(file) {
            Some(cwd) => cur.agent = agent_for_cwd(&cwd, home, kiro_agents),
            None => return, // sidecar not ready yet — retry next tick, don't advance
        }
        if cur.agent.is_none() {
            return; // cwd present but not a live fleet kiro agent — skip (re-checked next tick)
        }
    }
    let Ok(f) = std::fs::File::open(file) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // A same-path truncation/rebuild (shrink below cursor) → reset and re-drain from 0.
    if len < cur.offset {
        cur.offset = 0;
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
    let agent = match cur.agent.as_deref() {
        Some(a) => a,
        None => return,
    };
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        // Only act on a COMPLETE line (ends with '\n'); a partial trailing write is left for
        // the next tick (don't advance the cursor past it).
        if !line.ends_with('\n') {
            break;
        }
        consumed += n as u64;
        if let Some(ev) = record_to_evidence(&line, now_ms) {
            super::push(agent, ev);
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

    #[test]
    fn maps_prompt_assistant_tool_lifecycle() {
        assert_eq!(
            kind_of(
                r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"hi"}],"meta":{"timestamp":1780845431}}}"#
            ),
            Some(EvidenceKind::TurnStarted)
        );
        // AssistantMessage with a toolUse block → ToolStarted{name}.
        assert_eq!(
            kind_of(
                r#"{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"text","data":""},{"kind":"toolUse","data":{"toolUseId":"t1","name":"shell","input":{}}}]}}"#
            ),
            Some(EvidenceKind::ToolStarted {
                name: Some("shell".into())
            })
        );
        // AssistantMessage with only text → Responding.
        assert_eq!(
            kind_of(
                r#"{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"the answer is 42"}]}}"#
            ),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(
                r#"{"version":"v1","kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"toolUseId":"t1"}}]}}"#
            ),
            Some(EvidenceKind::ToolEnded)
        );
    }

    #[test]
    fn non_transition_and_malformed_are_none() {
        // unknown kind / malformed / empty.
        assert_eq!(
            kind_of(r#"{"version":"v1","kind":"SomethingElse","data":{}}"#),
            None
        );
        assert_eq!(kind_of("not json"), None);
        assert_eq!(kind_of(""), None);
        assert_eq!(kind_of("{}"), None);
    }

    #[test]
    fn evidence_is_stream_authority_at_meta_timestamp() {
        let ev = record_to_evidence(
            r#"{"kind":"Prompt","data":{"content":[],"meta":{"timestamp":1780845431}}}"#,
            9_999,
        )
        .unwrap();
        assert_eq!(ev.authority, Authority::Stream);
        // epoch SECONDS → ms; stamped at the record time, not the fallback now_ms.
        assert_eq!(ev.at_ms, 1_780_845_431_000);
        assert_ne!(ev.at_ms, 9_999);
        // a line without meta.timestamp falls back to now_ms.
        let ev2 =
            record_to_evidence(r#"{"kind":"ToolResults","data":{"content":[]}}"#, 9_999).unwrap();
        assert_eq!(ev2.at_ms, 9_999);
    }

    #[test]
    fn sidecar_cwd_reads_sibling_json() {
        let dir = std::env::temp_dir().join(format!("kiro_sidecar_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("abc.jsonl");
        std::fs::write(&jsonl, "").unwrap();
        // no sidecar yet → None
        assert_eq!(sidecar_cwd(&jsonl), None);
        // write sibling .json with cwd → resolved
        std::fs::write(
            dir.join("abc.json"),
            serde_json::json!({"session_id":"abc","cwd":"/Users/x/proj"}).to_string(),
        )
        .unwrap();
        assert_eq!(sidecar_cwd(&jsonl).as_deref(), Some("/Users/x/proj"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Attribution is SCOPED to this daemon's `home` AND separator-agnostic (#2437): a cwd
    /// attributes only if it equals `<home>/workspace/<live-name>`. Built with the platform
    /// `Path` API so it holds on macOS AND Windows.
    #[test]
    fn agent_for_cwd_matches_only_fleet_kiro_workspace() {
        let home = std::env::temp_dir().join("svk_test_home");
        let agents = vec!["kr".to_string(), "kr2".to_string()];
        let ws_kr = home.join("workspace").join("kr");
        assert_eq!(
            agent_for_cwd(&ws_kr.to_string_lossy(), &home, &agents).as_deref(),
            Some("kr")
        );
        // Unknown name under the fleet workspace → None.
        let ghost = home.join("workspace").join("ghost");
        assert_eq!(
            agent_for_cwd(&ghost.to_string_lossy(), &home, &agents),
            None
        );
        // A cwd not ending in workspace/<name> → None.
        assert_eq!(agent_for_cwd("/some/other/dir", &home, &agents), None);
    }

    /// A SAME-NAMED `workspace/<name>` OUTSIDE this daemon's home must NOT attribute.
    #[test]
    fn agent_for_cwd_rejects_same_named_stray_workspace() {
        let agents = vec!["kr".to_string()];
        assert_eq!(
            agent_for_cwd("/tmp/operator/workspace/kr", Path::new("/tmp/svk"), &agents),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_for_cwd_reconciles_macos_private_prefix() {
        let agents = vec!["kr".to_string()];
        assert_eq!(
            agent_for_cwd(
                "/private/tmp/svk/workspace/kr",
                Path::new("/tmp/svk"),
                &agents
            )
            .as_deref(),
            Some("kr")
        );
    }

    /// Integration: a real on-disk `.jsonl` + its `.json` sidecar (cwd) tailed by
    /// `drain_file` resolves the owning agent from the SIDECAR and pushes each transition as
    /// Stream Evidence. Also pins partial-trailing-line safety + the no-attribution-no-advance
    /// race guard (a line whose sidecar is missing is NOT consumed).
    #[test]
    #[serial(shadow_observer)]
    fn drain_file_tails_with_sidecar_attribution() {
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("agend_kiro_{}", std::process::id()));
        let ws = home.join("workspace").join("krt");
        std::fs::create_dir_all(&ws).unwrap();
        let cwd = ws.to_string_lossy().to_string();
        let sess = home.join("kiro-sess.jsonl");
        let mut f = std::fs::File::create(&sess).unwrap();
        writeln!(f, r#"{{"version":"v1","kind":"Prompt","data":{{"content":[{{"kind":"text","data":"go"}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"version":"v1","kind":"AssistantMessage","data":{{"content":[{{"kind":"toolUse","data":{{"name":"shell"}}}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"version":"v1","kind":"ToolResults","data":{{"content":[{{"kind":"toolResult","data":{{}}}}]}}}}"#).unwrap();
        f.flush().unwrap();

        let mut cur = Cursor {
            offset: 0,
            agent: None,
        };
        let agents = vec!["krt".to_string()];

        // No sidecar yet → drain must NOT advance and NOT attribute (race guard).
        drain_file(&sess, &mut cur, &home, &agents);
        assert_eq!(
            cur.offset, 0,
            "no advance until the sidecar resolves the agent"
        );
        assert!(super::super::peek("krt").is_empty());

        // Write the sidecar .json with the cwd → now attributable.
        std::fs::write(
            home.join("kiro-sess.json"),
            serde_json::json!({"cwd": &cwd}).to_string(),
        )
        .unwrap();
        drain_file(&sess, &mut cur, &home, &agents);

        assert_eq!(
            cur.agent.as_deref(),
            Some("krt"),
            "resolved via sidecar cwd"
        );
        let evs = super::super::peek("krt");
        let kinds: Vec<&EvidenceKind> = evs.iter().map(|e| &e.kind).collect();
        assert!(kinds.contains(&&EvidenceKind::TurnStarted), "{kinds:?}");
        assert!(
            kinds.iter().any(|k| matches!(k,
                EvidenceKind::ToolStarted { name } if name.as_deref() == Some("shell"))),
            "{kinds:?}"
        );
        assert!(kinds.contains(&&EvidenceKind::ToolEnded), "{kinds:?}");
        assert!(evs.iter().all(|e| e.authority == Authority::Stream));
        let off_after = cur.offset;

        // Partial-trailing-line safety: append bytes WITHOUT a newline → not consumed.
        let mut f2 = std::fs::OpenOptions::new()
            .append(true)
            .open(&sess)
            .unwrap();
        write!(
            f2,
            r#"{{"version":"v1","kind":"Prompt","data":{{"content":["#
        )
        .unwrap();
        f2.flush().unwrap();
        drain_file(&sess, &mut cur, &home, &agents);
        assert_eq!(
            cur.offset, off_after,
            "partial line must not advance the cursor"
        );

        super::super::drain("krt");
        super::super::forget_agent("krt");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// END-TO-END dogfood: a real-shape kiro `.jsonl` (Prompt + a `toolUse` AssistantMessage,
    /// the mid-turn tool-running case the spike proved kiro flushes) → `drain_file` →
    /// `super::observe` (the per-tick reducer driver) → the agent's `ObservedStatus` is
    /// derived with **`authority == Stream`** and a working (non-Idle) state. Proves the full
    /// kiro plane pipeline end-to-end inside the codebase; the REAL kiro binary writes exactly
    /// this line shape (confirm-first spike, `KIRO-OBSERVER-SPIKE-2026-06-24.md`).
    #[test]
    #[serial(shadow_observer)]
    fn dogfood_kiro_tooluse_yields_stream_observed_status() {
        use super::super::reducer::{Liveness, ObservedState, ScreenSignal};
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("agend_kiro_dog_{}", std::process::id()));
        let ws = home.join("workspace").join("kdog");
        std::fs::create_dir_all(&ws).unwrap();
        let cwd = ws.to_string_lossy().to_string();
        let sess = home.join("kiro-dog.jsonl");
        let mut f = std::fs::File::create(&sess).unwrap();
        writeln!(f, r#"{{"version":"v1","kind":"Prompt","data":{{"content":[{{"kind":"text","data":"build it"}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"version":"v1","kind":"AssistantMessage","data":{{"content":[{{"kind":"toolUse","data":{{"name":"shell"}}}}]}}}}"#).unwrap();
        f.flush().unwrap();
        std::fs::write(
            home.join("kiro-dog.json"),
            serde_json::json!({"cwd": &cwd}).to_string(),
        )
        .unwrap();

        let mut cur = Cursor {
            offset: 0,
            agent: None,
        };
        let agents = vec!["kdog".to_string()];
        drain_file(&sess, &mut cur, &home, &agents);
        assert_eq!(cur.agent.as_deref(), Some("kdog"));

        // Reducer driver: drain the kiro Evidence + derive the fused status. api in flight +
        // screen Working so liveness/screen agree it's working (no idle reconcile).
        let live = Liveness {
            api_in_flight: true,
            productive_silent_ms: 0,
            child_alive: true,
        };
        let observed = super::super::observe("kdog", ScreenSignal::Working, &live, 5_000);
        assert_eq!(
            observed.authority,
            Authority::Stream,
            "observed_status must be Stream-sourced from the kiro .jsonl tail, got {observed:?}"
        );
        assert_ne!(
            observed.state,
            ObservedState::Idle,
            "a mid-turn toolUse must read as working, not idle: {observed:?}"
        );

        super::super::forget_agent("kdog");
        let _ = std::fs::remove_dir_all(&home);
    }
}
