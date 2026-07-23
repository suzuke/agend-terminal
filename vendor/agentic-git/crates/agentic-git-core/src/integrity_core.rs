//! #1651 / embedder P1a: shared HMAC-SHA256 integrity primitives — the SINGLE
//! source for key provisioning, signing, and verifying binding sidecars. Used by
//! the standalone `agentic-git` shim (verifies) AND any embedding system (a fleet
//! daemon, an orchestrator, tests) that SIGNS sidecars. One source ⟹ no
//! signer/verifier drift (a drift in a security check fails silently: too-loose →
//! fail-open, too-strict → false-deny).
//!
//! P1a consolidates the contract: `ensure_key` (lock-free first-writer-wins key
//! provisioning, moved verbatim from the run CLI), `sign_binding` (provision +
//! sign — closes the reviewer4 hole where the low-level `sign` panicked without a
//! key), and a TYPED `verify` that parses a self-describing tag ENVELOPE before the
//! MAC check so an HMAC scheme skew is diagnosable, not a silent "unbound".
//!
//! **Ordering invariant (fleet-safety):** the DEFAULT emit stays the BARE hex tag
//! (implicit scheme v1) — byte-identical to the pre-P1a signer — so an unswapped
//! verifier and every on-disk sidecar keep verifying. The envelope is a CAPABILITY
//! (`envelope_tag` produces it, `verify` accepts it), NOT the default output; that
//! switch happens in a later phase.
//!
//! Threat model: same-uid injection-containment defense-in-depth, NOT a security
//! boundary (a same-uid agent could read the key and re-sign — #1653 ceiling).

// #1934 (hmac 0.13): `new_from_slice` moved behind the explicit `KeyInit`
// trait import (no longer implied by `Mac`). Construction + tag semantics are
// unchanged — pinned by the cross-version fixture test.
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

pub(crate) const KEY_LEN: usize = 32;
pub(crate) const KEY_FILE: &str = ".config-integrity-key";

/// The binding-FORMAT version — the schema of the binding JSON FIELDS, carried
/// INSIDE the HMAC-covered content (an injected agent cannot forge it without the
/// key). Bumped when the binding's fields change. Independent of `HMAC_ALGO_VERSION`.
pub const BINDING_FORMAT_VERSION: u32 = 1;

/// The HMAC-algorithm / tag-format version — carried in the tag ENVELOPE, OUTSIDE
/// HMAC protection, so `verify` can read it BEFORE the MAC check. Bumped ONLY when
/// key-derivation / tag format / hash algo changes. Independent of the binding format.
pub const HMAC_ALGO_VERSION: u32 = 1;

/// The scheme identifier that prefixes an enveloped tag (`<SCHEME_ID>:v<algo>:<key-format>:<hex>`).
/// A tag WITHOUT this prefix is a bare-hex legacy tag (implicit scheme v1).
pub const SCHEME_ID: &str = "ag-hmac-sha256";

/// The key-format component of the envelope: `raw` = the 32-byte raw key file read
/// by [`read_key`]. An enveloped tag naming any other key-format is unsupported.
pub const HMAC_KEY_FORMAT: &str = "raw";

pub(crate) fn key_path(home: &Path) -> PathBuf {
    home.join(KEY_FILE)
}

/// Read the key if present and exactly [`KEY_LEN`] bytes; `None` otherwise.
pub(crate) fn read_key(home: &Path) -> Option<[u8; KEY_LEN]> {
    let bytes = std::fs::read(key_path(home)).ok()?;
    bytes.try_into().ok()
}

