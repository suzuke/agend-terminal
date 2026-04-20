//! CLI → supervisor client helpers.
//!
//! The `agend-terminal upgrade` subcommand uses this module to:
//!
//! 1. Detect whether a supervisor is running (socket exists + connectable).
//! 2. Hash + stage the new binary into `$AGEND_HOME/bin/store/`.
//! 3. Swap the `current` symlink atomically.
//! 4. Send `Request::Upgrade` to the supervisor, stream progress, surface
//!    the final outcome.
//!
//! Failures before the symlink swap are safe (nothing changed). Failures
//! after the swap are the supervisor's responsibility to roll back.

use super::ipc::{self, Request, Response, UpgradeArgs, UpgradeStage, WIRE_VERSION};
use super::paths;
use anyhow::{Context, Result};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// Returns `Ok(Some(pid))` if a supervisor appears to be running and
/// responsive; `Ok(None)` if no supervisor socket exists.
#[cfg(unix)]
pub fn probe(home: &Path) -> Result<Option<u32>> {
    let sock = paths::supervisor_sock(home);
    if !sock.exists() {
        return Ok(None);
    }
    let stream = match ipc::uds::connect(&sock) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Stale socket file — supervisor died without cleanup. Treat
            // as "not running" so the caller can decide whether to
            // bootstrap a new one.
            return Ok(None);
        }
        Err(e) => return Err(anyhow::Error::from(e).context("connect supervisor socket")),
    };
    let resp = send_recv_single(stream, &Request::Ping)?;
    match resp {
        Response::Ok { data, .. } => {
            let pid = data
                .as_ref()
                .and_then(|v| v.get("pid"))
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            Ok(pid)
        }
        Response::Err { error, .. } => {
            anyhow::bail!("supervisor ping returned error: {error}")
        }
        Response::Progress { .. } => {
            anyhow::bail!("supervisor ping returned unexpected progress frame")
        }
    }
}

#[cfg(not(unix))]
pub fn probe(_home: &Path) -> Result<Option<u32>> {
    anyhow::bail!("supervisor IPC is Unix-only; upgrade is not supported on this platform")
}

/// Hash the binary at `src`, copy it into the content-addressed store at
/// `$AGEND_HOME/bin/store/<hash>`, and return the hex hash.
///
/// Idempotent: if the store already has the same hash we skip the copy.
pub fn stage_binary(home: &Path, src: &Path) -> Result<String> {
    let bytes = std::fs::read(src).with_context(|| format!("read new binary {}", src.display()))?;
    let hash = sha256_hex(&bytes);

    let store = paths::bin_store_dir(home);
    std::fs::create_dir_all(&store)
        .with_context(|| format!("create store dir {}", store.display()))?;

    let dest = paths::stored_binary(home, &hash);
    if !dest.exists() {
        // Write to a temp file in the same directory, then atomic rename.
        let tmp = store.join(format!(".{hash}.part"));
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("write staged binary {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&tmp, perms)
                .with_context(|| format!("chmod staged binary {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("commit staged binary {}", dest.display()))?;
    }
    Ok(hash)
}

/// Read whichever binary `current` currently points at, hash it, and make
/// sure it's staged in `bin/store/` so the supervisor can use it as the
/// rollback target. Returns the hex hash.
///
/// Errors if `current` is missing or dangling.
pub fn stage_current_as_prev(home: &Path) -> Result<String> {
    let current = paths::current_link(home);
    let target = std::fs::read_link(&current)
        .with_context(|| format!("read symlink {}", current.display()))?;
    // `current` is a relative symlink inside bin/; resolve it.
    let abs = if target.is_absolute() {
        target
    } else {
        paths::bin_dir(home).join(target)
    };
    stage_binary(home, &abs)
}

/// Atomic swap of the `current` symlink to the staged binary identified by
/// `new_hash`, simultaneously repointing `prev` to whatever `current` used
/// to point at. Both links are written via rename(2) so the filesystem
/// never observes a half-updated state.
#[cfg(unix)]
pub fn swap_current(home: &Path, new_hash: &str, prev_hash: &str) -> Result<()> {
    use std::os::unix::fs::symlink;

    let bin = paths::bin_dir(home);
    std::fs::create_dir_all(&bin).with_context(|| format!("create bin dir {}", bin.display()))?;

    // Relative targets keep the symlinks portable if $AGEND_HOME is moved.
    let new_rel = PathBuf::from("store").join(new_hash);
    let prev_rel = PathBuf::from("store").join(prev_hash);

    // 1. Update `prev` first — it's the rollback target. Write to a temp
    //    link, rename over.
    let prev_tmp = bin.join(".prev.new");
    let _ = std::fs::remove_file(&prev_tmp);
    symlink(&prev_rel, &prev_tmp)
        .with_context(|| format!("create prev symlink {}", prev_tmp.display()))?;
    std::fs::rename(&prev_tmp, paths::prev_link(home)).context("rename prev symlink into place")?;

    // 2. Update `current`.
    let cur_tmp = bin.join(".current.new");
    let _ = std::fs::remove_file(&cur_tmp);
    symlink(&new_rel, &cur_tmp)
        .with_context(|| format!("create current symlink {}", cur_tmp.display()))?;
    std::fs::rename(&cur_tmp, paths::current_link(home))
        .context("rename current symlink into place")?;

    Ok(())
}

#[cfg(not(unix))]
pub fn swap_current(_home: &Path, _new_hash: &str, _prev_hash: &str) -> Result<()> {
    anyhow::bail!("symlink-based upgrade swap is Unix-only")
}

/// Run `new_binary --version` as a basic sanity check. Returns the
/// reported version string (or a short placeholder if the binary doesn't
/// support `--version`).
pub fn probe_new_binary_version(binary: &Path) -> Result<String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("spawn {} --version", binary.display()))?;
    if !out.status.success() {
        anyhow::bail!("{} --version exited with {}", binary.display(), out.status);
    }
    let txt = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if txt.is_empty() {
        "(unknown)".into()
    } else {
        txt
    })
}

