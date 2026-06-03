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
//! Phase 2 adds Codex (`~/.codex/sessions/.../rollout-*.jsonl`,
//! `payload.info.total_token_usage` — session-cumulative, so the MAX per file
//! is taken, never summed), merged into the same per-instance aggregation.
//! OpenCode is deferred (its SQLite store needs a new `rusqlite`/`sqlx`
//! dependency — pending operator sign-off). Kiro has no usable token
//! surface and is reported as unsupported (never fabricated).

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

// #1077 Phase 2 (Codex). ⚠️ CALIBRATION-PENDING — representative OpenAI gpt-5
// family list prices (USD/million), captured 2026-05-29; operator must confirm
// per exact model id (gpt-5-codex / gpt-5.x-codex / gpt-5.x). OpenAI bills no
// cache *creation* charge (auto-cached prompt prefixes are simply discounted on
// read), so both cache-write rates are 0. `reasoning_output_tokens` is a subset
// of `output_tokens` and is therefore already priced at the output rate.
const CODEX_GPT5: ModelPricing = ModelPricing {
    input: 1.25,
    output: 10.0,
    cache_read: 0.125,
    cache_write_5m: 0.0,
    cache_write_1h: 0.0,
};

/// Resolve a `message.model` string to its pricing. Returns `(pricing,
/// estimated)` where `estimated == true` means the model was unrecognised and
/// the Sonnet table was used as a best-effort fallback.
fn pricing_for(model: &str) -> (ModelPricing, bool) {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        (OPUS, false)
    } else if m.contains("sonnet") {
        (SONNET, false)
    } else if m.contains("haiku") {
        (HAIKU, false)
    } else if m.contains("codex") || m.contains("gpt") {
        // #1077 Phase 2: OpenAI Codex / gpt-5 family. One rate row for the
        // family (calibration-pending); not `estimated` since the model IS
        // recognised — the global calibration caveat covers the dollar values.
        (CODEX_GPT5, false)
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

    /// All token classes summed — the denominator for the #1077
    /// unattributed-ratio reconciliation check.
    fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
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
    /// #1077 slice-1: transcript/event wall-clock (epoch ms) for the per-task
    /// time-join. `0` = the line had no parseable timestamp → it falls into the
    /// `(no active task)` bucket (fail-open, never dropped).
    ts_ms: i64,
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
    // #1077: capture the per-message ts (epoch ms) for the task time-join.
    // `0` when absent/unparseable — those lines are kept (best-effort) and land
    // in the `(no active task)` bucket rather than being dropped.
    let ts_ms = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);
    // Freshness gate (best-effort: lines without a parseable ts are kept).
    if let Some(cutoff) = since_cutoff_ms {
        if ts_ms != 0 && ts_ms < cutoff {
            return None;
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
            ts_ms,
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
/// Test-only per-instance Claude aggregate. Production folds the merged
/// claude+codex rows once in `handle_tokens`; this wrapper preserves the
/// original `collect` shape so the pre-#1077 dedup/attribution tests still
/// assert against the per-instance map directly.
#[cfg(test)]
fn collect(
    projects_dir: &Path,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> HashMap<String, HashMap<String, Agg>> {
    fold_by_instance(&collect_rows(projects_dir, roots, since_cutoff_ms))
}

/// #1077: fold per-message rows into the per-instance/per-model aggregate (the
/// pre-existing `tokens` view). The per-task fold ([`fold_by_task`]) sums to the
/// same totals, so the two views are additive / reconcile.
fn fold_by_instance(rows: &[Row]) -> HashMap<String, HashMap<String, Agg>> {
    let mut by_instance: HashMap<String, HashMap<String, Agg>> = HashMap::new();
    for row in rows {
        by_instance
            .entry(row.instance.clone())
            .or_default()
            .entry(row.model.clone())
            .or_default()
            .add(&row.usage);
    }
    by_instance
}

/// #1077: scan + dedup the Claude transcripts into the deduped row set (one row
/// per unique `message.id`, each carrying `ts_ms`). Both the per-instance and
/// the per-task folds consume this, so a single scan serves both views.
fn collect_rows(
    projects_dir: &Path,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> Vec<Row> {
    // message_id → row — global dedup; an id is unique to one turn, so global
    // == per-instance dedup but cheaper.
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

    deduped.into_values().collect()
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

// ── #1077 Phase 2: Codex collector ─────────────────────────────────────────
//
// Source: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. Each session file
// carries a `session_meta` line with `payload.cwd` (attribution), `turn_context`
// lines with `payload.model`, and `event_msg`/`token_count` lines with
// `payload.info.total_token_usage` — which is SESSION-CUMULATIVE, so we take
// the MAX (final total) per file, never a sum. Verified against real files
// 2026-05-29: `total_tokens == input_tokens + output_tokens`,
// `cached_input_tokens ⊆ input_tokens`, `reasoning_output_tokens ⊆ output_tokens`.

/// `~/.codex/sessions` — the Codex rollout root.
fn codex_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex").join("sessions"))
}

/// Recursively collect `rollout-*.jsonl` files under the Y/M/D-nested root.
fn codex_session_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-"))
            {
                out.push(p);
            }
        }
    }
    out
}

/// Parse one Codex session into per-`token_count`-line rows (delta usage + ts).
/// PURE (drives tests off a fixture string). Returns one [`Row`] per
/// `token_count` event using `payload.info.last_token_usage` — the per-turn
/// DELTA, whose sum reconciles to the final `total_token_usage` (verified;
/// asserted by `codex_delta_sum_equals_cumulative`) — and the line's top-level
/// `timestamp`. The session model is the latest `turn_context` (stamped on
/// every row); the instance is resolved from the session `cwd`. Mapping to the
/// shared `Agg`: uncached input = `input_tokens − cached_input_tokens`, cached
/// → `cache_read`, `output_tokens` → output (reasoning already included), no
/// cache-write (OpenAI bills no cache-creation charge). Empty when the session
/// has no attributable `cwd`, no model, or no `token_count` line.
fn parse_codex_rows(
    content: &str,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> Vec<Row> {
    // Pass 1: resolve cwd→instance + the session model (latest turn_context).
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = v.get("payload");
        if cwd.is_none() {
            if let Some(c) = payload.and_then(|p| p.get("cwd")).and_then(Value::as_str) {
                cwd = Some(c.to_string());
            }
        }
        // Latest turn_context wins — the model in effect when the session ended.
        if v.get("type").and_then(Value::as_str) == Some("turn_context") {
            if let Some(m) = payload.and_then(|p| p.get("model")).and_then(Value::as_str) {
                model = Some(m.to_string());
            }
        }
    }
    let (Some(cwd), Some(model)) = (cwd, model) else {
        return Vec::new();
    };
    let Some(instance) = attribute(Path::new(&cwd), roots) else {
        return Vec::new();
    };

    // Pass 2: one row per token_count line — delta usage + per-line ts.
    let mut rows = Vec::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = v.get("payload");
        if payload.and_then(|p| p.get("type")).and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        // Per-row freshness (replaces the old file-mtime gate): each token_count
        // line carries its own ts, so we filter per row. `0` (unparseable) kept.
        let ts_ms = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0);
        if let Some(cutoff) = since_cutoff_ms {
            if ts_ms != 0 && ts_ms < cutoff {
                continue;
            }
        }
        let Some(last) = payload
            .and_then(|p| p.get("info"))
            .and_then(|i| i.get("last_token_usage"))
        else {
            continue;
        };
        let g = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
        let cached = g("cached_input_tokens");
        rows.push(Row {
            instance: instance.clone(),
            model: model.clone(),
            usage: Agg {
                input: g("input_tokens").saturating_sub(cached),
                output: g("output_tokens"),
                cache_read: cached,
                cache_write_5m: 0,
                cache_write_1h: 0,
            },
            ts_ms,
        });
    }
    rows
}

