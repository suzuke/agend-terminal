//! Interactive quickstart — detect backends, configure channel (Telegram/Discord), generate fleet.yaml.

use crate::backend::Backend;
use std::io::{self, Write};
use std::path::Path;

/// Which channel the user selected during quickstart.
enum ChannelChoice {
    Telegram { token: String, group_id: Option<i64> },
    Discord { token: String, guild_id: String },
    Skip,
}

pub fn run(home: &Path) -> anyhow::Result<()> {
    println!("\n  AgEnD Terminal — Quickstart\n");

    // Step 1: Detect backends
    let backends = detect_backends();
    if backends.is_empty() {
        println!("  No supported backends found. Install one of:");
        println!("    npm install -g @anthropic-ai/claude-code");
        println!("    npm install -g @anthropic-ai/codex");
        println!("    npm install -g @anthropic-ai/gemini-cli");
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

    // Step 2: Check existing config and select channel
    let channel = detect_existing_or_prompt(home)?;

    // Step 3: Save .env + fleet.yaml
    match &channel {
        ChannelChoice::Telegram { token, .. } if !token.is_empty() => {
            save_env_var(home, "AGEND_BOT_TOKEN", token)?;
        }
        ChannelChoice::Discord { token, .. } if !token.is_empty() => {
            save_env_var(home, "AGEND_DISCORD_TOKEN", token)?;
        }
        _ => {}
    }
    generate_fleet_yaml(home, &selected, &channel)?;

    print_next_steps(home);
    Ok(())
}

/// Check for existing Telegram or Discord config; if none, prompt user to choose.
fn detect_existing_or_prompt(home: &Path) -> anyhow::Result<ChannelChoice> {
    let env_path = home.join(".env");
    let env_content = std::fs::read_to_string(&env_path).unwrap_or_default();
    let fleet_path = home.join("fleet.yaml");
    let fleet_content = std::fs::read_to_string(&fleet_path).ok();
    let fleet_yaml = fleet_content
        .as_deref()
        .and_then(|c| serde_yaml::from_str::<serde_yaml::Value>(c).ok());

    // Check existing Telegram config
    let existing_tg_token = env_content
        .lines()
        .find(|l| l.starts_with("AGEND_BOT_TOKEN="))
        .map(|l| l.trim_start_matches("AGEND_BOT_TOKEN=").trim().to_string())
        .filter(|t| !t.is_empty());
    let existing_tg_group = fleet_yaml
        .as_ref()
        .and_then(|c| c["channel"]["group_id"].as_i64());

    // Check existing Discord config
    let existing_dc_token = env_content
        .lines()
        .find(|l| l.starts_with("AGEND_DISCORD_TOKEN="))
        .map(|l| l.trim_start_matches("AGEND_DISCORD_TOKEN=").trim().to_string())
        .filter(|t| !t.is_empty());
    let existing_dc_guild = fleet_yaml
        .as_ref()
        .and_then(|c| c["channel"]["guild_id"].as_str().map(String::from));

    // Existing Discord config?
    if let (Some(tok), Some(gid)) = (&existing_dc_token, &existing_dc_guild) {
        println!("  ── Discord ──\n");
        println!("  ✓ Token: {}\n  ✓ Guild: {gid}", mask_token(tok));
        let answer = prompt("\n  Use existing Discord config? (Y/n): ")?;
        if !answer.trim().eq_ignore_ascii_case("n") {
            println!();
            return Ok(ChannelChoice::Discord {
                token: tok.clone(),
                guild_id: gid.clone(),
            });
        }
    }

    // Existing Telegram config?
    if existing_tg_token.is_some() && existing_tg_group.is_some() {
        let tok = existing_tg_token.clone().unwrap_or_default();
        let gid = existing_tg_group.unwrap_or(0);
        println!("  ── Telegram ──\n");
        println!("  ✓ Token: {}\n  ✓ Group: {gid}", mask_token(&tok));
        let answer = prompt("\n  Use existing Telegram config? (Y/n): ")?;
        if !answer.trim().eq_ignore_ascii_case("n") {
            println!();
            return Ok(ChannelChoice::Telegram {
                token: tok,
                group_id: Some(gid),
            });
        }
    } else if let Some(tok) = existing_tg_token {
        println!("  ── Telegram ──\n");
        println!("  ✓ Token found: {}", mask_token(&tok));
        let answer = prompt("  Use existing token? (Y/n): ")?;
        if !answer.trim().eq_ignore_ascii_case("n") {
            println!("\n  Add the bot to your Telegram group and send a message.\n");
            print!("  Waiting for group message (3 min timeout)... ");
            io::stdout().flush().ok();
            match detect_group(&tok) {
                Ok((gid, title)) => {
                    println!("✓ {title} ({gid})\n");
                    return Ok(ChannelChoice::Telegram {
                        token: tok,
                        group_id: Some(gid),
                    });
                }
                Err(e) => {
                    println!("timeout: {e}\n");
                    return Ok(ChannelChoice::Telegram {
                        token: tok,
                        group_id: None,
                    });
                }
            }
        }
    }

    // No existing config (or user declined) — prompt for channel type
    channel_selection_prompt(home)
}

fn channel_selection_prompt(home: &Path) -> anyhow::Result<ChannelChoice> {
    // S1: warn if fleet.yaml already has a different channel type
    if let Ok(content) = std::fs::read_to_string(home.join("fleet.yaml")) {
        if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            if let Some(ch_type) = config["channel"]["type"].as_str() {
                println!("  ⚠ Existing channel config: type={ch_type}");
            }
        }
    }

    println!("  Select channel:");
    println!("    1. Telegram");
    println!("    2. Discord");
    println!("    3. Skip\n");
    let choice = prompt("  Choice [1-3]: ")?;
    match choice.trim() {
        "2" => discord_setup(),
        "3" => {
            println!("  Skipping channel. Configure later in fleet.yaml.\n");
            Ok(ChannelChoice::Skip)
        }
        _ => telegram_setup(),
    }
}

