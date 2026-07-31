//! Read-only observer for Grok Build's structured session updates.
//!
//! Grok writes one JSON object per `session/update` event to
//! `~/.grok/sessions/<percent-encoded-cwd>/<session-id>/updates.jsonl`. The
//! observer only considers files reached through the exact daemon
//! `home/workspace/<live-grok-agent>` path and verifies every record's session id
//! before it emits evidence.

use super::evidence::{Evidence, EvidenceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const TAIL_TICK: std::time::Duration = std::time::Duration::from_secs(1);
const DISCOVER_RECENT: std::time::Duration = std::time::Duration::from_secs(26 * 3600);
const FREE_USAGE_MARKER: &str = "subscription:free-usage-exhausted";

#[derive(Debug, Deserialize)]
struct SessionRecord {
    timestamp: Option<u64>,
    method: String,
    params: SessionParams,
}

#[derive(Debug, Deserialize)]
struct SessionParams {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    update: SessionUpdate,
}

#[derive(Debug, Deserialize)]
struct SessionUpdate {
    #[serde(rename = "sessionUpdate")]
    session_update: String,
    #[serde(rename = "type")]
    update_type: Option<String>,
    reason: Option<String>,
    is_rate_limited: Option<bool>,
    stop_reason: Option<String>,
}

fn parse_record(line: &str) -> Option<SessionRecord> {
    serde_json::from_str(line.trim()).ok()
}

/// Map one structured session line to a typed stream observation. Retry-only,
/// normal turn completion, malformed JSON, and every unrelated update are ignored.
pub(crate) fn record_to_evidence(line: &str, now_ms: u64) -> Option<Evidence> {
    let record = parse_record(line)?;
    let update = record.params.update;
    let is_quota_wall = matches!(
        record.method.as_str(),
        "session/update" | "_x.ai/session/update"
    ) && update.session_update == "retry_state"
        && update.update_type.as_deref() == Some("exhausted")
        && update.is_rate_limited == Some(true)
        && update
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(FREE_USAGE_MARKER));
    if !is_quota_wall {
        return None;
    }
    let at_ms = record
        .timestamp
        .map(|seconds| seconds.saturating_mul(1_000))
        .unwrap_or(now_ms);
    Some(Evidence::stream(EvidenceKind::UsageLimit, at_ms))
}

/// Map genuine later model progress so a durable quota wall can release after a
/// session actually recovers. A rate-limit completion is intentionally excluded.
fn record_to_progress(line: &str, now_ms: u64) -> Option<Evidence> {
    let record = parse_record(line)?;
    if !matches!(
        record.method.as_str(),
        "session/update" | "_x.ai/session/update"
    ) {
        return None;
    }
    let update = record.params.update;
    let kind = match update.session_update.as_str() {
        "agent_thought_chunk" | "agent_message_chunk" => EvidenceKind::Responding,
        "turn_completed" if update.stop_reason.as_deref() == Some("end_turn") => {
            EvidenceKind::TurnEnded {
                stop_reason: Some("end_turn".to_string()),
            }
        }
        _ => return None,
    };
    let at_ms = record
        .timestamp
        .map(|seconds| seconds.saturating_mul(1_000))
        .unwrap_or(now_ms);
    Some(Evidence::stream(kind, at_ms))
}

fn record_session_id(line: &str) -> Option<String> {
    parse_record(line)?.params.session_id
}

