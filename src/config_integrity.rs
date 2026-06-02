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
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

const KEY_LEN: usize = 32;
const KEY_FILE: &str = ".config-integrity-key";

fn key_path(home: &Path) -> PathBuf {
    home.join(KEY_FILE)
}

/// Read the key if present and exactly [`KEY_LEN`] bytes; `None` otherwise.
fn read_key(home: &Path) -> Option<[u8; KEY_LEN]> {
    let bytes = std::fs::read(key_path(home)).ok()?;
    bytes.try_into().ok()
}

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

/// Constant-time verify of `content` against the hex `tag`. Returns `false` on
/// any error (no key yet, malformed tag, mismatch) — callers treat `false` as
/// "not authentic" and fail closed.
pub fn verify(home: &Path, content: &[u8], tag: &str) -> bool {
    let Some(key) = read_key(home) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(&key) else {
        return false;
    };
    mac.update(content);
    let Ok(tag_bytes) = hex::decode(tag.trim()) else {
        return false;
    };
    mac.verify_slice(&tag_bytes).is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
}
