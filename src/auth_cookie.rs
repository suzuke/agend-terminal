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

pub type Cookie = [u8; COOKIE_LEN];

/// Generate a fresh 32-byte cookie and write it atomically to
/// `{run_dir}/api.cookie` with mode 0600 on Unix. Returns the bytes so the
/// caller can keep an in-memory copy rather than re-reading on each connect.
pub fn issue(run_dir: &Path) -> Result<Cookie> {
    let mut cookie = [0u8; COOKIE_LEN];
    // `getrandom::Error` does not implement `std::error::Error` in 0.2, so
    // `.context()` can't decorate it — map through anyhow! manually.
    getrandom::getrandom(&mut cookie).map_err(|e| anyhow!("getrandom: {e}"))?;
    let path = run_dir.join(COOKIE_FILE);
    let tmp = run_dir.join(format!(".{COOKIE_FILE}.tmp"));
    write_restricted(&tmp, &cookie).context("write api.cookie tmp")?;
    std::fs::rename(&tmp, &path).context("rename api.cookie")?;
    Ok(cookie)
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

/// Read the cookie from `{run_dir}/api.cookie`. Enforces exact 32-byte size
/// so a truncated or padded file can't match by coincidence.
pub fn read_cookie(run_dir: &Path) -> Result<Cookie> {
    let mut f = File::open(run_dir.join(COOKIE_FILE)).context("open api.cookie")?;
    let mut bytes = [0u8; COOKIE_LEN];
    f.read_exact(&mut bytes).context("read api.cookie")?;
    let mut extra = [0u8; 1];
    if f.read(&mut extra).unwrap_or(0) != 0 {
        return Err(anyhow!("api.cookie: unexpected trailing bytes"));
    }
    Ok(bytes)
}

/// Constant-time 32-byte equality. Avoids short-circuit branches that could
/// leak the matched-prefix length over time. We don't pull in `subtle` for
/// a single fixed-size compare.
pub fn verify(expected: &Cookie, actual: &[u8]) -> bool {
    if actual.len() != COOKIE_LEN {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..COOKIE_LEN {
        diff |= expected[i] ^ actual[i];
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

/// Server-side NDJSON handshake: reads the first line, verifies `auth`
/// matches `expected`, writes `{"ok":true}` on success. Returns Err (and
/// writes a JSON error reply) on mismatch or malformed input.
pub fn server_handshake_ndjson<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    expected: &Cookie,
) -> Result<Option<u32>> {
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
    let ok = matches!(&got, Some(c) if verify(expected, c));
    if !ok {
        let _ = writeln!(writer, r#"{{"ok":false,"error":"auth"}}"#);
        return Err(anyhow!("auth: bad cookie"));
    }
    writeln!(writer, r#"{{"ok":true}}"#)?;
    writer.flush().ok();
    // Sprint 25 P1 F1: extract optional peer PID for telemetry.
    // Telemetry only: daemon does not poll this PID for liveness;
    // see Sprint 25 P3 follow-up (active peer-process invalidation).
    let peer_pid = parsed.get("pid").and_then(|v| v.as_u64()).map(|v| v as u32);
    Ok(peer_pid)
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

    #[test]
    fn ndjson_handshake_accepts_correct_cookie() {
        let cookie: Cookie = [0x42; COOKIE_LEN];
        let payload = format!("{{\"auth\":\"{}\"}}\n", to_hex(&cookie));
        let mut reader = std::io::BufReader::new(payload.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        server_handshake_ndjson(&mut reader, &mut writer, &cookie).unwrap();
        assert!(String::from_utf8_lossy(&writer).contains("\"ok\":true"));
    }

    #[test]
    fn ndjson_handshake_rejects_wrong_cookie() {
        let expected: Cookie = [0x42; COOKIE_LEN];
        let presented: Cookie = [0x43; COOKIE_LEN];
        let payload = format!("{{\"auth\":\"{}\"}}\n", to_hex(&presented));
        let mut reader = std::io::BufReader::new(payload.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let err = server_handshake_ndjson(&mut reader, &mut writer, &expected).unwrap_err();
        assert!(format!("{err}").contains("auth"));
        assert!(String::from_utf8_lossy(&writer).contains("\"ok\":false"));
    }

    #[test]
    fn ndjson_handshake_rejects_missing_auth_field() {
        let cookie: Cookie = [0x42; COOKIE_LEN];
        let mut reader = std::io::BufReader::new(&b"{\"method\":\"list\"}\n"[..]);
        let mut writer: Vec<u8> = Vec::new();
        let err = server_handshake_ndjson(&mut reader, &mut writer, &cookie).unwrap_err();
        assert!(format!("{err}").contains("auth"));
        assert!(String::from_utf8_lossy(&writer).contains("\"ok\":false"));
    }

    #[test]
    fn ndjson_handshake_rejects_empty_stream() {
        let cookie: Cookie = [0x42; COOKIE_LEN];
        let mut reader = std::io::BufReader::new(&b""[..]);
        let mut writer: Vec<u8> = Vec::new();
        assert!(server_handshake_ndjson(&mut reader, &mut writer, &cookie).is_err());
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
        server_handshake_ndjson(&mut server_reader, &mut server_writer, &cookie).unwrap();
        // Now feed server's reply back into client's reader.
        let mut client_reader = std::io::BufReader::new(&server_writer[..]);
        // Re-run client half against a fresh writer to exercise the success path.
        let mut ignored: Vec<u8> = Vec::new();
        client_handshake_ndjson(&mut client_reader, &mut ignored, &cookie).unwrap();
        // `dummy_reader` was unused; pacify the linter.
        let _ = dummy_reader.fill_buf();
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
