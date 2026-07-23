//! Embedder P1a contract tests for `integrity_core`: the byte-identical ordering
//! invariant, the scheme envelope + typed `verify` (reviewer4 acceptance
//! conditions #2/#3/#4), `sign_binding` provisioning (reviewer4 hole), and the
//! first-writer-wins key race regression.

use super::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_home(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("agcore-p1a-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Write a fixed key so signing is deterministic.
fn write_key(home: &Path, key: &[u8]) {
    std::fs::write(key_path(home), key).unwrap();
}

// ── The ORDERING INVARIANT (fleet-safety): default emit is BARE hex, byte- ──
// ── identical to the pre-P1a signer; an unswapped verifier keeps verifying. ──

#[test]
fn sign_binding_emits_byte_identical_bare_hex() {
    // Independent oracle (python hmac.new([7]*32, CONTENT, sha256).hexdigest()),
    // mirroring the #1934 golden-fixture methodology. If `sign_binding` ever emits
    // the envelope by default (the ordering landmine), the equality + no-colon
    // assertions go RED.
    let home = tmp_home("byte-id");
    write_key(&home, &[7u8; 32]);
    const CONTENT: &[u8] =
        b"agend-p1a byte-identical fixture: sign_binding emits bare hex (implicit scheme v1)";
    const GOLDEN: &str = "38a831c3949dfecc48223256eab15f9c59022454a822e95546450d3b7c8c20c3";

    let sig = sign_binding(&home, CONTENT).expect("sign_binding");
    assert_eq!(sig, GOLDEN, "sign_binding must be byte-identical bare hex");
    assert!(!sig.contains(':'), "default emit must be BARE hex, not the envelope");
    assert_eq!(sig.len(), 64, "HMAC-SHA256 hex is 64 chars");
    // the low-level signer agrees, and the legacy verify path accepts it.
    assert_eq!(sign(&home, CONTENT), sig, "sign_binding == low-level sign");
    assert_eq!(verify(&home, CONTENT, &sig), Ok(()), "legacy bare-hex verify accepts");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn sign_binding_provisions_then_signs_no_panic() {
    // reviewer4 hole: the low-level `sign` panics on a fresh home; `sign_binding`
    // must provision the key FIRST, then sign — no panic, and the output verifies.
    let home = tmp_home("provision");
    assert!(read_key(&home).is_none(), "fresh home has no key");
    let sig = sign_binding(&home, b"body").expect("sign_binding must provision + sign");
    assert_eq!(read_key(&home).map(|k| k.len()), Some(32), "key now provisioned");
    assert_eq!(verify(&home, b"body", &sig), Ok(()));
    let _ = std::fs::remove_dir_all(&home);
}

// ── Envelope capability + backward-compat (reviewer4 cond #4: bare hex accepted) ──

#[test]
fn verify_accepts_bare_hex_legacy_and_v1_envelope_roundtrip() {
    let home = tmp_home("roundtrip");
    write_key(&home, &[9u8; 32]);
    let hex = sign(&home, b"payload");
    // cond #4: a no-prefix bare-hex tag verifies (the legacy window).
    assert_eq!(verify(&home, b"payload", &hex), Ok(()));
    // the envelope capability round-trips: `envelope_tag` output verifies too.
    let env = envelope_tag(&hex);
    assert_eq!(env, format!("ag-hmac-sha256:v1:raw:{hex}"));
    assert_eq!(verify(&home, b"payload", &env), Ok(()), "v1 envelope must verify");
    let _ = std::fs::remove_dir_all(&home);
}

// ── reviewer4 cond #2: a malformed PREFIXED tag must NEVER fall back to legacy ──

#[test]
fn verify_malformed_prefixed_tag_never_falls_to_legacy() {
    let home = tmp_home("malformed");
    write_key(&home, &[3u8; 32]);
    let valid = sign(&home, b"data"); // a hex that WOULD verify as bare-hex legacy

    // The load-bearing case: wrapping the valid hex in a MALFORMED envelope must
    // NOT silently retry it as bare-hex legacy (that would be a fail-open bypass).
    let cases = [
        format!("ag-hmac-sha256:v1:raw:{valid}:extra"), // extra colon / field
        format!("ag-hmac-sha256:x1:raw:{valid}"),       // non-`v<n>` algo
        "ag-hmac-sha256:v1:raw:zznothex".to_string(),   // non-hex MAC
        format!("ag-hmac-sha256:{valid}"),              // missing algo/key-format fields
        "garbage:prefix:xyz".to_string(),               // foreign prefix (has colon)
    ];
    for c in cases {
        let r = verify(&home, b"data", &c);
        assert!(r.is_err(), "malformed prefixed tag must fail closed: {c:?} -> {r:?}");
        assert_ne!(r, Ok(()), "must NEVER authenticate: {c:?}");
    }
    // unknown key-format is a recognized-but-unsupported scheme.
    assert!(matches!(
        verify(&home, b"data", &format!("ag-hmac-sha256:v1:weird:{valid}")),
        Err(VerifyError::UnsupportedScheme { .. })
    ));
    let _ = std::fs::remove_dir_all(&home);
}

// ── reviewer4 cond #3: downgrade / newer-scheme / tamper must NEVER authenticate ──

#[test]
fn verify_unsupported_newer_scheme_and_tamper_never_ok() {
    let home = tmp_home("downgrade");
    write_key(&home, &[5u8; 32]);
    let hex = sign(&home, b"msg");

    // a NEWER algo the runtime doesn't implement → UnsupportedScheme BEFORE any MAC
    // check (fires even though the MAC bytes are otherwise valid), never Ok.
    let newer = format!("ag-hmac-sha256:v2:raw:{hex}");
    assert!(matches!(
        verify(&home, b"msg", &newer),
        Err(VerifyError::UnsupportedScheme { tag_scheme, runtime_scheme })
            if tag_scheme == "ag-hmac-sha256:v2:raw" && runtime_scheme == "ag-hmac-sha256:v1:raw"
    ));

    // tamper: a bit-flipped MAC (bare AND enveloped) → MacMismatch, never Ok.
    let mut bytes = hex::decode(&hex).unwrap();
    bytes[0] ^= 0x01;
    let flipped = hex::encode(&bytes);
    assert_eq!(verify(&home, b"msg", &flipped), Err(VerifyError::MacMismatch));
    assert_eq!(
        verify(&home, b"msg", &envelope_tag(&flipped)),
        Err(VerifyError::MacMismatch),
        "a wrong MAC wrapped in a valid envelope still fails the MAC check"
    );
    // right MAC, wrong content → MacMismatch.
    assert_eq!(verify(&home, b"other", &hex), Err(VerifyError::MacMismatch));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn verify_missing_key_is_typed() {
    let home = tmp_home("nokey");
    assert_eq!(verify(&home, b"x", "deadbeef"), Err(VerifyError::MissingKey));
    let _ = std::fs::remove_dir_all(&home);
}

// ── Regression: `ensure_key` first-writer-wins under concurrency (direct core) ──

#[test]
fn ensure_key_concurrent_threads_one_key_survives() {
    let home = tmp_home("race");
    let n = 16;
    let results: Vec<_> = std::thread::scope(|s| {
        (0..n)
            .map(|_| {
                let h = home.clone();
                s.spawn(move || ensure_key(&h))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|j| j.join().unwrap())
            .collect()
    });
    assert!(results.iter().all(|r| r.is_ok()), "every racer succeeds: {results:?}");
    // exactly ONE 32-byte key survives, and it reads back cleanly.
    let key = read_key(&home).expect("a key must exist after the race");
    assert_eq!(key.len(), 32);
    assert_eq!(
        std::fs::metadata(key_path(&home)).unwrap().len(),
        32,
        "one 32-byte key survives — no partial/torn file"
    );
    // idempotent: a follow-up call reuses it, bytes unchanged.
    ensure_key(&home).unwrap();
    assert_eq!(read_key(&home), Some(key));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ensure_key_refuses_corrupt_size_fail_closed() {
    let home = tmp_home("corrupt");
    write_key(&home, &[1u8; 16]); // wrong size
    assert!(
        ensure_key(&home).is_err(),
        "a wrong-size key must be a hard Err (refuse to overwrite), not a silent regen"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Δ2 regression (moved from the run CLI with `open_new_0600` in P1a): the temp
/// key file must be 0600 FROM BIRTH — reverting to `fs::write` + post-hoc chmod
/// turns this RED (the freshly created file would carry umask-default perms at
/// open time). And `create_new` must refuse a pre-planted path.
#[cfg(unix)]
#[test]
fn key_tmp_file_is_0600_at_creation() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp_home("0600");
    let p = home.join("k.tmp");
    let f = open_new_0600(&p).expect("create");
    let mode = f.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "temp key must be born 0600, got {mode:o}");
    assert!(open_new_0600(&p).is_err(), "create_new must refuse existing path");
    let _ = std::fs::remove_dir_all(&home);
}
