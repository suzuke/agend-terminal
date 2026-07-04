//! Interactive quickstart — detect backends, configure Telegram, generate fleet.yaml.

use crate::backend::Backend;
use std::io::{self, Write};
use std::path::Path;

/// Read whatever Telegram state a prior run left behind: the bot token from
/// `.env` and the `group_id` from `fleet.yaml`. Shared by the interactive and
/// unattended entry points, which both start from the same two sources
/// before applying their own precedence rules on top.
fn read_existing_telegram_state(home: &Path) -> (Option<String>, Option<i64>) {
    let env_path = home.join(".env");
    let token = std::fs::read_to_string(&env_path)
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find_map(extract_env_token)
                .map(str::to_string)
        })
        .filter(|t| !t.is_empty());

    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let group_id = std::fs::read_to_string(&fleet_path)
        .ok()
        .and_then(|content| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content).ok())
        .and_then(|config| config["channel"]["group_id"].as_i64());

    (token, group_id)
}

pub fn run(home: &Path, unattended: bool) -> anyhow::Result<()> {
    if unattended {
        return run_unattended(home);
    }
    println!("\n  AgEnD Terminal — Quickstart\n");

    // Step 1: Detect backends
    let backends = detect_backends();
    if backends.is_empty() {
        // Sprint 56 Track H4 (#525 item 13): list the supported backends so
        // operators with a Kiro or OpenCode preference get an install hint
        // instead of a bare "no supported backends". #1580: the Gemini CLI line
        // was dropped — gemini-cli is retired (sunset 2026-06-18); its successor
        // Agy (Antigravity CLI) is detected by command but has no npm one-liner.
        println!("  No supported backends found. Install one of:");
        println!("    Claude Code   npm install -g @anthropic-ai/claude-code");
        println!("    codex         npm install -g @openai/codex");
        println!("    Kiro CLI      see https://kiro.dev for installer");
        println!("    OpenCode      see https://opencode.ai for installer");
        println!();
        return Ok(());
    }

    let selected = if backends.len() == 1 {
        println!("  ✓ Detected: {}\n", backends[0].name());
        backends[0].clone()
    } else {
        println!("  Detected {} backends:", backends.len());
        for (i, b) in backends.iter().enumerate() {
            let version = b.get_version().unwrap_or_else(|| "?".into());
            println!("    {}. {} (v{})", i + 1, b.name(), version);
        }
        let choice = prompt(&format!("\n  Select backend [1-{}]: ", backends.len()))?;
        let idx: usize = choice.trim().parse().unwrap_or(1);
        let idx = idx.saturating_sub(1).min(backends.len() - 1);
        println!("  ✓ Selected: {}\n", backends[idx].name());
        backends[idx].clone()
    };

    // Step 2/3: existing .env token + fleet.yaml group_id from a prior run.
    let (existing_token, existing_group_id) = read_existing_telegram_state(home);

    let (token, group_id, user_id) = if existing_token.is_some() && existing_group_id.is_some() {
        let tok = existing_token.clone().unwrap_or_default();
        let gid = existing_group_id.unwrap_or(0);
        println!("  ── Telegram ──\n");
        println!("  ✓ Token: {}\n  ✓ Group: {gid}", mask_token(&tok));
        let answer = prompt("\n  Use existing Telegram config? (Y/n): ")?;
        if answer.trim().eq_ignore_ascii_case("n") {
            telegram_setup(home)?
        } else {
            println!();
            // Reusing an existing config — no fresh getUpdates poll, so no
            // auto-detected sender (#2207 B); the existing allowlist stands.
            (tok, Some(gid), None)
        }
    } else if let Some(tok) = existing_token {
        println!("  ── Telegram ──\n");
        println!("  ✓ Token found: {}", mask_token(&tok));
        let answer = prompt("  Use existing token? (Y/n): ")?;
        if answer.trim().eq_ignore_ascii_case("n") {
            telegram_setup(home)?
        } else {
            println!("\n  Add the bot to your Telegram group and send a message.\n");
            print!("  Waiting for group message (3 min timeout)... ");
            io::stdout().flush().ok();
            match detect_group(&tok) {
                Ok((gid, title, sender)) => {
                    println!("✓ {title} ({gid})\n");
                    (tok, Some(gid), sender)
                }
                Err(e) => {
                    println!("timeout: {e}\n");
                    (tok, None, None)
                }
            }
        }
    } else {
        telegram_setup(home)?
    };

    // Save .env + fleet.yaml
    if !token.is_empty() {
        save_env_token(home, &token, false)?;
    }
    generate_fleet_yaml(
        home,
        &selected,
        group_id,
        if token.is_empty() { None } else { Some(&token) },
        user_id,
        false,
    )?;

    print_next_steps(home);
    Ok(())
}

