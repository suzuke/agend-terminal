//! #2055 step 1 — MCP tool/param usage instrumentation (instrument-only).
//!
//! Pure observation, ZERO behaviour change: this records *that* a tool was
//! called and *which optional params* it carried, so a week of real usage can
//! drive the #2055 tool/param trimming (steps 3/5) with data instead of
//! intuition. It never reads or mutates the tool's result, never gates
//! anything, and is best-effort by construction — every error is swallowed, so
//! a failure here can never affect a tool call (same discipline as the #1808
//! instrument-only shadow probes).
//!
//! Storage is a bounded JSONL file at `<home>/mcp-usage-stats.jsonl` — its own
//! file, NOT the rotating daemon log. It rotates locally with a small retention
//! window, so usage stats survive restarts without unbounded disk growth. One
//! compact line per call:
//!
//! ```json
//! {"ts":"2026-06-12T05:00:00Z","tool":"task","action":"create","opt_params":["branch","priority"]}
//! ```
//!
//! `opt_params` lists only the schema-declared OPTIONAL params present in the
//! call (required params are excluded — they appear on every call and carry no
//! trim signal). `action` records `args.action` for action-based tools (`task`,
//! `ci`, `decision`, …) so step 5's per-action trimming has counts; it is
//! `null` for non-action tools. Aggregate with the jq one-liners in the PR body.
//!
//! Always on (no flag): single-user daemon, the event volume is tiny.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const MAX_STATS_BYTES: u64 = 1024 * 1024;
const MAX_ROTATED_FILES: usize = 5;
const MAX_ROTATED_AGE: Duration = Duration::from_secs(30 * 86400);

#[derive(Clone, Copy)]
struct RetentionPolicy {
    max_live_bytes: u64,
    max_rotated_files: usize,
    max_rotated_age: Duration,
}

const DEFAULT_RETENTION: RetentionPolicy = RetentionPolicy {
    max_live_bytes: MAX_STATS_BYTES,
    max_rotated_files: MAX_ROTATED_FILES,
    max_rotated_age: MAX_ROTATED_AGE,
};

/// The usage log path — a dedicated file under `<home>`, deliberately separate
/// from `daemon.log*` so log rotation never discards it.
fn stats_path(home: &Path) -> PathBuf {
    home.join("mcp-usage-stats.jsonl")
}

/// Per-tool set of schema-declared OPTIONAL param names (`properties − required`),
/// built once from the tool registry. A param present in a call is recorded only
/// if it is in this set, which excludes both required params (every-call noise)
/// and any non-schema key (e.g. an envelope field) — exactly the optional-usage
/// signal #2055 wants.
fn optional_params() -> &'static HashMap<String, HashSet<String>> {
    static MAP: OnceLock<HashMap<String, HashSet<String>>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::new();
        for entry in crate::mcp::registry::all() {
            let schema = (entry.definition)();
            let input = &schema["inputSchema"];
            let props: HashSet<String> = input["properties"]
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            let required: HashSet<String> = input["required"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let optional: HashSet<String> = props.difference(&required).cloned().collect();
            map.insert(entry.name.to_string(), optional);
        }
        map
    })
}

/// Record one MCP tool call. Best-effort — all errors are swallowed so this can
/// never affect the caller. Called from the single dispatch chokepoint
/// (`handlers::handle_tool`), so it observes every tool invocation exactly once.
pub fn record(home: &Path, tool: &str, args: &Value) {
    let line = build_line(tool, args, chrono::Utc::now().to_rfc3339());
    let _ = append_line(&stats_path(home), &line);
}

