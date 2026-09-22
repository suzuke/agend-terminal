//! Operator-only reconciliation of the seven audited orphan predecessors.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

const DECISION_ID: &str = "d-20260922032524703479-91";
const BOARD_PROJECT: &str = "Hack_agend-terminal";
const CONFIRMATION_TTL_SECS: i64 = 15 * 60;
const CONFIRMATION_DIR: &str = "orphan-reconcile-confirmations";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    predecessor_id: String,
    replacement_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RouteFingerprint {
    board: String,
    created_at: String,
    status: String,
    owner: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplacementTerminalProof {
    terminal_event_instance: String,
    terminal_event_seq: u64,
    record_sha256: String,
    evidence_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    schema_version: u32,
    operation_id: String,
    actor_digest: String,
    created_at: String,
    decision_id: String,
    board: String,
    audit_reason: String,
    canonical_mappings: Vec<Mapping>,
    mapping_digest: String,
    predecessor_routes: Vec<RouteFingerprint>,
    replacement_routes: Vec<RouteFingerprint>,
    replacement_terminal_proofs: Vec<ReplacementTerminalProof>,
    owner_authority_digest: String,
}

#[derive(Clone, Copy)]
struct ApprovedMapping {
    predecessor: &'static str,
    replacement: &'static str,
    owner: &'static str,
    stale_review: bool,
}

const APPROVED: &[ApprovedMapping] = &[
    ApprovedMapping {
        predecessor: "t-20260907235055249429-95750-527",
        replacement: "t-20260911011514337302-80976-487",
        owner: "claude-3df878",
        stale_review: true,
    },
    ApprovedMapping {
        predecessor: "t-20260914111332528060-87735-47",
        replacement: "t-20260914111408120558-87735-48",
        owner: "codex-41b2a8",
        stale_review: false,
    },
    ApprovedMapping {
        predecessor: "t-20260914111234728581-87735-45",
        replacement: "t-20260914111408120558-87735-48",
        owner: "codex-dae71d",
        stale_review: false,
    },
    ApprovedMapping {
        predecessor: "t-20260914111259722658-87735-46",
        replacement: "t-20260914111408120558-87735-48",
        owner: "codex-dae71d",
        stale_review: false,
    },
    ApprovedMapping {
        predecessor: "t-20260915052051881509-87735-153",
        replacement: "t-20260915052412876647-87735-158",
        owner: "codex-sol-dev",
        stale_review: false,
    },
    ApprovedMapping {
        predecessor: "t-20260914153935855471-87735-78",
        replacement: "t-20260914174754306925-87735-97",
        owner: "codex-sol-reviewer",
        stale_review: false,
    },
    ApprovedMapping {
        predecessor: "t-20260921192747528323-35876-350",
        replacement: "t-20260921210143816824-35876-355",
        owner: "codex-sol-reviewer",
        stale_review: false,
    },
];

fn actor_digest(actor: &str) -> String {
    crate::daemon::utils::sha256_hex(actor.as_bytes())
}

fn mapping_for(predecessor: &str) -> Option<ApprovedMapping> {
    APPROVED
        .iter()
        .copied()
        .find(|mapping| mapping.predecessor == predecessor)
}

fn canonical_mappings(mappings: &[Mapping]) -> String {
    mappings
        .iter()
        .map(|mapping| format!("{}={}", mapping.predecessor_id, mapping.replacement_id))
        .collect::<Vec<_>>()
        .join("\n")
}

fn expected_mappings() -> Vec<Mapping> {
    let mut mappings: Vec<_> = APPROVED
        .iter()
        .map(|mapping| Mapping {
            predecessor_id: mapping.predecessor.to_string(),
            replacement_id: mapping.replacement.to_string(),
        })
        .collect();
    mappings.sort();
    mappings
}

fn parse_mappings(args: &Value) -> Result<Vec<Mapping>, Value> {
    let Some(values) = args.get("mappings").and_then(Value::as_array) else {
        return Err(json!({"error":"mappings must be an array", "code":"invalid_request"}));
    };
    let parsed: Result<Vec<_>, _> = values.iter().cloned().map(serde_json::from_value).collect();
    let mut parsed = parsed.map_err(
        |error| json!({"error":format!("invalid mapping: {error}"),"code":"invalid_request"}),
    )?;
    if parsed.is_empty() {
        return Err(json!({
            "error": "mappings must contain the frozen seven-row set",
            "code": "invalid_request"
        }));
    }
    parsed.sort();
    if parsed != expected_mappings() {
        return Err(json!({
            "error":"mapping set is not the frozen seven-row decision allowlist",
            "code":"mapping_not_allowed"
        }));
    }
    Ok(parsed)
}

fn authority_with_live_instances(
    home: &Path,
    live_instances: &HashSet<String>,
) -> Result<super::sweep::OwnerAuthority, Value> {
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home));
    let fleet_instances = fleet
        .as_ref()
        .map(|config| config.instances.keys().cloned().collect())
        .unwrap_or_default();
    if fleet.is_err() {
        return Err(
            json!({"error":"fleet owner authority unavailable","code":"owner_authority_unavailable"}),
        );
    }
    Ok(super::sweep::OwnerAuthority {
        live_instances: live_instances.clone(),
        fleet_instances,
        live_available: true,
        fleet_available: true,
    })
}

fn authority(home: &Path) -> Result<super::sweep::OwnerAuthority, Value> {
    let Some(live_instances) = crate::runtime::list_live_agents(home) else {
        return Err(
            json!({"error":"live owner authority unavailable","code":"owner_authority_unavailable"}),
        );
    };
    authority_with_live_instances(home, &live_instances)
}

