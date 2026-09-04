use super::*;

// ── Δ3 v3: agent-name accept/reject matrix ──────────────────────────────

#[test]
fn agent_name_accepts_valid_names() {
    for v in [
        "a",
        "agent1",
        "run-ab12cd34ef56",
        "my.agent",
        "a-b_c.d",
        "z9",
        &"a".repeat(64),
    ] {
        assert!(validate_agent_name(v).is_ok(), "{v:?} should be accepted");
    }
}

#[test]
fn agent_name_rejects_v2_traversal_and_shape_cases() {
    // Original v2 test list: `../x`, `a/b`, `.hidden`, empty, >64 chars.
    for v in ["../x", "a/b", ".hidden", "", &"a".repeat(65)] {
        assert!(validate_agent_name(v).is_err(), "{v:?} must be rejected");
    }
}

#[test]
fn agent_name_rejects_v3_case_and_device_name_cases() {
    // Δ3 v3 round-2 additions: CON, con, Nul.log, Agent (uppercase), agent., com1.
    for v in ["CON", "con", "Nul.log", "Agent", "agent.", "com1"] {
        assert!(validate_agent_name(v).is_err(), "{v:?} must be rejected");
    }
}

#[test]
fn agent_name_rejects_leading_dot_or_dash() {
    for v in [".a", "-a"] {
        assert!(validate_agent_name(v).is_err(), "{v:?} must be rejected");
    }
}

#[test]
fn agent_name_reserved_device_name_check_is_stem_only() {
    // "console" is NOT "CON" — must not be over-blocked (full-stem match only).
    assert!(validate_agent_name("console").is_ok());
    assert!(validate_agent_name("commander").is_ok());
    // But "com1.log" IS blocked (stem before first '.' is "com1").
    assert!(validate_agent_name("com1.log").is_err());
}

#[test]
fn default_agent_name_is_always_valid() {
    for _ in 0..20 {
        let name = default_agent_name();
        assert!(
            validate_agent_name(&name).is_ok(),
            "generated default agent name {name:?} must itself be valid"
        );
        assert!(name.starts_with("run-"));
    }
}

// ── `run` argument parsing ───────────────────────────────────────────────

#[test]
fn parse_run_args_full_form() {
    let args: Vec<String> = [
        "--agent", "foo", "--branch", "feat/x", "--base", "origin/main", "--", "claude", "-x",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let parsed = parse_run_args(&args).expect("should parse");
    assert_eq!(parsed.agent.as_deref(), Some("foo"));
    assert_eq!(parsed.branch.as_deref(), Some("feat/x"));
    assert_eq!(parsed.base.as_deref(), Some("origin/main"));
    assert_eq!(parsed.cmd, vec!["claude".to_string(), "-x".to_string()]);
}

#[test]
fn parse_run_args_minimal_form() {
    let args: Vec<String> = ["--branch", "feat/x", "--", "claude"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let parsed = parse_run_args(&args).expect("should parse");
    assert_eq!(parsed.agent, None);
    assert_eq!(parsed.branch.as_deref(), Some("feat/x"));
    assert_eq!(parsed.base, None);
    assert_eq!(parsed.cmd, vec!["claude".to_string()]);
}

#[test]
fn parse_run_args_missing_separator_errors() {
    let args: Vec<String> = ["--branch", "feat/x"].iter().map(|s| s.to_string()).collect();
    assert!(parse_run_args(&args).is_err());
}

#[test]
fn parse_run_args_empty_command_after_separator_errors() {
    let args: Vec<String> = ["--branch", "feat/x", "--"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(parse_run_args(&args).is_err());
}

#[test]
fn parse_run_args_missing_flag_value_errors() {
    let args: Vec<String> = ["--branch"].iter().map(|s| s.to_string()).collect();
    assert!(parse_run_args(&args).is_err());
}

#[test]
fn parse_run_args_unknown_flag_errors() {
    let args: Vec<String> = ["--nope", "x", "--", "claude"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(parse_run_args(&args).is_err());
}

#[test]
fn parse_run_args_no_args_errors() {
    assert!(parse_run_args(&[]).is_err());
}

// ── default_branch ────────────────────────────────────────────────────────

#[test]
fn default_branch_embeds_agent_and_looks_like_a_timestamp() {
    let b = default_branch("my-agent");
    assert!(b.starts_with("agent/my-agent/"));
    let suffix = b.rsplit('/').next().unwrap();
    assert_eq!(suffix.len(), "yyyymmdd-hhmm".len());
    assert!(suffix.chars().all(|c| c.is_ascii_digit() || c == '-'));
}

// ── hex_lower ─────────────────────────────────────────────────────────────

#[test]
fn hex_lower_is_lowercase_and_correct_length() {
    let out = hex_lower(&[0xAB, 0x0F, 0x00]);
    assert_eq!(out, "ab0f00");
}

// ── Δ4 reuse predicate (unit-level, no real git — exercised via fixtures) ──

#[test]
fn check_reuse_none_when_no_binding_present() {
    let home = std::env::temp_dir().join(format!(
        "agentic-git-cli-reuse-none-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("runtime").join("agent-x")).unwrap();
    let result = check_reuse(
        &home,
        "agent-x",
        "feat/x",
        Path::new("/nonexistent/wt"),
        Path::new("/nonexistent/repo"),
        "git",
    );
    assert!(result.is_none(), "no binding.json → None (fresh path)");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn check_reuse_branch_mismatch_is_hard_error() {
    let home = std::env::temp_dir().join(format!(
        "agentic-git-cli-reuse-mismatch-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let dir = home.join("runtime").join("agent-x");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::json!({
            "version": 1,
            "agent": "agent-x",
            "task_id": "run-session-1",
            "branch": "old-branch",
            "issued_at": "2026-01-01T00:00:00Z",
            "worktree": "/some/wt",
            "source_repo": "/some/repo",
        })
        .to_string(),
    )
    .unwrap();
    let result = check_reuse(
        &home,
        "agent-x",
        "new-branch",
        Path::new("/some/wt"),
        Path::new("/some/repo"),
        "git",
    );
    match result {
        Some(Err(reason)) => assert!(reason.contains("branch"), "reason should mention branch mismatch: {reason}"),
        other => panic!("expected Some(Err(..)) branch mismatch, got {other:?}"),
    }
    std::fs::remove_dir_all(&home).ok();
}

// (Δ2 `open_new_0600` 0600-at-birth test moved to agentic-git-core with the helper
// in P1a — see integrity_core::p1a_contract::key_tmp_file_is_0600_at_creation.)
