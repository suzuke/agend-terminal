//! #3541: per-backend declared reasoning-effort grammar (mirrors the
//! #2744 `backend_model` split for `--model`). See [`EffortCapability`] for
//! the contract; `Backend::effort_capability` (in `backend.rs`) is the
//! enum-keyed accessor.
//!
//! Grammar pinned by `tests/fixtures/cli-help/*-effort.txt` at each
//! `calibrated_version`. `None` (KiroCli/OpenCode/Grok/Shell/Raw) means the
//! backend has no proven effort semantics: injection skips with a warning
//! and `set_model` records a warning instead of guessing.

use crate::backend::Backend;

/// #3541: how a backend accepts a reasoning-effort value on its CLI.
#[derive(Debug, PartialEq)]
pub enum EffortInjectionKind {
    /// Standard CLI flag pair: `--effort <value>` (Claude, Agy).
    CliFlag,
    /// Codex config override: `-c` `model_reasoning_effort="<value>"`.
    CodexConfig,
}

/// #3541: a backend's declared effort grammar, captured from its CLI help
/// (or live config key, for Codex) into `tests/fixtures/cli-help/` at the
/// calibrated version. `calibrated_version` is evidence/health metadata
/// only — never a runtime gate (same rule as #2744 decision
/// d-20260712101306674407-19).
#[derive(Debug, PartialEq)]
pub struct EffortCapability {
    pub injection: EffortInjectionKind,
    pub allowed_values: &'static [&'static str],
    pub calibrated_version: &'static str,
}

/// #3541: union of every backend's `allowed_values` — the `set_model` MCP
/// accepts any of these (fail-soft at spawn: `push_effort_arg` drops a value
/// the DECLARED backend does not allow with a warning, so a fleet-wide
/// `defaults.effort` never crashes a narrower backend).
pub const GLOBAL_EFFORT_VALUES: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// #3541: the DECLARED backend's effort grammar. Keyed off the enum —
/// never off a command string. Grammar pinned by
/// `tests/fixtures/cli-help/` at each `calibrated_version`.
pub(crate) fn capability_for(backend: &Backend) -> Option<&'static EffortCapability> {
    const CLAUDE: EffortCapability = EffortCapability {
        injection: EffortInjectionKind::CliFlag,
        allowed_values: &["low", "medium", "high", "xhigh", "max"],
        calibrated_version: "2.1.263",
    };
    const AGY: EffortCapability = EffortCapability {
        injection: EffortInjectionKind::CliFlag,
        allowed_values: &["low", "medium", "high"],
        calibrated_version: "1.1.12",
    };
    const CODEX: EffortCapability = EffortCapability {
        injection: EffortInjectionKind::CodexConfig,
        allowed_values: &["low", "medium", "high"],
        calibrated_version: "0.153.4",
    };
    match backend {
        Backend::ClaudeCode => Some(&CLAUDE),
        Backend::Agy => Some(&AGY),
        Backend::Codex => Some(&CODEX),
        Backend::KiroCli | Backend::OpenCode | Backend::Grok | Backend::Shell | Backend::Raw(_) => {
            None
        }
    }
}

/// The config key Codex uses for reasoning effort (overridden via `-c`).
pub const CODEX_EFFORT_KEY: &str = "model_reasoning_effort";

/// #3543 R1 B1 / #3557 R2 B1: the value glued onto a Codex config flag.
/// Returns `None` for the separate-value spellings (`-c` / `--config`
/// alone), which the next-token branch owns, and for any other token.
///
/// codex 0.153.2 accepts FOUR spellings of the same option, verified
/// against the real binary: `-c KEY=V`, `-cKEY=V`, `-c=KEY=V` and
/// `--config=KEY=V` all exit 0, while a bare `-c` exits 2. The first is
/// the next-token branch; the other three land here, so the optional `=`
/// separator has to be stripped too — the first cut of this helper appealed
/// to that equivalence class and then missed `-c=KEY=V`, leaving a
/// hand-written setting invisible to the scan.
fn glued_codex_config_value(tok: &str) -> Option<&str> {
    let rest = match tok.strip_prefix("--config=") {
        Some(rest) => rest,
        None => tok.strip_prefix("-c").filter(|rest| !rest.is_empty())?,
    };
    Some(rest.strip_prefix('=').unwrap_or(rest))
}

