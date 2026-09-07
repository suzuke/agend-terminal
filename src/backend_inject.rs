//! #3541: spawn-argv injection chokepoints — split out of `backend.rs`
//! to keep it under the `src_file_size_invariant` 2500-LOC anti-monolith
//! ceiling. `Backend::push_model_arg` / `push_effort_arg` /
//! `format_model_arg` in `backend.rs` are thin wrappers over these so every
//! existing call site (`agent_ops/spawn`, `agent_resolve`, `pane_factory`,
//! `instance_state/spawn`, `team_ops`) keeps working unchanged.

use crate::backend::{Backend, EffortInjectionKind, ModelFlagHit};

/// Format a `--model` value for this backend.
/// OpenCode requires `provider/model` format — auto-prefixes `anthropic/`
/// if the value doesn't already contain a `/`.
pub fn format_model_arg(backend: &Backend, model: &str) -> String {
    if matches!(backend, Backend::OpenCode) && !model.contains('/') {
        format!("anthropic/{model}")
    } else {
        model.to_string()
    }
}

/// #2038/#2744: apply the fleet-resolved model intent to a spawn argv,
/// gated on the DECLARED backend's [`ModelCapability`].
///
/// - No capability (Shell/Raw/custom) → warn + skip: `bash --model X`
///   breaks the spawn outright, so unsupported backends fail loud here
///   (and hard-error in `set_model`) instead of guessing.
/// - Existing model-flag spellings win (caller args > fleet intent,
///   #2038). Confirmed spellings skip silently — that precedence is by
///   design. Ambiguous glued spellings also skip, but WITH a warning:
///   fleet intent is being suppressed by a token whose parser
///   acceptance is unproven.
/// - The flag pair is inserted BEFORE the first bare `--` — everything
///   after the delimiter is payload, not flag territory. Presets never
///   carry `--`, so production argv is unchanged (append position).
/// - Formatting goes through [`format_model_arg`] (OpenCode
///   needs a provider prefix). Empty model is a no-op.
pub fn push_model_arg(args: &mut Vec<String>, backend: &Backend, model: &str) {
    if model.is_empty() {
        return;
    }
    let Some(cap) = backend.model_capability() else {
        tracing::warn!(
            backend = %backend.name(),
            model = %model,
            "model intent configured for a backend with no declared model \
             capability — skipping --model injection (#2744)"
        );
        return;
    };
    let hits = cap.scan(args);
    if !hits.is_empty() {
        if let Some(ModelFlagHit::Ambiguous(tok)) = hits
            .iter()
            .find(|h| matches!(h, ModelFlagHit::Ambiguous(_)))
        {
            tracing::warn!(
                backend = %backend.name(),
                token = %tok,
                model = %model,
                "ambiguous model-flag-like token suppresses fleet model \
                 injection; move payload after `--` or remove the token (#2744)"
            );
        }
        return;
    }
    let model_val = format_model_arg(backend, model);
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args.insert(at, cap.long_flag.to_string());
    args.insert(at + 1, model_val);
}

/// #3541: apply the fleet-resolved effort intent to a spawn argv,
/// gated on the DECLARED backend's [`EffortCapability`].
///
/// Double Fallback Drop (fail-soft, never crashes the CLI):
/// - No capability (KiroCli/OpenCode/Grok/Shell/Raw) → warn + skip:
///   there is no proven effort syntax to inject.
/// - Value outside this backend's `allowed_values` (e.g. a fleet-wide
///   `defaults.effort: max` reaching Codex, or a hand-written typo) →
///   warn + skip: injecting an unproven value risks a CLI error.
/// - Hand-written effort setting already in args wins (caller args >
///   fleet intent, #2038 precedence): skip with a warning, never inject
///   a duplicate.
/// - The pair is inserted BEFORE the first bare `--` — everything after
///   the delimiter is payload, not flag territory. Empty effort is a
///   no-op.
pub fn push_effort_arg(args: &mut Vec<String>, backend: &Backend, effort: &str) {
    if effort.is_empty() {
        return;
    }
    let Some(cap) = backend.effort_capability() else {
        tracing::warn!(
            backend = %backend.name(),
            effort = %effort,
            "effort intent configured for a backend with no declared effort \
             capability — fallback dropping effort argument (#3541)"
        );
        return;
    };
    if !cap.allowed_values.contains(&effort) {
        tracing::warn!(
            backend = %backend.name(),
            effort = %effort,
            allowed = ?cap.allowed_values,
            "effort value outside this backend's allowed values — fallback \
             dropping effort argument (#3541)"
        );
        return;
    }
    if let Some(hit) = crate::backend_effort::scan_effort_conflict(backend, args) {
        tracing::warn!(
            backend = %backend.name(),
            token = %hit,
            effort = %effort,
            "explicit effort setting already present in args — skipping \
             fleet effort injection (#3541)"
        );
        return;
    }
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    match cap.injection {
        EffortInjectionKind::CliFlag => {
            args.insert(at, crate::backend_effort::EFFORT_LONG_FLAG.to_string());
            args.insert(at + 1, effort.to_string());
        }
        EffortInjectionKind::CodexConfig => {
            args.insert(at, "-c".to_string());
            args.insert(
                at + 1,
                format!("{}=\"{}\"", crate::backend_effort::CODEX_EFFORT_KEY, effort),
            );
        }
    }
}
