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
    if let Some(target) = args["instance"].as_str().filter(|s| !s.is_empty()) {
        return describe_instance(home, target, runtime);
    }
    let include_evidence = args["verbose"].as_bool().unwrap_or(false)
        || args["include_evidence"].as_bool().unwrap_or(false);
    let Some(runtime) = runtime else {
        return with_operator_mode(json!({"instances": list_agents(), "compact": true}));
    };
    let resp = crate::agent_ops::list_snapshot(home, &runtime.registry, &runtime.externals);
    if let Some(agents) = resp["result"]["agents"].as_array() {
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

fn describe_instance(home: &Path, name: &str, runtime: Option<&RuntimeContext>) -> Value {
    crate::validate_name_or_err!(name);
    let Some(runtime) = runtime else {
        return json!({"error": "runtime context unavailable — describe requires a live registry"});
    };
    let resp = crate::agent_ops::list_snapshot(home, &runtime.registry, &runtime.externals);
    match resp["result"]["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(name)))
    {
        Some(agent) => {
            let mut info = agent.clone();
            merge_metadata(home, name, &mut info);
            if info.get("topic_id").is_none() {
                if let Some(tid) = crate::channel::telegram::lookup_topic_for_instance(home, name) {
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
    /// contain ZERO references to the API query layer — neither socket
    /// loopback (`api::call`) nor in-process API-layer calls
    /// (`api::list_response`).  Instance queries must go through the
    /// neutral agent_ops service, not the API wire layer.
    #[test]
    fn no_production_api_dependency_in_instance_queries_2454() {
        let source = include_str!("instance_queries.rs");
        let in_test_mod = source.find("#[cfg(test)]").unwrap_or(source.len());
        let production = &source[..in_test_mod];
        let needles: &[&str] = &[
            concat!("crate::", "api::", "call"),
            concat!("api::", "call("),
            concat!("crate::", "api::", "list_response"),
            concat!("api::", "list_response("),
        ];
        let violations: Vec<(usize, &str)> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let trimmed = line.trim();
                !trimmed.starts_with("//") && !trimmed.starts_with("///")
            })
            .filter(|(_, line)| needles.iter().any(|n| line.contains(n)))
            .collect();
        assert!(
            violations.is_empty(),
            "instance_queries.rs must contain zero production API-layer references \
             (api::call OR api::list_response); found {}: {:?}",
            violations.len(),
            violations
        );
    }
}
