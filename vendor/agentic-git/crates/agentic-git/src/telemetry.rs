//! Deny formatting + fleet event telemetry: `Disposition`,
//! `build_git_event`/`append_git_event`, and the audit/forensic writers.

use super::*;

/// #2234 defect#2: record a NON-agent (no `AGENTIC_GIT_AGENT`) canonical-cwd
/// `checkout`/`switch <branch>` that the shim is about to pass through via the
/// early-exit `exec_real_git` (it never reaches `classify`). These callers have
/// no agent identity, so attribution relies entirely on PROCESS ANCESTRY — this
/// is the blind spot that left `git checkout origin/main` (canonical-HEAD detach)
/// unattributed. Mirrors `log_init_heartbeat_forensics`: best-effort append to
/// the daemon-observable `fleet_events.jsonl` + a stderr line; NEVER blocks (the
/// caller `exec`s real git immediately after). Instrument-only — no behavior
/// change to the passthrough.
pub(crate) fn log_nonagent_canonical_checkout(home: &str, agent: &str, args: &[String]) {
    if !is_positional_branch_checkout(args) {
        return;
    }
    if !cwd_is_canonical_rooted() {
        return;
    }
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let target_branch = args.get(1).cloned().unwrap_or_default();
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = parent_pid();
    let ancestry = process_ancestry(8);
    // #26: canonical disposition-bearing shape + the shared appender.
    let mut extra = serde_json::Map::new();
    extra.insert("target_branch".into(), serde_json::json!(target_branch));
    extra.insert("argv".into(), serde_json::json!(args));
    extra.insert("cwd".into(), serde_json::json!(cwd));
    extra.insert("ppid".into(), serde_json::json!(ppid));
    extra.insert("process_ancestry".into(), serde_json::json!(ancestry));
    let event = build_git_event("canonical_passthrough_checkout", agent, subcmd, extra);
    append_git_event(home, &event);
    eprintln!(
        "[agentic-git #2234] non-agent canonical-cwd {subcmd} passthrough (HEAD-touching): target={target_branch} ppid={ppid} cwd={cwd} ancestry={ancestry:?}"
    );
}

/// #2158: build the bypass-mutating-op audit record. Pure — the caller supplies the
/// process context — so the json SHAPE is unit-testable without touching the live
/// process. Mirrors `log_nonagent_canonical_checkout`'s record + adds `bypass_layer`.
pub(crate) fn build_bypass_audit_event(
    agent: &str,
    subcmd: &str,
    args: &[String],
    cwd: &str,
    ppid: i32,
    ancestry: &[String],
    bypass_layer: &str,
) -> serde_json::Value {
    // #26: canonical disposition-bearing shape (shared builder).
    let mut extra = serde_json::Map::new();
    extra.insert("argv".into(), serde_json::json!(args));
    extra.insert("cwd".into(), serde_json::json!(cwd));
    extra.insert("ppid".into(), serde_json::json!(ppid));
    extra.insert("process_ancestry".into(), serde_json::json!(ancestry));
    extra.insert("bypass_layer".into(), serde_json::json!(bypass_layer));
    build_git_event("bypass_mutating_op", agent, subcmd, extra)
}

/// #2158: audit a SUB-AGENT's own `AGENTIC_GIT_BYPASS=1 git <mutating>` op — the
/// stray-worktree vector the daemon-side bypass audit (git_helpers.rs, #2242
/// PR2(iii)) cannot see (it audits only the daemon's OWN bypass; the shim is the
/// disjoint agent-side surface). Best-effort append to fleet_events.jsonl (the
/// operator forensics surface, same sink as the #2235 checkout log) + a greppable
/// stderr line; NEVER blocks — the caller `exec`s real git immediately after. The
/// caller gates this to audited ops (Option B) at `shim_depth()==0`.
pub(crate) fn log_bypass_mutating_op(home: &str, agent: &str, args: &[String]) {
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = parent_pid();
    let ancestry = process_ancestry(8);
    let event = build_bypass_audit_event(
        agent,
        subcmd,
        args,
        &cwd,
        ppid,
        &ancestry,
        active_bypass_layer(),
    );
    append_git_event(home, &event);
    eprintln!(
        "[agentic-git #2158] AGENTIC_GIT_BYPASS mutating {subcmd} (stray-worktree vector): ppid={ppid} cwd={cwd} ancestry={ancestry:?}"
    );
}

