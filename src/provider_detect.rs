//! Fugu / Sakana model-provider detection (#2441 slice 1).
//!
//! Decides whether the local environment is set up to run **Fugu** (the Sakana
//! API model served through the `codex` harness) and reports one of three
//! states so callers (doctor, the Ctrl+B C quick-spawn menu, quickstart) can
//! react without guessing:
//!
//! - [`FuguStatus::Available`] — codex on PATH, a Sakana provider is configured,
//!   and a usable credential is present.
//! - [`FuguStatus::ConfiguredNoCredential`] — provider configured but no usable
//!   `SAKANA_API_KEY` (config present, credential missing — a distinct,
//!   actionable state: "set the key", not "install it").
//! - [`FuguStatus::NotConfigured`] — no codex harness or no Sakana provider.
//!
//! **Judged on config + credential, artifacts are hints only** (#2441): a
//! `~/.codex/.fugu/` install dir or a launcher on PATH never *alone* counts as
//! configured — they miss the recommended isolated-`CODEX_HOME` setup and break
//! on uninstall.
//!
//! The core parsing functions are pure (they take file *contents* / directory
//! lists), so they unit-test without touching a real `~/.codex`. The thin
//! [`detect`] wrapper does the filesystem reads + env lookups.

use std::path::{Path, PathBuf};

/// Host that identifies the Sakana (Fugu) provider, matched against a
/// provider block's `base_url`.
const SAKANA_HOST: &str = "sakana.ai";

/// Tri-state result of Fugu environment detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuguStatus {
    /// Harness present + provider configured + credential usable.
    Available,
    /// Provider configured but no usable credential (e.g. `SAKANA_API_KEY` unset).
    ConfiguredNoCredential,
    /// No codex harness, or no Sakana provider configured anywhere we scanned.
    NotConfigured,
}

impl FuguStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FuguStatus::Available => "available",
            FuguStatus::ConfiguredNoCredential => "configured_no_credential",
            FuguStatus::NotConfigured => "not_configured",
        }
    }
}

/// Full detection report. `provider_source` points at the config file that
/// supplied the Sakana provider (or profile), for operator transparency.
#[derive(Debug, Clone)]
pub struct FuguDetection {
    pub status: FuguStatus,
    pub codex_on_path: bool,
    pub provider_source: Option<PathBuf>,
    pub has_credential: bool,
    pub models: Vec<String>,
    pub hints: Vec<String>,
    /// The resolved Sakana provider block (for provisioning an isolated home).
    pub provider: Option<ProviderBlock>,
    /// Absolute path to the model catalog JSON, when one was found.
    pub catalog_path: Option<PathBuf>,
}

/// A `[model_providers.<id>]` block reduced to the fields detection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBlock {
    pub id: String,
    pub base_url: Option<String>,
    pub env_key: Option<String>,
    pub wire_api: Option<String>,
}

/// Parse a codex `config.toml` body and return the first provider block that
/// looks like Sakana/Fugu — either the section id contains `sakana` or its
/// `base_url` host contains [`SAKANA_HOST`].
///
/// Lightweight line scanner (no `toml` production dependency): tracks the
/// current `[section]` header and collects simple `key = "value"` pairs until
/// the next header. Good enough for detection; values are unquoted leniently.
pub fn find_sakana_provider(config_toml: &str) -> Option<ProviderBlock> {
    let mut blocks: Vec<ProviderBlock> = Vec::new();
    let mut cur: Option<ProviderBlock> = None;

    let flush = |cur: &mut Option<ProviderBlock>, blocks: &mut Vec<ProviderBlock>| {
        if let Some(b) = cur.take() {
            blocks.push(b);
        }
    };

    for raw in config_toml.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            flush(&mut cur, &mut blocks);
            // Only track `model_providers.<id>` sections; skip nested `.env` etc.
            if let Some(id) = header.strip_prefix("model_providers.") {
                // A sub-table like `model_providers.sakana.env` has a dot in `id`.
                let base_id = id.split('.').next().unwrap_or(id);
                // Reopen the existing block if this is a sub-table of it.
                if id.contains('.') {
                    if let Some(prev) = blocks.iter().rposition(|b| b.id == base_id) {
                        cur = Some(blocks.remove(prev));
                    }
                    continue;
                }
                cur = Some(ProviderBlock {
                    id: base_id.to_string(),
                    base_url: None,
                    env_key: None,
                    wire_api: None,
                });
            }
            continue;
        }
        let Some(block) = cur.as_mut() else { continue };
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = unquote_toml(v.trim());
            match key {
                "base_url" => block.base_url = Some(val),
                "env_key" => block.env_key = Some(val),
                "wire_api" => block.wire_api = Some(val),
                _ => {}
            }
        }
    }
    flush(&mut cur, &mut blocks);

    blocks.into_iter().find(|b| {
        b.id.contains("sakana")
            || b.base_url
                .as_deref()
                .map(|u| u.contains(SAKANA_HOST))
                .unwrap_or(false)
    })
}

