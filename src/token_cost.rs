//! On-demand token / cost observability for Claude Code backends (#1077 Phase 1).
//!
//! No daemon ingester and no intermediate `token_events.jsonl`: the Claude Code
//! session transcript (`~/.claude/projects/<sanitised-cwd>/<session>.jsonl`) IS
//! the persistent source, so the `tokens` MCP tool scans it on demand at query
//! time. Each assistant line carries a `message.usage` block; this module
//!
//!   1. attributes each line to a fleet instance via its authoritative `cwd`
//!      field (deterministic — no fragile timestamp correlation),
//!   2. dedups streaming-duplicated rows by `message.id` (Claude emits the same
//!      id up to ~6× per turn — empirically 929 rows → 450 unique ids in one
//!      session; not deduping ~2× inflates every total),
//!   3. prices the deduped totals against a hardcoded Claude table (input /
//!      output / cache-read / cache-write split 5m vs 1h).
//!
//! Phase 2 (Codex `~/.codex/sessions`, OpenCode SQLite) + Kiro/Gemini
//! (no usable token surface) are out of scope — see the issue follow-up list.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Per-million-token USD rates for one model family.
///
/// ⚠️ PRICING NEEDS OPERATOR CALIBRATION. Values below are the published
/// Anthropic Claude-4 list prices as understood on 2026-05-29 (USD per
/// million tokens). They drive cost *estimates* only — verify against the
/// current Anthropic pricing page before trusting the dollar figures. Cache
/// write is split: 5-minute ephemeral = 1.25× input, 1-hour ephemeral = 2×
/// input. The >200k-token long-context surcharge tier is NOT modelled (Phase 1
/// scope) — outputs flag this.
#[derive(Clone, Copy)]
struct ModelPricing {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
}

// Source: Anthropic pricing page, captured 2026-05-29. Operator must confirm.
const OPUS: ModelPricing = ModelPricing {
    input: 15.0,
    output: 75.0,
    cache_read: 1.5,
    cache_write_5m: 18.75,
    cache_write_1h: 30.0,
};
const SONNET: ModelPricing = ModelPricing {
    input: 3.0,
    output: 15.0,
    cache_read: 0.3,
    cache_write_5m: 3.75,
    cache_write_1h: 6.0,
};
const HAIKU: ModelPricing = ModelPricing {
    input: 1.0,
    output: 5.0,
    cache_read: 0.1,
    cache_write_5m: 1.25,
    cache_write_1h: 2.0,
};

/// Resolve a `message.model` string to its pricing. Returns `(pricing,
/// estimated)` where `estimated == true` means the model was unrecognised and
/// the Sonnet table was used as a best-effort fallback.
fn pricing_for(model: &str) -> (ModelPricing, bool) {
    if model.contains("opus") {
        (OPUS, false)
    } else if model.contains("sonnet") {
        (SONNET, false)
    } else if model.contains("haiku") {
        (HAIKU, false)
    } else {
        (SONNET, true)
    }
}

/// Deduped token counts (one model family within one instance).
#[derive(Clone, Copy, Default)]
struct Agg {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
}

impl Agg {
    fn add(&mut self, o: &Agg) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write_5m += o.cache_write_5m;
        self.cache_write_1h += o.cache_write_1h;
    }

    fn cache_creation(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    fn cost(&self, p: &ModelPricing) -> f64 {
        let per_m = |tokens: u64, rate: f64| (tokens as f64) * rate / 1_000_000.0;
        per_m(self.input, p.input)
            + per_m(self.output, p.output)
            + per_m(self.cache_read, p.cache_read)
            + per_m(self.cache_write_5m, p.cache_write_5m)
            + per_m(self.cache_write_1h, p.cache_write_1h)
    }
}

/// One assistant-message usage row after dedup, tagged with attribution.
struct Row {
    instance: String,
    model: String,
    usage: Agg,
}

