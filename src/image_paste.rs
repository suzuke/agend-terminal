//! #2435 TUI image paste — surface-agnostic core.
//!
//! Captures an image from the operator's clipboard, encodes it to a PNG in the
//! system temp dir, and produces an `[AGEND-IMAGE-PASTE: <path>]` marker to
//! inject into an agent's input. The agent (taught by `crate::instructions`)
//! then `Read`s that path. Backend-agnostic: the agent only ever sees marker
//! text + a PNG file, so any backend with a `Read`-style tool works.
//!
//! ## Surface-agnostic by design
//! This module owns ONLY the clipboard→png→temp→marker logic — it binds NO
//! keystroke and picks NO TUI entry. `Ctrl+B i` is wired to BOTH surfaces by
//! their own dispatchers (operator chose both): the attach CLI (`crate::tui`,
//! via `BridgeClient`) and the `app` multi-pane TUI (`app::dispatch`, via
//! `write_to_focused`). Keeping the core here — not in either surface — is what
//! avoids the app-mode vs run_core/attach mode-mismatch class (#2434/#2438): the
//! same `capture_clipboard_image` serves both, and `app::dispatch`'s exhaustive
//! match compile-forces the app arm so the feature can't be live-in-attach-but-
//! dead-in-app.
//!
//! ## Cross-platform testing boundary ([xwin compile ≠ windows runtime])
//! `arboard` + the `png` encoder are cross-platform and compile for
//! windows-msvc (cargo-xwin verified). But the clipboard-READ step
//! (`arboard::get_image`) needs a real GUI clipboard holding an image, so it is
//! NOT reachable from headless CI — it is verified by a human paste on a real
//! desktop (a macOS arboard set→get→encode round-trip was run as manual proof).
//! Everything else here — PNG encode, temp write, the stale-file sweep, and the
//! marker chain — is pure/deterministic and unit-tested below, so the
//! windows-latest CI runtime gates it on Windows too.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Filename prefix for temp PNGs this feature writes — also what the cleanup
/// sweep matches. One constant so a rename can't desync writer and sweeper.
const PASTE_FILE_PREFIX: &str = "agend_paste_";

/// How long a pasted temp PNG lives before a later paste sweeps it. Generous —
/// the agent reads the file within seconds; this just bounds temp-dir growth
/// without risking the just-written file (age ≈ 0).
const PASTE_FILE_TTL: Duration = Duration::from_secs(3600);

/// The marker prefix the agent is taught to recognize (see `crate::instructions`).
const MARKER_PREFIX: &str = "[AGEND-IMAGE-PASTE: ";

/// Build the input marker for a pasted image. One function so the wire format
/// stays in lockstep with the `[AGEND-IMAGE-PASTE]` contract documented to
/// agents in `crate::instructions`.
fn image_paste_marker(path: &Path) -> String {
    format!("{MARKER_PREFIX}{}]\n", path.display())
}

/// Encode raw 8-bit RGBA bytes (`width*height*4` long) as a PNG in the system
/// temp dir; returns the absolute path. Uses the `png` crate directly — the
/// heavyweight `image` crate is deliberately NOT pulled (see Cargo.toml).
fn save_rgba_as_png(bytes: &[u8], width: usize, height: usize) -> anyhow::Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{PASTE_FILE_PREFIX}{ts}.png"));
    let file = std::fs::File::create(&path)?;
    let mut enc = png::Encoder::new(file, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(bytes)?;
    Ok(path)
}

/// Pure staleness decision, extracted so the TTL logic is unit-testable without
/// touching the filesystem or sleeping. A file is stale iff its age exceeds
/// `ttl`; a `modified` in the future (clock skew) → `duration_since` errs → not
/// stale (fail-safe: never delete a file that looks newer than now).
fn is_stale(modified: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    now.duration_since(modified)
        .map(|age| age > ttl)
        .unwrap_or(false)
}