/// Does a codex profile body (`<name>.config.toml`) select the Sakana provider
/// and/or the `fugu` model? Returns the referenced `model_catalog_json` path
/// when present so the caller can resolve the model list.
pub fn profile_targets_fugu(profile_toml: &str) -> Option<Option<String>> {
    let mut provider_is_sakana = false;
    let mut model_is_fugu = false;
    let mut catalog: Option<String> = None;
    for raw in profile_toml.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let val = unquote_toml(v.trim());
            match k.trim() {
                "model_provider" => provider_is_sakana = val.contains("sakana"),
                "model" => model_is_fugu = val.starts_with("fugu"),
                "model_catalog_json" => catalog = Some(val),
                _ => {}
            }
        }
    }
    if provider_is_sakana || model_is_fugu {
        Some(catalog)
    } else {
        None
    }
}

/// Resolve whether a credential is usable from a provider's `env_key`, given an
/// env lookup. Handles both shapes:
/// - inline `NAME=value` (the Sakana fork embeds the key) → present iff the
///   value part is non-empty,
/// - bare `NAME` → present iff the env var `NAME` is set non-empty.
///
/// A bare name also succeeds if `NAME` happens to be set in the environment.
pub fn credential_present(
    env_key: Option<&str>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let Some(env_key) = env_key else {
        // No env_key declared → fall back to the conventional var name.
        return env_lookup("SAKANA_API_KEY").is_some_and(|v| !v.is_empty());
    };
    if let Some((name, value)) = env_key.split_once('=') {
        if !value.trim().is_empty() {
            return true; // inline credential
        }
        return env_lookup(name.trim()).is_some_and(|v| !v.is_empty());
    }
    env_lookup(env_key.trim()).is_some_and(|v| !v.is_empty())
}

/// Parse a Fugu catalog (`fugu.json`) and return the model slugs that are
/// `supported_in_api`. Tolerant of missing fields.
pub fn parse_catalog_models(catalog_json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(catalog_json) else {
        return Vec::new();
    };
    let Some(models) = v.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|m| {
            // Default true when the field is absent — be permissive.
            m.get("supported_in_api")
                .and_then(|b| b.as_bool())
                .unwrap_or(true)
        })
        .filter_map(|m| m.get("slug").and_then(|s| s.as_str()).map(String::from))
        .collect()
}

/// Strip surrounding single/double quotes from a TOML scalar value, dropping a
/// trailing inline comment for unquoted values.
fn unquote_toml(v: &str) -> String {
    let v = v.trim();
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return v[1..v.len() - 1].to_string();
    }
    // Unquoted: cut an inline comment if present.
    v.split('#').next().unwrap_or(v).trim().to_string()
}

/// Expand a leading `~` against `home`; otherwise resolve relative paths
/// against `base_dir` (the codex home that referenced the catalog).
fn resolve_catalog_path(raw: &str, base_dir: &Path, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        base_dir.join(p)
    }
}

/// The codex-home directories to scan for a Fugu provider, in priority order:
/// `$CODEX_HOME` (if set), then `~/.codex`, then the agend-managed Fugu home
/// (`~/.agend-fugu-codex`). Deduplicated, order-preserving.
pub fn codex_home_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(ch) = std::env::var_os("CODEX_HOME") {
        push(PathBuf::from(ch));
    }
    if let Some(home) = dirs::home_dir() {
        push(home.join(".codex"));
        push(home.join(".agend-fugu-codex"));
    }
    out
}