/// Open `path` write-only, `create_new` (fail if it exists), mode 0600 on unix.
/// A key/tmp file must be born with its restrictive mode AT OPEN TIME, not gain it
/// via a later chmod — `fs::write` + `set_permissions` leaves a umask-dependent
/// window where another local uid can read the HMAC key and forge binding sidecars.
/// `create_new` additionally refuses a pre-planted path. (Moved from the run CLI in
/// P1a so it travels with `ensure_key`.)
fn open_new_0600(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Atomic, lock-free first-writer-wins key provisioning (moved verbatim from the
/// run CLI in P1a — the concurrency-robust `hard_link` variant, now the single core
/// key primitive). Write to a unique temp file, fsync, then `hard_link` it into
/// place — `AlreadyExists` means we LOST the race, not that we failed; we discard
/// our tmp and defer to the survivor. The key path only ever appears fully written
/// (never a partial/truncated file), matching [`read_key`]'s exactly-32-bytes
/// contract.
///
/// Idempotent: an existing exactly-[`KEY_LEN`] key is reused. A wrong-size key is a
/// hard `Err` — refuse to overwrite (fail-closed; a guarded session without a
/// signable binding must not silently degrade).
///
/// We MUST NOT fall back to `rename` on `hard_link` failure: `hard_link` fails
/// `AlreadyExists` (first-writer-wins, never clobbers a live key) whereas `rename`
/// overwrites (last-writer-wins) — the clobber this exists to prevent. The correct
/// fallback for an exotic no-hard_link FS is `create_new` (O_EXCL) directly on the
/// key path (preserves first-writer-wins); not implemented as the default.
pub fn ensure_key(home: &Path) -> Result<(), String> {
    let key_path = key_path(home);
    if let Ok(meta) = std::fs::metadata(&key_path) {
        if meta.len() as usize == KEY_LEN {
            return Ok(()); // already provisioned — reuse.
        }
        return Err(format!(
            "integrity key at {} exists but is not exactly {KEY_LEN} bytes (corrupt) — refusing \
             to overwrite; a guarded session without a signable binding must not silently \
             degrade. Remove it manually only if you are certain it is safe to regenerate.",
            key_path.display()
        ));
    }

    let mut rand_suffix = [0u8; 8];
    getrandom::fill(&mut rand_suffix).map_err(|e| format!("getrandom: {e}"))?;
    let tmp_path = home.join(format!(
        "key.tmp.{}.{}",
        std::process::id(),
        hex::encode(rand_suffix)
    ));

    let mut key = [0u8; KEY_LEN];
    getrandom::fill(&mut key).map_err(|e| format!("getrandom: {e}"))?;
    {
        use std::io::Write;
        let mut f = open_new_0600(&tmp_path).map_err(|e| format!("create temp key: {e}"))?;
        f.write_all(&key)
            .map_err(|e| format!("write temp key: {e}"))?;
        // fsync before the hard_link "publish" — the link must never observe
        // a not-yet-durable write.
        let _ = f.sync_all();
    }

    match std::fs::hard_link(&tmp_path, &key_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&tmp_path);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Someone else won the race; their key stands.
            let _ = std::fs::remove_file(&tmp_path);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(format!("hard_link key provisioning: {e}"))
        }
    }
}

/// Why `verify` failed — a TYPED result (replaces the pre-P1a `bool`) so a caller
/// can distinguish a benign "not authentic / unbound" from an HMAC SCHEME SKEW that
/// is diagnosable. All variants are fail-closed (deny); only the DIAGNOSIS differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// No integrity key at `home` yet.
    MissingKey,
    /// The tag is structurally invalid (bad envelope fields / extra colon / a
    /// non-`v<n>` algo / non-hex MAC, or a foreign non-hex prefix). NEVER a
    /// silent bare-hex-legacy retry — that would be a fail-open scheme bypass.
    MalformedTag,
    /// The MAC did not match the content under this key (tampered / wrong key /
    /// downgrade whose bytes don't authenticate).
    MacMismatch,
    /// The envelope names an algo/key-format this runtime does not implement (e.g.
    /// a NEWER scheme) — reported BEFORE any MAC computation. The shim maps this to
    /// a LOUD deny: signer and verifier were built from different core versions.
    UnsupportedScheme {
        tag_scheme: String,
        runtime_scheme: String,
    },
}

/// The scheme string THIS runtime implements, for `UnsupportedScheme` diagnostics.
fn runtime_scheme() -> String {
    format!("{SCHEME_ID}:v{HMAC_ALGO_VERSION}:{HMAC_KEY_FORMAT}")
}

/// Resolve a tag to its hex MAC, enforcing the scheme envelope. A tag carrying the
/// `<SCHEME_ID>:` prefix is parsed strictly (fail-closed, NEVER a legacy fallback);
/// a tag with no prefix and no colon is a bare-hex legacy tag (implicit scheme v1);
/// anything else (a colon but not our prefix) is malformed.
fn tag_to_hex(tag: &str) -> Result<&str, VerifyError> {
    let tag = tag.trim();
    if let Some(body) = tag
        .strip_prefix(SCHEME_ID)
        .and_then(|r| r.strip_prefix(':'))
    {
        // Enveloped: v<algo>:<key-format>:<hex>. Malformed → fail-closed, never legacy.
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() != 3 {
            return Err(VerifyError::MalformedTag); // missing field / extra colon
        }
        let Some(algo) = parts[0].strip_prefix('v').and_then(|n| n.parse::<u32>().ok()) else {
            return Err(VerifyError::MalformedTag); // non-`v<n>` algo
        };
        if algo != HMAC_ALGO_VERSION || parts[1] != HMAC_KEY_FORMAT {
            return Err(VerifyError::UnsupportedScheme {
                tag_scheme: format!("{SCHEME_ID}:v{algo}:{}", parts[1]),
                runtime_scheme: runtime_scheme(),
            });
        }
        Ok(parts[2])
    } else if tag.contains(':') {
        // A colon but not our scheme prefix = a malformed / foreign prefix. Must
        // NOT fall back to bare-hex legacy (that would be a fail-open bypass).
        Err(VerifyError::MalformedTag)
    } else {
        Ok(tag) // bare-hex legacy (implicit scheme v1)
    }
}

