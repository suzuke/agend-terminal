//! Shared daemon utilities — extracted from legacy_backfill and task_sweep
//! to eliminate duplication (G3 M2).

use sha2::{Digest, Sha256};

/// Strip HTML comments (`<!-- ... -->`) from a string.
pub fn strip_html_comments(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        result.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                // Unterminated comment — drop the tail (security: attacker
                // can't sneak directives past us via partial comments)
                return result;
            }
        }
    }
    result.push_str(rest);
    result
}

/// SHA-256 hex digest of arbitrary bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_comments_removes_comments() {
        assert_eq!(
            strip_html_comments("before<!-- hidden -->after"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_html_comments_no_comments() {
        assert_eq!(strip_html_comments("plain text"), "plain text");
    }

    #[test]
    fn sha256_hex_deterministic() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }
}
