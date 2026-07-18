use crate::agent_ops::{list_agents, merge_metadata};
use crate::mcp::handlers::dispatch::RuntimeContext;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_list_instances_with_runtime(
    home: &Path,
    args: &Value,
    instance_name: &str,
    runtime: Option<&RuntimeContext>,
) -> Value {
    // If `instance` param is provided, return detailed info for that instance (replaces describe_instance)
    if let Some(target) = args["instance"].as_str().filter(|s| !s.is_empty()) {
        return handle_describe_instance(home, &json!({"name": target}));
    }
    // #2475: compact-by-default for routine polling. The API LIST response still
    // carries full `observed_status.evidence` for dashboards / describe paths;
    // the MCP `list_instances` read tool strips that evidence unless explicitly
    // requested, because agents poll this often and the evidence array is noisy
    // context ballast. Set `verbose:true` or `include_evidence:true` for detail.
    let include_evidence = args["verbose"].as_bool().unwrap_or(false)
        || args["include_evidence"].as_bool().unwrap_or(false);
    let Some(runtime) = runtime else {
        return with_operator_mode(json!({"instances": list_agents(), "compact": true}));
    };
    let resp = crate::api::list_response(home, &runtime.registry, &runtime.externals);
    if let Some(agents) = resp["result"]["agents"].as_array() {
        // #991 PR-C: load fleet.yaml ONCE for the whole batch (not per-agent)
        // so operators can grep "which agents intentionally have no topic"
        // via `list_instances` — previously only `describe_instance`
        // (single-instance) exposed this field.
        let fleet_config =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
        let instances: Vec<Value> = agents
            .iter()
            .filter(|a| {
                let backend = a["backend"].as_str().unwrap_or("");
                crate::backend::Backend::from_command(backend).is_some()
            })
            .map(|a| {
                let mut info = a.clone();
                let name = a["name"].as_str().unwrap_or("");
                merge_metadata(home, name, &mut info);
                if name == instance_name {
                    info["is_self"] = json!(true);
                }
                // Omit when unset (auto default) — matches describe_instance's
                // existing omission convention, not an explicit "auto" value.
                if let Some(mode) = fleet_config
                    .as_ref()
                    .and_then(|c| c.instances.get(name))
                    .and_then(|i| i.topic_binding_mode.as_ref())
                {
                    info["topic_binding_mode"] = json!(mode);
                }
                if !include_evidence {
                    strip_observed_evidence(&mut info);
                }
                info
            })
            .collect();
        with_operator_mode(json!({"instances": instances, "compact": !include_evidence}))
    } else {
        with_operator_mode(json!({"instances": list_agents(), "compact": true}))
    }
}

/// #2548 PR-2: fold the retired `mode` MCP tool's read side into
/// `list_instances` — agents already poll this to observe fleet state, and
/// the operator-availability mode belongs alongside it (back off when the
/// operator is away/asleep). Setting the mode stays CLI-only
/// (`agend-terminal mode <active|away|sleep>`); this is read-only.
fn with_operator_mode(mut result: Value) -> Value {
    let state = crate::operator_mode::get();
    result["operator_mode"] = json!({
        "mode": state.mode,
        "delegate_to": state.delegate_to,
        "delegate_scope": state.delegate_scope,
    });
    result
}

fn strip_observed_evidence(info: &mut Value) {
    if let Some(obs) = info
        .get_mut("observed_status")
        .and_then(Value::as_object_mut)
    {
        obs.remove("evidence");
    }
}

