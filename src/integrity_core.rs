//! #1651: shared HMAC-SHA256 integrity primitives — the VERIFY + key-read half,
//! used by BOTH the main daemon (`config_integrity`, which adds the sign side)
//! and the standalone `agend-git` shim (which only verifies). The shim cannot
//! link the lib and `config_integrity` lives in the main-binary tree, so this
//! file is shared by source: `config_integrity` declares it as a module and the
//! shim pulls THE SAME file via `#[path = "../integrity_core.rs"] mod
//! integrity_core;`. One source ⟹ no signer/verifier algorithm drift (a drift
//! in a security check fails silently: too-loose → fail-open, too-strict →
//! false-deny).
//!
//! Threat model is documented in `config_integrity` (the signer): same-uid
//! injection-containment defense-in-depth, NOT a security boundary.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

pub(crate) const KEY_LEN: usize = 32;
pub(crate) const KEY_FILE: &str = ".config-integrity-key";

pub(crate) fn key_path(home: &Path) -> PathBuf {
    home.join(KEY_FILE)
}

/// Read the key if present and exactly [`KEY_LEN`] bytes; `None` otherwise.
pub(crate) fn read_key(home: &Path) -> Option<[u8; KEY_LEN]> {
    let bytes = std::fs::read(key_path(home)).ok()?;
    bytes.try_into().ok()
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

/// Test-only HMAC over the EXISTING home key (no generation), so tests in both
/// binaries can fabricate valid sidecars without the getrandom sign path.
// Used by the agend-git shim's tests (this file is shared by #[path]); unused in
// the main binary's test build, hence the allow.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn sign_for_test(home: &Path, content: &[u8]) -> String {
    let key = read_key(home).expect("test key must exist");
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(content);
    hex::encode(mac.finalize().into_bytes())
}