/// Non-interactive setup for CI / scripted installs (`quickstart
/// --unattended`). Hard guarantees, both structural (nothing in this call
/// graph reads stdin or talks to the network):
///   - NEVER reads stdin — a missing required input is a clear error +
///     non-zero exit, not a hang (the CI killer the flag exists to avoid).
///   - NEVER waits on the network — no `detect_group` (3-min wait) and no
///     `verify_bot`; an env-provided token is stored UNVERIFIED (noted in
///     the output; the daemon surfaces a bad token at startup).
///
/// Inputs: backend = first detected (no hardcoded assumption that any
/// specific backend is installed — zero detected is the one hard error);
/// Telegram from `AGEND_TELEGRAM_BOT_TOKEN` / `AGEND_TELEGRAM_GROUP_ID` env
/// (explicit per-invocation instruction, wins over `.env`), falling back to
/// an existing `.env` / fleet.yaml, else skipped (Telegram is optional).
/// An existing fleet.yaml is kept untouched → idempotent re-runs.
fn run_unattended(home: &Path) -> anyhow::Result<()> {
    println!("\n  AgEnD Terminal — Quickstart (unattended)\n");

    let backends = detect_backends();
    let Some(selected) = backends.first().cloned() else {
        eprintln!("  ✗ No supported backend found on PATH. Install one of:");
        eprintln!("      Claude Code   npm install -g @anthropic-ai/claude-code");
        eprintln!("      codex         npm install -g @openai/codex");
        eprintln!("      Kiro CLI      see https://kiro.dev for installer");
        eprintln!("      OpenCode      see https://opencode.ai for installer");
        anyhow::bail!("unattended quickstart: no supported backend detected");
    };
    println!(
        "  ✓ Backend: {} (first of {} detected)",
        selected.name(),
        backends.len()
    );

    // Existing state (the same sources the interactive flow reads).
    let (env_file_token, fleet_group_id) = read_existing_telegram_state(home);

    let resolved = resolve_unattended_telegram(
        env_file_token,
        fleet_group_id,
        std::env::var("AGEND_TELEGRAM_BOT_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty()),
        std::env::var("AGEND_TELEGRAM_GROUP_ID").ok(),
    );
    for note in &resolved.notes {
        println!("  · {note}");
    }

    if let Some(token) = &resolved.token {
        save_env_token(home, token, true)?;
    }
    generate_fleet_yaml(
        home,
        &selected,
        resolved.group_id,
        resolved.token.as_deref(),
        // Unattended never runs detect_group (no network) → no auto-detected
        // sender; the allowlist stays as-is (#2207 B is interactive-only).
        None,
        true,
    )?;

    print_next_steps(home);
    Ok(())
}

/// Decision record for the unattended Telegram resolution — pure so the
/// precedence matrix is unit-testable without env/process state.
struct UnattendedTelegram {
    token: Option<String>,
    group_id: Option<i64>,
    /// Human-readable decisions for the CI log (what was used, what was
    /// skipped and why).
    notes: Vec<String>,
}

/// Precedence: an env var is an explicit per-invocation instruction and wins
/// over state left by a previous run (`.env` token / fleet.yaml group_id).
/// No input at all → Telegram is skipped (it is optional; the fleet is
/// generated with a commented channel block).
fn resolve_unattended_telegram(
    env_file_token: Option<String>,
    fleet_group_id: Option<i64>,
    env_var_token: Option<String>,
    env_var_group_id: Option<String>,
) -> UnattendedTelegram {
    let mut notes = Vec::new();

    let token = match (env_var_token, env_file_token) {
        (Some(t), _) => {
            notes.push(
                "Telegram token: from AGEND_TELEGRAM_BOT_TOKEN env (stored unverified — \
                 network checks are skipped in unattended mode)"
                    .to_string(),
            );
            Some(t)
        }
        (None, Some(t)) => {
            notes.push(format!(
                "Telegram token: existing .env ({})",
                mask_token(&t)
            ));
            Some(t)
        }
        (None, None) => {
            notes.push(
                "Telegram: skipped (no AGEND_TELEGRAM_BOT_TOKEN env and no existing .env token)"
                    .to_string(),
            );
            None
        }
    };

    let group_id = if token.is_none() {
        None
    } else {
        match env_var_group_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<i64>())
        {
            Some(Ok(gid)) => {
                notes.push(format!(
                    "Telegram group: {gid} (from AGEND_TELEGRAM_GROUP_ID env)"
                ));
                Some(gid)
            }
            Some(Err(_)) => {
                notes.push(
                    "Telegram group: AGEND_TELEGRAM_GROUP_ID is not a valid integer — \
                     ignored (channel block left commented)"
                        .to_string(),
                );
                fleet_group_id
            }
            None => {
                if let Some(gid) = fleet_group_id {
                    notes.push(format!("Telegram group: {gid} (existing fleet.yaml)"));
                } else {
                    notes.push(
                        "Telegram group: none (group detection needs interactive mode — \
                         channel block left commented)"
                            .to_string(),
                    );
                }
                fleet_group_id
            }
        }
    };

    UnattendedTelegram {
        token,
        group_id,
        notes,
    }
}

/// Sprint 56 Track H3 (#525 items 7 + 16 + 17): operator response to
/// a failed format/verify check. The same enum drives both the
/// format-validation path and the verify_bot-failure path so the
/// flow stays consistent across the two error classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenChoice {
    /// Re-prompt for a fresh token (Item 16 retry loop entry).
    ReEnter,
    /// Skip telegram setup entirely; return early without ever
    /// reaching detect_group's 3-minute long-poll (Item 17 short-
    /// circuit).
    Skip,
    /// Proceed despite the warning (operator's escape hatch — covers
    /// the rare case where the format check is over-strict or a
    /// network blip caused verify_bot to fail spuriously).
    Continue,
}

