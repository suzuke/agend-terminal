//! #3414: typed session-selector grammar per DECLARED backend.
//!
//! `restart_instance mode=fresh` is a statement about the SESSION, not merely
//! about the preset (`Backend::preset_spawn_args`). A caller arg that pins a
//! session — the observed `--resume <uuid>` — survived every fresh restart and
//! silently reattached the agent to the same conversation.
//!
//! Sibling of [`crate::backend_model`] and deliberately the same shape: the
//! spellings come from the DECLARED [`Backend`], never from the command
//! basename (a wrapper script named `claude` is not proof of Claude grammar),
//! and flag territory ends at the first bare `--` — every token from the
//! delimiter onward is payload and is preserved byte-for-byte.

use crate::backend::Backend;

/// How many argv slots a selector owns beyond the flag token itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectorValue {
    /// Flag only (`-c`, `--resume-picker`).
    None,
    /// `--flag VALUE` / `--flag=VALUE`; a missing value is fail-closed.
    Required,
    /// Help declares the value in brackets (`--resume [value]`): the bare form
    /// is a legal picker, so a value is consumed ONLY when the next token is
    /// not itself a flag. Refusing the bare form would turn a legal restart
    /// into a refusal.
    Optional,
}

/// One session selector exactly as the backend's CLI help declares it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionSelector {
    pub long: &'static str,
    pub short: Option<&'static str>,
    pub value: SelectorValue,
}

/// Why a fresh restart refused to proceed. Carried into the MCP error so the
/// operator sees WHICH token blocked it, not just that something did.
#[derive(Debug, PartialEq)]
pub struct SessionArgsError {
    pub backend: String,
    pub token: String,
    pub reason: SessionArgsErrorReason,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum SessionArgsErrorReason {
    /// `--resume` with nothing usable after it.
    MissingRequiredValue,
    /// Glued/`=`-glued short spellings, or a Codex `resume` carrying more
    /// positionals than the session id it is allowed to own.
    Ambiguous,
    /// A session-coupled flag with no selector to couple to (`--fork` alone).
    OrphanCoupledFlag,
}

impl SessionArgsErrorReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingRequiredValue => "missing_required_value",
            Self::Ambiguous => "ambiguous",
            Self::OrphanCoupledFlag => "orphan_coupled_flag",
        }
    }
}

impl std::fmt::Display for SessionArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} session selector {:?} is {}",
            self.backend,
            self.token,
            self.reason.as_str()
        )
    }
}

impl std::error::Error for SessionArgsError {}

/// Selector matrix. One row per spelling the backend's help declares; the
/// couplings that a flat row cannot express (Codex's `resume` subcommand,
/// OpenCode `--fork`, Grok `--restore-code`) are handled explicitly in
/// [`sanitize_for_fresh`] so they stay visible rather than encoded.
fn selectors(backend: &Backend) -> &'static [SessionSelector] {
    const fn sel(
        long: &'static str,
        short: Option<&'static str>,
        value: SelectorValue,
    ) -> SessionSelector {
        SessionSelector { long, short, value }
    }
    const CLAUDE: &[SessionSelector] = &[
        sel("--continue", Some("-c"), SelectorValue::None),
        sel("--resume", Some("-r"), SelectorValue::Optional),
        sel("--from-pr", None, SelectorValue::Optional),
        sel("--teleport", None, SelectorValue::Optional),
    ];
    const KIRO: &[SessionSelector] = &[
        sel("--resume", Some("-r"), SelectorValue::None),
        sel("--resume-id", None, SelectorValue::Required),
        sel("--resume-picker", None, SelectorValue::None),
    ];
    const OPENCODE: &[SessionSelector] = &[
        sel("--continue", Some("-c"), SelectorValue::None),
        sel("--session", Some("-s"), SelectorValue::Required),
    ];
    const AGY: &[SessionSelector] = &[
        sel("--continue", Some("-c"), SelectorValue::None),
        sel("--conversation", None, SelectorValue::Required),
    ];
    const GROK: &[SessionSelector] = &[
        sel("--continue", Some("-c"), SelectorValue::None),
        sel("--resume", Some("-r"), SelectorValue::Optional),
    ];
    const NONE: &[SessionSelector] = &[];
    match backend {
        Backend::ClaudeCode => CLAUDE,
        Backend::KiroCli => KIRO,
        // Codex has no flag-form selector; its pin is the `resume` SUBCOMMAND,
        // handled explicitly below. Its global `-c` is CONFIG, not continue —
        // treating it as a selector would silently drop operator config.
        Backend::Codex => NONE,
        Backend::OpenCode => OPENCODE,
        Backend::Agy => AGY,
        Backend::Grok => GROK,
        // No declared grammar: never rewrite what we cannot parse.
        Backend::Shell | Backend::Raw(_) => NONE,
    }
}