/// The git `user.email` that WOULD author/commit in `cwd` — i.e. the
/// committer identity the heartbeat commit will carry. Invokes the real git
/// (AGENTIC_GIT_REAL_GIT) to avoid recursing through this shim.
pub(crate) fn effective_git_email(cwd: &str) -> Option<String> {
    let real_git = env_compat("AGENTIC_GIT_REAL_GIT").unwrap_or_else(|_| "git".to_string());
    let out = std::process::Command::new(real_git)
        .args(["-C", cwd, "config", "user.email"])
        .output()
        .ok()?;
    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!email.is_empty()).then_some(email)
}

/// #1463: append a rich forensic record for an intercepted init-heartbeat
/// commit to the daemon-observable `fleet_events.jsonl`, plus a stderr line
/// (surfaces in the agent pane + daemon log). Best-effort; never blocks the
/// commit.
pub(crate) fn log_init_heartbeat_forensics(home: &str, agent: &str, args: &[String]) {
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = parent_pid();
    let ancestry = process_ancestry(8);
    let email = effective_git_email(&cwd).unwrap_or_default();
    let has_allow_empty = args.iter().any(|a| a == "--allow-empty");
    // #26: canonical disposition-bearing shape + the shared appender.
    let mut extra = serde_json::Map::new();
    extra.insert("argv".into(), serde_json::json!(args));
    extra.insert("allow_empty".into(), serde_json::json!(has_allow_empty));
    extra.insert("cwd".into(), serde_json::json!(cwd));
    extra.insert("ppid".into(), serde_json::json!(ppid));
    extra.insert("process_ancestry".into(), serde_json::json!(ancestry));
    extra.insert("git_user_email".into(), serde_json::json!(email));
    let event = build_git_event("init_heartbeat_forensics", agent, "commit", extra);
    append_git_event(home, &event);
    eprintln!(
        "[agentic-git #1463] init-heartbeat commit intercepted: agent={agent} email={email} ppid={ppid} cwd={cwd} ancestry={ancestry:?}"
    );
}

// ── Error + Telemetry ───────────────────────────────────────────────────

pub(crate) fn emit_deny_error(subcmd: &str, reason: &str, agent: &str, binding: Option<&Binding>) {
    for line in format_deny_error(subcmd, reason, agent, binding) {
        eprintln!("{line}");
    }
}

/// #2379 ②: the shared, context-aware "where to run this instead" remedy block,
/// reused by every deny exit so they stay consistent. Pure `format!`, ZERO I/O —
/// `binding` is the IN-SCOPE binding (already loaded before `classify`) at the
/// `Action::Deny` / push-denylist sites, and `None` at the early canonical-bypass
/// deny (env+cwd only, no binding loaded). When the caller is bound, it names the
/// agent's own worktree so the fix is actionable ("cd there"); otherwise it points
/// at the ways to get a worktree. (Intentionally avoids "security"-flavoured
/// wording per the operator copy rule — enforced by a meta-test.)
pub(crate) fn deny_remedy_lines(binding: Option<&Binding>) -> Vec<String> {
    // #2379 ② (r6): decide "bound" by the SAME predicate production uses —
    // `is_bound` (task_id.is_some()) — AND require a worktree to name, so the
    // remedy can never contradict classify's deny verdict. A partial binding
    // (task_id=None, worktree=Some) is UNBOUND to classify, so it must get the
    // generic remedy here too — never a "your assigned worktree is <stale>" line
    // pointing at a path the caller isn't actually assigned to.
    match binding {
        Some(b) if is_bound(b) && b.worktree.is_some() => {
            let wt = b.worktree.as_deref().unwrap_or_default();
            let branch = b.branch.as_deref().unwrap_or("<unknown>");
            let task = b.task_id.as_deref().unwrap_or("—");
            vec![
                format!("           your assigned worktree is {wt}"),
                format!(
                    "           (branch '{branch}', task {task}) — cd there and run git, no bypass needed"
                ),
            ]
        }
        // Unbound / partial binding / no binding in scope: point at how to get
        // one. Tool-agnostic (P3): lead with agentic-git's OWN standalone path,
        // then the orchestrator-generic line — an agend-fleet agent still knows
        // its provisioning tool from its own prompt; a standalone user gets a
        // literal command. No orchestrator-specific vocab hardcoded here.
        _ => vec![
            "           no active worktree binding here — this git call isn't inside a"
                .to_string(),
            "           guarded session. Get one by either:".to_string(),
            "             - launching the agent via `agentic-git run --branch <branch> -- <cmd>`"
                .to_string(),
            "               (standalone: provisions + binds a worktree), or".to_string(),
            "             - having your orchestrator bind this agent to a worktree,"
                .to_string(),
            "               then running git from inside it.".to_string(),
        ],
    }
}

