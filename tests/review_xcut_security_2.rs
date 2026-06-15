//! xcut-security batch — interim defense-in-depth guard for the
//! "stolen api.cookie ⇒ full operator authority" residual risk.
//!
//! Finding (info / documented threat model): `operator_gate::check_operation_allowed`
//! grants FULL operator authority to EVERY non-`mcp_tool`/`mcp_tools_list`
//! method unconditionally (`if !is_agent_transport { return Ok(()) }`). The
//! entire operator-authority model therefore rests on a single secret: the
//! `api.cookie` file (mode 0600). There is no SECOND factor binding a
//! direct-method (operator) connection to the real operator process — so any
//! same-user process that obtains the cookie (compromised dependency, a cookie
//! leaked into a log / bug report — see the quickstart token-leak finding, or a
//! co-tenant in a shared-UID container) can authenticate and drive ANY direct
//! method (spawn / delete / shutdown) with full authority, bypassing
//! operator-mode gating entirely.
//!
//! The defense-in-depth fix is to bind the operator connection to a
//! KERNEL-VERIFIED peer credential. Today the only "peer PID" the daemon has is
//! SELF-REPORTED by the client in the NDJSON handshake
//! (`server_handshake_ndjson` does `parsed.get("pid")`) and is used for
//! telemetry / liveness ONLY — a stolen cookie can trivially forge it. A real
//! second factor must come from the OS (`SO_PEERCRED` / `getpeereid` /
//! `peer_cred`), which the code does NOT do anywhere today (the sole mention of
//! `SO_PEERCRED` is a comment in `auth_cookie.rs` explaining TCP loopback lacks
//! it).
//!
//! This is the structural reason the finding is `redesign_required`: the gate
//! does not even RECEIVE a verified credential, so the runtime authority gap
//! cannot be closed without an architecture change (see redesign_note in the
//! manifest). As the mandated interim guard, this SOURCE-SCANNING invariant
//! pins the precise defect: the peer PID feeding any trust/authority decision
//! must NOT be the client-self-reported handshake value. The bad pattern is the
//! `parsed.get("pid")` extraction in `server_handshake_ndjson` with no
//! accompanying kernel peer-credential call.
//!
//! RED now: `auth_cookie.rs` derives the peer PID from `parsed.get("pid")`
//! (self-reported) and no `SO_PEERCRED`/`getpeereid`/`peer_cred` verification
//! exists anywhere in `src/`. GREEN after a defense-in-depth fix introduces a
//! kernel-verified peer credential for operator (direct-method) connections.

use std::path::{Path, PathBuf};

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

#[test]
#[ignore = "cookie-only-operator-authority: deferred — operator WONTFIX, single-user trust model (decision d-20260615162503510062-0)"]
fn operator_connection_uses_kernel_verified_peer_credential_xcut_security() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    // (a) The defense-in-depth second factor: a kernel-verified peer credential
    //     must exist somewhere in the daemon (any one of these is sufficient).
    //     Today none do (the sole `SO_PEERCRED` hit is a doc comment, which we
    //     exclude below by requiring a non-comment code line).
    let verified_cred_markers = [
        "SO_PEERCRED",
        "getpeereid",
        "peer_cred",
        "LOCAL_PEERCRED",
        "PEERCRED",
    ];

    let mut has_kernel_verified_cred = false;
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read src file");
        for line in text.lines() {
            let t = line.trim_start();
            // Only count it if it is real code, not a doc / comment line
            // (the existing `SO_PEERCRED` mention is a `//!` module-doc line).
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            if verified_cred_markers.iter().any(|m| line.contains(m)) {
                has_kernel_verified_cred = true;
            }
        }
    }

    // (b) The bad pattern that the fix must replace: the operator/peer PID is
    //     taken from the CLIENT-SELF-REPORTED handshake payload. A forged `pid`
    //     under a stolen cookie defeats any trust placed in it.
    let auth = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/auth_cookie.rs");
    let auth_text = std::fs::read_to_string(&auth).expect("read src/auth_cookie.rs");
    let self_reported_pid_present = auth_text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("//") || t.starts_with('*'))
        })
        .any(|l| l.contains("parsed.get(\"pid\")"));

    assert!(
        has_kernel_verified_cred,
        "operator-authority rests on the api.cookie ALONE: there is no \
         kernel-verified peer credential (SO_PEERCRED / getpeereid / peer_cred) \
         anywhere in src/. A stolen cookie + a forged self-reported `pid` grants \
         full operator authority (spawn/delete/shutdown) with no second factor. \
         Bind direct-method (operator) connections to a kernel-verified peer \
         credential. (self_reported_pid_in_handshake={self_reported_pid_present})"
    );
}