/// Filesystem + env wrapper around the pure parsers. Scans `candidates` for a
/// Sakana provider (config block or fugu profile), determines credential
/// presence via `env_lookup`, and resolves the model list from the catalog.
pub fn detect(
    candidates: &[PathBuf],
    codex_on_path: bool,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> FuguDetection {
    let home = dirs::home_dir();
    let mut hints: Vec<String> = Vec::new();

    if !codex_on_path {
        hints.push("codex CLI not found on PATH — install codex to run Fugu".to_string());
        return FuguDetection {
            status: FuguStatus::NotConfigured,
            codex_on_path,
            provider_source: None,
            has_credential: false,
            models: Vec::new(),
            hints,
            provider: None,
            catalog_path: None,
        };
    }

    let mut provider_source: Option<PathBuf> = None;
    let mut provider_block: Option<ProviderBlock> = None;
    let mut catalog_path: Option<PathBuf> = None;

    for dir in candidates {
        // 1) Primary: a [model_providers.*] sakana block in config.toml.
        let config = dir.join("config.toml");
        if provider_block.is_none() {
            if let Ok(body) = std::fs::read_to_string(&config) {
                if let Some(block) = find_sakana_provider(&body) {
                    provider_block = Some(block);
                    provider_source = Some(config.clone());
                }
            }
        }
        // 2) Profiles `<name>.config.toml` that target fugu (also yields catalog).
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_profile = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".config.toml"))
                    .unwrap_or(false);
                if !is_profile {
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(&path) {
                    if let Some(cat) = profile_targets_fugu(&body) {
                        if provider_source.is_none() {
                            provider_source = Some(path.clone());
                        }
                        if catalog_path.is_none() {
                            if let Some(c) = cat {
                                catalog_path = Some(resolve_catalog_path(&c, dir, home.as_deref()));
                            }
                        }
                    }
                }
            }
        }
        // 3) Catalog fallback: a fugu.json sitting in the home.
        if catalog_path.is_none() {
            let fj = dir.join("fugu.json");
            if fj.exists() {
                catalog_path = Some(fj);
            }
        }
    }

    let configured = provider_block.is_some() || provider_source.is_some();
    if !configured {
        hints.push(
            "no Sakana (Fugu) provider found in ~/.codex/config.toml, a *.config.toml \
             profile, or an isolated CODEX_HOME"
                .to_string(),
        );
        return FuguDetection {
            status: FuguStatus::NotConfigured,
            codex_on_path,
            provider_source: None,
            has_credential: false,
            models: Vec::new(),
            hints,
            provider: None,
            catalog_path: None,
        };
    }

    let models = catalog_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_catalog_models(&s))
        .unwrap_or_default();

    let env_key = provider_block.as_ref().and_then(|b| b.env_key.as_deref());
    let has_credential = credential_present(env_key, env_lookup);

    let status = if has_credential {
        FuguStatus::Available
    } else {
        hints.push(
            "Sakana provider configured but no usable SAKANA_API_KEY \
             (set the env var or the provider's env_key)"
                .to_string(),
        );
        FuguStatus::ConfiguredNoCredential
    };

    FuguDetection {
        status,
        codex_on_path,
        provider_source,
        has_credential,
        models,
        hints,
        provider: provider_block,
        catalog_path,
    }
}

/// The agend-managed isolated `CODEX_HOME` for the Fugu agent. A dedicated home
/// lets a plain `codex` spawn default to the `fugu` model without needing the
/// `-p fugu` global flag (which agend's resume argv ordering can't carry).
pub fn fugu_codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".agend-fugu-codex"))
}

/// Render the `config.toml` body for the isolated Fugu `CODEX_HOME`. Pure so it
/// is unit-testable. Selects `fugu` as the default model and inlines the
/// detected Sakana provider block + catalog path.
pub fn render_fugu_home_config(provider: &ProviderBlock, catalog_abs: &Path) -> String {
    let base_url = provider
        .base_url
        .as_deref()
        .unwrap_or("https://api.sakana.ai/v1");
    let env_key = provider.env_key.as_deref().unwrap_or("SAKANA_API_KEY");
    let wire_api = provider.wire_api.as_deref().unwrap_or("responses");
    let mut out = String::new();
    out.push_str("# Auto-generated by agend-terminal (#2441) - isolated Fugu CODEX_HOME.\n");
    out.push_str("model = \"fugu\"\n");
    out.push_str("model_reasoning_effort = \"high\"\n");
    out.push_str(&format!("model_provider = {}\n", toml_quote(&provider.id)));
    out.push_str(&format!(
        "model_catalog_json = {}\n",
        toml_quote(&catalog_abs.to_string_lossy())
    ));
    out.push_str("check_for_update_on_startup = false\n\n");
    out.push_str(&format!("[model_providers.{}]\n", provider.id));
    out.push_str(&format!("base_url = {}\n", toml_quote(base_url)));
    out.push_str(&format!("env_key = {}\n", toml_quote(env_key)));
    out.push_str(&format!("wire_api = {}\n", toml_quote(wire_api)));
    out
}