/// #2379 ② (r6): the canonical-bypass deny block as a testable `Vec<String>`.
/// The header + the canonical-specific `AGENTIC_GIT_ALLOW_CANONICAL_MUTATE` bypass
/// are unique to this early deny (no `Binding` is loaded — env+cwd only, so the
/// generic [`deny_remedy_lines`]`(None)` remedy is used). Extracted from the
/// inline `eprintln!`s so the no-"security"-wording meta-test covers this prose
/// too (the inline form was a meta-test blind spot — r6).
pub(crate) fn format_canonical_bypass_deny(agent: &str, sub: &str) -> Vec<String> {
    let mut lines = vec![
        format!(
            "agentic-git: DENIED — agent '{agent}' must not bypass-{sub} in a canonical-rooted repo."
        ),
        "           a stray provision here detaches the operator's canonical HEAD (#2234)."
            .to_string(),
    ];
    lines.extend(deny_remedy_lines(None));
    lines.push(
        "           or, if you genuinely must: set AGENTIC_GIT_ALLOW_CANONICAL_MUTATE=1 for a one-shot (or ask lead)."
            .to_string(),
    );
    lines
}

/// Sprint 54 P2-4: build the deny-error block as a `Vec<String>` so the
/// 3-form bypass hint can be unit-tested for env-var-name presence
/// without capturing stderr. `emit_deny_error` is a thin wrapper that
/// `eprintln!`s each line. Per `should_bypass` (above), three bypass
/// forms exist; the hint enumerates all of them so operators don't
/// have to grep the source to discover the agent-specific or
/// time-limited variants.
///
/// #2379 ②: now carries the in-scope binding context via [`deny_remedy_lines`]
/// so every deny tells the caller WHERE to run the command instead (its own
/// worktree, or how to get one) — not just how to bypass.
pub(crate) fn format_deny_error(
    subcmd: &str,
    reason: &str,
    agent: &str,
    binding: Option<&Binding>,
) -> Vec<String> {
    let mut lines = vec![
        format!("agentic-git: ERROR git {subcmd} denied"),
        format!("           agent={agent}, reason: {reason}"),
    ];
    lines.extend(deny_remedy_lines(binding));
    lines.push("           or bypass with one of:".to_string());
    lines.push(
        "             AGENTIC_GIT_BYPASS=1               one-shot emergency override".to_string(),
    );
    lines.push(
        "             AGENTIC_GIT_BYPASS_AGENT=<name>    agent-specific exemption (matches AGENTIC_GIT_AGENT)"
            .to_string(),
    );
    lines.push(
        "             AGENTIC_GIT_BYPASS_UNTIL=<epoch>   time-limited exemption (Unix seconds, not ISO)"
            .to_string(),
    );
    lines
}

/// #2379 ②: the agent-facing DISPOSITION of a git_event — whether the agent must
/// STOP or may CONTINUE. Distinct from the fleet-events envelope (`"kind":"git_event"`)
/// and from the `event` type string; it is the single axis an agent routes its retry
/// decision on.
/// - `Deny` — terminal, fail-closed: the op was BLOCKED; the agent must fix + retry.
/// - `Warn` — advisory: the op proceeded (or a non-blocking condition was flagged); the
///   agent should heed it but is NOT blocked (e.g. merge conflict, cwd/worktree drift).
/// - `Info` — pure record (e.g. a recognized exemption); no agent action implied.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Disposition {
    Deny,
    Warn,
    Info,
}

