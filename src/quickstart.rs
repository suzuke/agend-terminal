//! Interactive quickstart — detect backends, configure Telegram, generate fleet.yaml.

use crate::backend::Backend;
use std::io::{self, Write};
use std::path::Path;

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

    // Step 2: Telegram setup
    println!("  ── Telegram Setup ──\n");
    println!("  1. Open Telegram, talk to @BotFather");
    println!("  2. Send /newbot and follow instructions");
    println!("  3. Copy the bot token\n");

    let token = prompt("  Bot token: ")?;
    let token = token.trim().to_string();

    if token.is_empty() {
        println!("\n  Skipping Telegram setup. You can configure later in fleet.yaml.\n");
        generate_fleet_yaml(home, &selected, None, None)?;
        print_next_steps(home);
        return Ok(());
    }

    // Validate token format
    if !token.contains(':') {
        println!("  ⚠ Token format looks wrong (expected number:string). Continuing anyway.\n");
    }

    // Verify bot via API
    print!("  Verifying bot... ");
    io::stdout().flush().ok();
    match verify_bot(&token) {
        Ok(bot_name) => println!("✓ @{bot_name}\n"),
        Err(e) => println!("⚠ {e} (continuing anyway)\n"),
    }

    // Step 3: Group detection
    println!("  Now add the bot to your Telegram group (as admin).");
    println!("  Then send any message in the group.\n");
    print!("  Waiting for group message (3 min timeout)... ");
    io::stdout().flush().ok();

    match detect_group(&token) {
        Ok((group_id, group_title)) => {
            println!("✓ {group_title} ({group_id})\n");

            // Save .env
            let env_path = home.join(".env");
            std::fs::write(&env_path, format!("AGEND_BOT_TOKEN={token}\n"))?;
            println!("  ✓ Token saved to {}\n", env_path.display());

            generate_fleet_yaml(home, &selected, Some(group_id), Some(&token))?;
        }
        Err(e) => {
            println!("timeout/error: {e}\n");
            println!("  You can set group_id manually in fleet.yaml later.\n");

            // Save token anyway
            let env_path = home.join(".env");
            std::fs::write(&env_path, format!("AGEND_BOT_TOKEN={token}\n"))?;

            generate_fleet_yaml(home, &selected, None, Some(&token))?;
        }
    }

    print_next_steps(home);
    Ok(())
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
            anyhow::bail!("Invalid token: {}", resp["description"].as_str().unwrap_or("unknown error"))
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

    // Detect project roots
    let project_dir = detect_project_root();

    let channel_section = if let Some(gid) = group_id {
        format!(r#"
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: {gid}
  mode: topic
"#)
    } else {
        "\n# channel:\n#   type: telegram\n#   bot_token_env: AGEND_BOT_TOKEN\n#   group_id: YOUR_GROUP_ID\n".to_string()
    };

    let working_dir = project_dir
        .map(|p| format!("    working_directory: {}", p.display()))
        .unwrap_or_else(|| "    # working_directory: ~/your-project".to_string());

    let yaml = format!(r#"defaults:
  backend: {backend_name}
{channel_section}
instances:
  general:
    role: "General assistant"
{working_dir}
"#);

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
            // Find first git repo
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

fn check_compatibility(yaml_content: &str, new_backend: &Backend, new_group_id: Option<i64>) {
    if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(yaml_content) {
        // Check backend
        let existing_backend = config["defaults"]["backend"].as_str().unwrap_or("");
        if !existing_backend.is_empty() && existing_backend != new_backend.name() {
            println!("  ⚠ Existing backend: {existing_backend}, new: {}",
                new_backend.name());
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
                println!("  ⚠ Existing config has {} instances (new config will have 1)",
                    instances.len());
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
