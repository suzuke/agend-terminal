//! #26 Embedder Contract v1: the typed, versioned binding document.
//!
//! This is the typed representation of `runtime/<agent>/binding.json` shared
//! by the reference `agentic-git run` writer and the shim reader. The agend
//! daemon does NOT link this crate — its zero-daemon-change compatibility is
//! SCHEMA compatibility, pinned by the golden `binding-agend-v1.json`
//! fixture. A second orchestrator signs and writes exactly this document —
//! see `docs/embedder-contract-v1.md`.
//!
//! Version policy (v1 freeze): a document with NO `version` field is a
//! legacy v1 (agend zero-daemon-change adoption); `version: 1` is v1; any
//! OTHER version fails closed with [`BindingDecodeError::UnsupportedVersion`]
//! — a future v2 document may carry authority semantics this reader cannot
//! enforce, so "treat as unbound" is the only safe disposition. Unknown
//! FIELDS within v1 are the bounded extension surface: they are preserved
//! round-trip in [`BindingV1::extra`] and ignored by readers.

use serde::{Deserialize, Serialize};

/// The binding document format version this crate reads and writes.
/// Mirrors [`crate::integrity_core::BINDING_FORMAT_VERSION`] (the original
/// constant stays for compatibility; this module is the typed owner).
pub const BINDING_FORMAT_VERSION: u32 = crate::integrity_core::BINDING_FORMAT_VERSION;

fn default_version() -> u32 {
    BINDING_FORMAT_VERSION
}

/// The v1 binding document. All identity fields are optional at the codec
/// layer — the *bound* predicate (`task_id` present + worktree exists) is
/// reader policy, not codec policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingV1 {
    /// Format version; absent in legacy documents (decodes as v1).
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The bound predicate's anchor: present ⇒ the agent is bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at: Option<String>,
    /// Bounded extension surface: unknown v1 fields are preserved verbatim
    /// (round-trip) and ignored by readers. A field a reader must UNDERSTAND
    /// to stay safe belongs in v2, not here.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for BindingV1 {
    fn default() -> Self {
        Self {
            version: BINDING_FORMAT_VERSION,
            agent: None,
            task_id: None,
            branch: None,
            worktree: None,
            source_repo: None,
            issued_at: None,
            extra: serde_json::Map::new(),
        }
    }
}

/// Decode failure. Every variant must be treated fail-closed (unbound) by
/// binding readers; `UnsupportedVersion` deserves a LOUD diagnostic (scheme
/// skew between signer and reader builds), mirroring the HMAC
/// `VerifyError::UnsupportedScheme` posture.
#[derive(Debug)]
pub enum BindingDecodeError {
    /// The document declares a version this crate does not implement.
    /// `found` is 0 when the field is present but not a positive integer.
    UnsupportedVersion { found: u64 },
    Parse(serde_json::Error),
}

impl std::fmt::Display for BindingDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindingDecodeError::UnsupportedVersion { found } => write!(
                f,
                "binding format version {found} is not supported (this build implements v{BINDING_FORMAT_VERSION})"
            ),
            BindingDecodeError::Parse(e) => write!(f, "binding parse error: {e}"),
        }
    }
}

impl std::error::Error for BindingDecodeError {}

/// Decode a binding document. The version gate runs FIRST on the raw value so
/// an unsupported version fails closed even when other fields are malformed.
pub fn decode(json: &str) -> Result<BindingV1, BindingDecodeError> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(BindingDecodeError::Parse)?;
    if let Some(version) = value.get("version") {
        let found = version.as_u64().unwrap_or(0);
        if found != u64::from(BINDING_FORMAT_VERSION) {
            return Err(BindingDecodeError::UnsupportedVersion { found });
        }
    }
    serde_json::from_value(value).map_err(BindingDecodeError::Parse)
}

/// Encode a binding document (pretty-printed — the on-disk shape the
/// reference `agentic-git run` writer signs; schema-compatible with what the
/// agend daemon writes independently).
pub fn encode(binding: &BindingV1) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(binding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_v1_roundtrips_identity_fields() {
        let doc = BindingV1 {
            agent: Some("ag".into()),
            task_id: Some("t-1".into()),
            branch: Some("feat/x".into()),
            worktree: Some("/tmp/wt".into()),
            source_repo: Some("/tmp/src".into()),
            issued_at: Some("2026-07-19T00:00:00Z".into()),
            ..Default::default()
        };
        let json = encode(&doc).unwrap();
        let back = decode(&json).unwrap();
        assert_eq!(back, doc);
        assert_eq!(back.version, BINDING_FORMAT_VERSION);
    }

    #[test]
    fn decode_missing_version_is_legacy_v1() {
        let b = decode(r#"{"task_id":"t-legacy","branch":"feat/l"}"#).unwrap();
        assert_eq!(b.version, BINDING_FORMAT_VERSION);
        assert_eq!(b.task_id.as_deref(), Some("t-legacy"));
    }

    #[test]
    fn decode_unsupported_version_fails_closed() {
        let err = decode(r#"{"version":2,"task_id":"t-f"}"#).unwrap_err();
        match err {
            BindingDecodeError::UnsupportedVersion { found } => assert_eq!(found, 2),
            other => panic!("expected UnsupportedVersion, got {other}"),
        }
    }

    #[test]
    fn decode_non_numeric_version_fails_closed() {
        let err = decode(r#"{"version":"x","task_id":"t-f"}"#).unwrap_err();
        assert!(matches!(
            err,
            BindingDecodeError::UnsupportedVersion { found: 0 }
        ));
    }

    #[test]
    fn unknown_fields_are_bounded_extensions_preserved_roundtrip() {
        let json = r#"{"version":1,"task_id":"t-1","custom_lease":"abc","nested":{"k":1}}"#;
        let doc = decode(json).unwrap();
        assert_eq!(doc.extra.get("custom_lease").and_then(|v| v.as_str()), Some("abc"));
        let re = encode(&doc).unwrap();
        let back = decode(&re).unwrap();
        assert_eq!(back, doc, "extension fields must survive a round-trip");
    }

    #[test]
    fn golden_fixtures_decode() {
        for (rel, task_id) in [
            ("../agentic-git/tests/fixtures/binding-agend-v1.json", "t-20260719-golden-agend"),
            ("../agentic-git/tests/fixtures/binding-run-v1.json", "run-session-1789000000"),
        ] {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
            let content = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("golden fixture {rel}: {e}"));
            let doc = decode(&content).unwrap_or_else(|e| panic!("golden {rel}: {e}"));
            assert_eq!(doc.version, 1, "golden {rel}");
            assert_eq!(doc.task_id.as_deref(), Some(task_id), "golden {rel}");
        }
    }
}