/// Constant-time verify of `content` against `tag` (a bare-hex legacy tag OR a
/// scheme envelope). Returns `Ok(())` iff authentic; every failure mode is a typed
/// [`VerifyError`] and fail-closed. The scheme envelope is parsed BEFORE the MAC
/// check so an `UnsupportedScheme` skew fires even when the MAC could never match.
pub fn verify(home: &Path, content: &[u8], tag: &str) -> Result<(), VerifyError> {
    let key = read_key(home).ok_or(VerifyError::MissingKey)?;
    let hex = tag_to_hex(tag)?;
    let mut mac = HmacSha256::new_from_slice(&key).map_err(|_| VerifyError::MacMismatch)?;
    mac.update(content);
    let tag_bytes = hex::decode(hex).map_err(|_| VerifyError::MalformedTag)?;
    mac.verify_slice(&tag_bytes)
        .map_err(|_| VerifyError::MacMismatch)
}

/// Reference signer: HMAC over the EXISTING home key, returned as a BARE hex tag.
/// Deliberately no key-generation side-effect — provisioning is [`sign_binding`]'s
/// (or the embedder's) job. The exact counterpart of [`verify`]'s legacy path.
///
/// # Panics
/// Panics if the home key does not exist — callers provision it first (use
/// [`sign_binding`], which provisions then signs).
pub fn sign(home: &Path, content: &[u8]) -> String {
    let key = read_key(home).expect("integrity key must exist before signing");
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(content);
    hex::encode(mac.finalize().into_bytes())
}

/// Provision the key if missing, then sign — the safe public signer for an embedder
/// (daemon / orchestrator / run CLI). Closes the reviewer4 hole where the low-level
/// [`sign`] panicked on a fresh home. The output is the BARE hex tag (implicit
/// scheme v1), byte-identical to [`sign`] and to the pre-P1a signer, so an unswapped
/// verifier and existing on-disk sidecars keep verifying (the P1a ordering invariant).
pub fn sign_binding(home: &Path, content: &[u8]) -> Result<String, String> {
    ensure_key(home)?;
    Ok(sign(home, content))
}

/// Wrap a bare hex MAC in the self-describing scheme envelope
/// (`<SCHEME_ID>:v<HMAC_ALGO_VERSION>:<HMAC_KEY_FORMAT>:<hex>`). This is the
/// CAPABILITY the migration will switch the default emit to in a later phase;
/// `sign_binding` does NOT emit it yet (byte-identical bare hex is the default).
/// [`verify`] already accepts the enveloped form.
pub fn envelope_tag(hex: &str) -> String {
    format!("{SCHEME_ID}:v{HMAC_ALGO_VERSION}:{HMAC_KEY_FORMAT}:{hex}")
}

/// #1934 cross-version pin: the HMAC-SHA256 output must be byte-identical
/// across the RustCrypto stack upgrade (hmac 0.12→0.13, sha2 0.10→0.11,
/// digest →0.11). The expected tag was generated BEFORE the upgrade and
/// cross-checked against an independent implementation (python hmac/hashlib)
/// — a tag change would silently invalidate every existing integrity sidecar
/// (#1576 fail-closed: all configs would read "not authentic" after deploy).
#[cfg(test)]
mod cross_version_pin_1934 {
    use super::*;
    use hmac::Mac;

    #[test]
    fn hmac_sha256_tag_is_stable_across_stack_upgrade() {
        const KEY: &[u8] = b"agend-1934-cross-version-fixture-key";
        const CONTENT: &[u8] =
            b"agend-1934 fixture content: integrity_core HMAC-SHA256 cross-version pin";
        // Generated on hmac 0.12.1 + sha2 0.10.9 (pre-upgrade), matches
        // python3 hmac.new(KEY, CONTENT, hashlib.sha256).hexdigest().
        const EXPECTED: &str = "80af9c21c0615da7849c54f1ba3ff9572061ac329d5f56455406b9317e8cc3fb";
        let mut mac = HmacSha256::new_from_slice(KEY).expect("HMAC accepts any key length");
        mac.update(CONTENT);
        assert_eq!(
            hex::encode(mac.finalize().into_bytes()),
            EXPECTED,
            "#1934: HMAC output changed across the RustCrypto upgrade — every \
             existing integrity sidecar would fail closed"
        );
    }
}

#[cfg(test)]
mod p1a_contract;