/// Parse one transcript line into a `(message_id, Row)` if it is an assistant
/// message carrying usage that (a) attributes to a known instance and (b)
/// falls within the freshness cutoff. Returns `None` otherwise.
fn parse_line(
    line: &str,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> Option<(String, Row)> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    // Freshness gate (best-effort: lines without a parseable ts are kept).
    if let Some(cutoff) = since_cutoff_ms {
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                if dt.timestamp_millis() < cutoff {
                    return None;
                }
            }
        }
    }
    let cwd = v.get("cwd").and_then(Value::as_str)?;
    let instance = attribute(Path::new(cwd), roots)?;
    let msg = v.get("message")?;
    let model = msg.get("model").and_then(Value::as_str)?.to_string();
    let u = msg.get("usage")?;
    let cc = u.get("cache_creation");
    let usage = Agg {
        input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_read: u
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_5m: cc
            .and_then(|c| c.get("ephemeral_5m_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_1h: cc
            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
            .and_then(Value::as_u64)
            // Fallback: older lines lack the 5m/1h split — treat the whole
            // `cache_creation_input_tokens` as 1h-ephemeral is wrong, so when
            // the split is absent we attribute it to 5m below instead.
            .unwrap_or(0),
    };
    // When the split block is absent, fold the lump-sum creation tokens into
    // the cheaper 5m bucket (conservative) so totals still reconcile.
    let usage = if cc.is_none() {
        Agg {
            cache_write_5m: u
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            ..usage
        }
    } else {
        usage
    };
    let id = msg.get("id").and_then(Value::as_str)?.to_string();
    Some((
        id,
        Row {
            instance,
            model,
            usage,
        },
    ))
}

/// Map a transcript `cwd` to the owning fleet instance. A cwd belongs to an
/// instance when it equals, or is nested under, one of that instance's roots
/// (its workspace dir and any worktree dir). `None` = foreign cwd (e.g. a
/// non-fleet local session) — skipped.
fn attribute(cwd: &Path, roots: &[(String, Vec<PathBuf>)]) -> Option<String> {
    for (instance, paths) in roots {
        for root in paths {
            if cwd == root || cwd.starts_with(root) {
                return Some(instance.clone());
            }
        }
    }
    None
}

/// Core scan: walk every `*.jsonl` under `projects_dir`, dedup by `message.id`
/// (keep the max-output occurrence so a truncated streaming row never wins),
/// and fold into per-instance / per-model aggregates. Parameterised on
/// `projects_dir` + `roots` + `cutoff` so tests drive it with a recorded
/// fixture instead of `$HOME`.
fn collect(
    projects_dir: &Path,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> HashMap<String, HashMap<String, Agg>> {
    // message_id → (instance, model, usage) — global dedup; an id is unique to
    // one turn, so global == per-instance dedup but cheaper.
    let mut deduped: HashMap<String, Row> = HashMap::new();

    let session_dirs = std::fs::read_dir(projects_dir)
        .into_iter()
        .flatten()
        .flatten();
    for dir in session_dirs {
        let p = dir.path();
        if !p.is_dir() {
            continue;
        }
        let files = std::fs::read_dir(&p).into_iter().flatten().flatten();
        for f in files {
            let fp = f.path();
            if fp.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // mtime pre-filter: a file untouched since the cutoff can't hold a
            // fresh row. Saves reading stale transcripts on a bounded `since`.
            if let Some(cutoff) = since_cutoff_ms {
                if let Ok(meta) = std::fs::metadata(&fp) {
                    if let Ok(modified) = meta.modified() {
                        if let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) {
                            if (age.as_millis() as i64) < cutoff {
                                continue;
                            }
                        }
                    }
                }
            }
            let Ok(content) = std::fs::read_to_string(&fp) else {
                continue;
            };
            for line in content.lines() {
                if let Some((id, row)) = parse_line(line, roots, since_cutoff_ms) {
                    match deduped.get(&id) {
                        Some(existing) if existing.usage.output >= row.usage.output => {}
                        _ => {
                            deduped.insert(id, row);
                        }
                    }
                }
            }
        }
    }

    let mut by_instance: HashMap<String, HashMap<String, Agg>> = HashMap::new();
    for row in deduped.into_values() {
        by_instance
            .entry(row.instance)
            .or_default()
            .entry(row.model)
            .or_default()
            .add(&row.usage);
    }
    by_instance
}

/// Build the instance → roots map from fleet.yaml + the daemon's on-disk
/// layout: each instance owns its workspace dir (`<home>/workspace/<name>` or
/// its fleet.yaml `working_directory`) plus every worktree dir
/// (`<home>/worktrees/<name>/…`). Worktree subdirs are enumerated live; merged
/// sessions whose worktree was already pruned still attribute via the
/// `<home>/worktrees/<name>` prefix match in `attribute`.
fn instance_roots(home: &Path) -> Vec<(String, Vec<PathBuf>)> {
    let fleet =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap_or_default();
    let workspace = crate::paths::workspace_dir(home);
    let worktrees = home.join("worktrees");
    fleet
        .instances
        .iter()
        .map(|(name, cfg)| {
            let mut roots = vec![cfg
                .working_directory
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| workspace.join(name))];
            // All worktree dirs live under <home>/worktrees/<name>/. A prefix
            // root captures every branch worktree (live or pruned-from-disk).
            roots.push(worktrees.join(name));
            (name.clone(), roots)
        })
        .collect()
}