/// Full Telegram setup flow — BotFather → token → group detection.
fn telegram_setup() -> anyhow::Result<ChannelChoice> {
    println!("  ── Telegram Setup ──\n");
    println!("  1. Open Telegram, talk to @BotFather");
    println!("  2. Send /newbot and follow instructions");
    println!("  3. Copy the bot token\n");

    let token = prompt("  Bot token (Enter to skip): ")?;
    let token = token.trim().to_string();

    if token.is_empty() {
        println!("\n  Skipping Telegram. Configure later in fleet.yaml.\n");
        return Ok(ChannelChoice::Skip);
    }

    if !token.contains(':') {
        println!("  ⚠ Token format looks wrong. Continuing anyway.\n");
    }

    print!("  Verifying bot... ");
    io::stdout().flush().ok();
    match verify_telegram_bot(&token) {
        Ok(bot_name) => println!("✓ @{bot_name}\n"),
        Err(e) => println!("⚠ {e}\n"),
    }

    println!("  Add the bot to your Telegram group (as admin).");
    println!("  Then send any message in the group.\n");
    print!("  Waiting for group message (3 min timeout)... ");
    io::stdout().flush().ok();

    match detect_group(&token) {
        Ok((group_id, group_title)) => {
            println!("✓ {group_title} ({group_id})\n");
            Ok(ChannelChoice::Telegram {
                token,
                group_id: Some(group_id),
            })
        }
        Err(e) => {
            println!("timeout: {e}\n");
            println!("  Set group_id manually in fleet.yaml later.\n");
            Ok(ChannelChoice::Telegram {
                token,
                group_id: None,
            })
        }
    }
}

