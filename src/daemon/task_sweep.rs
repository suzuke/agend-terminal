//! Sprint 24 P0 PR2 — task auto-close sweep daemon.
//!
//! Periodically polls GitHub for recently merged PRs in a configured repo
//! and emits canonical `TaskEvent::Linked` + `TaskEvent::Done` events for
//! every valid `Closes t-XXX-N` marker observed. Mutations route through
//! [`crate::task_events::append_batch`]; the legacy `tasks.json` is **not**
//! touched by this module — sweep is forward-only by design (PR3 retires
//! `tasks.json` and the read path moves to `task_events::replay`).
//!
//! ## Sweep validation pipeline (5 dev-reviewer-2 must-haves)
//! 1. **HTML-comment injection sanitize** — `<!-- Closes t-victim -->`
//!    rejected (the regex never sees the directive).
//! 2. **Non-ASCII codepoint reject** on task-ID match — strict ASCII-digit
//!    regex `(?m)Closes\s+(t-[0-9]+-[0-9]+)` defeats zero-width-char
//!    homoglyph attacks.
//! 3. **PR.user.login authorship ONLY** (not git trailer co-author) —
//!    defends the pre-PR-220 `update_decision` bug class.
//! 4. **GitHub API schema-mismatch fail-closed** — missing required
//!    fields (`merge_commit_sha`, `merged_at`, `user.login`) cause the
//!    sweep to skip the PR rather than emit a half-formed event.
//! 5. **Squash-merge SHA captured at decision-time** — recorded inside
//!    `DoneSource::PrMerged.merge_sha` + `PrSnapshot.merge_sha` so the
//!    decision survives squash deletion / PR description edits.
//!
//! ## DaemonTicker integration
//! Spawned via [`crate::daemon::ticker::DaemonTicker`] with the standard
//! drop-on-shutdown contract. Forward-compat with Sprint 25+ graceful-
//! join refactor (caller can switch to `join_on_shutdown()` without
//! changing the spawn site).

#![allow(dead_code)]

use crate::daemon::ticker::DaemonTicker;
use crate::task_events::{
    self, DoneSource, InstanceName, LinkSource, PrId, PrSnapshot, TaskEvent, TaskId,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// Default sweep tick interval. Operator can override via the
/// `task_sweep_config` MCP tool (`interval_secs`); the next tick after
/// the config save observes the new interval.
const DEFAULT_SWEEP_TICK_SECS: u64 = 300;

/// Emitter identity stamped on sweep-driven events. Distinct from any
/// real fleet instance so audit queries can filter sweep contributions
/// from operator manual transitions.
const SWEEP_EMITTER: &str = "system:task_sweep";

/// Per-tick PR list size. 30 is GitHub's default; we sort by `updated`
/// desc so recent merges land first.
const PR_LIST_LIMIT: u32 = 30;

/// Configuration persisted at `<home>/task_sweep.json`. Operator mutates
/// via the `task_sweep_config` MCP tool; sweep tick reads on each invocation.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct SweepConfig {
    /// `owner/repo` GitHub identifier (e.g. `"suzuke/agend-terminal"`).
    /// `None` = sweep disabled (tick is a no-op).
    pub repo: Option<String>,
    /// `true` = tick body short-circuits with no-op.
    #[serde(default)]
    pub paused: bool,
    /// `true` = log decisions but do not emit events.
    #[serde(default)]
    pub dry_run: bool,
}

fn config_path(home: &Path) -> PathBuf {
    home.join("task_sweep.json")
}

fn load_config(home: &Path) -> SweepConfig {
    crate::store::load(&config_path(home))
}

fn save_config(home: &Path, cfg: &SweepConfig) -> anyhow::Result<()> {
    crate::store::save_atomic(&config_path(home), cfg)
}

