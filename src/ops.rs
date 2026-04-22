//! Shared operations — called by MCP handlers.
//!
//! Historical note: this module previously held 21 additional CLI-wrapper
//! functions (send_message, delegate_task, create_instance, etc.). The CLI
//! subcommands now invoke `api::call` directly, making those wrappers dead
//! code. They were removed in the Task #9 Option C epilogue; only
//! `start_instance` (still invoked by `mcp/handlers.rs` dispatcher) remains.

use serde_json::{json, Value};
use std::path::Path;

pub fn start_instance(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return json!({"error": "No fleet.yaml"});
    }
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("fleet.yaml: {e}")}),
    };
    match config.resolve_instance(name) {
        Some(resolved) => {
            let cmd_args = resolved.args.join(" ");
            match crate::api::call(
                home,
                &json!({"method": crate::api::method::SPAWN, "params": {
                    "name": name, "backend": resolved.backend_command, "args": cmd_args,
                    "mode": "resume",
                    "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                }}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"name": name}),
                Ok(resp) => {
                    json!({"error": resp["error"].as_str().unwrap_or("spawn failed")})
                }
                Err(e) => json!({"error": format!("API unavailable: {e}")}),
            }
        }
        None => json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn no_mcp_prefix_in_ops() {
        // Verify old prefix was fully replaced with [agend]
        let source = include_str!("ops.rs");
        let old_prefix = format!("[{}]", "mcp");
        let lines_with_old: Vec<_> = source
            .lines()
            .filter(|l| l.contains(&old_prefix) && !l.contains("test"))
            .collect();
        assert!(
            lines_with_old.is_empty(),
            "ops.rs has old prefix: {:?}",
            lines_with_old
        );
    }

    #[test]
    fn backend_resolves_to_preset_command() {
        // "kiro" should resolve to "kiro-cli" via preset
        let resolved = crate::backend::Backend::from_command("kiro").map(|b| b.preset().command);
        assert_eq!(resolved, Some("kiro-cli"));

        // "claude" stays "claude"
        let resolved = crate::backend::Backend::from_command("claude").map(|b| b.preset().command);
        assert_eq!(resolved, Some("claude"));
    }
}
