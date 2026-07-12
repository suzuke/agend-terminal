#[cfg(test)]
mod tests {
    use super::{
        choose_candidate, mutation_guard_violations, notification_recipient, observe_tick,
        supervisor_hook_uses_raw_current, CandidateFacts, Correlation, Effect, Episode, EpisodeKey,
        EpisodeState, MemoryEffects, TickInput, TickOutcome,
    };
    use crate::state::AgentState;
    use chrono::{DateTime, Duration, Utc};

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
        let mut fx = MemoryEffects::default();
        fx.episode = Some(Episode::activated(
            key(),
            EpisodeState::CandidateReady,
            Some("worker-b".into()),
        ));
        fx.notification_ids.insert(key().notification_id());

        let outcome = observe_tick(&mut fx, input(AgentState::UsageLimit)).expect("restart replay");
        assert_eq!(outcome, TickOutcome::AlreadyActive);
        assert!(
            fx.effects.is_empty(),
            "restart must not duplicate durable effects"
        );
    }

    #[test]
    fn recovery_is_generation_matched_and_atomic() {
        let mut fx = MemoryEffects::default();
        fx.episode = Some(Episode::activated(key(), EpisodeState::AwaitReset, None));

        let outcome = observe_tick(&mut fx, input(AgentState::Idle)).expect("recover");
        assert_eq!(outcome, TickOutcome::Recovered);
        assert_eq!(
            fx.effects,
            vec![
                Effect::RecoverTaskAtomically(key()),
                Effect::Persist(EpisodeState::Recovered),
            ]
        );

        let mut raced = MemoryEffects::default();
        raced.episode = Some(Episode::activated(key(), EpisodeState::AwaitReset, None));
        raced.generation_matches_at_atomic_append = false;
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
}