/// Index of the first bare `--`; flag territory is everything before it.
fn payload_start(args: &[String]) -> usize {
    args.iter().position(|t| t == "--").unwrap_or(args.len())
}

/// Session-coupled flags: meaningless without a selector, so a fresh restart
/// removes them WITH the selector and refuses when one appears alone. Kept
/// beside the matrix rather than inside it because coupling is a relation,
/// not a spelling.
fn coupled_flags(backend: &Backend) -> &'static [&'static str] {
    match backend {
        // `opencode --help`: `--fork` is documented "use with --continue or
        // --session" (verified by lead on a local install).
        Backend::OpenCode => &["--fork"],
        // `claude --help`: "When resuming, create a new session ID ... with
        // --resume or --continue".
        Backend::ClaudeCode => &["--fork-session"],
        // `grok --help`: `--fork-session` is coupled to `--resume`/`--continue`
        // and `--restore-code` applies "when resuming".
        Backend::Grok => &["--fork-session", "--restore-code"],
        _ => &[],
    }
}

fn err(backend: &Backend, token: &str, reason: SessionArgsErrorReason) -> SessionArgsError {
    SessionArgsError {
        backend: backend.name().to_string(),
        token: token.to_string(),
        reason,
    }
}

/// Strip this backend's declared session selectors from `args`.
///
/// Only tokens before the first bare `--` are inspected. The delimiter and
/// everything after it are copied byte-for-byte, as are all unrelated flags
/// (model, config, and anything this backend does not declare as a selector).
///
/// Fails closed — the caller must abandon the restart WITHOUT touching the
/// live instance — when a selector's grammar cannot be resolved unambiguously.
/// Codex globals that OWN the following token, read off `codex --help`.
/// Without this the parser mistook a global's VALUE for the `resume`
/// subcommand — `-c resume` ate the config value and left a dangling flag.
const CODEX_VALUED_GLOBALS: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-i",
    "--image",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "--add-dir",
    "-a",
    "--ask-for-approval",
];

/// Flags `codex resume --help` declares as belonging to the SUBCOMMAND.
/// Promoting them to top level yields an argv the CLI rejects.
const CODEX_RESUME_OWNED_FLAGS: &[&str] = &["--last", "--all", "--include-non-interactive"];

/// Consume the Codex `resume` subcommand and everything it owns.
/// Returns the index just past it, or an error when the grammar is ambiguous.
fn codex_consume_resume(args: &[String], start: usize, boundary: usize) -> Result<usize, ()> {
    let mut positionals = 0usize;
    let mut probe = start + 1;
    while probe < boundary {
        let tok = &args[probe];
        if CODEX_RESUME_OWNED_FLAGS.contains(&tok.as_str()) {
            probe += 1;
            continue;
        }
        if tok.starts_with('-') {
            break;
        }
        positionals += 1;
        if positionals > 1 {
            // The second positional is the PROMPT. Dropping it loses user
            // intent and promoting it rewrites what the agent is asked to do,
            // so neither is done.
            return Err(());
        }
        probe += 1;
    }
    Ok(probe)
}

