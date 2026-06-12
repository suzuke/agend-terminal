use crate::agent_ops::{list_agents, merge_metadata};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_list_instances(home: &Path, args: &Value, instance_name: &str) -> Value {
    // If `instance` param is provided, return detailed info for that instance (replaces describe_instance)
    if let Some(target) = args["instance"].as_str().filter(|s| !s.is_empty()) {
        return handle_describe_instance(home, &json!({"name": target}));
    }
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
                        info
                    })
                    .collect();
                json!({"instances": instances})
            } else {
                json!({"instances": list_agents()})
            }
        }
        Err(_) => json!({"instances": list_agents()}),
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