fn authority_digest(authority: &super::sweep::OwnerAuthority) -> String {
    let mut live: Vec<_> = authority.live_instances.iter().cloned().collect();
    let mut fleet: Vec<_> = authority.fleet_instances.iter().cloned().collect();
    live.sort();
    fleet.sort();
    let value = json!({
        "live_available": authority.live_available,
        "fleet_available": authority.fleet_available,
        "live_instances": live,
        "fleet_instances": fleet,
    });
    crate::daemon::utils::sha256_hex(&serde_json::to_vec(&value).unwrap_or_default())
}

fn validate_predecessor(
    record: &crate::task_events::TaskRecord,
    approved: ApprovedMapping,
    authority: &super::sweep::OwnerAuthority,
) -> Result<(), String> {
    if record.owner.as_ref().map(|owner| owner.0.as_str()) != Some(approved.owner) {
        return Err("predecessor owner changed".to_string());
    }
    if crate::tasks::orphan::classify_owner(
        approved.owner,
        &authority.live_instances,
        &authority.fleet_instances,
    ) != crate::tasks::orphan::OwnerClassification::Strict
    {
        return Err("predecessor owner is not a strict ghost".to_string());
    }
    let expected = if approved.stale_review {
        crate::task_events::TaskStatus::InReview
    } else {
        crate::task_events::TaskStatus::Open
    };
    if record.status != expected {
        return Err(format!("predecessor status changed (expected {expected})"));
    }
    Ok(())
}

fn record_sha256(record: &crate::task_events::TaskRecord) -> Result<String, String> {
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    Ok(crate::daemon::utils::sha256_hex(&bytes))
}

fn terminal_proof(
    board: &Path,
    replacement: &crate::task_events::TaskRecord,
) -> Result<ReplacementTerminalProof, String> {
    if replacement.status != crate::task_events::TaskStatus::Done {
        return Err("replacement is not Done".to_string());
    }
    if replacement
        .result
        .as_deref()
        .is_none_or(|result| result.trim().is_empty())
    {
        return Err("replacement has no evidence-bearing result".to_string());
    }
    let history = crate::task_events::envelopes_for_task_at(board, &replacement.id.0)
        .map_err(|error| format!("replacement history unavailable: {error}"))?;
    let Some(done) = history
        .iter()
        .rev()
        .find(|envelope| matches!(envelope.event, crate::task_events::TaskEvent::Done { .. }))
    else {
        return Err("replacement has no terminal Done envelope".to_string());
    };
    let evidence = serde_json::to_vec(&(done, replacement.result.as_deref().unwrap_or("")))
        .map_err(|error| error.to_string())?;
    Ok(ReplacementTerminalProof {
        terminal_event_instance: done.instance.0.clone(),
        terminal_event_seq: done.seq,
        record_sha256: record_sha256(replacement)?,
        evidence_sha256: crate::daemon::utils::sha256_hex(&evidence),
    })
}

fn route_fingerprint(routed: &super::RoutedTask) -> RouteFingerprint {
    RouteFingerprint {
        board: routed.board().project().to_string(),
        created_at: routed.record().created_at.clone(),
        status: routed.record().status.to_string(),
        owner: routed.record().owner.as_ref().map(|owner| owner.0.clone()),
    }
}

fn validate_routes(
    home: &Path,
    mappings: &[Mapping],
    board_project: &str,
) -> Result<Vec<(super::RoutedTask, super::RoutedTask)>, Value> {
    let mut routed = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let predecessor = super::load_routed(home, &mapping.predecessor_id).map_err(|error| {
            json!({"error":format!("predecessor route unavailable: {error}"),"code":"task_route_unavailable"})
        })?;
        let replacement = super::load_routed(home, &mapping.replacement_id).map_err(|error| {
            json!({"error":format!("replacement route unavailable: {error}"),"code":"task_route_unavailable"})
        })?;
        if predecessor.board().project() != board_project
            || replacement.board().project() != board_project
            || predecessor.board().path() != replacement.board().path()
        {
            return Err(
                json!({"error":"all mappings must resolve to the decision board","code":"cross_board_mapping"}),
            );
        }
        routed.push((predecessor, replacement));
    }
    Ok(routed)
}

