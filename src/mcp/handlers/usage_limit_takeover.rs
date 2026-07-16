//! Operator-only, durable PREPARE seam for usage-limit takeover.
//!
//! This slice deliberately stops at a journaled `Prepared` phase. It never
//! changes a binding, task owner, process, model, or resume queue.

use crate::daemon::supervisor::usage_limit_control::{
    acquire_binding_lock, candidate_is_eligible, fleet_facts, Episode, EpisodeState, TickInput,
};
use crate::mcp::handlers::dispatch::HandlerCtx;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const JOURNAL_NAME: &str = "usage_limit_takeover.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparationJournal {
    episode_id: String,
    source: String,
    candidate: String,
    binding_issued_at: String,
    source_head: String,
    phase: String,
    created_at: String,
}

fn refusal(code: &str, message: impl Into<String>) -> Value {
    let message = message.into();
    json!({
        "error": format!("usage_limit_takeover refused: {message}"),
        "error_code": code,
        "code": code,
        "message": message,
    })
}

fn field<'a>(binding: &'a Value, name: &str) -> Option<&'a str> {
    binding
        .get(name)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

fn journal_path(home: &Path, source: &str) -> PathBuf {
    crate::paths::runtime_dir(home)
        .join(source)
        .join(JOURNAL_NAME)
}

fn git_proof(worktree: &Path, branch: &str) -> Result<String, Value> {
    if !worktree.is_dir() {
        return Err(refusal(
            "worktree_unreadable",
            "binding worktree is not a directory",
        ));
    }
    let current_branch = crate::git_helpers::git_cmd(worktree, &["branch", "--show-current"])
        .map_err(|_| refusal("git_unreadable", "cannot read source worktree branch"))?;
    if current_branch != branch {
        return Err(refusal(
            "branch_mismatch",
            format!("source worktree branch is {current_branch:?}, expected {branch:?}"),
        ));
    }
    let status = crate::git_helpers::git_cmd(worktree, &["status", "--porcelain"])
        .map_err(|_| refusal("git_unreadable", "cannot read source worktree status"))?;
    if !status.is_empty() {
        return Err(refusal(
            "source_dirty",
            "source worktree has uncommitted changes",
        ));
    }
    crate::git_helpers::git_cmd(worktree, &["rev-parse", "HEAD"])
        .map_err(|_| refusal("git_unreadable", "cannot prove source HEAD"))
}

