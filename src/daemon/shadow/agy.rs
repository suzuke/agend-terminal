//! #2413 Phase D — agy session-tail observer source.
//!
//! Tails the agy `history.jsonl` to resolve active workspace ↔ conversationId mapping,
//! then tails the matching `transcript.jsonl` under `brain/<uuid>/.system_generated/logs/`
//! to produce [`Evidence`] (`authority=Stream`).

use super::evidence::{Evidence, EvidenceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const TAIL_TICK: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Deserialize)]
struct HistoryLine {
    workspace: String,
    #[serde(rename = "conversationId")]
    conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgyStep {
    #[serde(rename = "type")]
    step_type: String,
    created_at: String,
    #[serde(default)]
    tool_calls: Vec<AgyToolCall>,
}

#[derive(Debug, Deserialize)]
struct AgyToolCall {
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

pub(crate) fn record_to_evidence(line: &str) -> Option<Evidence> {
    let step: AgyStep = serde_json::from_str(line.trim()).ok()?;
    let at_ms = parse_iso_ms(&step.created_at)?;
    let kind = match step.step_type.as_str() {
        "USER_INPUT" => EvidenceKind::TurnStarted,
        "PLANNER_RESPONSE" => {
            if !step.tool_calls.is_empty() {
                let first_tool = &step.tool_calls[0];
                let tool_name = extract_tool_name(first_tool);
                EvidenceKind::ToolStarted { name: Some(tool_name) }
            } else {
                EvidenceKind::Responding
            }
        }
        "CONVERSATION_HISTORY" | "CHECKPOINT" | "SYSTEM_MESSAGE" | "GENERIC" => return None,
        _ => EvidenceKind::ToolEnded,
    };
    Some(Evidence::stream(kind, at_ms))
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

fn extract_tool_name(tool_call: &AgyToolCall) -> String {
    if tool_call.name == "call_mcp_tool" {
        if let Some(args) = &tool_call.args {
            if let Some(tool_name_val) = args.get("ToolName") {
                if let Some(s) = tool_name_val.as_str() {
                    return s.trim_matches('"').to_string();
                }
            }
        }
    }
    tool_call.name.clone()
}

fn live_agy_agents(registry: &crate::agent::AgentRegistry) -> Vec<String> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|h| {
            crate::backend::Backend::from_command(&h.backend_command)
                == Some(crate::backend::Backend::Agy)
        })
        .map(|h| h.name.to_string())
        .collect()
}

fn agy_dir(home: &Path) -> Option<PathBuf> {
    let mut dir = home.join(".gemini").join("antigravity-cli");
    if !dir.join("history.jsonl").exists() {
        if let Some(parent) = home.parent() {
            let p_dir = parent.join(".gemini").join("antigravity-cli");
            if p_dir.join("history.jsonl").exists() {
                dir = p_dir;
            }
        }
    }
    if dir.join("history.jsonl").exists() {
        Some(dir)
    } else {
        None
    }
}

pub fn spawn(registry: crate::agent::AgentRegistry, home: PathBuf) {
    if !super::enabled() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("shadow-agy-tail".into())
        .spawn(move || {
            let mut history_offset = 0u64;
            let mut transcript_cursors: HashMap<PathBuf, u64> = HashMap::new();
            let mut workspace_conversations: HashMap<String, String> = HashMap::new();

            loop {
                if let Some(dir) = agy_dir(&home) {
                    let history_path = dir.join("history.jsonl");
                    tail_history(&history_path, &mut history_offset, &mut workspace_conversations);
                    tail_transcripts(&registry, &home, &dir, &workspace_conversations, &mut transcript_cursors);
                }
                std::thread::sleep(TAIL_TICK);
            }
        });
}

fn tail_history(
    history_path: &Path,
    offset: &mut u64,
    workspace_conversations: &mut HashMap<String, String>,
) {
    let Ok(f) = std::fs::File::open(history_path) else { return; };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset { *offset = 0; }
    if len <= *offset { return; }

    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let mut reader = BufReader::new(f);
    if reader.seek(SeekFrom::Start(*offset)).is_err() { return; }

    let mut consumed = *offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if !line.ends_with('\n') { break; }
        consumed += n as u64;
        if let Ok(hl) = serde_json::from_str::<HistoryLine>(line.trim()) {
            if let Some(cid) = hl.conversation_id {
                workspace_conversations.insert(hl.workspace, cid);
            }
        }
    }
    *offset = consumed;
}

fn tail_transcripts(
    registry: &crate::agent::AgentRegistry,
    home: &Path,
    agy_dir: &Path,
    workspace_conversations: &HashMap<String, String>,
    transcript_cursors: &mut HashMap<PathBuf, u64>,
) {
    let agents = live_agy_agents(registry);
    if agents.is_empty() { return; }

    for agent_name in agents {
        let ws_path = home.join("workspace").join(&agent_name);
        let ws_str = ws_path.to_string_lossy();

        let conversation_id = workspace_conversations
            .get(ws_str.as_ref())
            .or_else(|| {
                workspace_conversations.iter().find_map(|(ws, cid)| {
                    if agent_for_workspace(ws, home, &[agent_name.clone()]).is_some() {
                        Some(cid)
                    } else {
                        None
                    }
                })
            });

        let Some(cid) = conversation_id else { continue; };
        let transcript_path = agy_dir
            .join("brain")
            .join(cid)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");

        if !transcript_path.exists() { continue; }

        let offset = transcript_cursors.entry(transcript_path.clone()).or_insert(0);
        drain_transcript_file(&transcript_path, offset, &agent_name);
    }
}