fn preview_with_authority(
    home: &Path,
    actor: &str,
    args: &Value,
    authority: &super::sweep::OwnerAuthority,
) -> Value {
    let decision_id = args["decision_id"].as_str().unwrap_or("");
    let board = args["board"].as_str().unwrap_or("");
    let audit_reason = args["audit_reason"].as_str().unwrap_or("").trim();
    if decision_id != DECISION_ID || board != BOARD_PROJECT || audit_reason.is_empty() {
        return json!({"error":"decision_id, board and audit_reason do not match the frozen scope","code":"invalid_request"});
    }
    let mappings = match parse_mappings(args) {
        Ok(mappings) => mappings,
        Err(error) => return error,
    };
    let routed = match validate_routes(home, &mappings, board) {
        Ok(routed) => routed,
        Err(error) => return error,
    };
    let mut predecessor_routes = Vec::with_capacity(mappings.len());
    let mut replacement_routes = Vec::with_capacity(mappings.len());
    let mut replacement_terminal_proofs = Vec::with_capacity(mappings.len());
    let mut eligibility = Vec::with_capacity(mappings.len());
    for (mapping, (predecessor, replacement)) in mappings.iter().zip(routed.iter()) {
        let Some(approved) = mapping_for(&mapping.predecessor_id) else {
            return json!({"error":"mapping changed during validation","code":"mapping_not_allowed"});
        };
        if mapping.replacement_id != approved.replacement {
            return json!({"error":"mapping changed during validation","code":"mapping_not_allowed"});
        }
        if let Err(error) = validate_predecessor(predecessor.record(), approved, authority) {
            return json!({"error":error,"code":"stale_predecessor"});
        }
        eligibility.push(if approved.stale_review {
            "explicit_stale_review"
        } else {
            "strict_ghost"
        });
        let proof = match terminal_proof(replacement.board().path(), replacement.record()) {
            Ok(proof) => proof,
            Err(error) => return json!({"error":error,"code":"replacement_not_eligible"}),
        };
        predecessor_routes.push(route_fingerprint(predecessor));
        replacement_routes.push(route_fingerprint(replacement));
        replacement_terminal_proofs.push(proof);
    }
    let canonical = canonical_mappings(&mappings);
    let operation_id = uuid::Uuid::new_v4().to_string();
    let confirmation = Confirmation {
        schema_version: 1,
        operation_id: operation_id.clone(),
        actor_digest: actor_digest(actor),
        created_at: chrono::Utc::now().to_rfc3339(),
        decision_id: decision_id.to_string(),
        board: board.to_string(),
        audit_reason: audit_reason.to_string(),
        canonical_mappings: mappings,
        mapping_digest: crate::daemon::utils::sha256_hex(canonical.as_bytes()),
        predecessor_routes,
        replacement_routes,
        replacement_terminal_proofs,
        owner_authority_digest: authority_digest(authority),
    };
    let directory = home.join(CONFIRMATION_DIR);
    if let Err(error) = std::fs::create_dir_all(&directory).and_then(|_| {
        let bytes = serde_json::to_vec_pretty(&confirmation).map_err(std::io::Error::other)?;
        crate::store::atomic_write(&directory.join(format!("{operation_id}.json")), &bytes)
            .map_err(std::io::Error::other)
    }) {
        return json!({"error":format!("confirmation write failed: {error}"),"code":"confirmation_write_failed"});
    }
    let bytes = match std::fs::read(directory.join(format!("{operation_id}.json"))) {
        Ok(bytes) => bytes,
        Err(error) => return json!({"error":error.to_string(),"code":"confirmation_invalid"}),
    };
    let preview_digest = crate::daemon::utils::sha256_hex(&bytes);
    let signature = match crate::config_integrity::sign(home, preview_digest.as_bytes()) {
        Ok(signature) => signature,
        Err(error) => return json!({"error":error.to_string(),"code":"confirmation_write_failed"}),
    };
    if let Err(error) = crate::store::atomic_write(
        &directory.join(format!("{operation_id}.sha256")),
        preview_digest.as_bytes(),
    )
    .and_then(|_| {
        crate::store::atomic_write(
            &directory.join(format!("{operation_id}.sig")),
            signature.as_bytes(),
        )
    }) {
        return json!({"error":format!("confirmation integrity sidecar write failed: {error}"),"code":"confirmation_write_failed"});
    }
    json!({
        "ok": true,
        "result": {
            "confirmation": operation_id,
            "preview_digest": preview_digest,
            "mapping_digest": confirmation.mapping_digest,
            "event_count": 7,
            "valid_for_seconds": CONFIRMATION_TTL_SECS,
            "mappings": confirmation.canonical_mappings,
            "eligibility": eligibility,
            "predecessor_routes": confirmation.predecessor_routes,
            "replacement_routes": confirmation.replacement_routes,
            "replacement_terminal_proofs": confirmation.replacement_terminal_proofs,
        }
    })
}

fn load_confirmation(home: &Path, token: &str) -> Result<(Confirmation, Vec<u8>), Value> {
    if !uuid::Uuid::parse_str(token).is_ok_and(|id| id.to_string() == token) {
        return Err(json!({"error":"invalid confirmation","code":"invalid_confirmation"}));
    }
    let path = home.join(CONFIRMATION_DIR).join(format!("{token}.json"));
    let bytes = std::fs::read(path).map_err(|error| {
        json!({"error":format!("confirmation unavailable: {error}"),"code":"confirmation_unavailable"})
    })?;
    let digest = crate::daemon::utils::sha256_hex(&bytes);
    let stored_digest = std::fs::read_to_string(
        home.join(CONFIRMATION_DIR).join(format!("{token}.sha256")),
    )
    .map_err(|error| {
        json!({"error":format!("confirmation digest unavailable: {error}"),"code":"confirmation_integrity"})
    })?;
    let signature = std::fs::read_to_string(
        home.join(CONFIRMATION_DIR).join(format!("{token}.sig")),
    )
    .map_err(|error| {
        json!({"error":format!("confirmation signature unavailable: {error}"),"code":"confirmation_integrity"})
    })?;
    if stored_digest != digest
        || !crate::config_integrity::verify(home, stored_digest.as_bytes(), signature.trim())
    {
        return Err(json!({
            "error":"confirmation integrity verification failed",
            "code":"confirmation_integrity"
        }));
    }
    let confirmation: Confirmation = serde_json::from_slice(&bytes).map_err(|error| {
        json!({"error":format!("confirmation invalid: {error}"),"code":"confirmation_invalid"})
    })?;
    if confirmation.schema_version != 1 || confirmation.operation_id != token {
        return Err(
            json!({"error":"confirmation schema mismatch","code":"confirmation_authority_mismatch"}),
        );
    }
    Ok((confirmation, bytes))
}

