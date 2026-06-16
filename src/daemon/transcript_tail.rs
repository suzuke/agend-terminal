//! #2090 M2 (mode-1 mirror) — extract NEW assistant text from a Claude Code
//! transcript tail.
//!
//! Tails an agent's Claude Code transcript (`~/.claude/projects/<encoded-cwd>/
//! <session-uuid>.jsonl`) and returns the assistant *text* blocks appended since
//! the last call, so the progress-mirror per-tick handler can relay clean,
//! ANSI-free CLI-parity updates to the origin channel — without the raw
//! PTY/ANSI/tool noise.
//!
//! Exfil-relevant invariants (the mirror's safety rests on these):
//! - **No backlog replay.** First sight of a session seeds the tail offset at
//!   the file's current length, so only text produced from *now on* is mirrored
//!   — an enabled mirror never dumps the historical transcript to the channel.
//! - **Complete lines only.** A partially-written final line (no trailing `\n`)
//!   is left for the next tick; the offset advances only past consumed
//!   `\n`-terminated bytes.
//! - **Fail-open.** Any IO/parse error yields an empty vec — never panics a tick
//!   (this runs inside the daemon per-tick sweep).

use crate::backend::claude_session::{default_projects_root, newest_session_jsonl};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Per-agent tail position: which transcript file, and how many bytes of it have
/// already been consumed.
#[derive(Debug, Clone)]
pub(crate) struct TailPos {
    pub path: PathBuf,
    pub offset: u64,
}

/// Extract assistant text blocks appended to `working_dir`'s newest transcript
/// since the last call. Advances `*pos`. Returns the new text blocks in file
/// order; empty on first sight (offset seeded to EOF), no-new-bytes, or any
/// error. `projects_root` is injected so tests drive a hermetic transcript dir.
pub(crate) fn extract_new_assistant_text_in(
    working_dir: &Path,
    projects_root: &Path,
    pos: &mut Option<TailPos>,
) -> Vec<String> {
    let Some(path) = newest_session_jsonl(working_dir, projects_root) else {
        return Vec::new();
    };

    // First sight of this session (or a rollover to a new session file): seed the
    // offset at the current file length and return nothing — only text produced
    // from now on is mirrored, never the historical backlog.
    let is_new_session = match pos {
        Some(p) => p.path != path,
        None => true,
    };
    if is_new_session {
        // F2: seed at the CURRENT file length. A metadata Err must NOT fall back
        // to offset 0 — that would replay the ENTIRE file (the whole backlog) on
        // the next tick. On transient failure, leave `*pos` unchanged (None / the
        // prior session) so the seed is retried next tick; never seed 0.
        let Some(len) = current_eof(&path) else {
            return Vec::new();
        };
        *pos = Some(TailPos { path, offset: len });
        return Vec::new();
    }

    // is_new_session == false ⇒ pos is Some.
    let start_offset = match pos {
        Some(p) => p.offset,
        None => return Vec::new(),
    };

    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(start_offset)).is_err() {
        return Vec::new();
    }

    let mut consumed: u64 = 0;
    let mut out: Vec<String> = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(_) => break, // fail-open: stop, keep offset at last good line
        };
        // Only consume COMPLETE lines: a final line without `\n` is a
        // partially-written record — leave it for the next tick.
        if line.last() != Some(&b'\n') {
            break;
        }
        consumed += n as u64;
        if let Ok(text) = std::str::from_utf8(&line) {
            collect_assistant_text(text, &mut out);
        }
    }

    if let Some(p) = pos.as_mut() {
        p.offset = start_offset.saturating_add(consumed);
    }
    out
}

/// Production entry: tail under the real `~/.claude/projects` root.
pub(crate) fn extract_new_assistant_text(
    working_dir: &Path,
    pos: &mut Option<TailPos>,
) -> Vec<String> {
    extract_new_assistant_text_in(working_dir, &default_projects_root(), pos)
}