pub(super) fn handle_describe_instance(home: &Path, args: &Value) -> Value {
    let name = args["name"].as_str().unwrap_or("");
    crate::validate_name_or_err!(name);
    match crate::api::call(home, &json!({"method": crate::api::method::LIST})) {
        Ok(resp) => {
            match resp["result"]["agents"]
                .as_array()
                .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(name)))
            {
                Some(agent) => {
                    let mut info = agent.clone();
                    merge_metadata(home, name, &mut info);
                    // Surface topic_id from topics.json + topic_binding_mode from fleet.yaml.
                    if info.get("topic_id").is_none() {
                        if let Some(tid) =
                            crate::channel::telegram::lookup_topic_for_instance(home, name)
                        {
                            info["topic_id"] = json!(tid);
                        }
                    }
                    if let Some(inst) =
                        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                            .ok()
                            .and_then(|c| c.instances.get(name).cloned())
                    {
                        if let Some(ref mode) = inst.topic_binding_mode {
                            info["topic_binding_mode"] = json!(mode);
                        }
                    }
                    json!({"instance": info})
                }
                None => json!({"error": format!("Instance '{name}' not found")}),
            }
        }
        Err(e) => json!({"error": format!("API unavailable: {e}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn strip_observed_evidence_drops_only_evidence_2475() {
        let mut info = json!({
            "name": "dev-1",
            "agent_state": "idle",
            "observed_status": {
                "state": "Active",
                "confidence": "Strong",
                "evidence": [{"a": 1}, {"a": 2}, {"a": 3}]
            }
        });
        strip_observed_evidence(&mut info);
        let obs = &info["observed_status"];
        assert!(obs["evidence"].is_null(), "evidence must be stripped");
        assert_eq!(obs["state"], "Active", "non-evidence fields preserved");
        assert_eq!(info["agent_state"], "idle");
    }

    #[test]
    fn strip_observed_evidence_noop_without_observed_status_2475() {
        let mut info = json!({"name": "dev-2", "agent_state": "idle"});
        strip_observed_evidence(&mut info); // must not panic / must be inert
        assert_eq!(info["name"], "dev-2");
    }

    #[test]
    fn list_instances_includes_operator_mode_2548() {
        // #2548 PR-2: the retired `mode` tool's read side folds into
        // list_instances. Assert shape only (not a specific mode value) —
        // operator_mode is process-global state other tests may mutate
        // concurrently in the same test binary.
        let home = std::env::temp_dir().join(format!("agend-list-opmode-{}", std::process::id()));
        let result = handle_list_instances_with_runtime(&home, &json!({}), "caller", None);
        let om = &result["operator_mode"];
        assert!(
            om.is_object(),
            "list_instances must surface operator_mode: {result}"
        );
        assert!(
            om["mode"].is_string(),
            "operator_mode.mode must be a string: {result}"
        );
        assert!(
            om["delegate_scope"].is_array(),
            "operator_mode.delegate_scope must be an array: {result}"
        );
    }

    #[test]
    fn list_instances_uses_runtime_registry_without_api_loopback_2454() {
        let home = std::env::temp_dir().join(format!("agend-list-runtime-{}", std::process::id()));
        let registry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let externals = Arc::new(parking_lot::Mutex::new(HashMap::from([(
            "external-one".to_string(),
            crate::agent::ExternalAgentHandle {
                backend_command: "codex".to_string(),
                pid: 4242,
            },
        )])));
        let runtime = RuntimeContext {
            registry,
            externals,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: None,
        };

        let result =
            handle_list_instances_with_runtime(&home, &json!({}), "caller", Some(&runtime));
        let instances = result["instances"]
            .as_array()
            .expect("instances array from runtime path");
        assert!(
            instances
                .iter()
                .any(|agent| agent["name"].as_str() == Some("external-one")),
            "runtime registry result should be returned without depending on the API socket: {result}"
        );
    }

    fn runtime_with_external_one() -> RuntimeContext {
        RuntimeContext {
            registry: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            externals: Arc::new(parking_lot::Mutex::new(HashMap::from([(
                "external-one".to_string(),
                crate::agent::ExternalAgentHandle {
                    backend_command: "codex".to_string(),
                    pid: 4242,
                },
            )]))),
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: None,
        }
    }

    /// #991 PR-C: `list_instances` must expose `topic_binding_mode` (the
    /// original acceptance criterion literally names `list_instances`, not
    /// just `describe_instance` — see BIND-TOPIC-PRERESEARCH.md §1) so an
    /// operator can grep the whole fleet in one call for intentionally
    /// topic-less agents.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn list_instances_exposes_topic_binding_mode_991() {
        let home = std::env::temp_dir().join(format!(
            "agend-list-topic-binding-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  external-one:\n    backend: codex\n    topic_binding_mode: skip\n",
        )
        .unwrap();
        let runtime = runtime_with_external_one();

        let result =
            handle_list_instances_with_runtime(&home, &json!({}), "caller", Some(&runtime));
        let instances = result["instances"].as_array().expect("instances array");
        let agent = instances
            .iter()
            .find(|a| a["name"].as_str() == Some("external-one"))
            .expect("external-one present");
        assert_eq!(
            agent["topic_binding_mode"], "skip",
            "list_instances must surface topic_binding_mode: {agent}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #991 PR-C companion: omitting `topic_binding` in fleet.yaml (the auto
    /// default) must NOT surface a `topic_binding_mode` key at all — matches
    /// `describe_instance`'s existing omission convention (not an explicit
    /// `"auto"` string value).
    #[test]
    #[allow(clippy::unwrap_used)]
    fn list_instances_omits_topic_binding_mode_when_unset_991() {
        let home = std::env::temp_dir().join(format!(
            "agend-list-topic-binding-omit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  external-one:\n    backend: codex\n",
        )
        .unwrap();
        let runtime = runtime_with_external_one();

        let result =
            handle_list_instances_with_runtime(&home, &json!({}), "caller", Some(&runtime));
        let instances = result["instances"].as_array().expect("instances array");
        let agent = instances
            .iter()
            .find(|a| a["name"].as_str() == Some("external-one"))
            .expect("external-one present");
        assert!(
            agent.get("topic_binding_mode").is_none(),
            "unset topic_binding must NOT surface a topic_binding_mode key: {agent}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #2454 S3 RED: `describe_instance` (the `instance` param path in
    /// `list_instances`) must use the supplied RuntimeContext and succeed
    /// with NO daemon / API listener.  Currently it delegates to
    /// `handle_describe_instance` which calls `api::call` — this test
    /// fails until the in-process path is wired.
    #[test]
    fn describe_instance_uses_runtime_context_without_api_listener_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-describe-runtime-2454-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let registry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("target-agent");
        registry.lock().insert(handle.id, handle);
        let externals = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let runtime = RuntimeContext {
            registry,
            externals,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: None,
        };

        let result = handle_list_instances_with_runtime(
            &home,
            &json!({"instance": "target-agent"}),
            "caller",
            Some(&runtime),
        );
        assert!(
            result.get("error").is_none(),
            "describe via runtime must succeed without a daemon; got: {result}"
        );
        let inst = &result["instance"];
        assert_eq!(
            inst["name"].as_str(),
            Some("target-agent"),
            "describe must return the target instance from the runtime registry: {result}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #2454 S3 RED source invariant: production code in this module must
    /// contain ZERO `crate::api::call` invocations.  Any loopback in the
    /// instance-query path is a regression.
    #[test]
    fn no_production_api_call_in_instance_queries_2454() {
        let source = include_str!("instance_queries.rs");
        let needle_a = concat!("crate::", "api::", "call");
        let needle_b = concat!("api::", "call(");
        let in_test_mod = source.find("#[cfg(test)]").unwrap_or(source.len());
        let production = &source[..in_test_mod];
        let production_calls: Vec<(usize, &str)> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let trimmed = line.trim();
                !trimmed.starts_with("//") && !trimmed.starts_with("///")
            })
            .filter(|(_, line)| line.contains(needle_a) || line.contains(needle_b))
            .collect();
        assert!(
            production_calls.is_empty(),
            "instance_queries.rs must contain zero production api::call sites; found {}: {:?}",
            production_calls.len(),
            production_calls
        );
    }
}