fn durable_operation_state(
    home: &Path,
    confirmation: &Confirmation,
    preview_digest: &str,
) -> Result<Option<bool>, Value> {
    let mut applied = 0usize;
    for mapping in &confirmation.canonical_mappings {
        let routed = super::load_routed(home, &mapping.predecessor_id).map_err(|error| {
            json!({"error":format!("predecessor route unavailable: {error}"),"code":"task_route_unavailable"})
        })?;
        let history = crate::task_events::envelopes_for_task_at(
            routed.board().path(),
            &mapping.predecessor_id,
        )
        .map_err(|error| json!({"error":error.to_string(),"code":"history_unavailable"}))?;
        for envelope in history {
            if let crate::task_events::TaskEvent::Superseded {
                successor_id,
                proof: Some(proof),
                ..
            } = envelope.event
            {
                if proof.operation_id == confirmation.operation_id {
                    if proof.preview_digest == preview_digest
                        && successor_id.0 == mapping.replacement_id
                    {
                        applied += 1;
                    } else {
                        return Err(
                            json!({"error":"durable reconciliation proof conflicts with confirmation","code":"reconciliation_conflict"}),
                        );
                    }
                }
            }
        }
        #[cfg(test)]
        if applied > 0 && applied < confirmation.canonical_mappings.len() {
            fire_after_partial_durable_proof_hook_for_test(home);
        }
    }
    if applied == 0 {
        Ok(None)
    } else if applied == confirmation.canonical_mappings.len() {
        Ok(Some(true))
    } else {
        Err(
            json!({"error":"durable reconciliation proof is partial","code":"reconciliation_conflict"}),
        )
    }
}

#[cfg(test)]
type AfterPartialDurableProofHook = Box<dyn FnOnce() + Send + 'static>;

#[cfg(test)]
static AFTER_PARTIAL_DURABLE_PROOF_HOOK: std::sync::OnceLock<
    std::sync::Mutex<Option<(std::path::PathBuf, AfterPartialDurableProofHook)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn install_after_partial_durable_proof_hook_for_test(
    home: &Path,
    hook: impl FnOnce() + Send + 'static,
) {
    let hooks = AFTER_PARTIAL_DURABLE_PROOF_HOOK.get_or_init(|| std::sync::Mutex::new(None));
    *hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((home.to_path_buf(), Box::new(hook)));
}

#[cfg(test)]
fn fire_after_partial_durable_proof_hook_for_test(home: &Path) {
    let Some(hooks) = AFTER_PARTIAL_DURABLE_PROOF_HOOK.get() else {
        return;
    };
    let mut hooks = hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = hooks
        .as_ref()
        .is_some_and(|(target, _)| target == home)
        .then(|| hooks.take())
        .flatten()
        .map(|(_, hook)| hook);
    if let Some(hook) = hook {
        hook();
    }
}

