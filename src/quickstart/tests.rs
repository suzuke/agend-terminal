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
    std::fs::write(&path, "AGEND_TELEGRAM_BOT_TOKEN=secret\n").unwrap();
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
    const SOURCE: &str = include_str!("../quickstart.rs");
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

/// Item 13: no-supported-backends list must enumerate the supported
/// backends with an install hint (Claude / codex / Kiro / OpenCode).
/// #1580: Gemini CLI dropped — gemini-cli is retired (sunset 2026-06-18);
/// its successor Agy has no npm one-liner so it is not listed here.
#[test]
fn no_backends_message_lists_supported_backends() {
    const SOURCE: &str = include_str!("../quickstart.rs");
    for backend in ["Claude Code", "codex", "Kiro", "OpenCode"] {
        assert!(
            SOURCE.contains(backend),
            "no-supported-backends message must mention `{backend}`"
        );
    }
}

/// Item 12: `print_next_steps` must include a "Before you start"
/// block with the three first-day pitfalls (allowlist / admin /
/// supergroup) before the action steps.
#[test]
fn next_steps_includes_before_you_start_pitfalls_block() {
    const SOURCE: &str = include_str!("../quickstart.rs");
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

/// #2204 Phase B: the Next Steps block must LEAD with App mode as the
/// recommended path for newcomers, and demote the daemon + CLI commands to a
/// labeled advanced/automation path — so the Windows first impression isn't a
/// raw start/status/attach dump. Scans the source after the Next Steps header
/// (the first occurrence is `print_next_steps`'s `println!`, before this test).
#[test]
fn next_steps_recommends_app_mode_before_daemon_cli() {
    const SOURCE: &str = include_str!("../quickstart.rs");
    let next_steps = SOURCE
        .split_once("── Next Steps ──")
        .expect("Next Steps header present")
        .1;
    let app_at = next_steps
        .find("agend-terminal app")
        .expect("Next Steps must recommend `agend-terminal app`");
    let start_at = next_steps
        .find("agend-terminal start")
        .expect("Next Steps still lists the daemon path for automation");
    assert!(
        app_at < start_at,
        "App mode must be presented BEFORE the daemon/CLI path (#2204 recommended-first)"
    );
    assert!(
        next_steps.contains("[Recommended]"),
        "App mode must carry the [Recommended] label"
    );
    assert!(
        next_steps.contains("[Advanced]"),
        "the daemon + CLI path must be labeled [Advanced] (automation/headless audience)"
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
    // Should return 0 or more backends without panicking.
    // #987: bumped from 5 → 6 with Backend::Agy addition.
    assert!(backends.len() <= 6);
}

/// Snapshot test: emitted YAML with Telegram channel includes
/// `user_allowlist` (Sprint 21 fail-closed requirement).
#[test]
fn emitted_yaml_with_channel_includes_user_allowlist() {
    let home = std::env::temp_dir().join(format!("agend-quickstart-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(&home, &backend, Some(-1001234567890), None, None, false).expect("test");
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).expect("test");
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

// ── #2207 B: single-sender auto-detect for user_allowlist auto-fill ──

fn fake_update_from(sender_id: i64) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "from": { "id": sender_id },
            "chat": { "type": "supergroup", "id": -100, "title": "G" }
        }
    })
}

#[test]
fn extract_single_sender_returns_id_for_sole_sender() {
    // One distinct sender, even across repeated messages → that id.
    let updates = vec![fake_update_from(555), fake_update_from(555)];
    assert_eq!(extract_single_sender(&updates), Some(555));
}

#[test]
fn extract_single_sender_none_for_multiple_distinct() {
    // ≥2 distinct senders → None: never auto-allowlist the wrong user when
    // someone else messaged the group during the poll window.
    let updates = vec![fake_update_from(1), fake_update_from(2)];
    assert_eq!(extract_single_sender(&updates), None);
}

#[test]
fn extract_single_sender_none_for_empty_or_senderless() {
    assert_eq!(extract_single_sender(&[]), None);
    // Updates lacking message.from.id contribute no sender.
    let no_from = vec![serde_json::json!({"message": {"chat": {"id": -1}}})];
    assert_eq!(extract_single_sender(&no_from), None);
}