/// Current EOF (byte length) of `path`, or `None` on any metadata error. F2:
/// callers must treat `None` as "retry the seed next tick", NEVER as offset 0 —
/// a 0 seed would replay the entire file on the next tick.
fn current_eof(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

/// Parse one jsonl line; if it is a top-level `type == "assistant"` record,
/// append every `message.content[]` entry whose `type == "text"` (non-empty
/// after trim) to `out`, preserving order. Anything else (user / thinking /
/// tool_use / tool_result / parse error) is silently skipped — fail-open.
fn collect_assistant_text(line: &str, out: &mut Vec<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return;
    }
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            if !text.trim().is_empty() {
                out.push(text.to_string());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::backend::claude_session::encode_project_dir;
    use std::io::Write;

    /// A hermetic projects root + a project dir matching `working_dir`'s encoding,
    /// returning (projects_root, working_dir, jsonl_path).
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("agend-2090-tail-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let projects_root = base.join("projects");
        // working_dir must canonicalize stably → use the (existing) base dir.
        let working_dir = base.join("wt");
        std::fs::create_dir_all(&working_dir).unwrap();
        let canonical = dunce::canonicalize(&working_dir).unwrap();
        let project_dir = projects_root.join(encode_project_dir(&canonical));
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl = project_dir.join("session.jsonl");
        std::fs::write(&jsonl, b"").unwrap();
        (projects_root, working_dir, jsonl)
    }

    fn assistant_line(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
        )
    }

    fn append(path: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn first_sight_seeds_eof_no_backlog_replay() {
        let (root, wt, jsonl) = fixture("first-sight");
        // Pre-existing backlog the mirror must NOT replay.
        append(&jsonl, &assistant_line("OLD backlog line"));
        let mut pos = None;
        let out = extract_new_assistant_text_in(&wt, &root, &mut pos);
        assert!(out.is_empty(), "first sight must replay nothing: {out:?}");
        assert!(pos.is_some(), "offset seeded");
        std::fs::remove_dir_all(jsonl.parent().unwrap().parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn extracts_only_new_assistant_text_after_seed() {
        let (root, wt, jsonl) = fixture("new-only");
        append(&jsonl, &assistant_line("old"));
        let mut pos = None;
        extract_new_assistant_text_in(&wt, &root, &mut pos); // seed
        append(&jsonl, &assistant_line("hello"));
        append(&jsonl, "{\"type\":\"user\",\"message\":{}}\n"); // skipped
        append(&jsonl, &assistant_line("world"));
        let out = extract_new_assistant_text_in(&wt, &root, &mut pos);
        assert_eq!(out, vec!["hello".to_string(), "world".to_string()]);
        // Second call with no new bytes → empty.
        assert!(extract_new_assistant_text_in(&wt, &root, &mut pos).is_empty());
        std::fs::remove_dir_all(jsonl.parent().unwrap().parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn partial_final_line_held_until_complete() {
        let (root, wt, jsonl) = fixture("partial");
        let mut pos = None;
        extract_new_assistant_text_in(&wt, &root, &mut pos); // seed at 0
                                                             // Append a line WITHOUT trailing newline (mid-write).
        let l = assistant_line("partial");
        append(&jsonl, l.trim_end_matches('\n')); // no '\n'
        assert!(
            extract_new_assistant_text_in(&wt, &root, &mut pos).is_empty(),
            "partial line must be held"
        );
        // Now complete it.
        append(&jsonl, "\n");
        assert_eq!(
            extract_new_assistant_text_in(&wt, &root, &mut pos),
            vec!["partial".to_string()]
        );
        std::fs::remove_dir_all(jsonl.parent().unwrap().parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn missing_session_is_empty_noop() {
        let base =
            std::env::temp_dir().join(format!("agend-2090-tail-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut pos = None;
        assert!(
            extract_new_assistant_text_in(&base.join("wt"), &base.join("projects"), &mut pos)
                .is_empty()
        );
    }

    /// F2: a metadata failure must yield `None` (→ "retry the seed next tick"),
    /// NEVER offset 0 (which would replay the whole file). `current_eof` of a
    /// nonexistent path is `None`; of a real file it is the byte length.
    #[test]
    fn current_eof_none_on_missing_never_zero() {
        let base = std::env::temp_dir().join(format!("agend-2090-eof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let missing = base.join("nope.jsonl");
        assert_eq!(
            current_eof(&missing),
            None,
            "missing path must be None, not 0"
        );
        std::fs::create_dir_all(&base).unwrap();
        let f = base.join("f.jsonl");
        std::fs::write(&f, b"abcde").unwrap();
        assert_eq!(current_eof(&f), Some(5));
        std::fs::remove_dir_all(&base).ok();
    }

    /// F2 end-to-end: a first-sight seed whose `current_eof` is unavailable must
    /// leave `pos` unset (None) so the NEXT tick re-seeds — it must never seed 0
    /// and replay backlog. Modelled by a project dir whose `.jsonl` vanishes
    /// between the dir-listing and the metadata read is hard to force
    /// deterministically; the `current_eof`-None contract above + the
    /// `let Some(len) = current_eof(..) else { return }` guard pin the behaviour.
    #[test]
    fn first_sight_does_not_seed_when_eof_unavailable() {
        // `pos` stays None when there is no session file at all (the analogous
        // "no usable EOF" path), so nothing is ever seeded to 0.
        let base = std::env::temp_dir().join(format!("agend-2090-noseed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut pos = None;
        let out = extract_new_assistant_text_in(&base.join("wt"), &base.join("projects"), &mut pos);
        assert!(out.is_empty());
        assert!(
            pos.is_none(),
            "no seed → pos stays None (re-seed next tick), never offset 0"
        );
    }

    #[test]
    fn non_text_blocks_skipped() {
        let (root, wt, jsonl) = fixture("nontext");
        let mut pos = None;
        extract_new_assistant_text_in(&wt, &root, &mut pos);
        // thinking + tool_use blocks must not be mirrored; only text.
        append(&jsonl, "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"secret\"},{\"type\":\"text\",\"text\":\"shown\"},{\"type\":\"tool_use\",\"name\":\"x\"}]}}\n");
        assert_eq!(
            extract_new_assistant_text_in(&wt, &root, &mut pos),
            vec!["shown".to_string()]
        );
        std::fs::remove_dir_all(jsonl.parent().unwrap().parent().unwrap().parent().unwrap()).ok();
    }
}
