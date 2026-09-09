//! Persisted configuration for daemon-owned schedule executions.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    pub backends: Vec<String>,
    pub artifact_directory: PathBuf,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_secs: u64,
    #[serde(default)]
    pub output_context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<JobNotification>,
}
/// Explicit endpoint; credentials remain in the existing channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "channel", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobNotification {
    Telegram {
        chat_id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        topic_id: Option<i32>,
    },
}
impl JobNotification {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Telegram { chat_id, topic_id } => {
                if *chat_id == 0 || topic_id.is_some_and(|id| id <= 0) {
                    return Err(
                        "notification requires nonzero chat_id and positive topic_id".into(),
                    );
                }
            }
        }
        Ok(())
    }
    pub fn validate_for_home(&self, home: &std::path::Path) -> Result<(), String> {
        self.validate()?;
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .map_err(|_| "cannot read notification channel configuration".to_string())?;
        match self {
            Self::Telegram { chat_id, .. } => {
                if !matches!(fleet.telegram_channel(), Some(crate::fleet::ChannelConfig::Telegram { group_id, .. }) if group_id == chat_id)
                {
                    return Err(
                        "notification chat_id must match the configured Telegram group".into(),
                    );
                }
            }
        }
        Ok(())
    }
}

fn default_timeout() -> u64 {
    3600
}
fn default_attempts() -> u32 {
    3
}
fn default_retry_delay() -> u64 {
    60
}

impl JobConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.backends.is_empty() {
            return Err("job.backends must not be empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        for name in &self.backends {
            if !matches!(
                name.as_str(),
                "claude" | "codex" | "kiro-cli" | "opencode" | "antigravity-cli" | "grok"
            ) {
                return Err(format!("unsupported job backend: {name}"));
            }
            if !seen.insert(name) {
                return Err(format!("duplicate job backend: {name}"));
            }
        }
        if !self.artifact_directory.is_absolute() {
            return Err("job.artifact_directory must be absolute".into());
        }
        if !(60..=86400).contains(&self.timeout_secs) {
            return Err("job.timeout_secs must be 60..86400".into());
        }
        if !(1..=10).contains(&self.max_attempts) {
            return Err("job.max_attempts must be 1..10".into());
        }
        if !(1..=3600).contains(&self.retry_delay_secs) {
            return Err("job.retry_delay_secs must be 1..3600".into());
        }
        if let Some(notification) = &self.notification {
            notification.validate()?;
        }
        Ok(())
    }
    pub fn validate_for_home(&self, home: &std::path::Path) -> Result<(), String> {
        self.validate()?;
        if let Some(notification) = &self.notification {
            notification.validate_for_home(home)?;
        }
        Ok(())
    }
}