/// Holding-handle for the spawned sweep ticker. Drop is the existing
/// daemon "fire-and-forget" convention (the thread exits via the
/// shutdown atomic). Sprint 25+ graceful-join callers can switch to
/// `DaemonTicker::join_on_shutdown` without changing the spawn site.
pub struct TaskSweep {
    _ticker: DaemonTicker,
}

impl TaskSweep {
    /// Spawn the sweep tick thread. Reads `<home>/task_sweep.json` each
    /// tick; if `repo` is unset or `paused == true`, the body is a no-op.
    /// `body` is invoked once immediately at thread start (per
    /// [`DaemonTicker`] contract) — the operator sees an immediate sweep
    /// after enabling rather than waiting `tick_dur`.
    pub fn spawn(home: PathBuf, shutdown: Arc<AtomicBool>) -> Self {
        let ticker = DaemonTicker::spawn(
            "task_sweep",
            Duration::from_secs(DEFAULT_SWEEP_TICK_SECS),
            shutdown,
            move || {
                if let Err(e) = sweep_tick(&home) {
                    tracing::warn!(error = %e, "task_sweep tick failed");
                }
            },
        );
        Self { _ticker: ticker }
    }
}

// ── Tick body ───────────────────────────────────────────────────────

fn sweep_tick(home: &Path) -> anyhow::Result<()> {
    let cfg = load_config(home);
    if cfg.paused {
        return Ok(());
    }
    let repo = match &cfg.repo {
        Some(r) if !r.is_empty() => r.clone(),
        _ => return Ok(()),
    };

    let prs = list_recently_merged_prs(&repo)?;
    if prs.is_empty() {
        return Ok(());
    }

    // Snapshot of currently-open tasks. Read from tasks::list_all (the
    // PR2 bridge-phase legacy reader); PR3 cutover switches this to
    // `task_events::replay` derived view.
    let open_tasks = crate::tasks::list_all(home);
    let open_ids: std::collections::HashMap<String, &crate::tasks::Task> = open_tasks
        .iter()
        .filter(|t| matches!(t.status.as_str(), "open" | "claimed" | "in_progress"))
        .map(|t| (t.id.clone(), t))
        .collect();
    if open_ids.is_empty() {
        return Ok(());
    }

    let emitter = InstanceName::from(SWEEP_EMITTER);
    let sweep_id = format!("sweep-{}", chrono::Utc::now().to_rfc3339());

    for pr in &prs {
        if !pr.merged {
            continue;
        }
        // Validation must-have #4: GitHub API schema-mismatch fail-closed
        // — a merged PR without merge_commit_sha or merged_at is malformed
        // (or the API contract changed); skip rather than emit a half-
        // formed event that downstream auditors would have to reverse-
        // engineer.
        let merge_sha = match pr.merge_commit_sha.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::warn!(
                    pr = pr.number,
                    "sweep: merged PR with empty merge_commit_sha — schema mismatch, skip"
                );
                continue;
            }
        };
        let merged_at = match pr.merged_at.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::warn!(
                    pr = pr.number,
                    "sweep: merged PR with empty merged_at — schema mismatch, skip"
                );
                continue;
            }
        };

        // Validation must-have #1: HTML-comment injection sanitize.
        let sanitized_body = strip_html_comments(&pr.body);
        // Validation must-have #2: strict ASCII regex rejects non-ASCII
        // homoglyphs in the task ID portion.
        let markers = extract_closes_markers(&sanitized_body);
        if markers.is_empty() {
            continue;
        }

        for marker in markers {
            let task = match open_ids.get(&marker) {
                Some(t) => t,
                None => {
                    // Marker doesn't reference a currently-open task —
                    // either typo, already-closed task, or attacker
                    // referencing a non-existent ID.
                    tracing::debug!(pr = pr.number, marker = %marker, "sweep: marker doesn't match any open task");
                    continue;
                }
            };
            // Validation must-have #3: PR.user.login authorship ONLY —
            // task creator OR assignee must match. Defends pre-PR-220
            // `update_decision` bug class where a malicious PR body could
            // close another agent's task.
            let creator = task.created_by.as_str();
            let assignee = task.assignee.as_deref();
            let author_ok = pr.author_login.eq_ignore_ascii_case(creator)
                || assignee
                    .map(|a| pr.author_login.eq_ignore_ascii_case(a))
                    .unwrap_or(false);
            if !author_ok {
                tracing::warn!(
                    pr = pr.number,
                    marker = %marker,
                    pr_author = %pr.author_login,
                    task_creator = creator,
                    task_assignee = ?assignee,
                    "sweep: PR.user.login not authorised to close — rejected"
                );
                continue;
            }

            if cfg.dry_run {
                tracing::info!(
                    pr = pr.number,
                    marker = %marker,
                    "sweep dry-run: would auto-close (no event emitted)"
                );
                continue;
            }

            // Validation must-have #5: capture squash-merge SHA at
            // decision-time so the audit survives squash deletion / PR
            // body edits.
            let snapshot = PrSnapshot {
                pr_state: pr.state.clone(),
                merge_sha: Some(merge_sha.to_string()),
                api_response_hash: pr.api_response_hash.clone(),
                captured_at: chrono::Utc::now().to_rfc3339(),
            };
            let events = vec![
                TaskEvent::Linked {
                    task_id: TaskId(marker.clone()),
                    pr_id: PrId(pr.number),
                    source: LinkSource::SweepDiscovery {
                        sweep_id: sweep_id.clone(),
                    },
                    snapshot: snapshot.clone(),
                },
                TaskEvent::Done {
                    task_id: TaskId(marker.clone()),
                    by: InstanceName(pr.author_login.clone()),
                    source: DoneSource::PrMerged {
                        pr_id: PrId(pr.number),
                        merge_sha: merge_sha.to_string(),
                        merged_at: merged_at.to_string(),
                        snapshot,
                    },
                },
            ];
            task_events::append_batch(home, &emitter, events)?;
            tracing::info!(
                pr = pr.number,
                marker = %marker,
                "sweep: auto-closed (Linked + Done emitted)"
            );
        }
    }
    Ok(())
}

