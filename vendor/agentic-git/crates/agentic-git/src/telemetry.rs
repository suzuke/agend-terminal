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
            "           no active worktree binding here — this git call isn't inside a".to_string(),
            "           guarded session. Get one by either:".to_string(),
            "             - launching the agent via `agentic-git run --branch <branch> -- <cmd>`"
                .to_string(),
            "               (standalone: provisions + binds a worktree), or".to_string(),
            "             - having your orchestrator bind this agent to a worktree,".to_string(),
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
        "cwd_worktree_drift"
        | "git_conflict"
        | "snapshot_failed"
        | "bypass_mutating_op"
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
    // #3416: goes through the one serialized appender. Best-effort by contract —
    // a single non-blocking lock attempt, and on contention the record is SKIPPED.
    // Skipping is what preserves the never-blocks guarantee documented above; an
    // unlocked fallback would preserve it by reintroducing the interleaving that
    // corrupted 44.6% of recent records.
    let _ = agentic_audit_append::append_audit_line_best_effort(std::path::Path::new(home), event);
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

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Env var that switches this test binary into stress-worker mode.
    const WORKER_HOME: &str = "AGEND_3416_STRESS_WORKER_HOME";
    /// Fully-qualified name of the stress test, used to re-exec ourselves as a
    /// worker. Kept next to the test so a rename breaks loudly rather than
    /// silently spawning nothing.
    const STRESS_TEST_PATH: &str =
        "telemetry::tests::concurrent_append_git_event_writes_only_parseable_records";
    const WORKERS: usize = 8;
    const RECORDS_PER_WORKER: usize = 400;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-3416-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn sample_event(worker: &str, i: usize) -> serde_json::Value {
        // Shaped like a real record: production rows are p50 ~2 KB, and the size
        // matters because it is what makes a multi-syscall write interleave.
        serde_json::json!({
            "kind": "git_event",
            "event": "deny",
            "agent": worker,
            "seq": i,
            "process_ancestry": vec!["x".repeat(600); 3],
            "timestamp": "2026-08-28T00:00:00+00:00",
        })
    }

    /// Take an exclusive, non-blocking `flock` on `path`, returning the holder.
    /// `flock(2)` binds the lock to the OPEN FILE DESCRIPTION, so a second
    /// `open()` in this same process contends exactly as another process would —
    /// which is what lets these tests stay single-process and sleep-free.
    fn hold_lock(path: &std::path::Path) -> std::fs::File {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .unwrap();
        let rc = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&f),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        assert_eq!(rc, 0, "test must be able to take the companion lock");
        f
    }

    /// #3416 RED: the shim sink is best-effort and documented as "never blocks"
    /// (callers `exec` real git immediately after). Serializing it must keep that
    /// contract by SKIPPING on contention — never by falling back to an unlocked
    /// append, which would reintroduce exactly the interleaving being fixed.
    ///
    /// Pre-fix this FAILS: nothing consults the lock, so the record is appended.
    #[test]
    fn append_git_event_skips_while_companion_lock_is_held() {
        let home = tmp_home("skip-on-contention");
        let holder = hold_lock(&home.join("fleet_events.jsonl.lock"));

        let started = std::time::Instant::now();
        append_git_event(home.to_str().unwrap(), &sample_event("shim", 0));
        let elapsed = started.elapsed();

        let written = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap_or_default();
        assert!(
            written.trim().is_empty(),
            "best-effort sink must skip while the lock is held, never append unlocked; found: {written}"
        );
        // r1 F1: skipped and stalled-then-skipped leave the same empty file above;
        // only elapsed separates best-effort from a bounded retry.
        assert!(
            elapsed < agentic_audit_append::DEFAULT_BOUNDED_BUDGET / 2,
            "best-effort sink must return promptly on contention, never retry for the bounded budget; took {elapsed:?}"
        );

        drop(holder);
        std::fs::remove_dir_all(&home).ok();
    }

    /// The "after release" half of the contract. A best-effort sink may skip while
    /// the lock is held, but once it is free the very next append must land, intact
    /// and exactly once.
    ///
    /// Note what is deliberately NOT asserted: that a quiet period guarantees
    /// delivery. There is no such guarantee in a process that spawns children. A
    /// concurrent `fork` duplicates every open file descriptor, so the lock's open
    /// file description outlives the parent's `close` until the child `exec`s, and
    /// a best-effort `try_lock` can see `WouldBlock` with NO competing writer at
    /// all. Measured here: 0/15 runs hit it with no child spawning, ~2/27 with the
    /// stress test's 8 spawns running alongside. Delivery is therefore pinned on
    /// the bounded (retrying) path, in the appender crate's own tests.
    #[test]
    fn append_git_event_lands_once_the_lock_is_released() {
        let home = tmp_home("after-release");
        let holder = hold_lock(&agentic_audit_append::lock_path(&home));

        append_git_event(home.to_str().unwrap(), &sample_event("held", 0));
        assert!(
            std::fs::read_to_string(agentic_audit_append::audit_path(&home))
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "nothing may be written while the lock is held"
        );

        drop(holder);
        // `append_git_event` is best-effort BY CONTRACT: one non-blocking lock
        // attempt, skipped on contention, because the shim must never block. So
        // "the lock was released" does not promise that the very NEXT attempt
        // wins — only that attempts are no longer refused by our holder. Measured
        // under the parallel suite: this single call skipped with `Contended` in
        // 5 of 60 runs, while 2000 sequential release-then-append cycles skipped
        // zero times. Asserting on one attempt was asserting more than the API
        // offers; a bounded retry pins what the test actually means.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            append_git_event(home.to_str().unwrap(), &sample_event("freed", 1));
            let landed = std::fs::read_to_string(agentic_audit_append::audit_path(&home))
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false);
            if landed || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }

        let content =
            std::fs::read_to_string(agentic_audit_append::audit_path(&home)).unwrap_or_default();
        let rows: Vec<serde_json::Value> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("row written after release must be parseable"))
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "exactly the post-release record, got: {content}"
        );
        assert_eq!(
            rows[0]["seq"], 1,
            "the skipped record must not reappear later"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #3416 RED: the defect itself. Real production entry, real concurrency,
    /// separate PROCESSES. Deterministic without any sleep because the pre-fix
    /// failure is not probabilistic — a multi-syscall `O_APPEND` write interleaves
    /// essentially always under this load (measured: ~100% of records corrupt).
    #[test]
    fn concurrent_append_git_event_writes_only_parseable_records() {
        // Worker mode: this process was re-exec'd by the parent below.
        if let Ok(home) = std::env::var(WORKER_HOME) {
            let tag = format!("w{}", std::process::id());
            for i in 0..RECORDS_PER_WORKER {
                append_git_event(&home, &sample_event(&tag, i));
            }
            return;
        }

        let home = tmp_home("stress");
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..WORKERS {
            children.push(
                std::process::Command::new(&exe)
                    .args(["--exact", STRESS_TEST_PATH, "--nocapture"])
                    .env(WORKER_HOME, &home)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut c in children {
            let st = c.wait().unwrap();
            assert!(st.success(), "stress worker failed: {st}");
        }

        let content = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap_or_default();
        let mut rows: Vec<(&str, serde_json::Value)> = Vec::new();
        let mut corrupt = 0usize;
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) if v.is_object() => rows.push((line, v)),
                _ => corrupt += 1,
            }
        }
        let parsed = rows.len();
        // The invariant is INTEGRITY of what survives, NOT delivery of everything.
        // This sink is best-effort by contract (one try_lock, skip on contention),
        // so under saturation some records are legitimately absent and this test
        // must not claim zero loss. Delivery is pinned separately, on the bounded
        // path, in the appender crate's own tests.
        assert_eq!(
            corrupt,
            0,
            "every record that reaches the log must be intact; {corrupt} corrupt of {} lines",
            parsed + corrupt
        );
        assert!(
            parsed > 0,
            "the appender must still deliver records under contention, got none"
        );

        // Every surviving row must be a complete, well-formed record — not merely
        // parseable JSON — and must appear exactly once. Duplication would mean a
        // retry wrote twice; a malformed row would mean a partial write survived.
        let mut seen = std::collections::HashSet::new();
        for (raw, row) in &rows {
            let agent = row["agent"]
                .as_str()
                .expect("surviving row must carry agent");
            let seq = row["seq"].as_u64().expect("surviving row must carry seq");
            assert!(
                seen.insert((agent.to_string(), seq)),
                "record ({agent}, {seq}) surfaced more than once"
            );
            assert!(
                (seq as usize) < RECORDS_PER_WORKER,
                "record ({agent}, {seq}) is outside the attempted domain"
            );
            // The load-bearing assertion: a surviving line must be EXACTLY what the
            // appender renders for that record. Parsing alone is far too weak — a
            // reordered, re-serialized or field-dropped write still parses and still
            // looks unique, and this is an audit trail, so "close enough" is not a
            // property worth having.
            let expected = sample_event(agent, seq as usize).to_string();
            assert_eq!(
                *raw, expected,
                "surviving record ({agent}, {seq}) is not byte-identical to what the \
                 appender renders"
            );
        }

        // Skip accounting is REPORTED, not asserted. The previous version asserted
        // `parsed + skipped == attempted` after defining `skipped` as
        // `attempted - parsed`, which is true by construction and therefore proved
        // nothing. What is actually worth pinning is that no record can appear that
        // was never attempted, which the domain and byte-identity assertions above
        // do carry.
        let attempted = WORKERS * RECORDS_PER_WORKER;
        let skipped = attempted - parsed;
        assert!(
            parsed <= attempted,
            "more records surfaced ({parsed}) than were attempted ({attempted})"
        );
        // Measured on the authoring machine: 1532 of 3200 survived, i.e. roughly
        // half skipped with 8 processes appending back-to-back with no gap. That is
        // a SATURATION envelope, not a production rate — real fleet load is a median
        // of 3 events/sec with an 86 ms median inter-arrival. Recorded here so a
        // future production-rate probe has a reference point, not as a claim about
        // normal operation.
        eprintln!("stress: attempted={attempted} survived={parsed} skipped={skipped}");

        std::fs::remove_dir_all(&home).ok();
    }
}
