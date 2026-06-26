use crate::agent_ops::{list_agents, merge_metadata};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_list_instances(home: &Path, args: &Value, instance_name: &str) -> Value {
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
    match crate::api::call(home, &json!({"method": crate::api::method::LIST})) {
        Ok(resp) => {
            if let Some(agents) = resp["result"]["agents"].as_array() {
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
                        if !include_evidence {
                            strip_observed_evidence(&mut info);
                        }
                        info
                    })
                    .collect();
                json!({"instances": instances, "compact": !include_evidence})
            } else {
                json!({"instances": list_agents(), "compact": true})
            }
        }
        Err(_) => json!({"instances": list_agents(), "compact": true}),
    }
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
}
