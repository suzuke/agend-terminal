use crate::mcp::handlers::dispatch_hook;
use serde_json::{json, Value};
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

    let repo_slug = resolve_repo(home, args, target)?;

    let routed = crate::tasks::load_routed(home, task_id).map_err(|e| {
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
    })?;

    let domain = routed
        .record()
        .metadata
        .get("conflict_domain")
        .and_then(|v| v.as_str())
        .unwrap_or("__repo__")
        .to_string();

    let existing = TrainState::read(routed.record());
    match existing {
        TrainState::None => {}
        TrainState::Partial(n) => {
            return Err(json!({
                "ok": false,
                "error": format!(
                    "merge train: task '{task_id}' has partial train metadata ({n}/4 keys)"
                ),
                "code": "merge_train_partial_metadata",
            }));
        }
        TrainState::Complete {
            repo,
            domain: dom,
            position,
            seq,
        } => {
            if repo != repo_slug || dom != domain {
                return Err(json!({
                    "ok": false,
                    "error": format!(
                        "merge train: task '{task_id}' metadata mismatch: \
                         existing repo={repo} domain={dom} vs canonical repo={repo_slug} domain={domain}"
                    ),
                    "code": "merge_train_metadata_mismatch",
                }));
            }
            return Ok(match position.as_str() {
                "Front" => Admission::Front,
                _ => Admission::Queued(json!({
                    "ok": true,
                    "merge_train_position": position,
                    "merge_train_queue_seq": seq,
                    "merge_train_repository": repo_slug,
                    "merge_train_domain": domain,
                })),
            });
        }
    }

    let lock_key = format!(
        "{}__{}",
        sanitize_lock_component(&repo_slug),
        sanitize_lock_component(&domain)
    );
    let lock_dir = home.join("merge_train");
    std::fs::create_dir_all(&lock_dir).ok();
    let _lock = crate::store::acquire_file_lock(&lock_dir.join(format!("{lock_key}.lock")))
        .map_err(|e| {
            json!({
                "ok": false,
                "error": format!("merge train lock: {e}"),
                "code": "merge_train_lock_error",
            })
        })?;

    let fronts = crate::tasks::scan_merge_train_fronts(home, &repo_slug, &domain);
    let seq = crate::tasks::next_merge_train_seq(home, &repo_slug, &domain);
    let position = if fronts.is_empty() { "Front" } else { "Queued" };

    crate::tasks::write_merge_train_metadata(
        home,
        task_id,
        &[
            ("merge_train_repository", json!(repo_slug)),
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
            "merge_train_repository": repo_slug,
            "merge_train_domain": domain,
        })))
    } else {
        Ok(Admission::Front)
    }
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
        return dispatch_hook::canonicalize_repo_slug_any_forge(repo).ok_or_else(unresolved);
    }
    let source_repo =
        dispatch_hook::resolve_team_source_repo(home, target).ok_or_else(unresolved)?;
    dispatch_hook::canonical_repo_slug_for_source(&source_repo).ok_or_else(unresolved)
}

fn sanitize_lock_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
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