/// Strip this backend's declared session selectors from `args`.
///
/// Only tokens before the first bare `--` are inspected. The delimiter and
/// everything after it are copied byte-for-byte, as are all unrelated flags.
///
/// Fails closed — the caller must abandon the restart WITHOUT touching the
/// live instance — when a selector's grammar cannot be resolved unambiguously.
pub fn sanitize_for_fresh(
    backend: &Backend,
    args: &[String],
) -> Result<Vec<String>, SessionArgsError> {
    let matrix = selectors(backend);
    let coupled = coupled_flags(backend);
    let is_codex = matches!(backend, Backend::Codex);
    if matrix.is_empty() && coupled.is_empty() && !is_codex {
        return Ok(args.to_vec());
    }

    let boundary = payload_start(args);
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut removed_selector = false;
    // Coupled flags may PRECEDE their selector, so they are withheld and
    // resolved after the whole flag territory is known — order must not decide.
    let mut deferred_coupled: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < boundary {
        let tok = &args[index];

        if is_codex {
            // A valued global owns the next token; skipping it is what keeps
            // `-c resume` from being read as the subcommand.
            if CODEX_VALUED_GLOBALS.contains(&tok.as_str()) {
                out.push(tok.clone());
                if let Some(value) = args.get(index + 1).filter(|_| index + 1 < boundary) {
                    out.push(value.clone());
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            if tok == "resume" {
                let next = codex_consume_resume(args, index, boundary)
                    .map_err(|()| err(backend, tok, SessionArgsErrorReason::Ambiguous))?;
                removed_selector = true;
                index = next;
                continue;
            }
        }

        if coupled.contains(&tok.as_str()) {
            deferred_coupled.push(tok.clone());
            index += 1;
            continue;
        }

        if let Some(selector) = matrix
            .iter()
            .find(|s| tok == s.long || s.short.is_some_and(|short| tok == short))
        {
            removed_selector = true;
            index += 1;
            let value_follows = args
                .get(index)
                .filter(|_| index < boundary)
                .filter(|v| !v.is_empty() && *v != "--" && !v.starts_with('-'));
            match selector.value {
                SelectorValue::None => {}
                SelectorValue::Optional => {
                    if value_follows.is_some() {
                        index += 1;
                    }
                }
                SelectorValue::Required => {
                    if value_follows.is_none() {
                        return Err(err(
                            backend,
                            tok,
                            SessionArgsErrorReason::MissingRequiredValue,
                        ));
                    }
                    index += 1;
                }
            }
            continue;
        }

        // `--flag=VALUE` — one token, owns its own value.
        if let Some(selector) = matrix
            .iter()
            .find(|s| tok.strip_prefix(s.long).is_some_and(|r| r.starts_with('=')))
        {
            if tok.len() == selector.long.len() + 1 {
                return Err(err(
                    backend,
                    tok,
                    SessionArgsErrorReason::MissingRequiredValue,
                ));
            }
            removed_selector = true;
            index += 1;
            continue;
        }

        // Glued short spellings (`-rVAL`, `-r=VAL`). Parser acceptance is not
        // fixture-proven across clap/yargs, so — exactly as `ModelCapability`
        // treats them — they are a conservative conflict rather than a guess.
        if matrix.iter().any(|s| {
            s.short
                .is_some_and(|short| tok.strip_prefix(short).is_some_and(|rest| !rest.is_empty()))
        }) {
            return Err(err(backend, tok, SessionArgsErrorReason::Ambiguous));
        }

        out.push(tok.clone());
        index += 1;
    }

    // A coupled flag with nothing to couple to would be handed to the freshly
    // spawned child as an orphan — and the old instance is already gone by
    // then, so this must refuse rather than silently pass it through.
    if !removed_selector {
        if let Some(orphan) = deferred_coupled.first() {
            return Err(err(
                backend,
                orphan,
                SessionArgsErrorReason::OrphanCoupledFlag,
            ));
        }
    }

    out.extend_from_slice(&args[boundary..]);
    Ok(out)
}

/// Shared fixtures for both test modules.
#[cfg(test)]
mod tests_support {
    use super::*;

    pub fn v(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_string()).collect()
    }

    pub fn ok(backend: Backend, tokens: &[&str]) -> Vec<String> {
        sanitize_for_fresh(&backend, &v(tokens)).expect("selector grammar must resolve")
    }

    pub fn reason(backend: Backend, tokens: &[&str]) -> SessionArgsErrorReason {
        sanitize_for_fresh(&backend, &v(tokens))
            .expect_err("malformed selector must fail closed")
            .reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_string()).collect()
    }

    fn fresh(backend: Backend, tokens: &[&str]) -> Result<Vec<String>, SessionArgsError> {
        sanitize_for_fresh(&backend, &v(tokens))
    }

    fn ok(backend: Backend, tokens: &[&str]) -> Vec<String> {
        fresh(backend, tokens).expect("selector grammar must resolve")
    }

    fn reason(backend: Backend, tokens: &[&str]) -> SessionArgsErrorReason {
        fresh(backend, tokens)
            .expect_err("malformed selector must fail closed")
            .reason
    }

    /// The exact observed production case (#3414): a durable `--resume <uuid>`
    /// in the instance's stored args survived every fresh restart. The model
    /// flag is unrelated and must be preserved.
    #[test]
    fn claude_fresh_strips_observed_resume_pin_and_keeps_model_3414() {
        assert_eq!(
            ok(
                Backend::ClaudeCode,
                &[
                    "--resume",
                    "cb80bb9d-3613-4c0d-9097-788b1941f5db",
                    "--model",
                    "claude-opus-5"
                ]
            ),
            v(&["--model", "claude-opus-5"])
        );
    }

    #[test]
    fn selector_aliases_and_equals_forms_are_stripped_3414() {
        assert_eq!(
            ok(Backend::ClaudeCode, &["-c", "--verbose"]),
            v(&["--verbose"])
        );
        assert_eq!(ok(Backend::ClaudeCode, &["--continue"]), v(&[]));
        assert_eq!(ok(Backend::ClaudeCode, &["--resume=abc"]), v(&[]));
        assert_eq!(ok(Backend::ClaudeCode, &["--from-pr=42"]), v(&[]));
        assert_eq!(ok(Backend::ClaudeCode, &["--teleport", "x"]), v(&[]));
        assert_eq!(ok(Backend::KiroCli, &["-r"]), v(&[]));
        assert_eq!(ok(Backend::KiroCli, &["--resume-id=7"]), v(&[]));
        assert_eq!(ok(Backend::KiroCli, &["--resume-picker"]), v(&[]));
        assert_eq!(ok(Backend::OpenCode, &["-s", "sess-1"]), v(&[]));
        assert_eq!(ok(Backend::Agy, &["--conversation=c1"]), v(&[]));
        assert_eq!(ok(Backend::Grok, &["-r", "g1"]), v(&[]));
    }

    /// Negative controls: a flag that merely LOOKS like a selector prefix, and
    /// every unrelated flag, must survive untouched.
    #[test]
    fn unrelated_and_prefix_lookalike_flags_survive_3414() {
        assert_eq!(
            ok(Backend::ClaudeCode, &["--resume-foo", "--model", "m"]),
            v(&["--resume-foo", "--model", "m"])
        );
        assert_eq!(
            ok(Backend::Agy, &["--continue-on-error"]),
            v(&["--continue-on-error"])
        );
        // Codex's global `-c` is CONFIG, not continue.
        assert_eq!(ok(Backend::Codex, &["-c", "k=v"]), v(&["-c", "k=v"]));
    }

    /// Flag territory ends at the first bare `--`; the delimiter and every
    /// later token are payload and are preserved byte-for-byte — including a
    /// token that spells a selector.
    #[test]
    fn post_double_dash_payload_is_preserved_byte_for_byte_3414() {
        assert_eq!(
            ok(
                Backend::ClaudeCode,
                &["--resume", "u1", "--", "--resume", "u2", "-c"]
            ),
            v(&["--", "--resume", "u2", "-c"])
        );
    }

    /// SUPERSEDED BY INSTALLED HELP: the first two assertions here originally
    /// required a value for `claude --resume`. `claude --help` declares
    /// `-r, --resume [value]` — brackets mean OPTIONAL — so the bare form is a
    /// legal picker and refusing it turned a valid restart into
    /// `fresh_session_args_invalid`. Those two cases moved to
    /// `installed_help_fixtures_3414::claude_optional_value_selectors_accept_bare_picker_form_3414`
    /// with the opposite expectation. What remains here is genuinely required:
    /// an explicit `=` with nothing after it, and OpenCode `-s`, which
    /// `opencode --help` declares as `[string]` (a value, not a picker).
    #[test]
    fn missing_required_value_fails_closed_3414() {
        assert_eq!(
            reason(Backend::ClaudeCode, &["--resume="]),
            SessionArgsErrorReason::MissingRequiredValue
        );
        assert_eq!(
            reason(Backend::OpenCode, &["-s"]),
            SessionArgsErrorReason::MissingRequiredValue
        );
        assert_eq!(
            reason(Backend::OpenCode, &["--session="]),
            SessionArgsErrorReason::MissingRequiredValue
        );
    }

    /// Glued short spellings are not fixture-proven across clap/yargs, so they
    /// are a conservative conflict — same stance `ModelCapability` takes.
    #[test]
    fn glued_short_spellings_are_ambiguous_3414() {
        assert_eq!(
            reason(Backend::ClaudeCode, &["-rabc"]),
            SessionArgsErrorReason::Ambiguous
        );
        assert_eq!(
            reason(Backend::Grok, &["-r=abc"]),
            SessionArgsErrorReason::Ambiguous
        );
    }

    /// Codex pins via the `resume` SUBCOMMAND. Fresh may drop `resume` plus the
    /// one session-id positional it owns. A second positional is the PROMPT —
    /// silently promoting or dropping it would rewrite what the agent is asked
    /// to do, so it fails closed instead.
    #[test]
    fn codex_resume_subcommand_grammar_3414() {
        assert_eq!(ok(Backend::Codex, &["resume"]), v(&[]));
        assert_eq!(ok(Backend::Codex, &["resume", "sess-1"]), v(&[]));
        assert_eq!(
            ok(Backend::Codex, &["resume", "sess-1", "--model", "gpt"]),
            v(&["--model", "gpt"])
        );
        assert_eq!(
            reason(Backend::Codex, &["resume", "sess-1", "do the thing"]),
            SessionArgsErrorReason::Ambiguous
        );
    }

    /// Session-coupled flags go with their selector, and a lone one is
    /// malformed rather than silently kept.
    #[test]
    fn session_coupled_flags_follow_their_selector_3414() {
        assert_eq!(ok(Backend::OpenCode, &["-s", "s1", "--fork"]), v(&[]));
        assert_eq!(
            reason(Backend::OpenCode, &["--fork"]),
            SessionArgsErrorReason::OrphanCoupledFlag
        );
        assert_eq!(ok(Backend::Grok, &["-c", "--restore-code"]), v(&[]));
        assert_eq!(
            reason(Backend::Grok, &["--restore-code"]),
            SessionArgsErrorReason::OrphanCoupledFlag
        );
    }

    /// Backends with no declared grammar are never rewritten.
    #[test]
    fn shell_and_raw_args_are_never_rewritten_3414() {
        assert_eq!(
            ok(Backend::Shell, &["--resume", "x", "-c"]),
            v(&["--resume", "x", "-c"])
        );
        assert_eq!(
            ok(Backend::Raw("/opt/thing".into()), &["-r", "y"]),
            v(&["-r", "y"])
        );
    }
}

