//! #2744 PR-A: per-backend declared model-flag grammar (anti-monolith
//! split out of `backend.rs`). See [`ModelCapability`] for the contract;
//! `Backend::model_capability` is the enum-keyed accessor.

use crate::backend::Backend;

/// #2744 PR-A: a backend's declared model-flag grammar, captured verbatim
/// from its CLI help into `tests/fixtures/cli-help/` at the calibrated
/// version. `None` (Shell/Raw/custom) means the backend has no proven model
/// semantics: the injection path skips with a warning and `set_model`
/// hard-errors instead of guessing.
#[derive(Debug, PartialEq)]
pub struct ModelCapability {
    /// Long flag exactly as the CLI help declares it.
    pub long_flag: &'static str,
    /// Short spelling — ONLY where the CLI help proves it (codex/opencode/
    /// grok declare `-m`; claude/kiro-cli/agy are long-flag-only).
    pub short_flag: Option<&'static str>,
    /// CLI version the grammar was captured at. Evidence/health metadata
    /// only — never a runtime gate (decision d-20260712101306674407-19).
    pub calibrated_version: &'static str,
}

/// Classified hit from [`ModelCapability::scan`].
///
/// `Confirmed` spellings (`--model`, `--model=X`, separate `-m` where
/// declared) are fixture-proven for the backend. `Ambiguous` covers glued
/// `-mVAL` / `-m=VAL` tokens: their parser acceptance is NOT fixture-proven
/// (clap and yargs differ), so they are treated as conservative conflicts —
/// suppressing injection / rejecting set_model beats ever risking a double
/// model flag. Disambiguation for operators: payload text belongs after a
/// bare `--`; a real model choice belongs in `set_model`, not raw args.
#[derive(Debug, PartialEq)]
pub enum ModelFlagHit {
    Confirmed(String),
    Ambiguous(String),
}

impl ModelCapability {
    /// Scan flag territory — tokens BEFORE the first bare `--` delimiter —
    /// for existing spellings of this backend's model flag. Tokens after
    /// `--` are payload and never match.
    pub fn scan(&self, args: &[String]) -> Vec<ModelFlagHit> {
        let mut hits = Vec::new();
        for tok in args {
            if tok == "--" {
                break;
            }
            if tok == self.long_flag {
                hits.push(ModelFlagHit::Confirmed(tok.clone()));
                continue;
            }
            if let Some(rest) = tok.strip_prefix(self.long_flag) {
                // `--model=X` is confirmed; `--model-foo` is a different
                // flag and must not match.
                if rest.starts_with('=') {
                    hits.push(ModelFlagHit::Confirmed(tok.clone()));
                }
                continue;
            }
            let Some(short) = self.short_flag else {
                continue;
            };
            if tok == short {
                hits.push(ModelFlagHit::Confirmed(tok.clone()));
            } else if tok.strip_prefix(short).is_some_and(|rest| !rest.is_empty()) {
                // Glued value (or `=`-glued): conservative ambiguous match.
                // Long flags never reach here: `--…` fails the `-m` prefix.
                hits.push(ModelFlagHit::Ambiguous(tok.clone()));
            }
        }
        hits
    }
}

/// #2744 PR-A: the DECLARED backend's model-flag grammar. Keyed off the
/// enum — never off a command string (`from_command` basename guessing
/// misclassifies wrappers like `claude-wrapper.sh` and must not appear
/// anywhere in the model path). Grammar pinned by
/// `tests/fixtures/cli-help/` at each `calibrated_version`.
pub(crate) fn capability_for(backend: &Backend) -> Option<&'static ModelCapability> {
    const CLAUDE: ModelCapability = ModelCapability {
        long_flag: "--model",
        short_flag: None,
        calibrated_version: "2.1.207",
    };
    const CODEX: ModelCapability = ModelCapability {
        long_flag: "--model",
        short_flag: Some("-m"),
        calibrated_version: "0.144.1",
    };
    const KIRO: ModelCapability = ModelCapability {
        long_flag: "--model",
        short_flag: None,
        calibrated_version: "2.12.1",
    };
    const OPENCODE: ModelCapability = ModelCapability {
        long_flag: "--model",
        short_flag: Some("-m"),
        calibrated_version: "1.17.5",
    };
    const AGY: ModelCapability = ModelCapability {
        long_flag: "--model",
        short_flag: None,
        calibrated_version: "1.0.15",
    };
    const GROK: ModelCapability = ModelCapability {
        long_flag: "--model",
        short_flag: Some("-m"),
        calibrated_version: "0.2.93",
    };
    match backend {
        Backend::ClaudeCode => Some(&CLAUDE),
        Backend::Codex => Some(&CODEX),
        Backend::KiroCli => Some(&KIRO),
        Backend::OpenCode => Some(&OPENCODE),
        Backend::Agy => Some(&AGY),
        Backend::Grok => Some(&GROK),
        Backend::Shell | Backend::Raw(_) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// #2744 PR-A: scan classification — Confirmed (fixture-proven
    /// spellings) vs Ambiguous (glued short, conservative), plus the
    /// false-positive pins: `--max-turns` (grok help) and `--model-foo`
    /// must never match.
    #[test]
    fn model_capability_scan_classifies_hits_2744() {
        let cap = Backend::Grok.model_capability().unwrap();
        let args: Vec<String> = ["--max-turns", "3", "--model-foo", "-m", "x", "-m=y", "-mz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            cap.scan(&args),
            vec![
                ModelFlagHit::Confirmed("-m".into()),
                ModelFlagHit::Ambiguous("-m=y".into()),
                ModelFlagHit::Ambiguous("-mz".into()),
            ]
        );

        // Long-flag-only backend: `-m` never matches at all.
        let cap = Backend::ClaudeCode.model_capability().unwrap();
        let args: Vec<String> = ["-m", "x", "-mz"].iter().map(|s| s.to_string()).collect();
        assert!(cap.scan(&args).is_empty());
    }

    /// #2744 PR-A L2: every declared ModelCapability is pinned by a verbatim
    /// help fixture captured at its calibrated version; absent short flags
    /// are pinned by ABSENCE in the fixture (kiro has no `-m` — the spike's
    /// earlier assumption, refuted by capture, must not resurface).
    #[test]
    fn model_capability_grammar_pinned_by_help_fixtures_2744() {
        let cases: Vec<(Backend, &str)> = vec![
            (Backend::ClaudeCode, "claude-2.1.207.txt"),
            (Backend::Codex, "codex-0.144.1-root.txt"),
            (Backend::Codex, "codex-0.144.1-resume.txt"),
            (Backend::KiroCli, "kiro-cli-2.12.1-chat.txt"),
            (Backend::OpenCode, "opencode-1.17.5.txt"),
            (Backend::Agy, "agy-1.0.15.txt"),
            (Backend::Grok, "grok-0.2.93.txt"),
        ];
        for (backend, fixture) in cases {
            let cap = backend
                .model_capability()
                .unwrap_or_else(|| panic!("{backend:?} must declare a capability"));
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
            assert!(
                text.contains(cap.long_flag),
                "{fixture}: help must declare {}",
                cap.long_flag
            );
            match cap.short_flag {
                Some(short) => assert!(
                    text.contains(&format!("{short}, {}", cap.long_flag)),
                    "{fixture}: short flag {short} must be help-declared"
                ),
                None => assert!(
                    !text.contains(&format!("-m, {}", cap.long_flag)),
                    "{fixture}: claims long-flag-only but help declares -m"
                ),
            }
        }
        assert!(Backend::Shell.model_capability().is_none());
        assert!(Backend::Raw("/opt/x".into()).model_capability().is_none());
    }
}