/// Run `AGEND_SELF_TEST=1 new_binary` and check it exits 0. The new binary
/// performs its own smoke tests via [`super::self_test::run`].
pub fn run_self_test(binary: &Path, home: &Path) -> Result<()> {
    let out = std::process::Command::new(binary)
        .env("AGEND_SELF_TEST", "1")
        .env("AGEND_HOME", home)
        .output()
        .with_context(|| format!("spawn self-test of {}", binary.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "self-test of {} failed ({}): {}",
            binary.display(),
            out.status,
            stderr.trim()
        );
    }
    Ok(())
}

/// Send an upgrade request over the supervisor socket and stream progress
/// frames to `progress_cb` until the terminal response arrives.
#[cfg(unix)]
pub fn send_upgrade<F>(home: &Path, args: UpgradeArgs, mut progress_cb: F) -> Result<Response>
where
    F: FnMut(UpgradeStage, &str),
{
    let sock = paths::supervisor_sock(home);
    let stream = ipc::uds::connect(&sock).context("connect supervisor socket")?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(300)))
        .ok();

    let (reader, mut writer) = (stream.try_clone().context("clone stream")?, stream);
    ipc::write_one(&mut writer, &Request::Upgrade(args)).context("send upgrade request")?;

    let mut br = BufReader::new(reader);
    loop {
        let resp: Response = match ipc::read_one::<Response, _>(&mut br)? {
            Some(r) => r,
            None => anyhow::bail!("supervisor closed socket before terminal response"),
        };
        match resp {
            Response::Progress {
                stage,
                ref message,
                version,
            } => {
                check_version(version)?;
                progress_cb(stage, message);
            }
            Response::Ok {
                version, r#final, ..
            } => {
                check_version(version)?;
                if r#final {
                    return Ok(resp);
                }
                // Non-final Ok — treat as ack; keep reading.
            }
            Response::Err { version, .. } => {
                check_version(version)?;
                return Ok(resp);
            }
        }
    }
}

#[cfg(not(unix))]
pub fn send_upgrade<F>(_home: &Path, _args: UpgradeArgs, _cb: F) -> Result<Response>
where
    F: FnMut(UpgradeStage, &str),
{
    anyhow::bail!("supervisor IPC is Unix-only");
}

/// Send a request that expects a single non-streaming response (Ping, Status).
#[cfg(unix)]
fn send_recv_single(stream: std::os::unix::net::UnixStream, req: &Request) -> Result<Response> {
    let reader = stream.try_clone().context("clone stream")?;
    let mut writer = stream;
    ipc::write_one(&mut writer, req).context("write request")?;
    let mut br = BufReader::new(reader);
    let resp: Response = ipc::read_one::<Response, _>(&mut br)?
        .ok_or_else(|| anyhow::anyhow!("supervisor closed socket without response"))?;
    // For single-shot requests, Progress frames are a protocol error.
    if let Response::Progress { stage, .. } = &resp {
        anyhow::bail!("supervisor returned unexpected progress frame ({stage:?})");
    }
    if let Response::Ok { version, .. } | Response::Err { version, .. } = &resp {
        check_version(*version)?;
    }
    Ok(resp)
}

fn check_version(v: u32) -> Result<()> {
    if v > WIRE_VERSION {
        anyhow::bail!(
            "supervisor wire version {v} is newer than client ({WIRE_VERSION}); upgrade your CLI"
        );
    }
    Ok(())
}