/// Supplemental RED for lead review of 91d82117. Every fixture below is taken
/// from the INSTALLED CLI help on this machine, not from inference — the
/// original matrix guessed value arity and missed two coupled flags, and the
/// Codex arm treated any `resume` token as the subcommand.
#[cfg(test)]
mod installed_help_fixtures_3414 {
    use super::tests_support::*;
    use super::*;

    /// `claude --help`: `-r, --resume [value]`, `--from-pr [value]`,
    /// `--teleport [session]` — the brackets mean the value is OPTIONAL, so a
    /// bare picker form is valid. Marking them Required turned a legal restart
    /// into `fresh_session_args_invalid`.
    #[test]
    fn claude_optional_value_selectors_accept_bare_picker_form_3414() {
        assert_eq!(ok(Backend::ClaudeCode, &["--resume"]), v(&[]));
        assert_eq!(ok(Backend::ClaudeCode, &["-r"]), v(&[]));
        assert_eq!(ok(Backend::ClaudeCode, &["--from-pr"]), v(&[]));
        assert_eq!(ok(Backend::ClaudeCode, &["--teleport"]), v(&[]));
        // A bare selector must not swallow the NEXT flag as its value.
        assert_eq!(
            ok(Backend::ClaudeCode, &["--resume", "--model", "opus"]),
            v(&["--model", "opus"])
        );
        // The value form still works.
        assert_eq!(ok(Backend::ClaudeCode, &["--resume", "sess-1"]), v(&[]));
    }