fn apply_with_authority(
    home: &Path,
    actor: &str,
    args: &Value,
    authority: &super::sweep::OwnerAuthority,
    authority_refresh: &dyn Fn() -> Result<super::sweep::OwnerAuthority, Value>,
) -> Value {
    let token = args["confirmation"].as_str().unwrap_or("");
    let (confirmation, bytes) = match load_confirmation(home, token) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let preview_digest = crate::daemon::utils::sha256_hex(&bytes);
    // Lock every distinct ID before resolving the fresh routes. This makes an
    // identical concurrent apply wait for the winner, then observe its durable
    // proof instead of racing against a post-commit route fingerprint.
    let mut ids: Vec<String> = confirmation
        .canonical_mappings
        .iter()
        .flat_map(|mapping| {
            [
                mapping.predecessor_id.clone(),
                mapping.replacement_id.clone(),
            ]
        })
        .collect();
    ids.sort();
    ids.dedup();
    let mut id_locks = Vec::with_capacity(ids.len());
    for id in &ids {
        match super::board_router::acquire_task_id_lock(home, id) {
            Ok(lock) => id_locks.push(lock),
            Err(error) => {
                return json!({"error":format!("task lock failed: {error}"),"code":"task_lock_failed"})
            }
        }
    }
    // Read the durable proof only after the per-ID locks are held. The batch
    // writer appends one JSONL envelope at a time, so an unlocked read can see
    // a transient partial proof and incorrectly reject the idempotent retry.
    match durable_operation_state(home, &confirmation, &preview_digest) {
        Ok(Some(true)) => {
            return json!({"ok":true,"result":{"already_applied":true,"event_count":7}})
        }
        Ok(None) => {}
        Ok(Some(false)) => {
            return json!({"error":"durable reconciliation proof is not complete","code":"reconciliation_conflict"})
        }
        Err(error) => return error,
    }
    if confirmation.actor_digest != actor_digest(actor) {
        return json!({"error":"confirmation actor mismatch","code":"confirmation_authority_mismatch"});
    }
    let created = match chrono::DateTime::parse_from_rfc3339(&confirmation.created_at) {
        Ok(created) => created,
        Err(_) => {
            return json!({"error":"confirmation created_at invalid","code":"confirmation_invalid"})
        }
    };
    let age = chrono::Utc::now().signed_duration_since(created);
    if age < chrono::Duration::zero() || age >= chrono::Duration::seconds(CONFIRMATION_TTL_SECS) {
        return json!({"error":"confirmation expired","code":"confirmation_expired"});
    }
    if confirmation.decision_id != DECISION_ID
        || confirmation.board != BOARD_PROJECT
        || confirmation.audit_reason.trim().is_empty()
        || confirmation.canonical_mappings != expected_mappings()
        || confirmation.mapping_digest
            != crate::daemon::utils::sha256_hex(
                canonical_mappings(&confirmation.canonical_mappings).as_bytes(),
            )
    {
        return json!({"error":"confirmation scope or mapping digest mismatch","code":"confirmation_authority_mismatch"});
    }
    if confirmation.owner_authority_digest != authority_digest(authority) {
        return json!({"error":"owner authority changed since preview","code":"owner_authority_changed"});
    }
    let routed = match validate_routes(home, &confirmation.canonical_mappings, &confirmation.board)
    {
        Ok(routed) => routed,
        Err(error) => {
            if matches!(
                durable_operation_state(home, &confirmation, &preview_digest),
                Ok(Some(true))
            ) {
                return json!({"ok":true,"result":{"already_applied":true,"event_count":7}});
            }
            return error;
        }
    };
    if routed.len() != confirmation.predecessor_routes.len()
        || routed.len() != confirmation.replacement_routes.len()
        || routed.len() != confirmation.replacement_terminal_proofs.len()
    {
        return json!({"error":"confirmation proof cardinality mismatch","code":"confirmation_invalid"});
    }
    for (index, (predecessor, replacement)) in routed.iter().enumerate() {
        if route_fingerprint(predecessor) != confirmation.predecessor_routes[index]
            || route_fingerprint(replacement) != confirmation.replacement_routes[index]
        {
            if matches!(
                durable_operation_state(home, &confirmation, &preview_digest),
                Ok(Some(true))
            ) {
                return json!({"ok":true,"result":{"already_applied":true,"event_count":7}});
            }
            return json!({"error":"task route changed since preview","code":"stale_preview"});
        }
    }
    let board = routed
        .first()
        .map(|(predecessor, _)| predecessor.board().path().to_path_buf())
        .expect("frozen mapping is non-empty");
    let emitter = crate::task_events::InstanceName::from(actor);
    let append = crate::task_events::append_batch_computed_at(&board, &emitter, |state| {
        let fresh_authority = authority_refresh()
            .map_err(|error| format!("owner authority unavailable before commit: {error}"))?;
        if authority_digest(&fresh_authority) != authority_digest(authority) {
            return Err("owner authority changed before commit".to_string());
        }
        let mut existing = 0usize;
        for (index, mapping) in confirmation.canonical_mappings.iter().enumerate() {
            let predecessor = state
                .tasks
                .get(&crate::task_events::TaskId(mapping.predecessor_id.clone()))
                .ok_or_else(|| "predecessor disappeared before commit".to_string())?;
            let replacement = state
                .tasks
                .get(&crate::task_events::TaskId(mapping.replacement_id.clone()))
                .ok_or_else(|| "replacement disappeared before commit".to_string())?;
            let predecessor_route = RouteFingerprint {
                board: confirmation.board.clone(),
                created_at: predecessor.created_at.clone(),
                status: predecessor.status.to_string(),
                owner: predecessor.owner.as_ref().map(|owner| owner.0.clone()),
            };
            let replacement_route = RouteFingerprint {
                board: confirmation.board.clone(),
                created_at: replacement.created_at.clone(),
                status: replacement.status.to_string(),
                owner: replacement.owner.as_ref().map(|owner| owner.0.clone()),
            };
            if predecessor_route != confirmation.predecessor_routes[index]
                || replacement_route != confirmation.replacement_routes[index]
            {
                return Err("task route changed before commit".to_string());
            }
            if predecessor.status == crate::task_events::TaskStatus::Superseded {
                let history =
                    crate::task_events::envelopes_for_task_at(&board, &mapping.predecessor_id)
                        .map_err(|error| error.to_string())?;
                let matched = history.iter().any(|envelope| {
                    matches!(&envelope.event,
                        crate::task_events::TaskEvent::Superseded {
                            successor_id,
                            proof: Some(proof),
                            ..
                        } if successor_id.0 == mapping.replacement_id
                            && proof.operation_id == confirmation.operation_id
                            && proof.preview_digest == preview_digest)
                });
                if matched {
                    existing += 1;
                    continue;
                }
                return Err("predecessor already has a conflicting supersession".to_string());
            }
            let approved = mapping_for(&mapping.predecessor_id)
                .ok_or_else(|| "mapping is not approved".to_string())?;
            validate_predecessor(predecessor, approved, &fresh_authority)?;
            if predecessor.superseded_by.is_some() || predecessor.status.is_terminal() {
                return Err("predecessor changed before commit".to_string());
            }
            let proof = terminal_proof(&board, replacement)?;
            let expected = confirmation
                .replacement_terminal_proofs
                .get(index)
                .ok_or_else(|| "replacement terminal proof missing".to_string())?;
            if expected != &proof {
                return Err("replacement record or evidence changed".to_string());
            }
        }
        if existing == confirmation.canonical_mappings.len() {
            return Ok(Vec::new());
        }
        if existing != 0 {
            return Err("reconciliation batch is partially committed".to_string());
        }
        confirmation
            .canonical_mappings
            .iter()
            .zip(confirmation.replacement_terminal_proofs.iter())
            .map(|(mapping, terminal)| {
                let predecessor = state
                    .tasks
                    .get(&crate::task_events::TaskId(mapping.predecessor_id.clone()))
                    .ok_or_else(|| "predecessor disappeared before commit".to_string())?;
                let replacement = state
                    .tasks
                    .get(&crate::task_events::TaskId(mapping.replacement_id.clone()))
                    .ok_or_else(|| "replacement disappeared before commit".to_string())?;
                Ok(crate::task_events::TaskEvent::Superseded {
                    task_id: predecessor.id.clone(),
                    by: emitter.clone(),
                    successor_id: replacement.id.clone(),
                    proof: Some(crate::task_events::SupersessionReconciliationProof {
                        operation_id: confirmation.operation_id.clone(),
                        preview_digest: preview_digest.clone(),
                        mapping_digest: confirmation.mapping_digest.clone(),
                        board: confirmation.board.clone(),
                        audit_reason: confirmation.audit_reason.clone(),
                        predecessor_created_at: predecessor.created_at.clone(),
                        replacement_created_at: replacement.created_at.clone(),
                        replacement_terminal_event_instance: terminal
                            .terminal_event_instance
                            .clone(),
                        replacement_terminal_event_seq: terminal.terminal_event_seq,
                        replacement_record_sha256: terminal.record_sha256.clone(),
                        replacement_evidence_sha256: terminal.evidence_sha256.clone(),
                        actor_digest: confirmation.actor_digest.clone(),
                    }),
                })
            })
            .collect()
    });
    drop(id_locks);
    match append {
        Ok(Ok(seqs)) if seqs.is_empty() => {
            json!({"ok":true,"result":{"already_applied":true,"event_count":7}})
        }
        Ok(Ok(seqs)) => {
            for mapping in &confirmation.canonical_mappings {
                super::task_terminal_cleanup(home, &mapping.predecessor_id);
            }
            json!({"ok":true,"result":{"applied":seqs.len(),"event_count":seqs.len()}})
        }
        Ok(Err(reason)) => json!({"error":reason,"code":"stale_preview"}),
        Err(error) => {
            json!({"error":format!("reconciliation append failed: {error}"),"code":"reconciliation_append_failed"})
        }
    }
}

