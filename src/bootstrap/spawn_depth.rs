//! Recursion-guard for `agend-terminal`-spawns-`agend-terminal`.
//!
//! Tracks how deep into a self-spawn chain we are via the
//! `AGEND_SPAWN_DEPTH` env var. Every legitimate spawn site reads the
//! current depth, bails if it has reached [`THRESHOLD`], and otherwise
//! sets `current + 1` on the child's env.
//!
//! Why: #882 shipped a fork-bomb on cold start because `spawn_detached`'s
//! child re-entered main's `Start` arm without `--foreground`, recursively
//! calling `spawn_detached` again. 800f1cc + 446b9ed plugged the two known
//! callers via `--foreground`, but those are content-fixes; the guard here
//! is the structural fix that catches any future regression at any new
//! spawn site.
//!
//! Threshold rationale (from #879v3 E2 spike): the deepest legitimate
//! agend-spawns-agend chain is parent (app | tray | OS service) → daemon
//! (`start --foreground`, no further self-spawn). That tops out at depth 1.
//! Threshold = 2 gives one frame of headroom for that legitimate case
//! and hard-stops the grandchild that would mark a fork bomb.

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicU64, Ordering};

/// Env var name the guard reads/writes.
pub const ENV_KEY: &str = "AGEND_SPAWN_DEPTH";

/// Depth at which a self-spawn must bail. See module doc for rationale.
pub const THRESHOLD: u8 = 2;

/// Identity for system-generated inbox notifications on guard fire.
const NOTIFICATION_FROM: &str = "system:879v3-guard";

/// Inbox target for auto-notify on guard fire. Hard-coded to the fixup-lead
/// orchestrator because #879v3 is a fixup-team-owned safety class; if other
/// teams ever inherit the guard, fold this into a fleet-config lookup.
const NOTIFICATION_TARGET: &str = "fixup-lead";

/// Stable `kind` tag for inbox messages emitted on guard fire. Lead's
/// inbox-filtering / soak-monitor scripts key on this string.
const NOTIFICATION_KIND: &str = "spike_879v3_guard_fire";

/// Process-global counter incremented every time [`check`] returns Err
/// because [`THRESHOLD`] was reached. Operator-observable signal for the
/// 24-48hr post-merge soak window: ANY non-zero value during soak triggers
/// the documented revert criterion. Exposed via [`fire_count`].
///
/// `Relaxed` ordering is sufficient — we don't need cross-thread happens-
/// before with any other state; the counter is purely informational. The
/// auto-notify path runs immediately after the increment in the same
/// thread, so per-fire ordering is naturally preserved.
pub static GUARD_FIRES: AtomicU64 = AtomicU64::new(0);

/// Read the current guard-fire counter. Zero means the guard has not
/// caught any recursive-spawn attempt since this process started.
///
/// Surfaced via `agend-terminal status` / `list` / `status --json` (under
/// the `agend_spawn_depth_guard_fires` key) so soak-window monitoring
/// scripts can grep / jq for the value without parsing daemon.log lines.
pub fn fire_count() -> u64 {
    GUARD_FIRES.load(Ordering::Relaxed)
}

/// Current depth as observed from the process env. Missing or malformed
/// values are treated as 0 — defense-in-depth so an attacker / accidental
/// env-poisoning that sets `AGEND_SPAWN_DEPTH=abc` does not silently
/// disable the guard.
pub fn current() -> u8 {
    std::env::var(ENV_KEY)
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0)
}

