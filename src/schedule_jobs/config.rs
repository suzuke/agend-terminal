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
    /// Opt-in: let the daemon auto-clean a `Succeeded` Run whose worker it spawned
    /// in this same lifecycle and can prove fully contained (child reaped, whole
    /// process group gone, no external registry residue). Absent (default) keeps
    /// today's behaviour: every launched worker requires an operator recovery step.
    #[serde(default)]
    pub auto_cleanup: bool,
    /// Bounded retry window (seconds) for `auto_cleanup`: while a successful,
    /// contained-but-not-yet-exited worker is still shutting down, each daemon
    /// tick re-attempts containment proof for up to this long before falling back
    /// to `recovery_required`. Default 60; range 0..=600 (0 = today's immediate
    /// fallback). Ignored unless `auto_cleanup` is true.
    #[serde(default = "default_cleanup_retry")]
    pub cleanup_retry_secs: u64,
    /// Opt-in: the existing Telegram topic where the worker delivers business
    /// output through `schedule action=deliver`. Reuses the notification endpoint
    /// shape and its explicit-group validation; unlike `notification` the topic is
    /// required, no topic is ever created, and the transport owns no registry row
    /// (so worker teardown can never delete the shared topic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_topic: Option<JobNotification>,
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
    /// The topic this endpoint resolves to, when one is configured.
    pub fn topic_id(&self) -> Option<i32> {
        match self {
            Self::Telegram { topic_id, .. } => *topic_id,
        }
    }
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
fn default_cleanup_retry() -> u64 {
    60
}

/// Upper bound for `cleanup_retry_secs`; the window may never become unbounded.
pub const MAX_CLEANUP_RETRY_SECS: u64 = 600;

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
        if self.cleanup_retry_secs > MAX_CLEANUP_RETRY_SECS {
            return Err(format!(
                "job.cleanup_retry_secs must be 0..={MAX_CLEANUP_RETRY_SECS}"
            ));
        }
        if let Some(notification) = &self.notification {
            notification.validate()?;
        }
        if let Some(worker_topic) = &self.worker_topic {
            worker_topic.validate()?;
            if worker_topic.topic_id().is_none() {
                return Err("job.worker_topic requires an explicit existing topic_id".into());
            }
        }
        Ok(())
    }
    pub fn validate_for_home(&self, home: &std::path::Path) -> Result<(), String> {
        self.validate()?;
        if let Some(notification) = &self.notification {
            notification.validate_for_home(home)?;
        }
        if let Some(worker_topic) = &self.worker_topic {
            worker_topic.validate_for_home(home)?;
        }
        Ok(())
    }
}