/// Single-quoted TOML literal (preserves backslashes; falls back to a basic
/// string when an apostrophe is present).
fn toml_quote(s: &str) -> String {
    if s.contains('\\') {
        // Path with backslashes (Windows): use a TOML basic string and escape.
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{s}'")
    }
}

/// Ensure the isolated Fugu `CODEX_HOME` exists and is configured. Idempotent:
/// if its `config.toml` already exists it is left untouched. Requires an
/// `Available` detection carrying a provider block + catalog path; returns the
/// home path on success.
pub fn ensure_fugu_codex_home(detection: &FuguDetection) -> std::io::Result<PathBuf> {
    let home = fugu_codex_home()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    let config = home.join("config.toml");
    if config.exists() {
        return Ok(home);
    }
    let provider = detection.provider.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Fugu provider block not detected — cannot provision CODEX_HOME",
        )
    })?;
    let catalog = detection.catalog_path.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Fugu model catalog not detected — cannot provision CODEX_HOME",
        )
    })?;
    std::fs::create_dir_all(&home)?;
    std::fs::write(&config, render_fugu_home_config(provider, catalog))?;
    // Best-effort: copy auth.json from the source codex home (for bare-name
    // env_key setups that read credentials from disk). Inline-key setups don't
    // need it, so a missing source is not an error.
    if let Some(src_home) = detection.provider_source.as_ref().and_then(|p| p.parent()) {
        let src_auth = src_home.join("auth.json");
        if src_auth.exists() {
            let _ = std::fs::copy(&src_auth, home.join("auth.json"));
        }
    }
    Ok(home)
}

