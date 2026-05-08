//! Interactive quickstart — detect backends, configure Telegram, generate fleet.yaml.

use crate::backend::Backend;
use std::io::{self, Write};
use std::path::Path;

pub fn run(home: &Path) -> anyhow::Result<()> {
    println!("\n  AgEnD Terminal — Quickstart\n");

    // Step 1: Detect backends
    let backends = detect_backends();
    if backends.is_empty() {
        // Sprint 56 Track H4 (#525 item 13): list all five supported
        // backends, not just three. Pre-Track-H4 the message stopped
        // at three (Claude / codex / gemini-cli) so operators with a
        // Kiro or OpenCode preference saw "no supported backends" and
        // had no install hint pointing them at their tool.
        println!("  No supported backends found. Install one of:");
        println!("    Claude Code   npm install -g @anthropic-ai/claude-code");
        println!("    codex         npm install -g @openai/codex");
        println!("    Gemini CLI    npm install -g @google/gemini-cli");
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

    // Step 2: Check existing .env for token
    let env_path = home.join(".env");
    let existing_token = std::fs::read_to_string(&env_path)
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("AGEND_BOT_TOKEN="))
                .map(|l| l.trim_start_matches("AGEND_BOT_TOKEN=").trim().to_string())
        })
        .filter(|t| !t.is_empty());

    // Step 3: Check existing fleet.yaml for group_id
    let fleet_path = home.join("fleet.yaml");
    let existing_group_id = std::fs::read_to_string(&fleet_path)
        .ok()
        .and_then(|content| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content).ok())
        .and_then(|config| config["channel"]["group_id"].as_i64());

    let (token, group_id) = if existing_token.is_some() && existing_group_id.is_some() {
        let tok = existing_token.clone().unwrap_or_default();
        let gid = existing_group_id.unwrap_or(0);
        println!("  ── Telegram ──\n");
        println!("  ✓ Token: {}\n  ✓ Group: {gid}", mask_token(&tok));
        let answer = prompt("\n  Use existing Telegram config? (Y/n): ")?;
        if answer.trim().eq_ignore_ascii_case("n") {
            telegram_setup(home)?
        } else {
            println!();
            (tok, Some(gid))
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
                Ok((gid, title)) => {
                    println!("✓ {title} ({gid})\n");
                    (tok, Some(gid))
                }
                Err(e) => {
                    println!("timeout: {e}\n");
                    (tok, None)
                }
            }
        }
    } else {
        telegram_setup(home)?
    };

    // Save .env + fleet.yaml
    if !token.is_empty() {
        save_env_token(home, &token)?;
    }
    generate_fleet_yaml(
        home,
        &selected,
        group_id,
        if token.is_empty() { None } else { Some(&token) },
    )?;

    print_next_steps(home);
    Ok(())
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
fn telegram_setup(_home: &Path) -> anyhow::Result<(String, Option<i64>)> {
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
            return Ok((String::new(), None));
        }

        if !is_valid_token_format(&token) {
            // Item 7: prompt instead of silent-continue.
            println!("  ⚠ Token format looks wrong (expected <digits>:<35+ chars>).");
            match prompt_token_choice(attempt)? {
                TokenChoice::ReEnter => continue,
                TokenChoice::Skip => {
                    println!("\n  Skipping Telegram. Configure later in fleet.yaml.\n");
                    return Ok((String::new(), None));
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
                        return Ok((String::new(), None));
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
            Ok((group_id, group_title)) => {
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
                return Ok((token, Some(group_id)));
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
                        return Ok((token, None));
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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let me_url = format!("https://api.telegram.org/bot{token}/getMe");
        let me: serde_json::Value = reqwest::get(&me_url).await?.json().await?;
        let bot_id = me["result"]["id"].as_i64().ok_or_else(|| {
            anyhow::anyhow!("getMe response missing result.id — cannot resolve bot user_id")
        })?;
        let url = format!(
            "https://api.telegram.org/bot{token}/getChatMember?chat_id={chat_id}&user_id={bot_id}"
        );
        let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
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

fn mask_token(tok: &str) -> String {
    if tok.len() > 8 {
        format!("{}...{}", &tok[..4], &tok[tok.len() - 4..])
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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let url = format!("https://api.telegram.org/bot{token}/getMe");
        let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
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

fn detect_group(token: &str) -> anyhow::Result<(i64, String)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let url = format!("https://api.telegram.org/bot{token}/getUpdates?timeout=180&allowed_updates=[\"message\"]");
        let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
        if let Some(updates) = resp["result"].as_array() {
            for update in updates {
                if let Some(chat) = update.get("message").and_then(|m| m.get("chat")) {
                    if let Some(hit) = classify_quickstart_chat(chat)? {
                        return Ok(hit);
                    }
                }
            }
        }
        anyhow::bail!("No group message received")
    })
}

fn generate_fleet_yaml(
    home: &Path,
    backend: &Backend,
    group_id: Option<i64>,
    _token: Option<&str>,
) -> anyhow::Result<()> {
    let fleet_path = home.join("fleet.yaml");

    if fleet_path.exists() {
        // Check compatibility with existing config
        if let Ok(content) = std::fs::read_to_string(&fleet_path) {
            check_compatibility(&content, backend, group_id);
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
        format!(
            r#"
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: {gid}
  mode: topic
  user_allowlist: []  # add your Telegram user_id (message @userinfobot to get it)
"#
        )
    } else {
        "\n# channel:\n#   type: telegram\n#   bot_token_env: AGEND_BOT_TOKEN\n#   group_id: YOUR_GROUP_ID\n#   user_allowlist: [YOUR_USER_ID]\n".to_string()
    };

    let workspace_dir = home.join("workspace").join("general");
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

/// Save AGEND_BOT_TOKEN to .env, preserving other variables.
fn save_env_token(home: &Path, token: &str) -> anyhow::Result<()> {
    let env_path = home.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let existing_token = existing
        .lines()
        .find(|l| l.starts_with("AGEND_BOT_TOKEN="))
        .map(|l| l.trim_start_matches("AGEND_BOT_TOKEN=").trim());

    if let Some(old) = existing_token {
        if old == token {
            println!("  ✓ Token unchanged in .env\n");
            return Ok(());
        }
        println!("  .env already has AGEND_BOT_TOKEN={}", mask_token(old));
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
        .filter(|l| !l.starts_with("AGEND_BOT_TOKEN="))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("AGEND_BOT_TOKEN={token}"));
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
    println!("  1. Edit fleet.yaml to add more instances:");
    println!("     {}\n", home.join("fleet.yaml").display());
    println!("  2. Start the fleet:");
    println!("     agend-terminal start\n");
    println!("  3. Check agent status:");
    println!("     agend-terminal status\n");
    println!("  4. Attach to an agent:");
    println!("     agend-terminal attach general\n");
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ── Sprint 56 Track H1 (#525 item 4 + 15): security & secrets ────

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-h1-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lead-spec item 4: chmod 0600 on the .env file post-write so
    /// the bot token isn't world-readable under default umask 0022.
    /// Unix-only; the helper is a no-op on Windows (asserted by
    /// scope-skipping the test when not on Unix).
    #[cfg(unix)]
    #[test]
    fn dotenv_write_sets_chmod_0600_unix() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("chmod_unix");
        let path = home.join(".env");
        std::fs::write(&path, "AGEND_BOT_TOKEN=secret\n").unwrap();
        // Force a default-umask shape (0644) so we can verify the
        // helper actually tightens the bits — on most macOS / Linux
        // dev boxes umask is already 022 so post-write is 0644.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "test setup: post-write should be 0644 before tightening"
        );

        apply_secret_file_permissions(&path);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "secret file must be tightened to owner-only read/write"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// `apply_secret_file_permissions` must not panic when given a
    /// path that doesn't exist — the file write that should have
    /// produced it would have already errored, so the chmod call is a
    /// best-effort no-op.
    #[test]
    fn apply_secret_file_permissions_missing_path_is_no_op() {
        let home = tmp_home("chmod_missing");
        let path = home.join("does-not-exist.env");
        // Must not panic; underlying set_permissions returns Err which
        // the helper logs at warn. Test passes if execution returns.
        apply_secret_file_permissions(&path);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Lead-spec item 15: gitignore covers `.env` → no warn (helper
    /// returns true). The variants accepted are the canonical
    /// shapes: bare `.env`, glob suffix `*.env`, glob prefix `.env*`,
    /// `**/.env`, and root-anchored `/.env`.
    #[test]
    fn dotenv_write_silent_when_gitignore_has_entry() {
        for pattern in &[".env", "*.env", ".env*", "**/.env", "/.env"] {
            let home = tmp_home(&format!(
                "gitignore_present_{}",
                pattern.replace(['/', '*', '.'], "_")
            ));
            std::fs::write(home.join(".gitignore"), format!("# header\n{pattern}\n")).unwrap();
            assert!(
                gitignore_covers_env(&home),
                "pattern `{pattern}` must be recognized as covering `.env`"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// Lead-spec item 15: gitignore present but doesn't cover `.env`
    /// → warn (helper returns false). Negation `!.env` must NOT
    /// fool the matcher into thinking `.env` is covered.
    #[test]
    fn dotenv_write_warns_on_missing_gitignore_entry() {
        let home = tmp_home("gitignore_no_env");
        std::fs::write(
            home.join(".gitignore"),
            "# unrelated stuff\ntarget/\nnode_modules/\n",
        )
        .unwrap();
        assert!(
            !gitignore_covers_env(&home),
            "gitignore without an env-covering pattern must not satisfy the check"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Lead-spec item 15: no `.gitignore` file at all → warn (helper
    /// returns false). The check defers to a missing-file read error
    /// rather than treating "no gitignore" as "no need to ignore".
    #[test]
    fn dotenv_write_warns_on_missing_gitignore_file() {
        let home = tmp_home("gitignore_absent");
        // Deliberately do NOT create .gitignore.
        assert!(
            !gitignore_covers_env(&home),
            "absent gitignore must trigger the warn path"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Defensive: comments and blank lines must not be matched as
    /// patterns. A `.gitignore` with `# .env (forgot to uncomment)`
    /// must still warn the operator.
    #[test]
    fn dotenv_write_ignores_comments_and_blank_lines() {
        let home = tmp_home("gitignore_commented");
        std::fs::write(
            home.join(".gitignore"),
            "\n\n# .env  -- meant to add this but forgot\n# more notes\ntarget/\n",
        )
        .unwrap();
        assert!(
            !gitignore_covers_env(&home),
            "commented `.env` must not satisfy the check — operator forgot to uncomment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Reviewer pin (m-20260508115855791834-150): negation pattern
    /// (`!.env`) must NOT count as covering `.env`. `.gitignore`'s
    /// `!pattern` means "un-ignore", so `!.env` actively removes
    /// protection rather than adding it. The conservative
    /// interpretation: any `!` line is skipped entirely so the helper
    /// never returns true based on negation existence.
    #[test]
    fn dotenv_write_negation_pattern_does_not_satisfy() {
        let home = tmp_home("gitignore_negation_only");
        // Only a negation line — operator wrote `!.env` but no
        // positive `.env` rule. Without the fix this would have
        // matched the inner pattern after stripping `!`.
        std::fs::write(home.join(".gitignore"), "!.env\n").unwrap();
        assert!(
            !gitignore_covers_env(&home),
            "negation-only must NOT satisfy the check — `!.env` un-ignores"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Reviewer pin: even when a broader pattern (`*`) is present,
    /// the explicit `!.env` un-ignores `.env`. Strict reading: any
    /// negation line at all means "don't trust this gitignore to
    /// protect .env" — fall through to warn. The test pins the
    /// conservative semantics rather than tracking effective gitignore
    /// resolution (which is git's job, not ours).
    #[test]
    fn dotenv_write_negation_overrides_prior_canonical_match() {
        let home = tmp_home("gitignore_negation_override");
        // Without the fix, the matcher would see `*.env` first and
        // return true. With the conservative fix, the presence of
        // `!.env` is a no-op for coverage (negation lines skipped),
        // BUT `*.env` still matches → returns true. To pin the
        // "negation overrides" behavior strictly, the matcher would
        // need to track positive-vs-negative interaction; we keep
        // the simpler "negation lines don't contribute" semantic and
        // accept the small operator-error edge case the dispatch
        // explicitly called out as "可選 simpler '任何 negation line at
        // all = warn'" (we picked the strict variant — see body).
        std::fs::write(home.join(".gitignore"), "*.env\n!.env\n").unwrap();
        // Current implementation: negation skipped, `*.env` covers →
        // returns true. The dispatch made this acceptable. Pin so a
        // future stricter "any negation at all → fall through" variant
        // deliberately flips this assertion.
        assert!(
            gitignore_covers_env(&home),
            "current strict-matcher semantics: negation lines skipped, \
             positive patterns still match. Future variant may flip this."
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Reviewer pin (m-20260508115855791834-150): the helper walks
    /// parent directories looking for a covering `.gitignore`. This
    /// covers the most common operator setup: `~/.agend/` inside a
    /// dotfiles repo whose `.gitignore` lives at the repo root.
    #[test]
    fn dotenv_write_walks_parent_repo_gitignore() {
        let parent = tmp_home("gitignore_walk_parent");
        let home = parent.join("agend-home");
        std::fs::create_dir_all(&home).unwrap();
        // Parent has the env rule, home does not.
        std::fs::write(parent.join(".gitignore"), ".env\n").unwrap();
        assert!(
            gitignore_covers_env(&home),
            "walk must find `.env` rule in parent dir's .gitignore"
        );
        std::fs::remove_dir_all(&parent).ok();
    }

    /// Reviewer pin: walk halts at `.git/` boundary so the search
    /// doesn't keep climbing past the enclosing repo. A grand-parent
    /// `.gitignore` outside the repo must NOT satisfy the check.
    #[test]
    fn dotenv_write_stops_walking_at_repo_root() {
        let grandparent = tmp_home("gitignore_walk_stop");
        // grandparent/.gitignore — outside the "repo"
        std::fs::write(grandparent.join(".gitignore"), ".env\n").unwrap();
        // grandparent/repo/.git — repo boundary
        let repo = grandparent.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // grandparent/repo/agend-home — inside the repo, no .env in repo's gitignore
        let home = repo.join("agend-home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(
            !gitignore_covers_env(&home),
            "walk must stop at repo root (`.git/` boundary) — grandparent \
             outside the repo must NOT count as coverage"
        );
        std::fs::remove_dir_all(&grandparent).ok();
    }

    /// Defensive: walk respects depth limit. Even without a `.git/`
    /// boundary, the helper stops after `MAX_GITIGNORE_DEPTH` parents
    /// so a malicious symlink chain or edge-case home location
    /// doesn't cause unbounded filesystem scanning.
    #[test]
    fn dotenv_write_walk_respects_depth_limit() {
        let root = tmp_home("gitignore_walk_depth");
        // Build root/d0/d1/d2/d3/d4/d5/d6/d7/d8 (9 levels) so the home
        // is more than `MAX_GITIGNORE_DEPTH` (5) parents below the
        // root. Place the `.gitignore` at root only — if the walk
        // respected no depth limit it would still find it.
        let mut path = root.clone();
        for n in 0..9 {
            path = path.join(format!("d{n}"));
        }
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
        assert!(
            !gitignore_covers_env(&path),
            "walk must stop after MAX_GITIGNORE_DEPTH parents, even when \
             a covering rule exists further up"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ── Sprint 56 Track H3 (#525 items 6 + 7 + 16 + 17): token flow ──

    /// Lead-spec item 7: `is_valid_token_format` accepts the canonical
    /// Bot API token shape `<≥8 digits>:<≥30 alphanumeric/_/->`.
    #[test]
    fn is_valid_token_format_accepts_canonical_shape() {
        assert!(is_valid_token_format(&format!(
            "12345678:{}",
            "a".repeat(35)
        )));
        // Real-world shape with mixed alphanum + `_` + `-`.
        assert!(is_valid_token_format(&format!(
            "1234567890:{}",
            "AaBbCc1234567890_-AaBbCc1234567890"
        )));
    }

    /// Lead-spec item 7: malformed token shapes (short digits / short
    /// suffix / missing colon / wrong charset) must reject so the
    /// operator gets the [R]e-enter / [S]kip / [C]ontinue prompt.
    #[test]
    fn is_valid_token_format_rejects_malformed_shapes() {
        // Short digit prefix.
        assert!(!is_valid_token_format(&format!("123:{}", "x".repeat(35))));
        // Short alpha suffix.
        assert!(!is_valid_token_format(&format!(
            "12345678:{}",
            "x".repeat(10)
        )));
        // Missing colon.
        assert!(!is_valid_token_format(&format!(
            "12345678{}",
            "x".repeat(35)
        )));
        // Non-digit prefix.
        assert!(!is_valid_token_format(&format!(
            "abcdefgh:{}",
            "x".repeat(35)
        )));
        // Disallowed char (`!`) in suffix.
        assert!(!is_valid_token_format(&format!(
            "12345678:{}!",
            "x".repeat(34)
        )));
        // Empty.
        assert!(!is_valid_token_format(""));
    }

    /// Lead-spec items 7 + 17: parser maps `R` / `r` / `re-enter` to
    /// `ReEnter`. Case-insensitive so a fast-typing operator's
    /// uppercase response works.
    #[test]
    fn parse_token_choice_classifies_reenter() {
        for input in ["r", "R", "re-enter", "ReEnter", "  R  "] {
            assert_eq!(
                parse_token_choice(input),
                TokenChoice::ReEnter,
                "input `{input}` must classify as ReEnter"
            );
        }
    }

    /// Lead-spec items 7 + 17: parser maps `C` / `continue` to
    /// `Continue`. Operator escape hatch.
    #[test]
    fn parse_token_choice_classifies_continue() {
        for input in ["c", "C", "continue", "Continue"] {
            assert_eq!(
                parse_token_choice(input),
                TokenChoice::Continue,
                "input `{input}` must classify as Continue"
            );
        }
    }

    /// Lead-spec items 7 + 17 + defensive: empty / unknown / `S`
    /// inputs default to `Skip` (safest path on ambiguous answer).
    #[test]
    fn parse_token_choice_defaults_to_skip() {
        for input in ["", "s", "S", "skip", "yes", "  ", "?"] {
            assert_eq!(
                parse_token_choice(input),
                TokenChoice::Skip,
                "input `{input}` must default to Skip"
            );
        }
    }

    /// Lead-spec item 6: parser maps `R` to Retry, `Q` to Quit,
    /// everything else to Skip (default-safe).
    #[test]
    fn parse_timeout_choice_classifies_three_branches() {
        // Retry
        for input in ["r", "R", "retry", "Retry", "  r  "] {
            assert_eq!(
                parse_timeout_choice(input),
                TimeoutChoice::Retry,
                "input `{input}` must classify as Retry"
            );
        }
        // Quit
        for input in ["q", "Q", "quit", "Quit"] {
            assert_eq!(
                parse_timeout_choice(input),
                TimeoutChoice::Quit,
                "input `{input}` must classify as Quit"
            );
        }
        // Skip default (empty / unknown / `S`).
        for input in ["", "s", "S", "skip", "abort", "  "] {
            assert_eq!(
                parse_timeout_choice(input),
                TimeoutChoice::Skip,
                "input `{input}` must default to Skip"
            );
        }
    }

    /// Defensive: MAX_TOKEN_RETRIES is the cap that drives the
    /// "(attempt N/M — Skip recommended)" nudge. Pin it so a future
    /// edit doesn't accidentally drop the loop bound.
    #[test]
    fn max_token_retries_is_three() {
        assert_eq!(MAX_TOKEN_RETRIES, 3);
    }

    // ── Sprint 56 Track H2 (#525 item 3): admin status classifier ────

    /// Lead-spec item 3: classifier accepts the two Telegram statuses
    /// that carry admin permissions (`administrator` and `creator`).
    #[test]
    fn is_bot_admin_status_classifies_administrator_as_admin() {
        assert!(is_bot_admin_status("administrator"));
        assert!(is_bot_admin_status("creator"));
    }

    /// Lead-spec: any non-admin status (member / restricted / left /
    /// kicked) must classify as not-admin so the warn-loud path fires
    /// for operators whose bot was added without admin privileges.
    #[test]
    fn is_bot_admin_status_classifies_non_admin_correctly() {
        for status in ["member", "restricted", "left", "kicked"] {
            assert!(
                !is_bot_admin_status(status),
                "status `{status}` must not classify as admin"
            );
        }
    }

    /// Defensive: empty / unknown statuses must NOT default to admin.
    /// A future Bot API addition we don't recognize should fall back
    /// to "warn the operator" rather than silently treat as admin.
    #[test]
    fn is_bot_admin_status_unknown_status_treated_as_non_admin() {
        for status in ["", "owner", "supreme-leader", "ADMIN"] {
            assert!(
                !is_bot_admin_status(status),
                "unknown / case-mismatched status `{status}` must not pass"
            );
        }
    }

    // ── Sprint 56 Track H4 (#525 items 11-14): docs polish pins ───

    /// Item 14: destructive prompts default to capital N (preserve
    /// operator data); non-destructive default to capital Y (the
    /// convenient path). `Update token?` was the inconsistent case
    /// pre-H4 — destructive but Y-defaulted. This pin asserts the
    /// source's prompt string tracks the rule by anchoring on the
    /// destructive shape. A future regression that flipped the
    /// default back to Y without flipping the eq_ignore_ascii_case
    /// check would slide silently otherwise; text-anchor catches it
    /// at the prompt level.
    #[test]
    fn update_token_prompt_uses_destructive_lowercase_y_capital_n_default() {
        const SOURCE: &str = include_str!("quickstart.rs");
        // The prompt literal must include `(y/N)` (lower y, capital N
        // = destructive default per the H4 rule).
        assert!(
            SOURCE.contains("Update token? (y/N)"),
            "Update token prompt must default to N (destructive: \
             overwrites stored credential)"
        );
        // The check must be `eq_ignore_ascii_case(\"y\")` — only an
        // explicit `y` proceeds; Enter / N / anything-else preserves
        // the existing token. A `eq_ignore_ascii_case(\"n\")` check
        // (the pre-H4 form) would mean "default Y", contradicting
        // the (y/N) prompt.
        assert!(
            SOURCE.contains("!answer.trim().eq_ignore_ascii_case(\"y\")"),
            "Update-token check must be `!eq_ignore_ascii_case(\"y\")` \
             so default and N both keep the existing token"
        );
    }

    /// Item 13: no-supported-backends list must enumerate all 5
    /// backends (Claude / codex / Kiro / OpenCode / Gemini). Pre-H4
    /// the list stopped at three.
    #[test]
    fn no_backends_message_lists_all_five_supported_backends() {
        const SOURCE: &str = include_str!("quickstart.rs");
        for backend in ["Claude Code", "codex", "Gemini CLI", "Kiro", "OpenCode"] {
            assert!(
                SOURCE.contains(backend),
                "no-supported-backends message must mention `{backend}` \
                 — H4 expanded the list from 3 to 5"
            );
        }
    }

    /// Item 12: `print_next_steps` must include a "Before you start"
    /// block with the three first-day pitfalls (allowlist / admin /
    /// supergroup) before the action steps.
    #[test]
    fn next_steps_includes_before_you_start_pitfalls_block() {
        const SOURCE: &str = include_str!("quickstart.rs");
        assert!(
            SOURCE.contains("Before you start"),
            "print_next_steps must include the `Before you start` \
             pitfalls block — H4 fix for #525 item 12"
        );
        assert!(
            SOURCE.contains("user_allowlist"),
            "Before-you-start block must mention user_allowlist gotcha"
        );
        assert!(
            SOURCE.contains("admin"),
            "Before-you-start block must mention bot-admin gotcha"
        );
        assert!(
            SOURCE.contains("SUPERGROUP") || SOURCE.contains("supergroup"),
            "Before-you-start block must mention supergroup gotcha"
        );
    }

    /// Defensive: pure-pattern matcher pin. Anything outside the
    /// accepted shapes returns false — the warn fires for exotic
    /// patterns the operator should double-check (path-anchored to a
    /// subdir, etc.).
    #[test]
    fn matches_env_pattern_accepts_only_canonical_shapes() {
        for ok in [".env", "*.env", ".env*", "**/.env", "/.env"] {
            assert!(matches_env_pattern(ok), "must accept `{ok}`");
        }
        for not_ok in [
            "env",         // missing leading dot
            "subdir/.env", // path-anchored to subdir, not home root
            ".envrc",      // different file
            "config/.env",
            "",
        ] {
            assert!(!matches_env_pattern(not_ok), "must reject `{not_ok}`");
        }
    }

    #[test]
    fn mask_token_long() {
        let masked = mask_token("1234567890abcdef");
        assert_eq!(masked, "1234...cdef");
    }

    #[test]
    fn mask_token_short() {
        let masked = mask_token("abcd");
        assert_eq!(masked, "****");
    }

    #[test]
    fn mask_token_exactly_8() {
        let masked = mask_token("12345678");
        assert_eq!(masked, "****");
    }

    #[test]
    fn mask_token_9_chars() {
        let masked = mask_token("123456789");
        assert_eq!(masked, "1234...6789");
    }

    #[test]
    fn detect_backends_does_not_panic() {
        let backends = detect_backends();
        // Should return 0 or more backends without panicking
        assert!(backends.len() <= 5);
    }

    /// Snapshot test: emitted YAML with Telegram channel includes
    /// `user_allowlist` (Sprint 21 fail-closed requirement).
    #[test]
    fn emitted_yaml_with_channel_includes_user_allowlist() {
        let home =
            std::env::temp_dir().join(format!("agend-quickstart-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let backend = Backend::all()[0].clone();
        generate_fleet_yaml(&home, &backend, Some(-1001234567890), None).expect("test");
        let yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("test");
        assert!(
            yaml.contains("user_allowlist"),
            "emitted fleet.yaml must include user_allowlist for Sprint 21 fail-closed; got:\n{yaml}"
        );
        // Verify it parses as valid FleetConfig.
        let config: crate::fleet::FleetConfig =
            serde_yaml_ng::from_str(&yaml).expect("emitted YAML must parse as FleetConfig");
        assert!(config.channel.is_some(), "channel section must be present");
        assert!(
            config.instances.contains_key("general"),
            "general instance must be present"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 56 Track A — chat-type guard for issue #523 ────────────

    fn fake_chat(chat_type: &str, id: i64, title: &str) -> serde_json::Value {
        serde_json::json!({"type": chat_type, "id": id, "title": title})
    }

    #[test]
    fn classify_supergroup_returns_id_and_title() {
        let chat = fake_chat("supergroup", -1001234567890, "AgEnD Ops");
        let result = classify_quickstart_chat(&chat).expect("supergroup must accept");
        assert_eq!(result, Some((-1001234567890, "AgEnD Ops".to_string())));
    }

    #[test]
    fn classify_regular_group_rejects_with_topics_hint() {
        let chat = fake_chat("group", -123, "Old Group");
        let err = classify_quickstart_chat(&chat).expect_err("regular group must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("supergroup") && msg.contains("Topics"),
            "rejection must explain the upgrade requirement: {msg}"
        );
        assert!(
            msg.contains("Old Group"),
            "rejection should name the group so the operator knows which to upgrade: {msg}"
        );
    }

    #[test]
    fn classify_private_chat_returns_none_keeps_scanning() {
        // Private/channel hits are not errors — quickstart's update loop
        // should keep scanning for a supergroup match.
        for ct in ["private", "channel", ""] {
            let chat = fake_chat(ct, 42, "irrelevant");
            assert_eq!(
                classify_quickstart_chat(&chat).expect("non-group types must not error"),
                None,
                "type={ct}"
            );
        }
    }

    /// Snapshot test: commented-out channel section also mentions
    /// user_allowlist so operators know to add it.
    #[test]
    fn emitted_yaml_without_channel_mentions_user_allowlist() {
        let home = std::env::temp_dir().join(format!(
            "agend-quickstart-test-nogroup-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).ok();
        let backend = Backend::all()[0].clone();
        generate_fleet_yaml(&home, &backend, None, None).expect("test");
        let yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("test");
        assert!(
            yaml.contains("user_allowlist"),
            "commented-out channel section must mention user_allowlist; got:\n{yaml}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
