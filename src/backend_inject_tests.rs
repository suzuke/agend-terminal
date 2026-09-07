//! #3541: injection-chokepoint tests — split out of `backend.rs`
//! alongside `backend_inject.rs` (same anti-monolith ceiling reason).

use crate::backend::Backend;

/// #2038: `push_model_arg` appends the formatted flag pair and respects
/// an existing caller-supplied `--model` (separate or `=`-glued form).
#[test]
fn push_model_arg_appends_and_dedupes_2038() {
    let mut args = vec!["--continue".to_string()];
    Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "claude-opus-4-8");
    assert_eq!(args, vec!["--continue", "--model", "claude-opus-4-8"]);

    // OpenCode gets the provider prefix via format_model_arg.
    let mut args = Vec::new();
    Backend::push_model_arg(&mut args, &Backend::OpenCode, "opus");
    assert_eq!(args, vec!["--model", "anthropic/opus"]);

    // Caller already passed --model (separate form) — no duplicate.
    let mut args = vec!["--model".to_string(), "explicit".to_string()];
    Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
    assert_eq!(args, vec!["--model", "explicit"]);

    // Glued form counts too.
    let mut args = vec!["--model=explicit".to_string()];
    Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
    assert_eq!(args, vec!["--model=explicit"]);

    // Empty model is a no-op.
    let mut args = vec!["--continue".to_string()];
    Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "");
    assert_eq!(args, vec!["--continue"]);
}

/// #3541: `push_effort_arg` injects the right pair per backend and
/// inserts BEFORE the first bare `--` (payload territory).
#[test]
fn push_effort_arg_injects_per_backend_before_delimiter_3541() {
    // Claude: --effort <val>.
    let mut args = vec!["--continue".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::ClaudeCode, "high");
    assert_eq!(args, vec!["--continue", "--effort", "high"]);

    // Agy: same CliFlag shape.
    let mut args = Vec::new();
    Backend::push_effort_arg(&mut args, &Backend::Agy, "low");
    assert_eq!(args, vec!["--effort", "low"]);

    // Codex: -c model_reasoning_effort="<val>".
    let mut args: Vec<String> = Vec::new();
    Backend::push_effort_arg(&mut args, &Backend::Codex, "medium");
    assert_eq!(args, vec!["-c", "model_reasoning_effort=\"medium\""]);

    // Insert position: before `--`, payload untouched.
    let mut args = vec![
        "--continue".to_string(),
        "--".to_string(),
        "--effort".to_string(),
    ];
    Backend::push_effort_arg(&mut args, &Backend::ClaudeCode, "low");
    assert_eq!(
        args,
        vec!["--continue", "--effort", "low", "--", "--effort"]
    );

    // Empty effort is a no-op.
    let mut args = vec!["--continue".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::ClaudeCode, "");
    assert_eq!(args, vec!["--continue"]);
}

/// #3541: double Fallback Drop — unsupported backends AND out-of-range
/// values (fleet-wide defaults reaching a narrower backend, or typos)
/// leave argv untouched instead of crashing the CLI.
#[test]
fn push_effort_arg_fallback_drops_unsupported_and_invalid_3541() {
    // Unsupported backends: drop, argv unchanged.
    for backend in [
        Backend::KiroCli,
        Backend::OpenCode,
        Backend::Grok,
        Backend::Shell,
        Backend::Raw("/opt/custom/agent-bin".into()),
    ] {
        let mut args: Vec<String> = Vec::new();
        Backend::push_effort_arg(&mut args, &backend, "high");
        assert!(
            args.is_empty(),
            "backend {backend:?} must not receive effort, got {args:?}"
        );
    }

    // Fleet-wide `defaults.effort: max` reaching Codex/Agy: drop.
    for backend in [Backend::Codex, Backend::Agy] {
        let mut args: Vec<String> = Vec::new();
        Backend::push_effort_arg(&mut args, &backend, "max");
        assert!(
            args.is_empty(),
            "backend {backend:?} must drop out-of-range 'max', got {args:?}"
        );
    }

    // Hand-written typo: drop.
    let mut args = vec!["--continue".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::ClaudeCode, "ultra");
    assert_eq!(args, vec!["--continue"]);
}

