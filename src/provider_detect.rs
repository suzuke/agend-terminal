//! Model-provider detection (#2441).
//!
//! Decides whether the local environment is set up to run hosted model providers
//! through compatible CLI harnesses. Fugu/Sakana is the first shipped descriptor
//! and is served through the `codex` harness. Detection reports one of three
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// First-class descriptor for API model providers (#2441). The harness axis
/// remains PATH-detected (`compatible_harnesses`), while this descriptor captures
/// the model-provider axis that is configured through a harness config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelProviderDescriptor {
    pub name: &'static str,
    pub base_url: &'static str,
    pub env_key: &'static str,
    pub wire_api: &'static str,
    pub compatible_harnesses: &'static [&'static str],
    pub probe_path: Option<&'static str>,
    pub provider_id_hint: &'static str,
    pub model_prefixes: &'static [&'static str],
}

impl ModelProviderDescriptor {
    pub fn host(self) -> &'static str {
        host_from_url(self.base_url)
    }
}

/// Fugu/Sakana is the first declared provider (#2441).
pub const FUGU_PROVIDER_DESCRIPTOR: ModelProviderDescriptor = ModelProviderDescriptor {
    name: "fugu",
    base_url: "https://api.sakana.ai/v1",
    env_key: "SAKANA_API_KEY",
    wire_api: "responses",
    compatible_harnesses: &["codex"],
    probe_path: Some("/models"),
    provider_id_hint: "sakana",
    model_prefixes: &["fugu"],
};

/// All hosted/API provider descriptors known to agend-terminal.
pub const PROVIDER_DESCRIPTORS: &[ModelProviderDescriptor] = &[FUGU_PROVIDER_DESCRIPTOR];

/// CLI backends whose provider is intentionally fixed, not base_url/env_key
/// swappable under #2441. This is an explicit boundary of the provider axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedProviderBackend {
    pub backend: &'static str,
    pub reason: &'static str,
}

pub const FIXED_PROVIDER_BACKENDS: &[FixedProviderBackend] = &[
    FixedProviderBackend {
        backend: "kiro-cli",
        reason: "fixed AWS endpoint / signed-auth shape, not bearer base_url-overridable",
    },
    FixedProviderBackend {
        backend: "agy",
        reason: "Google service-account/OAuth shape, not bearer base_url-overridable",
    },
];

const DEFAULT_PROBE_TTL: Duration = Duration::from_secs(10 * 60);

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

/// Fail-open endpoint probe status. `Unknown` is deliberately non-fatal: network
/// failures, auth service outages, malformed responses, or stale/no cache must
/// not make a configured provider disappear from startup/menu detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProbeStatus {
    Healthy,
    Unhealthy,
    Unknown,
}

impl ProviderProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderProbeStatus::Healthy => "healthy",
            ProviderProbeStatus::Unhealthy => "unhealthy",
            ProviderProbeStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderProbeCache {
    pub provider: String,
    pub status: ProviderProbeStatus,
    pub checked_at_unix_secs: u64,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Full detection report. `provider_source` points at the config file that
/// supplied the Sakana provider (or profile), for operator transparency.
#[derive(Debug, Clone)]
pub struct FuguDetection {
    pub status: FuguStatus,
    pub descriptor: ModelProviderDescriptor,
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
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub env_key: Option<String>,
    pub wire_api: Option<String>,
}

fn provider_block_matches_descriptor(
    block: &ProviderBlock,
    descriptor: ModelProviderDescriptor,
) -> bool {
    block.id.contains(descriptor.provider_id_hint)
        || block
            .base_url
            .as_deref()
            .map(|u| host_from_url(u).contains(descriptor.host()))
            .unwrap_or(false)
}

fn host_from_url(url: &str) -> &str {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

/// Parse a codex `config.toml` body and return the first provider block that
/// matches `descriptor` — either the section id contains the descriptor's
/// `provider_id_hint` or its `base_url` host matches the descriptor host.
///
/// Lightweight line scanner (no `toml` production dependency): tracks the
/// current `[section]` header and collects simple `key = "value"` pairs until
/// the next header. Good enough for detection; values are unquoted leniently.
pub fn find_provider(
    config_toml: &str,
    descriptor: ModelProviderDescriptor,
) -> Option<ProviderBlock> {
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
                    name: None,
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
                "name" => block.name = Some(val),
                "base_url" => block.base_url = Some(val),
                "env_key" => block.env_key = Some(val),
                "wire_api" => block.wire_api = Some(val),
                _ => {}
            }
        }
    }
    flush(&mut cur, &mut blocks);

    blocks
        .into_iter()
        .find(|b| provider_block_matches_descriptor(b, descriptor))
}

