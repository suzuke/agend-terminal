//! agend-mcp-bridge — near-zero-state stdio↔TCP relay for MCP tool calls.
//!
//! Sprint 25 P0 Option F: this binary is spawned by agent backends as
//! their MCP server. Almost every `tools/call` request is forwarded to
//! the daemon API socket which dispatches via `handle_tool` in daemon
//! process context (where ACTIVE_CHANNEL, heartbeat_pair, etc. are
//! registered).
//!
//! MCP protocol methods (initialize, ping, tools/list, notifications)
//! are handled locally — they don't need daemon state.
//!
//! Bridge-side session state (#1000): a single `RecentCall` tracks the
//! most recent `tools/call` per bridge process. When the LLM hallucinates
//! a duplicate tool call in the same turn (same tool + identical args
//! within 500 ms), the second call is dropped at the bridge with a
//! success-with-`note` response. This is NOT a logical-request dedup
//! (`src/api/request_dedup.rs` handles that via UUIDv4 `request_id`),
//! it's a content-level guard for upstream LLM-side double-fire.
//! Daemon's append-only inbox contract is preserved — the daemon never
//! sees the second call.
//!
//! Protocol:
//! - stdin/stdout: NDJSON JSON-RPC (MCP spec, one JSON object per line)
//! - daemon: NDJSON over TCP loopback with cookie auth

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "../mcp_wire.rs"]
mod mcp_wire;

/// Window for bridge-side `tools/call` content-dedup (#1000). Empirically
/// the LLM double-fire produces calls within ~84 ms of each other; 500 ms
/// is a generous buffer that still excludes any human-paced retry.
const CONTENT_DEDUP_WINDOW: Duration = Duration::from_millis(500);

/// Most-recently-seen `tools/call` in the current bridge session.
/// Used by `is_duplicate_call` to suppress LLM-side double-fire.
#[derive(Debug, Clone)]
struct RecentCall {
    tool: String,
    args: serde_json::Value,
    at: Instant,
}