/// Production convenience: detect using the live PATH + env + standard scan set.
pub fn detect_default() -> FuguDetection {
    let codex_on_path = which::which("codex").is_ok();
    let env_lookup = |name: &str| std::env::var(name).ok();
    detect(&codex_home_candidates(), codex_on_path, &env_lookup)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn finds_sakana_block_by_id() {
        let cfg = "model = \"gpt-5.5\"\n\n[model_providers.sakana]\nname = \"Sakana API\"\n\
                   base_url = \"https://api.sakana.ai/v1\"\nenv_key = \"SAKANA_API_KEY=fish_abc\"\n\
                   wire_api = \"responses\"\n";
        let b = find_sakana_provider(cfg).expect("block");
        assert_eq!(b.id, "sakana");
        assert_eq!(b.base_url.as_deref(), Some("https://api.sakana.ai/v1"));
        assert_eq!(b.wire_api.as_deref(), Some("responses"));
        assert_eq!(b.env_key.as_deref(), Some("SAKANA_API_KEY=fish_abc"));
    }

    #[test]
    fn finds_provider_by_base_url_host_even_with_other_id() {
        let cfg = "[model_providers.custom]\nbase_url = \"https://api.sakana.ai/v1\"\n";
        let b = find_sakana_provider(cfg).expect("block");
        assert_eq!(b.id, "custom");
    }

    #[test]
    fn no_sakana_block_returns_none() {
        let cfg = "[model_providers.openai]\nbase_url = \"https://api.openai.com/v1\"\n";
        assert!(find_sakana_provider(cfg).is_none());
    }

    #[test]
    fn inline_env_key_is_credential_present() {
        assert!(credential_present(
            Some("SAKANA_API_KEY=fish_realkey"),
            &no_env
        ));
    }

    #[test]
    fn bare_env_key_needs_env_var() {
        assert!(!credential_present(Some("SAKANA_API_KEY"), &no_env));
        let with = |n: &str| (n == "SAKANA_API_KEY").then(|| "fish_x".to_string());
        assert!(credential_present(Some("SAKANA_API_KEY"), &with));
    }

    #[test]
    fn inline_env_key_empty_value_falls_back_to_env() {
        // `NAME=` with empty value → must consult env.
        assert!(!credential_present(Some("SAKANA_API_KEY="), &no_env));
        let with = |n: &str| (n == "SAKANA_API_KEY").then(|| "fish_x".to_string());
        assert!(credential_present(Some("SAKANA_API_KEY="), &with));
    }

    #[test]
    fn profile_detects_fugu_and_catalog() {
        let prof = "model = \"fugu\"\nmodel_provider = \"sakana\"\n\
                    model_catalog_json = \"~/.codex/fugu.json\"\n";
        let cat = profile_targets_fugu(prof).expect("targets fugu");
        assert_eq!(cat.as_deref(), Some("~/.codex/fugu.json"));
    }

    #[test]
    fn profile_without_fugu_returns_none() {
        let prof = "model = \"gpt-5.5\"\nmodel_provider = \"openai\"\n";
        assert!(profile_targets_fugu(prof).is_none());
    }

    #[test]
    fn catalog_models_filters_supported() {
        let json = r#"{"models":[
            {"slug":"fugu","supported_in_api":true},
            {"slug":"fugu-ultra","supported_in_api":true},
            {"slug":"hidden","supported_in_api":false}
        ]}"#;
        let m = parse_catalog_models(json);
        assert_eq!(m, vec!["fugu".to_string(), "fugu-ultra".to_string()]);
    }

    #[test]
    fn detect_not_configured_without_codex() {
        let d = detect(&[], false, &no_env);
        assert_eq!(d.status, FuguStatus::NotConfigured);
        assert!(!d.codex_on_path);
        assert!(d.hints.iter().any(|h| h.contains("PATH")));
    }

    #[test]
    fn detect_available_end_to_end() {
        let dir = std::env::temp_dir().join(format!("agend-fugu-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[model_providers.sakana]\nbase_url = \"https://api.sakana.ai/v1\"\n\
             env_key = \"SAKANA_API_KEY=fish_inline\"\nwire_api = \"responses\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("fugu.json"),
            r#"{"models":[{"slug":"fugu","supported_in_api":true}]}"#,
        )
        .unwrap();
        let d = detect(std::slice::from_ref(&dir), true, &no_env);
        assert_eq!(d.status, FuguStatus::Available);
        assert!(d.has_credential);
        assert_eq!(d.models, vec!["fugu".to_string()]);
        assert!(d.provider_source.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_configured_no_credential() {
        let dir = std::env::temp_dir().join(format!("agend-fugu-nocred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[model_providers.sakana]\nbase_url = \"https://api.sakana.ai/v1\"\n\
             env_key = \"SAKANA_API_KEY\"\nwire_api = \"responses\"\n",
        )
        .unwrap();
        let d = detect(std::slice::from_ref(&dir), true, &no_env);
        assert_eq!(d.status, FuguStatus::ConfiguredNoCredential);
        assert!(!d.has_credential);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn render_fugu_home_config_selects_fugu_and_provider() {
        let provider = ProviderBlock {
            id: "sakana".to_string(),
            base_url: Some("https://api.sakana.ai/v1".to_string()),
            env_key: Some("SAKANA_API_KEY=fish_inline".to_string()),
            wire_api: Some("responses".to_string()),
        };
        let body =
            render_fugu_home_config(&provider, std::path::Path::new("/Users/x/.codex/fugu.json"));
        assert!(body.contains("model = \"fugu\""));
        assert!(
            body.contains("model_provider = \"sakana\"")
                || body.contains("model_provider = 'sakana'")
        );
        assert!(body.contains("[model_providers.sakana]"));
        assert!(body.contains("https://api.sakana.ai/v1"));
        assert!(body.contains("SAKANA_API_KEY=fish_inline"));
        assert!(body.contains("/Users/x/.codex/fugu.json"));
    }

    #[test]
    fn ensure_fugu_home_errors_without_provider() {
        let detection = FuguDetection {
            status: FuguStatus::ConfiguredNoCredential,
            codex_on_path: true,
            provider_source: None,
            has_credential: false,
            models: Vec::new(),
            hints: Vec::new(),
            provider: None,
            catalog_path: None,
        };
        assert!(ensure_fugu_codex_home(&detection).is_err());
    }
}