/// Back-compat helper for the first shipped provider.
pub fn find_sakana_provider(config_toml: &str) -> Option<ProviderBlock> {
    find_provider(config_toml, FUGU_PROVIDER_DESCRIPTOR)
}

/// Does a codex profile body (`<name>.config.toml`) select `descriptor` by
/// provider id and/or model prefix? Returns the referenced `model_catalog_json`
/// path when present so the caller can resolve the model list.
pub fn profile_targets_provider(
    profile_toml: &str,
    descriptor: ModelProviderDescriptor,
) -> Option<Option<String>> {
    let mut provider_matches = false;
    let mut model_matches = false;
    let mut catalog: Option<String> = None;
    for raw in profile_toml.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let val = unquote_toml(v.trim());
            match k.trim() {
                "model_provider" => provider_matches = val.contains(descriptor.provider_id_hint),
                "model" => {
                    model_matches = descriptor
                        .model_prefixes
                        .iter()
                        .any(|prefix| val.starts_with(prefix));
                }
                "model_catalog_json" => catalog = Some(val),
                _ => {}
            }
        }
    }
    if provider_matches || model_matches {
        Some(catalog)
    } else {
        None
    }
}

/// Back-compat helper for the first shipped provider.
pub fn profile_targets_fugu(profile_toml: &str) -> Option<Option<String>> {
    profile_targets_provider(profile_toml, FUGU_PROVIDER_DESCRIPTOR)
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
    credential_value(env_key, FUGU_PROVIDER_DESCRIPTOR.env_key, env_lookup).is_some()
}