/// Returns `true` when the incoming `(tool, args, now)` is a duplicate
/// of `last` within `window`. Pure-function design (no `Instant::now()`
/// internally) so tests can inject deterministic timestamps.
fn is_duplicate_call(
    last: Option<&RecentCall>,
    tool: &str,
    args: &serde_json::Value,
    now: Instant,
    window: Duration,
) -> bool {
    last.is_some_and(|l| {
        l.tool == tool
            && &l.args == args
            && now.checked_duration_since(l.at).is_some_and(|d| d < window)
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("agend-mcp-bridge: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let home = home_dir();
    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();

    // Lazy persistent TCP connection to daemon API.
    let mut conn: Option<mcp_wire::Client> = None;

    // #1000: bridge-side content-dedup state. Single most-recent
    // `tools/call` is enough — LLM double-fire shows up as consecutive
    // identical calls within ms, not as a sliding-window pattern.
    let mut last_call: Option<RecentCall> = None;

    loop {
        let body = match read_message(&mut reader)? {
            Some(b) => b,
            None => break,
        };

        let req: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let id = extract_id(&body);
                let resp = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32700,"message":"Parse error: {e}"}}}}"#
                );
                write_message(&mut stdout, &resp)?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    // Slice α build provenance: semver + build metadata
                    // (`<semver>+g<sha12>[.dirty]`, bare semver when unknown).
                    "serverInfo": { "name": "agend-terminal", "version": env!("AGEND_MCP_VERSION") }
                }
            })
            .to_string(),

            "ping" => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string(),

            "notifications/initialized" | "notifications/cancelled" => continue,

            "tools/list" => match proxy_tools_list_with_retry(&home, &instance, &mut conn) {
                Ok(r) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string(),
                Err(e) => {
                    // #879v4 C2 — Bug 2 fix: a `tools/list` failure surfaces
                    // as a visible JSON-RPC error so operators see daemon
                    // unavailability instead of an unexplained empty tool
                    // list (the previous `{tools: []}` silent-degrade was
                    // the same antipattern shape as #881 noop_guard).
                    eprintln!("agend-mcp-bridge: tools/list failed after retry: {e}");
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32603, "message": format!("daemon not ready: {e}")}
                    })
                    .to_string()
                }
            },

            "tools/call" => {
                let tool = req["params"]["name"].as_str().unwrap_or("");
                let args = &req["params"]["arguments"];
                let now = Instant::now();

                // #1000: drop LLM-side double-fire BEFORE proxying to
                // daemon. The dropped call gets a success-with-`note`
                // response so the LLM's tool-call loop completes cleanly;
                // operators see the drop via the eprintln warning.
                if is_duplicate_call(last_call.as_ref(), tool, args, now, CONTENT_DEDUP_WINDOW) {
                    eprintln!(
                        "agend-mcp-bridge: dropped duplicate tool call '{tool}' within {}ms",
                        CONTENT_DEDUP_WINDOW.as_millis()
                    );
                    let dropped = serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": "{\"status\":\"ok\",\"note\":\"duplicate tool call dropped by bridge\"}"
                            }],
                            "isError": false
                        }
                    })
                    .to_string();
                    write_message(&mut stdout, &dropped)?;
                    // Refresh the dedup-window anchor — successive
                    // double-fires within the same turn keep getting
                    // dropped instead of one slipping through after the
                    // first window expires. Safe because the original
                    // record was seeded from a confirmed-forwarded call
                    // (see Ok branch below) — propagation of validity.
                    last_call = Some(RecentCall {
                        tool: tool.to_string(),
                        args: args.clone(),
                        at: now,
                    });
                    continue;
                }

                // RC1 (PR #1008 reviewer): record `last_call` ONLY after
                // a successful forward. Pre-fix recorded unconditionally
                // before `proxy_tool_call`, which meant a first call that
                // FAILED at the daemon would still seed the dedup cache —
                // a retry within 500 ms would then be wrongly dropped with
                // a success-with-note while the daemon never saw it.
                // Outcome contract: dedup only covers requests that
                // actually reached the daemon's logical-execution path.
                match proxy_tool_call(&home, &instance, tool, args, &mut conn) {
                    Ok(result) => {
                        last_call = Some(RecentCall {
                            tool: tool.to_string(),
                            args: args.clone(),
                            at: now,
                        });
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default()}],
                                "isError": result.get("error").is_some()
                            }
                        }).to_string()
                    }
                    Err(e) => {
                        conn = None;
                        // Deliberately NOT updating `last_call` — operator
                        // can retry the identical call within 500 ms and
                        // the second attempt MUST actually forward (the
                        // first attempt never reached daemon execution).
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32603, "message": format!("daemon proxy error: {e}")}
                        })
                        .to_string()
                    }
                }
            }

            m if m.starts_with("notifications/") => continue,

            _ => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")}
            })
            .to_string(),
        };

        write_message(&mut stdout, &response)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon proxy
// ---------------------------------------------------------------------------

fn proxy_tool_call(
    home: &Path,
    instance: &str,
    tool: &str,
    args: &serde_json::Value,
    conn: &mut Option<mcp_wire::Client>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let envelope = mcp_wire::mcp_tool_envelope(instance, tool, args);
    let resp = proxy_request(home, conn, &envelope)?;
    if resp["ok"].as_bool() == Some(true) {
        if mcp_wire::classify_response(&resp) == mcp_wire::ResponseClass::Accepted {
            Ok(resp)
        } else {
            Ok(resp["result"].clone())
        }
    } else {
        let msg = resp["error"].as_str().unwrap_or("daemon error");
        Err(msg.into())
    }
}