    /// `grok --help`: `-r, --resume [<SESSION_ID_OR_TITLE>]` is OPTIONAL too.
    #[test]
    fn grok_resume_value_is_optional_3414() {
        assert_eq!(ok(Backend::Grok, &["--resume"]), v(&[]));
        assert_eq!(ok(Backend::Grok, &["-r"]), v(&[]));
        assert_eq!(ok(Backend::Grok, &["-r", "my-title"]), v(&[]));
    }

    /// Both Claude and Grok declare `--fork-session` as resume/continue-coupled
    /// ("When resuming ... create a new session ID"). Leaving it behind after
    /// the selector is stripped hands the freshly spawned child an orphan flag
    /// — and by then the old instance is already deleted.
    #[test]
    fn fork_session_is_coupled_for_claude_and_grok_3414() {
        assert_eq!(
            ok(Backend::ClaudeCode, &["--resume", "s1", "--fork-session"]),
            v(&[])
        );
        assert_eq!(ok(Backend::Grok, &["-c", "--fork-session"]), v(&[]));
        assert_eq!(
            reason(Backend::ClaudeCode, &["--fork-session"]),
            SessionArgsErrorReason::OrphanCoupledFlag
        );
        assert_eq!(
            reason(Backend::Grok, &["--fork-session"]),
            SessionArgsErrorReason::OrphanCoupledFlag
        );
    }