pub(super) fn handle(home: &Path, actor: &str, args: &Value) -> Value {
    match args["action"].as_str() {
        Some("orphan_reconcile_preview") => {
            if actor != "operator" {
                return json!({"error":"orphan reconciliation is operator-only","code":"operator_only"});
            }
            if args["decision_id"].as_str() != Some(DECISION_ID)
                || args["board"].as_str() != Some(BOARD_PROJECT)
                || args["audit_reason"]
                    .as_str()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return json!({"error":"decision_id, board and audit_reason do not match the frozen scope","code":"invalid_request"});
            }
            if let Err(error) = parse_mappings(args) {
                return error;
            }
            match authority(home) {
                Ok(authority) => preview_with_authority(home, actor, args, &authority),
                Err(error) => error,
            }
        }
        Some("orphan_reconcile_apply") => {
            if actor != "operator" {
                return json!({"error":"orphan reconciliation is operator-only","code":"operator_only"});
            }
            let token = args["confirmation"].as_str().unwrap_or("");
            if !uuid::Uuid::parse_str(token).is_ok_and(|id| id.to_string() == token) {
                return json!({"error":"invalid confirmation","code":"invalid_confirmation"});
            }
            match authority(home) {
                Ok(initial_authority) => {
                    let refresh = || authority(home);
                    apply_with_authority(home, actor, args, &initial_authority, &refresh)
                }
                Err(error) => error,
            }
        }
        _ => json!({"error":"unknown orphan reconciliation action","code":"unknown_action"}),
    }
}

pub(super) fn handle_with_live_instances(
    home: &Path,
    actor: &str,
    args: &Value,
    live_instances: &HashSet<String>,
) -> Value {
    let refresh = || Some(live_instances.clone());
    handle_with_live_instances_and_refresh(home, actor, args, live_instances, &refresh)
}

