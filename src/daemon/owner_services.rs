//! #2453 Stage 1a: two host-agnostic helpers that own the WIRING (not the
//! JoinHandles) of the owner-only background services started identically by
//! BOTH run hosts — the TUI owned-mode `app::run_app` and the headless
//! `daemon::run_core` (via `build_tick_infrastructure`). Extracting the shared
//! spawn calls into one place closes the dual-host drift class (#982/#1002/
//! #1720/#2434: "wired in one host, silently dead in the other"). Pure,
//! behavior-preserving: same calls, same order, same args, same cfg — no new
//! threads/locks/state/channels.
//!
//! Deliberately EXCLUDED (each a separate decision fork, per d-20260711201257672833-2):
//! `shadow::start` (different ordering per host), `supervisor::spawn` (`#[cfg(unix)]`
//! delta), `router`/`TaskSweep`/recovery (headless-only), bootstrap/restart/shutdown.

use crate::agent::AgentRegistry;
use std::path::Path;
use std::sync::Arc;

/// Owner-only agent liveness/activity monitoring services. Each spawns a
/// process-lifetime thread internally and returns `()` — this helper owns the
/// WIRING, not a `JoinHandle`. Called identically by owned `run_app` and headless
/// `build_tick_infrastructure`.
pub(crate) fn start_shared_monitoring_services(home: &Path, registry: &AgentRegistry) {
    crate::instance_monitor::spawn_monitor_tick(home.to_path_buf(), Arc::clone(registry));
    // #2413 Phase 1: out-of-path lsof API-activity probe (feeds
    // AgentCore::api_activity for false-idle detection). Self-disables if `lsof`
    // is absent.
    crate::api_activity_probe::spawn(Arc::clone(registry));
}

/// Owner-only Shadow Observer per-backend Evidence-SOURCE planes (Stream plane).
/// Each is a no-op under `AGEND_SHADOW_OBSERVER=0` (default-ON). The live fleet
/// daemon is app mode, so gating these run_core-only would leave each backend's
/// observer source dead in production (#2434). `shadow::start` (the socket-ingest
/// plane) is deliberately NOT here — its per-host ordering differs (separate fork).
pub(crate) fn start_shared_stream_observers(home: &Path, registry: &AgentRegistry) {
    // #2413 Phase D: codex rollout-tail — read-only tail of
    // ~/.codex/sessions/.../rollout-*.jsonl → Evidence → shared buffer.
    crate::daemon::shadow::rollout::spawn(Arc::clone(registry), home.to_path_buf());
    // #2413 opencode plane: SSE `/event` observer (per-agent embedded server).
    crate::daemon::shadow::opencode::spawn(Arc::clone(registry), home.to_path_buf());
    // #2413 kiro plane: read-only tail of ~/.kiro/sessions/cli/<uuid>.jsonl.
    crate::daemon::shadow::kiro::spawn(Arc::clone(registry), home.to_path_buf());
}

#[cfg(test)]
mod tests {
    /// The five spawn calls this stage folds into the two helpers, matched as
    /// path-suffix substrings so the guard is robust to the `crate::…` prefix.
    const MOVED_SPAWNS: [&str; 5] = [
        "instance_monitor::spawn_monitor_tick(",
        "api_activity_probe::spawn(",
        "shadow::rollout::spawn(",
        "shadow::opencode::spawn(",
        "shadow::kiro::spawn(",
    ];

    const HELPERS: [&str; 2] = [
        "start_shared_monitoring_services(",
        "start_shared_stream_observers(",
    ];

    /// Read a repo-relative source file, tolerating either cwd (crate root or
    /// workspace root — mirrors the existing `run_app_wires_*` pins).
    fn read_source(rel: &str) -> String {
        std::fs::read_to_string(rel)
            .or_else(|_| std::fs::read_to_string(format!("agend-terminal/{rel}")))
            .unwrap_or_else(|_| panic!("source file must be readable from test cwd: {rel}"))
    }

    /// Production region only, with comments stripped and string-literal CONTENTS
    /// blanked — so neither a commented-out call nor a string literal that
    /// happens to contain a needle can satisfy (or defeat) a wiring pin.
    /// String/char/raw-string/lifetime aware. Local test-only helper (the
    /// equivalent in `app::tests` is private; production visibility is
    /// intentionally NOT widened for tests).
    ///
    /// Masks FIRST, then drops the test module at the first `mod tests {` in the
    /// masked source. This is robust to a stray item-level `#[cfg(test)]` before
    /// the real module (e.g. daemon's `test_env_lock`) and to an attribute
    /// between the `#[cfg(test)]` and `mod tests {` (e.g. `#[allow(...)]`) — a
    /// naive "cut at the first `#[cfg(test)]`" truncates those files early and
    /// makes the absence pins pass vacuously.
    fn prod_masked(rel: &str) -> String {
        let masked = mask_code(&read_source(rel));
        let end = masked.find("mod tests {").unwrap_or(masked.len());
        masked[..end].to_string()
    }

