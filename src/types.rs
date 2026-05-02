//! Core types shared across modules.

use serde::{Deserialize, Serialize};

/// Unique instance identifier — UUIDv4 primary, 8-char short alias for display.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub uuid::Uuid);

impl InstanceId {
    /// Generate a new random UUIDv4 instance ID.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// 8-character short alias for display (first 8 hex chars of UUID).
    pub fn short(&self) -> String {
        self.0.as_simple().to_string()[..8].to_string()
    }

    /// Parse from a full UUID string. Short aliases are display-only (no parse-back).
    pub fn parse(s: &str) -> Option<Self> {
        uuid::Uuid::parse_str(s).ok().map(Self)
    }

    /// Full UUID string.
    pub fn full(&self) -> String {
        self.0.to_string()
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_generation_is_uuid_v4() {
        let id = InstanceId::new();
        assert_eq!(id.0.get_version_num(), 4);
        assert_eq!(id.short().len(), 8);
    }

    #[test]
    fn id_short_is_deterministic() {
        let id = InstanceId::new();
        assert_eq!(id.short(), id.short());
    }
}
