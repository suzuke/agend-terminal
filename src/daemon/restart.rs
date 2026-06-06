//! #851 restart-supervisor detection.
//!
//! `restart_daemon` MCP triggers a graceful shutdown + `exit(42)`. The
//! exit code is meaningful ONLY when something supervises the daemon
//! process and respawns on exit (launchd's `KeepAlive`, systemd's
//! `Restart=on-failure`, Windows Task Scheduler, or the bash wrapper
//! script at `scripts/agend-wrapper.sh`). When the daemon was started
//! bare (`agend-terminal start` from a shell, no supervisor in the
//! parent chain), `exit(42)` just kills the process — operators then
//! see `Resource temporarily unavailable (os error 35)` on every
//! subsequent MCP call until they manually restart.
//!
//! Today's incident (2026-05-16): general assumed `restart_daemon`
//! succeeded post-#849 ship; all MCP calls hung for ~5 min until
//! operator manually intervened. The handler returned `ok: true` and
//! the daemon DID exit cleanly with 42, but nothing respawned.
//!
//! Fix: detect-and-fail-closed. The handler queries
//! [`is_restart_supervised`] before setting `RESTART_PENDING`. If no
//! supervisor signal is found, the handler returns
//! `{ok: false, error: "..."}` with an actionable hint and the daemon
//! stays up.
//!
//! ## Detection signals
//!
//! Composite env-var check. A supervisor signal means SOME process in
//! the parent chain will respawn the daemon after `exit(42)`:
//!
//! - `AGEND_WRAPPED=1` — set by `scripts/agend-wrapper.sh` before each
//!   daemon invocation. Marker for the manual / dev-mode supervisor.
//! - `AGEND_SUPERVISED=1` ([`SUPERVISED_ENV`]) — explicit positive
//!   sentinel written into the service-manager config by
//!   `agend-terminal service install` (launchd plist
//!   `EnvironmentVariables`, systemd unit `Environment=`). This is the
//!   PRIMARY signal we control end-to-end: its presence proves the
//!   daemon was launched by an agend-installed supervisor (launchd
//!   `KeepAlive` / systemd `Restart=on-failure`).
//! - `INVOCATION_ID` — set by systemd for every unit instance. Kept as
//!   a belt-and-suspenders systemd-only indicator; it is never set
//!   outside a systemd-spawned unit, so (unlike `XPC_SERVICE_NAME`
//!   below) it is not ambient and stays a reliable positive.
//!
//! ### Why `XPC_SERVICE_NAME` was removed (#1812)
//!
//! Earlier revisions accepted `XPC_SERVICE_NAME` as a launchd signal.
//! That was WRONG: macOS exports `XPC_SERVICE_NAME` (commonly the value
//! `0`) into EVERY process spawned inside a GUI login session —
//! including a bare `agend-terminal start` from Terminal.app. So in a
//! macOS GUI session the check returned true UNCONDITIONALLY, defeating
//! the #851 fail-closed guard: a bare, UNsupervised daemon was treated
//! as supervised, so `restart_daemon` exited with nobody to respawn it.
//! The positive `AGEND_SUPERVISED` sentinel replaces it — launchd is now
//! detected via the sentinel the install-time plist carries, not via an
//! ambient OS variable.
//!
//! Windows Task Scheduler does not set a comparably reliable env var,
//! and its task XML has no native environment-variable element, so the
//! sentinel cannot be injected there. Windows therefore stays
//! fail-closed on bare-start (the correct behavior — same actionable
//! error as macOS/Linux). FUTURE: a Windows supervisor path can carry
//! the signal as a `--supervised` launch arg in the task `<Arguments>`
//! and have startup translate it into `AGEND_SUPERVISED` (deferred —
//! out of #1812 scope).

