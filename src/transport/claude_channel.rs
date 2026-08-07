//! Claude Code ChannelBridge transport.
//!
//! The implementation is intentionally introduced test-first.  Claude's
//! research-preview channel contract is an MCP stdio server with a loopback
//! authenticated webhook and a structured `reply` tool; the RED fixtures below
//! pin those production seams before the adapter is filled in.

use super::SessionLocator;
use serde_json::Value;
use std::path::Path;

pub(crate) fn legacy_pty_opt_in(_home: &Path, _instance: &str) -> bool {
    false
}

pub(crate) fn prepare_claude_channel(
    _home: &Path,
    _instance: &str,
) -> anyhow::Result<SessionLocator> {
    anyhow::bail!("Claude ChannelBridge is not implemented")
}

pub(crate) fn channel_server_entry(_home: &Path, _instance: &str) -> anyhow::Result<Value> {
    anyhow::bail!("Claude ChannelBridge is not implemented")
}

pub(crate) fn deliver_resident(
    _home: &Path,
    _instance: &str,
    _envelope: super::DeliveryEnvelope,
) -> anyhow::Result<super::DeliveryReceipt> {
    anyhow::bail!("Claude ChannelBridge is not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::transport::{mode_for_backend, mode_for_instance, TransportMode};
    use std::fs;
    use uuid::Uuid;

    fn home(tag: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-claude-channel-red-{}-{tag}", Uuid::new_v4()));
        fs::create_dir_all(&home).expect("home");
        home
    }

    #[test]
    fn claude_uses_channel_bridge_by_default() {
        assert_eq!(
            mode_for_backend(&Backend::ClaudeCode),
            TransportMode::ChannelBridge
        );
    }

    #[test]
    fn explicit_legacy_pty_is_the_only_claude_fallback() {
        let home = home("legacy");
        fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");
        assert_eq!(
            mode_for_instance(&home, "claude-agent"),
            TransportMode::LegacyPty
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn channel_locator_persists_across_daemon_restart() {
        let home = home("locator");
        let first = prepare_claude_channel(&home, "claude-agent").expect("initial channel locator");
        let second =
            prepare_claude_channel(&home, "claude-agent").expect("reloaded channel locator");
        assert_eq!(
            first, second,
            "restart must reuse endpoint, token, and session"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn channel_server_entry_declares_authenticated_local_bridge() {
        let home = home("entry");
        let entry = channel_server_entry(&home, "claude-agent").expect("channel MCP server entry");
        assert_eq!(entry["env"]["AGEND_INSTANCE_NAME"], "claude-agent");
        assert_eq!(entry["args"][0], "channel-bridge");
        assert_eq!(entry["env"]["AGEND_HOME"], home.display().to_string());
        let _ = fs::remove_dir_all(home);
    }
}
