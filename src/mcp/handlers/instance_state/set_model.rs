//! #2744 PR-A: `set_model` — typed, fleet-scoped explicit model intent.
//!
//! Contract (decision d-20260712101306674407-19): exactly one of
//! `model`/`tier` (handler-enforced); persists ONLY to the target instance's
//! fleet.yaml entry; atomically sets one field and clears the other in a
//! single lock/write transaction; every false/skip persistence outcome is a
//! hard tool error; default no restart (`restart:true` opts in, and a restart
//! failure after a durable persist reports `persisted:true, restart_ok:false`
//! — never a rollback); ACL = instance management (anonymous/operator full
//! authority; identified caller must be the instance itself, its team
//! orchestrator, or its creator); capability keys off the DECLARED backend
//! (Shell/Raw/custom hard-error); pre-existing model-flag spellings in the
//! entry's args are hard conflicts (parser-aware; no automatic argv
//! rewriting).

use serde_json::{json, Value};
use std::path::Path;

pub(crate) fn handle_set_model(
    home: &Path,
    args: &Value,
    sender: &Option<crate::identity::Sender>,
) -> Value {
    let name = match crate::mcp::handlers::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);

    // ACL — same ladder as handle_delete_instance (AUDIT2-002): anonymous
    // (operator-direct) keeps full authority; an identified caller must be
    // the instance itself, its team orchestrator, or its creator.
    let actor = sender
        .as_ref()
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "operator".to_string());
    if let Some(caller) = sender.as_ref().map(|s| s.as_str()) {
        if caller != name && !crate::teams::is_orchestrator_of(home, caller, name) {
            let is_creator = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.created_by.clone()))
                .as_deref()
                == Some(caller);
            if !is_creator {
                return json!({
                    "error": format!(
                        "permission denied: '{caller}' cannot set_model '{name}' \
                         (only the instance itself, its team orchestrator, or its creator may)"
                    ),
                    "code": "not_owner_or_orchestrator"
                });
            }
        }
    }

    let model = args["model"].as_str().filter(|s| !s.is_empty());
    let tier = args["tier"].as_str().filter(|s| !s.is_empty());
    let (set_field, set_val, clear_field) = match (model, tier) {
        (Some(m), None) => ("model", m, "model_tier"),
        (None, Some(t)) => ("model_tier", t, "model"),
        _ => {
            return json!({
                "error": "set_model requires exactly one of `model` or `tier`",
                "code": "exactly_one_required"
            })
        }
    };

    let fleet = match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        Ok(f) => f,
        Err(e) => return json!({"error": format!("fleet.yaml load failed: {e}")}),
    };
    let Some(inst) = fleet.instances.get(name) else {
        return json!({
            "error": format!("unknown instance '{name}' — no fleet.yaml entry"),
            "code": "instance_entry_missing"
        });
    };

    // Capability keys off the DECLARED backend (entry, else fleet default) —
    // never a command string. Shell/Raw/custom = no declared capability.
    let declared = inst
        .backend
        .clone()
        .or_else(|| fleet.defaults.backend.clone())
        .unwrap_or(crate::backend::Backend::ClaudeCode);
    let Some(cap) = declared.model_capability() else {
        return json!({
            "error": format!(
                "backend '{}' declares no model capability — set_model is \
                 unsupported for it (an adapter must opt in explicitly)",
                declared.name()
            ),
            "code": "no_model_capability"
        });
    };

    if let Some(t) = tier {
        if fleet.model_tiers.get(t).is_none_or(|m| m.is_empty()) {
            return json!({
                "error": format!(
                    "unknown model tier '{t}' — no non-empty fleet.yaml model_tiers entry"
                ),
                "code": "unknown_tier"
            });
        }
    }

    // Parser-aware conflict scan over the args the entry would spawn with
    // (instance args, else defaults — same precedence as resolve_instance).
    // Confirmed spellings and conservative ambiguous tokens both REJECT: the
    // contract forbids returning success while args would win, and forbids
    // automatic argv rewriting.
    let scan_args: &[String] = if inst.args.is_empty() {
        &fleet.defaults.args
    } else {
        &inst.args
    };
    if let Some(hit) = cap.scan(scan_args).into_iter().next() {
        return match hit {
            crate::backend::ModelFlagHit::Confirmed(tok) => json!({
                "error": format!(
                    "instance '{name}' args already pin an explicit model flag ('{tok}') — \
                     args win over fleet intent, so set_model would be a silent no-op. \
                     Remove the flag from the entry's args, then retry."
                ),
                "code": "args_conflict_confirmed"
            }),
            crate::backend::ModelFlagHit::Ambiguous(tok) => json!({
                "error": format!(
                    "instance '{name}' args carry an ambiguous model-flag-like token ('{tok}'). \
                     If it is payload text, move it after a bare `--` delimiter; if it is a \
                     model flag, remove it — then retry."
                ),
                "code": "args_conflict_ambiguous"
            }),
        };
    }

    let old_model = inst.model.clone();
    let old_tier = inst.model_tier.clone();
    match crate::fleet::persist::update_instance_fields(
        home,
        name,
        &[(set_field, serde_yaml_ng::Value::String(set_val.to_string()))],
        &[clear_field],
    ) {
        Ok(true) => {}
        Ok(false) => {
            return json!({
                "error": format!(
                    "persist failed for '{name}': fleet.yaml entry missing or malformed \
                     (see daemon warn log for the skip reason)"
                ),
                "code": "persist_skipped"
            })
        }
        Err(e) => return json!({"error": format!("persist failed: {e}"), "code": "persist_error"}),
    }

    // Durable audit — event log, not just tracing. No secrets: model ids /
    // tier keys + actor + old→new + cleared field + source.
    crate::event_log::log(
        home,
        "set_model",
        name,
        &format!(
            "{set_field}={set_val} cleared={clear_field} \
             was_model={old_model:?} was_tier={old_tier:?} by={actor} source=set_model"
        ),
    );

    let mut resp = json!({
        "ok": true,
        "persisted": true,
        "set": {set_field: set_val},
        "cleared": clear_field,
        "note": "takes effect on the next respawn"
    });
    // Restart only AFTER the durable persist. A restart failure must not
    // roll back or mask the persist: persisted:true + restart_ok:false.
    if args["restart"].as_bool() == Some(true) {
        let r = super::handle_restart_instance(
            home,
            &json!({"instance": name, "mode": "resume", "reason": "set_model"}),
        );
        let restart_ok = r.get("error").is_none_or(Value::is_null) && r["spawned"] == json!(true);
        resp["restart_ok"] = json!(restart_ok);
        if restart_ok {
            resp["note"] = json!("restarted — new model intent active");
        } else {
            resp["restart_error"] = json!(format!(
                "restart failed ({}) — the persisted intent still applies on the next respawn",
                r["error"].as_str().unwrap_or("spawn did not complete")
            ));
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_home(tag: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-2744-setmodel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");
        home
    }

    fn write_fleet(home: &Path, yaml: &str) {
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).expect("write fleet.yaml");
    }

    fn fleet_text(home: &Path) -> String {
        std::fs::read_to_string(crate::fleet::fleet_yaml_path(home)).expect("read fleet.yaml")
    }

    fn sender(name: &str) -> Option<crate::identity::Sender> {
        Some(crate::identity::Sender::new(name).expect("sender"))
    }

    const CODEX_SEAT: &str =
        "model_tiers:\n  cheap: claude-haiku-4-5\ninstances:\n  seat:\n    backend: codex\n    model_tier: cheap\n";

    /// T2: exactly one of model/tier — both or neither is a hard error.
    #[test]
    fn set_model_enforces_exactly_one_of_model_or_tier_2744() {
        let home = test_home("exclusive");
        write_fleet(&home, CODEX_SEAT);
        for bad in [
            json!({"instance": "seat", "model": "o3", "tier": "cheap"}),
            json!({"instance": "seat"}),
        ] {
            let r = handle_set_model(&home, &bad, &None);
            assert!(
                r["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("exactly one")),
                "want exactly-one error, got {r}"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T5: unknown instance is a hard error (no silent no-op — the
    /// `update_instance_field` Ok(false) class must surface).
    #[test]
    fn set_model_unknown_instance_hard_errors_2744() {
        let home = test_home("unknown");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(&home, &json!({"instance": "ghost", "model": "o3"}), &None);
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("ghost")),
            "want unknown-instance error, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// G4: declared Shell/Raw have no model capability — hard error, and the
    /// yaml is untouched.
    #[test]
    fn set_model_shell_raw_hard_error_2744() {
        let home = test_home("shellraw");
        write_fleet(
            &home,
            "instances:\n  sh-seat:\n    backend: shell\n  raw-seat:\n    backend: /opt/custom/bin\n",
        );
        for seat in ["sh-seat", "raw-seat"] {
            let r = handle_set_model(&home, &json!({"instance": seat, "model": "x"}), &None);
            assert!(
                r["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("model capability")),
                "{seat}: want capability error, got {r}"
            );
        }
        // FleetConfig::load side-stamps `id:` fields, so the file is not
        // byte-stable — the contract is that no model intent was written.
        assert!(
            !fleet_text(&home).contains("model:"),
            "no model key may be written on the error path"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T2: a tier must exist (non-empty) in model_tiers at write time.
    #[test]
    fn set_model_unknown_tier_hard_errors_2744() {
        let home = test_home("badtier");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(&home, &json!({"instance": "seat", "tier": "nope"}), &None);
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("nope")),
            "want unknown-tier error, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 confirmed conflict: an explicit model flag in the entry's args
    /// REJECTS (no rewrite, no success-while-args-wins).
    #[test]
    fn set_model_rejects_confirmed_args_conflict_2744() {
        let home = test_home("confirmed");
        write_fleet(
            &home,
            "instances:\n  seat:\n    backend: codex\n    args:\n      - -m\n      - pinned\n",
        );
        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["code"], "args_conflict_confirmed", "got {r}");
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("-m")),
            "error must name the conflicting token, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 ambiguous conflict: a glued `-mVAL` token on a `-m`-declaring
    /// backend rejects with the AMBIGUOUS class and a disambiguation hint
    /// (move payload after `--`).
    #[test]
    fn set_model_rejects_ambiguous_args_conflict_with_hint_2744() {
        let home = test_home("ambiguous");
        write_fleet(
            &home,
            "instances:\n  seat:\n    backend: codex\n    args:\n      - -mpinned\n",
        );
        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["code"], "args_conflict_ambiguous", "got {r}");
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("--")),
            "error must hint at the -- delimiter, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 false-positive pins: `-m` on a long-flag-only backend (claude) and
    /// model-flag lookalikes AFTER a bare `--` are NOT conflicts.
    #[test]
    fn set_model_no_false_positive_conflicts_2744() {
        let home = test_home("falsepos");
        write_fleet(
            &home,
            "instances:\n  cl:\n    backend: claude\n    args:\n      - -m\n      - notaflag\n  cx:\n    backend: codex\n    args:\n      - --\n      - --model\n      - payload\n",
        );
        for seat in ["cl", "cx"] {
            let r = handle_set_model(
                &home,
                &json!({"instance": seat, "model": "claude-opus-4-8"}),
                &None,
            );
            assert!(
                r["error"].is_null(),
                "{seat}: lookalike token must not conflict, got {r}"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Core semantics: set model clears model_tier (and vice versa) in the
    /// SAME write; the response reports what was set and cleared.
    #[test]
    fn set_model_atomically_clears_tier_and_vice_versa_2744() {
        let home = test_home("mutualclear");
        write_fleet(&home, CODEX_SEAT);

        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["persisted"], true, "got {r}");
        let text = fleet_text(&home);
        assert!(text.contains("model: o3"), "model must persist: {text}");
        assert!(
            !text.contains("model_tier: cheap"),
            "the entry's model_tier must be cleared in the same write: {text}"
        );

        let r = handle_set_model(&home, &json!({"instance": "seat", "tier": "cheap"}), &None);
        assert_eq!(r["persisted"], true, "got {r}");
        let text = fleet_text(&home);
        assert!(
            text.contains("model_tier: cheap"),
            "tier must persist: {text}"
        );
        assert!(
            !text.contains("model: o3"),
            "model must be cleared in the same write: {text}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// ACL: identified unrelated caller is denied; the instance itself is
    /// allowed; anonymous (operator-direct) is allowed.
    #[test]
    fn set_model_acl_matches_instance_management_2744() {
        let home = test_home("acl");
        write_fleet(&home, CODEX_SEAT);
        let params = json!({"instance": "seat", "model": "o3"});

        let r = handle_set_model(&home, &params, &sender("bystander"));
        assert!(
            r["error"]
                .as_str()
                .is_some_and(|e| e.contains("permission denied")),
            "unrelated caller must be denied, got {r}"
        );

        let r = handle_set_model(&home, &params, &sender("seat"));
        assert_eq!(r["persisted"], true, "self must be allowed, got {r}");

        let r = handle_set_model(&home, &json!({"instance": "seat", "tier": "cheap"}), &None);
        assert_eq!(r["persisted"], true, "operator must be allowed, got {r}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// ACL: the team orchestrator of the target IS allowed.
    #[test]
    fn set_model_acl_allows_team_orchestrator_2744() {
        let home = test_home("aclorch");
        write_fleet(
            &home,
            "model_tiers:\n  cheap: claude-haiku-4-5\nteams:\n  crew:\n    orchestrator: boss\n    members:\n      - seat\ninstances:\n  boss:\n    backend: claude\n  seat:\n    backend: codex\n",
        );
        let r = handle_set_model(
            &home,
            &json!({"instance": "seat", "model": "o3"}),
            &sender("boss"),
        );
        assert_eq!(r["persisted"], true, "orchestrator allowed, got {r}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// restart=true fires only AFTER durable persistence; a restart failure
    /// (instance not running here) must NOT roll back or mask the persist:
    /// `persisted:true, restart_ok:false` + actionable error.
    #[test]
    fn set_model_restart_failure_reports_persisted_true_2744() {
        let home = test_home("restartfail");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(
            &home,
            &json!({"instance": "seat", "model": "o3", "restart": true}),
            &None,
        );
        assert_eq!(r["persisted"], true, "persist must survive, got {r}");
        assert_eq!(
            r["restart_ok"], false,
            "restart must report failure, got {r}"
        );
        assert!(
            fleet_text(&home).contains("model: o3"),
            "no rollback on restart failure"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Durable audit: a successful write appends a set_model event to the
    /// daemon event log (not just a tracing line).
    #[test]
    fn set_model_writes_durable_audit_event_2744() {
        let home = test_home("audit");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["persisted"], true, "got {r}");
        let hits: Vec<String> = walkdir_jsonl(&home)
            .into_iter()
            .filter(|l| l.contains("set_model") && l.contains("seat"))
            .collect();
        assert!(
            !hits.is_empty(),
            "expected a durable set_model audit line under {home:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Collect every line of every .jsonl file under home (audit lives in the
    /// daemon event log; the exact filename is an implementation detail).
    fn walkdir_jsonl(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "jsonl") {
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        out.extend(text.lines().map(String::from));
                    }
                }
            }
        }
        out
    }
}