/// Poll `mcp_tools_list` at a fixed cadence until it succeeds or the
/// deadline expires. Covers the residual sub-millisecond race between
/// `api::serve` thread start and the TCP `bind` + `api.port` write that
/// remains even after the #879v4 C1 daemon reorder.
///
/// Retry is gated to TRANSPORT failures only (no run dir, connection
/// refused, broken pipe, etc.). Application-level errors — the daemon
/// answered with `ok:false` — propagate immediately so the caller sees
/// the real diagnostic instead of looping on a deterministic failure.
///
/// Production budget is a fixed 30 s — well above any observed startup time on
/// a 9-agent fleet. On exhaustion the last transport error propagates; callers
/// convert to a visible JSON-RPC error, never a silent empty tool list (Bug 2
/// contract).
///
/// `AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS` is a **test-only seam, NOT a production
/// tunable** (#env-cleanup): this proxy runs in the `agend-mcp-bridge` BINARY,
/// so a cross-process integration test that spawns the bridge has env as its
/// only lever to shorten this timeout (e.g. `tests/attached_path_mcp_invariants`
/// sets 300ms so the daemon-unreachable error path returns fast instead of
/// waiting the full 30 s). Operators never set it.
fn proxy_tools_list_with_retry(
    home: &Path,
    instance: &str,
    conn: &mut Option<mcp_wire::Client>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    // test-only seam (see fn doc): prod always falls through to the 30_000 default.
    let timeout_ms: u64 = std::env::var("AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let interval = std::time::Duration::from_millis(100);
    // #2300 P0: pass the caller's instance so the daemon can subset the tool
    // list by role (mirrors the tool-call path). Empty → daemon serves the full
    // surface (default-all-open).
    let envelope = mcp_wire::tools_list_envelope(instance);
    loop {
        match proxy_request(home, conn, &envelope) {
            Ok(resp) => {
                if resp["ok"].as_bool() == Some(true) {
                    return Ok(resp["result"].clone());
                }
                let msg = resp["error"]
                    .as_str()
                    .unwrap_or("daemon error on tools_list");
                return Err(msg.into());
            }
            Err(e) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(e);
                }
                let remaining = deadline - now;
                std::thread::sleep(interval.min(remaining));
            }
        }
    }
}

/// Send one request envelope to the daemon and return the parsed response.
///
/// The persistent connection in `conn` may be silently closed by the daemon
/// after idle (post-auth read timeout) or by intervening network state. The
/// shared wire client drops the connection and retries transport failures
/// exactly once. A second failure propagates so genuine errors are not masked.
///
/// #842: a UUIDv4 `request_id` is injected into the envelope before the
/// first attempt and **reused verbatim** on the retry — the retry IS the
/// same logical request, just re-transported. The daemon dedups on
/// `request_id` so a successful original execution whose response never
/// reached us isn't replayed as a fresh side-effect call.
fn proxy_request(
    home: &Path,
    conn: &mut Option<mcp_wire::Client>,
    envelope: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = conn.get_or_insert_with(mcp_wire::Client::default);
    client
        .request_with_retry(home, envelope)
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
}

/// Clone `envelope` with a freshly-minted `request_id` (UUIDv4) iff the
/// caller didn't already set one. The whole point of the field is for
/// the daemon to recognize the retry — generated once, sent twice on
/// the retry path is exactly the right shape.
#[cfg(test)]
fn envelope_with_request_id(envelope: &serde_json::Value) -> serde_json::Value {
    mcp_wire::with_request_id(envelope)
}

// ---------------------------------------------------------------------------
// MCP framing (NDJSON-only)
// ---------------------------------------------------------------------------

fn read_message(reader: &mut impl BufRead) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // NDJSON-only: all known MCP backends (Claude Code, Kiro CLI, Codex,
        // Gemini, OpenCode) send NDJSON over stdio. Content-Length (LSP-style)
        // fallback removed — it was an attack surface (drip-feed DoS via
        // blocking read_exact, negative Content-Length crash, OOM via large
        // Content-Length). See the framing contract in docs/MCP-TOOLS.md.
        if trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
        // Non-JSON, non-empty line: log and skip (defensive)
        eprintln!("agend-mcp-bridge: ignoring non-JSON input line");
    }
}