/// #3557 N6: does `rest` assign the reasoning-effort key? Compares the KEY
/// — everything before the first `=` — exactly, so a different key that
/// merely shares the prefix (`model_reasoning_effort_summary`) is not
/// mistaken for one. Whitespace around the key is tolerated because that is
/// how TOML assignments are normally written (`key = "value"`, the shape
/// `provider_detect` itself emits); a key with no `=` at all still counts,
/// which keeps the pre-existing conservative reading of a bare key.
fn is_effort_assignment(rest: &str) -> bool {
    let key = rest.split_once('=').map_or(rest, |(key, _)| key);
    key.trim() == CODEX_EFFORT_KEY
}

/// Long flag backends with [`EffortInjectionKind::CliFlag`] use.
pub const EFFORT_LONG_FLAG: &str = "--effort";

/// #3541: scan flag territory — tokens BEFORE the first bare `--`
/// delimiter — for an existing hand-written effort setting for this backend.
/// Returns the conflicting token when found. Tokens after `--` are payload
/// and never match.
///
/// - `CliFlag` backends: `--effort` (separate value) or `--effort=<val>`
///   (glued value). `--effort-foo` is a different flag and must not match.
/// - `CodexConfig`: a `-c` / `--config` token whose NEXT token assigns
///   `model_reasoning_effort`, OR any of the glued spellings codex accepts
///   for the same option (`-c<val>`, `-c=<val>`, `--config=<val>`) whose
///   value assigns it. Both arms compare the key EXACTLY (see
///   [`is_effort_assignment`]), so `model_reasoning_effort_summary` — a
///   different setting that shares the prefix — is not a conflict. A lone
///   `-c` at end of argv with no next token is not a conflict either.
pub fn scan_effort_conflict(backend: &Backend, args: &[String]) -> Option<String> {
    let cap = capability_for(backend)?;
    match cap.injection {
        EffortInjectionKind::CliFlag => {
            for tok in args {
                if tok == "--" {
                    break;
                }
                if tok == EFFORT_LONG_FLAG {
                    return Some(tok.clone());
                }
                if let Some(rest) = tok.strip_prefix(EFFORT_LONG_FLAG) {
                    // `--effort=X` is a conflict; `--effort-foo` is a
                    // different flag and must not match.
                    if rest.starts_with('=') {
                        return Some(tok.clone());
                    }
                }
            }
            None
        }
        EffortInjectionKind::CodexConfig => {
            let mut index = 0;
            while let Some(tok) = args.get(index) {
                if tok == "--" {
                    break;
                }
                if tok == "-c" || tok == "--config" {
                    if let Some(next) = args.get(index + 1) {
                        if is_effort_assignment(next) {
                            return Some(next.clone());
                        }
                    }
                } else if let Some(rest) = glued_codex_config_value(tok) {
                    // Same setting, glued: report the whole token, since that
                    // is what the operator would have to remove.
                    if is_effort_assignment(rest) {
                        return Some(tok.clone());
                    }
                }
                index += 1;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// #3541: capability matrix — Claude/Agy/Codex declare, the rest None.
    #[test]
    fn effort_capability_matrix_3541() {
        assert!(Backend::ClaudeCode.effort_capability().is_some());
        assert!(Backend::Agy.effort_capability().is_some());
        assert!(Backend::Codex.effort_capability().is_some());
        for backend in [
            Backend::KiroCli,
            Backend::OpenCode,
            Backend::Grok,
            Backend::Shell,
            Backend::Raw("/opt/custom/bin".into()),
        ] {
            assert!(
                backend.effort_capability().is_none(),
                "{backend:?} must not declare an effort capability"
            );
        }
    }

    /// #3541: every declared EffortCapability is pinned by a verbatim help
    /// fixture captured at its calibrated version (mirrors the #2744
    /// `model_capability_grammar_pinned_by_help_fixtures_2744` gate).
    #[test]
    fn effort_capability_grammar_pinned_by_help_fixtures_3541() {
        let cases: Vec<(Backend, &str)> = vec![
            (Backend::ClaudeCode, "claude-2.1.263-effort.txt"),
            (Backend::Agy, "agy-1.1.12-effort.txt"),
            (Backend::Codex, "codex-0.153.4-effort.txt"),
        ];
        for (backend, fixture) in cases {
            let cap = backend
                .effort_capability()
                .unwrap_or_else(|| panic!("{backend:?} must declare an effort capability"));
            let path = format!(
                "{}/tests/fixtures/cli-help/{fixture}",
                env!("CARGO_MANIFEST_DIR")
            );
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"));
            assert!(
                text.contains("# Provenance:"),
                "{fixture}: fixture must carry a provenance header"
            );
            assert!(
                fixture.contains(cap.calibrated_version),
                "{fixture}: filename must carry calibrated version {}",
                cap.calibrated_version
            );
            match cap.injection {
                EffortInjectionKind::CliFlag => assert!(
                    text.contains(EFFORT_LONG_FLAG),
                    "{fixture}: help must declare {EFFORT_LONG_FLAG}"
                ),
                EffortInjectionKind::CodexConfig => {
                    assert!(
                        text.contains("-c, --config"),
                        "{fixture}: help must declare the -c/--config mechanism"
                    );
                    assert!(
                        text.contains(CODEX_EFFORT_KEY),
                        "{fixture}: fixture must carry the {CODEX_EFFORT_KEY} key evidence"
                    );
                }
            }
            for v in cap.allowed_values {
                assert!(
                    text.contains(v),
                    "{fixture}: allowed value {v} must appear in the fixture"
                );
            }
        }
    }

    /// #3541: CliFlag scan — separate + glued spellings hit; `--`-payload
    /// and `--effort-foo` prefixes never match.
    #[test]
    fn effort_scan_cliflag_classifies_hits_3541() {
        let backend = Backend::ClaudeCode;
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["--effort", "high"])),
            Some("--effort".to_string())
        );
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["--effort=high"])),
            Some("--effort=high".to_string())
        );
        // Payload after `--` is never flag territory.
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["--", "--effort", "high"])),
            None
        );
        // `--effort-foo` is a different flag.
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["--effort-foo", "x"])),
            None
        );
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["--model", "opus"])),
            None
        );
    }

    /// #3541: Codex scan — `-c`/`--config` whose next token sets the effort
    /// key hits; unrelated `-c` pairs and payload never match.
    #[test]
    fn effort_scan_codex_config_pairs_3541() {
        let backend = Backend::Codex;
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["-c", "model_reasoning_effort=\"high\""])),
            Some("model_reasoning_effort=\"high\"".to_string())
        );
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["--config", "model_reasoning_effort=low"])),
            Some("model_reasoning_effort=low".to_string())
        );
        // TOML's normal spacing — the shape `provider_detect` writes — is
        // the same assignment. Nothing pinned this before #3557.
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["-c", "model_reasoning_effort = \"low\""])),
            Some("model_reasoning_effort = \"low\"".to_string())
        );
        // Unrelated -c pair: no conflict.
        assert_eq!(
            scan_effort_conflict(&backend, &args(&["-c", "model=\"o3\""])),
            None
        );
        // Lone trailing -c: no next token, no conflict.
        assert_eq!(scan_effort_conflict(&backend, &args(&["-c"])), None);
        // Payload after `--` never matches.
        assert_eq!(
            scan_effort_conflict(
                &backend,
                &args(&["--", "-c", "model_reasoning_effort=\"high\""])
            ),
            None
        );
    }

    /// #3541: backends without a capability never report conflicts.
    #[test]
    fn effort_scan_unsupported_backend_never_conflicts_3541() {
        for backend in [Backend::KiroCli, Backend::Shell] {
            assert_eq!(
                scan_effort_conflict(&backend, &args(&["--effort", "high"])),
                None,
                "{backend:?} has no capability so nothing can conflict"
            );
        }
    }

    /// #3541: GLOBAL_EFFORT_VALUES is exactly the union of per-backend
    /// allowed values (no value accepted by set_model is unknown, and no
    /// known value is rejected at the MCP gate).
    #[test]
    fn global_effort_values_cover_all_backends_3541() {
        for backend in [Backend::ClaudeCode, Backend::Agy, Backend::Codex] {
            let cap = backend.effort_capability().unwrap();
            for v in cap.allowed_values {
                assert!(
                    GLOBAL_EFFORT_VALUES.contains(v),
                    "backend {backend:?} allows {v} but GLOBAL_EFFORT_VALUES lacks it"
                );
            }
        }
        assert_eq!(GLOBAL_EFFORT_VALUES.len(), 5);
    }
}