fn canonical_cwd(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn grok_home() -> Option<PathBuf> {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")))
}

/// Return only recent update logs under exact live-agent cwd directories. The
/// path construction is the ownership check; no loose basename or glob match is
/// used, so a same-named workspace elsewhere cannot receive evidence.
fn discover_session_files(
    grok_home: &Path,
    home: &Path,
    agents: &[String],
) -> Vec<(PathBuf, String)> {
    let sessions_root = grok_home.join("sessions");
    let recent_cutoff = std::time::SystemTime::now()
        .checked_sub(DISCOVER_RECENT)
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut out = Vec::new();
    for agent in agents {
        let cwd = canonical_cwd(&home.join("workspace").join(agent));
        let encoded = crate::backend::grok_session::encode_session_dir(&cwd);
        let cwd_dir = sessions_root.join(encoded);
        let Ok(entries) = std::fs::read_dir(cwd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let session_dir = entry.path();
            if !session_dir.is_dir() {
                continue;
            }
            let updates = session_dir.join("updates.jsonl");
            let fresh = std::fs::metadata(&updates)
                .and_then(|meta| meta.modified())
                .map(|modified| modified >= recent_cutoff)
                .unwrap_or(false);
            if fresh {
                out.push((updates, agent.clone()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

struct Cursor {
    offset: u64,
}

/// Spawn the Grok structured-session tailer. It owns no daemon state and only
/// reads Grok's files; the daemon's next boot rediscovers any live session.
pub fn spawn(
    _permit: &crate::daemon::owner_services::OwnerServicePermit,
    registry: crate::agent::AgentRegistry,
    home: PathBuf,
) {
    if !super::enabled() {
        return;
    }
    // fire-and-forget: detached read-only Grok session tail exits with the daemon;
    // no owned state or join handle is needed for a best-effort observer.
    let _ = std::thread::Builder::new()
        .name("shadow-grok-session".into())
        .spawn(move || {
            let Some(grok_home) = grok_home() else {
                tracing::info!(
                    tag = "#shadow-observer",
                    "grok session observer: no HOME/GROK_HOME — disabled"
                );
                return;
            };
            tracing::info!(tag = "#shadow-observer", root = %grok_home.display(),
                "grok structured-session observer listening (stream plane)");
            let mut cursors: HashMap<PathBuf, Cursor> = HashMap::new();
            loop {
                tail_once(&grok_home, &registry, &home, &mut cursors);
                std::thread::sleep(TAIL_TICK);
            }
        });
}

fn tail_once(
    grok_home: &Path,
    registry: &crate::agent::AgentRegistry,
    home: &Path,
    cursors: &mut HashMap<PathBuf, Cursor>,
) {
    let agents = live_grok_agents(registry);
    if agents.is_empty() {
        return;
    }
    for (file, agent) in discover_session_files(grok_home, home, &agents) {
        let cursor = cursors.entry(file.clone()).or_insert(Cursor { offset: 0 });
        drain_file(&file, cursor, &agent);
    }
}

fn live_grok_agents(registry: &crate::agent::AgentRegistry) -> Vec<String> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|handle| handle.backend_command.contains("grok"))
        .map(|handle| handle.name.to_string())
        .collect()
}

/// Read complete appended lines from one exact session file. A record is
/// attributed only if its in-band session id matches the parent directory id.
fn drain_file(file: &Path, cursor: &mut Cursor, agent: &str) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let Ok(file_handle) = std::fs::File::open(file) else {
        return;
    };
    let len = file_handle.metadata().map(|meta| meta.len()).unwrap_or(0);
    if len < cursor.offset {
        cursor.offset = 0;
    }
    if len <= cursor.offset {
        return;
    }
    let Some(session_id) = file
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
    else {
        return;
    };

    let mut reader = BufReader::new(file_handle);
    if reader.seek(SeekFrom::Start(cursor.offset)).is_err() {
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut consumed = cursor.offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if !line.ends_with('\n') {
            break;
        }
        consumed += n as u64;
        if record_session_id(&line).as_deref() != Some(session_id) {
            continue;
        }
        if let Some(evidence) =
            record_to_evidence(&line, now_ms).or_else(|| record_to_progress(&line, now_ms))
        {
            super::push(agent, evidence);
        }
    }
    cursor.offset = consumed;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::evidence::{Authority, EvidenceKind};
    use super::super::reducer::{AgentRuntime, Liveness, ObservedState, ScreenSignal};
    use super::*;
    use serial_test::serial;

    const REAL_LINES: &str =
        include_str!("../../../tests/fixtures/grok-s1-usage-limit-updates.jsonl");

    #[test]
    fn preserved_real_grok_lines_classify_only_exhausted_quota() {
        let lines: Vec<&str> = REAL_LINES.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(record_to_evidence(lines[0], 1_785_471_700_000).is_none());
        let ev = record_to_evidence(lines[1], 1_785_471_700_000).unwrap();
        assert_eq!(ev.kind, EvidenceKind::UsageLimit);
        assert_eq!(ev.authority, Authority::Stream);
        assert_eq!(ev.at_ms, 1_785_471_700_000);
        assert!(record_to_evidence(lines[2], 1_785_471_700_000).is_none());

        let normal_end = r#"{"timestamp":1785471800,"method":"session/update","params":{"sessionId":"019fb667-4172-72d2-85b3-d647f819e0ed","update":{"sessionUpdate":"turn_completed","stop_reason":"end_turn"}}}"#;
        assert!(record_to_evidence(normal_end, 0).is_none());
        assert!(matches!(
            record_to_progress(normal_end, 0),
            Some(Evidence {
                kind: EvidenceKind::TurnEnded { .. },
                ..
            })
        ));
        assert!(record_to_progress(lines[2], 0).is_none());
    }

    #[test]
    fn malformed_structured_update_is_ignored() {
        assert!(record_to_evidence("not-json", 1_000).is_none());
    }

    #[test]
    #[serial(shadow_observer)]
    fn exact_cwd_and_session_tail_reaches_shared_buffer() {
        let root = std::env::temp_dir().join(format!("agend_grok_{}", std::process::id()));
        let home = root.join("daemon");
        let cwd = home.join("workspace").join("grok-agent");
        let session_id = "019fb667-4172-72d2-85b3-d647f819e0ed";
        let updates = root
            .join(".grok")
            .join("sessions")
            .join(crate::backend::grok_session::encode_session_dir(
                &canonical_cwd(&cwd),
            ))
            .join(session_id)
            .join("updates.jsonl");
        std::fs::create_dir_all(updates.parent().unwrap()).unwrap();
        let foreign_session_line = REAL_LINES
            .lines()
            .nth(1)
            .unwrap()
            .replace(session_id, "foreign-session");
        std::fs::write(&updates, format!("{REAL_LINES}{foreign_session_line}\n")).unwrap();

        let mut cursor = Cursor { offset: 0 };
        drain_file(&updates, &mut cursor, "grok-agent");
        let evidence = super::super::peek("grok-agent");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].kind, EvidenceKind::UsageLimit);
        assert_eq!(evidence[0].authority, Authority::Stream);

        super::super::drain("grok-agent");
        super::super::forget_agent("grok-agent");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn discovery_rejects_same_named_workspace_outside_daemon_home() {
        let root =
            std::env::temp_dir().join(format!("agend_grok_discovery_{}", std::process::id()));
        let home = root.join("daemon");
        let owner_cwd = home.join("workspace").join("grok-agent");
        let owner_file = root
            .join(".grok")
            .join("sessions")
            .join(crate::backend::grok_session::encode_session_dir(
                &canonical_cwd(&owner_cwd),
            ))
            .join("owner-session")
            .join("updates.jsonl");
        let stray_cwd = root.join("operator").join("workspace").join("grok-agent");
        let stray_file = root
            .join(".grok")
            .join("sessions")
            .join(crate::backend::grok_session::encode_session_dir(
                &canonical_cwd(&stray_cwd),
            ))
            .join("stray-session")
            .join("updates.jsonl");
        std::fs::create_dir_all(owner_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stray_file.parent().unwrap()).unwrap();
        std::fs::write(&owner_file, "").unwrap();
        std::fs::write(&stray_file, "").unwrap();

        let discovered =
            discover_session_files(&root.join(".grok"), &home, &["grok-agent".to_string()]);
        assert_eq!(discovered, vec![(owner_file, "grok-agent".to_string())]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn quota_is_operated_and_lasts_until_later_progress() {
        let mut runtime = AgentRuntime::default();
        runtime.ingest(&Evidence::stream(EvidenceKind::UsageLimit, 2_000));
        let live = Liveness {
            api_in_flight: false,
            productive_silent_ms: 0,
            child_alive: true,
        };
        let blocked = runtime.observe(ScreenSignal::Idle, &live, 2_100);
        assert_eq!(blocked.state, ObservedState::UsageLimit);
        assert_eq!(
            super::super::gate::gated_override(crate::state::AgentState::Idle, &blocked),
            Some(crate::state::AgentState::UsageLimit)
        );

        runtime.ingest(&Evidence::stream(EvidenceKind::TurnStarted, 1_900));
        assert_eq!(
            runtime.observe(ScreenSignal::Idle, &live, 2_200).state,
            ObservedState::UsageLimit,
            "older progress cannot clear a newer quota wall"
        );
        runtime.ingest(&Evidence::stream(EvidenceKind::TurnStarted, 2_300));
        assert_ne!(
            runtime.observe(ScreenSignal::Idle, &live, 2_400).state,
            ObservedState::UsageLimit,
            "genuine later progress clears the durable quota wall"
        );
        runtime.ingest(&Evidence::stream(EvidenceKind::UsageLimit, 2_100));
        assert_ne!(
            runtime.observe(ScreenSignal::Idle, &live, 2_500).state,
            ObservedState::UsageLimit,
            "replayed older quota evidence cannot resurrect a recovered wall"
        );
    }
}