/// #3541: hand-written effort in args wins — no duplicate injection.
#[test]
fn push_effort_arg_skips_on_hand_written_conflict_3541() {
    let mut args = vec!["--effort".to_string(), "low".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::ClaudeCode, "high");
    assert_eq!(args, vec!["--effort", "low"]);

    let mut args = vec![
        "-c".to_string(),
        "model_reasoning_effort=\"low\"".to_string(),
    ];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(args, vec!["-c", "model_reasoning_effort=\"low\""]);
}

/// #3543 R1 B1: clap also accepts the config value GLUED to the short flag
/// (`-c<val>`). That is the same hand-written setting as `-c <val>`, so
/// fleet effort must not append a second `-c model_reasoning_effort=…`:
/// Codex applies `-c` overrides in order, the later one wins, and the fleet
/// value would silently override what the operator typed.
#[test]
fn push_effort_arg_skips_on_glued_short_codex_config_3541() {
    let mut args = vec!["-cmodel_reasoning_effort=\"low\"".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(
        args,
        vec!["-cmodel_reasoning_effort=\"low\""],
        "hand-written glued -c must win — no second -c appended"
    );

    // A glued value for a DIFFERENT key says nothing about effort, so fleet
    // intent still applies. (Guards against over-blocking the whole spelling.)
    let mut args = vec!["-cmodel=\"o3\"".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(
        args,
        vec!["-cmodel=\"o3\"", "-c", "model_reasoning_effort=\"high\""],
        "an unrelated glued config key must not block effort injection"
    );
}

/// #3543 R1 B1: the long glued spelling `--config=<val>` is the same
/// setting again — see the short-flag test above for why a second `-c`
/// would reverse the documented "caller args > fleet intent" order.
#[test]
fn push_effort_arg_skips_on_glued_long_codex_config_3541() {
    let mut args = vec!["--config=model_reasoning_effort=\"low\"".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(
        args,
        vec!["--config=model_reasoning_effort=\"low\""],
        "hand-written --config=… must win — no second -c appended"
    );
}

/// #3557 R2 B1: `-c=KEY=V` is the third glued spelling codex accepts
/// (verified against codex 0.153.2: `-cfoo=bar`, `-c=foo=bar` and
/// `--config=foo=bar` all exit 0). Stripping only the flag left the `=`
/// separator on the value, so the scan read it as "no effort set" and the
/// fleet value was appended on top of the operator's.
#[test]
fn push_effort_arg_skips_on_equals_separated_codex_config_3541() {
    let mut args = vec!["-c=model_reasoning_effort=\"low\"".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(
        args,
        vec!["-c=model_reasoning_effort=\"low\""],
        "hand-written -c=… must win — no second -c appended"
    );
}

/// #3557 N6: a DIFFERENT codex setting that merely shares the prefix is not
/// an effort setting, so fleet intent still applies. Prefix matching would
/// have read both of these as a conflict and silently dropped the injection.
#[test]
fn push_effort_arg_injects_for_prefix_sharing_codex_key_3541() {
    let mut args = vec![
        "-c".to_string(),
        "model_reasoning_effort_summary=\"auto\"".to_string(),
    ];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(
        args,
        vec![
            "-c",
            "model_reasoning_effort_summary=\"auto\"",
            "-c",
            "model_reasoning_effort=\"high\""
        ],
        "a prefix-sharing key must not block effort injection (separate form)"
    );

    let mut args = vec!["-cmodel_reasoning_effort_summary=\"auto\"".to_string()];
    Backend::push_effort_arg(&mut args, &Backend::Codex, "high");
    assert_eq!(
        args,
        vec![
            "-cmodel_reasoning_effort_summary=\"auto\"",
            "-c",
            "model_reasoning_effort=\"high\""
        ],
        "a prefix-sharing key must not block effort injection (glued form)"
    );
}

/// #2744 PR-A: Shell/Raw (any command without a declared model
/// capability) must never receive a blind `--model` injection — `bash
/// --model X` fails to spawn, and an arbitrary executable's argv
/// semantics are unknown. Reachable today via create_instance's
/// unrestricted `model` param.
#[test]
fn push_model_arg_shell_raw_never_inject_2744() {
    let mut args: Vec<String> = Vec::new();
    Backend::push_model_arg(&mut args, &Backend::Shell, "opus");
    assert!(
        args.is_empty(),
        "shell must not receive --model, got {args:?}"
    );

    let mut args: Vec<String> = Vec::new();
    Backend::push_model_arg(
        &mut args,
        &Backend::Raw("/opt/custom/agent-bin".into()),
        "opus",
    );
    assert!(
        args.is_empty(),
        "raw must not receive --model, got {args:?}"
    );
}

/// #2744 PR-A (B8): dedupe must recognize the short `-m` spelling on
/// backends whose CLI help declares it (codex/opencode/grok — see
/// tests/fixtures/cli-help/) instead of appending a second model flag.
#[test]
fn push_model_arg_dedupes_short_m_on_declaring_backends_b8_2744() {
    for backend in [Backend::Codex, Backend::OpenCode, Backend::Grok] {
        let mut args = vec!["-m".to_string(), "explicit".to_string()];
        Backend::push_model_arg(&mut args, &backend, "from-fleet");
        assert_eq!(
            args,
            vec!["-m", "explicit"],
            "backend {backend:?}: separate -m must dedupe"
        );
    }
}

/// #2744 PR-A: claude/kiro-cli/agy help declares NO `-m` short flag — a
/// `-m` token there is not a model flag, so fleet injection must still
/// happen. Pins the per-backend alias set so the scanner never
/// over-matches on long-flag-only backends.
#[test]
fn push_model_arg_ignores_short_m_on_non_declaring_backends_2744() {
    for backend in [Backend::ClaudeCode, Backend::KiroCli, Backend::Agy] {
        let mut args = vec!["-m".to_string(), "unrelated".to_string()];
        Backend::push_model_arg(&mut args, &backend, "from-fleet");
        assert_eq!(
            args,
            vec!["-m", "unrelated", "--model", "from-fleet"],
            "backend {backend:?}: undeclared -m must not suppress injection"
        );
    }
}

/// #2744 PR-A: `-m=X` / `-mVAL` glued spellings are a CONSERVATIVE
/// conflict on -m-declaring backends: the glued-value acceptance is not
/// fixture-proven per CLI (clap vs yargs differ), so suppressing
/// injection is the fail-loud choice vs risking a double model flag.
#[test]
fn push_model_arg_conservative_conflict_on_glued_short_m_2744() {
    for tok in ["-m=explicit", "-mexplicit"] {
        let mut args = vec![tok.to_string()];
        Backend::push_model_arg(&mut args, &Backend::Codex, "from-fleet");
        assert_eq!(
            args,
            vec![tok],
            "glued {tok} must suppress injection (conservative conflict)"
        );
    }
}

/// #2744 PR-A: a bare `--` is the end-of-options delimiter. Injection
/// must place the flag pair BEFORE it (options territory), and model
/// tokens AFTER it are payload — never a dedupe/conflict match.
#[test]
fn push_model_arg_respects_double_dash_delimiter_2744() {
    // Inject before the delimiter, not appended into payload.
    let mut args = vec!["--".to_string(), "some prompt text".to_string()];
    Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
    assert_eq!(
        args,
        vec!["--model", "from-fleet", "--", "some prompt text"]
    );

    // `--model` inside payload is prompt text, not a real flag: fleet
    // injection must still happen (before the delimiter).
    let mut args = vec![
        "--".to_string(),
        "--model".to_string(),
        "quoted".to_string(),
    ];
    Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
    assert_eq!(
        args,
        vec!["--model", "from-fleet", "--", "--model", "quoted"]
    );
}

#[test]
fn format_model_arg_opencode_adds_prefix() {
    assert_eq!(
        crate::backend_inject::format_model_arg(&Backend::OpenCode, "opus"),
        "anthropic/opus"
    );
    assert_eq!(
        crate::backend_inject::format_model_arg(&Backend::OpenCode, "anthropic/opus"),
        "anthropic/opus"
    );
    assert_eq!(
        crate::backend_inject::format_model_arg(&Backend::OpenCode, "openai/gpt-4"),
        "openai/gpt-4"
    );
}

#[test]
fn format_model_arg_other_backends_passthrough() {
    assert_eq!(
        crate::backend_inject::format_model_arg(&Backend::ClaudeCode, "opus"),
        "opus"
    );
    assert_eq!(
        crate::backend_inject::format_model_arg(&Backend::Codex, "o3"),
        "o3"
    );
}