/// Sprint 56 Track H3 (#525 item 6): operator response after the
/// 3-minute getUpdates poll times out. Three branches mirroring the
/// post-fail UX of TokenChoice but scoped to the post-timeout case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutChoice {
    /// Re-arm the 3-minute getUpdates wait. Operator may have
    /// forgotten to send a group message and wants to retry.
    Retry,
    /// Skip the group capture; quickstart writes fleet.yaml without
    /// `group_id` and the operator can hand-fill it later.
    Skip,
    /// Abort quickstart entirely.
    Quit,
}

/// Sprint 56 Track H3 (#525 item 16): cap on the token re-enter
/// loop. Past this many bad-format / verify-failure attempts, the
/// operator is quietly nudged toward Skip rather than allowed to
/// keep retrying indefinitely. 3 attempts mirrors the existing
/// SERVER_RATE_LIMIT_MAX_RETRIES convention.
const MAX_TOKEN_RETRIES: u32 = 3;

/// Full Telegram setup flow — BotFather → token → group detection.
fn telegram_setup(_home: &Path) -> anyhow::Result<(String, Option<i64>, Option<i64>)> {
    println!("  ── Telegram Setup ──\n");
    println!("  1. Open Telegram, talk to @BotFather");
    println!("  2. Send /newbot and follow instructions");
    println!("  3. Copy the bot token\n");

    // Sprint 56 Track H3 (#525 items 7 + 16 + 17): wrap the token-
    // acquisition path in a retry loop. The operator gets up to
    // MAX_TOKEN_RETRIES re-enter attempts before the loop nudges
    // toward Skip; bad format and verify_bot failures both route
    // through the same `TokenChoice` prompt so the UX stays
    // consistent.
    let mut attempt = 0_u32;
    let token = loop {
        attempt += 1;
        let token = prompt("  Bot token (Enter to skip): ")?;
        let token = token.trim().to_string();
        if token.is_empty() {
            println!("\n  Skipping Telegram. Configure later in fleet.yaml.\n");
            return Ok((String::new(), None, None));
        }

        if !is_valid_token_format(&token) {
            // Item 7: prompt instead of silent-continue.
            println!("  ⚠ Token format looks wrong (expected <digits>:<35+ chars>).");
            match prompt_token_choice(attempt)? {
                TokenChoice::ReEnter => continue,
                TokenChoice::Skip => {
                    println!("\n  Skipping Telegram. Configure later in fleet.yaml.\n");
                    return Ok((String::new(), None, None));
                }
                TokenChoice::Continue => {
                    // fall through to verify_bot — operator may have a
                    // legitimate format the matcher rejects.
                }
            }
        }

        print!("  Verifying bot... ");
        io::stdout().flush().ok();
        match verify_bot(&token) {
            Ok(bot_name) => {
                println!("✓ @{bot_name}\n");
                break token;
            }
            Err(e) => {
                // Item 17: short-circuit instead of silent-continue
                // into a 3-minute getUpdates poll on an unverified
                // token. The same `TokenChoice` prompt drives the
                // recovery — Re-enter loops back, Skip exits, Continue
                // proceeds anyway (escape hatch for transient
                // verify_bot failures).
                println!("⚠ {e}");
                match prompt_token_choice(attempt)? {
                    TokenChoice::ReEnter => continue,
                    TokenChoice::Skip => {
                        println!("\n  Skipping Telegram. Configure later in fleet.yaml.\n");
                        return Ok((String::new(), None, None));
                    }
                    TokenChoice::Continue => break token,
                }
            }
        }
    };

    println!("  Add the bot to your Telegram group (as admin).");
    println!("  Then send any message in the group.\n");

    // Sprint 56 Track H3 (#525 item 6): wrap detect_group in a
    // post-timeout Retry/Skip/Quit prompt instead of silently
    // falling through to "set group_id manually".
    loop {
        print!("  Waiting for group message (3 min timeout)... ");
        io::stdout().flush().ok();
        match detect_group(&token) {
            Ok((group_id, group_title, sender)) => {
                println!("✓ {group_title} ({group_id})\n");
                // Sprint 56 Track H2 (#525 item 3): verify the bot is
                // admin in the captured group. Topic mode (the only mode
                // we write — see `generate_fleet_yaml`) calls
                // `bot.create_forum_topic`, which requires admin. Without
                // this pre-check, a non-admin bot proceeds through
                // quickstart silently; bootstrap then fails with
                // `tracing::error!` on first topic-create attempt and
                // silently continues. Operator only finds out when
                // notifications never arrive. Warn-loud non-fatal — the
                // operator may add the bot as admin later and re-run.
                match verify_bot_is_admin(&token, group_id) {
                    Ok(true) => println!("  ✓ Bot has admin in group\n"),
                    Ok(false) => println!(
                        "  ⚠ Bot is NOT admin in group — topic mode requires admin. \
                         Add the bot as admin in Telegram group settings, then \
                         re-run quickstart or restart the daemon. Continuing for \
                         now…\n"
                    ),
                    Err(e) => {
                        println!("  ⚠ Could not verify admin status: {e} — continuing anyway\n")
                    }
                }
                return Ok((token, Some(group_id), sender));
            }
            Err(e) => {
                println!("timeout: {e}");
                match prompt_timeout_choice()? {
                    TimeoutChoice::Retry => {
                        println!();
                        continue;
                    }
                    TimeoutChoice::Skip => {
                        println!("\n  Set group_id manually in fleet.yaml later.\n");
                        return Ok((token, None, None));
                    }
                    TimeoutChoice::Quit => {
                        anyhow::bail!("quickstart aborted by operator after group-detect timeout");
                    }
                }
            }
        }
    }
}