/// `~/.claude/projects` — the Claude Code transcript root.
fn claude_projects_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("projects"))
}

/// Parse a `since` argument (`"24h"` / `"7d"` / `"90m"` / `"all"`) into a
/// cutoff epoch-ms. `None` (or `"all"`) = no cutoff.
fn parse_since(since: Option<&str>, now_ms: i64) -> Option<i64> {
    let s = since?;
    if s == "all" {
        return None;
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().ok()?;
    let ms = match unit {
        "h" => n * 3_600_000,
        "d" => n * 86_400_000,
        "m" => n * 60_000,
        _ => return None,
    };
    Some(now_ms - ms)
}

// ── human-readable formatting ────────────────────────────────────────────

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Render the per-instance aggregates into `(json, text_table)`.
fn render(
    by_instance: HashMap<String, HashMap<String, Agg>>,
    since_label: &str,
    by_instance_filter: Option<&str>,
) -> Value {
    // Sort instances by descending cost for a stable, hotspot-first view.
    let mut instances: Vec<(String, Agg, f64, Vec<Value>)> = by_instance
        .into_iter()
        .filter(|(name, _)| by_instance_filter.is_none_or(|f| f == name))
        .map(|(name, models)| {
            let mut total = Agg::default();
            let mut usd = 0.0;
            let mut by_model: Vec<(String, Agg, f64, bool)> = models
                .into_iter()
                .map(|(model, agg)| {
                    let (p, est) = pricing_for(&model);
                    let c = agg.cost(&p);
                    total.add(&agg);
                    usd += c;
                    (model, agg, c, est)
                })
                .collect();
            by_model.sort_by(|a, b| b.2.total_cmp(&a.2));
            let model_json: Vec<Value> = by_model
                .iter()
                .map(|(model, agg, c, est)| {
                    json!({
                        "model": model,
                        "input": agg.input,
                        "output": agg.output,
                        "cache_read": agg.cache_read,
                        "cache_creation": agg.cache_creation(),
                        "usd": round2(*c),
                        "pricing_estimated": est,
                    })
                })
                .collect();
            (name, total, usd, model_json)
        })
        .collect();
    instances.sort_by(|a, b| b.2.total_cmp(&a.2));

    let grand_usd: f64 = instances.iter().map(|(_, _, u, _)| u).sum();
    let grand: Agg = instances
        .iter()
        .fold(Agg::default(), |mut acc, (_, t, _, _)| {
            acc.add(t);
            acc
        });

    // Text table.
    let mut table = String::new();
    table.push_str(&format!(
        "Token usage ({since_label}) — Claude Code only. Excludes >200k long-context surcharge. \
         Pricing is an estimate pending operator calibration.\n"
    ));
    table.push_str(&format!(
        "{:<18} {:>9} {:>9} {:>9} {:>9} {:>10}\n",
        "Instance", "Input", "Output", "CacheRd", "CacheWr", "USD"
    ));
    for (name, total, usd, _) in &instances {
        table.push_str(&format!(
            "{:<18} {:>9} {:>9} {:>9} {:>9} {:>10}\n",
            name,
            fmt_tokens(total.input),
            fmt_tokens(total.output),
            fmt_tokens(total.cache_read),
            fmt_tokens(total.cache_creation()),
            format!("${:.2}", usd),
        ));
    }
    table.push_str(&format!(
        "{:<18} {:>9} {:>9} {:>9} {:>9} {:>10}\n",
        "TOTAL",
        fmt_tokens(grand.input),
        fmt_tokens(grand.output),
        fmt_tokens(grand.cache_read),
        fmt_tokens(grand.cache_creation()),
        format!("${:.2}", grand_usd),
    ));

    let per_instance: Vec<Value> = instances
        .iter()
        .map(|(name, total, usd, models)| {
            json!({
                "instance": name,
                "input": total.input,
                "output": total.output,
                "cache_read": total.cache_read,
                "cache_creation": total.cache_creation(),
                "usd": round2(*usd),
                "by_model": models,
            })
        })
        .collect();

    json!({
        "ok": true,
        "since": since_label,
        "backend": "claude",
        "note": "Claude Code only; excludes >200k long-context surcharge; pricing pending operator calibration",
        "totals": {
            "input": grand.input,
            "output": grand.output,
            "cache_read": grand.cache_read,
            "cache_creation": grand.cache_creation(),
            "usd": round2(grand_usd),
        },
        "per_instance": per_instance,
        "table": table,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// MCP `tokens` handler. Shape `ha` — `(home, args)`.
pub(crate) fn handle_tokens(home: &Path, args: &Value) -> Value {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    if action != "summary" && action != "by_instance" {
        return json!({"ok": false, "error": format!("unknown tokens action: {action} (expected summary | by_instance)")});
    }
    let since = args.get("since").and_then(Value::as_str);
    let since_label = since.unwrap_or("24h");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff = parse_since(Some(since_label), now_ms);

    let Some(projects) = claude_projects_dir() else {
        return json!({"ok": false, "error": "cannot resolve $HOME/.claude/projects"});
    };

    let instance_filter = if action == "by_instance" {
        match args.get("instance").and_then(Value::as_str) {
            Some(i) => Some(i),
            None => return json!({"ok": false, "error": "action=by_instance requires `instance`"}),
        }
    } else {
        args.get("instance").and_then(Value::as_str)
    };

    let roots = instance_roots(home);
    let by_instance = collect(&projects, &roots, cutoff);
    render(by_instance, since_label, instance_filter)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agend-1077-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Recorded-fixture line mirroring the real Claude transcript shape
    /// (verified against `~/.claude/projects/.../*.jsonl`).
    #[allow(clippy::too_many_arguments)]
    fn usage_line(
        cwd: &str,
        id: &str,
        model: &str,
        inp: u64,
        out: u64,
        cr: u64,
        cw5: u64,
        cw1: u64,
    ) -> String {
        json!({
            "type": "assistant",
            "cwd": cwd,
            "timestamp": "2026-05-29T00:00:00.000Z",
            "message": {
                "id": id,
                "model": model,
                "usage": {
                    "input_tokens": inp,
                    "output_tokens": out,
                    "cache_read_input_tokens": cr,
                    "cache_creation_input_tokens": cw5 + cw1,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": cw5,
                        "ephemeral_1h_input_tokens": cw1,
                    }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn dedup_by_message_id_no_double_count() {
        let home = tmp("dedup-home");
        let projects = tmp("dedup-proj");
        let cwd = home.join("workspace").join("dev-a");
        let session = projects.join("session-a");
        let line = usage_line(
            cwd.to_str().unwrap(),
            "msg_dup",
            "claude-opus-4-8",
            100,
            50,
            200,
            30,
            10,
        );
        // Same message.id emitted 3× (streaming duplicate) — must count once.
        write(&session, "s.jsonl", &[&line, &line, &line]);

        let roots = vec![("dev-a".to_string(), vec![cwd.clone()])];
        let by = collect(&projects, &roots, None);
        let agg = &by["dev-a"]["claude-opus-4-8"];
        assert_eq!(
            agg.input, 100,
            "input must not be tripled by streaming dupes"
        );
        assert_eq!(agg.output, 50);
        assert_eq!(agg.cache_read, 200);
        assert_eq!(agg.cache_write_5m, 30);
        assert_eq!(agg.cache_write_1h, 10);
    }

    #[test]
    fn attributes_workspace_and_worktree_paths_to_same_instance() {
        let home = tmp("dual-home");
        let projects = tmp("dual-proj");
        let ws = home.join("workspace").join("dev-a");
        let wt = home.join("worktrees").join("dev-a").join("feat").join("x");
        write(
            &projects.join("ws"),
            "s.jsonl",
            &[&usage_line(
                ws.to_str().unwrap(),
                "msg_ws",
                "claude-sonnet-4-6",
                10,
                5,
                0,
                0,
                0,
            )],
        );
        write(
            &projects.join("wt"),
            "s.jsonl",
            &[&usage_line(
                wt.to_str().unwrap(),
                "msg_wt",
                "claude-sonnet-4-6",
                7,
                3,
                0,
                0,
                0,
            )],
        );
        // Roots mirror instance_roots(): workspace dir + worktrees/<name> prefix.
        let roots = vec![(
            "dev-a".to_string(),
            vec![ws.clone(), home.join("worktrees").join("dev-a")],
        )];
        let by = collect(&projects, &roots, None);
        let agg = &by["dev-a"]["claude-sonnet-4-6"];
        assert_eq!(
            agg.input, 17,
            "workspace + worktree sessions fold into one instance"
        );
        assert_eq!(agg.output, 8);
    }

    #[test]
    fn pricing_splits_cache_5m_vs_1h_and_flags_unknown_model() {
        // 1M input @15 + 1M output @75 + 1M cache_read @1.5
        //   + 1M cw5m @18.75 + 1M cw1h @30  = 140.25 for opus.
        let a = Agg {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
        };
        let (p, est) = pricing_for("claude-opus-4-8");
        assert!(!est);
        assert!(
            (a.cost(&p) - 140.25).abs() < 1e-9,
            "opus cost = {}",
            a.cost(&p)
        );

        let (_, est_unknown) = pricing_for("gpt-5-mini");
        assert!(est_unknown, "unknown model must flag pricing_estimated");
    }

    #[test]
    fn foreign_cwd_is_skipped() {
        let home = tmp("foreign-home");
        let projects = tmp("foreign-proj");
        write(
            &projects.join("foreign"),
            "s.jsonl",
            &[&usage_line(
                "/some/other/repo",
                "msg_x",
                "claude-opus-4-8",
                999,
                999,
                0,
                0,
                0,
            )],
        );
        let roots = vec![(
            "dev-a".to_string(),
            vec![home.join("workspace").join("dev-a")],
        )];
        let by = collect(&projects, &roots, None);
        assert!(by.is_empty(), "non-fleet cwd must not be attributed");
    }

    #[test]
    fn since_cutoff_filters_old_rows() {
        let home = tmp("since-home");
        let projects = tmp("since-proj");
        let cwd = home.join("workspace").join("dev-a");
        // ts is 2026-05-29T00:00:00Z; cutoff one ms later drops it.
        let line = usage_line(
            cwd.to_str().unwrap(),
            "msg_old",
            "claude-haiku-4-5",
            10,
            10,
            0,
            0,
            0,
        );
        write(&projects.join("s"), "s.jsonl", &[&line]);
        let roots = vec![("dev-a".to_string(), vec![cwd])];
        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-05-29T00:00:00.001Z")
            .unwrap()
            .timestamp_millis();
        let by = collect(&projects, &roots, Some(cutoff));
        assert!(by.is_empty(), "row older than cutoff must be excluded");
    }

    #[test]
    fn parse_since_units() {
        assert_eq!(parse_since(Some("all"), 1000), None);
        assert_eq!(
            parse_since(Some("24h"), 100_000_000),
            Some(100_000_000 - 86_400_000)
        );
        assert_eq!(
            parse_since(Some("30m"), 100_000_000),
            Some(100_000_000 - 1_800_000)
        );
        assert_eq!(
            parse_since(Some("7d"), 1_000_000_000),
            Some(1_000_000_000 - 604_800_000)
        );
        assert_eq!(parse_since(None, 1000), None);
    }
}