fn drain_transcript_file(file: &Path, offset: &mut u64, agent_name: &str) {
    let Ok(f) = std::fs::File::open(file) else { return; };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset { *offset = 0; }
    if len <= *offset { return; }

    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let mut reader = BufReader::new(f);
    if reader.seek(SeekFrom::Start(*offset)).is_err() { return; }

    let mut consumed = *offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if !line.ends_with('\n') { break; }
        consumed += n as u64;
        if let Some(ev) = record_to_evidence(&line) {
            super::push(agent_name, ev);
        }
    }
    *offset = consumed;
}

fn agent_for_workspace(workspace: &str, home: &Path, agy_agents: &[String]) -> Option<String> {
    let path = Path::new(workspace);
    let folder_name = path.file_name()?.to_str()?;
    if agy_agents.iter().any(|name| name == folder_name) {
        return Some(folder_name.to_string());
    }
    let ws = home.join("workspace");
    let canonical = Path::new(strip_private(workspace));
    agy_agents
        .iter()
        .find(|name| canonical == ws.join(name))
        .cloned()
}

#[cfg(unix)]
fn strip_private(p: &str) -> &str {
    match p.strip_prefix("/private") {
        Some(rest) if rest.starts_with('/') => rest,
        _ => p,
    }
}
#[cfg(not(unix))]
fn strip_private(p: &str) -> &str { p }

#[cfg(test)]
mod tests {
    use super::super::evidence::{Authority, EvidenceKind};
    use super::*;

    fn kind_of(line: &str) -> Option<EvidenceKind> {
        record_to_evidence(line).map(|e| e.kind)
    }

    #[test]
    fn maps_agy_step_lifecycle() {
        assert_eq!(
            kind_of(r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-06-25T16:04:17Z"}"#),
            Some(EvidenceKind::TurnStarted)
        );
        assert_eq!(
            kind_of(r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-06-25T16:04:19Z","tool_calls":[{"name":"call_mcp_tool","args":{"ToolName":"\"task\""}}]}"#),
            Some(EvidenceKind::ToolStarted {
                name: Some("task".to_string())
            })
        );
        assert_eq!(
            kind_of(r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-06-25T16:04:19Z","tool_calls":[{"name":"run_command","args":{}}]}"#),
            Some(EvidenceKind::ToolStarted {
                name: Some("run_command".to_string())
            })
        );
        assert_eq!(
            kind_of(r#"{"step_index":423,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-06-24T13:00:29Z"}"#),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(r#"{"step_index":6,"source":"MODEL","type":"MCP_TOOL","status":"DONE","created_at":"2026-06-25T16:04:20Z"}"#),
            Some(EvidenceKind::ToolEnded)
        );
    }

    #[test]
    fn ignore_metadata_steps() {
        assert_eq!(
            kind_of(r#"{"step_index":1,"source":"SYSTEM","type":"CONVERSATION_HISTORY","status":"DONE","created_at":"2026-06-25T16:04:17Z"}"#),
            None
        );
        assert_eq!(
            kind_of(r#"{"step_index":4,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-06-25T16:04:18Z"}"#),
            None
        );
    }

    #[test]
    fn evidence_is_stream_authority_with_timestamp() {
        let ev = record_to_evidence(
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-06-25T16:04:17Z"}"#
        ).unwrap();
        assert_eq!(ev.authority, Authority::Stream);
        assert_eq!(ev.at_ms, 1782403457000); // 2026-06-25T16:04:17Z in epoch ms
    }

    #[cfg(unix)]
    #[test]
    fn dogfood_agy_tooluse_yields_stream_observed_status() {
        use super::super::reducer::{Liveness, ObservedState, ScreenSignal};
        use std::io::Write;

        let home = std::env::temp_dir().join(format!("agend_agy_dog_{}", std::process::id()));
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let ws = home.join("workspace").join("adog");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(agy_dir.join("brain").join("sess123").join(".system_generated").join("logs")).unwrap();

        let history_path = agy_dir.join("history.jsonl");
        let mut hist_f = std::fs::File::create(&history_path).unwrap();
        writeln!(hist_f, r#"{{"workspace":"{}","conversationId":"sess123"}}"#, ws.to_string_lossy()).unwrap();
        hist_f.flush().unwrap();

        let trans_path = agy_dir.join("brain").join("sess123").join(".system_generated").join("logs").join("transcript.jsonl");
        let mut trans_f = std::fs::File::create(&trans_path).unwrap();
        writeln!(trans_f, "{}", r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-06-25T16:04:17Z"}"#).unwrap();
        writeln!(trans_f, "{}", r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-06-25T16:04:19Z","tool_calls":[{"name":"run_command","args":{}}]}"#).unwrap();
        trans_f.flush().unwrap();

        let mut hist_offset = 0u64;
        let mut trans_cursors = HashMap::new();
        let mut ws_convs = HashMap::new();

        tail_history(&history_path, &mut hist_offset, &mut ws_convs);
        assert_eq!(ws_convs.get(ws.to_str().unwrap()).map(|s| s.as_str()), Some("sess123"));

        let registry = crate::agent::AgentRegistry::default();
        let inst_id = crate::types::InstanceId::new();
        let mut handle = crate::agent::mk_test_handle("adog", inst_id);
        handle.backend_command = "agy".to_string();
        {
            let mut reg = crate::agent::lock_registry(&registry);
            reg.insert(inst_id, handle);
        }

        tail_transcripts(&registry, &home, &agy_dir, &ws_convs, &mut trans_cursors);

        let live = Liveness {
            api_in_flight: true,
            productive_silent_ms: 0,
            child_alive: true,
        };
        let observed = super::super::observe("adog", ScreenSignal::Working, &live, 1782403460000);
        assert_eq!(observed.authority, Authority::Stream);
        assert_eq!(observed.state, ObservedState::ToolUse);

        super::super::forget_agent("adog");
        let _ = std::fs::remove_dir_all(&home);
    }
}
