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
//! Storage is an append-only JSONL file at `<home>/mcp-usage-stats.jsonl` — its
//! own file, NOT the rotating daemon log, so it survives restarts and is never
//! rotated away. One compact line per call:
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
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