/// Sprint 56 Track H3 (#525 item 7): pure check for the Bot API
/// token shape `<digits>:<alphanumeric+_-, ≥30 chars>`. Extracted
/// from the previous inline check in `telegram_setup` so the format
/// policy can be unit-tested without entering the prompt loop.
fn is_valid_token_format(token: &str) -> bool {
    token.split_once(':').is_some_and(|(num, rest)| {
        num.len() >= 8
            && num.chars().all(|c| c.is_ascii_digit())
            && rest.len() >= 30
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Sprint 56 Track H3 (#525 items 7 + 16 + 17): prompt the operator
/// to choose how to recover from a bad token format / verify
/// failure. Reads stdin once via [`prompt`]; the answer is parsed
/// through [`parse_token_choice`] (pure helper) so the response
/// matrix is unit-testable without stdin.
///
/// `attempt_number` carries the 1-indexed retry count so the prompt
/// can nudge toward Skip past `MAX_TOKEN_RETRIES`.
fn prompt_token_choice(attempt_number: u32) -> anyhow::Result<TokenChoice> {
    let nudge = if attempt_number >= MAX_TOKEN_RETRIES {
        format!(" (attempt {attempt_number}/{MAX_TOKEN_RETRIES} — Skip recommended)")
    } else {
        String::new()
    };
    let raw = prompt(&format!(
        "  [R]e-enter token / [S]kip telegram / [C]ontinue anyway?{nudge}: "
    ))?;
    Ok(parse_token_choice(&raw))
}

/// Pure parser: classify operator's `R`/`S`/`C` answer (case-
/// insensitive). Anything unrecognized defaults to `Skip` — the
/// safest choice when the operator gave an ambiguous answer (skip
/// rather than continue with an unverified token).
fn parse_token_choice(input: &str) -> TokenChoice {
    match input.trim().to_ascii_lowercase().as_str() {
        "r" | "re-enter" | "reenter" => TokenChoice::ReEnter,
        "c" | "continue" => TokenChoice::Continue,
        // Default (including empty / "s" / "skip" / unrecognized) →
        // Skip. The empty-Enter default deliberately sends the
        // operator down the safe path.
        _ => TokenChoice::Skip,
    }
}

/// Sprint 56 Track H3 (#525 item 6): prompt the operator to choose
/// how to recover from a 3-minute getUpdates poll timeout. Same
/// shape as [`prompt_token_choice`]; parses through
/// [`parse_timeout_choice`].
fn prompt_timeout_choice() -> anyhow::Result<TimeoutChoice> {
    let raw = prompt("  [R]etry / [S]kip telegram for now / [Q]uit: ")?;
    Ok(parse_timeout_choice(&raw))
}

/// Pure parser for the timeout-choice prompt. Anything unrecognized
/// defaults to `Skip` so the operator's setup completes (with
/// `group_id` left for hand-fill) rather than aborts on an ambiguous
/// answer.
fn parse_timeout_choice(input: &str) -> TimeoutChoice {
    match input.trim().to_ascii_lowercase().as_str() {
        "r" | "retry" => TimeoutChoice::Retry,
        "q" | "quit" => TimeoutChoice::Quit,
        _ => TimeoutChoice::Skip,
    }
}

/// Sprint 56 Track H2 (#525 item 3): verify the bot has admin status
/// in the given chat by calling `getMe` to learn the bot's user_id,
/// then `getChatMember` to read the bot's status. Returns `Ok(true)`
/// for `"administrator"` / `"creator"` (admin equivalents per Bot
/// API), `Ok(false)` for other statuses, `Err` only when the API
/// calls themselves fail (network, malformed response). The Err arm
/// distinguishes "we don't know" from "we know it's not admin"; the
/// caller surfaces both with different operator hints.
fn verify_bot_is_admin(token: &str, chat_id: i64) -> anyhow::Result<bool> {
    run_async(async {
        let me = bot_api_get(token, "getMe").await?;
        let bot_id = me["result"]["id"].as_i64().ok_or_else(|| {
            anyhow::anyhow!("getMe response missing result.id — cannot resolve bot user_id")
        })?;
        let resp = bot_api_get(
            token,
            &format!("getChatMember?chat_id={chat_id}&user_id={bot_id}"),
        )
        .await?;
        if resp["ok"].as_bool() != Some(true) {
            anyhow::bail!(
                "getChatMember failed: {}",
                resp["description"].as_str().unwrap_or("unknown error")
            );
        }
        let status = resp["result"]["status"].as_str().unwrap_or("");
        Ok(is_bot_admin_status(status))
    })
}

/// Pure helper: classify a Telegram chat-member `status` string into
/// admin-vs-non-admin. Extracted from [`verify_bot_is_admin`] so the
/// policy can be unit-tested without HTTP. Per Bot API docs, only
/// `"administrator"` and `"creator"` carry admin permissions; every
/// other status (member / restricted / left / kicked) is non-admin.
fn is_bot_admin_status(status: &str) -> bool {
    matches!(status, "administrator" | "creator")
}

/// Telegram Bot API host. Kept as a const, SEPARATE from the `/bot<token>/…`
/// path, so the token interpolation lives in exactly ONE place ([`bot_api_get`])
/// instead of being spelled out at every call site.
const TELEGRAM_API: &str = "https://api.telegram.org";

/// GET a Telegram Bot API method and parse the JSON body.
///
/// SECURITY (CR-2026-06-14): the bot token is part of the Bot API URL path
/// (Telegram has no header auth, unlike the daemon's GitHub path). reqwest's
/// `Error` `Display` echoes the full request URL on a transport failure and
/// redacts only userinfo, NOT path segments — so a bare `reqwest::get(&url)`
/// whose error is printed (`println!("{e}")`) leaks the token to the terminal /
/// logs / pasted bug reports on ANY network error. We strip the URL from every
/// error with [`reqwest::Error::without_url`] BEFORE it propagates, so the token
/// never reaches a surfaced message. Centralising the four call sites here is
/// what removes the inline `bot<token>` URL templates.
async fn bot_api_get(token: &str, method: &str) -> anyhow::Result<serde_json::Value> {
    let url = format!("{TELEGRAM_API}/bot{token}/{method}");
    let resp = reqwest::get(&url).await.map_err(|e| e.without_url())?;
    let json = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| e.without_url())?;
    Ok(json)
}

/// Run a one-shot async Bot API call on a fresh current-thread runtime.
/// Extracted from three near-identical `Builder::new_current_thread()` +
/// `block_on` call sites (`verify_bot`, `verify_bot_is_admin`,
/// `detect_group`) — same behavior, one place to get the runtime setup right.
fn run_async<T>(fut: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(fut)
}

fn mask_token(tok: &str) -> String {
    if tok.len() > 8 {
        // Char-boundary-safe prefix/suffix: a raw `&tok[..4]` / `&tok[len-4..]`
        // panics when byte index 4 (or len-4) splits a multibyte UTF-8 char
        // (e.g. a CJK token). Clamp to the nearest char boundary instead.
        let prefix_end = (1..=4)
            .rev()
            .find(|&i| tok.is_char_boundary(i))
            .unwrap_or(0);
        let suffix_start = (tok.len().saturating_sub(4)..tok.len())
            .find(|&i| tok.is_char_boundary(i))
            .unwrap_or(tok.len());
        format!("{}...{}", &tok[..prefix_end], &tok[suffix_start..])
    } else {
        "****".to_string()
    }
}

fn detect_backends() -> Vec<Backend> {
    Backend::all()
        .iter()
        .filter(|b| b.is_installed())
        .cloned()
        .collect()
}

fn prompt(msg: &str) -> anyhow::Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input)
}

fn verify_bot(token: &str) -> anyhow::Result<String> {
    run_async(async {
        let resp = bot_api_get(token, "getMe").await?;
        if resp["ok"].as_bool() == Some(true) {
            let username = resp["result"]["username"].as_str().unwrap_or("unknown");
            Ok(username.to_string())
        } else {
            anyhow::bail!(
                "Invalid token: {}",
                resp["description"].as_str().unwrap_or("unknown error")
            )
        }
    })
}

/// Classify a single `chat` payload from `getUpdates` against the
/// quickstart's "topic mode" requirement. Pure function — pulled out
/// of `detect_group` so the chat-type policy can be unit-tested
/// without HTTP. Returns:
///   - `Ok(Some((id, title)))` for an accepted supergroup
///   - `Err(...)` for an explicit reject (regular group; Topics required
///     but the chat hasn't been upgraded yet — issue #523 first half)
///   - `Ok(None)` for irrelevant chat types (private, channel, etc.)
///     — keep scanning the update stream for a matching chat.
fn classify_quickstart_chat(chat: &serde_json::Value) -> anyhow::Result<Option<(i64, String)>> {
    let chat_type = chat["type"].as_str().unwrap_or("");
    let title = chat["title"].as_str().unwrap_or("Unknown Group");
    match chat_type {
        "supergroup" => {
            let id = chat["id"].as_i64().unwrap_or(0);
            Ok(Some((id, title.to_string())))
        }
        "group" => anyhow::bail!(
            "Group '{title}' is a regular group, but topic mode requires a \
             supergroup. Open the group settings in Telegram and enable Topics \
             (this upgrades the group to a supergroup), then send another message \
             and re-run quickstart."
        ),
        _ => Ok(None),
    }
}

/// #2207 B: pick the operator's `user_id` from a `getUpdates` batch — but ONLY
/// when there is exactly ONE distinct sender across the polled messages. During
/// quickstart the operator is told to send a message to their group, so a single
/// sender is overwhelmingly the operator. We deliberately return `None` for 0 or
/// ≥2 distinct senders (e.g. someone else in the group messaged during the poll
/// window) so quickstart never auto-allowlists the wrong user — the detached
/// fail-fast (#2207 A1) then backstops the still-empty allowlist. Pure (no HTTP)
/// so the single-sender policy is unit-testable.
fn extract_single_sender(updates: &[serde_json::Value]) -> Option<i64> {
    let mut senders = std::collections::BTreeSet::new();
    for update in updates {
        if let Some(id) = update
            .get("message")
            .and_then(|m| m.get("from"))
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_i64())
        {
            senders.insert(id);
        }
    }
    if senders.len() == 1 {
        senders.into_iter().next()
    } else {
        None
    }
}