pub(super) fn handle_with_live_instances_and_refresh(
    home: &Path,
    actor: &str,
    args: &Value,
    live_instances: &HashSet<String>,
    live_refresh: &dyn Fn() -> Option<HashSet<String>>,
) -> Value {
    if actor != "operator" {
        return json!({"error":"orphan reconciliation is operator-only","code":"operator_only"});
    }
    let authority = match authority_with_live_instances(home, live_instances) {
        Ok(authority) => authority,
        Err(error) => return error,
    };
    match args["action"].as_str() {
        Some("orphan_reconcile_preview") => preview_with_authority(home, actor, args, &authority),
        Some("orphan_reconcile_apply") => {
            let refresh = || {
                live_refresh()
                    .ok_or_else(|| {
                        json!({
                            "error":"live owner authority unavailable",
                            "code":"owner_authority_unavailable"
                        })
                    })
                    .and_then(|live| authority_with_live_instances(home, &live))
            };
            apply_with_authority(home, actor, args, &authority, &refresh)
        }
        _ => json!({"error":"unknown orphan reconciliation action","code":"unknown_action"}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    fn tmp_home(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "agend-orphan-reconcile-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create test home");
        home
    }

    fn fixture() -> (PathBuf, Vec<Mapping>) {
        let home = tmp_home("fixture");
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n")
            .expect("write empty fleet");
        let board = crate::task_events::board_root(&home, BOARD_PROJECT);
        let fixture_actor = crate::task_events::InstanceName::from("fixture");
        let mappings = expected_mappings();
        let replacement_ids: std::collections::BTreeSet<_> = mappings
            .iter()
            .map(|mapping| mapping.replacement_id.clone())
            .collect();
        for mapping in &mappings {
            let approved = mapping_for(&mapping.predecessor_id).expect("approved predecessor");
            crate::task_events::append_at(
                &board,
                &fixture_actor,
                crate::task_events::TaskEvent::Created {
                    task_id: crate::task_events::TaskId(mapping.predecessor_id.clone()),
                    title: "audited predecessor".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: Some(approved.owner.into()),
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
                    governing_decision_id: None,
                    review_class: None,
                },
            )
            .expect("create predecessor");
            if approved.stale_review {
                crate::task_events::append_at(
                    &board,
                    &fixture_actor,
                    crate::task_events::TaskEvent::MovedToReview {
                        task_id: crate::task_events::TaskId(mapping.predecessor_id.clone()),
                    },
                )
                .expect("move predecessor to review");
            }
        }
        for replacement_id in replacement_ids {
            crate::task_events::append_at(
                &board,
                &fixture_actor,
                crate::task_events::TaskEvent::Created {
                    task_id: crate::task_events::TaskId(replacement_id.clone()),
                    title: "existing completed replacement".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
                    governing_decision_id: None,
                    review_class: None,
                },
            )
            .expect("create replacement");
            crate::task_events::append_at(
                &board,
                &fixture_actor,
                crate::task_events::TaskEvent::Done {
                    task_id: crate::task_events::TaskId(replacement_id),
                    by: fixture_actor.clone(),
                    source: crate::task_events::DoneSource::OperatorManual {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                        result: Some("verified replacement evidence".into()),
                    },
                },
            )
            .expect("complete replacement");
        }
        (home, mappings)
    }

    fn preview_args(mappings: &[Mapping]) -> Value {
        serde_json::json!({
            "action": "orphan_reconcile_preview",
            "decision_id": DECISION_ID,
            "board": BOARD_PROJECT,
            "audit_reason": "operator audit reconciliation test",
            "mappings": mappings,
        })
    }

    #[test]
    fn preview_apply_and_retry_are_atomic_and_replacement_immutable() {
        let (home, mappings) = fixture();
        let board = crate::task_events::board_root(&home, BOARD_PROJECT);
        let before = std::fs::read(board.join("task_events.jsonl")).expect("fixture log");
        let preview = handle_with_live_instances(
            &home,
            "operator",
            &preview_args(&mappings),
            &HashSet::new(),
        );
        assert_eq!(preview["ok"], true, "preview: {preview}");
        assert_eq!(preview["result"]["event_count"], 7);
        let confirmation = preview["result"]["confirmation"]
            .as_str()
            .expect("confirmation")
            .to_string();
        assert_eq!(
            std::fs::read(board.join("task_events.jsonl")).unwrap(),
            before
        );

        let replacement_before: Vec<_> = mappings
            .iter()
            .map(|mapping| {
                let routed = super::super::load_routed(&home, &mapping.replacement_id)
                    .expect("replacement route");
                serde_json::to_value(routed.record()).expect("replacement record")
            })
            .collect();
        let applied = handle_with_live_instances(
            &home,
            "operator",
            &serde_json::json!({
                "action": "orphan_reconcile_apply",
                "confirmation": confirmation,
            }),
            &HashSet::new(),
        );
        assert_eq!(applied["ok"], true, "apply: {applied}");
        assert_eq!(applied["result"]["applied"], 7);

        let state = crate::task_events::projected_state_at(&board).expect("projected state");
        for mapping in &mappings {
            let predecessor =
                &state.tasks[&crate::task_events::TaskId(mapping.predecessor_id.clone())];
            assert_eq!(
                predecessor.status,
                crate::task_events::TaskStatus::Superseded
            );
            assert_eq!(
                predecessor.superseded_by.as_ref().map(|id| id.0.as_str()),
                Some(mapping.replacement_id.as_str())
            );
        }
        let replacement_after: Vec<_> = mappings
            .iter()
            .map(|mapping| {
                let routed = super::super::load_routed(&home, &mapping.replacement_id)
                    .expect("replacement route");
                serde_json::to_value(routed.record()).expect("replacement record")
            })
            .collect();
        assert_eq!(replacement_after, replacement_before);

        let proof_count = mappings
            .iter()
            .flat_map(|mapping| {
                crate::task_events::envelopes_for_task_at(&board, &mapping.predecessor_id)
                    .expect("predecessor history")
            })
            .filter(|envelope| {
                matches!(
                    envelope.event,
                    crate::task_events::TaskEvent::Superseded { proof: Some(_), .. }
                )
            })
            .count();
        assert_eq!(proof_count, 7);

        let retry = handle_with_live_instances(
            &home,
            "operator",
            &serde_json::json!({
                "action": "orphan_reconcile_apply",
                "confirmation": preview["result"]["confirmation"],
            }),
            &HashSet::new(),
        );
        assert_eq!(retry["result"]["already_applied"], true, "retry: {retry}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn stale_predecessor_refuses_before_batch_append() {
        let (home, mappings) = fixture();
        let board = crate::task_events::board_root(&home, BOARD_PROJECT);
        let preview = handle_with_live_instances(
            &home,
            "operator",
            &preview_args(&mappings),
            &HashSet::new(),
        );
        let confirmation = preview["result"]["confirmation"].as_str().unwrap();
        let stale_id = &mappings[1].predecessor_id;
        crate::task_events::append_at(
            &board,
            &crate::task_events::InstanceName::from("fixture"),
            crate::task_events::TaskEvent::Claimed {
                task_id: crate::task_events::TaskId(stale_id.clone()),
                by: crate::task_events::InstanceName::from("codex-41b2a8"),
            },
        )
        .expect("mutate predecessor");
        let before_apply = std::fs::read(board.join("task_events.jsonl")).unwrap();
        let response = handle_with_live_instances(
            &home,
            "operator",
            &serde_json::json!({
                "action": "orphan_reconcile_apply",
                "confirmation": confirmation,
            }),
            &HashSet::new(),
        );
        assert_eq!(response["code"], "stale_preview", "response: {response}");
        assert_eq!(
            std::fs::read(board.join("task_events.jsonl")).unwrap(),
            before_apply,
            "a stale batch must append zero events"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn authority_change_inside_append_fence_refuses_before_batch_append() {
        let (home, mappings) = fixture();
        let board = crate::task_events::board_root(&home, BOARD_PROJECT);
        let preview = handle_with_live_instances(
            &home,
            "operator",
            &preview_args(&mappings),
            &HashSet::new(),
        );
        let confirmation = preview["result"]["confirmation"].as_str().unwrap();
        let before = std::fs::read(board.join("task_events.jsonl")).expect("fixture log");
        let newly_live = HashSet::from(["claude-3df878".to_string()]);
        let refresh = || Some(newly_live.clone());
        let applied = handle_with_live_instances_and_refresh(
            &home,
            "operator",
            &serde_json::json!({
                "action": "orphan_reconcile_apply",
                "confirmation": confirmation,
            }),
            &HashSet::new(),
            &refresh,
        );
        assert_eq!(
            applied["code"], "stale_preview",
            "authority race: {applied}"
        );
        assert_eq!(
            std::fs::read(board.join("task_events.jsonl")).expect("event log"),
            before,
            "authority change must append zero events"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn edited_confirmation_bytes_and_ttl_refuse_before_batch_append() {
        let (home, mappings) = fixture();
        let board = crate::task_events::board_root(&home, BOARD_PROJECT);
        let preview = handle_with_live_instances(
            &home,
            "operator",
            &preview_args(&mappings),
            &HashSet::new(),
        );
        let confirmation = preview["result"]["confirmation"].as_str().unwrap();
        let path = home
            .join(CONFIRMATION_DIR)
            .join(format!("{confirmation}.json"));
        let mut bytes: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        bytes["audit_reason"] = Value::String("edited after preview".into());
        bytes["created_at"] = Value::String("2099-01-01T00:00:00Z".into());
        crate::store::atomic_write(&path, &serde_json::to_vec_pretty(&bytes).unwrap()).unwrap();
        let before = std::fs::read(board.join("task_events.jsonl")).expect("fixture log");
        let applied = handle_with_live_instances(
            &home,
            "operator",
            &serde_json::json!({
                "action": "orphan_reconcile_apply",
                "confirmation": confirmation,
            }),
            &HashSet::new(),
        );
        assert_eq!(
            applied["code"], "confirmation_integrity",
            "tampered confirmation: {applied}"
        );
        assert_eq!(
            std::fs::read(board.join("task_events.jsonl")).expect("event log"),
            before,
            "tampered confirmation must append zero events"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn concurrent_identical_applies_have_one_winner_and_one_idempotent_retry() {
        let (home, mappings) = fixture();
        let preview = handle_with_live_instances(
            &home,
            "operator",
            &preview_args(&mappings),
            &HashSet::new(),
        );
        let confirmation = preview["result"]["confirmation"]
            .as_str()
            .expect("confirmation")
            .to_string();
        let expected_locks = mappings
            .iter()
            .flat_map(|mapping| [&mapping.predecessor_id, &mapping.replacement_id])
            .collect::<HashSet<_>>()
            .len();
        let (locks_ready_tx, locks_ready_rx) = mpsc::channel();
        let observed_locks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_locks_for_hook = std::sync::Arc::clone(&observed_locks);
        crate::tasks::board_router::install_after_task_id_lock_hook_for_test(&home, move |_, _| {
            if observed_locks_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
                == expected_locks
            {
                locks_ready_tx.send(()).expect("all task locks observer");
            }
        });
        let (partial_observed_tx, partial_observed_rx) = mpsc::channel();
        super::install_after_partial_durable_proof_hook_for_test(&home, move || {
            partial_observed_tx
                .send(())
                .expect("partial proof observer");
        });
        let (partial_tx, partial_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        crate::task_events::catalog::install_after_first_event_append_hook_for_test(
            &home,
            move || {
                partial_tx.send(()).expect("partial append observer");
                release_rx.recv().expect("release partial append");
            },
        );
        let home_a = home.clone();
        let token_a = confirmation.clone();
        let first = std::thread::spawn(move || {
            handle_with_live_instances(
                &home_a,
                "operator",
                &serde_json::json!({
                    "action": "orphan_reconcile_apply",
                    "confirmation": token_a,
                }),
                &HashSet::new(),
            )
        });
        locks_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first apply must hold every task-ID lock");
        partial_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("winner must expose a synced partial append");
        let home_b = home.clone();
        let second = std::thread::spawn(move || {
            handle_with_live_instances(
                &home_b,
                "operator",
                &serde_json::json!({
                    "action": "orphan_reconcile_apply",
                    "confirmation": confirmation,
                }),
                &HashSet::new(),
            )
        });
        let partial_proof_observed = partial_observed_rx
            .recv_timeout(Duration::from_millis(100))
            .is_ok();
        release_tx.send(()).expect("release winner");
        assert!(
            !partial_proof_observed,
            "losing retry must not inspect a partial proof before acquiring the winner's task locks"
        );
        let first = first.join().expect("first apply");
        let second = second.join().expect("second apply");
        let responses = [first, second];
        assert_eq!(
            responses
                .iter()
                .filter(|response| response["result"]["applied"] == 7)
                .count(),
            1,
            "exactly one concurrent apply must append the batch: {responses:?}"
        );
        assert_eq!(
            responses
                .iter()
                .filter(|response| response["result"]["already_applied"] == true)
                .count(),
            1,
            "the losing retry must be idempotent: {responses:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn proofless_supersession_round_trips_and_unknown_proof_fields_fail_closed() {
        let proofless = serde_json::json!({
            "kind": "Superseded",
            "task_id": "t-predecessor",
            "by": "operator",
            "successor_id": "t-successor",
        });
        let event: crate::task_events::TaskEvent =
            serde_json::from_value(proofless).expect("proofless historical event remains valid");
        assert!(matches!(
            event,
            crate::task_events::TaskEvent::Superseded { proof: None, .. }
        ));
        let unknown = serde_json::json!({
            "kind": "Superseded",
            "task_id": "t-predecessor",
            "by": "operator",
            "successor_id": "t-successor",
            "proof": {
                "operation_id": "op",
                "preview_digest": "digest",
                "mapping_digest": "mapping",
                "board": BOARD_PROJECT,
                "audit_reason": "reason",
                "predecessor_created_at": "2026-01-01T00:00:00Z",
                "replacement_created_at": "2026-01-01T00:00:00Z",
                "replacement_terminal_event_instance": "operator",
                "replacement_terminal_event_seq": 1,
                "replacement_record_sha256": "record",
                "replacement_evidence_sha256": "evidence",
                "actor_digest": "actor",
                "unexpected": true,
            }
        });
        assert!(serde_json::from_value::<crate::task_events::TaskEvent>(unknown).is_err());
    }
}