fn write_message(stdout: &mut io::Stdout, json: &str) -> io::Result<()> {
    writeln!(stdout, "{json}")?;
    stdout.flush()
}

// ---------------------------------------------------------------------------
// Minimal filesystem helpers (NO crate:: dependencies — zero state)
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
fn find_run_dir(home: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    mcp_wire::find_run_dir(home).map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
}

fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("AGEND_HOME") {
        return PathBuf::from(h);
    }
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let new_path = base.join(".agend");
    let legacy = base.join(".agend-terminal");
    if new_path.exists() || !legacy.exists() {
        new_path
    } else {
        legacy
    }
}

fn extract_id(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #842: `proxy_request` must inject `request_id` exactly once and
    /// reuse it across the (up-to-2) transport attempts. The retry IS
    /// the same logical request, just re-transported — without a stable
    /// id the daemon can't recognize it.
    #[test]
    fn envelope_with_request_id_injects_when_missing() {
        let envelope = serde_json::json!({
            "method": "send",
            "params": {"target": "agent-a", "text": "hello"}
        });
        let with_id = envelope_with_request_id(&envelope);
        let id = with_id
            .get("request_id")
            .and_then(|v| v.as_str())
            .expect("request_id must be injected when missing");

        // UUIDv4 string form is 36 chars with the canonical layout.
        assert_eq!(id.len(), 36, "UUIDv4 string length");
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);

        // Other fields must be preserved verbatim.
        assert_eq!(with_id["method"], "send");
        assert_eq!(with_id["params"]["target"], "agent-a");
    }

    /// Pre-supplied `request_id` must be preserved as-is. Callers that
    /// already manage idempotency keys (e.g. test harnesses) win.
    #[test]
    fn envelope_with_request_id_preserves_caller_value() {
        let envelope = serde_json::json!({
            "method": "send",
            "params": {"target": "agent-b"},
            "request_id": "caller-supplied-deadbeef"
        });
        let with_id = envelope_with_request_id(&envelope);
        assert_eq!(with_id["request_id"], "caller-supplied-deadbeef");
    }

    /// Repeated calls must mint distinct ids — each `proxy_request`
    /// invocation is a separate logical request unless the caller
    /// pinned a value. Guards against accidental id reuse.
    #[test]
    fn envelope_with_request_id_mints_distinct_ids_per_call() {
        let envelope = serde_json::json!({"method": "list", "params": {}});
        let a = envelope_with_request_id(&envelope);
        let b = envelope_with_request_id(&envelope);
        assert_ne!(a["request_id"], b["request_id"]);
    }

    // ── #1000 bridge-side content-dedup ─────────────────────────────────

    fn args_send(target: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"instance": target, "message": text})
    }

    /// LLM double-fire: identical tool + args within the 500 ms window
    /// MUST be flagged as duplicate. This is the load-bearing case for
    /// #1000 — the cheerc snippet's canonical behaviour.
    #[test]
    fn is_duplicate_call_identical_within_window() {
        let t0 = Instant::now();
        let last = RecentCall {
            tool: "send".to_string(),
            args: args_send("lead", "hello"),
            at: t0,
        };
        let now = t0 + Duration::from_millis(84); // matches issue body timing
        assert!(is_duplicate_call(
            Some(&last),
            "send",
            &args_send("lead", "hello"),
            now,
            CONTENT_DEDUP_WINDOW,
        ));
    }

    /// Identical call AFTER the window must NOT be deduped — the LLM
    /// might legitimately re-send the same content as a follow-up turn.
    #[test]
    fn is_duplicate_call_identical_after_window() {
        let t0 = Instant::now();
        let last = RecentCall {
            tool: "send".to_string(),
            args: args_send("lead", "hello"),
            at: t0,
        };
        let now = t0 + Duration::from_millis(501); // just past 500 ms
        assert!(!is_duplicate_call(
            Some(&last),
            "send",
            &args_send("lead", "hello"),
            now,
            CONTENT_DEDUP_WINDOW,
        ));
    }

    /// Same tool, DIFFERENT args within window MUST forward — two
    /// distinct sends are two distinct messages even if they happen
    /// fast. Dedup is content-keyed.
    #[test]
    fn is_duplicate_call_same_tool_different_args_within_window() {
        let t0 = Instant::now();
        let last = RecentCall {
            tool: "send".to_string(),
            args: args_send("lead", "hello"),
            at: t0,
        };
        let now = t0 + Duration::from_millis(50);
        assert!(!is_duplicate_call(
            Some(&last),
            "send",
            &args_send("lead", "world"), // different message
            now,
            CONTENT_DEDUP_WINDOW,
        ));
    }

    /// DIFFERENT tool, same args envelope shape within window MUST
    /// forward — `send` and `reply` may legitimately fire in sequence.
    #[test]
    fn is_duplicate_call_different_tool_within_window() {
        let t0 = Instant::now();
        let last = RecentCall {
            tool: "send".to_string(),
            args: args_send("lead", "hello"),
            at: t0,
        };
        let now = t0 + Duration::from_millis(50);
        assert!(!is_duplicate_call(
            Some(&last),
            "reply", // different tool
            &args_send("lead", "hello"),
            now,
            CONTENT_DEDUP_WINDOW,
        ));
    }

    /// First call ever (no `last_call` recorded) MUST forward — cold
    /// start has nothing to compare against.
    #[test]
    fn is_duplicate_call_no_prior_state() {
        let now = Instant::now();
        assert!(!is_duplicate_call(
            None,
            "send",
            &args_send("lead", "hello"),
            now,
            CONTENT_DEDUP_WINDOW,
        ));
    }

    /// RC1 (PR #1008 reviewer): a first proxy_tool_call ERROR must NOT
    /// seed the dedup cache. Production invariant: `last_call` is only
    /// assigned inside the `Ok(_)` branch of the proxy match. If the
    /// first call fails (daemon unavailable, connection drop, etc.),
    /// `last_call` stays at its pre-call value — so an identical retry
    /// within 500 ms goes through the proxy normally instead of being
    /// dropped with a success-with-note while the daemon never saw it.
    ///
    /// This test pins the predicate side of the invariant. The matching
    /// production change in `run()`'s `tools/call` arm moved the
    /// `last_call = Some(...)` assignment from BEFORE the proxy match
    /// to INSIDE the `Ok(_)` branch.
    #[test]
    fn proxy_failure_does_not_seed_dedup() {
        // State simulating "first proxy errored, last_call stayed as it
        // was before that call". Production code path: the `Err(_)` arm
        // does NOT touch `last_call`.
        let last_call: Option<RecentCall> = None;
        let now = Instant::now() + Duration::from_millis(84); // retry at same 84ms timing
        assert!(
            !is_duplicate_call(
                last_call.as_ref(),
                "send",
                &args_send("lead", "hello"),
                now,
                CONTENT_DEDUP_WINDOW,
            ),
            "RC1 invariant: post-proxy-failure dedup state must NOT block an identical retry"
        );
    }

    /// Window boundary — exactly `window` elapsed must NOT be a duplicate
    /// (strict `<`, not `<=`). Pins the comparison operator.
    #[test]
    fn is_duplicate_call_window_boundary_exclusive() {
        let t0 = Instant::now();
        let last = RecentCall {
            tool: "send".to_string(),
            args: args_send("lead", "hello"),
            at: t0,
        };
        let now = t0 + CONTENT_DEDUP_WINDOW; // exactly 500ms
        assert!(!is_duplicate_call(
            Some(&last),
            "send",
            &args_send("lead", "hello"),
            now,
            CONTENT_DEDUP_WINDOW,
        ));
    }
}

#[cfg(test)]
#[path = "agend-mcp-bridge/review_repro_bootstrap_config_cli.rs"]
mod review_repro_bootstrap_config_cli;