/// Returns `(group_id, group_title, sole_sender_user_id)`. The third element is
/// the #2207 B auto-fill candidate (see [`extract_single_sender`]) — `None`
/// unless exactly one sender appeared in the same poll.
fn detect_group(token: &str) -> anyhow::Result<(i64, String, Option<i64>)> {
    run_async(async {
        let resp = bot_api_get(
            token,
            "getUpdates?timeout=180&allowed_updates=[\"message\"]",
        )
        .await?;
        let Some(updates) = resp["result"].as_array() else {
            anyhow::bail!("No group message received")
        };
        let mut group: Option<(i64, String)> = None;
        for update in updates {
            if let Some(chat) = update.get("message").and_then(|m| m.get("chat")) {
                if let Some(hit) = classify_quickstart_chat(chat)? {
                    group = Some(hit);
                    break;
                }
            }
        }
        let (gid, title) = group.ok_or_else(|| anyhow::anyhow!("No group message received"))?;
        Ok((gid, title, extract_single_sender(updates)))
    })
}

fn generate_fleet_yaml(
    home: &Path,
    backend: &Backend,
    group_id: Option<i64>,
    _token: Option<&str>,
    // #2207 B: the sole sender detected during group detection, auto-filled
    // into `user_allowlist` so a fresh quickstart yields a usable channel
    // instead of the silently-dropping empty `[]`. `None` → emit the TODO
    // template (multi/zero sender, reused config, or unattended).
    user_id: Option<i64>,
    unattended: bool,
) -> anyhow::Result<()> {
    let fleet_path = crate::fleet::fleet_yaml_path(home);

    if fleet_path.exists() {
        // Check compatibility with existing config
        if let Ok(content) = std::fs::read_to_string(&fleet_path) {
            check_compatibility(&content, backend, group_id);
        }

        // Unattended: NEVER overwrite an existing fleet.yaml (same answer as
        // the interactive default `N` — destructive prompts fail safe), which
        // also makes unattended re-runs idempotent.
        if unattended {
            println!("  Keeping existing fleet.yaml (unattended never overwrites).\n");
            return Ok(());
        }
        let answer = prompt("  fleet.yaml already exists. Overwrite? (y/N): ")?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("  Keeping existing fleet.yaml.\n");
            return Ok(());
        }

        // Backup before overwriting
        let backup = home.join("fleet.yaml.bak");
        std::fs::copy(&fleet_path, &backup)?;
        println!("  ✓ Backed up to {}\n", backup.display());
    }

    let backend_name = backend.name();

    let channel_section = if let Some(gid) = group_id {
        // #2207 B: auto-fill the allowlist when quickstart detected exactly one
        // sender (assumed to be the operator — they were just told to message
        // the group). Otherwise emit the TODO `[]` template; the detached
        // fail-fast (#2207 A1) backstops a still-empty allowlist at start.
        let allowlist_line = match user_id {
            Some(uid) => format!(
                "user_allowlist:\n  - {uid}  # auto-detected by quickstart (sole sender during group detection)"
            ),
            None => {
                "user_allowlist: []  # add your Telegram user_id (message @userinfobot to get it)"
                    .to_string()
            }
        };
        format!(
            r#"
channel:
  type: telegram
  bot_token_env: AGEND_TELEGRAM_BOT_TOKEN
  group_id: {gid}
  mode: topic
  {allowlist_line}
"#
        )
    } else {
        "\n# channel:\n#   type: telegram\n#   bot_token_env: AGEND_TELEGRAM_BOT_TOKEN\n#   group_id: YOUR_GROUP_ID\n#   user_allowlist: [YOUR_USER_ID]\n".to_string()
    };

    let workspace_dir = crate::paths::workspace_dir(home).join("general");
    std::fs::create_dir_all(&workspace_dir)?;
    let working_dir = format!("    working_directory: {}", workspace_dir.display());

    let yaml = format!(
        r#"defaults:
  backend: {backend_name}
{channel_section}
instances:
  general:
    role: "General assistant"
{working_dir}
"#
    );

    std::fs::write(&fleet_path, &yaml)?;
    println!("  ✓ Generated {}\n", fleet_path.display());

    Ok(())
}