/// Pure event builder (unit-tested): the JSONL object for one call. `opt_params`
/// is the intersection of the call's top-level arg keys with the tool's optional
/// param set (sorted for stable output); `action` mirrors `args.action`.
fn build_line(tool: &str, args: &Value, ts: String) -> Value {
    let optional = optional_params();
    let allowed = optional.get(tool);
    let mut opt_params: Vec<String> = args
        .as_object()
        .map(|obj| {
            obj.keys()
                .filter(|k| allowed.map(|s| s.contains(k.as_str())).unwrap_or(false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    opt_params.sort();
    serde_json::json!({
        "ts": ts,
        "tool": tool,
        "action": args.get("action").and_then(|v| v.as_str()),
        "opt_params": opt_params,
    })
}

/// Append one compact JSONL line. Create-if-missing + append, so concurrent
/// daemon threads each get an atomic-enough single `writeln!`; a torn line is
/// tolerable (the analyst's jq skips a malformed line), and a failed write is
/// silently dropped — usage stats must never be load-bearing.
fn append_line(path: &Path, line: &Value) -> std::io::Result<()> {
    append_line_with_policy(path, line, DEFAULT_RETENTION, SystemTime::now())
}

fn append_line_with_policy(
    path: &Path,
    line: &Value,
    policy: RetentionPolicy,
    now: SystemTime,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path).map_err(std::io::Error::other)?;

    rotate_if_needed(path, policy);
    prune_rotated_stats(path, policy, now);

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    drop(f);

    rotate_if_needed(path, policy);
    prune_rotated_stats(path, policy, now);
    Ok(())
}

fn rotated_stats_path(base: &Path, gen: usize) -> PathBuf {
    let mut name = base.file_name().map(|s| s.to_owned()).unwrap_or_default();
    name.push(format!(".{gen}"));
    base.with_file_name(name)
}

fn rotate_if_needed(path: &Path, policy: RetentionPolicy) {
    if policy.max_rotated_files == 0 {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= policy.max_live_bytes {
        return;
    }

    let _ = std::fs::remove_file(rotated_stats_path(path, policy.max_rotated_files));
    for gen in (1..policy.max_rotated_files).rev() {
        let src = rotated_stats_path(path, gen);
        let dst = rotated_stats_path(path, gen + 1);
        if src.exists() {
            let _ = std::fs::rename(src, dst);
        }
    }
    if std::fs::rename(path, rotated_stats_path(path, 1)).is_ok() {
        let _ = std::fs::File::create(path);
    }
}

fn prune_rotated_stats(path: &Path, policy: RetentionPolicy, now: SystemTime) {
    let Some(dir) = path.parent() else {
        return;
    };
    let Some(base_name) = path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let prefix = format!("{base_name}.");

    let mut rotated: Vec<(PathBuf, usize, SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let gen = name.strip_prefix(&prefix)?.parse::<usize>().ok()?;
            let mtime = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, gen, mtime))
        })
        .collect();

    for (path, gen, mtime) in &rotated {
        let too_old = now
            .duration_since(*mtime)
            .map(|age| age > policy.max_rotated_age)
            .unwrap_or(false);
        if *gen > policy.max_rotated_files || too_old {
            let _ = std::fs::remove_file(path);
        }
    }

    rotated.retain(|(path, gen, mtime)| {
        path.exists()
            && *gen <= policy.max_rotated_files
            && now
                .duration_since(*mtime)
                .map(|age| age <= policy.max_rotated_age)
                .unwrap_or(true)
    });
    rotated.sort_by_key(|(_, gen, _)| *gen);
    for (idx, (path, _, _)) in rotated.iter().enumerate() {
        if idx >= policy.max_rotated_files {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    /// `optional_params` derives `properties − required` from the live registry:
    /// `task` (required `action`) keeps `branch`/`priority` as optional and drops
    /// `action`; `send` (required `message`) drops `message`.
    #[test]
    fn optional_set_excludes_required_2055() {
        let opt = optional_params();
        let task = opt.get("task").expect("task tool registered");
        assert!(task.contains("branch"), "branch is an optional task param");
        assert!(task.contains("priority"));
        assert!(
            !task.contains("action"),
            "action is required → excluded from the optional set"
        );
        let send = opt.get("send").expect("send tool registered");
        assert!(!send.contains("message"), "message is required → excluded");
        assert!(send.contains("branch"), "branch is an optional send param");
    }

    /// `build_line` records only optional params present in the call, sorted,
    /// excludes required + unknown keys, and lifts `action` into its own field.
    #[test]
    fn build_line_records_optional_params_and_action_2055() {
        let args = json!({
            "action": "create",          // required → not in opt_params, lifted to `action`
            "title": "x",                // task-schema OPTIONAL (only `action` is required) → recorded
            "branch": "feat/y",          // optional → recorded
            "priority": "high",          // optional → recorded
            "not_a_real_param": 1        // not in the schema → excluded
        });
        let line = build_line("task", &args, "2026-06-12T00:00:00Z".into());
        assert_eq!(line["tool"], "task");
        assert_eq!(line["action"], "create");
        let params: Vec<&str> = line["opt_params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            params,
            vec!["branch", "priority", "title"],
            "schema-optional present params, sorted; required (action) + unknown excluded"
        );
    }

    /// A non-action tool records `action: null`.
    #[test]
    fn build_line_non_action_tool_null_action_2055() {
        let args = json!({"instance": "dev-1", "message": "hi", "branch": "feat/z"});
        let line = build_line("send", &args, "2026-06-12T00:00:00Z".into());
        assert!(line["action"].is_null(), "non-action tool → action null");
        let params: Vec<&str> = line["opt_params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(params.contains(&"instance") && params.contains(&"branch"));
        assert!(
            !params.contains(&"message"),
            "message is required → excluded"
        );
    }

    /// `record` appends a JSONL line that survives across calls (the
    /// restart-safety property: each event is durably appended, no in-memory
    /// state to lose). Two calls → two parseable lines.
    #[test]
    fn record_appends_durable_jsonl_lines_2055() {
        let home = std::env::temp_dir().join(format!("agend-usage-2055-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let path = stats_path(&home);
        let _ = std::fs::remove_file(&path);

        record(&home, "task", &json!({"action": "list"}));
        record(
            &home,
            "send",
            &json!({"instance": "x", "message": "m", "branch": "b"}),
        );

        let body = std::fs::read_to_string(&path).expect("stats file written");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one JSONL line per call");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["tool"], "task");
        assert_eq!(first["action"], "list");
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["tool"], "send");
        assert!(second["opt_params"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "branch"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn append_line_rotates_when_live_stats_exceeds_budget_r9() {
        let home = tmp_home("rotate-budget-r9");
        let path = stats_path(&home);
        let policy = RetentionPolicy {
            max_live_bytes: 180,
            max_rotated_files: 2,
            max_rotated_age: Duration::from_secs(30 * 86400),
        };

        for i in 0..40 {
            let line = json!({
                "ts": "2026-06-20T00:00:00Z",
                "tool": "task",
                "action": "create",
                "opt_params": ["branch", "priority", format!("p{i}")],
            });
            append_line_with_policy(&path, &line, policy, SystemTime::now()).unwrap();
        }

        let live_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            live_len <= policy.max_live_bytes,
            "live stats file must stay under the rotation budget; got {live_len}"
        );
        assert!(
            rotated_stats_path(&path, 1).exists(),
            "rotation must keep history"
        );
        assert!(
            !rotated_stats_path(&path, 3).exists(),
            "count retention must prune generations beyond max_rotated_files"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn append_line_prunes_stale_rotated_stats_even_without_size_rotation_r9() {
        let home = tmp_home("retention-age-r9");
        let path = stats_path(&home);
        let old = rotated_stats_path(&path, 1);
        let fresh = rotated_stats_path(&path, 2);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(&old, "old\n").unwrap();
        std::fs::write(&fresh, "fresh\n").unwrap();

        let stale_mtime = SystemTime::now() - Duration::from_secs(10);
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(stale_mtime).unwrap();

        let policy = RetentionPolicy {
            max_live_bytes: 1024 * 1024,
            max_rotated_files: 5,
            max_rotated_age: Duration::from_secs(1),
        };
        append_line_with_policy(
            &path,
            &json!({"ts":"now","tool":"task","action":"list","opt_params":[]}),
            policy,
            SystemTime::now(),
        )
        .unwrap();

        assert!(!old.exists(), "stale rotated stats must be pruned on write");
        assert!(fresh.exists(), "fresh rotated stats must be retained");
        std::fs::remove_dir_all(&home).ok();
    }

    fn tmp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-usage-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
