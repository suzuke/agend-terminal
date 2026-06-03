//! #1576: HMAC-SHA256 keyed-hash integrity for the operator-mode authority file.
//!
//! THREAT MODEL — Agend-Terminal is a SINGLE-USER, SINGLE-MACHINE tool. There is
//! no second human / external attacker / multi-tenant. The only "attacker" is
//! the user's OWN agent getting prompt-injected into authority abuse — e.g.
//! blind-writing `operator-mode.json` to flip the #1339 gate to `active` and
//! disable it. Signing that file blocks the blind write: a reload trusts the
//! content only if it carries a valid HMAC, and an injected agent doesn't know
//! the key/scheme, so `echo '{"mode":"active"}' > operator-mode.json` is
//! rejected (the daemon keeps the last-known-good mode + alerts).
//!
//! This is injection-containment DEFENSE-IN-DEPTH, NOT a multi-user security
//! boundary. The key lives in a same-uid 0600 file, so a sufficiently
//! sophisticated agent could read the key and re-sign a tampered file — that is
//! ACCEPTED, because the only such agent is the user's own. It raises the bar
//! from "one echo" to "find + read the key + recompute the HMAC". Full sealing
//! would need OS-level isolation (separate uid / sandbox), parked as overkill
//! for a single-user tool (#1653).
//!
//! KISS: HMAC-SHA256 (the standard keyed hash) over the exact file bytes, stored
//! as a hex sidecar next to the file. No key rotation, no asymmetric crypto, no
//! nonces — none of which a single-user threat model needs.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::Path;

// #1651: the VERIFY + key-read half now lives in `integrity_core` (shared by
// source with the agend-git shim, which only verifies). The SIGN side stays here
// (getrandom key generation + the threat-model doc). `verify` is re-exported so
// existing callers (`operator_mode`) keep the `config_integrity::verify` API.
pub use crate::integrity_core::verify;
use crate::integrity_core::{key_path, read_key, KEY_FILE, KEY_LEN};

type HmacSha256 = Hmac<Sha256>;

/// Load the key, generating it (crypto-random, 0600) on first use.
fn ensure_key(home: &Path) -> std::io::Result<[u8; KEY_LEN]> {
    if let Some(k) = read_key(home) {
        return Ok(k);
    }
    let mut key = [0u8; KEY_LEN];
    getrandom::fill(&mut key).map_err(|e| std::io::Error::other(format!("getrandom: {e}")))?;
    let path = key_path(home);
    let tmp = home.join(format!(".{KEY_FILE}.tmp"));
    write_restricted(&tmp, &key)?;
    std::fs::rename(&tmp, &path)?;
    Ok(key)
}

#[cfg(unix)]
fn write_restricted(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(windows)]
fn write_restricted(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // Mirrors auth_cookie: the home dir is under %USERPROFILE%, whose NTFS ACL
    // already restricts to the user + SYSTEM. (And on a single-user box the
    // only reader is the user's own process anyway — see the threat model.)
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// HMAC-SHA256(`content`) under the home key, hex-encoded. Creates the key on
/// first use, so the first signer (the operator's `set_mode`) establishes it.
pub fn sign(home: &Path, content: &[u8]) -> std::io::Result<String> {
    let key = ensure_key(home)?;
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(content);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

// `verify` is re-exported from `integrity_core` (see the `pub use` above) —
// the shim shares that exact verifier by source.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("agend-cfgintegrity-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let home = tmp("roundtrip");
        let content = br#"{"mode":"sleep"}"#;
        let tag = sign(&home, content).unwrap();
        assert!(verify(&home, content, &tag), "fresh signature must verify");
    }

    #[test]
    fn tampered_content_fails_verify() {
        let home = tmp("tamper");
        let tag = sign(&home, br#"{"mode":"away"}"#).unwrap();
        // Same key, different content (the blind-overwrite attack).
        assert!(
            !verify(&home, br#"{"mode":"active"}"#, &tag),
            "a different payload must not verify under the same tag"
        );
    }

    #[test]
    fn verify_without_key_is_false() {
        let home = tmp("nokey");
        // No key has been created (nothing signed) → cannot authenticate.
        assert!(
            !verify(&home, b"anything", "00"),
            "no key yet → cannot authenticate → false (fail closed)"
        );
    }

    #[test]
    fn malformed_tag_is_false() {
        let home = tmp("malformed");
        sign(&home, b"x").unwrap(); // create the key
        assert!(!verify(&home, b"x", "not-hex-zzz"));
    }

    /// #1651: GOLDEN parity — pins the exact HMAC-SHA256 output for a FIXED key +
    /// FIXED operator-mode.json content. Computed from the pre-`integrity_core`-
    /// extraction code; if the extraction (or any future change) alters the
    /// algorithm/encoding, this tag changes and the test fails — proving
    /// operator-mode.json signing stays byte-identical ("別動 #1576 已簽檔邏輯").
    #[test]
    fn operator_mode_signing_is_byte_identical_golden_1651() {
        let home = tmp("golden");
        // Deterministic key (NOT the random one) so the tag is reproducible.
        std::fs::write(home.join(KEY_FILE), [1u8; KEY_LEN]).unwrap();
        let content = br#"{"mode":"active","since":"2026-01-01T00:00:00Z"}"#;
        let tag = sign(&home, content).unwrap();
        assert_eq!(
            tag, "def046eac649ccbc86de77718e0f4363ba835283fdd8bee1a5b11cb98671ef72",
            "#1651: operator-mode.json HMAC must stay byte-identical across the \
             integrity_core extraction (golden from pre-extraction code)"
        );
    }

    /// #1651 cross-compat: the agend-git shim's verifier (`integrity_core::verify`,
    /// shared by source) MUST accept what the daemon's signer
    /// (`config_integrity::sign`) produces — this pins the signer/verifier contract
    /// the binding push-authority relies on.
    #[test]
    fn shim_verifier_accepts_daemon_signature_1651() {
        let home = tmp("crosscompat");
        let content = br#"{"version":1,"task_id":"T-9","branch":"feat/y"}"#;
        let tag = sign(&home, content).unwrap();
        assert!(
            crate::integrity_core::verify(&home, content, &tag),
            "the shim verifier must accept a daemon-signed binding tag"
        );
        // negative: same key, mutated content (the blind self-authorization) → reject.
        assert!(!crate::integrity_core::verify(
            &home,
            br#"{"version":1,"task_id":"T-9","branch":"main"}"#,
            &tag
        ));
    }
}