/// Canonical `.env` key quickstart writes for the Telegram bot token.
const TELEGRAM_TOKEN_KEY: &str = "AGEND_TELEGRAM_BOT_TOKEN";

/// Extract the token value from a `.env` line written as either the canonical
/// `AGEND_TELEGRAM_BOT_TOKEN=` or the legacy `AGEND_BOT_TOKEN=` key. Legacy is
/// still detected so operators who ran an older quickstart keep being
/// recognized (and get migrated to the canonical key on the next write). The
/// runtime read keeps its own separate legacy fallback — see
/// `channel::telegram::creds`.
fn extract_env_token(line: &str) -> Option<&str> {
    line.strip_prefix("AGEND_TELEGRAM_BOT_TOKEN=")
        .or_else(|| line.strip_prefix("AGEND_BOT_TOKEN="))
        .map(str::trim)
}

/// True if a `.env` line carries the bot token under either the canonical or
/// the legacy key (used to strip both before rewriting the canonical one).
fn is_telegram_token_line(line: &str) -> bool {
    line.starts_with("AGEND_TELEGRAM_BOT_TOKEN=") || line.starts_with("AGEND_BOT_TOKEN=")
}

/// Save the Telegram bot token to .env under the canonical
/// `AGEND_TELEGRAM_BOT_TOKEN` key, preserving other variables (and migrating
/// any legacy `AGEND_BOT_TOKEN` line out).
fn save_env_token(home: &Path, token: &str, unattended: bool) -> anyhow::Result<()> {
    let env_path = home.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let existing_token = existing.lines().find_map(extract_env_token);

    if let Some(old) = existing_token {
        if old == token {
            println!("  ✓ Token unchanged in .env\n");
            return Ok(());
        }
        println!("  .env already has a bot token: {}", mask_token(old));
        // Unattended: the differing token can only have come from the
        // AGEND_TELEGRAM_BOT_TOKEN env var (resolve_unattended_telegram's
        // .env arm is by definition equal to `old`) — an explicit
        // per-invocation instruction, e.g. a CI token rotation. Update
        // without prompting.
        if unattended {
            println!("  ✓ Updating from AGEND_TELEGRAM_BOT_TOKEN env (explicit instruction)");
        } else {
            // Sprint 56 Track H4 (#525 item 14): destructive prompts default
            // to `N` (preserve operator data); non-destructive prompts
            // default to `Y` (the convenient path). Updating an existing
            // token overwrites stored credentials → destructive →
            // (y/N). Only an explicit `y` proceeds; Enter/N/anything-else
            // keeps the current token.
            let answer = prompt("  Update token? (y/N): ")?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                println!("  Keeping existing token.\n");
                return Ok(());
            }
        }
    }

    // Sprint 56 Track H1 (#525 item 15): warn-loud non-fatal if the
    // home dir's `.gitignore` doesn't cover `.env`. Operators who put
    // `~/.agend/` inside a dotfiles repo would otherwise commit the
    // bot token by accident; the warn carries an operator-actionable
    // hint without blocking the write (some operators may have
    // intentional reasons to skip the check).
    if !gitignore_covers_env(home) {
        println!(
            "  ⚠ {} has no `.gitignore` entry covering `.env`. \
             Add `.env` to `.gitignore` to avoid committing the bot \
             token if this dir is under git.",
            home.display()
        );
    }

    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !is_telegram_token_line(l))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("{TELEGRAM_TOKEN_KEY}={token}"));
    std::fs::write(&env_path, lines.join("\n") + "\n")?;
    // Sprint 56 Track H1 (#525 item 4): chmod 0600 on Unix so the bot
    // token isn't world-readable (default umask 0022 produces 0644).
    // Windows has no equivalent in std without ACL dependencies; the
    // file inherits parent dir permissions there. Operators on Windows
    // get no automatic protection — `cargo` plus the issue body hint
    // ("`chmod 0600` on Unix; on Windows fall back to icacls or
    // document it") is the documented escape. We log a debug line so
    // a curious operator running RUST_LOG=debug can see what we did.
    apply_secret_file_permissions(&env_path);
    println!("  ✓ Token saved to {}\n", env_path.display());
    Ok(())
}

