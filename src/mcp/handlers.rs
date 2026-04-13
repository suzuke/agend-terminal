//! MCP tool dispatch — thin routing layer that delegates to crate::ops.

use serde_json::{json, Value};

pub fn handle_tool(tool: &str, args: &Value, _agent_socket: &str, instance_name: &str) -> Value {
    let home = crate::home_dir();
    let instance_name = if instance_name.is_empty() {
        std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default()
    } else {
        instance_name.to_string()
    };

    match tool {
        // --- Channel ---
        "reply" => crate::ops::reply(&home, &instance_name, args["text"].as_str().unwrap_or("")),
        "react" => crate::ops::react(
            &instance_name,
            args["emoji"].as_str().unwrap_or(""),
            args["message_id"].as_str(),
        ),
        "edit_message" => {
            let mid = match args["message_id"].as_str() {
                Some(m) => m,
                None => return json!({"error": "missing 'message_id'"}),
            };
            let text = match args["text"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'text'"}),
            };
            crate::ops::edit_message(&instance_name, mid, text)
        }
        "download_attachment" => {
            let file_id = match args["file_id"].as_str() {
                Some(f) => f,
                None => return json!({"error": "missing 'file_id'"}),
            };
            crate::ops::download_attachment(&instance_name, file_id)
        }

        // --- Cross-instance communication ---
        "send_to_instance" | "send" => {
            let target = match args["instance_name"]
                .as_str()
                .or_else(|| args["target"].as_str())
            {
                Some(t) => t,
                None => return json!({"error": "missing 'instance_name' or 'target'"}),
            };
            let text = args["message"]
                .as_str()
                .or_else(|| args["text"].as_str())
                .unwrap_or("");
            let kind = args["request_kind"]
                .as_str()
                .or_else(|| args["kind"].as_str());
            crate::ops::send_message(&home, &instance_name, target, text, kind)
        }
        "delegate_task" => {
            let target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            let task = match args["task"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'task'"}),
            };
            crate::ops::delegate_task(
                &home,
                &instance_name,
                target,
                task,
                args["success_criteria"].as_str(),
                args["context"].as_str(),
            )
        }
        "report_result" => {
            let target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            let summary = match args["summary"].as_str() {
                Some(s) => s,
                None => return json!({"error": "missing 'summary'"}),
            };
            crate::ops::report_result(
                &home,
                &instance_name,
                target,
                summary,
                args["correlation_id"].as_str(),
                args["artifacts"].as_str(),
            )
        }
        "request_information" => {
            let target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            let question = match args["question"].as_str() {
                Some(q) => q,
                None => return json!({"error": "missing 'question'"}),
            };
            crate::ops::request_information(
                &home,
                &instance_name,
                target,
                question,
                args["context"].as_str(),
            )
        }
        "broadcast" => {
            let message = match args["message"].as_str() {
                Some(m) => m,
                None => return json!({"error": "missing 'message'"}),
            };
            let targets: Option<Vec<String>> = args["targets"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
            crate::ops::broadcast(
                &home,
                &instance_name,
                message,
                args["team"].as_str(),
                targets.as_deref(),
            )
        }
        "inbox" => crate::ops::drain_inbox(&home, &instance_name),

        // --- Instance management ---
        "list_instances" => crate::ops::list_instances(&home),
        "create_instance" => crate::ops::create_instance(&home, args),
        "delete_instance" => crate::ops::delete_instance(&home, args),
        "start_instance" => crate::ops::start_instance(&home, args),
        "describe_instance" => {
            crate::ops::describe_instance(&home, args["name"].as_str().unwrap_or(""))
        }
        "replace_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            crate::ops::replace_instance(&home, name, args["reason"].as_str().unwrap_or("manual"))
        }
        "set_display_name" => {
            crate::ops::set_display_name(&home, &instance_name, args["name"].as_str().unwrap_or(""))
        }
        "set_description" => crate::ops::set_description(
            &home,
            &instance_name,
            args["description"].as_str().unwrap_or(""),
        ),

        // --- Decisions ---
        "post_decision" => crate::decisions::post(&home, &instance_name, args),
        "list_decisions" => crate::decisions::list(&home, args),
        "update_decision" => crate::decisions::update(&home, args),

        // --- Task board ---
        "task" => crate::tasks::handle(&home, &instance_name, args),

        // --- Teams ---
        "create_team" => crate::teams::create(&home, args),
        "delete_team" => crate::teams::delete(&home, args),
        "list_teams" => crate::teams::list(&home),
        "update_team" => crate::teams::update(&home, args),

        // --- Scheduling ---
        "create_schedule" => crate::schedules::create(&home, &instance_name, args),
        "list_schedules" => crate::schedules::list(&home, args),
        "update_schedule" => crate::schedules::update(&home, args),
        "delete_schedule" => crate::schedules::delete(&home, args),

        // --- Deployments ---
        "deploy_template" => crate::deployments::deploy(&home, &instance_name, args),
        "teardown_deployment" => crate::deployments::teardown(&home, args),
        "list_deployments" => crate::deployments::list(&home),

        // --- Repo access ---
        "checkout_repo" => {
            let source = match args["source"].as_str() {
                Some(s) => s,
                None => return json!({"error": "missing 'source'"}),
            };
            let branch = args["branch"].as_str().unwrap_or("HEAD");
            crate::ops::checkout_repo(&home, &instance_name, source, branch)
        }
        "release_repo" => {
            let path = match args["path"].as_str() {
                Some(p) => p,
                None => return json!({"error": "missing 'path'"}),
            };
            crate::ops::release_repo(path)
        }

        // --- CI watch ---
        "watch_ci" => {
            let repo = match args["repo"].as_str() {
                Some(r) => r,
                None => return json!({"error": "missing 'repo'"}),
            };
            crate::ops::watch_ci(
                &home,
                &instance_name,
                repo,
                args["branch"].as_str().unwrap_or("main"),
                args["interval_secs"].as_u64().unwrap_or(60),
            )
        }
        "unwatch_ci" => {
            let repo = match args["repo"].as_str() {
                Some(r) => r,
                None => return json!({"error": "missing 'repo'"}),
            };
            crate::ops::unwatch_ci(&home, repo)
        }

        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::*;
    use serde_json::json;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-handlers-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    // validate_branch tests
    #[test]
    fn branch_valid_simple() {
        assert!(validate_branch("main"));
        assert!(validate_branch("feature/foo"));
        assert!(validate_branch("v1.0.0"));
        assert!(validate_branch("fix-123"));
        assert!(validate_branch("release_2.0"));
    }

    #[test]
    fn branch_rejects_empty() {
        assert!(!validate_branch(""));
    }

    #[test]
    fn branch_rejects_dotdot() {
        assert!(!validate_branch(".."));
        assert!(!validate_branch("foo/.."));
        assert!(!validate_branch("../bar"));
    }

    #[test]
    fn branch_rejects_leading_dash() {
        assert!(!validate_branch("-main"));
        assert!(!validate_branch("-"));
    }

    #[test]
    fn branch_rejects_special_chars() {
        assert!(!validate_branch("main branch"));
        assert!(!validate_branch("foo;bar"));
        assert!(!validate_branch("$(echo)"));
        assert!(!validate_branch("main\ninjected"));
    }

    // merge_metadata tests
    #[test]
    fn merge_metadata_no_file() {
        let home = tmp_home("merge_meta_no_file");
        let mut info = json!({"name": "agent1", "state": "idle"});
        merge_metadata(&home, "agent1", &mut info);
        assert_eq!(info["name"], "agent1");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_metadata_merges_fields() {
        let home = tmp_home("merge_meta_fields");
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        std::fs::write(
            meta_dir.join("agent1.json"),
            r#"{"display_name": "Dev Agent", "custom": 42}"#,
        )
        .ok();
        let mut info = json!({"name": "agent1", "state": "idle"});
        merge_metadata(&home, "agent1", &mut info);
        assert_eq!(info["display_name"], "Dev Agent");
        assert_eq!(info["custom"], 42);
        assert_eq!(info["name"], "agent1");
        std::fs::remove_dir_all(&home).ok();
    }

    // save_metadata tests
    #[test]
    fn save_and_load_metadata() {
        let home = tmp_home("save_meta");
        save_metadata(&home, "agent1", "display_name", json!("My Agent"));
        save_metadata(&home, "agent1", "version", json!(2));
        let content = std::fs::read_to_string(home.join("metadata/agent1.json")).expect("read");
        let meta: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(meta["display_name"], "My Agent");
        assert_eq!(meta["version"], 2);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_metadata_creates_dir() {
        let home = tmp_home("save_meta_dir");
        assert!(!home.join("metadata").exists());
        save_metadata(&home, "agent1", "key", json!("value"));
        assert!(home.join("metadata").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // get_submit_key tests
    #[test]
    fn get_submit_key_default() {
        let home = tmp_home("submit_key");
        let key = get_submit_key(&home, "agent1");
        assert_eq!(key, "\r");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn get_submit_key_from_fleet() {
        let home = tmp_home("submit_key_fleet");
        let yaml = r#"defaults:
  backend: claude
instances:
  dev:
    role: "Developer"
"#;
        std::fs::write(home.join("fleet.yaml"), yaml).ok();
        let key = get_submit_key(&home, "dev");
        assert!(!key.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    // --- cleanup_working_dir ---

    #[test]
    fn cleanup_agend_workspace_removes_entire_dir() {
        let home = tmp_home("cleanup_ws");
        let ws = home.join("workspaces").join("test-agent");
        std::fs::create_dir_all(&ws).ok();
        std::fs::write(ws.join("somefile.txt"), "data").ok();
        std::fs::write(ws.join("opencode.json"), "{}").ok();

        cleanup_working_dir(&home, "test-agent", &ws);
        assert!(!ws.exists(), "workspace dir should be fully removed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_user_dir_only_removes_agend_files() {
        let home = tmp_home("cleanup_user");
        let user_dir = tmp_home("cleanup_user_proj");

        std::fs::write(user_dir.join("main.rs"), "fn main() {}").ok();
        std::fs::write(user_dir.join("opencode.json"), "{}").ok();
        std::fs::write(user_dir.join("mcp-config.json"), "{}").ok();
        std::fs::create_dir_all(user_dir.join(".claude")).ok();
        std::fs::write(user_dir.join(".claude/settings.local.json"), "{}").ok();

        cleanup_working_dir(&home, "agent1", &user_dir);

        assert!(user_dir.join("main.rs").exists(), "user file must survive");
        assert!(!user_dir.join("opencode.json").exists());
        assert!(!user_dir.join("mcp-config.json").exists());
        assert!(!user_dir.join(".claude/settings.local.json").exists());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&user_dir).ok();
    }

    #[test]
    fn cleanup_removes_metadata_and_session() {
        let home = tmp_home("cleanup_meta");
        let ws = home.join("workspaces").join("agent1");
        std::fs::create_dir_all(&ws).ok();

        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(home.join("metadata/agent1.json"), "{}").ok();
        std::fs::create_dir_all(home.join("sessions")).ok();
        std::fs::write(home.join("sessions/agent1.sid"), "abc123").ok();

        cleanup_working_dir(&home, "agent1", &ws);

        assert!(!home.join("metadata/agent1.json").exists());
        assert!(!home.join("sessions/agent1.sid").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_nonexistent_dir_no_panic() {
        let home = tmp_home("cleanup_nodir");
        let fake = std::path::PathBuf::from("/tmp/nonexistent-agend-test-dir");
        cleanup_working_dir(&home, "agent1", &fake);
        std::fs::remove_dir_all(&home).ok();
    }
}
