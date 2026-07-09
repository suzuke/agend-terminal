//! API connection cookie authentication.
//!
//! Stage 8 / P1-10: TCP loopback has no OS-level peer-UID mechanism (unlike
//! UDS `SO_PEERCRED`), so any same-host process that can read the port file
//! could otherwise speak the daemon protocol. We gate each connection with a
//! 32-byte random cookie whose file is readable only by the daemon's user.
//! Security comes from filesystem permissions: mode 0600 on Unix, and default
//! NTFS inheritance under `%USERPROFILE%\.agend\run\<pid>\` on Windows (a
//! directory only the owner and SYSTEM can read).
//!
//! Handshake:
//! - NDJSON API: client sends `{"auth":"<hex>"}` as the first line; server
//!   replies `{"ok":true}` or `{"ok":false,"error":"auth"}` then closes.
//! - TUI framing: client sends 32 raw cookie bytes before the existing
//!   protocol-version byte.

use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::Path;

pub const COOKIE_LEN: usize = 32;
pub const COOKIE_FILE: &str = "api.cookie";
/// P0a (#2342 B4): the operator full-capability token — a SECOND per-daemon
/// secret, distinct from the shared agent `api.cookie`. The operator CLI/TUI
/// present THIS token (see `api::call`/`call_at`); the agent MCP bridge presents
/// only `api.cookie`. Holding it authenticates as [`Principal::Operator`] (full
/// capability); the cookie authenticates as [`Principal::Agent`] (MCP tunnel
/// only). 0600, per-boot fresh — see [`issue`].
pub const OPERATOR_TOKEN_FILE: &str = "api.operator";

pub type Cookie = [u8; COOKIE_LEN];

/// The authenticated principal a control-socket connection presented at
/// handshake — i.e. WHICH per-daemon secret it holds. Authority is proven by
/// this principal (a capability class), NEVER by the request's method-shape —
/// this closes the **method-shape / sidecar-agent-cookie subcase** of dev2 A1.
/// The **same-user-agent subcase** is NOT closed here — see
/// [`SAME_UID_OPERATOR_ISOLATION`]. Consumed by `api::operator_gate::capability_allows`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    /// Holder of the boot-minted operator full-capability token
    /// (`api.operator`) — the operator CLI/TUI. Full capability: every method.
    Operator,
    /// Holder of the shared agent cookie (`api.cookie`) — the MCP bridge.
    /// Capability = the MCP tunnel ONLY; every direct method is default-DENIED.
    Agent,
    // Future (#2342 Phase 2): a `Sidecar` principal whose sole capability is
    // `enqueue-to-responder-inbox`. Add the variant here — the exhaustive match
    // in `capability_allows` will then force its capability decision.
}

/// Status of the same-uid operator/agent SECRET-ISOLATION residual — dev2's P0a
/// review finding; tracking task `t-20260709010037959088-61315-1`.
///
/// P0a closes the **method-shape / sidecar-agent-cookie** subcase of dev2 A1 (an
/// entity holding only the agent cookie cannot reach the direct methods). It does
/// NOT close the **same-user-agent** subcase: `api.operator` lives in `run_dir`
/// mode 0600, and 0600 isolates cross-USER only — a same-uid agent (the future
/// #2342 Conversational **responder**, if prompt-injected) can
/// `read(run_dir/api.operator)`, present it, authenticate as [`Principal::Operator`],
/// and drive every direct method (`inject`/`spawn`/`delete`/…), bypassing the whole
/// capability model + Conversational ACL. Root cause: the principal is bound to a
/// *same-uid-readable secret*, not to caller IDENTITY (TCP loopback has no OS
/// peer-cred; the handshake `pid` is client-supplied and unauthenticated).
///
/// Closing it requires binding the principal to caller identity — OS peer-cred
/// (operator surface over a UDS + `SO_PEERCRED`), an out-of-band operator token
/// (never written to agent-readable disk), or per-agent private capability cookies.
/// It MUST NOT be considered satisfied by the no-tools sandbox alone (pillar-3 —
/// manifest §0.2 explicitly forbids leaning on it).
///
/// This is a **HARD prerequisite** for shipping a Conversational responder that
/// accepts inbound (Phase 2). The invariant test
/// `responder_inbound_requires_same_uid_isolation` (tests/p0a_capability_auth.rs)
/// couples the two: wiring a responder-inbound path while this is `Unresolved`
/// fails loud, so the prerequisite cannot be shipped around silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationStatus {
    /// Same-uid operator/agent secret isolation is NOT yet enforced. Safe ONLY
    /// while no same-uid Conversational responder accepts inbound.
    Unresolved,
    /// Caller identity is bound (peer-cred / out-of-band token / per-agent
    /// cookie); a same-uid agent can no longer impersonate the operator. Not yet
    /// constructed in production — the flip to this state lands with task
    /// t-20260709010037959088-61315-1 (the Phase-2 hard prerequisite).
    #[allow(dead_code)] // forward state: constructed when isolation is resolved (Phase 2).
    Resolved,
}