/// Sprint 56 Track H1 (#525 item 4): set the file at `path` to the
/// "owner-only read/write" mode (0600) on Unix. Best-effort —
/// permission-set failures are logged at warn level but don't fail
/// the caller because the file write itself already succeeded; a
/// botched chmod is a security-degraded state worth surfacing but
/// not worth aborting the operator's setup over.
///
/// Windows: no-op. `std::fs::set_permissions` on Windows only toggles
/// the read-only bit, which is not what 0600 means; proper ACL
/// restriction would require a Windows-specific dependency
/// (`windows-sys` ACL APIs or the `icacls` shell out the issue body
/// suggested). Documented in the call site as a known platform
/// asymmetry until a follow-up addresses Windows specifically.
fn apply_secret_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => tracing::debug!(
                path = %path.display(),
                "applied 0600 permissions to secret file"
            ),
            Err(e) => tracing::warn!(
                %e,
                path = %path.display(),
                "failed to chmod 0600 — secret file may be world-readable"
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        tracing::debug!(
            "secret file permission tightening skipped: non-Unix platform — \
             consider using icacls or equivalent ACL tool to restrict access"
        );
    }
}

/// Sprint 56 Track H1 (#525 item 15): true iff `<home>/.gitignore` —
/// or any ancestor directory's `.gitignore` up to the enclosing repo
/// root — contains a line that would cover `.env`.
///
/// Why walk parents: operators frequently put `~/.agend/` inside a
/// dotfiles repo whose `.gitignore` lives at the repo root, not in
/// the home subdir. Reading only the home gitignore would warn
/// spuriously when the parent already covers `.env`. The walk stops
/// at the first `.git/` (repo root marker), the filesystem root, or
/// after [`MAX_GITIGNORE_DEPTH`] parents — whichever comes first.
///
/// Matching is conservative: we look for a non-comment, non-blank,
/// non-negation line that, after trimming, is one of `.env`,
/// `*.env`, `.env*`, `**/.env`, or `/.env`. **Negation lines (`!.env`
/// and any `!`-prefixed line) do NOT contribute to coverage** —
/// `.gitignore`'s `!pattern` means "un-ignore", so `!.env` actively
/// removes protection rather than adding it. The conservative
/// interpretation: negation existence at all → no satisfying signal,
/// fall through to warn so the operator double-checks.
fn gitignore_covers_env(home: &Path) -> bool {
    let mut current = Some(home.to_path_buf());
    let mut depth = 0_usize;
    while let Some(dir) = current {
        if depth > MAX_GITIGNORE_DEPTH {
            break;
        }
        if scan_gitignore(&dir.join(".gitignore")) {
            return true;
        }
        // Repo-root boundary: stop AFTER scanning this dir's gitignore
        // because the repo-root `.gitignore` is the one operators most
        // commonly use.
        if dir.join(".git").exists() {
            break;
        }
        current = dir.parent().map(|p| p.to_path_buf());
        depth += 1;
    }
    false
}

