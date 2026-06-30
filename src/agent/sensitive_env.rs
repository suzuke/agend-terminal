//! Spawn-time sensitive-env deny-list (extracted from `agent/mod.rs` to keep
//! that file under its anti-monolith ceiling). Matching is case-insensitive
//! (`is_sensitive_env_key` in the parent module), so a pure case-sensitive
//! deny-list would miss e.g. `anthropic_api_key`.

/// Env-var names dropped from a spawned agent's environment.
pub(super) const SENSITIVE_ENV_KEYS: &[&str] = &[
    // API credentials for backends we drive
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    // Cloud credentials commonly present in dev environments
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    // Git forge tokens
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    // Dynamic-linker injection vectors (Linux / macOS)
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_FRAMEWORK_PATH",
    // AUDIT2-004: interpreter-level code-injection vectors. The dynamic-linker
    // vars above only cover native libs; these run arbitrary code in the exact
    // runtimes agend spawns — Node (Claude Code / opencode), the daemon's git
    // (every worktree/fetch/merge), and shells/python/perl/ruby. `create_instance`
    // accepts an agent-supplied `env` that is spawned AND persisted to fleet.yaml,
    // so leaving these open is a persistent code-exec/backdoor surface.
    "NODE_OPTIONS",
    "GIT_SSH_COMMAND",
    "BASH_ENV",
    "ENV",
    "PYTHONSTARTUP",
    "PERL5OPT",
    "RUBYOPT",
    // agend's own runtime wiring — overriding these lets a template redirect
    // the spawned agent to a different home / break MCP config discovery
    "AGEND_HOME",
    "AGEND_INSTANCE_NAME",
];