fn credential_value(
    env_key: Option<&str>,
    default_env_key: &str,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let env_key = env_key.unwrap_or(default_env_key);
    if let Some((name, value)) = env_key.split_once('=') {
        if !value.trim().is_empty() {
            return Some(value.trim().to_string()); // inline credential
        }
        return env_lookup(name.trim()).filter(|v| !v.is_empty());
    }
    env_lookup(env_key.trim()).filter(|v| !v.is_empty())
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

/// Parse common `/models` response shapes. Supports the Fugu catalog shape
/// (`models[].slug`) and OpenAI-compatible shape (`data[].id`).
pub fn parse_probe_models(models_json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(models_json) else {
        return Vec::new();
    };
    if let Some(models) = v.get("data").and_then(|m| m.as_array()) {
        return models
            .iter()
            .filter_map(|m| m.get("id").and_then(|s| s.as_str()).map(String::from))
            .collect();
    }
    if let Some(models) = v.get("models").and_then(|m| m.as_array()) {
        return models
            .iter()
            .filter_map(|m| {
                m.get("slug")
                    .or_else(|| m.get("id"))
                    .and_then(|s| s.as_str())
                    .map(String::from)
            })
            .collect();
    }
    Vec::new()
}

pub fn provider_probe_cache_path(home: &Path, descriptor: ModelProviderDescriptor) -> PathBuf {
    home.join("provider-probes")
        .join(format!("{}.json", descriptor.name))
}

pub fn probe_cache_is_fresh(cache: &ProviderProbeCache, now: SystemTime, ttl: Duration) -> bool {
    let now_secs = unix_secs(now);
    now_secs.saturating_sub(cache.checked_at_unix_secs) <= ttl.as_secs()
}

pub fn read_provider_probe_cache(
    path: &Path,
    now: SystemTime,
    ttl: Duration,
) -> Option<ProviderProbeCache> {
    let body = std::fs::read_to_string(path).ok()?;
    let cache = serde_json::from_str::<ProviderProbeCache>(&body).ok()?;
    probe_cache_is_fresh(&cache, now, ttl).then_some(cache)
}

pub fn write_provider_probe_cache(path: &Path, cache: &ProviderProbeCache) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(cache).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// Optional fail-open endpoint probe (#2441). This is intentionally NOT called
/// from startup/menu detection: callers opt in from diagnostics/background
/// refresh, provide a cache path, and treat `Unknown` as non-fatal.
pub async fn probe_provider_models_fail_open(
    descriptor: ModelProviderDescriptor,
    provider: &ProviderBlock,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    cache_path: &Path,
) -> ProviderProbeCache {
    let now = SystemTime::now();
    if let Some(cache) = read_provider_probe_cache(cache_path, now, DEFAULT_PROBE_TTL) {
        return cache;
    }

    let Some(probe_path) = descriptor.probe_path else {
        return ProviderProbeCache {
            provider: descriptor.name.to_string(),
            status: ProviderProbeStatus::Unknown,
            checked_at_unix_secs: unix_secs(now),
            models: Vec::new(),
            error: Some("provider has no probe_path".to_string()),
        };
    };

    let Some(key) = credential_value(provider.env_key.as_deref(), descriptor.env_key, env_lookup)
    else {
        return ProviderProbeCache {
            provider: descriptor.name.to_string(),
            status: ProviderProbeStatus::Unknown,
            checked_at_unix_secs: unix_secs(now),
            models: Vec::new(),
            error: Some("credential unavailable for probe".to_string()),
        };
    };

    let base = provider.base_url.as_deref().unwrap_or(descriptor.base_url);
    let url = format!("{}{}", base.trim_end_matches('/'), probe_path);
    let result = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;
        let resp = client.get(url).bearer_auth(key).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        Ok::<_, reqwest::Error>((status, body))
    }
    .await;

    let cache = match result {
        Ok((status, body)) if status.is_success() => ProviderProbeCache {
            provider: descriptor.name.to_string(),
            status: ProviderProbeStatus::Healthy,
            checked_at_unix_secs: unix_secs(now),
            models: parse_probe_models(&body),
            error: None,
        },
        Ok((status, _)) => ProviderProbeCache {
            provider: descriptor.name.to_string(),
            status: ProviderProbeStatus::Unknown,
            checked_at_unix_secs: unix_secs(now),
            models: Vec::new(),
            error: Some(format!("probe returned HTTP {status}")),
        },
        Err(e) => ProviderProbeCache {
            provider: descriptor.name.to_string(),
            status: ProviderProbeStatus::Unknown,
            checked_at_unix_secs: unix_secs(now),
            models: Vec::new(),
            error: Some(e.to_string()),
        },
    };
    let _ = write_provider_probe_cache(cache_path, &cache);
    cache
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
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
    let descriptor = FUGU_PROVIDER_DESCRIPTOR;
    let home = dirs::home_dir();
    let mut hints: Vec<String> = Vec::new();

    if !codex_on_path {
        hints.push("codex CLI not found on PATH — install codex to run Fugu".to_string());
        return FuguDetection {
            status: FuguStatus::NotConfigured,
            descriptor,
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
            descriptor,
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
        descriptor,
        codex_on_path,
        provider_source,
        has_credential,
        models,
        hints,
        provider: provider_block,
        catalog_path,
    }
}

/// The default Codex home (`~/.codex`). Fugu SHARES this home — its provider
/// block and `auth.json` live here — and selects the `fugu` model via a layered
/// profile file (`fugu.config.toml`, spawned with `codex -p fugu`) rather than an
/// isolated `CODEX_HOME`. Sharing avoids the auth-snapshot drift and the
/// credential-embedding `env_key` reshape that the isolated-home generator hit.
pub fn default_codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// Render the Codex profile body (`fugu.config.toml`) layered via `codex -p fugu`.
/// Pure so it is unit-testable. Selects the `fugu` model, references the detected
/// provider by id, and points at the catalog.
///
/// It deliberately emits NO `[model_providers.*]` block and NO `env_key`: the
/// provider (with the BARE `env_key` codex actually reads) is defined once in the
/// shared base `config.toml` and merely referenced here. Re-emitting the provider
/// block is what let the inline `env_key = "NAME=value"` form leak into a file
/// codex parses, which it rejects with "Missing environment variable".
pub fn render_fugu_profile_config(provider: &ProviderBlock, catalog_abs: &Path) -> String {
    let mut out = String::new();
    out.push_str("# Auto-generated by agend-terminal — Fugu profile (codex `-p fugu`).\n");
    out.push_str("model = \"fugu\"\n");
    out.push_str("model_reasoning_effort = \"high\"\n");
    out.push_str(&format!("model_provider = {}\n", toml_quote(&provider.id)));
    out.push_str(&format!(
        "model_catalog_json = {}\n",
        toml_quote(&catalog_abs.to_string_lossy())
    ));
    out
}

/// TOML basic string literal for generated config values.
fn toml_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A Fugu profile file needs (re)writing if it is missing the `model_provider`
/// selector — the load-bearing line that points `-p fugu` at the Sakana
/// provider. A stale isolated-home `config.toml` accidentally found at this path
/// lacks this top-level key in the expected shape, so it is rewritten too.
fn fugu_profile_needs_rewrite(body: &str) -> bool {
    !body
        .lines()
        .any(|l| l.trim_start().starts_with("model_provider"))
}

/// Ensure the Fugu Codex profile (`<codex_home>/fugu.config.toml`) exists, so a
/// `codex -p fugu` spawn selects the `fugu` model + Sakana provider while SHARING
/// the codex home (its `auth.json`, sessions, and `[model_providers.*]` block).
/// Idempotent: a profile already carrying a `model_provider` selector is left
/// untouched. The profile is written into the home where the provider is defined
/// (`provider_source`'s dir), falling back to `~/.codex`, so the referenced
/// provider id resolves. Requires a detection carrying a provider block + catalog
/// path; returns the codex home the profile was written into (the caller points
/// the instance at it via `CODEX_HOME` only when it is not the default `~/.codex`).
pub fn ensure_fugu_profile(detection: &FuguDetection) -> std::io::Result<PathBuf> {
    let home = detection
        .provider_source
        .as_ref()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .or_else(default_codex_home)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no codex home"))?;
    let profile = home.join("fugu.config.toml");
    if profile.exists() {
        let existing = std::fs::read_to_string(&profile)?;
        if !fugu_profile_needs_rewrite(&existing) {
            return Ok(home);
        }
    }
    let provider = detection.provider.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Fugu provider block not detected — cannot provision profile",
        )
    })?;
    let catalog = detection.catalog_path.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Fugu model catalog not detected — cannot provision profile",
        )
    })?;
    std::fs::create_dir_all(&home)?;
    std::fs::write(&profile, render_fugu_profile_config(provider, catalog))?;
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
    fn fugu_descriptor_matches_2441_acceptance() {
        let d = FUGU_PROVIDER_DESCRIPTOR;
        assert_eq!(d.name, "fugu");
        assert_eq!(d.base_url, "https://api.sakana.ai/v1");
        assert_eq!(d.env_key, "SAKANA_API_KEY");
        assert_eq!(d.wire_api, "responses");
        assert_eq!(d.compatible_harnesses, &["codex"]);
        assert_eq!(d.probe_path, Some("/models"));
        assert!(PROVIDER_DESCRIPTORS.iter().any(|p| p.name == "fugu"));
    }

    #[test]
    fn fixed_provider_backends_are_explicitly_out_of_axis() {
        let names: Vec<_> = FIXED_PROVIDER_BACKENDS.iter().map(|b| b.backend).collect();
        assert!(names.contains(&"kiro-cli"));
        assert!(names.contains(&"agy"));
        assert!(FIXED_PROVIDER_BACKENDS
            .iter()
            .all(|b| b.reason.contains("not bearer base_url")));
    }

    #[test]
    fn generic_descriptor_finds_provider_by_host() {
        let cfg = "[model_providers.anything]\nbase_url = 'https://api.sakana.ai/v1'\n";
        let b = find_provider(cfg, FUGU_PROVIDER_DESCRIPTOR).expect("block");
        assert_eq!(b.id, "anything");
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
    fn probe_models_parse_openai_and_catalog_shapes() {
        let openai = r#"{"data":[{"id":"fugu"},{"id":"fugu-ultra"}]}"#;
        assert_eq!(
            parse_probe_models(openai),
            vec!["fugu".to_string(), "fugu-ultra".to_string()]
        );

        let catalog = r#"{"models":[{"slug":"fugu"},{"id":"fugu-ultra"}]}"#;
        assert_eq!(
            parse_probe_models(catalog),
            vec!["fugu".to_string(), "fugu-ultra".to_string()]
        );
    }

    #[test]
    fn probe_cache_respects_ttl() {
        let cache = ProviderProbeCache {
            provider: "fugu".to_string(),
            status: ProviderProbeStatus::Healthy,
            checked_at_unix_secs: 1_000,
            models: vec!["fugu".to_string()],
            error: None,
        };
        let fresh_at = UNIX_EPOCH + Duration::from_secs(1_590);
        let stale_at = UNIX_EPOCH + Duration::from_secs(1_601);
        assert!(probe_cache_is_fresh(
            &cache,
            fresh_at,
            Duration::from_secs(600)
        ));
        assert!(!probe_cache_is_fresh(
            &cache,
            stale_at,
            Duration::from_secs(600)
        ));
    }

    #[test]
    fn probe_cache_round_trips_json() {
        let dir =
            std::env::temp_dir().join(format!("agend-provider-probe-cache-{}", std::process::id()));
        let path = provider_probe_cache_path(&dir, FUGU_PROVIDER_DESCRIPTOR);
        let cache = ProviderProbeCache {
            provider: "fugu".to_string(),
            status: ProviderProbeStatus::Unknown,
            checked_at_unix_secs: 42,
            models: Vec::new(),
            error: Some("fail-open".to_string()),
        };
        write_provider_probe_cache(&path, &cache).unwrap();
        let loaded = read_provider_probe_cache(
            &path,
            UNIX_EPOCH + Duration::from_secs(43),
            Duration::from_secs(600),
        )
        .expect("fresh cache");
        assert_eq!(loaded, cache);
        std::fs::remove_dir_all(&dir).ok();
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
    fn render_fugu_profile_selects_model_provider_and_catalog() {
        let provider = ProviderBlock {
            id: "sakana".to_string(),
            name: Some("Sakana API".to_string()),
            base_url: Some("https://api.sakana.ai/v1".to_string()),
            env_key: Some("SAKANA_API_KEY".to_string()),
            wire_api: Some("responses".to_string()),
        };
        let body =
            render_fugu_profile_config(&provider, std::path::Path::new("/Users/x/.codex/fugu.json"));
        let parsed: toml::Value = toml::from_str(&body).expect("profile is valid TOML");
        assert_eq!(parsed["model"].as_str(), Some("fugu"));
        assert_eq!(parsed["model_provider"].as_str(), Some("sakana"));
        assert_eq!(
            parsed["model_catalog_json"].as_str(),
            Some("/Users/x/.codex/fugu.json")
        );
    }

    /// Regression pin: the generated profile MUST NOT carry a `[model_providers.*]`
    /// block or an `env_key`. The provider (with its bare `env_key`) lives only in
    /// the shared base `config.toml`; re-emitting it here is exactly what let the
    /// inline `env_key = "NAME=value"` form reach a file codex parses and reject.
    #[test]
    fn render_fugu_profile_has_no_provider_block_or_env_key() {
        let provider = ProviderBlock {
            id: "sakana".to_string(),
            name: Some("Sakana API".to_string()),
            base_url: Some("https://api.sakana.ai/v1".to_string()),
            env_key: Some("SAKANA_API_KEY=fish_inline".to_string()),
            wire_api: Some("responses".to_string()),
        };
        let body =
            render_fugu_profile_config(&provider, std::path::Path::new("/Users/x/.codex/fugu.json"));
        assert!(!body.contains("[model_providers"));
        assert!(!body.contains("env_key"));
        assert!(!body.contains("fish_inline"));
    }

    #[test]
    fn render_fugu_profile_quotes_apostrophes_as_valid_toml() {
        let provider = ProviderBlock {
            id: "sakana".to_string(),
            name: None,
            base_url: None,
            env_key: None,
            wire_api: None,
        };
        let body = render_fugu_profile_config(
            &provider,
            std::path::Path::new("/Users/x/O'Brien/fugu.json"),
        );
        let parsed: toml::Value = toml::from_str(&body).expect("generated profile is valid TOML");
        assert_eq!(
            parsed["model_catalog_json"].as_str(),
            Some("/Users/x/O'Brien/fugu.json")
        );
    }

    #[test]
    fn fugu_profile_needs_rewrite_detects_missing_selector() {
        assert!(fugu_profile_needs_rewrite("model = \"fugu\"\n"));
        assert!(!fugu_profile_needs_rewrite(
            "model = \"fugu\"\nmodel_provider = \"sakana\"\n"
        ));
    }

    #[test]
    fn ensure_fugu_profile_writes_into_provider_home_and_is_idempotent() {
        let home = std::env::temp_dir().join(format!("agend-fugu-profile-write-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        let catalog = home.join("fugu.json");
        std::fs::write(&catalog, "{\"models\":[]}").unwrap();
        let detection = FuguDetection {
            status: FuguStatus::Available,
            descriptor: FUGU_PROVIDER_DESCRIPTOR,
            codex_on_path: true,
            provider_source: Some(home.join("config.toml")),
            has_credential: true,
            models: Vec::new(),
            hints: Vec::new(),
            provider: Some(ProviderBlock {
                id: "sakana".to_string(),
                name: Some("Sakana API".to_string()),
                base_url: Some("https://api.sakana.ai/v1".to_string()),
                env_key: Some("SAKANA_API_KEY".to_string()),
                wire_api: Some("responses".to_string()),
            }),
            catalog_path: Some(catalog),
        };
        let out = ensure_fugu_profile(&detection).expect("provision profile");
        assert_eq!(out, home);
        let body = std::fs::read_to_string(home.join("fugu.config.toml")).unwrap();
        assert!(body.contains("model_provider = \"sakana\""));
        assert!(!body.contains("env_key"));
        // Second call: file already carries the selector → left untouched.
        assert_eq!(ensure_fugu_profile(&detection).expect("idempotent"), home);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ensure_fugu_profile_errors_without_provider() {
        let home = std::env::temp_dir().join(format!(
            "agend-fugu-profile-missing-provider-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        let detection = FuguDetection {
            status: FuguStatus::ConfiguredNoCredential,
            descriptor: FUGU_PROVIDER_DESCRIPTOR,
            codex_on_path: true,
            provider_source: Some(home.join("config.toml")),
            has_credential: false,
            models: Vec::new(),
            hints: Vec::new(),
            provider: None,
            catalog_path: None,
        };
        assert!(ensure_fugu_profile(&detection).is_err());
        std::fs::remove_dir_all(&home).ok();
    }
}