/// Sprint 56 Track H1 fixup (reviewer m-20260508115855791834-150): cap
/// on how far up the directory tree `gitignore_covers_env` walks. 5
/// levels is a sane default — covers `~/.agend/` inside a dotfiles
/// repo (1 level), or `~/.agend/<deep-nested>/` shapes — without
/// scanning the entire filesystem when the operator runs quickstart
/// in a non-repo location. The walk also halts at any `.git/`
/// directory, so this depth limit is the safety net rather than the
/// primary stop condition.
const MAX_GITIGNORE_DEPTH: usize = 5;

/// Scan a single `.gitignore` file for env-covering patterns. Pure
/// helper — the walk above composes calls to this. Returns `true` iff
/// at least one non-comment, non-blank, non-negation line matches a
/// canonical env-covering shape.
fn scan_gitignore(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|raw| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        // Negation lines actively un-ignore — do NOT count as coverage.
        if line.starts_with('!') {
            return false;
        }
        matches_env_pattern(line)
    })
}

/// True for the .gitignore patterns that cover `.env` in the home
/// root. Extracted as a pure helper so tests can pin the matcher
/// independent of file IO. See [`gitignore_covers_env`] for the list
/// of accepted shapes; anything outside that list is intentionally
/// rejected so the warn path fires when the gitignore uses an
/// exotic pattern that the operator should double-check.
fn matches_env_pattern(line: &str) -> bool {
    matches!(line, ".env" | "*.env" | ".env*" | "**/.env" | "/.env")
}

fn check_compatibility(yaml_content: &str, new_backend: &Backend, new_group_id: Option<i64>) {
    if let Ok(config) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml_content) {
        // Check backend
        let existing_backend = config["defaults"]["backend"].as_str().unwrap_or("");
        if !existing_backend.is_empty() && existing_backend != new_backend.name() {
            println!(
                "  ⚠ Existing backend: {existing_backend}, new: {}",
                new_backend.name()
            );
        }

        // Check group_id
        if let Some(new_gid) = new_group_id {
            let existing_gid = config["channel"]["group_id"].as_i64().unwrap_or(0);
            if existing_gid != 0 && existing_gid != new_gid {
                println!("  ⚠ Existing group_id: {existing_gid}, new: {new_gid}");
            }
        }

        // Check instance count
        if let Some(instances) = config["instances"].as_mapping() {
            if instances.len() > 1 {
                println!(
                    "  ⚠ Existing config has {} instances (new config will have 1)",
                    instances.len()
                );
            }
        }
    }
}

fn print_next_steps(home: &Path) {
    // Sprint 56 Track H4 (#525 item 12): pre-Track-H4 the Next Steps
    // block jumped straight to `agend-terminal start` without any
    // mention of the three first-day pitfalls operators hit (#525
    // items 1, 2, 3). The "Before you start" block surfaces them
    // up front so an operator scanning the output catches the
    // gotchas before the silent-drop fallout starts.
    println!("  ── Before you start ──\n");
    println!("  Three first-day gotchas to double-check (see issue #525):");
    println!("    1. `user_allowlist` must list YOUR Telegram user_id —");
    println!("       an empty list (`[]`) silently drops every reply.");
    println!("    2. The bot must be admin in your Telegram group —");
    println!("       topic mode requires admin to call create_forum_topic.");
    println!("    3. Topic mode requires a SUPERGROUP — enabling Topics");
    println!("       on a regular group migrates it to a supergroup with a");
    println!("       new id; quickstart now refuses regular groups upfront.\n");
    println!("  ── Next Steps ──\n");
    println!("  Pick how to run your fleet:\n");
    // #2204: lead with App mode — the graphical TUI is the recommended first
    // path for newcomers (one screen for every agent's pane + status). The
    // daemon + CLI path is kept but demoted to its real audience (automation /
    // headless), so a first-time operator isn't dropped straight into
    // start/status/attach plumbing.
    println!("  [Recommended] New to agend, or want the whole fleet at a glance:");
    println!("      agend-terminal app");
    println!("      (a TUI dashboard: every agent's pane + live status in one screen)\n");
    println!("  [Advanced] Automation / headless / scripted (CI, remote, no TUI):");
    println!("      agend-terminal start            # launch the fleet daemon");
    println!("      agend-terminal status           # check agent status");
    println!("      agend-terminal attach general   # attach to one agent's pane\n");
    println!("  Add or edit instances anytime in fleet.yaml:");
    println!("     {}\n", crate::fleet::fleet_yaml_path(home).display());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

#[cfg(test)]
mod review_repro_panic_io_extra;