/// Scan Codex rollout files → deduped delta-row set (one row per `token_count`
/// line). Single scan; both folds consume it.
fn collect_codex_rows(
    sessions_dir: &Path,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for fp in codex_session_files(sessions_dir) {
        let Ok(content) = std::fs::read_to_string(&fp) else {
            continue;
        };
        rows.extend(parse_codex_rows(&content, roots, since_cutoff_ms));
    }
    rows
}

/// Per-instance/per-model Codex aggregate. Σ of the delta rows == the session
/// cumulative total (see `codex_delta_sum_equals_cumulative`). Test-only
/// convenience: production folds the merged claude+codex rows once in
/// `handle_tokens`; this wrapper lets a codex-only fixture assert the
/// per-instance reconciliation in isolation.
#[cfg(test)]
fn collect_codex(
    sessions_dir: &Path,
    roots: &[(String, Vec<PathBuf>)],
    since_cutoff_ms: Option<i64>,
) -> HashMap<String, HashMap<String, Agg>> {
    fold_by_instance(&collect_codex_rows(sessions_dir, roots, since_cutoff_ms))
}

/// Fold `src` per-instance/per-model aggregates into `dst` (additive). Test-only
/// since #1077 — production now folds claude+codex rows in one pass via
/// `fold_by_instance`, but the cross-backend merge invariant is still asserted.
#[cfg(test)]
fn merge_into(
    dst: &mut HashMap<String, HashMap<String, Agg>>,
    src: HashMap<String, HashMap<String, Agg>>,
) {
    for (inst, models) in src {
        let d = dst.entry(inst).or_default();
        for (model, agg) in models {
            d.entry(model).or_default().add(&agg);
        }
    }
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

// ── #1077 slice-1: instance→task time-join ──────────────────────────────────

/// A half-open `[start_ms, end_ms)` interval during which one instance was
/// actively on one task. `end_ms == None` = still open (extends to +∞ until the
/// instance's next claim). One instance's windows are disjoint and sorted by
/// `start_ms` — each new claim truncates the prior ("most-recent-active wins").
#[derive(Clone, Debug, PartialEq)]
struct TaskWindow {
    task_id: String,
    start_ms: i64,
    end_ms: Option<i64>,
}

/// Close `inst`'s currently-open window at `end_ms`, recording it. Shared by
/// every close path (truncate / Done / Cancelled / Released) so the owner-aware
/// bookkeeping (`holder`) stays consistent.
fn close_window(
    result: &mut HashMap<String, Vec<TaskWindow>>,
    open: &mut HashMap<String, (String, i64)>,
    holder: &mut HashMap<String, String>,
    inst: &str,
    end_ms: i64,
) {
    if let Some((tid, start)) = open.remove(inst) {
        result
            .entry(inst.to_string())
            .or_default()
            .push(TaskWindow {
                task_id: tid.clone(),
                start_ms: start,
                end_ms: Some(end_ms),
            });
        if holder.get(&tid).is_some_and(|h| h == inst) {
            holder.remove(&tid);
        }
    }
}

/// Build per-instance `[start, end)` task windows from the sorted task-event
/// stream. Open on `Claimed`/`InProgress{by}`; close on `Done`/`Cancelled{by}`
/// of the active task, on `Released` of the active task (**no `by` field** —
/// close whichever instance currently holds it, the owner-aware rule), or by
/// the instance's next claim (truncation). A never-closed window stays open to
/// +∞.
fn build_task_windows(
    envelopes: &[crate::task_events::TaskEventEnvelope],
) -> HashMap<String, Vec<TaskWindow>> {
    use crate::task_events::TaskEvent;
    let mut result: HashMap<String, Vec<TaskWindow>> = HashMap::new();
    // instance → (task_id, start_ms) currently open (≤1 open window per instance).
    let mut open: HashMap<String, (String, i64)> = HashMap::new();
    // task_id → instance currently holding it open (owner-aware Released close).
    let mut holder: HashMap<String, String> = HashMap::new();

    for env in envelopes {
        let ts = chrono::DateTime::parse_from_rfc3339(&env.timestamp)
            .map(|d| d.timestamp_millis())
            .unwrap_or(0);
        match &env.event {
            TaskEvent::Claimed { task_id, by } | TaskEvent::InProgress { task_id, by } => {
                let (tid, inst) = (task_id.0.clone(), by.0.clone());
                // Already actively on this exact task → keep the open window
                // (don't reset its start).
                if open.get(&inst).is_some_and(|(t, _)| *t == tid) {
                    continue;
                }
                // Truncate this instance's other open window.
                close_window(&mut result, &mut open, &mut holder, &inst, ts);
                // Re-claim by a DIFFERENT instance → close the prior holder too.
                if let Some(prev) = holder.get(&tid).cloned() {
                    if prev != inst {
                        close_window(&mut result, &mut open, &mut holder, &prev, ts);
                    }
                }
                open.insert(inst.clone(), (tid.clone(), ts));
                holder.insert(tid, inst);
            }
            TaskEvent::Done { task_id, by, .. } | TaskEvent::Cancelled { task_id, by, .. } => {
                let (tid, inst) = (task_id.0.clone(), by.0.clone());
                if open.get(&inst).is_some_and(|(t, _)| *t == tid) {
                    close_window(&mut result, &mut open, &mut holder, &inst, ts);
                }
            }
            TaskEvent::Released { task_id, .. } => {
                // No `by` — close whichever instance currently holds it open.
                if let Some(inst) = holder.get(&task_id.0).cloned() {
                    close_window(&mut result, &mut open, &mut holder, &inst, ts);
                }
            }
            _ => {}
        }
    }

    // Remaining open windows extend to +∞.
    for (inst, (tid, start)) in open {
        result.entry(inst).or_default().push(TaskWindow {
            task_id: tid,
            start_ms: start,
            end_ms: None,
        });
    }
    for windows in result.values_mut() {
        windows.sort_by_key(|w| w.start_ms);
    }
    result
}

/// Task active at `ts_ms` for one instance's (disjoint, sorted) windows, or
/// `None` (→ the `(no active task)` bucket).
fn attribute_to_task(windows: &[TaskWindow], ts_ms: i64) -> Option<&str> {
    windows
        .iter()
        .find(|w| ts_ms >= w.start_ms && w.end_ms.is_none_or(|e| ts_ms < e))
        .map(|w| w.task_id.as_str())
}

/// Bucket name for rows whose ts falls in no task window.
const NO_ACTIVE_TASK: &str = "(no active task)";

/// Time-join `rows` into per-instance → per-task → per-model aggregates. Σ over
/// tasks (including `(no active task)`) == [`fold_by_instance`], so the per-task
/// and per-instance views reconcile.
fn fold_by_task(
    rows: &[Row],
    windows: &HashMap<String, Vec<TaskWindow>>,
) -> HashMap<String, HashMap<String, HashMap<String, Agg>>> {
    let mut out: HashMap<String, HashMap<String, HashMap<String, Agg>>> = HashMap::new();
    for row in rows {
        let task = windows
            .get(&row.instance)
            .and_then(|w| attribute_to_task(w, row.ts_ms))
            .unwrap_or(NO_ACTIVE_TASK)
            .to_string();
        out.entry(row.instance.clone())
            .or_default()
            .entry(task)
            .or_default()
            .entry(row.model.clone())
            .or_default()
            .add(&row.usage);
    }
    out
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
        "Token usage ({since_label}) — Claude Code + Codex. Excludes Claude >200k long-context \
         surcharge. Kiro unsupported (no token telemetry source). Pricing is an estimate \
         pending operator calibration.\n"
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
        "backends": ["claude", "codex"],
        "unsupported_backends": ["kiro-cli"],
        "note": "Claude Code + Codex; Kiro unsupported (no token telemetry source, not fabricated); excludes Claude >200k long-context surcharge; pricing pending operator calibration",
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

/// Sum one task's per-model aggregates → `(total, usd, by_model_json)`.
fn task_totals(models: &HashMap<String, Agg>) -> (Agg, f64, Vec<Value>) {
    let mut total = Agg::default();
    let mut usd = 0.0;
    let mut by_model: Vec<(String, Agg, f64, bool)> = models
        .iter()
        .map(|(model, agg)| {
            let (p, est) = pricing_for(model);
            let c = agg.cost(&p);
            total.add(agg);
            usd += c;
            (model.clone(), *agg, c, est)
        })
        .collect();
    by_model.sort_by(|a, b| b.2.total_cmp(&a.2));
    let json = by_model
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
    (total, usd, json)
}

/// #1077: render the per-instance → per-task view. Includes the
/// `(no active task)` bucket so per-task totals reconcile to the per-instance
/// view, flags any instance whose unattributed share exceeds 25%, and carries
/// the time-window-attribution caveat (this is NOT per-task billing).
fn render_by_task(
    by_task: HashMap<String, HashMap<String, HashMap<String, Agg>>>,
    since_label: &str,
    instance_filter: Option<&str>,
) -> Value {
    const CAVEAT: &str = "time-window attribution, NOT per-task billing — each \
        message is attributed to whichever task the instance had active at the \
        message's timestamp; off-task work within a window is mis-attributed.";
    const UNATTRIBUTED_FLAG_PCT: f64 = 25.0;

    // Assemble per instance: tasks sorted by cost desc, instance totals,
    // unattributed share + flag.
    let mut instances: Vec<(String, Agg, f64, f64, bool, Vec<Value>)> = by_task
        .into_iter()
        .filter(|(name, _)| instance_filter.is_none_or(|f| f == name))
        .map(|(name, tasks)| {
            let mut inst_total = Agg::default();
            let mut inst_usd = 0.0;
            let mut no_task_tokens = 0u64;
            let mut task_rows: Vec<(String, Agg, f64, Vec<Value>)> = tasks
                .into_iter()
                .map(|(task_id, models)| {
                    let (total, usd, by_model) = task_totals(&models);
                    inst_total.add(&total);
                    inst_usd += usd;
                    if task_id == NO_ACTIVE_TASK {
                        no_task_tokens = total.total_tokens();
                    }
                    (task_id, total, usd, by_model)
                })
                .collect();
            task_rows.sort_by(|a, b| b.2.total_cmp(&a.2));
            let inst_tokens = inst_total.total_tokens();
            let unattributed_pct = if inst_tokens > 0 {
                (no_task_tokens as f64) / (inst_tokens as f64) * 100.0
            } else {
                0.0
            };
            let flag = unattributed_pct > UNATTRIBUTED_FLAG_PCT;
            let task_json: Vec<Value> = task_rows
                .iter()
                .map(|(task_id, total, usd, by_model)| {
                    json!({
                        "task_id": task_id,
                        "input": total.input,
                        "output": total.output,
                        "cache_read": total.cache_read,
                        "cache_creation": total.cache_creation(),
                        "usd": round2(*usd),
                        "by_model": by_model,
                    })
                })
                .collect();
            (
                name,
                inst_total,
                inst_usd,
                unattributed_pct,
                flag,
                task_json,
            )
        })
        .collect();
    instances.sort_by(|a, b| b.2.total_cmp(&a.2));

    // Text table.
    let mut table = String::new();
    table.push_str(&format!(
        "Token usage by task ({since_label}) — Claude Code + Codex. \
         {CAVEAT}\n"
    ));
    for (name, inst_total, inst_usd, pct, flag, task_json) in &instances {
        table.push_str(&format!(
            "\n{name}  (total {} in / {} out / ${:.2}; unattributed {:.0}%{})\n",
            fmt_tokens(inst_total.input),
            fmt_tokens(inst_total.output),
            inst_usd,
            pct,
            if *flag { " ⚠ >25%" } else { "" },
        ));
        table.push_str(&format!(
            "  {:<34} {:>9} {:>9} {:>10}\n",
            "task", "Input", "Output", "USD"
        ));
        for t in task_json {
            let usd = t.get("usd").and_then(Value::as_f64).unwrap_or(0.0);
            table.push_str(&format!(
                "  {:<34} {:>9} {:>9} {:>10}\n",
                t.get("task_id").and_then(Value::as_str).unwrap_or("?"),
                fmt_tokens(t.get("input").and_then(Value::as_u64).unwrap_or(0)),
                fmt_tokens(t.get("output").and_then(Value::as_u64).unwrap_or(0)),
                format!("${:.2}", usd),
            ));
        }
    }

    let per_instance: Vec<Value> = instances
        .iter()
        .map(|(name, total, usd, pct, flag, tasks)| {
            json!({
                "instance": name,
                "input": total.input,
                "output": total.output,
                "cache_read": total.cache_read,
                "cache_creation": total.cache_creation(),
                "usd": round2(*usd),
                "unattributed_pct": round2(*pct),
                "unattributed_flag": flag,
                "by_task": tasks,
            })
        })
        .collect();

    json!({
        "ok": true,
        "since": since_label,
        "group_by": "task",
        "backends": ["claude", "codex"],
        "caveat": CAVEAT,
        "note": "per-task totals (incl. (no active task)) reconcile to the per-instance view; unattributed_flag marks >25% unattributed",
        "per_instance": per_instance,
        "table": table,
    })
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
    // #1077 slice-1: optional per-task grouping. Default `instance` =
    // pre-existing behaviour (backward-compatible; no schema break).
    let group_by = args
        .get("group_by")
        .and_then(Value::as_str)
        .unwrap_or("instance");
    if group_by != "instance" && group_by != "task" {
        return json!({"ok": false, "error": format!("unknown group_by: {group_by} (expected instance | task)")});
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
    // Single scan → deduped row set (claude + codex), consumed by whichever fold.
    let mut rows = collect_rows(&projects, &roots, cutoff);
    if let Some(codex) = codex_sessions_dir() {
        rows.extend(collect_codex_rows(&codex, &roots, cutoff));
    }

    if group_by == "task" {
        let windows = crate::task_events::stream_envelopes(home)
            .map(|envs| build_task_windows(&envs))
            .unwrap_or_default();
        render_by_task(fold_by_task(&rows, &windows), since_label, instance_filter)
    } else {
        render(fold_by_instance(&rows), since_label, instance_filter)
    }
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

        // A model outside the claude/gpt/codex families is unknown → estimated.
        // (gpt-* is now recognised as the Codex family — see Phase 2.)
        let (_, est_unknown) = pricing_for("mistral-large-2");
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

    // ── #1077 Phase 2: Codex collector ──

    /// Build a Codex rollout fixture in the REAL format: every line has a
    /// top-level `timestamp`; `token_count` lines carry both `total_token_usage`
    /// (session-cumulative) and `last_token_usage` (the per-turn delta). Two
    /// turns, each delta = (1000 in / 400 cached / 100 out); cumulative ends at
    /// 2200 — so Σdelta reconciles to the cumulative total.
    fn codex_session(cwd: &str, model: &str) -> String {
        [
            format!(r#"{{"timestamp":"2026-04-25T00:10:18.000Z","type":"session_meta","payload":{{"cwd":"{cwd}"}}}}"#),
            format!(r#"{{"timestamp":"2026-04-25T00:10:19.000Z","type":"turn_context","payload":{{"model":"{model}"}}}}"#),
            // turn 1 — cumulative 1100, delta == cumulative (first turn)
            r#"{"timestamp":"2026-04-25T00:10:22.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"reasoning_output_tokens":30,"total_tokens":1100},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"reasoning_output_tokens":30,"total_tokens":1100}}}}"#.to_string(),
            // turn 2 — cumulative 2200, delta 1100
            r#"{"timestamp":"2026-04-25T00:10:29.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":800,"output_tokens":200,"reasoning_output_tokens":60,"total_tokens":2200},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"reasoning_output_tokens":30,"total_tokens":1100}}}}"#.to_string(),
        ]
        .join("\n")
    }

    /// CORRECTION #2 (load-bearing): per-line `last_token_usage` deltas + the
    /// line's top-level ts. The Σ of the delta rows MUST equal what the old
    /// cumulative-MAX implementation reported (= the final `total_token_usage`),
    /// otherwise the default per-instance view regresses. Reasoning tokens are
    /// already inside `output_tokens` (not double-counted).
    #[test]
    fn codex_delta_sum_equals_cumulative() {
        let cwd = "/h/workspace/dev-a";
        let roots = vec![("dev-a".to_string(), vec![PathBuf::from(cwd)])];
        let content = codex_session(cwd, "gpt-5-codex");
        let rows = parse_codex_rows(&content, &roots, None);
        assert_eq!(
            rows.len(),
            2,
            "one row per token_count line (delta, not MAX)"
        );
        for r in &rows {
            assert_eq!(r.instance, "dev-a");
            assert_eq!(r.model, "gpt-5-codex");
            assert!(r.ts_ms > 0, "each delta row carries its per-message ts");
        }
        assert!(
            rows[0].ts_ms < rows[1].ts_ms,
            "rows preserve chronological order"
        );
        // Σ of the per-turn deltas.
        let mut sum = Agg::default();
        for r in &rows {
            sum.add(&r.usage);
        }
        // Final cumulative `total_token_usage`: input 2000, cached 800, out 200.
        // Mapped: uncached input 1200, cache_read 800, output 200, no cache-write.
        assert_eq!(sum.input, 1200, "Σdelta uncached input == cumulative");
        assert_eq!(sum.cache_read, 800, "Σdelta cached == cumulative cached");
        assert_eq!(
            sum.output, 200,
            "Σdelta output == cumulative output (reasoning not doubled)"
        );
        assert_eq!(
            sum.cache_creation(),
            0,
            "OpenAI has no cache-creation charge"
        );
    }

    /// A session whose cwd is not under any fleet root yields no rows.
    #[test]
    fn codex_foreign_cwd_skipped() {
        let roots = vec![(
            "dev-a".to_string(),
            vec![PathBuf::from("/h/workspace/dev-a")],
        )];
        let content = codex_session("/some/other/project", "gpt-5-codex");
        assert!(parse_codex_rows(&content, &roots, None).is_empty());
    }

    /// Codex/gpt models resolve to the Codex pricing row (recognised, not the
    /// estimated Claude fallback).
    #[test]
    fn codex_models_priced_as_codex_not_claude_fallback() {
        for m in ["gpt-5-codex", "gpt-5.3-codex", "gpt-5.5", "GPT-5"] {
            let (p, est) = pricing_for(m);
            assert!(!est, "{m} must be a recognised model");
            assert_eq!(p.output, CODEX_GPT5.output, "{m} must use Codex pricing");
            assert_eq!(p.cache_write_5m, 0.0, "{m} has no cache-creation rate");
        }
    }

    /// Cross-backend merge: same instance, Claude + Codex models, sums per model.
    #[test]
    fn merge_combines_claude_and_codex_per_instance() {
        let mut claude: HashMap<String, HashMap<String, Agg>> = HashMap::new();
        claude.entry("dev-a".into()).or_default().insert(
            "claude-opus-4-8".into(),
            Agg {
                input: 10,
                output: 20,
                ..Agg::default()
            },
        );
        let mut codex: HashMap<String, HashMap<String, Agg>> = HashMap::new();
        codex.entry("dev-a".into()).or_default().insert(
            "gpt-5-codex".into(),
            Agg {
                input: 5,
                output: 7,
                ..Agg::default()
            },
        );
        merge_into(&mut claude, codex);
        let dev_a = claude.get("dev-a").expect("dev-a present");
        assert_eq!(
            dev_a.len(),
            2,
            "both backend models coexist under one instance"
        );
        assert_eq!(dev_a.get("gpt-5-codex").unwrap().output, 7);
        assert_eq!(dev_a.get("claude-opus-4-8").unwrap().input, 10);
    }

    /// The default per-instance codex view must be UNCHANGED by the delta
    /// refactor: `collect_codex` (now Σdelta) reports the cumulative total.
    #[test]
    fn collect_codex_per_instance_unchanged_after_delta_refactor() {
        let sessions = tmp("codex-sessions");
        let day = sessions.join("2026").join("04").join("25");
        std::fs::create_dir_all(&day).unwrap();
        // Virtual forward-slash cwd so the `codex_session` fixture (built with
        // `format!`, which does NOT escape backslashes) is valid JSON on Windows
        // too — a real tmp path embeds `\U…` and breaks `serde_json::from_str`.
        let cwd = "/virtual/workspace/dev-a";
        std::fs::write(
            day.join("rollout-x.jsonl"),
            codex_session(cwd, "gpt-5-codex"),
        )
        .unwrap();
        let roots = vec![("dev-a".to_string(), vec![PathBuf::from(cwd)])];
        let by = collect_codex(&sessions, &roots, None);
        let agg = &by["dev-a"]["gpt-5-codex"];
        assert_eq!(agg.input, 1200, "Σdelta per-instance == cumulative");
        assert_eq!(agg.cache_read, 800);
        assert_eq!(agg.output, 200);
    }

    // ── #1077 slice-1: instance→task time-join ──

    use crate::task_events::{DoneSource, InstanceName, TaskEvent, TaskEventEnvelope, TaskId};

    /// `ts_secs` → an RFC3339 envelope; `build_task_windows` parses it back to
    /// `ts_secs * 1000` ms.
    fn env(ts_secs: i64, inst: &str, event: TaskEvent) -> TaskEventEnvelope {
        TaskEventEnvelope {
            schema_version: 1,
            seq: 0,
            timestamp: chrono::DateTime::from_timestamp(ts_secs, 0)
                .unwrap()
                .to_rfc3339(),
            instance: InstanceName(inst.to_string()),
            emitter_id: None,
            event,
        }
    }
    fn claimed(task: &str, by: &str) -> TaskEvent {
        TaskEvent::Claimed {
            task_id: TaskId(task.into()),
            by: InstanceName(by.into()),
        }
    }
    fn done(task: &str, by: &str) -> TaskEvent {
        TaskEvent::Done {
            task_id: TaskId(task.into()),
            by: InstanceName(by.into()),
            source: DoneSource::OperatorManual {
                authored_at: "2026-01-01T00:00:00Z".into(),
                result: None,
            },
        }
    }
    fn released(task: &str) -> TaskEvent {
        TaskEvent::Released {
            task_id: TaskId(task.into()),
            reason: "swept".into(),
        }
    }

    #[test]
    fn attribute_to_task_window_boundaries_and_gaps() {
        let w = vec![
            TaskWindow {
                task_id: "A".into(),
                start_ms: 100,
                end_ms: Some(200),
            },
            TaskWindow {
                task_id: "B".into(),
                start_ms: 300,
                end_ms: None,
            },
        ];
        assert_eq!(attribute_to_task(&w, 150), Some("A"), "inside window");
        assert_eq!(attribute_to_task(&w, 100), Some("A"), "start inclusive");
        assert_eq!(attribute_to_task(&w, 200), None, "end exclusive");
        assert_eq!(attribute_to_task(&w, 50), None, "before first");
        assert_eq!(attribute_to_task(&w, 250), None, "in the gap");
        assert_eq!(attribute_to_task(&w, 9_999), Some("B"), "open window → +∞");
    }

    #[test]
    fn windows_next_claim_truncates_overlap() {
        // dev-a claims A then B without closing A → A truncated at B's claim.
        let envs = vec![
            env(100, "dev-a", claimed("A", "dev-a")),
            env(200, "dev-a", claimed("B", "dev-a")),
        ];
        let w = build_task_windows(&envs);
        let da = &w["dev-a"];
        assert_eq!(da.len(), 2);
        assert_eq!(
            da[0],
            TaskWindow {
                task_id: "A".into(),
                start_ms: 100_000,
                end_ms: Some(200_000)
            }
        );
        assert_eq!(da[1].end_ms, None, "B stays open to +∞");
    }

    #[test]
    fn windows_never_done_stays_open_and_done_closes() {
        let open = build_task_windows(&[env(100, "dev-a", claimed("A", "dev-a"))]);
        assert_eq!(open["dev-a"][0].end_ms, None, "never-done → open");
        let closed = build_task_windows(&[
            env(100, "dev-a", claimed("A", "dev-a")),
            env(150, "dev-a", done("A", "dev-a")),
        ]);
        assert_eq!(closed["dev-a"][0].end_ms, Some(150_000), "Done closes");
    }

    #[test]
    fn windows_owner_aware_released_closes_holder() {
        // CORRECTION #1: `Released` carries NO `by` — it must close whichever
        // instance currently HOLDS the task open, not be ignored for lack of by.
        let w = build_task_windows(&[
            env(100, "dev-a", claimed("A", "dev-a")),
            env(150, "dev-a", released("A")),
        ]);
        assert_eq!(
            w["dev-a"][0].end_ms,
            Some(150_000),
            "Released closes the holder's open window"
        );
    }

    #[test]
    fn windows_reclaim_yields_disjoint_windows() {
        // A → B → A on one instance: two disjoint A windows.
        let w = build_task_windows(&[
            env(100, "dev-a", claimed("A", "dev-a")),
            env(200, "dev-a", claimed("B", "dev-a")),
            env(300, "dev-a", claimed("A", "dev-a")),
        ]);
        let a: Vec<_> = w["dev-a"].iter().filter(|x| x.task_id == "A").collect();
        assert_eq!(a.len(), 2, "re-claim → two disjoint A windows");
        assert_eq!(a[0].end_ms, Some(200_000));
    }

    #[test]
    fn windows_multi_instance_isolation() {
        let w = build_task_windows(&[
            env(100, "dev-a", claimed("A", "dev-a")),
            env(120, "dev-b", claimed("B", "dev-b")),
        ]);
        assert_eq!(w["dev-a"][0].task_id, "A");
        assert_eq!(w["dev-b"][0].task_id, "B");
        assert_eq!(w["dev-a"].len(), 1);
        assert_eq!(w["dev-b"].len(), 1);
    }

    fn row(inst: &str, input: u64, ts_ms: i64) -> Row {
        Row {
            instance: inst.into(),
            model: "claude-opus-4-8".into(),
            usage: Agg {
                input,
                ..Agg::default()
            },
            ts_ms,
        }
    }

    #[test]
    fn fold_by_task_buckets_no_task_and_reconciles_to_per_instance() {
        let rows = vec![
            row("dev-a", 100, 150), // in window A
            row("dev-a", 50, 180),  // in window A
            row("dev-a", 30, 250),  // gap → (no active task)
        ];
        let mut windows = HashMap::new();
        windows.insert(
            "dev-a".to_string(),
            vec![TaskWindow {
                task_id: "A".into(),
                start_ms: 100,
                end_ms: Some(200),
            }],
        );
        let by_task = fold_by_task(&rows, &windows);
        let da = &by_task["dev-a"];
        assert_eq!(da["A"]["claude-opus-4-8"].input, 150, "in-window rows → A");
        assert_eq!(
            da[NO_ACTIVE_TASK]["claude-opus-4-8"].input, 30,
            "gap row → (no active task)"
        );
        // Reconciliation: Σ over tasks == per-instance fold.
        let per_inst_total: u64 = fold_by_instance(&rows)["dev-a"]
            .values()
            .map(|a| a.input)
            .sum();
        let per_task_total: u64 = da.values().flat_map(|m| m.values()).map(|a| a.input).sum();
        assert_eq!(per_task_total, per_inst_total, "per-task Σ == per-instance");
        assert_eq!(per_task_total, 180);
    }

    #[test]
    fn render_by_task_flags_unattributed_over_25pct_and_carries_caveat() {
        let mut by_task: HashMap<String, HashMap<String, HashMap<String, Agg>>> = HashMap::new();
        let inst = by_task.entry("dev-a".into()).or_default();
        inst.entry("A".into()).or_default().insert(
            "claude-opus-4-8".into(),
            Agg {
                input: 70,
                ..Agg::default()
            },
        );
        inst.entry(NO_ACTIVE_TASK.into()).or_default().insert(
            "claude-opus-4-8".into(),
            Agg {
                input: 30,
                ..Agg::default()
            },
        );
        let v = render_by_task(by_task, "all", None);
        assert_eq!(v["group_by"], json!("task"));
        assert!(v["caveat"]
            .as_str()
            .unwrap()
            .contains("NOT per-task billing"));
        let pi = &v["per_instance"][0];
        assert_eq!(pi["unattributed_flag"], json!(true), "30% > 25% → flagged");
        assert!(pi["unattributed_pct"].as_f64().unwrap() >= 29.0);
    }

    #[test]
    fn default_group_by_instance_output_shape_unchanged() {
        // Regression: the no-group_by (default) render keeps the pre-#1077
        // shape — per_instance[].by_model, and no group_by/by_task/caveat keys.
        let rows = vec![Row {
            instance: "dev-a".into(),
            model: "claude-opus-4-8".into(),
            usage: Agg {
                input: 10,
                output: 20,
                ..Agg::default()
            },
            ts_ms: 0,
        }];
        let v = render(fold_by_instance(&rows), "all", None);
        assert_eq!(
            v["group_by"],
            json!(null),
            "no group_by key in default view"
        );
        assert!(
            v.get("caveat").is_none(),
            "no per-task caveat in default view"
        );
        let pi = &v["per_instance"][0];
        assert!(pi.get("by_model").is_some(), "per-model breakdown retained");
        assert!(
            pi.get("by_task").is_none(),
            "no per-task key in default view"
        );
        assert_eq!(pi["input"], json!(10));
    }
}