    /// Coupled flags may precede their selector; order must not decide.
    #[test]
    fn coupled_flag_order_permutations_3414() {
        assert_eq!(
            ok(Backend::ClaudeCode, &["--fork-session", "--resume", "s1"]),
            v(&[])
        );
        assert_eq!(ok(Backend::OpenCode, &["--fork", "-s", "s1"]), v(&[]));
        assert_eq!(ok(Backend::Grok, &["--restore-code", "-r"]), v(&[]));
    }

    /// `codex --help`: `-c/--config <key=value>` and `-m/--model <MODEL>` are
    /// VALUED globals. Treating any `resume` token as the subcommand ate the
    /// value and left a dangling flag — which also broke the explicit
    /// preserve-`-c` contract this module claims to honour.
    #[test]
    fn codex_valued_globals_are_not_mistaken_for_the_subcommand_3414() {
        assert_eq!(ok(Backend::Codex, &["-c", "resume"]), v(&["-c", "resume"]));
        assert_eq!(
            ok(Backend::Codex, &["--model", "resume"]),
            v(&["--model", "resume"])
        );
        assert_eq!(
            ok(Backend::Codex, &["--config", "k=v", "resume", "s1"]),
            v(&["--config", "k=v"])
        );
    }

    /// `codex resume --help`: `--last`, `--all` and `--include-non-interactive`
    /// belong to the subcommand. Promoting them to top level produces an argv
    /// the CLI does not accept.
    #[test]
    fn codex_resume_owned_flags_do_not_survive_to_top_level_3414() {
        assert_eq!(ok(Backend::Codex, &["resume", "--last"]), v(&[]));
        assert_eq!(ok(Backend::Codex, &["resume", "--all"]), v(&[]));
        assert_eq!(
            ok(Backend::Codex, &["resume", "--include-non-interactive"]),
            v(&[])
        );
        assert_eq!(
            ok(Backend::Codex, &["resume", "--last", "--model", "gpt"]),
            v(&["--model", "gpt"])
        );
    }
}
