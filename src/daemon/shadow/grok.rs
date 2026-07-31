//! Read-only observer for Grok Build's structured session updates.

use super::evidence::{Evidence, EvidenceKind};

/// RED-first seam: the production observer is added after the preserved real-line
/// fixture proves the currently missing quota signal.
pub(crate) fn record_to_evidence(_line: &str, _now_ms: u64) -> Option<Evidence> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_LINES: &str = include_str!("../../../tests/fixtures/grok-s1-usage-limit-updates.jsonl");

    #[test]
    fn preserved_real_grok_lines_classify_only_exhausted_quota() {
        let lines: Vec<&str> = REAL_LINES.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(record_to_evidence(lines[0], 1_785_471_700_000).is_none());
        assert!(matches!(
            record_to_evidence(lines[1], 1_785_471_700_000),
            Some(Evidence { kind: EvidenceKind::UsageLimit, .. })
        ));
        assert!(record_to_evidence(lines[2], 1_785_471_700_000).is_none());
    }

    #[test]
    fn malformed_structured_update_is_ignored() {
        assert!(record_to_evidence("not-json", 1_000).is_none());
    }
}