/// Best-effort sweep of stale paste PNGs in `dir` older than `ttl`. Never errors
/// (missing dir / unreadable entry / failed remove is silently skipped) — temp
/// hygiene must never break the paste path.
fn cleanup_old_paste_images_in(dir: &Path, ttl: Duration) {
    let now = SystemTime::now();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(PASTE_FILE_PREFIX) && name.ends_with(".png")) {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).ok();
        if modified.is_some_and(|m| is_stale(m, now, ttl)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Surface-agnostic core: encode `width*height` RGBA `bytes` to a temp PNG and
/// return `(path, marker)` — the marker is what a caller injects into the
/// agent's input. Clipboard-independent (the bytes are passed in), so this is
/// the seam the representative wiring test drives with a synthetic image (the
/// real clipboard read is mocked out). Sweeps stale prior pastes first.
fn encode_and_mark(bytes: &[u8], width: usize, height: usize) -> anyhow::Result<(PathBuf, String)> {
    cleanup_old_paste_images_in(&std::env::temp_dir(), PASTE_FILE_TTL);
    let path = save_rgba_as_png(bytes, width, height)?;
    let marker = image_paste_marker(&path);
    Ok((path, marker))
}

/// Read an image from the system clipboard, save it as a PNG, and return
/// `(path, marker)` for the caller to inject into the attached agent. This is
/// the one not-headless-testable seam — `arboard::get_image` needs a real GUI
/// clipboard image (verified manually). Errors (no image / unreachable
/// clipboard) propagate so the caller can log + no-op rather than break the
/// session.
pub(crate) fn capture_clipboard_image() -> anyhow::Result<(PathBuf, String)> {
    let mut cb = arboard::Clipboard::new()?;
    let img = cb.get_image()?;
    encode_and_mark(&img.bytes, img.width, img.height)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// 2x2 RGBA (red, green, blue, white) — minimal synthetic image.
    fn synthetic_rgba() -> (usize, usize, Vec<u8>) {
        let px: [[u8; 4]; 4] = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        (2, 2, px.concat())
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-imgpaste-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Decode a PNG file's dimensions (png 0.18 needs `BufRead + Seek`).
    fn png_dims(path: &Path) -> (u32, u32) {
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let reader = dec.read_info().unwrap();
        let info = reader.info();
        (info.width, info.height)
    }

    #[test]
    fn save_rgba_writes_valid_png_in_temp() {
        let (w, h, rgba) = synthetic_rgba();
        let path = save_rgba_as_png(&rgba, w, h).expect("encode");
        assert!(path.exists());
        assert!(
            path.starts_with(std::env::temp_dir()),
            "writes into temp dir"
        );
        assert!(path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PASTE_FILE_PREFIX) && n.ends_with(".png")));
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..8], b"\x89PNG\r\n\x1a\n", "valid PNG signature");
        assert_eq!(png_dims(&path), (w as u32, h as u32));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn marker_matches_documented_contract() {
        let p = Path::new("/tmp/agend_paste_123.png");
        let m = image_paste_marker(p);
        // Must match the `[AGEND-IMAGE-PASTE: <path>]` form instructions.rs
        // teaches the agent, and end in a newline so it submits as one line.
        assert!(m.starts_with("[AGEND-IMAGE-PASTE: "));
        assert!(m.ends_with("]\n"));
        assert!(m.contains("/tmp/agend_paste_123.png"));
    }

    #[test]
    fn is_stale_decision_both_directions() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let ttl = Duration::from_secs(3600);
        // 2h old → stale.
        assert!(is_stale(now - Duration::from_secs(7200), now, ttl));
        // 1min old → fresh.
        assert!(!is_stale(now - Duration::from_secs(60), now, ttl));
        // future mtime (clock skew) → fail-safe not-stale.
        assert!(!is_stale(now + Duration::from_secs(60), now, ttl));
    }

    #[test]
    fn cleanup_removes_matching_stale_keeps_others() {
        let dir = unique_tmp_dir("cleanup");
        let stale = dir.join("agend_paste_old.png");
        let other_png = dir.join("unrelated.png"); // wrong prefix
        let other_ext = dir.join("agend_paste_note.txt"); // wrong ext
        for f in [&stale, &other_png, &other_ext] {
            std::fs::write(f, b"x").unwrap();
        }
        // ttl=0 → any matching file (age > 0) is stale. A brief pause makes the
        // age unambiguously > 0 without depending on clock granularity.
        std::thread::sleep(Duration::from_millis(15));
        cleanup_old_paste_images_in(&dir, Duration::ZERO);
        assert!(!stale.exists(), "matching stale PNG removed");
        assert!(other_png.exists(), "non-prefixed PNG untouched");
        assert!(other_ext.exists(), "non-PNG untouched");

        // With a generous ttl, a fresh matching file is kept.
        let fresh = dir.join("agend_paste_fresh.png");
        std::fs::write(&fresh, b"x").unwrap();
        cleanup_old_paste_images_in(&dir, Duration::from_secs(3600));
        assert!(fresh.exists(), "fresh matching PNG kept");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cleanup_on_missing_dir_is_noop() {
        // Never errors on a dir that doesn't exist.
        cleanup_old_paste_images_in(Path::new("/nonexistent/agend/imgpaste/xyz"), Duration::ZERO);
    }

    /// Representative WIRING test (lead reinforcement): drive the whole chain a
    /// real paste traverses — synthetic clipboard image → `encode_and_mark` →
    /// the injected marker → the path an agent would `Read` — with the
    /// clipboard read mocked (synthetic bytes stand in for `arboard::get_image`).
    /// Proves the pieces are actually wired together, not just individually
    /// correct: the marker the agent receives points at a real, readable PNG of
    /// the captured image.
    #[test]
    fn chain_synthetic_image_to_agent_readable_marker() {
        let (w, h, rgba) = synthetic_rgba();
        let (path, marker) = encode_and_mark(&rgba, w, h).expect("encode_and_mark");

        // The marker an agent receives must carry exactly this file's path.
        assert!(marker.starts_with("[AGEND-IMAGE-PASTE: "));
        assert!(marker.contains(&path.display().to_string()));

        // What the agent does: parse the path out of the marker and Read it.
        let parsed = marker
            .strip_prefix("[AGEND-IMAGE-PASTE: ")
            .and_then(|s| s.strip_suffix("]\n"))
            .expect("marker parses to a path");
        let agent_path = Path::new(parsed);
        assert_eq!(agent_path, path, "agent-visible path == written file");

        // That path must be a real, readable PNG of the captured image.
        let raw = std::fs::read(agent_path).expect("agent can read the file");
        assert_eq!(&raw[..8], b"\x89PNG\r\n\x1a\n", "agent reads a valid PNG");
        assert_eq!(png_dims(agent_path), (w as u32, h as u32));

        std::fs::remove_file(&path).ok();
    }
}
