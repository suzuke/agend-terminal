//! M2 (#2090) — smart progress mirror via transcript tail.
//!
//! Tails an agent's Claude Code transcript (`~/.claude/projects/<encoded-cwd>/
//! <session-uuid>.jsonl`) and extracts NEW assistant *text* blocks so the
//! progress-mirror per-tick handler can relay clean CLI-parity updates to the
//! origin channel — without mirroring raw PTY/ANSI/tool noise.
//!
//! Design constraints:
//!
//! - **No backlog replay.** First sight of a session seeds the tail offset at
//!   the file's current length, so only text produced from now on is mirrored.
//! - **Complete lines only.** A partially-written final line (no trailing
//!   `\n`) is left for the next tick; the offset advances only past consumed
//!   `\n`-terminated bytes.
//! - **Fail-open.** Any IO/parse error yields an empty vec — never panic a
//!   tick (this runs inside the daemon main loop's per-tick sweep).

use crate::backend::claude_session::{default_projects_root, newest_session_jsonl};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Per-agent tail position: which transcript file, and how many bytes of it we
/// have already consumed.
#[derive(Debug, Clone)]
pub(crate) struct TailPos {
    pub path: PathBuf,
    pub offset: u64,
}

/// Extract assistant text blocks appended to `working_dir`'s newest transcript
/// since the last call. Advances `*pos`. Returns the new text blocks in file
/// order; empty on first sight (offset seeded to EOF), no-new-bytes, or any
/// error.
pub(crate) fn extract_new_assistant_text(
    working_dir: &Path,
    pos: &mut Option<TailPos>,
) -> Vec<String> {
    let Some(path) = newest_session_jsonl(working_dir, &default_projects_root()) else {
        return Vec::new();
    };

    // First sight of this session (or a session rollover to a new file): seed
    // the offset at the current file length and return nothing — we only
    // mirror text produced from now on, never the historical backlog.
    let is_new_session = match pos {
        Some(p) => p.path != path,
        None => true,
    };
    if is_new_session {
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        *pos = Some(TailPos { path, offset: len });
        return Vec::new();
    }

    // Safe: is_new_session is false ⇒ pos is Some.
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
        // Only consume COMPLETE lines: if the last read did not end in '\n',
        // it is a partially-written record — leave it for the next tick.
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

/// Parse one jsonl line; if it is a top-level `type == "assistant"` record,
/// append every `message.content[]` entry whose `type == "text"` (non-empty
/// after trim) to `out`, preserving order. Anything else (user/thinking/
/// tool_use/tool_result/parse error) is silently skipped — fail-open.
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A real Claude jsonl assistant line carries `type:"assistant"` at the top
    /// level and a `message.content[]` array of typed blocks.
    fn assistant_line(blocks: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{blocks}]}}}}\n"
        )
    }

    fn text_block(t: &str) -> String {
        format!("{{\"type\":\"text\",\"text\":\"{t}\"}}")
    }

    fn unique_jsonl(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-transcript-tail-test-{}-{}-{}",
            std::process::id(),
            label,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("session.jsonl")
    }

    fn append(path: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    /// Direct-path variant of `extract_new_assistant_text` that takes the
    /// transcript path explicitly (bypasses the `~/.claude/projects` lookup so
    /// the tail logic is testable in a temp dir). Mirrors the production logic
    /// exactly except for path resolution.
    fn extract_from(path: &Path, pos: &mut Option<TailPos>) -> Vec<String> {
        let is_new = match pos {
            Some(p) => p.path != path,
            None => true,
        };
        if is_new {
            let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            *pos = Some(TailPos {
                path: path.to_path_buf(),
                offset: len,
            });
            return Vec::new();
        }
        let start = pos.as_ref().map(|p| p.offset).unwrap_or(0);
        let Ok(file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(start)).unwrap();
        let mut consumed = 0u64;
        let mut out = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if line.last() != Some(&b'\n') {
                break;
            }
            consumed += n as u64;
            if let Ok(t) = std::str::from_utf8(&line) {
                collect_assistant_text(t, &mut out);
            }
        }
        if let Some(p) = pos.as_mut() {
            p.offset = start.saturating_add(consumed);
        }
        out
    }

    #[test]
    fn first_sight_returns_empty_even_with_existing_text() {
        let path = unique_jsonl("first-sight");
        append(&path, &assistant_line(&text_block("already here")));
        let mut pos: Option<TailPos> = None;
        // First sight seeds offset at EOF: no historical backlog replay.
        assert!(extract_from(&path, &mut pos).is_empty());
        assert!(pos.is_some());
        assert_eq!(
            pos.as_ref().unwrap().offset,
            std::fs::metadata(&path).unwrap().len()
        );
    }

    #[test]
    fn appended_assistant_text_blocks_returned_tool_use_skipped() {
        let path = unique_jsonl("append");
        append(&path, &assistant_line(&text_block("seed")));
        let mut pos: Option<TailPos> = None;
        assert!(
            extract_from(&path, &mut pos).is_empty(),
            "first sight empty"
        );

        // Append one assistant record with two text blocks + one tool_use.
        let blocks = format!(
            "{},{},{}",
            text_block("first update"),
            "{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{}}",
            text_block("second update")
        );
        append(&path, &assistant_line(&blocks));

        let got = extract_from(&path, &mut pos);
        assert_eq!(got, vec!["first update", "second update"]);
    }

    #[test]
    fn no_new_bytes_returns_empty() {
        let path = unique_jsonl("no-new");
        append(&path, &assistant_line(&text_block("seed")));
        let mut pos: Option<TailPos> = None;
        extract_from(&path, &mut pos); // first sight
        append(&path, &assistant_line(&text_block("update")));
        assert_eq!(extract_from(&path, &mut pos), vec!["update"]);
        // Third call, no new bytes appended.
        assert!(extract_from(&path, &mut pos).is_empty());
    }

    #[test]
    fn user_line_and_thinking_block_ignored() {
        let path = unique_jsonl("ignore");
        append(&path, &assistant_line(&text_block("seed")));
        let mut pos: Option<TailPos> = None;
        extract_from(&path, &mut pos); // first sight

        // A user-type line.
        append(
            &path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        );
        // An assistant line whose only block is a thinking block.
        append(
            &path,
            &assistant_line("{\"type\":\"thinking\",\"thinking\":\"hmm\"}"),
        );
        assert!(
            extract_from(&path, &mut pos).is_empty(),
            "user line + thinking block must be ignored"
        );

        // Sanity: a real text block after still comes through.
        append(&path, &assistant_line(&text_block("visible")));
        assert_eq!(extract_from(&path, &mut pos), vec!["visible"]);
    }
}