/// Discord setup flow — Developer Portal → token → guild verification.
fn discord_setup() -> anyhow::Result<ChannelChoice> {
    println!("  ── Discord Setup ──\n");
    println!("  1. Go to https://discord.com/developers/applications");
    println!("  2. Create application → Bot → copy token");
    println!("  3. Enable MESSAGE CONTENT intent");
    println!("  4. Invite bot to server with permissions:");
    println!("     Manage Channels, Send Messages, Read Message History, Add Reactions\n");

    let token = prompt("  Bot token (Enter to skip): ")?;
    let token = token.trim().to_string();

    if token.is_empty() {
        println!("\n  Skipping Discord. Configure later in fleet.yaml.\n");
        return Ok(ChannelChoice::Skip);
    }

    print!("  Verifying bot... ");
    io::stdout().flush().ok();
    match verify_discord_bot(&token) {
        Ok(bot_name) => println!("✓ {bot_name}\n"),
        Err(e) => println!("⚠ {e}\n"),
    }

    let guild_input = prompt("  Guild ID (right-click server → Copy Server ID, Enter to skip): ")?;
    let guild_id = guild_input.trim().to_string();

    if guild_id.is_empty() {
        println!("\n  Set guild_id manually in fleet.yaml later.\n");
        return Ok(ChannelChoice::Discord {
            token,
            guild_id: String::new(),
        });
    }

    // Verify guild
    print!("  Verifying guild... ");
    io::stdout().flush().ok();
    match verify_discord_guild(&token, &guild_id) {
        Ok(name) => println!("✓ {name}\n"),
        Err(e) => println!("⚠ {e}\n"),
    }

    Ok(ChannelChoice::Discord { token, guild_id })
}

fn mask_token(tok: &str) -> String {
    if tok.len() > 8 {
        format!("{}...{}", &tok[..4], &tok[tok.len() - 4..])
    } else {
        "****".to_string()
    }
}

pub fn detect_backends() -> Vec<Backend> {
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

fn verify_telegram_bot(token: &str) -> anyhow::Result<String> {
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

fn verify_discord_bot(token: &str) -> anyhow::Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::new();
        let resp: serde_json::Value = client
            .get("https://discord.com/api/v10/users/@me")
            .header("Authorization", format!("Bot {token}"))
            .send()
            .await?
            .json()
            .await?;
        match resp["username"].as_str() {
            Some(name) => Ok(format!(
                "{}#{}",
                name,
                resp["discriminator"].as_str().unwrap_or("0")
            )),
            None => anyhow::bail!(
                "Invalid token: {}",
                resp["message"].as_str().unwrap_or("unknown error")
            ),
        }
    })
}

fn verify_discord_guild(token: &str, guild_id: &str) -> anyhow::Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::new();
        let resp: serde_json::Value = client
            .get(format!("https://discord.com/api/v10/guilds/{guild_id}"))
            .header("Authorization", format!("Bot {token}"))
            .send()
            .await?
            .json()
            .await?;
        match resp["name"].as_str() {
            Some(name) => Ok(name.to_string()),
            None => anyhow::bail!(
                "Cannot access guild: {}",
                resp["message"].as_str().unwrap_or("unknown error")
            ),
        }
    })
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
                    let chat_type = chat["type"].as_str().unwrap_or("");
                    if chat_type == "supergroup" || chat_type == "group" {
                        let id = chat["id"].as_i64().unwrap_or(0);
                        let title = chat["title"].as_str().unwrap_or("Unknown Group").to_string();
                        return Ok((id, title));
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
    channel: &ChannelChoice,
) -> anyhow::Result<()> {
    let fleet_path = home.join("fleet.yaml");

    if fleet_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&fleet_path) {
            check_compatibility(&content, backend, channel);
        }

        let answer = prompt("  fleet.yaml already exists. Overwrite? (y/N): ")?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("  Keeping existing fleet.yaml.\n");
            return Ok(());
        }

        let backup = home.join("fleet.yaml.bak");
        std::fs::copy(&fleet_path, &backup)?;
        println!("  ✓ Backed up to {}\n", backup.display());
    }

    let backend_name = backend.name();
    let project_dir = detect_project_root();

    let channel_section = match channel {
        ChannelChoice::Telegram {
            group_id: Some(gid),
            ..
        } => format!(
            r#"
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: {gid}
  mode: topic
"#
        ),
        ChannelChoice::Telegram { group_id: None, .. } => {
            "\n# channel:\n#   type: telegram\n#   bot_token_env: AGEND_BOT_TOKEN\n#   group_id: YOUR_GROUP_ID\n".to_string()
        }
        ChannelChoice::Discord { guild_id, .. } if !guild_id.is_empty() => format!(
            r#"
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_TOKEN
  guild_id: "{guild_id}"
  # category_name: "AgEnD Agents"
"#
        ),
        ChannelChoice::Discord { .. } => {
            "\n# channel:\n#   type: discord\n#   bot_token_env: AGEND_DISCORD_TOKEN\n#   guild_id: \"YOUR_GUILD_ID\"\n".to_string()
        }
        ChannelChoice::Skip => String::new(),
    };

    let working_dir = project_dir
        .map(|p| format!("    working_directory: {}", p.display()))
        .unwrap_or_else(|| "    # working_directory: ~/your-project".to_string());

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