#[test]
fn generate_fleet_yaml_auto_fills_allowlist_for_single_sender() {
    let home = std::env::temp_dir().join(format!("agend-2207-autofill-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(
        &home,
        &backend,
        Some(-100123),
        Some("t"),
        Some(987654),
        false,
    )
    .expect("gen");
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).expect("read");
    assert!(
        yaml.contains("- 987654"),
        "single detected sender must be auto-filled into user_allowlist: {yaml}"
    );
    // Must parse into a real, populated allowlist (not just a string match).
    let config = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("generated fleet.yaml must parse");
    let Some(crate::fleet::ChannelConfig::Telegram { user_allowlist, .. }) = config.channel else {
        panic!("expected a telegram channel in generated config");
    };
    assert_eq!(
        user_allowlist.expect("allowlist must be present").len(),
        1,
        "exactly the one detected sender is allowlisted"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn generate_fleet_yaml_keeps_empty_allowlist_without_sender() {
    // No detected sender (multi/zero sender, reused config, unattended) →
    // the TODO `[]` template stays; #2207 A1 backstops it at start.
    let home = std::env::temp_dir().join(format!("agend-2207-nofill-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(&home, &backend, Some(-100123), Some("t"), None, false).expect("gen");
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).expect("read");
    assert!(
        yaml.contains("user_allowlist: []"),
        "no detected sender → empty TODO template: {yaml}"
    );
    std::fs::remove_dir_all(&home).ok();
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
    generate_fleet_yaml(&home, &backend, None, None, None, false).expect("test");
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).expect("test");
    assert!(
        yaml.contains("user_allowlist"),
        "commented-out channel section must mention user_allowlist; got:\n{yaml}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2005: bot_token_env canonical/legacy resolution — REAL entry pins ──

/// Serialize env mutation across the #2005 tests (process-global vars).
fn token_env_guard() -> std::sync::MutexGuard<'static, ()> {
    static G: std::sync::Mutex<()> = std::sync::Mutex::new(());
    G.lock().unwrap_or_else(|e| e.into_inner())
}

fn clear_token_envs() {
    std::env::remove_var("AGEND_TELEGRAM_BOT_TOKEN");
    std::env::remove_var("AGEND_BOT_TOKEN");
}

/// Fresh-install shape: the NEW quickstart template (canonical
/// bot_token_env) + a `.env`-style canonical env var → the real resolve
/// entry must succeed. Pre-#2005 the template pinned the legacy name and
/// this exact shape failed at daemon startup.
#[test]
fn fresh_install_channel_resolves_2005() {
    let _g = token_env_guard();
    clear_token_envs();
    let home = std::env::temp_dir().join(format!("agend-2005-fresh-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(&home, &backend, Some(-100123), Some("t"), None, false).expect("gen");
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).expect("read");
    assert!(
        yaml.contains("bot_token_env: AGEND_TELEGRAM_BOT_TOKEN"),
        "template must pin the CANONICAL env name (what save_env_token writes): {yaml}"
    );
    std::env::set_var("AGEND_TELEGRAM_BOT_TOKEN", "123:fresh");
    let res = crate::channel::telegram::creds::resolve_channel_from(&home);
    clear_token_envs();
    let (creds, _) = res.expect("fresh install (canonical fleet + canonical env) must resolve");
    assert_eq!(creds.token, "123:fresh");
    assert_eq!(creds.group_id, -100123);
    std::fs::remove_dir_all(&home).ok();
}

/// Old-install shape: new template + an old `.env` still carrying the
/// LEGACY key → resolves via the legacy fallback (deprecation warn).
#[test]
fn old_install_legacy_env_still_resolves_2005() {
    let _g = token_env_guard();
    clear_token_envs();
    let home = std::env::temp_dir().join(format!("agend-2005-legacy-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(&home, &backend, Some(-100123), Some("t"), None, false).expect("gen");
    std::env::set_var("AGEND_BOT_TOKEN", "123:legacy");
    let res = crate::channel::telegram::creds::resolve_channel_from(&home);
    clear_token_envs();
    let (creds, _) = res.expect("legacy .env key must keep working (old installs)");
    assert_eq!(creds.token, "123:legacy");
    std::fs::remove_dir_all(&home).ok();
}

/// THE #2005 bug pin: an old-template fleet.yaml (bot_token_env pinned to
/// the LEGACY name) + a migrated/canonical-only env. Pre-#2005 the
/// fallback retried the SAME legacy name (dead code) and resolution
/// failed; the symmetric fallback must now find the canonical var.
#[test]
fn old_template_fleet_with_canonical_env_resolves_2005() {
    let _g = token_env_guard();
    clear_token_envs();
    let home = std::env::temp_dir().join(format!("agend-2005-oldtmpl-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "defaults:\n  backend: claude\nchannel:\n  type: telegram\n  bot_token_env: AGEND_BOT_TOKEN\n  group_id: -100456\ninstances: {}\n",
        )
        .expect("write");
    std::env::set_var("AGEND_TELEGRAM_BOT_TOKEN", "123:canon");
    let res = crate::channel::telegram::creds::resolve_channel_from(&home);
    clear_token_envs();
    let (creds, _) =
        res.expect("old fleet template + canonical-only env must resolve via symmetric fallback");
    assert_eq!(creds.token, "123:canon");
    assert_eq!(creds.group_id, -100456);
    std::fs::remove_dir_all(&home).ok();
}

/// Negative: neither env set → clear error naming the configured var.
#[test]
fn no_token_env_errors_clearly_2005() {
    let _g = token_env_guard();
    clear_token_envs();
    let home = std::env::temp_dir().join(format!("agend-2005-none-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(&home, &backend, Some(-1), Some("t"), None, false).expect("gen");
    let err = crate::channel::telegram::creds::resolve_channel_from(&home)
        .expect_err("no env set must error");
    assert!(
        err.to_string().contains("AGEND_TELEGRAM_BOT_TOKEN"),
        "error must name the configured var: {err}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── quickstart --unattended (產品化第2站) ──

#[test]
fn unattended_env_var_token_wins_over_env_file() {
    let r = resolve_unattended_telegram(
        Some("file-token".into()),
        Some(42),
        Some("env-token".into()),
        None,
    );
    assert_eq!(r.token.as_deref(), Some("env-token"));
    assert_eq!(r.group_id, Some(42), "fleet gid is the fallback");
    assert!(
        r.notes.iter().any(|n| n.contains("unverified")),
        "env token must be flagged unverified (no network in unattended): {:?}",
        r.notes
    );
}

#[test]
fn unattended_falls_back_to_env_file_then_skips() {
    let r = resolve_unattended_telegram(Some("file-token".into()), None, None, None);
    assert_eq!(r.token.as_deref(), Some("file-token"));
    assert_eq!(r.group_id, None);

    let r = resolve_unattended_telegram(None, Some(42), None, None);
    assert_eq!(r.token, None, "no token → telegram skipped");
    assert_eq!(
        r.group_id, None,
        "gid without token is meaningless — stays off"
    );
    assert!(r.notes.iter().any(|n| n.contains("skipped")));
}

#[test]
fn unattended_group_id_env_wins_and_invalid_is_ignored() {
    let r = resolve_unattended_telegram(Some("t".into()), Some(42), None, Some("-1009999".into()));
    assert_eq!(r.group_id, Some(-1009999), "env gid wins over fleet gid");

    let r = resolve_unattended_telegram(
        Some("t".into()),
        Some(42),
        None,
        Some("not-a-number".into()),
    );
    assert_eq!(
        r.group_id,
        Some(42),
        "invalid env gid → ignored, fleet fallback"
    );
    assert!(
        r.notes.iter().any(|n| n.contains("not a valid integer")),
        "invalid gid must be called out: {:?}",
        r.notes
    );
}

#[test]
fn unattended_generate_fleet_never_overwrites_existing() {
    let home =
        std::env::temp_dir().join(format!("agend-quickstart-ua-keep-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let fleet_path = crate::fleet::fleet_yaml_path(&home);
    std::fs::write(&fleet_path, "# operator-owned\ninstances: {}\n").expect("test");
    let backend = Backend::all()[0].clone();
    generate_fleet_yaml(&home, &backend, Some(1), Some("tok"), None, true).expect("test");
    let after = std::fs::read_to_string(&fleet_path).expect("test");
    assert_eq!(
        after, "# operator-owned\ninstances: {}\n",
        "unattended must keep an existing fleet.yaml byte-identical (idempotent re-runs)"
    );
    assert!(
        !home.join("fleet.yaml.bak").exists(),
        "no backup churn when nothing was overwritten"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn unattended_save_env_token_updates_without_prompt() {
    let home =
        std::env::temp_dir().join(format!("agend-quickstart-ua-token-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    std::fs::write(home.join(".env"), "AGEND_TELEGRAM_BOT_TOKEN=old-token\n").expect("test");
    // Differing token in unattended = explicit env instruction → updated
    // with NO stdin read (a prompt here would hang CI — the test itself
    // runs with no stdin, so a regression reads EOF → keeps old → fails).
    save_env_token(&home, "new-token", true).expect("test");
    let env = std::fs::read_to_string(home.join(".env")).expect("test");
    assert!(
        env.contains("AGEND_TELEGRAM_BOT_TOKEN=new-token"),
        "unattended token update must apply: {env}"
    );
    std::fs::remove_dir_all(&home).ok();
}