/// Env var name of the explicit supervisor sentinel (#1812). Written
/// into the launchd plist `EnvironmentVariables` and the systemd unit
/// `Environment=` by `agend-terminal service install`; shared with the
/// detector and its tests so the literal lives in exactly one place.
pub const SUPERVISED_ENV: &str = "AGEND_SUPERVISED";

/// True iff the daemon's parent process chain includes a supervisor
/// that will respawn the daemon on `exit(42)`. Returns false (fail-
/// closed) when no supervisor is detected — `restart_daemon` should
/// then refuse the request rather than silently killing the daemon.
///
/// Composite env-var check (see module doc for each signal + why
/// `XPC_SERVICE_NAME` was dropped in #1812). Pure helper — no globals,
/// no side effects, safe to call from any thread.
pub fn is_restart_supervised() -> bool {
    has_env("AGEND_WRAPPED") || has_env(SUPERVISED_ENV) || has_env("INVOCATION_ID")
}

/// Returns true iff env var `name` is set (any value, including
/// empty string — presence is the signal). Distinct from
/// `std::env::var(name).is_ok()` because we don't care about UTF-8
/// validity for the indicator role.
fn has_env(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

// ── #1814 Stage 1: self-respawn (flag-gated successor handoff) ──────────
//
// When `AGEND_RESTART_HANDOFF=1`, `restart_daemon` no longer relies on an
// external supervisor + `exit(42)`. Instead the running daemon spawns its
// OWN successor, confirms it is healthy (Phase-1 gate), and only then exits
// (0). If the successor fails the gate, the old daemon never shuts down — it
// stays alive with its agents intact (abort-stay-alive). This retires the
// supervision-detection brick class (#851/#1812) at the root: restart no
// longer hinges on a static env-var guess about whether something will
// respawn the process. See decision d-20260606104934346030-2.
//
// Flag OFF (default) → behaviour is byte-identical to the pre-#1814
// `is_restart_supervised()` + `exit(42)` path. Stage 1 is opt-in.

/// Env flag enabling #1814 self-respawn. `"1"` activates; anything else (or
/// unset) keeps the legacy supervisor + `exit(42)` path.
pub const RESTART_HANDOFF_ENV: &str = "AGEND_RESTART_HANDOFF";

/// Env marker the old daemon sets on the successor it spawns:
/// `AGEND_SUCCESSOR_HANDOFF=<old_pid>:<token>`. Its presence (well-formed)
/// is the ONLY signal that routes a boot through the minimal pre-lock handoff
/// path; a normal / operator `agend-terminal start` never carries it and so
/// always runs the full `bootstrap::prepare` (including the destructive
/// reconciles that MUST NOT run in a two-daemon overlap window).
pub const SUCCESSOR_HANDOFF_ENV: &str = "AGEND_SUCCESSOR_HANDOFF";

/// True iff `AGEND_RESTART_HANDOFF=1`. Read at restart time (handler) and at
/// daemon exit (to choose `exit(0)` vs `exit(42)`).
pub fn self_respawn_enabled() -> bool {
    std::env::var(RESTART_HANDOFF_ENV).as_deref() == Ok("1")
}

/// Parse a legitimate successor-handoff marker into `(old_pid, token)`.
/// Returns `None` when the env is absent or malformed — the boot then takes
/// the normal full-prepare path. "Legitimate" = `<u32 pid>:<non-empty token>`
/// (the token guard the lead required so a stray/normal start can never skip
/// the destructive reconciles).
pub fn successor_handoff_marker() -> Option<(u32, String)> {
    let raw = std::env::var(SUCCESSOR_HANDOFF_ENV).ok()?;
    let (pid, token) = raw.split_once(':')?;
    let pid: u32 = pid.parse().ok()?;
    if token.is_empty() {
        return None;
    }
    Some((pid, token.to_string()))
}

/// Build the `AGEND_SUCCESSOR_HANDOFF` env VALUE (`<old_pid>:<token>`) for a
/// fresh successor spawn. The token is derived from the old pid + a monotonic
/// nanos stamp so each handoff is distinguishable in logs; it carries no
/// security role (the loopback api + cookie do).
pub fn make_handoff_value(old_pid: u32) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{old_pid}:{old_pid}-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests touch process-global env state — serialise via the SINGLE
    /// crate-wide [`crate::daemon::test_env_lock`] (NOT a module-local
    /// mutex): env mutation races across all keys, so per-module locks
    /// don't serialise against other modules' env tests (#1812).
    fn with_env<R>(set: &[(&str, &str)], unset: &[&str], f: impl FnOnce() -> R) -> R {
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let all_keys: Vec<&str> = set
            .iter()
            .map(|(k, _)| *k)
            .chain(unset.iter().copied())
            .collect();
        let prior: Vec<(String, Option<String>)> = all_keys
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();

        // SAFETY: test-only mutation, serialised via the mutex above.
        // Rust 1.84+ requires unsafe for env mutations; older toolchains
        // treat the unsafe block as a no-op.
        unsafe {
            for k in unset {
                std::env::remove_var(k);
            }
            for (k, v) in set {
                std::env::set_var(k, v);
            }
        }

        let result = f();

        unsafe {
            for (k, v) in &prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }

        result
    }

    /// Every supervisor signal the detector accepts AFTER #1812 (XPC is
    /// no longer one of them). Used as the unset list so a single-signal
    /// test isolates exactly one positive.
    const ALL_SIGNAL_VARS: &[&str] = &["AGEND_WRAPPED", "AGEND_SUPERVISED", "INVOCATION_ID"];

    /// The ambient macOS-GUI variable #1812 deliberately stopped trusting.
    /// Kept in unset lists so a positive-signal test proves it's the OUR
    /// marker (not stray XPC) driving the true result.
    const AMBIENT_NON_SIGNAL: &str = "XPC_SERVICE_NAME";

    /// Bare `agend-terminal start` — no supervisor in parent chain,
    /// none of the signal env vars set. Detection must fail-closed.
    #[test]
    fn detect_returns_false_when_no_supervisor_env_set() {
        let unset: Vec<&str> = ALL_SIGNAL_VARS
            .iter()
            .copied()
            .chain([AMBIENT_NON_SIGNAL])
            .collect();
        with_env(&[], &unset, || {
            assert!(
                !is_restart_supervised(),
                "bare daemon invocation (no signal env) must fail-closed"
            );
        });
    }

    /// `scripts/agend-wrapper.sh` sets `AGEND_WRAPPED=1` before each
    /// daemon invocation. Detection must recognize the explicit marker.
    #[test]
    fn detect_returns_true_when_agend_wrapped_set() {
        with_env(
            &[("AGEND_WRAPPED", "1")],
            &[SUPERVISED_ENV, "INVOCATION_ID", AMBIENT_NON_SIGNAL],
            || {
                assert!(
                    is_restart_supervised(),
                    "AGEND_WRAPPED=1 must be recognized as a supervisor signal"
                );
            },
        );
    }

    /// #1812: the `AGEND_SUPERVISED` sentinel that `service install` writes
    /// into the launchd plist / systemd unit is the PRIMARY launchd/systemd
    /// signal now (replacing the ambient XPC false-positive).
    #[test]
    fn detect_returns_true_when_agend_supervised_sentinel_set() {
        with_env(
            &[(SUPERVISED_ENV, "1")],
            &["AGEND_WRAPPED", "INVOCATION_ID", AMBIENT_NON_SIGNAL],
            || {
                assert!(
                    is_restart_supervised(),
                    "AGEND_SUPERVISED=1 sentinel must be recognized as a supervisor signal"
                );
            },
        );
    }

    /// #1812 regression: `XPC_SERVICE_NAME` is ambient in a macOS GUI login
    /// session (set on a bare `agend-terminal start` too). It must NO LONGER
    /// be trusted — otherwise the #851 fail-closed guard is defeated on every
    /// macOS GUI launch. Only XPC set, all OUR markers unset → fail-closed.
    #[test]
    fn detect_returns_false_when_only_ambient_xpc_set() {
        with_env(&[(AMBIENT_NON_SIGNAL, "0")], ALL_SIGNAL_VARS, || {
            assert!(
                !is_restart_supervised(),
                "ambient XPC_SERVICE_NAME alone must NOT be treated as a supervisor \
                 signal (#1812: it is set on bare GUI launches → #851 false-positive)"
            );
        });
    }

    /// Linux systemd sets `INVOCATION_ID` for every unit instance.
    /// Used by the `service install` systemd user unit that ships
    /// `Restart=on-failure`.
    #[test]
    fn detect_returns_true_when_systemd_invocation_id_set() {
        with_env(
            &[("INVOCATION_ID", "abc123def456")],
            &["AGEND_WRAPPED", SUPERVISED_ENV, AMBIENT_NON_SIGNAL],
            || {
                assert!(
                    is_restart_supervised(),
                    "systemd INVOCATION_ID must be recognized as a supervisor signal"
                );
            },
        );
    }

    // ── #1812 §3.9 real-entry tests ──────────────────────────────────────
    //
    // These live here (not in `tests/`) ON PURPOSE: `crate::service` and
    // `crate::daemon::restart` are BIN-only modules (declared in main.rs;
    // lib.rs is a thin facade), so an integration test in `tests/` — which
    // links only the lib — cannot reach either the template constants or the
    // detector. The "true entry" requirement is met by exercising the REAL
    // `service::apply_substitutions` over the REAL shipped templates, parsing
    // the rendered output with a REAL format parser (plist / INI — no
    // `.contains()` string hacks), applying the parsed env into the process,
    // and asserting the REAL `is_restart_supervised()`.

    /// Apply an owned set of env vars (+ explicit unsets) under the shared
    /// env mutex, run `f`, then restore. Owned-string sibling of `with_env`
    /// for values produced at runtime (parsed out of a rendered template).
    fn with_env_owned<R>(set: &[(String, String)], unset: &[&str], f: impl FnOnce() -> R) -> R {
        let borrowed: Vec<(&str, &str)> =
            set.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        with_env(&borrowed, unset, f)
    }

    /// Parse the launchd `EnvironmentVariables` dict out of a rendered plist
    /// using the real `plist` parser and return its key/value pairs.
    fn launchd_env_vars(rendered: &str) -> Vec<(String, String)> {
        let value = plist::Value::from_reader_xml(std::io::Cursor::new(rendered.as_bytes()))
            .expect("rendered launchd template must be valid plist XML");
        let dict = value.as_dictionary().expect("plist root must be a <dict>");
        let env = dict
            .get("EnvironmentVariables")
            .expect("plist must carry an EnvironmentVariables key")
            .as_dictionary()
            .expect("EnvironmentVariables must be a <dict>");
        env.iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.as_string()
                        .expect("each EnvironmentVariables value is a <string>")
                        .to_string(),
                )
            })
            .collect()
    }

    /// Parse the systemd `[Service] Environment=` directives out of a
    /// rendered unit using the real `ini` parser. systemd allows repeated
    /// `Environment=` lines, so `get_all` is used; each value is a single
    /// `KEY=VALUE` assignment.
    fn systemd_env_vars(rendered: &str) -> Vec<(String, String)> {
        let ini = ini::Ini::load_from_str(rendered)
            .expect("rendered systemd template must be valid unit/INI syntax");
        let service = ini
            .section(Some("Service"))
            .expect("systemd unit must have a [Service] section");
        service
            .get_all("Environment")
            .map(|assignment| {
                let (k, v) = assignment
                    .split_once('=')
                    .expect("each Environment= directive is KEY=VALUE");
                (k.to_string(), v.to_string())
            })
            .collect()
    }

    /// Render the launchd template exactly as `macos::install` does, parse
    /// its env block with the real plist parser, confirm the sentinel is
    /// structurally present, apply the parsed env, and assert the REAL
    /// detector returns true.
    #[test]
    fn launchd_template_env_makes_detector_supervised() {
        let rendered = crate::service::apply_substitutions(
            crate::service::LAUNCHD_TEMPLATE,
            &[
                ("__LABEL__", "com.agend-terminal.daemon"),
                ("__EXECUTABLE__", "/usr/local/bin/agend-terminal"),
                ("__HOME__", "/Users/test/.agend-terminal"),
            ],
        );
        let env = launchd_env_vars(&rendered);
        assert!(
            env.iter().any(|(k, v)| k == SUPERVISED_ENV && v == "1"),
            "launchd plist EnvironmentVariables must carry {SUPERVISED_ENV}=1 — got {env:?}"
        );
        with_env_owned(
            &env,
            &["AGEND_WRAPPED", "INVOCATION_ID", AMBIENT_NON_SIGNAL],
            || {
                assert!(
                    is_restart_supervised(),
                    "applying the launchd template env must make the daemon detect as supervised"
                );
            },
        );
    }

    /// Same end-to-end check for the systemd unit: real INI parse of the
    /// `[Service] Environment=` block → apply → real detector true.
    #[test]
    fn systemd_template_env_makes_detector_supervised() {
        let rendered = crate::service::apply_substitutions(
            crate::service::SYSTEMD_TEMPLATE,
            &[
                ("__EXECUTABLE__", "/usr/local/bin/agend-terminal"),
                ("__HOME__", "/home/test/.agend-terminal"),
            ],
        );
        let env = systemd_env_vars(&rendered);
        assert!(
            env.iter().any(|(k, v)| k == SUPERVISED_ENV && v == "1"),
            "systemd [Service] must carry an Environment={SUPERVISED_ENV}=1 directive — got {env:?}"
        );
        // INVOCATION_ID is injected by systemd at RUNTIME, not by the unit
        // file — unset it here so the assertion proves the sentinel alone
        // (the part the unit file controls) drives the result.
        with_env_owned(
            &env,
            &["AGEND_WRAPPED", "INVOCATION_ID", AMBIENT_NON_SIGNAL],
            || {
                assert!(
                    is_restart_supervised(),
                    "applying the systemd unit env must make the daemon detect as supervised"
                );
            },
        );
    }

    /// Class-closing invariant: EVERY supervisor artifact we ship must yield
    /// a signal the detector accepts. Table-driven so a future artifact added
    /// without a matching detector signal fails here. Each row supplies the
    /// single env var that artifact contributes to the daemon's environment.
    #[test]
    fn every_shipped_supervisor_artifact_makes_detector_supervised() {
        // (artifact name, (env_key, env_value) it provides).
        let launchd_env = launchd_env_vars(&crate::service::apply_substitutions(
            crate::service::LAUNCHD_TEMPLATE,
            &[
                ("__LABEL__", "com.agend-terminal.daemon"),
                ("__EXECUTABLE__", "/usr/local/bin/agend-terminal"),
                ("__HOME__", "/Users/test/.agend-terminal"),
            ],
        ));
        let systemd_env = systemd_env_vars(&crate::service::apply_substitutions(
            crate::service::SYSTEMD_TEMPLATE,
            &[
                ("__EXECUTABLE__", "/usr/local/bin/agend-terminal"),
                ("__HOME__", "/home/test/.agend-terminal"),
            ],
        ));
        let launchd_sentinel = launchd_env
            .iter()
            .find(|(k, _)| k == SUPERVISED_ENV)
            .cloned()
            .expect("launchd plist provides the sentinel");
        let systemd_sentinel = systemd_env
            .iter()
            .find(|(k, _)| k == SUPERVISED_ENV)
            .cloned()
            .expect("systemd unit provides the sentinel");

        let artifacts: &[(&str, (String, String))] = &[
            ("launchd plist (service install)", launchd_sentinel),
            ("systemd user unit (service install)", systemd_sentinel),
            (
                "systemd runtime (INVOCATION_ID)",
                ("INVOCATION_ID".to_string(), "abc123".to_string()),
            ),
            (
                "scripts/agend-wrapper.sh (AGEND_WRAPPED)",
                ("AGEND_WRAPPED".to_string(), "1".to_string()),
            ),
        ];

        for (artifact, (key, value)) in artifacts {
            // Unset every OTHER signal so each row proves THAT artifact alone
            // closes the class. AMBIENT_NON_SIGNAL is always unset.
            let unset: Vec<&str> = ALL_SIGNAL_VARS
                .iter()
                .copied()
                .chain(["INVOCATION_ID", AMBIENT_NON_SIGNAL])
                .filter(|k| k != key)
                .collect();
            with_env_owned(
                std::slice::from_ref(&(key.clone(), value.clone())),
                &unset,
                || {
                    assert!(
                        is_restart_supervised(),
                        "shipped supervisor artifact {artifact:?} (provides {key}={value}) must \
                     make is_restart_supervised() return true — the brick class is only closed \
                     if every artifact we ship yields an accepted signal"
                    );
                },
            );
        }
    }

    // ── #1814 self-respawn helpers ──────────────────────────────────────

    #[test]
    fn self_respawn_enabled_only_for_exactly_one() {
        with_env(&[(RESTART_HANDOFF_ENV, "1")], &[], || {
            assert!(self_respawn_enabled(), "AGEND_RESTART_HANDOFF=1 enables");
        });
        with_env(&[(RESTART_HANDOFF_ENV, "0")], &[], || {
            assert!(!self_respawn_enabled(), "0 does not enable");
        });
        with_env(&[(RESTART_HANDOFF_ENV, "true")], &[], || {
            assert!(!self_respawn_enabled(), "only the literal \"1\" enables");
        });
        with_env(&[], &[RESTART_HANDOFF_ENV], || {
            assert!(!self_respawn_enabled(), "unset = off (flag-off default)");
        });
    }

    #[test]
    fn handoff_marker_parses_only_well_formed_values() {
        // Well-formed → Some. This is the guard: only a legitimate marker
        // routes a boot through the minimal pre-lock handoff path.
        with_env(&[(SUCCESSOR_HANDOFF_ENV, "4321:4321-99")], &[], || {
            assert_eq!(
                successor_handoff_marker(),
                Some((4321, "4321-99".to_string()))
            );
        });
        // Absent → None (normal/operator start never skips reconciles).
        with_env(&[], &[SUCCESSOR_HANDOFF_ENV], || {
            assert_eq!(successor_handoff_marker(), None);
        });
        // Malformed → None (no colon / bad pid / empty token).
        for bad in ["nopid", "abc:tok", "4321:", ":tok", ""] {
            with_env(&[(SUCCESSOR_HANDOFF_ENV, bad)], &[], || {
                assert_eq!(
                    successor_handoff_marker(),
                    None,
                    "malformed {bad:?} must be rejected"
                );
            });
        }
    }

    #[test]
    fn make_handoff_value_round_trips_through_marker_parser() {
        // The value `make_handoff_value` produces must be accepted by the
        // parser the successor uses — pins the producer/consumer contract.
        with_env(&[], &[], || {
            let value = make_handoff_value(777);
            let parsed = {
                // SAFETY: serialised by with_env's lock; restored on exit.
                unsafe { std::env::set_var(SUCCESSOR_HANDOFF_ENV, &value) };
                let m = successor_handoff_marker();
                unsafe { std::env::remove_var(SUCCESSOR_HANDOFF_ENV) };
                m
            };
            let (pid, token) = parsed.expect("self-produced value must parse");
            assert_eq!(pid, 777, "old pid prefix preserved");
            assert!(!token.is_empty(), "token non-empty");
        });
    }
}
