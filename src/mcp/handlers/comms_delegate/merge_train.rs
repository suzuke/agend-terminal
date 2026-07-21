use crate::mcp::handlers::dispatch_hook;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) enum Admission {
    NotGoverned,
    Front,
    Queued(Value),
}

pub(crate) fn admit(
    home: &Path,
    args: &Value,
    target: &str,
    is_review: bool,
) -> Result<Admission, Value> {
    if is_review {
        return Ok(Admission::NotGoverned);
    }
    if args["branch"].as_str().filter(|b| !b.is_empty()).is_none() {
        return Ok(Admission::NotGoverned);
    }
    let task_id = match args["task_id"].as_str().filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => return Ok(Admission::NotGoverned),
    };

    let repo_identity = resolve_repo(home, args, target)?;

    let routed = load_task(home, task_id)?;
    let initial_domain = conflict_domain(routed.record());
    let lock_key = lock_key(&repo_identity, &initial_domain);
    let lock_dir = home.join("merge_train");
    std::fs::create_dir_all(&lock_dir).map_err(|e| {
        json!({
            "ok": false,
            "error": format!("merge train lock directory: {e}"),
            "code": "merge_train_lock_error",
        })
    })?;
    let _lock = crate::store::acquire_file_lock(&lock_dir.join(format!("{lock_key}.lock")))
        .map_err(|e| {
            json!({
                "ok": false,
                "error": format!("merge train lock: {e}"),
                "code": "merge_train_lock_error",
            })
        })?;

    // All authority below is freshly replayed under the repo/domain lock. The
    // preliminary route above exists only to identify which outer lock to take.
    let routed = load_task(home, task_id)?;
    let domain = conflict_domain(routed.record());
    if domain != initial_domain {
        return Err(json!({
            "ok": false,
            "error": format!("merge train: task '{task_id}' conflict domain changed while acquiring admission lock"),
            "code": "merge_train_domain_changed",
        }));
    }

    match TrainState::read(routed.record()) {
        TrainState::None => {}
        TrainState::Partial(n) => {
            return Err(json!({
                "ok": false,
                "error": format!("merge train: task '{task_id}' has partial train metadata ({n}/4 keys)"),
                "code": "merge_train_partial_metadata",
            }));
        }
        TrainState::Complete {
            repo,
            domain: dom,
            position,
            seq,
        } => {
            if !matches!(position.as_str(), "Front" | "Queued") {
                return Err(json!({
                    "ok": false,
                    "error": format!("merge train: task '{task_id}' has invalid stored position '{position}'"),
                    "code": "merge_train_invalid_position",
                }));
            }
            if repo != repo_identity || dom != domain {
                return Err(json!({
                    "ok": false,
                    "error": format!("merge train: task '{task_id}' metadata mismatch: existing repo={repo} domain={dom} vs canonical repo={repo_identity} domain={domain}"),
                    "code": "merge_train_metadata_mismatch",
                }));
            }
            return Ok(if position == "Front" {
                Admission::Front
            } else {
                Admission::Queued(json!({
                    "ok": true,
                    "merge_train_position": "Queued",
                    "merge_train_queue_seq": seq,
                    "merge_train_repository": repo_identity,
                    "merge_train_domain": domain,
                }))
            });
        }
    }

    let (fronts, seq) = crate::tasks::scan_merge_train_state(home, &repo_identity, &domain)
        .map_err(|e| {
            json!({
                "ok": false,
                "error": format!("merge train durable authority scan: {e}"),
                "code": "merge_train_board_replay_error",
            })
        })?;
    let position = if fronts.is_empty() { "Front" } else { "Queued" };

    crate::tasks::write_merge_train_metadata(
        home,
        task_id,
        &[
            ("merge_train_repository", json!(repo_identity)),
            ("merge_train_domain", json!(domain)),
            ("merge_train_position", json!(position)),
            ("merge_train_queue_seq", json!(seq)),
        ],
    )?;

    if position == "Queued" {
        Ok(Admission::Queued(json!({
            "ok": true,
            "merge_train_position": "Queued",
            "merge_train_queue_seq": seq,
            "merge_train_repository": repo_identity,
            "merge_train_domain": domain,
        })))
    } else {
        Ok(Admission::Front)
    }
}

fn load_task(home: &Path, task_id: &str) -> Result<crate::tasks::RoutedTask, Value> {
    crate::tasks::load_routed(home, task_id).map_err(|e| {
        use crate::tasks::TaskRouteError;
        match e {
            TaskRouteError::NotFound => json!({
                "ok": false,
                "error": format!("merge train: task '{task_id}' not found on any board"),
                "code": "merge_train_task_not_found",
            }),
            TaskRouteError::Ambiguous { .. } => json!({
                "ok": false,
                "error": format!("merge train: task '{task_id}' ambiguous across boards"),
                "code": "merge_train_ambiguous_task",
            }),
            _ => json!({
                "ok": false,
                "error": format!("merge train: task '{task_id}' route error: {e}"),
                "code": "merge_train_route_error",
            }),
        }
    })
}

fn resolve_repo(home: &Path, args: &Value, target: &str) -> Result<String, Value> {
    let unresolved = || {
        json!({
            "ok": false,
            "error": "merge train: could not resolve canonical repo identity — \
                      set `repository=owner/repo` or a team `source_repo`",
            "code": "merge_train_repo_unresolved",
        })
    };
    if let Some(repo) = args["repository"].as_str().filter(|s| !s.is_empty()) {
        return dispatch_hook::canonicalize_repo_slug_any_forge(repo)
            .map(|slug| format!("forge:{slug}"))
            .ok_or_else(unresolved);
    }
    let source_repo = dispatch_hook::resolve_source_repo_for_target(home, target);
    let canonical = std::fs::canonicalize(&source_repo).map_err(|_| unresolved())?;
    Ok(
        match dispatch_hook::canonical_repo_slug_for_source(&canonical) {
            Some(slug) => format!("forge:{slug}"),
            None => format!("path:{}", canonical.display()),
        },
    )
}

fn conflict_domain(record: &crate::task_events::TaskRecord) -> String {
    record
        .metadata
        .get("conflict_domain")
        .and_then(Value::as_str)
        .unwrap_or("__repo__")
        .to_string()
}

fn lock_key(repo: &str, domain: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(repo.as_bytes());
    digest.update([0]);
    digest.update(domain.as_bytes());
    hex::encode(digest.finalize())
}

enum TrainState {
    None,
    Partial(usize),
    Complete {
        repo: String,
        domain: String,
        position: String,
        seq: u64,
    },
}

impl TrainState {
    fn read(record: &crate::task_events::TaskRecord) -> Self {
        let repo = record
            .metadata
            .get("merge_train_repository")
            .and_then(|v| v.as_str());
        let domain = record
            .metadata
            .get("merge_train_domain")
            .and_then(|v| v.as_str());
        let position = record
            .metadata
            .get("merge_train_position")
            .and_then(|v| v.as_str());
        let seq = record
            .metadata
            .get("merge_train_queue_seq")
            .and_then(|v| v.as_u64());
        let count = [
            repo.is_some(),
            domain.is_some(),
            position.is_some(),
            seq.is_some(),
        ]
        .iter()
        .filter(|&&x| x)
        .count();
        match (repo, domain, position, seq) {
            (None, None, None, None) => Self::None,
            (Some(r), Some(d), Some(p), Some(s)) => Self::Complete {
                repo: r.to_string(),
                domain: d.to_string(),
                position: p.to_string(),
                seq: s,
            },
            _ => Self::Partial(count),
        }
    }
}