fn load_episode(home: &Path, source: &str) -> Result<Episode, Value> {
    let path = crate::paths::runtime_dir(home)
        .join(source)
        .join("usage_limit_episode.json");
    let bytes = std::fs::read(&path)
        .map_err(|_| refusal("episode_unreadable", "usage-limit episode is unreadable"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| refusal("episode_unreadable", "usage-limit episode is malformed"))
}

fn validate_blocked_task(
    home: &Path,
    task_id: &str,
    source: &str,
    branch: &str,
    issued_at: &str,
    episode_id: &str,
    candidate: &str,
) -> Result<(), Value> {
    let routed = crate::tasks::load_routed(home, task_id)
        .map_err(|_| refusal("task_unreadable", "task route is not uniquely readable"))?;
    if routed.task.status != crate::task_events::TaskStatus::Blocked
        || routed.task.assignee.as_deref() != Some(source)
        || routed.task.branch.as_deref() != Some(branch)
        || routed.record().owner.as_ref().map(|owner| owner.as_str()) != Some(source)
        || routed.record().branch.as_deref() != Some(branch)
    {
        return Err(refusal(
            "task_generation_mismatch",
            "task is not blocked for the exact source and branch",
        ));
    }
    let reason = routed
        .record()
        .block_reason
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .ok_or_else(|| {
            refusal(
                "blocked_reason_mismatch",
                "task blocked reason is unreadable",
            )
        })?;
    let base_exact = reason.get("type").and_then(Value::as_str) == Some("usage_limit_episode")
        && reason.get("episode_id").and_then(Value::as_str) == Some(episode_id)
        && reason.get("source").and_then(Value::as_str) == Some(source)
        && reason.get("binding_issued_at").and_then(Value::as_str) == Some(issued_at)
        && reason.get("branch").and_then(Value::as_str) == Some(branch)
        && reason.get("state").and_then(Value::as_str) == Some("CandidateReady")
        && reason
            .get("proposal")
            .and_then(|proposal| proposal.get("executable"))
            .and_then(Value::as_bool)
            == Some(false)
        && reason
            .get("proposal")
            .and_then(|proposal| proposal.get("requires"))
            .and_then(Value::as_str)
            == Some("operator_takeover_slice_2");
    let recorded_candidate = reason
        .get("proposal")
        .and_then(|proposal| proposal.get("candidate"))
        .and_then(Value::as_str);
    if base_exact && recorded_candidate == Some(candidate) {
        Ok(())
    } else if base_exact && recorded_candidate.is_some() {
        Err(refusal(
            "candidate_mismatch",
            "task block reason records a different candidate",
        ))
    } else {
        Err(refusal(
            "blocked_reason_mismatch",
            "task block reason does not prove this episode and candidate",
        ))
    }
}

fn validate_candidate(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    source: &str,
    task: &crate::tasks::Task,
    candidate: &str,
) -> Result<(), Value> {
    let fleet = crate::teams::try_load_fleet(home)
        .map_err(|_| refusal("fleet_unreadable", "fleet configuration is unreadable"))?;
    let source_backend = fleet
        .resolve_instance(source)
        .map(|resolved| resolved.backend.as_str().to_string())
        .ok_or_else(|| refusal("source_unreadable", "source instance is not configured"))?;
    let (source_team, source_role, candidates, _) = fleet_facts(home, registry, source, Some(task));
    let input = TickInput {
        now: Utc::now(),
        raw_state: crate::state::AgentState::UsageLimit,
        source_backend,
        source_team,
        source_role,
        unlock_at: None,
        correlation: None,
        candidates: Vec::new(),
        recipient: String::new(),
    };
    let Some(facts) = candidates.iter().find(|facts| facts.name == candidate) else {
        return Err(refusal(
            "candidate_ineligible",
            "episode candidate is not live in the source team",
        ));
    };
    if candidate_is_eligible(&input, facts) {
        Ok(())
    } else {
        Err(refusal(
            "candidate_ineligible",
            "episode candidate no longer satisfies current eligibility",
        ))
    }
}

fn prepare(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    source: &str,
    episode_id: &str,
) -> Value {
    let Some(binding) =
        crate::daemon::supervisor::usage_limit_control::read_current_binding(home, source)
    else {
        return refusal(
            "binding_unreadable",
            "source binding is missing or malformed",
        );
    };
    if field(&binding, "agent") != Some(source) {
        return refusal(
            "binding_source_mismatch",
            "binding agent does not match source",
        );
    }
    let episode = match load_episode(home, source) {
        Ok(episode) => episode,
        Err(error) => return error,
    };
    let candidate = match episode.candidate.as_deref() {
        Some(candidate) if !candidate.is_empty() => candidate,
        _ => {
            return refusal(
                "candidate_ineligible",
                "CandidateReady episode has no candidate",
            )
        }
    };
    if episode.state != EpisodeState::CandidateReady {
        return refusal(
            "episode_not_candidate_ready",
            "episode is not CandidateReady",
        );
    }
    if episode.key.source != source || episode.key.notification_id() != episode_id {
        return refusal(
            "episode_mismatch",
            "requested episode is not the persisted source episode",
        );
    }
    let issued_at = match field(&binding, "issued_at") {
        Some(value) if value == episode.key.binding_issued_at => value,
        _ => {
            return refusal(
                "binding_generation_mismatch",
                "binding generation differs from episode",
            )
        }
    };
    if field(&binding, "task_id") != Some(episode.key.task_id.as_str())
        || field(&binding, "branch") != Some(episode.key.branch.as_str())
    {
        return refusal(
            "binding_generation_mismatch",
            "binding task or branch differs from episode",
        );
    }
    let worktree = match field(&binding, "worktree") {
        Some(path) => PathBuf::from(path),
        None => return refusal("worktree_unreadable", "binding has no worktree path"),
    };
    let source_repo = match field(&binding, "source_repo") {
        Some(path) => PathBuf::from(path),
        None => return refusal("source_unreadable", "binding has no source repository path"),
    };
    if !source_repo.is_dir()
        || crate::git_helpers::git_cmd(&source_repo, &["rev-parse", "--show-toplevel"]).is_err()
    {
        return refusal("source_unreadable", "binding has no source repository path");
    }
    let source_head = match git_proof(&worktree, &episode.key.branch) {
        Ok(head) => head,
        Err(error) => return error,
    };
    let routed = match crate::tasks::load_routed(home, &episode.key.task_id) {
        Ok(routed) => routed,
        Err(_) => return refusal("task_unreadable", "task route is not uniquely readable"),
    };
    if let Err(error) = validate_blocked_task(
        home,
        &episode.key.task_id,
        source,
        &episode.key.branch,
        issued_at,
        episode_id,
        candidate,
    ) {
        return error;
    }
    if let Err(error) = validate_candidate(home, registry, source, &routed.task, candidate) {
        return error;
    }

    let path = journal_path(home, source);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let journal: PreparationJournal = match serde_json::from_slice(&bytes) {
                Ok(journal) => journal,
                Err(_) => return refusal("journal_unreadable", "takeover journal is malformed"),
            };
            if journal.episode_id == episode_id
                && journal.source == source
                && journal.candidate == candidate
                && journal.binding_issued_at == issued_at
                && journal.source_head == source_head
                && journal.phase == "Prepared"
            {
                return json!({
                    "status": "prepared",
                    "phase": journal.phase,
                    "episode_id": journal.episode_id,
                    "source": journal.source,
                    "candidate": journal.candidate,
                    "binding_issued_at": journal.binding_issued_at,
                    "source_head": journal.source_head,
                    "created_at": journal.created_at,
                    "idempotent": true,
                });
            }
            return refusal(
                "conflicting_preparation",
                "source already has a different prepared takeover",
            );
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound && !path.is_dir() => {
            return refusal("journal_unreadable", "takeover journal cannot be read")
        }
        Err(_) => {}
    }

    let journal = PreparationJournal {
        episode_id: episode_id.to_string(),
        source: source.to_string(),
        candidate: candidate.to_string(),
        binding_issued_at: issued_at.to_string(),
        source_head: source_head.clone(),
        phase: "Prepared".into(),
        created_at: Utc::now().to_rfc3339(),
    };
    let bytes = match serde_json::to_vec_pretty(&journal) {
        Ok(bytes) => bytes,
        Err(_) => return refusal("journal_write_failed", "cannot serialize takeover journal"),
    };
    if crate::store::atomic_write(&path, &bytes).is_err() {
        return refusal(
            "journal_write_failed",
            "cannot durably write takeover journal",
        );
    }
    json!({
        "status": "prepared",
        "phase": "Prepared",
        "episode_id": episode_id,
        "source": source,
        "candidate": candidate,
        "binding_issued_at": issued_at,
        "source_head": source_head,
        "created_at": journal.created_at,
        "idempotent": false,
    })
}

pub(crate) fn handle_usage_limit_takeover(ctx: &HandlerCtx<'_>) -> Value {
    if ctx.sender.is_some() {
        return refusal("operator_only", "usage_limit_takeover is operator-only");
    }
    let Some(runtime) = ctx.runtime else {
        return refusal(
            "runtime_unavailable",
            "usage_limit_takeover requires daemon runtime",
        );
    };
    let Some(source) = ctx.args.get("source").and_then(Value::as_str) else {
        return refusal("missing_source", "source is required");
    };
    let Some(episode_id) = ctx.args.get("episode_id").and_then(Value::as_str) else {
        return refusal("missing_episode_id", "episode_id is required");
    };
    if source.is_empty() || episode_id.is_empty() {
        return refusal(
            "invalid_argument",
            "source and episode_id must be non-empty",
        );
    }
    let _lock = match acquire_binding_lock(ctx.home, source) {
        Ok(lock) => lock,
        Err(_) => {
            return refusal(
                "source_lock_failed",
                "cannot acquire source-scoped takeover lock",
            )
        }
    };
    prepare(ctx.home, &runtime.registry, source, episode_id)
}