/// Daemon-side: send a Ready ping to the supervisor if we're running under
/// one (i.e. `AGEND_SUPERVISOR_SOCK` is set in the environment).
///
/// Called after the daemon has finished booting — agents spawned, API socket
/// bound — so the supervisor's `wait_for_ready` phase can advance. Silent
/// no-op if the env var is missing (i.e. the daemon was started directly,
/// not under a supervisor).
///
/// Errors are intentionally not propagated as the daemon's main-line control
/// flow: a missing or dead supervisor shouldn't take the daemon down. The
/// caller logs and moves on.
#[cfg(unix)]
pub fn notify_ready(pid: u32, version: &str) -> Result<()> {
    let sock_path = match std::env::var_os("AGEND_SUPERVISOR_SOCK") {
        Some(v) => PathBuf::from(v),
        None => return Ok(()),
    };
    let stream = ipc::uds::connect(&sock_path)
        .with_context(|| format!("connect supervisor ready socket {}", sock_path.display()))?;
    let resp = send_recv_single(
        stream,
        &Request::Ready {
            pid,
            version: version.to_string(),
        },
    )?;
    match resp {
        Response::Ok { .. } => Ok(()),
        Response::Err { error, .. } => anyhow::bail!("supervisor rejected ready ping: {error}"),
        Response::Progress { .. } => unreachable!("send_recv_single already rejects Progress"),
    }
}

#[cfg(not(unix))]
pub fn notify_ready(_pid: u32, _version: &str) -> Result<()> {
    Ok(())
}

// --- sha256 ----------------------------------------------------------------
//
// Tiny pure-Rust SHA-256 — avoids pulling `sha2` just for one call site.
// Constant-time / side-channel behavior doesn't matter here; this is a
// content hash, not a MAC.

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bitlen = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// Silence unused warnings on non-unix where the connection helpers are stubs.
#[allow(dead_code)]
fn _reader_type_hint<R: Read>(_: &mut BufReader<R>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_long_input() {
        // Stress the multi-block path.
        let input = vec![b'x'; 1024];
        let h = sha256_hex(&input);
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_nist_multi_block_vector() {
        // FIPS 180-4 Appendix B.2: two-block message.
        // Proves the hand-rolled impl handles the 56-byte → multi-block
        // padding boundary correctly (not just literal-string tests).
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(input),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn stage_binary_idempotent() {
        let home = std::env::temp_dir().join(format!("agend-client-stage-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        let src = home.join("fake-bin");
        std::fs::write(&src, b"#!/bin/sh\necho hi\n").ok();

        let h1 = stage_binary(&home, &src).expect("stage 1");
        let h2 = stage_binary(&home, &src).expect("stage 2 (idempotent)");
        assert_eq!(h1, h2);
        assert!(paths::stored_binary(&home, &h1).exists());

        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn swap_current_and_prev_repoint_atomically() {
        let home = std::env::temp_dir().join(format!(
            "agend-client-swap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(paths::bin_store_dir(&home)).ok();

        // Two distinct binaries staged.
        let a = home.join("src-a");
        let b = home.join("src-b");
        std::fs::write(&a, b"AAAA").ok();
        std::fs::write(&b, b"BBBB").ok();
        let ha = stage_binary(&home, &a).expect("stage a");
        let hb = stage_binary(&home, &b).expect("stage b");
        assert_ne!(ha, hb);

        // Seed bin/current → store/<ha> so swap has something to replace.
        use std::os::unix::fs::symlink;
        let seed_target = std::path::PathBuf::from("store").join(&ha);
        symlink(&seed_target, paths::current_link(&home)).expect("seed current");

        swap_current(&home, &hb, &ha).expect("swap");

        let cur = std::fs::read_link(paths::current_link(&home)).expect("read current");
        assert_eq!(cur, std::path::PathBuf::from("store").join(&hb));
        let prev = std::fs::read_link(paths::prev_link(&home)).expect("read prev");
        assert_eq!(prev, std::path::PathBuf::from("store").join(&ha));

        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn stage_current_as_prev_follows_symlink() {
        let home = std::env::temp_dir().join(format!(
            "agend-client-current-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(paths::bin_store_dir(&home)).ok();

        let src = home.join("src");
        std::fs::write(&src, b"payload-v1").ok();
        let hash = stage_binary(&home, &src).expect("stage");
        use std::os::unix::fs::symlink;
        symlink(
            std::path::PathBuf::from("store").join(&hash),
            paths::current_link(&home),
        )
        .expect("symlink current");

        let staged = stage_current_as_prev(&home).expect("stage current as prev");
        assert_eq!(
            staged, hash,
            "staging the same bytes must produce the same hash"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