/// Check guard at a spawn site or Start-arm entry. Returns the depth value
/// the child should run at (`current + 1`) on the legitimate path, or an
/// `Err` carrying the fork-bomb-signature message if depth has reached
/// [`THRESHOLD`].
///
/// On the bail path: increments [`GUARD_FIRES`], emits a structured
/// `tracing::error!` event with the `spike_879v3_guard_fire = true` tag
/// (operator/lead soak-monitor scripts key on this), and dispatches a
/// best-effort inbox notification to the fixup-lead so a guard fire at
/// 2am wakes the right person rather than waiting on a log-grep cron.
/// Notification I/O failures are intentionally swallowed (`let _ =`) so
/// the guard's bail return is never blocked by a transient filesystem
/// or daemon-not-yet-ready condition — the structured tracing event +
/// counter still record the fire even when auto-notify fails.
pub fn check() -> Result<u8> {
    let depth = current();
    if depth >= THRESHOLD {
        record_guard_fire(depth);
        bail!(
            "AGEND_SPAWN_DEPTH={depth} reached threshold {THRESHOLD} — \
             refusing recursive self-spawn (fork-bomb guard, see #882 RCA). \
             If you reached this legitimately, that means a new agend-spawns-agend \
             code path needs a different mechanism; do not raise the threshold."
        );
    }
    Ok(depth + 1)
}

/// Side-effect surface for a guard fire. Split out from [`check`] so tests
/// can exercise it directly without colliding with the process-global env
/// state the guard reads.
fn record_guard_fire(depth: u8) {
    let count = GUARD_FIRES.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::error!(
        spike_879v3_guard_fire = true,
        depth,
        threshold = THRESHOLD,
        guard_fire_count = count,
        "AGEND_SPAWN_DEPTH guard fired — refusing recursive self-spawn (#879v3 / #882 RCA)"
    );
    let home = crate::home_dir();
    let _ = enqueue_lead_notification(&home, depth, count);
}

/// Best-effort inbox enqueue for the fixup-lead auto-notify. Returns the
/// raw `anyhow::Result` for testability; production callers in
/// [`record_guard_fire`] ignore the result.
///
/// Works from any process — `crate::inbox::enqueue` is a file-system
/// append to `{home}/inbox/<target>.jsonl` and does not require the
/// daemon to be running. This covers BOTH context (a) daemon-side
/// guard fires AND context (b) cold-start fires (`agend-terminal tui`
/// before its auto-spawned daemon is up); the lead picks up either
/// flavor on the next inbox drain.
fn enqueue_lead_notification(home: &std::path::Path, depth: u8, count: u64) -> Result<()> {
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        from: NOTIFICATION_FROM.to_string(),
        text: format!(
            "[{NOTIFICATION_FROM}] AGEND_SPAWN_DEPTH guard fired \
             — depth={depth}, threshold={THRESHOLD}, fire_count={count}. \
             This is the fork-bomb-signature (#882 RCA) the #879v3 safeguard \
             7 monitors. If observed during 24-48hr soak: revert PR + reopen \
             #879 per documented revert criteria."
        ),
        kind: Some(NOTIFICATION_KIND.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        delivery_mode: Some("inbox_fallback".to_string()),
        task_id: None,
        force_meta: None,
        correlation_id: None,
        reviewed_head: None,
        attachments: Vec::new(),
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        sequencing: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
    };
    crate::inbox::enqueue(home, NOTIFICATION_TARGET, msg)
}

/// Canonical args + env for `agend-terminal start --foreground`. Produced
/// once per spawn site so all three legitimate self-spawn paths
/// (`agend-terminal start` default-detach branch, `agend-terminal tui`
/// auto-spawn-on-missing-daemon, tray "Start daemon" menu) converge at
/// the SPEC level. Decoupled from Command construction so each caller
/// preserves its own #548 Q7 separation contract:
///
/// - `bootstrap::daemon_spawn::spawn_detached` builds the Command + sets
///   stdio + applies detach flags (the canonical owner of the import).
/// - Tray builds its own Command via `Command::new(current_exe()?)` per
///   #548 Q7 (no `bootstrap::daemon_spawn` import); the spec it consumes
///   here is import-clean (only `bootstrap::spawn_depth`).
///
/// The spec carries the load-bearing invariants the #879v3 spike
/// captured:
/// - args = `["start", "--foreground"]` (+ optional `["--fleet", path]`)
/// - env = `[(AGEND_SPAWN_DEPTH, current+1)]`
///
/// Tested by `canonical_spawn_args_*` in this module + the
/// `tests/issue_879v3_canonical_spawn_invariants` source-scanning
/// invariants.
///
/// Errors when [`check`] bails: caller has reached the fork-bomb
/// threshold and must not allocate any further child resources.
pub fn canonical_spawn_args(fleet_path: Option<&std::path::Path>) -> Result<CanonicalSpawnSpec> {
    let next_depth = check()?;
    let mut args = vec!["start".to_string(), "--foreground".to_string()];
    if let Some(fp) = fleet_path {
        args.push("--fleet".to_string());
        args.push(fp.display().to_string());
    }
    let env = vec![(ENV_KEY.to_string(), next_depth.to_string())];
    Ok(CanonicalSpawnSpec { args, env })
}