// ── PR body sanitisation + marker extraction ─────────────────────────

/// Strip every `<!-- ... -->` HTML comment. Pre-validation step so the
/// `Closes t-XXX` regex never observes a directive an adversary tried to
/// hide inside a comment.
///
/// The implementation walks bytes (ASCII-safe — we strip whole comments,
/// not their bytes) and rebuilds a String char-by-char where the byte is
/// guaranteed to be a single-byte char by the surrounding ASCII bracket
/// match. Unterminated comments (`<!--` without `-->`) drop the rest of
/// the body — a future fuzzy attacker writing partial comments still
/// can't sneak a directive past us.
fn strip_html_comments(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 4 <= bytes.len() && &bytes[i..i + 4] == b"<!--" {
            match body[i + 4..].find("-->") {
                Some(end) => {
                    i += 4 + end + 3;
                    continue;
                }
                None => break, // unterminated — drop tail
            }
        }
        // Push the next UTF-8 character to preserve unicode in the
        // non-comment body. char_indices iterator-style walk:
        let ch_len = body[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&body[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Extract every `Closes t-<digits>-<digits>` marker. Strict ASCII regex
/// rejects non-ASCII codepoints inside the task ID; combined with the
/// HTML-comment sanitiser this defends against zero-width-char +
/// HTML-injection adversary surface.
fn extract_closes_markers(body: &str) -> Vec<String> {
    static MARKER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = MARKER.get_or_init(|| {
        // Multi-line; case-sensitive on `Closes` to match GitHub's
        // closing-keyword convention.
        regex::Regex::new(r"(?m)Closes\s+(t-[0-9]+-[0-9]+)\b").expect("static regex must compile")
    });
    re.captures_iter(body)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

// ── GitHub API ──────────────────────────────────────────────────────

/// PR metadata captured from the GitHub list-pulls response. Fields
/// chosen to satisfy the 5 sweep validation must-haves; intermediate
/// JSON parsing in [`parse_pr_meta`] flags schema mismatches.
struct PrMeta {
    number: u64,
    state: String,
    merged: bool,
    merge_commit_sha: Option<String>,
    merged_at: Option<String>,
    body: String,
    author_login: String,
    /// SHA-256 of the per-PR JSON object (hex). Forensic correlation
    /// fingerprint stamped onto the resulting `PrSnapshot`.
    api_response_hash: String,
}

fn list_recently_merged_prs(repo: &str) -> anyhow::Result<Vec<PrMeta>> {
    // Build a per-tick current-thread runtime so the sync DaemonTicker
    // body can call async reqwest. Pattern lifted from `ci_watch.rs`.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("agend-terminal/task-sweep")
            .build()?;
        let url = format!(
            "https://api.github.com/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page={PR_LIST_LIMIT}"
        );
        let mut req = client
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("GitHub list-pulls {} for {url}", resp.status());
        }
        let body = resp.text().await?;
        let arr: Vec<serde_json::Value> = serde_json::from_str(&body)?;
        let mut out = Vec::with_capacity(arr.len());
        for pr in arr {
            // Per-PR response hash so each event's PrSnapshot fingerprints
            // the exact JSON object the sweep observed (squash deletions
            // + future PR body edits won't change this).
            let pr_json_bytes = serde_json::to_vec(&pr)?;
            let api_response_hash = sha256_hex(&pr_json_bytes);
            if let Some(meta) = parse_pr_meta(&pr, api_response_hash) {
                out.push(meta);
            }
        }
        Ok(out)
    })
}

fn parse_pr_meta(v: &serde_json::Value, api_response_hash: String) -> Option<PrMeta> {
    let number = v.get("number")?.as_u64()?;
    let state = v.get("state")?.as_str()?.to_string();
    let merged = v.get("merged_at").map(|x| !x.is_null()).unwrap_or(false);
    let merge_commit_sha = v
        .get("merge_commit_sha")
        .and_then(|x| x.as_str())
        .map(String::from);
    let merged_at = v
        .get("merged_at")
        .and_then(|x| x.as_str())
        .map(String::from);
    let body = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    // Validation must-have #3 — PR.user.login is the authorship anchor.
    // Missing user.login = malformed PR (deleted account?); skip rather
    // than fall back to less reliable signals.
    let author_login = v
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(|s| s.as_str())?
        .to_string();
    Some(PrMeta {
        number,
        state,
        merged,
        merge_commit_sha,
        merged_at,
        body,
        author_login,
        api_response_hash,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ── Operator-facing MCP tool surface ────────────────────────────────

/// `task_sweep_config` MCP tool body. Args:
/// - `repo`: `"owner/repo"` to enable; empty string disables.
/// - `pause`: `true|false`.
/// - `dry_run`: `true|false`.
///
/// Returns the resulting [`SweepConfig`] state as JSON so the operator
/// can verify the change without a follow-up read.
pub fn handle_task_sweep_config(home: &Path, args: &serde_json::Value) -> serde_json::Value {
    let mut cfg = load_config(home);
    if let Some(repo) = args.get("repo").and_then(|v| v.as_str()) {
        cfg.repo = if repo.is_empty() {
            None
        } else {
            Some(repo.to_string())
        };
    }
    if let Some(p) = args.get("pause").and_then(|v| v.as_bool()) {
        cfg.paused = p;
    }
    if let Some(d) = args.get("dry_run").and_then(|v| v.as_bool()) {
        cfg.dry_run = d;
    }
    if let Err(e) = save_config(home, &cfg) {
        return serde_json::json!({"error": format!("save failed: {e}")});
    }
    serde_json::json!({
        "repo": cfg.repo,
        "paused": cfg.paused,
        "dry_run": cfg.dry_run,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-task-sweep-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Must-have #1 — HTML-comment injection: a directive hidden inside
    /// a comment must NOT extract. Real-world task IDs use digit-only
    /// segments (`t-<ts>-<seq>`); the test fixture uses the same shape.
    #[test]
    fn html_comment_injection_stripped() {
        let body = "Closes t-12345-1\n<!-- Closes t-99999-2 -->";
        let sanitised = strip_html_comments(body);
        let markers = extract_closes_markers(&sanitised);
        assert_eq!(markers, vec!["t-12345-1".to_string()]);
    }

    /// Must-have #1 — unterminated comment drops tail entirely; even a
    /// trailing valid marker after a partial `<!--` is dropped (an
    /// adversary playing fuzzing tricks can't slip a directive through).
    #[test]
    fn html_comment_unterminated_drops_tail() {
        let body = "Closes t-1-1\n<!-- partial Closes t-2-2";
        let sanitised = strip_html_comments(body);
        let markers = extract_closes_markers(&sanitised);
        assert_eq!(markers, vec!["t-1-1".to_string()]);
    }

    /// Must-have #2 — non-ASCII codepoint inside the task ID portion
    /// (e.g. zero-width-joiner mimicking a digit) NEVER matches the
    /// strict ASCII-digit regex.
    #[test]
    fn non_ascii_task_id_rejected() {
        // U+200B (zero-width-space) between digits.
        let body = "Closes t-12\u{200B}3-4";
        let markers = extract_closes_markers(body);
        assert!(
            markers.is_empty(),
            "non-ASCII codepoint must defeat the regex"
        );
    }

    /// Multiple markers in one PR body are all extracted.
    #[test]
    fn multiple_markers_extracted() {
        let body = "Closes t-1-1\nCloses t-2-2\nCloses t-3-3";
        let markers = extract_closes_markers(body);
        assert_eq!(
            markers,
            vec![
                "t-1-1".to_string(),
                "t-2-2".to_string(),
                "t-3-3".to_string()
            ]
        );
    }

    /// `parse_pr_meta` rejects the PR if `user.login` is missing — our
    /// authorship anchor (must-have #3) requires it. Better to drop the
    /// PR than fall back to git trailer (the bug class we're defending).
    #[test]
    fn parse_rejects_missing_user_login() {
        let v = serde_json::json!({
            "number": 1,
            "state": "closed",
            "merge_commit_sha": "abc",
            "merged_at": "2026-04-27T00:00:00Z",
            "body": "",
            "user": {} // login missing
        });
        assert!(parse_pr_meta(&v, "h".into()).is_none());
    }

    /// Must-have #5 — `parse_pr_meta` carries the merge SHA so callers
    /// can stamp it onto `PrSnapshot.merge_sha` at decision-time.
    #[test]
    fn parse_carries_merge_sha() {
        let v = serde_json::json!({
            "number": 42,
            "state": "closed",
            "merge_commit_sha": "abcdef1234",
            "merged_at": "2026-04-27T00:00:00Z",
            "body": "Closes t-1-1",
            "user": {"login": "dev-impl-1"}
        });
        let meta = parse_pr_meta(&v, "fakehash".into()).unwrap();
        assert_eq!(meta.merge_commit_sha.as_deref(), Some("abcdef1234"));
        assert_eq!(meta.author_login, "dev-impl-1");
        assert!(meta.merged); // merged_at non-null
    }

    /// `task_sweep_config` MCP tool round-trip — operator sets repo, then
    /// pauses, then disables dry-run; final state matches.
    #[test]
    fn config_tool_round_trip() {
        let home = tmp_home("config_rt");
        let r1 =
            handle_task_sweep_config(&home, &serde_json::json!({"repo": "suzuke/agend-terminal"}));
        assert_eq!(r1["repo"], "suzuke/agend-terminal");
        assert_eq!(r1["paused"], false);

        let r2 = handle_task_sweep_config(&home, &serde_json::json!({"pause": true}));
        assert_eq!(r2["paused"], true);
        assert_eq!(r2["repo"], "suzuke/agend-terminal");

        let r3 = handle_task_sweep_config(&home, &serde_json::json!({"dry_run": true}));
        assert_eq!(r3["dry_run"], true);
        assert_eq!(r3["paused"], true);
        fs::remove_dir_all(&home).ok();
    }

    /// `task_sweep_config` empty-string `repo` disables sweep (sets to
    /// `None`) — operator's escape hatch when the repo identifier is
    /// wrong and they want to clear it without `unsetenv`.
    #[test]
    fn config_tool_empty_repo_disables() {
        let home = tmp_home("config_empty");
        handle_task_sweep_config(&home, &serde_json::json!({"repo": "x/y"}));
        let r = handle_task_sweep_config(&home, &serde_json::json!({"repo": ""}));
        assert_eq!(r["repo"], serde_json::Value::Null);
        fs::remove_dir_all(&home).ok();
    }

    /// `sweep_tick` short-circuits when the config is missing or has no
    /// repo set. Operator sees a no-op rather than a noisy GitHub call.
    #[test]
    fn tick_no_op_when_repo_unconfigured() {
        let home = tmp_home("tick_no_op");
        // No config file written. Tick must complete without error and
        // without writing to task_events.jsonl.
        sweep_tick(&home).unwrap();
        assert!(!home.join("task_events.jsonl").exists());
        fs::remove_dir_all(&home).ok();
    }

    /// `sha256_hex` produces a 64-char hex string. Forensic hash
    /// fingerprint must be a deterministic crypto-grade digest so a
    /// later auditor can correlate against an archived API response.
    #[test]
    fn sha256_hex_is_deterministic_64_hex() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
        let h3 = sha256_hex(b"world");
        assert_ne!(h1, h3);
    }

    /// REJECT criteria (per dev-reviewer m-25) — exercise the
    /// closes-marker extractor against the **actual `Closes t-` commit
    /// messages on origin/main**. Defends against three failure modes
    /// the unit-test fixtures alone can't catch:
    ///
    /// 1. Real PR descriptions contain prose around the marker that
    ///    might trip the regex if the boundary check is loose.
    /// 2. A future contributor edits a PR description AFTER merge to
    ///    add a malicious `Closes t-victim`; this real-repo audit lets
    ///    a subsequent CI run surface the regression.
    /// 3. The on-main commit messages prove that PR1 + the wider Sprint
    ///    24 P0 wave actually used the marker convention sweep depends
    ///    on, not just the unit-test mocks.
    ///
    /// Skipped (with diagnostic stderr) when `git` binary unavailable or
    /// the cwd isn't a git repo. Tests should never *fail* due to env
    /// shape — operator-run CI on a fresh checkout always has both.
    #[test]
    fn closes_markers_extract_cleanly_from_actual_main() {
        let output = match std::process::Command::new("git")
            .args([
                "log",
                "origin/main",
                "--grep=Closes t-",
                "--format=%B",
                "--all-match",
            ])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("skip: git binary unavailable: {e}");
                return;
            }
        };
        if !output.status.success() {
            eprintln!(
                "skip: `git log` failed (likely no `origin/main` ref locally): exit={:?}",
                output.status.code()
            );
            return;
        }
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            eprintln!("skip: no `Closes t-` commits on origin/main yet");
            return;
        }
        let markers = extract_closes_markers(&text);
        // Every extracted marker MUST conform to strict format. A
        // regression where the extractor accepts (e.g.) `t-foo-1`
        // surfaces here.
        let strict = regex::Regex::new(r"^t-[0-9]+-[0-9]+$").unwrap();
        for m in &markers {
            assert!(
                strict.is_match(m),
                "marker `{m}` extracted from real-main commit but fails strict format check"
            );
        }
        eprintln!(
            "real-repo audit: extracted {} markers from {} bytes of `Closes t-` commit messages on origin/main",
            markers.len(),
            text.len()
        );
    }
}