impl Disposition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Disposition::Deny => "deny",
            Disposition::Warn => "warn",
            Disposition::Info => "info",
        }
    }
}

/// #2379 ②: the SINGLE SOURCE mapping every emitted `event_type` → its [`Disposition`],
/// so a type's disposition can never drift between call sites. An unmapped type fails
/// CLOSED to `Deny` (an unrecognized event reads as "stop + check", never silently
/// advisory); `disposition_for_covers_all_emitted_event_types_2379` pins every real type.
pub(crate) fn disposition_for(event_type: &str) -> Disposition {
    match event_type {
        "deny" | "deny_trust_root" | "deny_protected_ref" | "deny_snapshot_ref_push" => {
            Disposition::Deny
        }
        // #4: a snapshot failure is advisory, never terminal — the op still
        // ran (fail-open is the whole point); the agent should heed the
        // warning but is not blocked.
        // #26: audited-bypass mutations and unattributed canonical HEAD-touches
        // are advisory-noteworthy instrumentation, never terminal denials.
        "cwd_worktree_drift" | "git_conflict" | "snapshot_failed" | "bypass_mutating_op"
        | "canonical_passthrough_checkout" => Disposition::Warn,
        // #26: heartbeat-pile forensics are routine instrumentation.
        "post_merge_cleanup_exempt" | "init_heartbeat_forensics" => Disposition::Info,
        _ => Disposition::Deny,
    }
}

/// #26: the canonical event-record builder — EVERY `fleet_events.jsonl`
/// record carries `kind`/`event`/`disposition`/`agent`/`subcommand`/
/// `timestamp` (disposition via the single-source [`disposition_for`]);
/// callers contribute event-specific fields through `extra`. Pure, so each
/// writer's json SHAPE stays unit-testable without touching the live process.
pub(crate) fn build_git_event(
    event_type: &str,
    agent: &str,
    subcmd: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    // Canonical fields are AUTHORITATIVE: extras land first, the canonical
    // envelope is written last so a caller-supplied key can never overwrite
    // the routing fields (esp. `disposition` — the stop-vs-continue axis).
    let mut map = extra;
    map.insert("kind".into(), serde_json::json!("git_event"));
    map.insert("event".into(), serde_json::json!(event_type));
    map.insert(
        "disposition".into(),
        serde_json::json!(disposition_for(event_type).as_str()),
    );
    map.insert("agent".into(), serde_json::json!(agent));
    map.insert("subcommand".into(), serde_json::json!(subcmd));
    map.insert(
        "timestamp".into(),
        serde_json::json!(chrono::Utc::now().to_rfc3339()),
    );
    serde_json::Value::Object(map)
}

/// #26: the single best-effort `fleet_events.jsonl` appender (never blocks;
/// callers `exec` real git immediately after).
pub(crate) fn append_git_event(home: &str, event: &serde_json::Value) {
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{event}");
    }
}

/// Sprint 57 Wave 2 Track D: structured audit-event writer with an
/// explicit event-type discriminator. Replaces the previous untyped
/// `write_git_event` that hardcoded `event="deny"`. `event_type` is
/// the new `kind`-style discriminator (`"deny"` or
/// `"post_merge_cleanup_exempt"`); `target_branch` carries the
/// resolved checkout target when relevant for the exemption case;
/// `detail` mirrors the human-readable reason string.
///
/// #2379 ②: every event also carries a `disposition` (deny|warn|info, via
/// [`disposition_for`]) so an agent reading `fleet_events.jsonl` can route deny
/// (must-stop) vs warn (advisory) WITHOUT re-deriving it from the `event` string.
pub(crate) fn write_git_event_typed(
    home: &str,
    agent: &str,
    subcmd: &str,
    event_type: &str,
    target_branch: Option<&str>,
    detail: Option<&str>,
) {
    // #2379 ② / #26: disposition + shape come from the canonical builder.
    let mut extra = serde_json::Map::new();
    extra.insert("target_branch".into(), serde_json::json!(target_branch));
    extra.insert("reason".into(), serde_json::json!(detail));
    let event = build_git_event(event_type, agent, subcmd, extra);
    append_git_event(home, &event);
}