/// Canonical-spawn build artifact produced by [`canonical_spawn_args`].
/// Decoupled from `Command` so the spec is import-friendly for any caller
/// (tray, CLI Start arm, app auto-spawn) that needs the args/env without
/// pulling in `bootstrap::daemon_spawn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSpawnSpec {
    /// Argv slots after the binary path. Always begins with
    /// `["start", "--foreground"]`; appends `["--fleet", <path>]` when a
    /// fleet override was supplied.
    pub args: Vec<String>,
    /// Env pairs to set on the child Command. Currently always exactly
    /// `[(AGEND_SPAWN_DEPTH, current+1)]`; the Vec shape leaves room for
    /// the auto-notify wiring to add follow-on entries without a breaking
    /// signature change.
    pub env: Vec<(String, String)>,
}

impl CanonicalSpawnSpec {
    /// Apply the spec's args + env to an externally-built [`Command`].
    /// Caller retains responsibility for `Command::new(...)` (so tray can
    /// resolve `current_exe()` itself per #548 Q7), stdio configuration,
    /// and platform-specific detach flags via [`apply_detach_flags`].
    pub fn apply_to(&self, cmd: &mut std::process::Command) {
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
    }
}

/// Platform-appropriate detach flags. Unix: `process_group(0)` so Ctrl+C
/// on the parent terminal doesn't propagate to the daemon. Windows:
/// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`. Each spawn site applies
/// this exactly once before `.spawn()`.
pub fn apply_detach_flags(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x00000008) | CREATE_NEW_PROCESS_GROUP (0x00000200)
        cmd.creation_flags(0x00000008 | 0x00000200);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize env mutations across tests in this module — `std::env::set_var`
    // is process-global, and cargo runs tests in the same process in parallel
    // by default. A mutex (not `serial_test`) keeps this module self-contained.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<R>(value: Option<&str>, body: impl FnOnce() -> R) -> R {
        // PoisonError is recoverable here — a poisoned lock means a prior
        // test panicked while holding it. We restore env after the body
        // regardless, so taking the value through poison is safe.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(ENV_KEY).ok();
        match value {
            Some(v) => std::env::set_var(ENV_KEY, v),
            None => std::env::remove_var(ENV_KEY),
        }
        let out = body();
        match prior {
            Some(p) => std::env::set_var(ENV_KEY, p),
            None => std::env::remove_var(ENV_KEY),
        }
        out
    }

    #[test]
    fn unset_env_starts_at_zero_and_child_gets_one() {
        with_env(None, || {
            assert_eq!(current(), 0);
            let next = check().expect("legit first spawn");
            assert_eq!(next, 1, "child env AGEND_SPAWN_DEPTH should be 1");
        });
    }

    #[test]
    fn depth_zero_explicit_allows_legit_first_spawn() {
        with_env(Some("0"), || {
            let next = check().expect("legit");
            assert_eq!(next, 1);
        });
    }

    #[test]
    fn depth_one_allows_legit_daemon_layer() {
        // Daemon child sees AGEND_SPAWN_DEPTH=1 (set by parent). If the
        // daemon entry attempts a further self-spawn (the pre-hotfix #882
        // path), the guard allows one more frame; the grandchild then
        // bails. Threshold=2 leaves exactly this frame of headroom.
        with_env(Some("1"), || {
            let next = check().expect("daemon layer allowed");
            assert_eq!(next, 2);
        });
    }

    /// LOAD-BEARING — proves the fork-bomb guard fires deterministically at
    /// the boundary that mattered for #882. Reviewer §3.20 SOP 3 RED protocol
    /// will revert the `check()` body to `Ok(0)` and observe this test FAIL,
    /// then re-apply and observe PASS.
    #[test]
    fn depth_two_bails_recursive_fork_bomb() {
        with_env(Some("2"), || {
            let err = check().expect_err("must bail at threshold");
            let msg = format!("{err}");
            assert!(
                msg.contains("AGEND_SPAWN_DEPTH=2"),
                "error must surface the observed depth (got: {msg})"
            );
            assert!(
                msg.contains("threshold 2"),
                "error must surface the threshold (got: {msg})"
            );
            assert!(
                msg.contains("fork-bomb guard"),
                "error must reference the #882 RCA (got: {msg})"
            );
        });
    }

    #[test]
    fn malformed_env_treated_as_unset() {
        // Defense-in-depth: an attacker / accidental env-poisoning that
        // sets `AGEND_SPAWN_DEPTH=abc` MUST NOT silently disable the
        // guard. Parse failure → 0 → legitimate first spawn allowed.
        with_env(Some("not-a-number"), || {
            assert_eq!(current(), 0, "malformed env defaults to 0, not to ∞");
            let next = check().expect("malformed = treat as 0");
            assert_eq!(next, 1);
        });
    }

    #[test]
    fn high_value_above_threshold_bails() {
        with_env(Some("99"), || {
            let err = check().expect_err("any depth >= threshold must bail");
            assert!(format!("{err}").contains("AGEND_SPAWN_DEPTH=99"));
        });
    }

    /// LOAD-BEARING (#879v3 safeguard 7 counter): the 24-48hr soak revert
    /// criterion is "if GUARD_FIRES counter > 0 OR MCP tools count == 0 →
    /// revert PR + reopen #879". If `check()`'s bail path stops incrementing
    /// `GUARD_FIRES`, the soak criterion becomes silently unmeasurable —
    /// hence this RED-protocol target.
    #[test]
    fn check_bail_increments_guard_fires_counter() {
        with_env(Some("2"), || {
            let before = fire_count();
            let _ = check().expect_err("must bail at threshold");
            let after = fire_count();
            assert_eq!(
                after,
                before + 1,
                "GUARD_FIRES must increment on every bail; observed {before} → {after}"
            );
            // Second bail in same process — counter accumulates.
            let _ = check().expect_err("must bail again");
            assert_eq!(
                fire_count(),
                before + 2,
                "GUARD_FIRES must accumulate across multiple fires in the same process"
            );
        });
    }

    /// Ok-path must NOT increment the counter — only bails count for the
    /// soak monitor. If this regresses, every legitimate spawn looks like a
    /// fire and the revert criterion would trigger on healthy launches.
    #[test]
    fn check_ok_does_not_increment_guard_fires() {
        with_env(None, || {
            let before = fire_count();
            let _ = check().expect("legit first spawn");
            assert_eq!(
                fire_count(),
                before,
                "GUARD_FIRES must not increment on the legitimate Ok path"
            );
        });
    }

    /// LOAD-BEARING (#879v3 safeguard 7 auto-notify): the inbox enqueue path
    /// to the fixup-lead is what the operator/lead notifications key on. If
    /// the enqueue silently drops the message, the auto-notify-on-fire
    /// promise is broken — operator/lead would have to grep daemon.log on a
    /// cron, which is the manual workflow this safeguard exists to replace.
    #[test]
    fn enqueue_lead_notification_writes_to_inbox() {
        let home = std::env::temp_dir().join(format!(
            "agend-879v3-safeguard7-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap_or_else(|e| panic!("mkdir tmp home: {e}"));

        super::enqueue_lead_notification(&home, 2, 1)
            .expect("enqueue must succeed on writable home");

        // Inbox path is `{home}/inbox/<target>.jsonl` — read it back and
        // verify the message landed with the load-bearing fields.
        let inbox_dir = home.join("inbox");
        let read = std::fs::read_dir(&inbox_dir).unwrap_or_else(|e| {
            panic!(
                "inbox dir missing after enqueue: {} ({e})",
                inbox_dir.display()
            )
        });
        let mut found_message = false;
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            for line in body.lines() {
                let v: serde_json::Value =
                    serde_json::from_str(line).unwrap_or_else(|e| panic!("bad jsonl line: {e}"));
                if v["kind"] == NOTIFICATION_KIND {
                    found_message = true;
                    assert_eq!(v["from"], NOTIFICATION_FROM, "from identity wrong");
                    let text = v["text"].as_str().unwrap_or("");
                    assert!(
                        text.contains("depth=2"),
                        "notification text must surface the observed depth: {text:?}"
                    );
                    assert!(
                        text.contains("fire_count=1"),
                        "notification text must surface the counter value: {text:?}"
                    );
                    assert!(
                        text.contains("safeguard 7"),
                        "notification text must reference the safeguard for triage: {text:?}"
                    );
                }
            }
        }
        assert!(
            found_message,
            "no inbox message with kind={NOTIFICATION_KIND} found under {}",
            inbox_dir.display()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Spec carries `["start", "--foreground"]` for the no-fleet case.
    /// Pins the args shape every legitimate self-spawn site shares.
    #[test]
    fn canonical_spawn_args_includes_start_and_foreground() {
        with_env(None, || {
            let spec = canonical_spawn_args(None).expect("guard not tripped");
            assert_eq!(
                spec.args,
                vec!["start".to_string(), "--foreground".to_string()],
                "canonical spawn spec must always carry `start --foreground` \
                 (no --fleet when None)"
            );
        });
    }

    #[test]
    fn canonical_spawn_args_appends_fleet_path_when_provided() {
        with_env(None, || {
            let fleet = std::path::PathBuf::from("/tmp/some/fleet.yaml");
            let spec = canonical_spawn_args(Some(&fleet)).expect("guard not tripped");
            assert_eq!(
                spec.args,
                vec![
                    "start".to_string(),
                    "--foreground".to_string(),
                    "--fleet".to_string(),
                    fleet.display().to_string(),
                ],
            );
        });
    }

    #[test]
    fn canonical_spawn_args_increments_depth_env_for_child() {
        with_env(None, || {
            let spec = canonical_spawn_args(None).expect("guard not tripped");
            assert_eq!(
                spec.env,
                vec![(ENV_KEY.to_string(), "1".to_string())],
                "canonical spawn spec must set AGEND_SPAWN_DEPTH=1 on child env \
                 (test process is depth 0)"
            );
        });
    }

    #[test]
    fn canonical_spawn_args_bails_at_threshold() {
        with_env(Some("2"), || {
            let err = canonical_spawn_args(None).expect_err("must bail at threshold");
            let msg = format!("{err}");
            assert!(
                msg.contains("AGEND_SPAWN_DEPTH=2"),
                "spec build must surface the guard error (got: {msg})"
            );
        });
    }

    #[test]
    fn canonical_spawn_spec_apply_to_writes_args_and_env() {
        let spec = CanonicalSpawnSpec {
            args: vec!["start".to_string(), "--foreground".to_string()],
            env: vec![("AGEND_SPAWN_DEPTH".to_string(), "1".to_string())],
        };
        let mut cmd = std::process::Command::new("/bin/true");
        spec.apply_to(&mut cmd);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["start".to_string(), "--foreground".to_string()]);
        let depth = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("AGEND_SPAWN_DEPTH"))
            .and_then(|(_, v)| v.map(|os| os.to_string_lossy().into_owned()));
        assert_eq!(depth.as_deref(), Some("1"));
    }
}