/// Current status of the same-uid operator/agent secret isolation. Flip to
/// [`IsolationStatus::Resolved`] ONLY when task `t-20260709010037959088-61315-1`
/// actually lands (identity-bound auth). The responder-inbound guard reads this.
pub const SAME_UID_OPERATOR_ISOLATION: IsolationStatus = IsolationStatus::Unresolved;

/// Issue BOTH per-daemon control-socket secrets, fresh, into `run_dir`:
///  - `api.cookie`   — the shared agent cookie (RETURNED so the caller can cache
///    an in-memory copy rather than re-reading on each connect).
///  - `api.operator` — the operator full-capability token (P0a #2342 B4).
///
/// Two INDEPENDENT 32-byte random secrets, each written 0600 (Unix) and fsynced
/// before rename. Every daemon boot path funnels through `issue` before the API
/// socket accepts, so both secrets are durably published before accept
/// (publish-before-accept) and rotate per boot. Callers needing the operator
/// token read it back via [`read_operator_token`].
pub fn issue(run_dir: &Path) -> Result<Cookie> {
    let cookie = issue_secret(run_dir, COOKIE_FILE)?;
    issue_secret(run_dir, OPERATOR_TOKEN_FILE)?;
    Ok(cookie)
}

/// Generate a fresh 32-byte secret and write it atomically to `{run_dir}/{file}`
/// with mode 0600 on Unix (fsync-before-rename). Returns the bytes.
fn issue_secret(run_dir: &Path, file: &str) -> Result<Cookie> {
    let mut secret = [0u8; COOKIE_LEN];
    // `getrandom::Error` implements `std::error::Error` in 0.3+.
    getrandom::fill(&mut secret).map_err(|e| anyhow!("getrandom: {e}"))?;
    let path = run_dir.join(file);
    let tmp = run_dir.join(format!(".{file}.tmp"));
    write_restricted(&tmp, &secret).with_context(|| format!("write {file} tmp"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {file}"))?;
    Ok(secret)
}

#[cfg(unix)]
fn write_restricted(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
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
    // Parent directory ({home}/run/<pid>/) is under %USERPROFILE% by default,
    // whose NTFS ACL restricts reads to the user and SYSTEM. No extra ACL
    // work here — if hardening is ever needed (e.g. shared profiles), call
    // CreateFile with a bespoke SECURITY_ATTRIBUTES instead.
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Read the shared agent cookie from `{run_dir}/api.cookie`. Enforces exact
/// 32-byte size so a truncated or padded file can't match by coincidence.
pub fn read_cookie(run_dir: &Path) -> Result<Cookie> {
    read_secret(run_dir, COOKIE_FILE)
}

/// Read the operator full-capability token from `{run_dir}/api.operator` (P0a
/// #2342 B4). Same exact-size enforcement as [`read_cookie`]. Missing/unreadable
/// → Err: operator clients fail CLOSED — they must refuse rather than silently
/// fall back to the shared cookie (which would authenticate as a mere Agent and
/// lock the operator out of its own direct methods).
pub fn read_operator_token(run_dir: &Path) -> Result<Cookie> {
    read_secret(run_dir, OPERATOR_TOKEN_FILE)
}

fn read_secret(run_dir: &Path, file: &str) -> Result<Cookie> {
    let mut f = File::open(run_dir.join(file)).with_context(|| format!("open {file}"))?;
    let mut bytes = [0u8; COOKIE_LEN];
    f.read_exact(&mut bytes)
        .with_context(|| format!("read {file}"))?;
    let mut extra = [0u8; 1];
    if f.read(&mut extra).unwrap_or(0) != 0 {
        return Err(anyhow!("{file}: unexpected trailing bytes"));
    }
    Ok(bytes)
}

/// Verify the auth cookie using constant-time comparison.
///
/// Defense-in-depth: although the API is localhost-only and same-user,
/// constant-time comparison prevents timing side-channels if the threat
/// model ever expands (e.g. container-shared localhost).
pub fn verify(expected: &Cookie, actual: &[u8]) -> bool {
    // H2: constant-time comparison to prevent timing side-channel
    if actual.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in actual.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

pub fn to_hex(cookie: &Cookie) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(COOKIE_LEN * 2);
    for b in cookie {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Option<Cookie> {
    if s.len() != COOKIE_LEN * 2 {
        return None;
    }
    let mut out = [0u8; COOKIE_LEN];
    let bytes = s.as_bytes();
    for i in 0..COOKIE_LEN {
        let h = hex_nibble(bytes[2 * i])?;
        let l = hex_nibble(bytes[2 * i + 1])?;
        out[i] = (h << 4) | l;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Server-side NDJSON handshake: reads the first line and verifies the presented
/// `auth` hex against the two per-daemon secrets, returning WHICH [`Principal`]
/// authenticated (P0a #2342 B4). Writes `{"ok":true}` on a match; on mismatch or
/// malformed input writes `{"ok":false,"error":"auth"}` and returns Err
/// (fail-closed — a connection presenting NEITHER secret is refused before any
/// method). Presented secret == `operator_token` → [`Principal::Operator`];
/// == `agent_cookie` → [`Principal::Agent`]. Also returns the optional peer PID
/// (telemetry only — the daemon does not poll it for liveness).
pub fn server_handshake_ndjson<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    operator_token: &Cookie,
    agent_cookie: &Cookie,
) -> Result<(Principal, Option<u32>)> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(anyhow!("auth: connection closed before handshake"));
    }
    let parsed: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(writer, r#"{{"ok":false,"error":"auth"}}"#);
            return Err(anyhow!("auth: parse error: {e}"));
        }
    };
    let hex = parsed.get("auth").and_then(|x| x.as_str()).unwrap_or("");
    let got = from_hex(hex);
    // Operator token checked first (a full-capability match wins). Both
    // comparisons are constant-time (`verify`); a rejected attempt runs both.
    let principal = match &got {
        Some(c) if verify(operator_token, c) => Principal::Operator,
        Some(c) if verify(agent_cookie, c) => Principal::Agent,
        _ => {
            let _ = writeln!(writer, r#"{{"ok":false,"error":"auth"}}"#);
            return Err(anyhow!("auth: bad cookie"));
        }
    };
    writeln!(writer, r#"{{"ok":true}}"#)?;
    writer.flush().ok();
    // Sprint 25 P1 F1: extract optional peer PID for telemetry.
    let peer_pid = parsed.get("pid").and_then(|v| v.as_u64()).map(|v| v as u32);
    Ok((principal, peer_pid))
}

/// Client-side NDJSON handshake: sends `{"auth":"<hex>"}`, reads one-line
/// reply, returns Ok iff the server reported `ok:true`.
pub fn client_handshake_ndjson<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cookie: &Cookie,
) -> Result<()> {
    writeln!(writer, r#"{{"auth":"{}"}}"#, to_hex(cookie))?;
    writer.flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(anyhow!("auth: server closed before reply"));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| anyhow!("auth: parse error: {e}"))?;
    if parsed.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        Err(anyhow!("auth: rejected"))
    }
}

/// TUI framing: client sends 32 raw bytes.
pub fn write_tui_auth<W: Write>(writer: &mut W, cookie: &Cookie) -> std::io::Result<()> {
    writer.write_all(cookie)?;
    writer.flush()
}

/// TUI framing: server reads 32 raw bytes and compares against `expected`.
pub fn read_and_verify_tui<R: Read>(reader: &mut R, expected: &Cookie) -> Result<()> {
    let mut got = [0u8; COOKIE_LEN];
    reader.read_exact(&mut got).context("tui auth read")?;
    if !verify(expected, &got) {
        return Err(anyhow!("tui auth failed"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-cookie-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn issue_then_read_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let written = issue(&dir).unwrap();
        let read = read_cookie(&dir).unwrap();
        assert_eq!(written, read);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn issued_cookies_are_not_all_zero_and_differ_across_calls() {
        let dir1 = tmp_dir("rand1");
        let dir2 = tmp_dir("rand2");
        let a = issue(&dir1).unwrap();
        let b = issue(&dir2).unwrap();
        assert_ne!(a, [0u8; COOKIE_LEN], "cookie must not be all-zero");
        // Two independent getrandom calls of 32 bytes colliding is negligibly
        // unlikely; if this ever flakes we have bigger problems than the test.
        assert_ne!(a, b, "two issued cookies should differ");
        std::fs::remove_dir_all(&dir1).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[cfg(unix)]
    #[test]
    fn issued_cookie_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("mode");
        issue(&dir).unwrap();
        let meta = std::fs::metadata(dir.join(COOKIE_FILE)).unwrap();
        // Mask owner/group/world bits only — ignore upper bits like setuid.
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn issued_operator_token_has_mode_0600() {
        // P0a: the operator full-capability token is a secret — it MUST be 0600,
        // same as the cookie (it is written via the same `write_restricted`).
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("op-mode");
        issue(&dir).unwrap();
        let meta = std::fs::metadata(dir.join(OPERATOR_TOKEN_FILE)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn operator_token_and_cookie_are_independent_secrets() {
        // If they collided, there would be no operator/agent separation.
        let dir = tmp_dir("distinct");
        let cookie = issue(&dir).unwrap();
        let read_cookie_back = read_cookie(&dir).unwrap();
        let operator = read_operator_token(&dir).unwrap();
        assert_eq!(cookie, read_cookie_back, "issue returns the agent cookie");
        assert_ne!(
            operator, cookie,
            "operator token must differ from the agent cookie"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_cookie_rejects_short_file() {
        let dir = tmp_dir("short");
        std::fs::write(dir.join(COOKIE_FILE), b"too short").unwrap();
        assert!(read_cookie(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_cookie_rejects_long_file() {
        let dir = tmp_dir("long");
        let mut long = vec![0u8; COOKIE_LEN];
        long.push(0);
        std::fs::write(dir.join(COOKIE_FILE), &long).unwrap();
        let err = read_cookie(&dir).unwrap_err();
        assert!(format!("{err}").contains("trailing"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_accepts_exact_match_and_rejects_everything_else() {
        let c: Cookie = [7u8; COOKIE_LEN];
        assert!(verify(&c, &c));
        let mut flipped = c;
        flipped[17] ^= 0x01;
        assert!(!verify(&c, &flipped));
        // Wrong length always fails, even if prefix matches.
        assert!(!verify(&c, &c[..COOKIE_LEN - 1]));
        let mut longer = vec![7u8; COOKIE_LEN];
        longer.push(0);
        assert!(!verify(&c, &longer));
    }

    #[test]
    fn hex_roundtrip_lower_and_upper() {
        let c: Cookie = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x00, 0xff, 0xaa, 0x55, 0x5a, 0xa5, 0x0f, 0xf0, 0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88,
        ];
        let hex = to_hex(&c);
        assert_eq!(hex.len(), 64);
        assert_eq!(from_hex(&hex), Some(c));
        assert_eq!(from_hex(&hex.to_uppercase()), Some(c));
        assert_eq!(from_hex("short"), None);
        assert_eq!(from_hex(&"z".repeat(64)), None);
    }

    // P0a: distinct operator token + agent cookie for the two-secret handshake.
    const OPERATOR: Cookie = [0x42; COOKIE_LEN];
    const AGENT: Cookie = [0x24; COOKIE_LEN];

    fn run_handshake(auth_hex: &str) -> (Result<(Principal, Option<u32>)>, String) {
        let payload = format!("{{\"auth\":\"{auth_hex}\"}}\n");
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(payload.into_bytes()));
        let mut writer: Vec<u8> = Vec::new();
        let res = server_handshake_ndjson(&mut reader, &mut writer, &OPERATOR, &AGENT);
        (res, String::from_utf8_lossy(&writer).into_owned())
    }

    #[test]
    fn ndjson_handshake_maps_operator_token_to_operator_principal() {
        let (res, reply) = run_handshake(&to_hex(&OPERATOR));
        let (principal, _pid) = res.unwrap();
        assert_eq!(principal, Principal::Operator);
        assert!(reply.contains("\"ok\":true"));
    }

    #[test]
    fn ndjson_handshake_maps_agent_cookie_to_agent_principal() {
        let (res, reply) = run_handshake(&to_hex(&AGENT));
        let (principal, _pid) = res.unwrap();
        assert_eq!(principal, Principal::Agent);
        assert!(reply.contains("\"ok\":true"));
    }

    #[test]
    fn ndjson_handshake_rejects_secret_matching_neither() {
        let stranger: Cookie = [0x99; COOKIE_LEN];
        let (res, reply) = run_handshake(&to_hex(&stranger));
        assert!(format!("{}", res.unwrap_err()).contains("auth"));
        assert!(reply.contains("\"ok\":false"));
    }

    #[test]
    fn ndjson_handshake_rejects_missing_auth_field() {
        let mut reader = std::io::BufReader::new(&b"{\"method\":\"list\"}\n"[..]);
        let mut writer: Vec<u8> = Vec::new();
        let err = server_handshake_ndjson(&mut reader, &mut writer, &OPERATOR, &AGENT).unwrap_err();
        assert!(format!("{err}").contains("auth"));
        assert!(String::from_utf8_lossy(&writer).contains("\"ok\":false"));
    }

    #[test]
    fn ndjson_handshake_rejects_empty_stream() {
        let mut reader = std::io::BufReader::new(&b""[..]);
        let mut writer: Vec<u8> = Vec::new();
        assert!(server_handshake_ndjson(&mut reader, &mut writer, &OPERATOR, &AGENT).is_err());
    }

    #[test]
    fn ndjson_client_then_server_pair_agrees() {
        let cookie: Cookie = [0x77; COOKIE_LEN];
        // Pipe client-side output into server-side input, and vice versa.
        let mut to_server: Vec<u8> = Vec::new();
        // Prepare client reader from pre-computed server reply.
        // Easiest: run server on client's output, capture reply, feed back.
        let mut client_writer: Vec<u8> = Vec::new();
        let mut dummy_reader = std::io::BufReader::new(&[] as &[u8]);
        // Client writes auth request to `client_writer`; we don't call
        // `client_handshake_ndjson` yet because it also wants to read a reply.
        writeln!(&mut client_writer, "{{\"auth\":\"{}\"}}", to_hex(&cookie)).unwrap();
        to_server.extend_from_slice(&client_writer);
        let mut server_reader = std::io::BufReader::new(&to_server[..]);
        let mut server_writer: Vec<u8> = Vec::new();
        // The client presents `cookie` as the agent cookie; a different value
        // stands in for the operator token so the two secrets are distinct.
        let operator: Cookie = [0x11; COOKIE_LEN];
        let (principal, _) =
            server_handshake_ndjson(&mut server_reader, &mut server_writer, &operator, &cookie)
                .unwrap();
        assert_eq!(principal, Principal::Agent);
        // Now feed server's reply back into client's reader.
        let mut client_reader = std::io::BufReader::new(&server_writer[..]);
        // Re-run client half against a fresh writer to exercise the success path.
        let mut ignored: Vec<u8> = Vec::new();
        client_handshake_ndjson(&mut client_reader, &mut ignored, &cookie).unwrap();
        // `dummy_reader` was unused; pacify the linter.
        let _ = dummy_reader.fill_buf();
    }

    /// Recursively test whether any `.rs` file under `dir` contains `needle`.
    fn rs_files_contain(dir: &std::path::Path, needle: &str) -> bool {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                    if let Ok(txt) = std::fs::read_to_string(&p) {
                        if txt.contains(needle) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// HARD-PREREQUISITE fail-loud guard (dev2's P0a review residual, tracking
    /// task t-20260709010037959088-61315-1).
    ///
    /// P0a leaves the **same-user-agent** subcase of dev2 A1 OPEN: a same-uid
    /// agent can read `api.operator` and impersonate the operator (see
    /// [`SAME_UID_OPERATOR_ISOLATION`]). That is safe ONLY while no same-uid
    /// Conversational responder accepts inbound. CONTRACT: any PR that wires such
    /// a responder MUST (a) place the responder-inbound contract marker (the
    /// `marker` value assembled below — split so this guard does not self-match)
    /// at the wiring site, and (b) resolve the isolation, flipping
    /// `SAME_UID_OPERATOR_ISOLATION` to `Resolved`. If the marker appears while
    /// isolation is still `Unresolved`, this guard fails loud — so the
    /// prerequisite cannot be shipped around silently, and in particular NOT by
    /// leaning on the no-tools sandbox (manifest §0.2).
    #[test]
    fn responder_inbound_requires_same_uid_isolation() {
        let marker = concat!("RESPONDER-", "INBOUND-LIVE");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let responder_inbound_wired = rs_files_contain(&src, marker);
        if responder_inbound_wired && SAME_UID_OPERATOR_ISOLATION != IsolationStatus::Resolved {
            panic!(
                "HARD PREREQ VIOLATED: a Conversational responder inbound path is wired \
                 (contract marker present) but SAME_UID_OPERATOR_ISOLATION is Unresolved. \
                 A same-uid prompt-injected responder can read api.operator and impersonate \
                 the operator, bypassing the whole capability model. Resolve tracking task \
                 t-20260709010037959088-61315-1 (UDS peer-cred / out-of-band operator token / \
                 per-agent capability cookie — NOT the no-tools sandbox, manifest §0.2), then \
                 set SAME_UID_OPERATOR_ISOLATION = IsolationStatus::Resolved."
            );
        }
    }

    #[test]
    fn tui_handshake_accepts_match_and_rejects_mismatch() {
        let cookie: Cookie = [0x5a; COOKIE_LEN];
        // Matching bytes pass.
        let mut ok_reader = &cookie[..];
        assert!(read_and_verify_tui(&mut ok_reader, &cookie).is_ok());
        // Flipped last byte fails.
        let mut bad = cookie;
        bad[COOKIE_LEN - 1] ^= 0xFF;
        let mut bad_reader = &bad[..];
        assert!(read_and_verify_tui(&mut bad_reader, &cookie).is_err());
        // Truncated stream errors.
        let mut short_reader = &cookie[..COOKIE_LEN - 1];
        assert!(read_and_verify_tui(&mut short_reader, &cookie).is_err());
    }
}
