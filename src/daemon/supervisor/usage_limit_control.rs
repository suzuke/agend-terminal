use crate::state::AgentState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct EpisodeKey {
    pub(crate) task_id: String,
    pub(crate) source: String,
    pub(crate) binding_issued_at: String,
    pub(crate) branch: String,
}

impl EpisodeKey {
    pub(crate) fn notification_id(&self) -> String {
        format!(
            "usage-limit:{}:{}:{}:{}",
            self.task_id, self.source, self.binding_issued_at, self.branch
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Correlation {
    pub(crate) task_id: String,
    pub(crate) source: String,
    pub(crate) owner: String,
    pub(crate) task_status: String,
    pub(crate) task_branch: String,
    pub(crate) binding_task_id: String,
    pub(crate) binding_branch: String,
    pub(crate) binding_issued_at: String,
}

impl TryFrom<&Correlation> for EpisodeKey {
    type Error = ();

    fn try_from(value: &Correlation) -> Result<Self, Self::Error> {
        if value.task_id.is_empty()
            || value.binding_issued_at.is_empty()
            || value.owner != value.source
            || value.task_id != value.binding_task_id
            || value.task_branch != value.binding_branch
            || !matches!(
                value.task_status.as_str(),
                "claimed" | "in_progress" | "blocked"
            )
        {
            return Err(());
        }
        Ok(Self {
            task_id: value.task_id.clone(),
            source: value.source.clone(),
            binding_issued_at: value.binding_issued_at.clone(),
            branch: value.task_branch.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EpisodeState {
    Detected,
    AwaitReset,
    CandidateReady,
    NeedsOperator,
    Recovered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Episode {
    pub(crate) key: EpisodeKey,
    pub(crate) state: EpisodeState,
    pub(crate) consecutive_ticks: u8,
    pub(crate) candidate: Option<String>,
    #[serde(default)]
    pub(crate) unlock_at: Option<String>,
    #[serde(default)]
    pub(crate) recipient: String,
}

impl Episode {
    #[cfg(test)]
    pub(crate) fn activated(
        key: EpisodeKey,
        state: EpisodeState,
        candidate: Option<String>,
    ) -> Self {
        Self {
            key,
            state,
            consecutive_ticks: 2,
            candidate,
            unlock_at: None,
            recipient: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateFacts {
    pub(crate) name: String,
    pub(crate) team: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) backend: String,
    pub(crate) live: bool,
    pub(crate) healthy: bool,
    pub(crate) idle: bool,
    pub(crate) bound: bool,
    pub(crate) has_active_task: bool,
    pub(crate) current_usage_limit: bool,
    pub(crate) routing_compatible: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TickInput {
    pub(crate) now: DateTime<Utc>,
    pub(crate) raw_state: AgentState,
    pub(crate) source_backend: String,
    pub(crate) source_team: Option<String>,
    pub(crate) source_role: Option<String>,
    pub(crate) unlock_at: Option<DateTime<Utc>>,
    pub(crate) correlation: Option<Correlation>,
    pub(crate) candidates: Vec<CandidateFacts>,
    pub(crate) recipient: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    NoEpisode,
    Detected,
    AwaitReset,
    CandidateReady(String),
    NeedsOperator,
    AlreadyActive,
    Recovered,
}

trait ControlEffects {
    fn load_episode(&self) -> Option<Episode>;
    fn persist_episode(&mut self, episode: Episode) -> anyhow::Result<()>;
    fn block_task(&mut self, key: &EpisodeKey) -> anyhow::Result<()>;
    fn recover_task_atomically(&mut self, key: &EpisodeKey) -> anyhow::Result<bool>;
    fn notify_once(
        &mut self,
        key: &EpisodeKey,
        recipient: &str,
        executable: bool,
    ) -> anyhow::Result<()>;
    fn reconcile_active(&mut self, _key: &EpisodeKey, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

pub(crate) fn candidate_is_eligible(input: &TickInput, candidate: &CandidateFacts) -> bool {
    candidate.team == input.source_team
        && candidate.role == input.source_role
        && candidate.backend != input.source_backend
        && candidate.live
        && candidate.healthy
        && candidate.idle
        && !candidate.bound
        && !candidate.has_active_task
        && !candidate.current_usage_limit
        && candidate.routing_compatible
}

pub(crate) fn choose_candidate(input: &TickInput, candidates: &[CandidateFacts]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| candidate_is_eligible(input, candidate))
        .map(|candidate| candidate.name.clone())
}

pub(crate) fn notification_recipient(
    source: &str,
    orchestrator: Result<Option<&str>, ()>,
) -> String {
    match orchestrator {
        Ok(Some(orchestrator)) if orchestrator != source => orchestrator.to_string(),
        Ok(Some(_)) | Ok(None) | Err(()) => "general".to_string(),
    }
}

fn activation_state(input: &TickInput) -> (EpisodeState, Option<String>, TickOutcome) {
    let Some(unlock_at) = input.unlock_at else {
        return (
            EpisodeState::NeedsOperator,
            None,
            TickOutcome::NeedsOperator,
        );
    };
    if unlock_at.signed_duration_since(input.now) < chrono::Duration::minutes(30) {
        return (EpisodeState::AwaitReset, None, TickOutcome::AwaitReset);
    }
    match choose_candidate(input, &input.candidates) {
        Some(candidate) => (
            EpisodeState::CandidateReady,
            Some(candidate.clone()),
            TickOutcome::CandidateReady(candidate),
        ),
        None => (
            EpisodeState::NeedsOperator,
            None,
            TickOutcome::NeedsOperator,
        ),
    }
}

fn observe_tick<E: ControlEffects>(
    effects: &mut E,
    input: TickInput,
) -> anyhow::Result<TickOutcome> {
    let existing = effects.load_episode();

    if input.raw_state != AgentState::UsageLimit {
        let Some(mut episode) = existing else {
            return Ok(TickOutcome::NoEpisode);
        };
        if episode.state == EpisodeState::Detected {
            episode.consecutive_ticks = 0;
            effects.persist_episode(episode)?;
            return Ok(TickOutcome::NoEpisode);
        }
        if matches!(episode.state, EpisodeState::Recovered) {
            return Ok(TickOutcome::NoEpisode);
        }
        if !matches!(input.raw_state, AgentState::Idle | AgentState::Active) {
            return Ok(TickOutcome::AlreadyActive);
        }
        if effects.recover_task_atomically(&episode.key)? {
            episode.state = EpisodeState::Recovered;
            effects.persist_episode(episode)?;
            return Ok(TickOutcome::Recovered);
        }
        episode.state = EpisodeState::NeedsOperator;
        effects.persist_episode(episode)?;
        return Ok(TickOutcome::NeedsOperator);
    }

    let Some(correlation) = input.correlation.as_ref() else {
        if let Some(mut episode) = existing {
            if episode.state != EpisodeState::Recovered {
                episode.state = EpisodeState::NeedsOperator;
                effects.persist_episode(episode)?;
            }
        }
        return Ok(TickOutcome::NeedsOperator);
    };
    let Ok(current_key) = EpisodeKey::try_from(correlation) else {
        if let Some(mut episode) = existing {
            if episode.state != EpisodeState::Recovered {
                episode.state = EpisodeState::NeedsOperator;
                effects.persist_episode(episode)?;
            }
        }
        return Ok(TickOutcome::NeedsOperator);
    };

    match existing {
        // #2759 r1: `Recovered` is terminal HISTORY, never an active episode —
        // without this exclusion a same-key recurrence hit reconcile_active on
        // ONE tick (no two-tick cycle) and the re-blocked task could never
        // heal, because both recovery paths skip Recovered episodes. Same-key
        // Recovered falls through to the fresh-Detected arm below.
        Some(mut episode)
            if episode.key == current_key && episode.state != EpisodeState::Recovered =>
        {
            if episode.state != EpisodeState::Detected {
                let recipient = if episode.recipient.is_empty() {
                    input.recipient.as_str()
                } else {
                    episode.recipient.as_str()
                };
                effects.reconcile_active(&current_key, recipient)?;
                return Ok(if episode.state == EpisodeState::NeedsOperator {
                    TickOutcome::NeedsOperator
                } else {
                    TickOutcome::AlreadyActive
                });
            }
            episode.consecutive_ticks = episode.consecutive_ticks.saturating_add(1);
            if episode.consecutive_ticks < 2 {
                effects.persist_episode(episode)?;
                return Ok(TickOutcome::Detected);
            }
            let (state, candidate, outcome) = activation_state(&input);
            episode.state = state;
            episode.candidate = candidate;
            episode.unlock_at = input.unlock_at.map(|deadline| deadline.to_rfc3339());
            episode.recipient = input.recipient.clone();
            effects.persist_episode(episode)?;
            effects.block_task(&current_key)?;
            effects.notify_once(&current_key, &input.recipient, false)?;
            Ok(outcome)
        }
        Some(episode) if episode.state == EpisodeState::Recovered => {
            if !matches!(correlation.task_status.as_str(), "claimed" | "in_progress") {
                return Ok(TickOutcome::NeedsOperator);
            }
            effects.persist_episode(Episode {
                key: current_key,
                state: EpisodeState::Detected,
                consecutive_ticks: 1,
                candidate: None,
                unlock_at: input.unlock_at.map(|deadline| deadline.to_rfc3339()),
                recipient: input.recipient,
            })?;
            Ok(TickOutcome::Detected)
        }
        Some(mut episode) => {
            episode.state = EpisodeState::NeedsOperator;
            effects.persist_episode(episode)?;
            Ok(TickOutcome::NeedsOperator)
        }
        None => {
            if !matches!(correlation.task_status.as_str(), "claimed" | "in_progress") {
                return Ok(TickOutcome::NeedsOperator);
            }
            effects.persist_episode(Episode {
                key: current_key,
                state: EpisodeState::Detected,
                consecutive_ticks: 1,
                candidate: None,
                unlock_at: input.unlock_at.map(|deadline| deadline.to_rfc3339()),
                recipient: input.recipient,
            })?;
            Ok(TickOutcome::Detected)
        }
    }
}

struct FsEffects<'a> {
    home: &'a Path,
    source: &'a str,
    path: PathBuf,
    episode: Option<Episode>,
}

pub(crate) fn acquire_binding_lock(
    home: &Path,
    source: &str,
) -> anyhow::Result<crate::store::FileFlockGuard> {
    crate::store::acquire_file_lock(
        &crate::paths::runtime_dir(home)
            .join(source)
            .join(".binding.json.lock"),
    )
}

pub(crate) fn read_current_binding(home: &Path, source: &str) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(
        crate::paths::runtime_dir(home)
            .join(source)
            .join("binding.json"),
    )
    .ok()?;
    crate::binding::parse_binding_guarded(&content)
}

impl<'a> FsEffects<'a> {
    fn load(home: &'a Path, source: &'a str) -> anyhow::Result<Self> {
        let path = crate::paths::runtime_dir(home)
            .join(source)
            .join("usage_limit_episode.json");
        let episode = match std::fs::read_to_string(&path) {
            Ok(content) => Some(serde_json::from_str(&content)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            home,
            source,
            path,
            episode,
        })
    }

    fn current_binding_matches(&self, key: &EpisodeKey) -> bool {
        read_current_binding(self.home, self.source).is_some_and(|binding| {
            binding.get("task_id").and_then(|v| v.as_str()) == Some(key.task_id.as_str())
                && binding.get("branch").and_then(|v| v.as_str()) == Some(key.branch.as_str())
                && binding.get("issued_at").and_then(|v| v.as_str())
                    == Some(key.binding_issued_at.as_str())
        })
    }
}

impl ControlEffects for FsEffects<'_> {
    fn load_episode(&self) -> Option<Episode> {
        self.episode.clone()
    }

    fn persist_episode(&mut self, episode: Episode) -> anyhow::Result<()> {
        let body = serde_json::to_vec_pretty(&episode)?;
        crate::store::atomic_write(&self.path, &body)?;
        self.episode = Some(episode);
        Ok(())
    }

    fn block_task(&mut self, key: &EpisodeKey) -> anyhow::Result<()> {
        if !self.current_binding_matches(key) {
            anyhow::bail!("binding generation changed before UsageLimit block");
        }
        // #2760 item 4: the strict route, per-id lock, write-time revalidation and
        // the board append all happen INSIDE the narrow `tasks` op — the supervisor
        // no longer touches a raw board path or a generic append. It supplies only
        // the identity guard + the fully-built block-reason payload (which embeds
        // the episode id, so the op's idempotency check can recognise it).
        let notification_id = key.notification_id();
        let episode = self.episode.as_ref().cloned();
        let reason = serde_json::json!({
            "type": "usage_limit_episode",
            "episode_id": notification_id,
            "source": key.source,
            "binding_issued_at": key.binding_issued_at,
            "branch": key.branch,
            "state": episode.as_ref().map(|e| e.state),
            "unlock_at": episode.as_ref().and_then(|e| e.unlock_at.clone()),
            "proposal": {
                "candidate": episode.as_ref().and_then(|e| e.candidate.clone()),
                "executable": false,
                "requires": "operator_takeover_slice_2"
            }
        })
        .to_string();
        let guard = crate::tasks::UsageLimitGuard {
            task_id: key.task_id.clone(),
            source: key.source.clone(),
            branch: key.branch.clone(),
            episode_id: notification_id,
        };
        match crate::tasks::apply_usage_limit_block(self.home, &guard, reason)? {
            crate::tasks::ApplyOutcome::Applied | crate::tasks::ApplyOutcome::AlreadyApplied => {
                Ok(())
            }
            // Route/generation changed → no event; surface as an error so the
            // control loop retries (matches the pre-#2760 generation-change bail).
            crate::tasks::ApplyOutcome::Stale => {
                anyhow::bail!("task generation changed before UsageLimit block")
            }
        }
    }

    fn recover_task_atomically(&mut self, key: &EpisodeKey) -> anyhow::Result<bool> {
        if !self.current_binding_matches(key) {
            return Ok(false);
        }
        // #2760 item 4: strict route + per-id lock + revalidation + the atomic
        // Unblocked+InProgress append all happen INSIDE the narrow `tasks` op. It
        // folds the crash-window idempotency (already InProgress for this
        // owner+branch → AlreadyApplied) and the commit-time generation guard. A
        // route/generation change → Stale → not recovered this pass (no events),
        // and the control loop re-attempts.
        let guard = crate::tasks::UsageLimitGuard {
            task_id: key.task_id.clone(),
            source: key.source.clone(),
            branch: key.branch.clone(),
            episode_id: key.notification_id(),
        };
        match crate::tasks::recover_usage_limit_block(self.home, &guard)? {
            crate::tasks::ApplyOutcome::Applied | crate::tasks::ApplyOutcome::AlreadyApplied => {
                Ok(true)
            }
            crate::tasks::ApplyOutcome::Stale => Ok(false),
        }
    }

    fn notify_once(
        &mut self,
        key: &EpisodeKey,
        recipient: &str,
        executable: bool,
    ) -> anyhow::Result<()> {
        let id = key.notification_id();
        if crate::inbox::find_message(self.home, &id).is_some() {
            return Ok(());
        }
        let episode = self
            .episode
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("episode must be persisted before notification"))?;
        let payload = serde_json::json!({
            "type": "usage_limit_failover_proposal",
            "episode_id": id,
            "task_id": key.task_id,
            "source": key.source,
            "state": episode.state,
            "unlock_at": episode.unlock_at,
            "proposal": {
                "candidate": episode.candidate,
                "executable": executable,
                "requires": "operator_takeover_slice_2"
            }
        });
        let mut message = crate::inbox::InboxMessage::new_system(
            "system:usage-limit",
            "usage_limit_failover",
            payload.to_string(),
        );
        message.id = Some(key.notification_id());
        message.task_id = Some(key.task_id.clone());
        message.correlation_id = Some(key.notification_id());
        crate::inbox::enqueue(self.home, recipient, message)
    }

    fn reconcile_active(&mut self, key: &EpisodeKey, recipient: &str) -> anyhow::Result<()> {
        self.block_task(key)?;
        self.notify_once(key, recipient, false)
    }
}

fn correlation_from_disk(home: &Path, source: &str) -> Option<(Correlation, crate::tasks::Task)> {
    let binding = read_current_binding(home, source)?;
    let task_id = binding.get("task_id")?.as_str()?.to_string();
    // #2760: resolve via the STRICT route (any route error → None: no correlation
    // is built on a task whose board cannot be uniquely proven). The resolved Task
    // is RETURNED so `episode_start` reuses this SINGLE route (no double load).
    let task = crate::tasks::load_routed(home, &task_id).ok()?.task;
    let task_status = match task.status {
        crate::task_events::TaskStatus::Claimed => "claimed",
        crate::task_events::TaskStatus::InProgress => "in_progress",
        crate::task_events::TaskStatus::Blocked => "blocked",
        _ => "other",
    }
    .to_string();
    let correlation = Correlation {
        task_id,
        source: source.to_string(),
        owner: task.assignee.clone().unwrap_or_default(),
        task_status,
        task_branch: task.branch.clone().unwrap_or_default(),
        binding_task_id: binding.get("task_id")?.as_str()?.to_string(),
        binding_branch: binding.get("branch")?.as_str()?.to_string(),
        binding_issued_at: binding.get("issued_at")?.as_str()?.to_string(),
    };
    Some((correlation, task))
}

pub(crate) fn fleet_facts(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    source: &str,
    task: Option<&crate::tasks::Task>,
) -> (Option<String>, Option<String>, Vec<CandidateFacts>, String) {
    let Ok(fleet) = crate::teams::try_load_fleet(home) else {
        return (None, None, Vec::new(), "general".into());
    };
    let Some(team) = crate::teams::find_team_for_in(&fleet, source) else {
        return (None, None, Vec::new(), "general".into());
    };
    let source_resolved = fleet.resolve_instance(source);
    let source_role = source_resolved
        .as_ref()
        .and_then(|config| config.role.clone());
    let recipient = notification_recipient(source, Ok(team.orchestrator.as_deref()));
    let live_handles = {
        let registry = registry.lock();
        registry
            .values()
            .map(|handle| {
                let state = AgentState::from_u8(
                    handle
                        .published_state
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
                (
                    handle.name.to_string(),
                    (
                        crate::backend::Backend::from_command(&handle.backend_command)
                            .map(|backend| backend.as_str().to_string())
                            .unwrap_or_else(|| handle.backend_command.clone()),
                        state,
                        !handle.deleted.load(std::sync::atomic::Ordering::Acquire),
                        std::sync::Arc::clone(&handle.core),
                    ),
                )
            })
            .collect::<Vec<_>>()
    };
    let live = live_handles
        .into_iter()
        .map(|(name, (backend, state, is_live, core))| {
            let healthy = core.lock().health.state == crate::health::HealthState::Healthy;
            (name, (backend, state, is_live, healthy))
        })
        .collect::<HashMap<_, _>>();
    let active_owners = crate::tasks::list_all_boards(home)
        .into_iter()
        .flat_map(|(_, tasks)| tasks)
        .filter(|task| {
            matches!(
                task.status,
                crate::task_events::TaskStatus::Claimed
                    | crate::task_events::TaskStatus::InProgress
            )
        })
        .filter_map(|task| task.assignee)
        .collect::<HashSet<_>>();
    let required_backend = task.and_then(|task| {
        task.metadata
            .get("backend_lock")
            .or_else(|| task.metadata.get("backend"))
            .and_then(|value| value.as_str())
    });
    let required_model = task.and_then(|task| {
        task.metadata
            .get("model")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| {
                task.metadata
                    .get("model_tier")
                    .and_then(|value| value.as_str())
                    .and_then(|tier| fleet.model_tiers.get(tier).cloned())
            })
    });
    let candidates = team
        .members
        .iter()
        .filter(|name| name.as_str() != source)
        .filter_map(|name| {
            let resolved = fleet.resolve_instance(name)?;
            let (backend, state, is_live, is_healthy) = live.get(name)?.clone();
            let routing_compatible = required_backend.is_none_or(|required| required == backend)
                && required_model
                    .as_deref()
                    .is_none_or(|required| resolved.model.as_deref() == Some(required));
            Some(CandidateFacts {
                name: name.clone(),
                team: Some(team.name.clone()),
                role: resolved.role,
                backend,
                live: is_live,
                healthy: is_healthy && !state.is_error(),
                idle: state == AgentState::Idle,
                bound: crate::binding::read(home, name).is_some(),
                has_active_task: active_owners.contains(name),
                current_usage_limit: state == AgentState::UsageLimit,
                routing_compatible,
            })
        })
        .collect();
    (Some(team.name), source_role, candidates, recipient)
}

/// Post-core-lock supervisor seam. Detection and recovery both consume the
/// same authoritative raw `AgentState`; observed/operated state is never read.
pub(crate) fn observe_supervisor_tick(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    source: &str,
    raw_state: AgentState,
    backend_command: &str,
    pane_tail: &str,
) -> anyhow::Result<TickOutcome> {
    let mut effects = FsEffects::load(home, source)?;
    if raw_state != AgentState::UsageLimit {
        let Some(episode) = effects.episode.as_ref() else {
            return Ok(TickOutcome::NoEpisode);
        };
        if episode.state == EpisodeState::Recovered {
            return Ok(TickOutcome::NoEpisode);
        }
        if episode.state == EpisodeState::Detected {
            return observe_tick(
                &mut effects,
                TickInput {
                    now: Utc::now(),
                    raw_state,
                    source_backend: String::new(),
                    source_team: None,
                    source_role: None,
                    unlock_at: None,
                    correlation: None,
                    candidates: Vec::new(),
                    recipient: String::new(),
                },
            );
        }
        if !matches!(raw_state, AgentState::Idle | AgentState::Active) {
            return Ok(TickOutcome::AlreadyActive);
        }
        let _binding_guard = acquire_binding_lock(home, source)?;
        return observe_tick(
            &mut effects,
            TickInput {
                now: Utc::now(),
                raw_state,
                source_backend: String::new(),
                source_team: None,
                source_role: None,
                unlock_at: None,
                correlation: None,
                candidates: Vec::new(),
                recipient: String::new(),
            },
        );
    }

    let _binding_guard = acquire_binding_lock(home, source)?;
    // #2760: `correlation_from_disk` resolves the task ONCE and returns it, so the
    // fleet-facts lookup reuses that snapshot instead of a second `load_by_id`.
    let (correlation, task) = match correlation_from_disk(home, source) {
        Some((correlation, task)) => (Some(correlation), Some(task)),
        None => (None, None),
    };
    let (source_team, source_role, candidates, recipient) =
        fleet_facts(home, registry, source, task.as_ref());
    let now = Utc::now();
    let unlock_at = super::parse_unlock_at(pane_tail)
        .as_deref()
        .and_then(|hhmm| super::unlock_deadline(hhmm, now));
    let source_backend = crate::backend::Backend::from_command(backend_command)
        .map(|backend| backend.as_str().to_string())
        .unwrap_or_else(|| backend_command.to_string());
    observe_tick(
        &mut effects,
        TickInput {
            now,
            raw_state,
            source_backend,
            source_team,
            source_role,
            unlock_at,
            correlation,
            candidates,
            recipient,
        },
    )
}

#[cfg(test)]
pub(crate) fn supervisor_hook_uses_raw_current(source: &str) -> bool {
    source.contains("let agent_state = core.state.current;")
        && source.contains("usage_limit_raw_state = agent_state;")
        && source.contains("usage_limit_control::observe_supervisor_tick(")
        && source.contains("usage_limit_raw_state,")
}

#[cfg(test)]
pub(crate) fn mutation_guard_violations(source: &str) -> Vec<String> {
    use syn::visit::Visit;

    const BANNED: &[&str] = &[
        "crate::binding::bind",
        "crate::binding::bind_full",
        "crate::binding::release",
        "crate::binding::release_full",
        "crate::worktree_pool::release_full",
        "crate::agent_ops::create_instance",
        "crate::agent_ops::delete_instance",
        "crate::agent_ops::restart_instance",
        "crate::mcp::handlers::instance_state::restart_instance_autonomic",
        "crate::task_events::TaskEvent::OwnerAssigned",
        "crate::task_events::TaskEvent::Linked",
        "crate::task_events::TaskEvent::BranchLinked",
        "crate::tasks::link_branch_to_task",
        "crate::dispatch_tracking::track_dispatch",
    ];

    #[derive(Default)]
    struct Guard {
        aliases: HashMap<String, String>,
        violations: HashSet<String>,
    }

    fn flatten_use(prefix: String, tree: &syn::UseTree, aliases: &mut HashMap<String, String>) {
        match tree {
            syn::UseTree::Path(path) => {
                let next = if prefix.is_empty() {
                    path.ident.to_string()
                } else {
                    format!("{prefix}::{}", path.ident)
                };
                flatten_use(next, &path.tree, aliases);
            }
            syn::UseTree::Name(name) => {
                let canonical = if prefix.is_empty() {
                    name.ident.to_string()
                } else {
                    format!("{prefix}::{}", name.ident)
                };
                aliases.insert(name.ident.to_string(), canonical);
            }
            syn::UseTree::Rename(rename) => {
                let canonical = if prefix.is_empty() {
                    rename.ident.to_string()
                } else {
                    format!("{prefix}::{}", rename.ident)
                };
                aliases.insert(rename.rename.to_string(), canonical);
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    flatten_use(prefix.clone(), item, aliases);
                }
            }
            syn::UseTree::Glob(_) => {}
        }
    }

    impl<'ast> Visit<'ast> for Guard {
        fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
            flatten_use(String::new(), &node.tree, &mut self.aliases);
            syn::visit::visit_item_use(self, node);
        }

        fn visit_path(&mut self, node: &'ast syn::Path) {
            let mut segments = node.segments.iter();
            if let Some(first) = segments.next() {
                let suffix = segments
                    .map(|segment| segment.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                let base = self
                    .aliases
                    .get(&first.ident.to_string())
                    .cloned()
                    .unwrap_or_else(|| first.ident.to_string());
                let canonical = if suffix.is_empty() {
                    base
                } else {
                    format!("{base}::{suffix}")
                };
                if BANNED.contains(&canonical.as_str()) {
                    self.violations.insert(canonical);
                }
            }
            syn::visit::visit_path(self, node);
        }
    }

    let Ok(file) = syn::parse_file(source) else {
        return vec!["<parse-error>".to_string()];
    };
    let mut guard = Guard::default();
    guard.visit_file(&file);
    let mut violations = guard.violations.into_iter().collect::<Vec<_>>();
    violations.sort();
    violations
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum Effect {
    Persist(EpisodeState),
    BlockTask(EpisodeKey),
    RecoverTaskAtomically(EpisodeKey),
    NotifyOnce { recipient: String, executable: bool },
}

#[cfg(test)]
impl Effect {
    fn is_takeover_mutation(&self) -> bool {
        false
    }
}

#[cfg(test)]
struct MemoryEffects {
    episode: Option<Episode>,
    effects: Vec<Effect>,
    notification_ids: HashSet<String>,
    generation_matches_at_atomic_append: bool,
    registry_len: usize,
    /// Mirrors the board state FsEffects::block_task reads: `true` ⟺ the task
    /// is already Blocked with THIS episode's reason, so a re-block is an
    /// idempotent no-op (the FsEffects early-return). #2759 r1: without this
    /// mirror the default no-op `reconcile_active` hid every reconcile side
    /// effect from the memory harness — the exact blind spot that let the
    /// same-key one-tick re-block ship green.
    task_blocked_with_episode_reason: bool,
}

#[cfg(test)]
impl Default for MemoryEffects {
    fn default() -> Self {
        Self {
            episode: None,
            effects: Vec::new(),
            notification_ids: HashSet::new(),
            generation_matches_at_atomic_append: true,
            registry_len: 7,
            task_blocked_with_episode_reason: false,
        }
    }
}

#[cfg(test)]
impl ControlEffects for MemoryEffects {
    fn load_episode(&self) -> Option<Episode> {
        self.episode.clone()
    }

    fn persist_episode(&mut self, episode: Episode) -> anyhow::Result<()> {
        self.effects.push(Effect::Persist(episode.state));
        self.episode = Some(episode);
        Ok(())
    }

    fn block_task(&mut self, key: &EpisodeKey) -> anyhow::Result<()> {
        if self.task_blocked_with_episode_reason {
            return Ok(());
        }
        self.effects.push(Effect::BlockTask(key.clone()));
        self.task_blocked_with_episode_reason = true;
        Ok(())
    }

    fn recover_task_atomically(&mut self, key: &EpisodeKey) -> anyhow::Result<bool> {
        if !self.generation_matches_at_atomic_append {
            return Ok(false);
        }
        self.effects
            .push(Effect::RecoverTaskAtomically(key.clone()));
        Ok(true)
    }

    fn notify_once(
        &mut self,
        key: &EpisodeKey,
        recipient: &str,
        executable: bool,
    ) -> anyhow::Result<()> {
        if self.notification_ids.insert(key.notification_id()) {
            self.effects.push(Effect::NotifyOnce {
                recipient: recipient.to_string(),
                executable,
            });
        }
        Ok(())
    }

    // #2759 r1 (M1): mirror FsEffects::reconcile_active instead of inheriting
    // the trait's no-op default, so active-episode ticks surface their
    // block/notify side effects to memory-level assertions.
    fn reconcile_active(&mut self, key: &EpisodeKey, recipient: &str) -> anyhow::Result<()> {
        self.block_task(key)?;
        self.notify_once(key, recipient, false)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        choose_candidate, mutation_guard_violations, notification_recipient, observe_tick,
        supervisor_hook_uses_raw_current, CandidateFacts, Correlation, Effect, Episode, EpisodeKey,
        EpisodeState, MemoryEffects, TickInput, TickOutcome,
    };
    use crate::state::AgentState;
    use chrono::{DateTime, Duration, Utc};

    fn tmp_home(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agend-usage-limit-slice1-{label}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).expect("create temp home");
        path
    }

    fn now() -> DateTime<Utc> {
        "2026-07-12T14:00:00Z".parse().expect("valid fixture time")
    }

    fn correlation() -> Correlation {
        Correlation {
            task_id: "t-slice-1".into(),
            source: "worker-a".into(),
            owner: "worker-a".into(),
            task_status: "in_progress".into(),
            task_branch: "feat/slice-1".into(),
            binding_task_id: "t-slice-1".into(),
            binding_branch: "feat/slice-1".into(),
            binding_issued_at: "2026-07-12T13:00:00Z".into(),
        }
    }

    fn key() -> EpisodeKey {
        EpisodeKey::try_from(&correlation()).expect("fixture must correlate exactly")
    }

    fn candidate(name: &str, backend: &str) -> CandidateFacts {
        CandidateFacts {
            name: name.into(),
            team: Some("archfix".into()),
            role: Some("dev".into()),
            backend: backend.into(),
            live: true,
            healthy: true,
            idle: true,
            bound: false,
            has_active_task: false,
            current_usage_limit: false,
            routing_compatible: true,
        }
    }

    fn input(raw_state: AgentState) -> TickInput {
        TickInput {
            now: now(),
            raw_state,
            source_backend: "codex".into(),
            source_team: Some("archfix".into()),
            source_role: Some("dev".into()),
            unlock_at: Some(now() + Duration::minutes(30)),
            correlation: Some(correlation()),
            candidates: vec![candidate("worker-b", "claude")],
            recipient: "lead".into(),
        }
    }

    #[test]
    fn two_consecutive_raw_usage_limit_ticks_persist_before_block_and_notify() {
        let mut fx = MemoryEffects::default();

        let first = observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("tick one");
        assert_eq!(first, TickOutcome::Detected);
        assert_eq!(fx.effects, vec![Effect::Persist(EpisodeState::Detected)]);

        fx.effects.clear();
        let before_registry = fx.registry_len;
        let second = observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("tick two");
        assert_eq!(second, TickOutcome::CandidateReady("worker-b".into()));
        assert_eq!(
            fx.effects,
            vec![
                Effect::Persist(EpisodeState::CandidateReady),
                Effect::BlockTask(key()),
                Effect::NotifyOnce {
                    recipient: "lead".into(),
                    executable: false,
                },
            ],
            "the durable episode must precede the only allowed board mutation and notice"
        );
        assert_eq!(
            fx.registry_len, before_registry,
            "Slice 1 cannot add a seat"
        );
        assert!(!fx.effects.iter().any(Effect::is_takeover_mutation));
    }

    #[test]
    fn intervening_raw_healthy_tick_resets_the_stability_counter() {
        let mut fx = MemoryEffects::default();
        observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("first detection");
        observe_tick(&mut fx, input(AgentState::Idle)).expect("healthy reset");
        fx.effects.clear();

        let outcome =
            observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("new episode tick");
        assert_eq!(outcome, TickOutcome::Detected);
        assert_eq!(fx.effects, vec![Effect::Persist(EpisodeState::Detected)]);
        assert!(!fx.effects.iter().any(|e| matches!(e, Effect::BlockTask(_))));
    }

    #[test]
    fn exact_correlation_is_required_before_blocking() {
        for break_field in ["owner", "task_id", "task_branch", "binding_branch"] {
            let mut fx = MemoryEffects::default();
            let mut bad = correlation();
            match break_field {
                "owner" => bad.owner = "someone-else".into(),
                "task_id" => bad.binding_task_id = "t-other".into(),
                "task_branch" => bad.task_branch = "feat/other".into(),
                "binding_branch" => bad.binding_branch = "feat/other".into(),
                _ => unreachable!(),
            }
            let mut tick = input(AgentState::UsageLimit);
            tick.correlation = Some(bad);
            observe_tick(&mut fx, tick.clone()).expect("first tick");
            let outcome = observe_tick(&mut fx, tick).expect("second tick");
            assert_eq!(outcome, TickOutcome::NeedsOperator, "field={break_field}");
            assert!(!fx.effects.iter().any(|e| matches!(e, Effect::BlockTask(_))));
        }
    }

    #[test]
    fn unlock_boundary_is_exact_and_null_needs_operator() {
        for (delta, expected) in [
            (Duration::seconds(29 * 60 + 59), EpisodeState::AwaitReset),
            (Duration::minutes(30), EpisodeState::CandidateReady),
        ] {
            let mut fx = MemoryEffects::default();
            let mut tick = input(AgentState::UsageLimit);
            tick.unlock_at = Some(now() + delta);
            observe_tick(&mut fx, tick.clone()).expect("first tick");
            observe_tick(&mut fx, tick).expect("second tick");
            assert!(fx.effects.contains(&Effect::Persist(expected)));
        }

        let mut fx = MemoryEffects::default();
        let mut tick = input(AgentState::UsageLimit);
        tick.unlock_at = None;
        observe_tick(&mut fx, tick.clone()).expect("first tick");
        assert_eq!(
            observe_tick(&mut fx, tick).expect("second tick"),
            TickOutcome::NeedsOperator
        );
    }

    #[test]
    fn restart_replay_deduplicates_block_and_notification() {
        // Restart precondition: the pre-restart activation already Blocked the
        // task with this episode's reason and enqueued the notification —
        // mirror BOTH durable states so the honest (#2759 r1 M1) reconcile
        // path no-ops exactly like FsEffects would against the real board.
        let mut fx = MemoryEffects {
            episode: Some(Episode::activated(
                key(),
                EpisodeState::CandidateReady,
                Some("worker-b".into()),
            )),
            task_blocked_with_episode_reason: true,
            ..Default::default()
        };
        fx.notification_ids.insert(key().notification_id());

        let outcome = observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("restart replay");
        assert_eq!(outcome, TickOutcome::AlreadyActive);
        assert!(
            fx.effects.is_empty(),
            "restart must not duplicate durable effects"
        );
    }

    /// #2759 r1: memory-level pin of the same-key recurrence guard. With the
    /// honest reconcile mirror (M1), an unguarded arm-1 would surface
    /// `BlockTask` here on ONE tick — the fixed guard must instead start a
    /// fresh Detected cycle with no board mutation and no notification.
    #[test]
    fn single_ul_tick_after_recovered_same_key_starts_fresh_cycle() {
        let mut fx = MemoryEffects {
            episode: Some(Episode {
                key: key(),
                state: EpisodeState::Recovered,
                consecutive_ticks: 2,
                candidate: None,
                unlock_at: None,
                recipient: "lead".into(),
            }),
            ..Default::default()
        };
        let outcome = observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("single tick");
        assert_eq!(outcome, TickOutcome::Detected);
        assert_eq!(
            fx.effects,
            vec![Effect::Persist(EpisodeState::Detected)],
            "one tick after Recovered: fresh cycle only — no re-block, no notify"
        );
    }

    /// RED: completing one same-key episode and then activating a second one
    /// currently emits only one notification because both episodes share the
    /// key-derived notification id. Each completed episode must produce its
    /// own actionable candidate recommendation.
    #[test]
    fn same_key_recurrence_after_recovery_emits_fresh_notification_red() {
        let mut fx = MemoryEffects::default();

        observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("first episode tick one");
        observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("first episode activation");
        observe_tick(&mut fx, input(AgentState::Idle)).expect("first episode recovery");

        observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("second episode tick one");
        observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("second episode activation");

        let notifications = fx
            .effects
            .iter()
            .filter(|effect| matches!(effect, Effect::NotifyOnce { .. }))
            .count();
        assert_eq!(
            notifications, 2,
            "each completed same-key episode must emit a distinct notification"
        );
    }

    #[test]
    fn restart_starting_state_does_not_false_recover_an_active_episode() {
        let mut fx = MemoryEffects {
            episode: Some(Episode::activated(key(), EpisodeState::AwaitReset, None)),
            ..Default::default()
        };

        let outcome = observe_tick(&mut fx, input(AgentState::Starting)).expect("restart tick");
        assert_eq!(outcome, TickOutcome::AlreadyActive);
        assert!(
            fx.effects.is_empty(),
            "Starting is not proof of quota recovery"
        );
    }

    #[test]
    fn recovery_is_generation_matched_and_atomic() {
        let mut fx = MemoryEffects {
            episode: Some(Episode::activated(key(), EpisodeState::AwaitReset, None)),
            ..Default::default()
        };

        let outcome = observe_tick(&mut fx, input(AgentState::Idle)).expect("recover");
        assert_eq!(outcome, TickOutcome::Recovered);
        assert_eq!(
            fx.effects,
            vec![
                Effect::RecoverTaskAtomically(key()),
                Effect::Persist(EpisodeState::Recovered),
            ]
        );

        let mut raced = MemoryEffects {
            episode: Some(Episode::activated(key(), EpisodeState::AwaitReset, None)),
            generation_matches_at_atomic_append: false,
            ..Default::default()
        };
        let outcome = observe_tick(&mut raced, input(AgentState::Idle)).expect("raced recover");
        assert_eq!(outcome, TickOutcome::NeedsOperator);
        assert_eq!(
            raced.effects,
            vec![Effect::Persist(EpisodeState::NeedsOperator)]
        );
    }

    #[test]
    fn candidate_matrix_is_deterministic_and_source_backend_scoped() {
        let source = input(AgentState::UsageLimit);
        let mut invalid = Vec::new();

        let mut same_backend = candidate("same-backend", "codex");
        same_backend.healthy = true;
        invalid.push(same_backend);
        let mut limited = candidate("limited", "claude");
        limited.current_usage_limit = true;
        invalid.push(limited);
        let mut unhealthy = candidate("unhealthy", "claude");
        unhealthy.healthy = false;
        invalid.push(unhealthy);
        let mut busy = candidate("busy", "claude");
        busy.idle = false;
        invalid.push(busy);
        let mut bound = candidate("bound", "claude");
        bound.bound = true;
        invalid.push(bound);
        let mut tasked = candidate("tasked", "claude");
        tasked.has_active_task = true;
        invalid.push(tasked);
        let mut dead = candidate("dead", "claude");
        dead.live = false;
        invalid.push(dead);
        let mut cross_team = candidate("cross-team", "claude");
        cross_team.team = Some("other".into());
        invalid.push(cross_team);
        let mut wrong_role = candidate("wrong-role", "claude");
        wrong_role.role = Some("reviewer".into());
        invalid.push(wrong_role);
        let mut route_locked = candidate("route-locked", "claude");
        route_locked.routing_compatible = false;
        invalid.push(route_locked);

        invalid.push(candidate("first-valid", "claude"));
        invalid.push(candidate("second-valid", "claude"));
        assert_eq!(
            choose_candidate(&source, &invalid),
            Some("first-valid".into())
        );
    }

    #[test]
    fn notification_falls_back_for_self_orchestrator_missing_or_unreadable_team() {
        assert_eq!(notification_recipient("worker", Ok(Some("lead"))), "lead");
        assert_eq!(notification_recipient("lead", Ok(Some("lead"))), "general");
        assert_eq!(notification_recipient("worker", Ok(None)), "general");
        assert_eq!(notification_recipient("worker", Err(())), "general");
    }

    #[test]
    fn supervisor_hook_consumes_raw_current_not_operated_or_observed_state() {
        let supervisor = include_str!("../supervisor.rs");
        assert!(
            supervisor_hook_uses_raw_current(supervisor),
            "Slice 1 detection and recovery must receive the raw `core.state.current` captured \
             under the core lock, never `operated_state` or `observed_status`"
        );
    }

    #[test]
    fn structural_mutation_guard_is_alias_proof_and_production_is_clean() {
        let production = include_str!("usage_limit_control.rs");
        assert_eq!(mutation_guard_violations(production), Vec::<String>::new());

        let disguised = r#"
            use crate::binding::release_full as harmless_cleanup;
            fn forbidden(home: &std::path::Path) {
                harmless_cleanup(home, "worker-a", false);
            }
        "#;
        assert_eq!(
            mutation_guard_violations(disguised),
            vec!["crate::binding::release_full".to_string()],
            "guard must resolve use-tree aliases instead of grepping bare call names"
        );
    }

    #[test]
    fn filesystem_episode_blocks_without_releasing_and_recovers_exact_generation() {
        let home = tmp_home("filesystem");
        std::fs::write(
            home.join("fleet.yaml"),
            r#"
instances:
  worker-a: { backend: codex, role: dev }
  lead: { backend: claude, role: orchestrator }
teams:
  archfix:
    members: [worker-a, lead]
    orchestrator: lead
"#,
        )
        .expect("write fleet");
        let runtime = crate::paths::runtime_dir(&home).join("worker-a");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        let binding = serde_json::json!({
            "version": 1,
            "agent": "worker-a",
            "task_id": "t-slice-1",
            "branch": "feat/slice-1",
            "issued_at": "2026-07-12T13:00:00Z",
            "worktree": home.join("worktrees/worker-a/feat/slice-1"),
            "source_repo": home.join("repo")
        });
        let binding_bytes = serde_json::to_vec_pretty(&binding).expect("binding json");
        std::fs::write(runtime.join("binding.json"), &binding_bytes).expect("binding write");

        let task_id = crate::task_events::TaskId("t-slice-1".into());
        let owner = crate::task_events::InstanceName("worker-a".into());
        crate::task_events::append_batch(
            &home,
            &owner,
            vec![
                crate::task_events::TaskEvent::Created {
                    task_id: task_id.clone(),
                    title: "slice 1".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: Some(owner.clone()),
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: Some("feat/slice-1".into()),
                    bind: Some(true),
                    eta_secs: None,
                    tags: Vec::new(),
                    parent_id: None,
                },
                crate::task_events::TaskEvent::Claimed {
                    task_id: task_id.clone(),
                    by: owner.clone(),
                },
                crate::task_events::TaskEvent::InProgress {
                    task_id: task_id.clone(),
                    by: owner.clone(),
                },
            ],
        )
        .expect("seed task");

        let _binding_guard = super::acquire_binding_lock(&home, "worker-a").expect("binding lock");
        let make_input = |state| TickInput {
            now: now(),
            raw_state: state,
            source_backend: "codex".into(),
            source_team: Some("archfix".into()),
            source_role: Some("dev".into()),
            unlock_at: Some(now() + Duration::seconds(29 * 60 + 59)),
            correlation: super::correlation_from_disk(&home, "worker-a").map(|(c, _)| c),
            candidates: Vec::new(),
            recipient: "lead".into(),
        };

        let mut effects = super::FsEffects::load(&home, "worker-a").expect("effects");
        assert_eq!(
            super::observe_tick(&mut effects, make_input(AgentState::UsageLimit))
                .expect("first tick"),
            TickOutcome::Detected
        );
        assert_eq!(
            super::observe_tick(&mut effects, make_input(AgentState::UsageLimit))
                .expect("second tick"),
            TickOutcome::AwaitReset
        );

        let blocked = crate::task_events::replay(&home).expect("replay blocked");
        let blocked = blocked.tasks.get(&task_id).expect("task remains");
        assert_eq!(blocked.status, crate::task_events::TaskStatus::Blocked);
        assert_eq!(blocked.owner.as_ref(), Some(&owner));
        assert_eq!(
            std::fs::read(runtime.join("binding.json")).expect("binding after block"),
            binding_bytes,
            "block must keep binding/worktree/path byte-identical"
        );
        let episode_id = key().notification_id();
        assert!(crate::inbox::find_message(&home, &episode_id).is_some());
        let event_count = std::fs::read_to_string(home.join("task_events.jsonl"))
            .expect("event log")
            .lines()
            .count();

        let mut restarted = super::FsEffects::load(&home, "worker-a").expect("restart load");
        assert_eq!(
            super::observe_tick(&mut restarted, make_input(AgentState::UsageLimit))
                .expect("restart tick"),
            TickOutcome::AlreadyActive
        );
        assert_eq!(
            std::fs::read_to_string(home.join("task_events.jsonl"))
                .expect("event log after restart")
                .lines()
                .count(),
            event_count,
            "restart cannot append a duplicate Blocked event"
        );

        let mut rebound = binding.clone();
        rebound["issued_at"] = serde_json::json!("2026-07-12T14:30:00Z");
        std::fs::write(
            runtime.join("binding.json"),
            serde_json::to_vec_pretty(&rebound).expect("rebound json"),
        )
        .expect("simulate concurrent rebind");
        let mut raced = super::FsEffects::load(&home, "worker-a").expect("raced load");
        assert_eq!(
            super::observe_tick(&mut raced, make_input(AgentState::Idle))
                .expect("generation-mismatched recovery"),
            TickOutcome::NeedsOperator
        );
        assert_eq!(
            crate::task_events::replay(&home)
                .expect("replay raced")
                .tasks
                .get(&task_id)
                .expect("raced task")
                .status,
            crate::task_events::TaskStatus::Blocked,
            "a changed binding generation cannot be clobbered by recovery"
        );
        std::fs::write(runtime.join("binding.json"), &binding_bytes)
            .expect("restore original generation");

        let mut recovering = super::FsEffects::load(&home, "worker-a").expect("recovery load");
        assert_eq!(
            super::observe_tick(&mut recovering, make_input(AgentState::Idle))
                .expect("recovery tick"),
            TickOutcome::Recovered
        );
        let recovered = crate::task_events::replay(&home).expect("replay recovered");
        let recovered = recovered.tasks.get(&task_id).expect("task recovered");
        assert_eq!(recovered.status, crate::task_events::TaskStatus::InProgress);
        assert_eq!(recovered.owner.as_ref(), Some(&owner));
        assert_eq!(
            std::fs::read(runtime.join("binding.json")).expect("binding after recover"),
            binding_bytes
        );
        std::fs::remove_dir_all(home).ok();
    }
    /// #2759 r1 RED — same-key UsageLimit recurrence after a COMPLETED
    /// (Recovered) episode. A Recovered episode is terminal history, not an
    /// active one: a NEW raw-UsageLimit observation on the SAME task/binding
    /// generation must start a fresh two-tick cycle (no one-tick re-block),
    /// and after the limit lifts the task must return to InProgress. Pre-fix,
    /// the `key == current_key` arm shadowed the Recovered arm: ONE tick
    /// re-Blocked via `reconcile_active`, then both recovery paths skipped
    /// Recovered episodes — the task wedged Blocked forever, silently.
    #[test]
    fn same_key_recurrence_after_recovered_needs_two_ticks_and_reheals() {
        let home = tmp_home("same-key-recurrence");
        std::fs::write(
            home.join("fleet.yaml"),
            r#"
instances:
  worker-a: { backend: codex, role: dev }
  lead: { backend: claude, role: orchestrator }
teams:
  archfix:
    members: [worker-a, lead]
    orchestrator: lead
"#,
        )
        .expect("write fleet");
        let runtime = crate::paths::runtime_dir(&home).join("worker-a");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        let binding = serde_json::json!({
            "version": 1,
            "agent": "worker-a",
            "task_id": "t-slice-1",
            "branch": "feat/slice-1",
            "issued_at": "2026-07-12T13:00:00Z",
            "worktree": home.join("worktrees/worker-a/feat/slice-1"),
            "source_repo": home.join("repo")
        });
        let binding_bytes = serde_json::to_vec_pretty(&binding).expect("binding json");
        std::fs::write(runtime.join("binding.json"), &binding_bytes).expect("binding write");

        let task_id = crate::task_events::TaskId("t-slice-1".into());
        let owner = crate::task_events::InstanceName("worker-a".into());
        crate::task_events::append_batch(
            &home,
            &owner,
            vec![
                crate::task_events::TaskEvent::Created {
                    task_id: task_id.clone(),
                    title: "slice 1".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: Some(owner.clone()),
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: Some("feat/slice-1".into()),
                    bind: Some(true),
                    eta_secs: None,
                    tags: Vec::new(),
                    parent_id: None,
                },
                crate::task_events::TaskEvent::Claimed {
                    task_id: task_id.clone(),
                    by: owner.clone(),
                },
                crate::task_events::TaskEvent::InProgress {
                    task_id: task_id.clone(),
                    by: owner.clone(),
                },
            ],
        )
        .expect("seed task");

        // A COMPLETED prior episode: Recovered, SAME generation as the binding.
        let recovered = Episode {
            key: key(),
            state: EpisodeState::Recovered,
            consecutive_ticks: 2,
            candidate: None,
            unlock_at: None,
            recipient: "lead".into(),
        };
        std::fs::write(
            runtime.join("usage_limit_episode.json"),
            serde_json::to_vec_pretty(&recovered).expect("episode json"),
        )
        .expect("episode write");

        let _binding_guard = super::acquire_binding_lock(&home, "worker-a").expect("binding lock");
        let make_input = |state| TickInput {
            now: now(),
            raw_state: state,
            source_backend: "codex".into(),
            source_team: Some("archfix".into()),
            source_role: Some("dev".into()),
            unlock_at: Some(now() + Duration::seconds(29 * 60 + 59)),
            correlation: super::correlation_from_disk(&home, "worker-a").map(|(c, _)| c),
            candidates: Vec::new(),
            recipient: "lead".into(),
        };

        // Tick 1 (UsageLimit): fresh detection — must NOT block on one tick.
        let mut tick1 = super::FsEffects::load(&home, "worker-a").expect("tick1 load");
        let outcome1 =
            super::observe_tick(&mut tick1, make_input(AgentState::UsageLimit)).expect("tick1");
        assert_eq!(
            outcome1,
            TickOutcome::Detected,
            "a Recovered episode is history: a new same-key UsageLimit must start a fresh cycle"
        );
        let after_one = crate::task_events::replay(&home).expect("replay tick1");
        assert_ne!(
            after_one.tasks.get(&task_id).expect("task").status,
            crate::task_events::TaskStatus::Blocked,
            "two-tick invariant: one UsageLimit tick after Recovered must not Block"
        );

        // Tick 2 (UsageLimit): second consecutive tick activates and blocks.
        let mut tick2 = super::FsEffects::load(&home, "worker-a").expect("tick2 load");
        let outcome2 =
            super::observe_tick(&mut tick2, make_input(AgentState::UsageLimit)).expect("tick2");
        assert_eq!(outcome2, TickOutcome::AwaitReset);
        assert_eq!(
            crate::task_events::replay(&home)
                .expect("replay tick2")
                .tasks
                .get(&task_id)
                .expect("task")
                .status,
            crate::task_events::TaskStatus::Blocked,
            "second consecutive tick must re-block"
        );

        // Tick 3 (Idle): limit lifted — the re-blocked task must re-heal.
        let mut tick3 = super::FsEffects::load(&home, "worker-a").expect("tick3 load");
        let outcome3 =
            super::observe_tick(&mut tick3, make_input(AgentState::Idle)).expect("tick3");
        assert_eq!(outcome3, TickOutcome::Recovered);
        let healed = crate::task_events::replay(&home).expect("replay tick3");
        let healed = healed.tasks.get(&task_id).expect("task");
        assert_eq!(
            healed.status,
            crate::task_events::TaskStatus::InProgress,
            "recurrence episode must stay recoverable — no permanent wedge"
        );
        assert_eq!(healed.owner.as_ref(), Some(&owner));
        std::fs::remove_dir_all(home).ok();
    }
}