fn detect_project_root() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        format!("{home}/Documents"),
        format!("{home}/Projects"),
        format!("{home}/Code"),
        format!("{home}/src"),
        format!("{home}/dev"),
    ];

    for dir in &candidates {
        let path = std::path::PathBuf::from(dir);
        if path.exists() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if entry.path().join(".git").exists() {
                        return Some(entry.path());
                    }
                }
            }
        }
    }
    None
}

/// Save an env var to .env, preserving other variables.
fn save_env_var(home: &Path, var_name: &str, value: &str) -> anyhow::Result<()> {
    let env_path = home.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let prefix = format!("{var_name}=");
    let existing_val = existing
        .lines()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l.trim_start_matches(&prefix).trim());

    if let Some(old) = existing_val {
        if old == value {
            println!("  ✓ {var_name} unchanged in .env\n");
            return Ok(());
        }
        println!("  .env already has {var_name}={}", mask_token(old));
        let answer = prompt("  Update? (Y/n): ")?;
        if answer.trim().eq_ignore_ascii_case("n") {
            println!("  Keeping existing value.\n");
            return Ok(());
        }
    }

    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("{prefix}{value}"));
    std::fs::write(&env_path, lines.join("\n") + "\n")?;
    println!("  ✓ {var_name} saved to {}\n", env_path.display());
    Ok(())
}

fn check_compatibility(yaml_content: &str, new_backend: &Backend, channel: &ChannelChoice) {
    if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(yaml_content) {
        // Check backend
        let existing_backend = config["defaults"]["backend"].as_str().unwrap_or("");
        if !existing_backend.is_empty() && existing_backend != new_backend.name() {
            println!(
                "  ⚠ Existing backend: {existing_backend}, new: {}",
                new_backend.name()
            );
        }

        // Check channel type mismatch
        let existing_type = config["channel"]["type"].as_str().unwrap_or("");
        let new_type = match channel {
            ChannelChoice::Telegram { .. } => "telegram",
            ChannelChoice::Discord { .. } => "discord",
            ChannelChoice::Skip => "",
        };
        if !existing_type.is_empty() && !new_type.is_empty() && existing_type != new_type {
            println!("  ⚠ Existing channel: {existing_type}, new: {new_type}");
        }

        // Check group_id / guild_id
        match channel {
            ChannelChoice::Telegram {
                group_id: Some(new_gid),
                ..
            } => {
                let existing_gid = config["channel"]["group_id"].as_i64().unwrap_or(0);
                if existing_gid != 0 && existing_gid != *new_gid {
                    println!("  ⚠ Existing group_id: {existing_gid}, new: {new_gid}");
                }
            }
            ChannelChoice::Discord { guild_id, .. } if !guild_id.is_empty() => {
                let existing = config["channel"]["guild_id"].as_str().unwrap_or("");
                if !existing.is_empty() && existing != guild_id {
                    println!("  ⚠ Existing guild_id: {existing}, new: {guild_id}");
                }
            }
            _ => {}
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
mod tests {
    use super::*;

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
    fn detect_project_root_empty_home() {
        let _ = detect_project_root();
    }

    #[test]
    fn detect_backends_does_not_panic() {
        let backends = detect_backends();
        assert!(backends.len() <= 5);
    }
}