    /// Strip `//` line + `/* */` block comments and BLANK the contents of string
    /// literals (keeping the delimiters). String-literal-aware: `//`/`/*` inside a
    /// `"…"`, `'…'`, or raw string is not a comment; lifetimes (`'a`) are left as
    /// normal chars. Faithful, compact clone of the proven app-side stripper.
    fn mask_code(src: &str) -> String {
        let s: Vec<char> = src.chars().collect();
        let n = s.len();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < n {
            let c = s[i];
            // line comment → drop to (but keep) the newline.
            if c == '/' && i + 1 < n && s[i + 1] == '/' {
                i += 2;
                while i < n && s[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // block comment → drop to `*/`.
            if c == '/' && i + 1 < n && s[i + 1] == '*' {
                i += 2;
                while i + 1 < n && !(s[i] == '*' && s[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                continue;
            }
            // raw string `r"…"` / `r#"…"#` / `br"…"` — blank contents, keep delimiters.
            if c == 'r' || (c == 'b' && i + 1 < n && s[i + 1] == 'r') {
                let r_pos = if c == 'b' { i + 1 } else { i };
                let mut k = r_pos + 1;
                let mut hashes = 0;
                while k < n && s[k] == '#' {
                    hashes += 1;
                    k += 1;
                }
                if k < n && s[k] == '"' {
                    for ch in &s[i..=k] {
                        out.push(*ch); // opening `r###"`
                    }
                    i = k + 1;
                    loop {
                        if i >= n {
                            break;
                        }
                        if s[i] == '"' {
                            let mut h = 0;
                            while i + 1 + h < n && h < hashes && s[i + 1 + h] == '#' {
                                h += 1;
                            }
                            if h == hashes {
                                for ch in &s[i..i + 1 + hashes] {
                                    out.push(*ch); // closing `"###`
                                }
                                i += 1 + hashes;
                                break;
                            }
                        }
                        i += 1; // blank the raw-string content
                    }
                    continue;
                }
                // plain identifier starting with r/b → fall through.
            }
            // normal / byte string `"…"` — blank contents, honor `\` escapes.
            if c == '"' {
                out.push(c);
                i += 1;
                while i < n {
                    if s[i] == '\\' && i + 1 < n {
                        i += 2; // skip the escape pair (blanked)
                        continue;
                    }
                    let closing = s[i] == '"';
                    i += 1;
                    if closing {
                        out.push('"');
                        break;
                    }
                    // else: blanked
                }
                continue;
            }
            // char literal `'x'` / `'\n'` (vs a lifetime `'a`) — copy verbatim.
            if c == '\'' {
                if i + 1 < n && s[i + 1] == '\\' {
                    out.push(s[i]);
                    out.push(s[i + 1]);
                    i += 2;
                    if i < n {
                        out.push(s[i]);
                        i += 1;
                    }
                    while i < n && s[i] != '\'' {
                        out.push(s[i]);
                        i += 1;
                    }
                    if i < n {
                        out.push(s[i]);
                        i += 1;
                    }
                    continue;
                }
                if i + 2 < n && s[i + 2] == '\'' {
                    out.push(s[i]);
                    out.push(s[i + 1]);
                    out.push(s[i + 2]);
                    i += 3;
                    continue;
                }
                // lifetime / stray `'` → normal char.
            }
            out.push(c);
            i += 1;
        }
        out
    }

    /// The `if !attached_mode { … }` owner-guard block in app production (masked),
    /// extracted by brace-matching — braces inside strings/comments are already
    /// gone, so the match is exact.
    fn app_owner_guard_block(masked_app: &str) -> String {
        let start = masked_app
            .find("if !attached_mode {")
            .expect("app production must contain the `if !attached_mode {` owner guard");
        let after = &masked_app[start..];
        let mut depth = 0i32;
        let mut end = after.len();
        for (idx, ch) in after.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = idx + ch.len_utf8();
                        break;
                    }
                }
                _ => {}
            }
        }
        after[..end].to_string()
    }

    /// G1 (STATIC, not runtime): BOTH production hosts call BOTH shared helpers.
    /// Deleting either host's helper call turns this RED — the dual-host wiring
    /// path is structurally required.
    #[test]
    fn both_hosts_call_the_two_shared_helpers() {
        for host in ["src/app/mod.rs", "src/daemon/mod.rs"] {
            let code = prod_masked(host);
            for helper in HELPERS {
                assert!(
                    code.contains(helper),
                    "{host} production must call `{helper}` (dual-host shared-wiring path)"
                );
            }
        }
    }

    /// G2 (STATIC, not runtime): the five spawn calls live ONLY inside
    /// `owner_services` — absent from both host bodies, present in this module.
    /// Moving a spawn back into a host (or dropping it from the helper) turns
    /// this RED: one wiring site, no silent per-host drift.
    #[test]
    fn moved_spawns_live_only_in_owner_services() {
        for host in ["src/app/mod.rs", "src/daemon/mod.rs"] {
            let code = prod_masked(host);
            for spawn in MOVED_SPAWNS {
                assert!(
                    !code.contains(spawn),
                    "{host} must NOT call `{spawn}` directly — it moved into owner_services \
                     (single wiring site; a host-local copy reintroduces dual-host drift)"
                );
            }
        }
        let helper = prod_masked("src/daemon/owner_services.rs");
        for spawn in MOVED_SPAWNS {
            assert!(
                helper.contains(spawn),
                "owner_services must contain `{spawn}` (the one shared wiring site)"
            );
        }
    }

    /// G3 (STATIC, not runtime): in app mode both helper calls sit INSIDE the
    /// `if !attached_mode` owner guard — an attached TUI must never start the
    /// shared daemon services. Moving a call outside the guard turns this RED.
    #[test]
    fn app_helper_calls_are_inside_the_attached_mode_owner_guard() {
        let code = prod_masked("src/app/mod.rs");
        let block = app_owner_guard_block(&code);
        for helper in HELPERS {
            assert!(
                block.contains(helper),
                "app must call `{helper}` INSIDE the `if !attached_mode` owner guard \
                 (attached TUI must not start shared services)"
            );
        }
    }
}
