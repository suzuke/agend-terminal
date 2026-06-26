use super::*;

// ── CR-2026-06-14: shim is_protected_ref must mirror the lib-side
// case-insensitive E4.5 guard (kept in sync; see agent_ops.rs). ──
#[test]
fn shim_is_protected_ref_case_insensitive() {
    for v in ["main", "master", "Main", "MAIN", "Master", "MASTER"] {
        assert!(is_protected_ref(v), "{v:?} must be protected");
    }
    // Full-string compare: substrings / case-only-prefix are not over-blocked.
    for v in [
        "mainline",
        "maintenance",
        "main-feature",
        "upstream-main",
        "",
    ] {
        assert!(!is_protected_ref(v), "{v:?} must NOT be protected");
    }
}

// ── #1651: binding.json HMAC verify (push-authority integrity) ──
fn home_1651(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("agend-git-1651-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    // The shared integrity key (both signer + verifier read it).
    std::fs::write(p.join(".config-integrity-key"), [7u8; 32]).unwrap();
    p
}

fn write_binding_1651(home: &Path, agent: &str, body: &str, signed: bool) {
    let dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("binding.json"), body).unwrap();
    if signed {
        let tag = integrity_core::sign_for_test(home, body.as_bytes());
        std::fs::write(dir.join("binding.json.sig"), tag).unwrap();
    }
}

// No `worktree` field → the orphan-worktree guard is skipped, so the binding
// outcome reflects the HMAC verify alone.
const BODY_1651: &str = r#"{"version":1,"task_id":"T-1","branch":"feat/x"}"#;

#[test]
fn valid_signed_binding_is_bound_1651() {
    let home = home_1651("valid");
    write_binding_1651(&home, "ag", BODY_1651, true);
    let b = read_binding(home.to_str().unwrap(), "ag");
    assert!(is_bound(&b), "a validly-signed binding must be bound");
    assert_eq!(b.branch.as_deref(), Some("feat/x"));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn tampered_branch_without_resign_is_unbound_and_denied_1651() {
    let home = home_1651("tamper");
    // Daemon wrote + signed the original (branch feat/x).
    write_binding_1651(&home, "ag", BODY_1651, true);
    // Injection blind-write: self-authorize `main`, WITHOUT re-signing
    // (the agent doesn't know the key/scheme).
    let tampered = r#"{"version":1,"task_id":"T-1","branch":"main"}"#;
    std::fs::write(
        home.join("runtime").join("ag").join("binding.json"),
        tampered,
    )
    .unwrap();
    let b = read_binding(home.to_str().unwrap(), "ag");
    assert!(
        !is_bound(&b),
        "#1651: a tampered (unsigned-for-new-content) binding must read as unbound"
    );
    // …and a mutating op on unbound takes the EXISTING fail-closed deny path.
    assert!(matches!(
        deny_unbound_else_chdir(is_bound(&b), &b),
        Action::Deny(_)
    ));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn missing_sidecar_is_unbound_1651() {
    let home = home_1651("nosig");
    write_binding_1651(&home, "ag", BODY_1651, false); // NO sidecar
    let b = read_binding(home.to_str().unwrap(), "ag");
    assert!(
        !is_bound(&b),
        "#1651: a binding with no HMAC sidecar must fail closed (unbound)"
    );
    std::fs::remove_dir_all(home).ok();
}

// ── #1463: init-heartbeat argv detection (forensic hook gate) ──
fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

// ── #1504 L2: shim self-exclusion via canonicalize + lexical fallback ──
#[test]
fn same_dir_lexical_slash_fallback_1504() {
    // Nonexistent dirs → lexical fallback → backslash normalized to `/`, so a
    // forward-slash `$AGEND_HOME/bin` still matches a backslash PATH entry
    // (the Windows self-exclusion miss that caused the recursion).
    assert!(same_dir(
        std::path::Path::new("C:/h/bin"),
        Some(std::path::Path::new("C:\\h\\bin")),
    ));
    assert!(!same_dir(
        std::path::Path::new("/usr/bin"),
        Some(std::path::Path::new("/h/bin")),
    ));
    assert!(!same_dir(std::path::Path::new("/usr/bin"), None));
}

#[test]
fn extract_commit_message_all_forms() {
    assert_eq!(
        extract_commit_message(&s(&["commit", "-m", "init"])),
        Some("init")
    );
    assert_eq!(
        extract_commit_message(&s(&["commit", "-minit"])),
        Some("init")
    );
    assert_eq!(
        extract_commit_message(&s(&["commit", "--message", "init"])),
        Some("init")
    );
    assert_eq!(
        extract_commit_message(&s(&["commit", "--message=init"])),
        Some("init")
    );
    assert_eq!(
        extract_commit_message(&s(&["commit", "--allow-empty"])),
        None
    );
}

#[test]
fn init_heartbeat_argv_detected() {
    assert!(commit_is_init_heartbeat_argv(&s(&[
        "commit",
        "--allow-empty",
        "-m",
        "init"
    ])));
    // `--allow-empty` not required (forensics errs toward catching).
    assert!(commit_is_init_heartbeat_argv(&s(&[
        "commit", "-m", "initial"
    ])));
    assert!(commit_is_init_heartbeat_argv(&s(&["commit", "-minit"])));
}

#[test]
fn init_heartbeat_argv_rejects_non_heartbeat() {
    // Real work commits / other subcommands must NOT trigger the hook.
    assert!(!commit_is_init_heartbeat_argv(&s(&[
        "commit",
        "-m",
        "fix: real work"
    ])));
    assert!(!commit_is_init_heartbeat_argv(&s(&[
        "commit",
        "--allow-empty"
    ])));
    assert!(!commit_is_init_heartbeat_argv(&s(&["status"])));
    assert!(!commit_is_init_heartbeat_argv(&s(&["push"])));
}

fn bound_binding(branch: &str, worktree: &str) -> Binding {
    Binding {
        task_id: Some("T-test".into()),
        branch: Some(branch.into()),
        worktree: Some(worktree.into()),
    }
}

// ── #1511: index-mutating plumbing folded into the mutating arm ──

/// Unbound `read-tree` (the bug shape: `git read-tree -m <base> a b` from a
/// canonical-rooted cwd) must now DENY instead of passing through to the
/// shared index. Pre-#1511 it fell to the `_` arm → `unbound → Passthrough`.
#[test]
fn read_tree_unbound_denied_1511() {
    let action = classify(
        "read-tree",
        &[
            "read-tree".into(),
            "-m".into(),
            "base".into(),
            "a".into(),
            "b".into(),
        ],
        &Binding::default(), // unbound
        false,
        false,
        true,
    );
    match action {
        Action::Deny(reason) => assert!(
            reason.contains("unbound"),
            "unbound read-tree must deny: {reason}"
        ),
        other => {
            panic!("unbound read-tree MUST deny (was the index-pollution hole), got {other:?}")
        }
    }
}

/// A BOUND agent's `read-tree` routes to its private worktree (ChdirPass)
/// regardless of cwd — so it never touches the canonical shared index. No
/// canonical_cwd gate is needed; ChdirPass redirects away from cwd.
#[test]
fn read_tree_bound_routes_to_worktree_1511() {
    let action = classify(
        "read-tree",
        &["read-tree".into(), "-m".into(), "base".into()],
        &bound_binding("feat/x", "/tmp/.worktrees/dev"),
        false,
        false,
        true,
    );
    assert_eq!(
        action,
        Action::ChdirPass("/tmp/.worktrees/dev".into()),
        "bound read-tree must route to the private worktree, not deny"
    );
}

/// #2027 chain precondition: a BOUND agent's ref-naming `git branch <name>`
/// lands in the read-only group → `ChdirPass(worktree)`. That ChdirPass is the
/// exact input `apply_foreign_repo_passthrough` must flip to `Passthrough` in a
/// foreign repo — pinning the full classify→apply chain that produced the
/// success-lie (the redirect ran the create against the worktree, so the
/// foreign repo silently got nothing yet the shim exited 0).
#[test]
fn bound_branch_create_classifies_to_chdirpass_2027() {
    let action = classify(
        "branch",
        &["branch".into(), "feat-x".into()],
        &bound_binding("feat/x", "/tmp/.worktrees/dev"),
        false,
        false,
        true,
    );
    assert_eq!(
        action,
        Action::ChdirPass("/tmp/.worktrees/dev".into()),
        "bound `git branch <name>` must classify to the read-only group's \
             ChdirPass — the #2027 redirect apply_foreign_repo_passthrough flips"
    );
}

/// `update-index` and `apply` join the same arm (clear index plumbing).
#[test]
fn update_index_and_apply_unbound_denied_1511() {
    for sub in ["update-index", "apply"] {
        let action = classify(sub, &[sub.into()], &Binding::default(), false, false, true);
        assert!(
            matches!(action, Action::Deny(_)),
            "unbound {sub} must deny, got {action:?}"
        );
    }
}

/// Precise-match guard: `merge-tree` is READ-ONLY (writes only to the object
/// DB, never the index) — it must NOT be caught by the `read-tree`/`merge`
/// tokens. It falls to the `_` arm → unbound Passthrough, so the daemon's
/// `merge-tree --write-tree` conflict check (and agents using it) keep working.
#[test]
fn merge_tree_not_caught_by_1511() {
    let action = classify(
        "merge-tree",
        &[
            "merge-tree".into(),
            "--write-tree".into(),
            "a".into(),
            "b".into(),
        ],
        &Binding::default(),
        false,
        false,
        true,
    );
    assert_eq!(
        action,
        Action::Passthrough,
        "merge-tree is read-only and must NOT be denied/caught by #1511"
    );
}

// ── #1511 follow-up: flag-discriminated index/ref plumbing ──

/// The MUTATING forms — `restore --staged`/`-S`, `update-ref` (always),
/// `symbolic-ref` write (`<name> <ref>` or `-d`) — must DENY when unbound
/// (closes the canonical-write hole they had via the `_` Passthrough arm).
#[test]
fn fu1511_mutating_plumbing_forms_unbound_denied() {
    let cases: &[&[&str]] = &[
        &["restore", "--staged", "file.rs"],
        &["restore", "-S", "file.rs"],
        &["restore", "--staged", "--worktree", "file.rs"], // both → has --staged
        &["update-ref", "refs/heads/main", "deadbeef"],
        &["update-ref", "-d", "refs/heads/tmp"],
        &["symbolic-ref", "HEAD", "refs/heads/feat"], // 2 non-flag args → write
        &["symbolic-ref", "-d", "HEAD"],              // delete → write
    ];
    for argv in cases {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let action = classify(argv[0], &args, &Binding::default(), false, false, true);
        assert!(
            matches!(action, Action::Deny(ref reason) if reason.contains("unbound")),
            "unbound `{}` must deny, got {action:?}",
            argv.join(" ")
        );
    }
}

/// The READ / working-tree forms must NOT be over-denied (the operator's
/// "don't block bare restore" + don't break read-only `symbolic-ref`):
/// they fall through to `unbound → Passthrough` like any read-only command.
#[test]
fn fu1511_readonly_and_worktree_forms_not_overdenied() {
    let cases: &[&[&str]] = &[
        &["restore", "file.rs"],               // bare → working tree
        &["restore", "--worktree", "file.rs"], // explicit working tree, no --staged
        &["symbolic-ref", "HEAD"],             // 1 non-flag arg → read
        &["symbolic-ref", "--short", "HEAD"],  // read with a flag
    ];
    for argv in cases {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let action = classify(argv[0], &args, &Binding::default(), false, false, true);
        assert_eq!(
            action,
            Action::Passthrough,
            "unbound read/working-tree form `{}` must NOT be denied",
            argv.join(" ")
        );
    }
}

/// A BOUND agent's mutating-plumbing forms route to its PRIVATE worktree
/// (ChdirPass), never deny. (Shared-ref caveat noted in the arm: ChdirPass
/// can't isolate ref writes, but bound agents are trusted — Policy A.)
#[test]
fn fu1511_bound_routes_to_worktree() {
    let wt = "/tmp/.worktrees/dev";
    let cases: &[&[&str]] = &[
        &["restore", "--staged", "file.rs"],
        &["update-ref", "refs/heads/main", "deadbeef"],
        &["symbolic-ref", "HEAD", "refs/heads/feat"],
    ];
    for argv in cases {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let action = classify(
            argv[0],
            &args,
            &bound_binding("feat/x", wt),
            false,
            false,
            true,
        );
        assert_eq!(
            action,
            Action::ChdirPass(wt.into()),
            "bound `{}` must route to the worktree",
            argv.join(" ")
        );
    }
}

/// `git reset --hard` stays the agent's self-recovery tool: a bound agent
/// must be able to run it (routes to its worktree), not be blocked.
#[test]
fn reset_hard_not_blocked_for_bound_agent_1511() {
    let action = classify(
        "reset",
        &["reset".into(), "--hard".into(), "origin/main".into()],
        &bound_binding("feat/x", "/tmp/.worktrees/dev"),
        false,
        false,
        true,
    );
    assert_eq!(
        action,
        Action::ChdirPass("/tmp/.worktrees/dev".into()),
        "reset --hard must remain available to a bound agent (self-recovery)"
    );
}

#[test]
fn deny_hint_lists_all_three_bypass_forms() {
    let lines = format_deny_error("commit", "unbound", "dev", None);
    let joined = lines.join("\n");
    for var in [
        "AGEND_GIT_BYPASS=1",
        "AGEND_GIT_BYPASS_AGENT=",
        "AGEND_GIT_BYPASS_UNTIL=",
    ] {
        assert!(
            joined.contains(var),
            "deny hint must list {var}, got:\n{joined}"
        );
    }
    assert!(
        joined.contains("epoch") && joined.contains("Unix seconds"),
        "AGEND_GIT_BYPASS_UNTIL hint must clarify epoch wording (not ISO), got:\n{joined}"
    );
}

/// #2379 ②: a BOUND caller's deny names its OWN worktree (branch + task) so the
/// remedy is "cd there", not just "bypass" — the in-scope binding context
/// (zero I/O). Also retains the 3-form bypass hint.
#[test]
fn deny_message_names_bound_worktree_2379() {
    let binding = Binding {
        task_id: Some("t-42".into()),
        branch: Some("feat/x".into()),
        worktree: Some("/wt/feat-x".into()),
    };
    let joined = format_deny_error("checkout", "cross-branch", "dev", Some(&binding)).join("\n");
    assert!(
        joined.contains("/wt/feat-x"),
        "must name the bound worktree, got:\n{joined}"
    );
    assert!(
        joined.contains("feat/x"),
        "must name the bound branch, got:\n{joined}"
    );
    assert!(
        joined.contains("t-42"),
        "must name the task, got:\n{joined}"
    );
    assert!(
        joined.contains("no bypass needed"),
        "bound remedy is 'cd there', got:\n{joined}"
    );
    assert!(
        joined.contains("AGEND_GIT_BYPASS=1"),
        "3-form bypass hint retained, got:\n{joined}"
    );
}

/// #2379 ②: an UNBOUND caller (empty binding) and a no-binding deny (`None`,
/// the early canonical-bypass path) both get the SAME generic "get a worktree"
/// remedy via the shared builder.
#[test]
fn deny_message_unbound_points_at_getting_a_worktree_2379() {
    let empty = Binding::default();
    for binding in [Some(&empty), None] {
        let joined = format_deny_error("commit", "unbound", "dev", binding).join("\n");
        assert!(
            joined.contains("binding_state"),
            "unbound remedy must mention binding_state, got:\n{joined}"
        );
        assert!(
            joined.contains("bind_self") || joined.contains("repo action=checkout"),
            "unbound remedy must point at provisioning a worktree, got:\n{joined}"
        );
    }
    // The shared builder is what the canonical-bypass deny (no Binding) reuses.
    assert!(
        deny_remedy_lines(None).join("\n").contains("binding_state"),
        "the shared remedy builder serves the no-binding deny too"
    );
}

/// #2379 ②: operator copy rule — deny prose must NOT use "security"/"安全"
/// wording. Guards every shared deny-copy builder (the bulk of the deny prose).
#[test]
fn deny_copy_has_no_security_wording_2379() {
    let bound = Binding {
        task_id: Some("t-1".into()),
        branch: Some("b".into()),
        worktree: Some("/wt".into()),
    };
    let mut corpus: Vec<String> = Vec::new();
    corpus.extend(format_deny_error("checkout", "r", "dev", Some(&bound)));
    corpus.extend(format_deny_error("commit", "r", "dev", None));
    corpus.extend(deny_remedy_lines(Some(&bound)));
    corpus.extend(deny_remedy_lines(None));
    // #2379 ② (r6 FIX1): the canonical-bypass deny prose has its own header —
    // it MUST be in the corpus or "security" could slip in there undetected
    // (the inline-eprintln form was a meta-test blind spot).
    corpus.extend(format_canonical_bypass_deny("dev", "worktree"));
    let joined = corpus.join("\n");
    assert!(
        !joined.to_lowercase().contains("security"),
        "deny copy must not use 'security' wording:\n{joined}"
    );
    assert!(
        !joined.contains("安全"),
        "deny copy must not use '安全' wording:\n{joined}"
    );
}

/// #2379 ② (r6 FIX2): a PARTIAL binding (worktree set but task_id=None) is
/// UNBOUND to production `is_bound` (task_id.is_some()) → classify denies it.
/// The remedy must NOT then claim "your assigned worktree is <that path>" (a
/// self-contradiction). It must use the generic remedy and never name the stale
/// path. RED pre-fix (deny_remedy_lines keyed on worktree.is_some() → named
/// `/other/path`); GREEN after keying on `is_bound`.
#[test]
fn deny_remedy_partial_binding_uses_generic_not_stale_path_2379() {
    let partial = Binding {
        task_id: None,
        branch: Some("feat/x".into()),
        worktree: Some("/other/path".into()),
    };
    assert!(
        !is_bound(&partial),
        "precondition: task_id=None ⇒ unbound to the production is_bound predicate"
    );
    let joined = deny_remedy_lines(Some(&partial)).join("\n");
    assert!(
        !joined.contains("/other/path"),
        "a partial binding (task_id=None) is denied as unbound — the remedy must NOT \
             name it as 'your assigned worktree' (would contradict the deny):\n{joined}"
    );
    assert!(
        joined.contains("binding_state") || joined.contains("bind_self"),
        "partial binding must fall through to the generic 'get a worktree' remedy:\n{joined}"
    );
}

// ----- Sprint 57 Wave 2 Track D — gh post-merge exemption -----

#[test]
fn gh_post_merge_checkout_exempted_from_e45_fence() {
    // Happy path: agent is bound to a feat branch, gh just merged
    // it + deleted remote, now runs `git checkout main` to clean
    // up local state. parent=gh signal fires → SilentExempt.
    let binding = bound_binding("sprint57-track-x", "/tmp/.worktrees/dev");
    let action = classify(
        "checkout",
        &["checkout".into(), "main".into()],
        &binding,
        true, // parent_is_gh = true
        false,
        false, // is_agent_caller — operator default; the gh-exemption
               // is independent of #852's agent-vs-operator gate
    );
    match action {
        Action::SilentExempt {
            target_branch,
            reason,
        } => {
            assert_eq!(target_branch, "main");
            assert!(
                reason.contains("gh post-merge"),
                "reason must label the exemption: {reason}"
            );
        }
        other => panic!("expected SilentExempt for gh post-merge cleanup, got {other:?}"),
    }
}

#[test]
fn gh_post_merge_exemption_also_covers_master() {
    // master is part of the protected set per `is_protected_ref`;
    // legacy repos using `master` as default branch must also
    // trigger the exemption.
    let binding = bound_binding("sprint57-track-y", "/tmp/.worktrees/dev");
    let action = classify(
        "checkout",
        &["checkout".into(), "master".into()],
        &binding,
        true,
        false,
        false, // is_agent_caller — operator default
    );
    assert!(
        matches!(action, Action::SilentExempt { .. }),
        "master target must also be exempted, got {action:?}"
    );
}

#[test]
fn interactive_checkout_to_main_still_blocked() {
    // Regression-proof of E4.5 normal protection: when parent is
    // NOT gh (interactive shell, script, IDE), the cross-branch
    // fence must still fire. Without this guarantee Track D
    // would silently weaken the rule.
    let binding = bound_binding("sprint57-track-z", "/tmp/.worktrees/dev");
    let action = classify(
        "checkout",
        &["checkout".into(), "main".into()],
        &binding,
        false, // parent_is_gh = false (interactive shell)
        false,
        false, // is_agent_caller — operator default
    );
    match action {
        Action::Deny(reason) => {
            assert!(
                reason.contains("cross-branch"),
                "interactive case must still trip the cross-branch fence: {reason}"
            );
            assert!(
                reason.contains("'main'"),
                "deny message must mention target branch: {reason}"
            );
        }
        other => panic!("interactive checkout to main MUST be denied, got {other:?}"),
    }
}

#[test]
fn switch_subcommand_also_routes_through_gate() {
    // `git switch main` is the modern equivalent of `git checkout
    // main`; the gate must apply to both subcommands so the
    // exemption + the normal block both work via either spelling.
    let binding = bound_binding("sprint57-track-q", "/tmp/.worktrees/dev");
    // gh path → exempt
    let action_gh = classify(
        "switch",
        &["switch".into(), "main".into()],
        &binding,
        true,
        false,
        false, // is_agent_caller — operator default
    );
    assert!(matches!(action_gh, Action::SilentExempt { .. }));
    // interactive path → deny
    let action_interactive = classify(
        "switch",
        &["switch".into(), "main".into()],
        &binding,
        false,
        false,
        false, // is_agent_caller — operator default
    );
    match action_interactive {
        Action::Deny(_) => {}
        other => panic!("interactive `switch main` must deny, got {other:?}"),
    }
}

#[test]
fn cross_branch_to_non_protected_target_never_exempted() {
    // Heuristic correctness: even with parent_is_gh=true, a
    // checkout to a NON-protected branch must still be denied.
    // The exemption is narrow by design — protected refs only.
    // gh in normal operation never checks out feature branches
    // post-merge, so this case represents a heuristic false-
    // positive boundary we explicitly guard.
    let binding = bound_binding("sprint57-track-r", "/tmp/.worktrees/dev");
    let action = classify(
        "checkout",
        &["checkout".into(), "feat-other".into()],
        &binding,
        true, // parent_is_gh — but target isn't protected.
        false,
        false, // is_agent_caller — operator default
    );
    match action {
        Action::Deny(reason) => {
            assert!(
                reason.contains("cross-branch"),
                "non-protected cross-branch must deny even with parent=gh: {reason}"
            );
        }
        other => panic!(
            "non-protected cross-branch with parent=gh must deny (NOT exempt), got {other:?}"
        ),
    }
}

#[test]
fn gh_invocation_detection_robust_against_simulated_external_invocation() {
    // The detection helper must reject `gh`-lookalike basenames
    // that aren't the canonical CLI binary. This pins the
    // basename matcher: only the literal `gh` (or `gh.exe`)
    // qualifies — common false-positives like `github`,
    // `gh-cli-helper`, or empty strings must NOT.
    assert!(process_basename_is_gh("gh"));
    assert!(process_basename_is_gh("/usr/local/bin/gh"));
    assert!(process_basename_is_gh("/opt/homebrew/bin/gh"));
    assert!(process_basename_is_gh(
        "C:\\Program Files\\GitHub CLI\\gh.exe"
    ));
    assert!(process_basename_is_gh("gh.exe"));

    // Negative cases — must NOT fire the heuristic.
    assert!(!process_basename_is_gh(""));
    assert!(!process_basename_is_gh("github"));
    assert!(!process_basename_is_gh("/usr/bin/github"));
    assert!(!process_basename_is_gh("gh-cli-helper"));
    assert!(!process_basename_is_gh("not-gh"));
    assert!(!process_basename_is_gh("/path/to/gh.sh")); // shell wrapper
    assert!(!process_basename_is_gh("agh")); // adjacent letters
}

#[test]
fn audit_event_logged_when_exemption_fires() {
    // Round-trip: classify produces SilentExempt → main() writes a
    // structured `post_merge_cleanup_exempt` event. We can't run
    // main() in a unit test (it calls std::process::exit), but
    // we can call the underlying `write_git_event_typed` writer
    // directly and assert the on-disk shape, which is what main
    // would emit.
    let home = std::env::temp_dir().join(format!(
        "agend-git-d-audit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).ok();

    write_git_event_typed(
        home.to_str().unwrap(),
        "dev",
        "checkout",
        "post_merge_cleanup_exempt",
        Some("main"),
        Some("gh post-merge cleanup checkout — test fixture"),
    );

    let events_path = home.join("fleet_events.jsonl");
    assert!(events_path.exists(), "audit event file must be created");

    let content = std::fs::read_to_string(&events_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    assert_eq!(v["kind"], "git_event");
    assert_eq!(v["event"], "post_merge_cleanup_exempt");
    assert_eq!(v["agent"], "dev");
    assert_eq!(v["subcommand"], "checkout");
    assert_eq!(v["target_branch"], "main");
    assert!(
        v["reason"]
            .as_str()
            .map(|s| s.contains("post-merge"))
            .unwrap_or(false),
        "reason must record the exemption rationale"
    );
    assert!(
        v["timestamp"].as_str().is_some(),
        "timestamp must be RFC3339 string"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deny_event_still_uses_typed_writer() {
    // Defensive bonus pin: the legacy `event="deny"` shape must
    // continue to work via the new `write_git_event_typed`
    // function. Previously the wrapper had a separate
    // `write_git_event` for deny-only; consolidating to a typed
    // writer must not change the on-disk shape for the deny
    // event-type so downstream parsers keep working.
    let home = std::env::temp_dir().join(format!(
        "agend-git-d-deny-audit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).ok();

    write_git_event_typed(
        home.to_str().unwrap(),
        "dev",
        "checkout",
        "deny",
        None,
        Some("cross-branch — assigned to 'feat-x', cannot switch to 'main'"),
    );

    let events_path = home.join("fleet_events.jsonl");
    let content = std::fs::read_to_string(&events_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    assert_eq!(v["event"], "deny");
    assert_eq!(v["target_branch"], serde_json::Value::Null);
    assert!(
        v["reason"]
            .as_str()
            .map(|s| s.contains("cross-branch"))
            .unwrap_or(false),
        "deny reason must round-trip"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── #2379 ② kind taxonomy: deny vs warn disposition ─────────────────

#[test]
fn disposition_for_covers_all_emitted_event_types_2379() {
    // Pin every event_type the shim emits to its disposition (single source of
    // truth). Reverse-mutation: flip any arm in `disposition_for` → this catches it.
    // A new event_type added without a mapping falls to the fail-closed Deny default
    // — add it here AND in disposition_for.
    assert_eq!(disposition_for("deny"), Disposition::Deny);
    assert_eq!(disposition_for("deny_trust_root"), Disposition::Deny);
    assert_eq!(disposition_for("deny_protected_ref"), Disposition::Deny);
    assert_eq!(disposition_for("cwd_worktree_drift"), Disposition::Warn);
    assert_eq!(disposition_for("git_conflict"), Disposition::Warn);
    assert_eq!(
        disposition_for("post_merge_cleanup_exempt"),
        Disposition::Info
    );
    // Fail-closed default: an unrecognized event_type reads as terminal, not advisory.
    assert_eq!(
        disposition_for("some_future_unmapped_event"),
        Disposition::Deny
    );
}

#[test]
fn git_event_carries_disposition_field_2379() {
    // The agent-facing routing axis must land in the JSON: deny→"deny", warn→"warn",
    // exemption→"info". RM: drop the `"disposition"` line in write_git_event_typed
    // (or flip disposition_for) → these fail. The envelope `kind` stays "git_event"
    // (no collision with the new axis).
    let home = std::env::temp_dir().join(format!(
        "agend-git-disp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).ok();
    let read_event = |event_type: &str| -> serde_json::Value {
        let p = home.join("fleet_events.jsonl");
        let _ = std::fs::remove_file(&p);
        write_git_event_typed(
            home.to_str().unwrap(),
            "dev",
            "checkout",
            event_type,
            None,
            Some("x"),
        );
        serde_json::from_str(std::fs::read_to_string(&p).unwrap().trim()).unwrap()
    };
    assert_eq!(read_event("deny")["disposition"], "deny");
    assert_eq!(read_event("deny_trust_root")["disposition"], "deny");
    assert_eq!(read_event("cwd_worktree_drift")["disposition"], "warn");
    assert_eq!(read_event("git_conflict")["disposition"], "warn");
    assert_eq!(
        read_event("post_merge_cleanup_exempt")["disposition"],
        "info"
    );
    assert_eq!(read_event("deny")["kind"], "git_event");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn conflict_guidance_emits_git_conflict_warn_event_2379() {
    // (b): a merge conflict is now mirrored into fleet_events as a WARN (was
    // stderr-only → invisible to fleet observers). RM: drop the write_git_event_typed
    // call in emit_conflict_guidance → no git_conflict line → this fails.
    let home = std::env::temp_dir().join(format!(
        "agend-git-conflict-evt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).ok();
    emit_conflict_guidance(home.to_str().unwrap(), "dev", "rebase");
    let content = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap();
    let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    assert_eq!(v["event"], "git_conflict");
    assert_eq!(
        v["disposition"], "warn",
        "a conflict is advisory (resolve + continue), not a deny"
    );
    assert_eq!(v["subcommand"], "rebase");
    std::fs::remove_dir_all(&home).ok();
}

// ── #2379 S3: protected-ref push deny (policy.toml override) ─────────

fn vargs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn home_s3(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("agend-git-s3-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    // The shared integrity key (signer + verifier read it).
    std::fs::write(p.join(".config-integrity-key"), [7u8; 32]).unwrap();
    p
}

fn write_policy(home: &std::path::Path, body: &str, signed: bool) {
    std::fs::write(home.join("policy.toml"), body).unwrap();
    if signed {
        let tag = integrity_core::sign_for_test(home, body.as_bytes());
        std::fs::write(home.join("policy.toml.sig"), tag).unwrap();
    }
}

#[test]
fn push_dest_refs_normalizes_refspec_targets_s3() {
    assert_eq!(
        push_dest_refs(&vargs(&["push", "origin", "HEAD:main"])),
        vec!["origin", "main"]
    );
    // force markers (+ prefix) + refs/heads/ prefix + delete (:ref) all normalize.
    assert_eq!(
        push_dest_refs(&vargs(&["push", "origin", "+HEAD:refs/heads/main"])),
        vec!["origin", "main"]
    );
    assert_eq!(
        push_dest_refs(&vargs(&["push", "origin", ":master"])),
        vec!["origin", "master"]
    );
    // flags skipped; a normal task-branch push leaves a non-protected dest.
    assert_eq!(
        push_dest_refs(&vargs(&["push", "--force", "-u", "origin", "feat/x"])),
        vec!["origin", "feat/x"]
    );
    assert!(push_dest_refs(&vargs(&["push"])).is_empty());
}

#[test]
fn push_protected_violation_explicit_refspec_s3() {
    let p = vargs(&["main", "master", "release-1.0"]);
    let denied = |a: &[&str]| push_protected_violation(&vargs(a), &p, false);
    // explicit protected dest → deny (message names the ref).
    assert!(denied(&["push", "origin", "HEAD:main"])
        .unwrap()
        .contains("main"));
    // override-added ref → deny.
    assert!(denied(&["push", "origin", "HEAD:release-1.0"])
        .unwrap()
        .contains("release-1.0"));
    // case-insensitive (mirrors is_protected_ref's Main→main fold).
    assert!(denied(&["push", "origin", "HEAD:Main"]).is_some());
    // delete a protected ref (`:main`) → deny.
    assert!(denied(&["push", "origin", ":main"]).is_some());
    // the agent's OWN task branch is allowed (zero regression to normal pushes).
    assert!(denied(&["push", "-u", "origin", "feat/x"]).is_none());
    assert!(denied(&["push"]).is_none());
    assert!(denied(&["push", "origin"]).is_none());
}

#[test]
fn push_protected_violation_bulk_and_wildcard_forms_s3() {
    // r6's bypass: `--all`/`--mirror` (and abbreviations) push EVERY local head — must
    // deny regardless of positionals. RM: drop the is_bulk_push_flag branch → RED.
    let p = vargs(&["main", "master"]);
    let denied = |a: &[&str]| push_protected_violation(&vargs(a), &p, false);
    assert!(denied(&["push", "origin", "--all"]).is_some());
    assert!(denied(&["push", "--mirror", "origin"]).is_some());
    assert!(denied(&["push", "--all"]).is_some());
    // unambiguous abbreviations git accepts.
    assert!(denied(&["push", "origin", "--mir"]).is_some());
    assert!(denied(&["push", "origin", "--al"]).is_some());
    // wildcard refspec that could write a protected ref → deny.
    assert!(denied(&["push", "origin", "refs/heads/*:refs/heads/*"]).is_some());
    assert!(denied(&["push", "origin", "+HEAD:refs/heads/*"]).is_some());
    // safe-by-shape: --tags pushes refs/tags/*, never a protected BRANCH → allowed.
    assert!(denied(&["push", "--tags", "origin"]).is_none());
    // --atomic is not a bulk flag (shares no prefix with all/mirror).
    assert!(denied(&["push", "--atomic", "origin", "feat/x"]).is_none());
}

#[test]
fn push_protected_violation_push_default_matching_s3() {
    // no-refspec push under the DEPRECATED push.default=matching pushes every same-named
    // branch (incl. a local protected ref) → deny. simple/current (matching=false) →
    // allow (only the current/assigned branch). RM: drop the matching branch → first RED.
    let p = vargs(&["main", "master"]);
    assert!(push_protected_violation(&vargs(&["push"]), &p, true).is_some());
    assert!(push_protected_violation(&vargs(&["push", "origin"]), &p, true).is_some());
    // an EXPLICIT refspec governs even under matching → only that dest matters.
    assert!(push_protected_violation(&vargs(&["push", "origin", "feat/x"]), &p, true).is_none());
    // not matching → no-refspec push is safe (current branch only).
    assert!(push_protected_violation(&vargs(&["push"]), &p, false).is_none());
    // r6: `--tags` is TAGS-ONLY (refs/tags/*) even under matching → MUST be allowed
    // (the previous cut wrongly denied it). RM: drop the is_tags_only_push exemption → RED.
    assert!(push_protected_violation(&vargs(&["push", "--tags", "origin"]), &p, true).is_none());
    assert!(push_protected_violation(&vargs(&["push", "--tags"]), &p, true).is_none());
    // `--follow-tags` is NOT tags-only — under matching it pushes the matching branches
    // (incl. main, verified by dry-run) → MUST stay denied (no over-exemption).
    assert!(
        push_protected_violation(&vargs(&["push", "--follow-tags", "origin"]), &p, true).is_some()
    );
}

#[test]
fn push_head_main_denied_by_hardcode_floor_even_without_policy_s3() {
    // THE core deny: with NO policy.toml, `push origin HEAD:main` is still denied by the
    // hardcode floor; a normal task-branch push is allowed. RM: neuter
    // push_protected_violation, OR load_protected_refs drop the floor → RED.
    let h = home_s3("e2e");
    let protected = load_protected_refs(h.to_str().unwrap());
    assert!(
        push_protected_violation(&vargs(&["push", "origin", "HEAD:main"]), &protected, false)
            .is_some()
    );
    assert!(push_protected_violation(
        &vargs(&["push", "-u", "origin", "feat/x"]),
        &protected,
        false
    )
    .is_none());
    std::fs::remove_dir_all(&h).ok();
}

#[test]
fn is_bulk_push_flag_matches_all_mirror_not_others_s3() {
    for f in ["--all", "--mirror", "--al", "--mir", "--a", "--m"] {
        assert!(is_bulk_push_flag(f), "{f} must be a bulk-push flag");
    }
    for f in [
        "--atomic",
        "--tags",
        "--follow-tags",
        "--force",
        "-f",
        "--",
        "origin",
        "HEAD:x",
    ] {
        assert!(!is_bulk_push_flag(f), "{f} must NOT be a bulk-push flag");
    }
}

#[test]
fn push_default_is_matching_reads_config_s3() {
    let dir = std::env::temp_dir().join(format!(
        "agend-git-s3pd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
    };
    git(&["init"]);
    let wt = dir.to_str().unwrap();
    // unset → git's built-in `simple` → false.
    assert!(!push_default_is_matching(wt));
    // the deprecated bulk mode → true.
    git(&["config", "push.default", "matching"]);
    assert!(push_default_is_matching(wt));
    // a safe mode → false.
    git(&["config", "push.default", "current"]);
    assert!(!push_default_is_matching(wt));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_protected_refs_fail_closed_s3() {
    // missing policy.toml → hardcode floor only (the common default).
    let h = home_s3("missing");
    assert_eq!(
        load_protected_refs(h.to_str().unwrap()),
        vargs(&["main", "master"])
    );
    std::fs::remove_dir_all(&h).ok();

    // present + SIGNED + valid → override ADDED (tighten-only).
    let h = home_s3("signed");
    write_policy(&h, "protected_refs = [\"release-1.0\"]\n", true);
    let got = load_protected_refs(h.to_str().unwrap());
    assert!(got.contains(&"main".to_string()) && got.contains(&"release-1.0".to_string()));
    std::fs::remove_dir_all(&h).ok();

    // present but UNSIGNED → fail-closed (override ignored, floor remains).
    let h = home_s3("unsigned");
    write_policy(&h, "protected_refs = [\"release-1.0\"]\n", false);
    let got = load_protected_refs(h.to_str().unwrap());
    assert!(
        !got.contains(&"release-1.0".to_string()) && got.contains(&"main".to_string()),
        "unsigned override must be ignored, floor preserved"
    );
    std::fs::remove_dir_all(&h).ok();

    // signed then TAMPERED (sig no longer matches) → fail-closed.
    let h = home_s3("tampered");
    write_policy(&h, "protected_refs = [\"release-1.0\"]\n", true);
    std::fs::write(h.join("policy.toml"), "protected_refs = [\"sneaky\"]\n").unwrap();
    let got = load_protected_refs(h.to_str().unwrap());
    assert!(
        !got.contains(&"sneaky".to_string()) && got.contains(&"main".to_string()),
        "tampered override must be ignored, floor preserved"
    );
    std::fs::remove_dir_all(&h).ok();

    // signed but CORRUPT array (unterminated) → fail-closed to floor only.
    let h = home_s3("corrupt");
    write_policy(&h, "protected_refs = [ not valid\n", true);
    assert_eq!(
        load_protected_refs(h.to_str().unwrap()),
        vargs(&["main", "master"])
    );
    std::fs::remove_dir_all(&h).ok();
}

#[test]
fn parse_protected_refs_handles_array_forms_s3() {
    assert_eq!(
        parse_protected_refs("protected_refs = [\"main\", \"release-1\"]"),
        vargs(&["main", "release-1"])
    );
    // multi-line + trailing comma.
    assert_eq!(
        parse_protected_refs("protected_refs = [\n  \"a\",\n  \"b\",\n]"),
        vargs(&["a", "b"])
    );
    // other keys + a comment before the key.
    assert_eq!(
        parse_protected_refs("# policy\nother = 1\nprotected_refs = [\"x\"]\n"),
        vargs(&["x"])
    );
    assert!(parse_protected_refs("other = [\"y\"]").is_empty()); // no key
    assert!(parse_protected_refs("protected_refs = [\"a\"").is_empty()); // unterminated
    assert!(parse_protected_refs("protected_refs = []").is_empty()); // empty array
}

// ----- #778 Option 3 — canonical-worktree leniency for unbound -----

#[test]
fn p778_unbound_canonical_worktree_checkout_branch_passes_through() {
    // Empirical regression-proof anchor for #778 Option 3:
    // commenting out the `if !target_branch.is_empty() && ...
    // canonical_cwd { Action::Passthrough }` block makes this
    // FAIL with Action::Deny.
    let action = classify(
        "checkout",
        &["checkout".into(), "feat/p778".into()],
        &Binding::default(), // unbound
        false,               // parent_is_gh = no
        true,                // canonical_cwd = yes
        false,               // is_agent_caller — operator default; the
                             // #778 leniency must still fire for the
                             // operator-driven validation-canary flow
    );
    assert!(
        matches!(action, Action::Passthrough),
        "unbound + canonical worktree + positional branch must Passthrough, got {action:?}"
    );
}

#[test]
fn p778_unbound_canonical_switch_subcommand_also_passes() {
    // `git switch` is the modern equivalent and must benefit from
    // the same leniency — otherwise the rule is partial and the
    // validation-canary workflow stays broken on the recommended
    // `switch` path.
    let action = classify(
        "switch",
        &["switch".into(), "feat/p778".into()],
        &Binding::default(),
        false,
        true,
        false, // is_agent_caller — operator default
    );
    assert!(
        matches!(action, Action::Passthrough),
        "switch must also benefit from the leniency, got {action:?}"
    );
}

#[test]
fn p778_unbound_non_canonical_worktree_still_denied() {
    // Negative: when cwd is not a canonical worktree (placeholder
    // repo with no origin, or no worktree at all), the original
    // unbound deny must still fire — this is the security
    // guarantee that keeps the leniency narrow.
    let action = classify(
        "checkout",
        &["checkout".into(), "feat/p778".into()],
        &Binding::default(),
        false,
        false, // canonical_cwd = no
        false, // is_agent_caller — operator default
    );
    match action {
        Action::Deny(reason) => assert!(
            reason.contains("unbound"),
            "non-canonical cwd must keep the unbound deny: {reason}"
        ),
        other => panic!("non-canonical unbound must deny, got {other:?}"),
    }
}

#[test]
fn p778_unbound_canonical_flag_arg_still_denied() {
    // Heuristic safety: when the next arg is a flag (`-b
    // newbranch`, `-B foo`, `--orphan`) the leniency must NOT
    // fire — those create branches or detach in ways that aren't
    // "just navigation". Keep the deny for the unbound case so
    // we don't accidentally widen the surface.
    let action = classify(
        "checkout",
        &["checkout".into(), "-b".into(), "evil".into()],
        &Binding::default(),
        false,
        true,  // canonical_cwd = yes, but arg is a flag
        false, // is_agent_caller — operator default
    );
    match action {
        Action::Deny(reason) => assert!(
            reason.contains("unbound"),
            "flag arg in unbound canonical must deny: {reason}"
        ),
        other => panic!("flag arg leniency leak: {other:?}"),
    }
}

#[test]
fn p778_unbound_canonical_no_branch_arg_still_denied() {
    // `git checkout` with no positional branch (just to inspect
    // status) shouldn't even hit the leniency block — keep the
    // existing unbound deny for the no-target case.
    let action = classify(
        "checkout",
        &["checkout".into()],
        &Binding::default(),
        false,
        true,
        false, // is_agent_caller — operator default
    );
    match action {
        Action::Deny(reason) => assert!(reason.contains("unbound"), "got {reason}"),
        other => panic!("no-arg unbound must deny, got {other:?}"),
    }
}

#[test]
fn p778_bound_path_unchanged_when_canonical_cwd_true() {
    // Regression-proof of the bound path: canonical_cwd must NOT
    // alter behavior when the agent is bound. The existing
    // cross-branch check + ChdirPass dispatch are the source of
    // truth; the leniency only opens when bound=false.
    let binding = bound_binding("feat/p778", "/tmp/.worktrees/dev");
    let action = classify(
        "checkout",
        &["checkout".into(), "feat/p778".into()],
        &binding,
        false,
        true,  // canonical_cwd — should NOT route through leniency
        false, // is_agent_caller — operator default
    );
    match action {
        Action::ChdirPass(ref wt) => assert_eq!(wt, "/tmp/.worktrees/dev"),
        other => panic!("bound same-branch must ChdirPass, got {other:?}"),
    }
}

// ----- #852 PR-B — agent caller + canonical cwd → Deny -----
//
// The pre-#852 `!bound + canonical_cwd + positional non-flag arg →
// Passthrough` leniency was designed for the operator-typed
// validation-canary flow (`repo action=checkout` provisions a
// worktree in detached-HEAD; operator's natural `git switch
// <branch>` follow-up needed to pass). It accidentally also
// covered agent callers whose binding lookup failed for the
// current cwd — reviewers especially, who inspect PRs via
// canonical-rooted worktrees and end up creating `pr*_head` /
// `tmp*` / `review/*` refs that pollute the canonical's branch
// list. PR-B gates the leniency on agent-vs-operator identity:
// operators keep the leniency, agents are routed to the
// `repo action=checkout bind=true` MCP tool (which gives them a
// properly-bound worktree) or `gh pr diff/view` (read-only).

/// #852 PR-B core: when caller is an agent (AGEND_INSTANCE_NAME
/// set) AND cwd is a canonical-rooted worktree, the leniency must
/// NOT fire — checkout is denied with an actionable hint pointing
/// to the supported alternatives.
#[test]
fn shim_denies_agent_checkout_in_canonical() {
    let action = classify(
        "checkout",
        &["checkout".into(), "abc1234".into()], // SHA — reviewer's
        // "let me see this
        // PR's tree" workflow
        &Binding::default(), // unbound (binding lookup failed for
        // canonical cwd)
        false, // parent_is_gh = no
        true,  // canonical_cwd = yes
        true,  // is_agent_caller = yes
    );
    match action {
        Action::Deny(reason) => {
            assert!(
                reason.contains("agent"),
                "deny reason must explicitly call out the agent-caller \
                     identity so reviewers see WHY their workflow is rejected: \
                     {reason}"
            );
            assert!(
                reason.contains("repo action=checkout") || reason.contains("gh pr diff"),
                "deny reason must surface the supported alternative \
                     (repo action=checkout MCP or gh pr diff): {reason}"
            );
            assert!(
                reason.contains("#852"),
                "deny reason should reference the issue for operator \
                     traceability: {reason}"
            );
        }
        other => panic!(
            "agent caller in canonical worktree must Deny, not {other:?} \
                 — that's the reviewer-pollution bug fix"
        ),
    }
}

/// #852 PR-B operator preservation: when caller is NOT an agent
/// (operator's interactive shell, no AGEND_INSTANCE_NAME), the
/// existing #778 leniency must continue to fire — the validation-
/// canary flow must not regress.
#[test]
fn shim_allows_operator_checkout_in_canonical() {
    let action = classify(
        "checkout",
        &["checkout".into(), "feat/canary".into()],
        &Binding::default(),
        false, // parent_is_gh = no
        true,  // canonical_cwd = yes
        false, // is_agent_caller = no (operator shell)
    );
    assert!(
        matches!(action, Action::Passthrough),
        "operator in canonical worktree must keep the #778 leniency, \
             got {action:?}"
    );
}

/// #852 PR-B narrowness check: when the agent IS a caller but cwd
/// is NOT canonical (e.g. agent invoked git from a non-worktree
/// path), the gate must NOT fire — only the canonical-pollution
/// surface is targeted. Operator's `unbound + non-canonical →
/// Deny` outcome is preserved (different code path).
#[test]
fn shim_agent_outside_canonical_unchanged() {
    let action = classify(
        "checkout",
        &["checkout".into(), "feat/x".into()],
        &Binding::default(),
        false, // parent_is_gh = no
        false, // canonical_cwd = NO — gate must NOT fire
        true,  // is_agent_caller = yes
    );
    // Falls through to the existing `unbound — no active task
    // assignment` Deny (different from the new #852 Deny). The
    // pre-existing safety net stays intact.
    match action {
        Action::Deny(reason) => {
            assert!(
                reason.contains("unbound"),
                "non-canonical agent path must keep the original \
                     unbound deny (not the new #852 agent-canonical deny): \
                     {reason}"
            );
            assert!(
                !reason.contains("#852"),
                "non-canonical agent path must NOT trigger the #852 \
                     gate (gate is narrow by design): {reason}"
            );
        }
        other => panic!(
            "non-canonical unbound must keep the pre-existing deny, \
                 got {other:?}"
        ),
    }
}

// ----- #852 residual PR-A — cwd_is_canonical_rooted detection -----
//
// Pre-#852-residual, the detection helper required `.git` to be a
// FILE (worktree marker). This excluded canonical source repos
// where `.git` is a DIRECTORY — reviewers `cd`'ing into source
// sailed past the `is_agent_caller && canonical_cwd` deny because
// canonical_cwd was always false for source. Operator's reflog
// evidenced two such checkouts (21:46 + 22:24 today) AFTER
// #858+#859 shipped. PR-A broadens the helper to cover BOTH
// shapes.
//
// Tests use `std::env::set_current_dir` to position cwd inside
// the synthetic fixtures. Mutex-serialized so parallel test
// threads don't race the process-global cwd.

fn with_cwd<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
    use std::sync::{Mutex, OnceLock};
    static CWD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = CWD_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let prior = std::env::current_dir().expect("snapshot cwd");
    std::env::set_current_dir(dir).expect("set test cwd");
    let result = f();
    std::env::set_current_dir(&prior).expect("restore cwd");
    result
}

fn make_source_repo_with_origin(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agend-852-pr-a-source-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("mkdir source-base");
    let repo = base.join("repo");
    let git_dir = repo.join(".git");
    std::fs::create_dir_all(&git_dir).expect("mkdir .git");
    // Synthetic config: matches the canonical-detection criterion
    // (contains `[remote "origin"]`).
    std::fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\
             [remote \"origin\"]\n\turl = https://example.test/foo.git\n",
    )
    .expect("write .git/config");
    repo
}

fn make_source_repo_without_origin(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agend-852-pr-a-no-origin-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("mkdir no-origin-base");
    let repo = base.join("repo");
    let git_dir = repo.join(".git");
    std::fs::create_dir_all(&git_dir).expect("mkdir .git");
    // Orphan workspace-placeholder shape: `.git` directory but
    // no `[remote "origin"]`. Daemon startup creates these
    // before fleet config resolves; they must NOT trigger the
    // canonical-rooted gate.
    std::fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    )
    .expect("write .git/config");
    repo
}

fn make_canonical_worktree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    // Two-step: build a source repo with origin, then a synthetic
    // worktree pointing into it via the gitdir: marker. Mirrors
    // git's real worktree layout at <source>/.git/worktrees/<name>.
    let base = std::env::temp_dir().join(format!("agend-852-pr-a-wt-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("mkdir wt-base");
    let source = base.join("source");
    let source_git = source.join(".git");
    let worktrees_dir = source_git.join("worktrees").join("agent-1");
    std::fs::create_dir_all(&worktrees_dir).expect("mkdir worktree entry");
    std::fs::write(
        source_git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\
             [remote \"origin\"]\n\turl = https://example.test/foo.git\n",
    )
    .expect("write source .git/config");
    // Worktree dir with `.git` FILE pointing at the worktrees entry.
    let wt = base.join("worktree-cwd");
    std::fs::create_dir_all(&wt).expect("mkdir worktree dir");
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", worktrees_dir.display()),
    )
    .expect("write worktree .git pointer");
    (wt, base)
}

fn cleanup_base(repo: &std::path::Path) {
    if let Some(base) = repo.parent() {
        let _ = std::fs::remove_dir_all(base);
    }
}

/// #2234 defect#2: the pure gate for the non-agent canonical-checkout log.
/// `checkout`/`switch <branch>` (positional, non-flag target) → true; a
/// flag-led / empty target or a non-checkout subcommand → false.
#[test]
fn is_positional_branch_checkout_gate() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // canonical-HEAD-touching nav shapes → log candidate.
    assert!(is_positional_branch_checkout(&s(&[
        "checkout",
        "origin/main"
    ])));
    assert!(is_positional_branch_checkout(&s(&["switch", "main"])));
    assert!(is_positional_branch_checkout(&s(&[
        "checkout",
        "feature/x",
        "--",
        "file"
    ])));
    // not a branch nav / not checkout → not a candidate.
    assert!(!is_positional_branch_checkout(&s(&["checkout"])));
    assert!(!is_positional_branch_checkout(&s(&[
        "checkout", "--detach"
    ])));
    assert!(!is_positional_branch_checkout(&s(&[
        "checkout", "-b", "tmp"
    ])));
    assert!(!is_positional_branch_checkout(&s(&["status"])));
    assert!(!is_positional_branch_checkout(&s(&["commit", "main"])));
}

/// #2234 Patch A: leading `-C` resolution for the deny's effective cwd —
/// absolute target wins, repeated/relative `-C` accumulate, a value-taking
/// global before `-C` is skipped WITH its value, and no `-C` leaves the
/// process cwd untouched.
#[test]
fn effective_cwd_through_globals_resolves_leading_dash_c() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // A REAL absolute base (drive-qualified on Windows, `/`-rooted on Unix) so
    // `is_absolute()` agrees with the assertion on every platform. A hardcoded
    // `/abs/...` is NOT absolute on Windows — it would resolve drive-relative
    // (`D:/abs/...`) and only this test (not the production logic) would diverge.
    let abs = std::env::current_dir().expect("cwd").join("canon-fixture");
    let abs_s = abs.to_str().expect("utf8 path");
    // Absolute `-C <path>` → that path, independent of the process cwd.
    let args = s(&["-C", abs_s, "worktree", "add", "x"]);
    let idx = subcommand_index(&args).expect("has subcommand");
    assert_eq!(idx, 2, "subcommand starts after `-C <path>`");
    assert_eq!(effective_cwd_through_globals(&args, idx), abs);
    // Repeated `-C`: a later RELATIVE `-C` joins onto the accumulated path.
    let args = s(&["-C", abs_s, "-C", "sub", "checkout", "main"]);
    let idx = subcommand_index(&args).expect("has subcommand");
    assert_eq!(effective_cwd_through_globals(&args, idx), abs.join("sub"));
    // A value-taking global (`-c k=v`) before `-C` is skipped WITH its value,
    // so `k=v` is never mistaken for the `-C` target.
    let args = s(&["-c", "k=v", "-C", abs_s, "status"]);
    let idx = subcommand_index(&args).expect("has subcommand");
    assert_eq!(effective_cwd_through_globals(&args, idx), abs);
    // No leading `-C` → process cwd unchanged.
    let args = s(&["worktree", "add", "x"]);
    let idx = subcommand_index(&args).expect("has subcommand");
    assert_eq!(
        effective_cwd_through_globals(&args, idx),
        std::env::current_dir().unwrap()
    );
}

/// #2234 fix B: the pure deny decision. Deny only when agent + !escape +
/// canonical + provisioning op (worktree / positional checkout|switch <ref>).
#[test]
fn deny_agent_canonical_bypass_decision_matrix_2234() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // DENY: agent, not escaped, canonical, provisioning ops.
    assert!(deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["worktree", "add", "x", "origin/main"])
    ));
    assert!(deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["checkout", "origin/main"])
    ));
    assert!(deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["switch", "main"])
    ));

    // ALLOW — non-`add` worktree subcommands are NOT stray/detach vectors
    // (r4 #2316 over-block fix): `list` is read-only; remove/prune/move
    // don't detach or stray. Bare `worktree` (no subcommand) → not add.
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["worktree", "list"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["worktree", "remove", "x"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["worktree", "prune"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["worktree"])
    ));

    // ALLOW — carve-outs / non-provisioning:
    // non-agent caller (daemon-correlated / operator shell).
    assert!(!deny_agent_canonical_bypass(
        false,
        false,
        true,
        &s(&["worktree", "add", "x"])
    ));
    // explicit escape env set.
    assert!(!deny_agent_canonical_bypass(
        true,
        true,
        true,
        &s(&["worktree", "add", "x"])
    ));
    // cwd not canonical-rooted (e.g. /tmp nextest fixture).
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        false,
        &s(&["checkout", "origin/main"])
    ));
    // non-provisioning ops agents legitimately bypass in their own worktree.
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["push", "origin", "feat/x"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["commit", "-m", "wip"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["add", "-A"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["reset", "--hard", "HEAD"])
    ));
    // checkout flag/pathspec forms are NOT positional-branch → not denied.
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["checkout", "-b", "tmp"])
    ));
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["checkout", "--", "file.rs"])
    ));
    // read-only.
    assert!(!deny_agent_canonical_bypass(
        true,
        false,
        true,
        &s(&["status"])
    ));
}

/// #852 residual core: canonical source repo (`.git` directory +
/// `[remote "origin"]`) must classify as canonical-rooted. This
/// is the path that pre-#852-residual missed entirely.
#[test]
fn cwd_is_canonical_rooted_returns_true_for_source_repo_with_origin() {
    let repo = make_source_repo_with_origin("with-origin");
    let result = with_cwd(&repo, cwd_is_canonical_rooted);
    cleanup_base(&repo);
    assert!(
        result,
        "canonical source repo with `[remote \"origin\"]` must classify \
             as canonical-rooted (this is the #852 residual fix)"
    );
}

/// Defense against orphan workspace-placeholder repos: `.git`
/// directory present but no remote configured. Daemon startup
/// creates these before fleet config resolves; the canonical-
/// rooted gate must NOT fire on them.
#[test]
fn cwd_is_canonical_rooted_returns_false_for_source_repo_without_origin() {
    let repo = make_source_repo_without_origin("no-origin");
    let result = with_cwd(&repo, cwd_is_canonical_rooted);
    cleanup_base(&repo);
    assert!(
        !result,
        "orphan workspace-placeholder (`.git` directory but no \
             `[remote \"origin\"]`) must NOT classify as canonical-rooted"
    );
}

/// Preserves the #858 contract: canonical-rooted worktree
/// (`.git` FILE with `gitdir:` pointer to source carrying origin)
/// still classifies. This is the pre-PR-A path; the broadening
/// must NOT regress it.
#[test]
fn cwd_is_canonical_rooted_returns_true_for_canonical_worktree() {
    let (wt, _base) = make_canonical_worktree("worktree");
    let result = with_cwd(&wt, cwd_is_canonical_rooted);
    cleanup_base(&wt);
    assert!(
        result,
        "canonical worktree (`.git` FILE + gitdir: pointer to source \
             with origin) must still classify (pre-#852-residual contract)"
    );
}

// ── #883 pre-push cleanup tests ───────────────────────────────

/// Build a synthetic repo with a real `origin` remote pointing at
/// a sibling bare repo, then create N empty `init` heartbeat
/// commits on `feat/test` followed by one real commit. Returns
/// `(worktree_path, real_commit_sha)`. The fixture mirrors the
/// operator's PR #882 case (multiple init heartbeats before the
/// real commit) — exactly the scenario the shim cleanup must
/// handle.
fn setup_branch_with_init_pile(init_count: usize) -> (std::path::PathBuf, String) {
    let id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let base = std::env::temp_dir().join(format!("agend-883-fixture-{id}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let origin_bare = base.join("origin.git");
    let worktree = base.join("worktree");
    let git_env = [
        ("AGEND_GIT_BYPASS", "1"),
        ("GIT_AUTHOR_NAME", "test"),
        ("GIT_AUTHOR_EMAIL", "test@test"),
        ("GIT_COMMITTER_NAME", "test"),
        ("GIT_COMMITTER_EMAIL", "test@test"),
    ];
    let git_run = |args: &[&str], dir: &std::path::Path| -> std::process::Output {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        for (k, v) in git_env.iter() {
            cmd.env(k, v);
        }
        cmd.output().expect("git spawn")
    };

    // Create a bare origin repo with one commit on main.
    assert!(git_run(
        &[
            "init",
            "--bare",
            "-b",
            "main",
            origin_bare.to_str().unwrap()
        ],
        &base
    )
    .status
    .success());
    // Create the worktree by cloning the bare repo.
    assert!(git_run(
        &[
            "clone",
            origin_bare.to_str().unwrap(),
            worktree.to_str().unwrap()
        ],
        &base
    )
    .status
    .success());
    // #883 r1: configure user identity + gpgsign in the LOCAL repo
    // config so the production cleanup's `git rebase` (which spawns
    // its own Command without inheriting test env vars) has the
    // info it needs to materialize the cherry-picked real commit.
    // macOS GH-Actions runners auto-derive user.name from
    // /etc/passwd, but Ubuntu / Windows runners do not — that's
    // why CI failed at 7fd4628. Local config takes precedence over
    // global (which is /dev/null in `GIT_CONFIG_GLOBAL=/dev/null`
    // pre-push baseline) AND over /etc/passwd derivation.
    assert!(git_run(&["config", "user.name", "test"], &worktree)
        .status
        .success());
    assert!(
        git_run(&["config", "user.email", "test@test.local"], &worktree)
            .status
            .success()
    );
    // Belt-and-suspenders: disable commit signing in case the CI
    // runner has `commit.gpgsign=true` baked in somewhere.
    assert!(git_run(&["config", "commit.gpgsign", "false"], &worktree)
        .status
        .success());
    // Seed origin/main with an initial real commit so origin/main..HEAD has a valid base.
    std::fs::write(worktree.join("README.md"), "initial\n").unwrap();
    assert!(git_run(&["add", "README.md"], &worktree).status.success());
    assert!(git_run(&["commit", "-m", "initial real"], &worktree)
        .status
        .success());
    assert!(git_run(&["push", "origin", "main"], &worktree)
        .status
        .success());

    // Create the feature branch and pile N empty init heartbeats.
    assert!(git_run(&["checkout", "-b", "feat/test"], &worktree)
        .status
        .success());
    for _ in 0..init_count {
        assert!(
            git_run(&["commit", "--allow-empty", "-m", "init"], &worktree)
                .status
                .success()
        );
    }
    // Add a real commit on top — the operator's PR #882 scenario.
    std::fs::write(worktree.join("feature.txt"), "real work\n").unwrap();
    assert!(git_run(&["add", "feature.txt"], &worktree).status.success());
    assert!(git_run(&["commit", "-m", "feat: real work"], &worktree)
        .status
        .success());
    let real_sha = String::from_utf8(git_run(&["rev-parse", "HEAD"], &worktree).stdout)
        .unwrap()
        .trim()
        .to_string();

    (worktree, real_sha)
}

/// Count commits between `<base>..HEAD` in a worktree. Used to
/// assert how many commits survive the cleanup.
fn count_commits_above_base(worktree: &std::path::Path, base: &str) -> usize {
    let output = Command::new("git")
        .args(["log", &format!("{base}..HEAD"), "--format=%H"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git log spawn");
    if !output.status.success() {
        panic!(
            "git log {base}..HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Read the rev pointed at by a ref / HEAD.
fn rev_parse(worktree: &std::path::Path, refname: &str) -> String {
    let output = Command::new("git")
        .args(["rev-parse", refname])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("rev-parse spawn");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// #883 RED→GREEN: the shim's pre-push cleanup must drop the
/// init pile before push so origin only sees the real commit.
/// Pre-fix the cleanup function doesn't exist (compile-fail RED).
/// Post-fix the mixed-history rebase path runs and the
/// `origin/main..HEAD` count drops from N+1 to 1.
///
/// This test calls `cleanup_init_pile_pre_push` directly rather
/// than running the full `git push` (no need to wire a real
/// remote round-trip; the cleanup operates on local refs).
#[test]
fn shim_push_cleans_init_pile_before_push() {
    let (worktree, real_sha) = setup_branch_with_init_pile(3);
    // Pre-cleanup: 3 inits + 1 real = 4 commits above origin/main.
    assert_eq!(
        count_commits_above_base(&worktree, "origin/main"),
        4,
        "fixture must build 3 inits + 1 real commit above origin/main"
    );

    // Run the production cleanup function directly. This is what
    // the shim's `Action::CleanupAndChdirPushPass` arm calls
    // before `exec_real_git`.
    cleanup_init_pile_pre_push(worktree.to_str().unwrap());

    // Post-cleanup: only the real commit should remain above
    // origin/main; the 3 inits must be dropped.
    assert_eq!(
        count_commits_above_base(&worktree, "origin/main"),
        1,
        "post-#883: shim cleanup must drop the 3 init heartbeats"
    );

    // The real commit's TREE must be preserved — rebase keeps
    // identical content even if the SHA changes (rebase rewrites
    // parent pointers). Assert by checking the file content.
    let feature = std::fs::read_to_string(worktree.join("feature.txt")).unwrap();
    assert_eq!(
        feature.trim(),
        "real work",
        "real commit's tree must survive"
    );

    // The real commit's SHA may or may not change depending on
    // whether interactive rebase rewrites parents. Both are
    // valid; if the SHA stayed identical that's an even stronger
    // signal (cherry-pick optimization), but either way the
    // origin/main..HEAD count must be exactly 1.
    let _ = real_sha; // intentionally not asserted equal

    // Cleanup tempdir (best-effort).
    let _ = std::fs::remove_dir_all(worktree.parent().unwrap());
}

/// Negative regression: no-op when origin/main..HEAD already has
/// only real commits. The cleanup must not mutate a clean branch.
#[test]
fn shim_push_cleanup_noop_when_no_inits() {
    let id = format!(
        "{}-noop-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let base = std::env::temp_dir().join(format!("agend-883-noop-{id}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let origin_bare = base.join("origin.git");
    let worktree = base.join("worktree");
    let env = [
        ("AGEND_GIT_BYPASS", "1"),
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@t"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@t"),
    ];
    let run = |args: &[&str], dir: &std::path::Path| {
        let mut c = Command::new("git");
        c.args(args).current_dir(dir);
        for (k, v) in env.iter() {
            c.env(k, v);
        }
        c.output().expect("spawn")
    };
    assert!(run(
        &[
            "init",
            "--bare",
            "-b",
            "main",
            origin_bare.to_str().unwrap()
        ],
        &base
    )
    .status
    .success());
    assert!(run(
        &[
            "clone",
            origin_bare.to_str().unwrap(),
            worktree.to_str().unwrap()
        ],
        &base
    )
    .status
    .success());
    // #883 r1: configure local repo user identity + gpgsign so the
    // production cleanup's `git rebase` doesn't fail on CI runners
    // (Ubuntu / Windows) lacking global gitconfig. See
    // `setup_branch_with_init_pile` for the full rationale.
    assert!(run(&["config", "user.name", "test"], &worktree)
        .status
        .success());
    assert!(run(&["config", "user.email", "test@test.local"], &worktree)
        .status
        .success());
    assert!(run(&["config", "commit.gpgsign", "false"], &worktree)
        .status
        .success());
    std::fs::write(worktree.join("R.md"), "x\n").unwrap();
    assert!(run(&["add", "R.md"], &worktree).status.success());
    assert!(run(&["commit", "-m", "initial"], &worktree)
        .status
        .success());
    assert!(run(&["push", "origin", "main"], &worktree).status.success());
    assert!(run(&["checkout", "-b", "feat/clean"], &worktree)
        .status
        .success());
    std::fs::write(worktree.join("real.txt"), "work\n").unwrap();
    assert!(run(&["add", "real.txt"], &worktree).status.success());
    assert!(run(&["commit", "-m", "feat: real"], &worktree)
        .status
        .success());
    let head_before = rev_parse(&worktree, "HEAD");

    cleanup_init_pile_pre_push(worktree.to_str().unwrap());

    let head_after = rev_parse(&worktree, "HEAD");
    assert_eq!(head_before, head_after, "no-op on a clean branch");
    let _ = std::fs::remove_dir_all(&base);
}

// ----- #1225: conflict resolution guidance -----

#[test]
fn is_conflict_capable_covers_rebase_merge_pull_cherry_pick() {
    for cmd in ["rebase", "merge", "pull", "cherry-pick"] {
        assert!(is_conflict_capable(cmd), "{cmd} should be conflict-capable");
    }
    for cmd in ["commit", "add", "push", "status", "reset"] {
        assert!(
            !is_conflict_capable(cmd),
            "{cmd} should NOT be conflict-capable"
        );
    }
}

#[test]
fn has_unmerged_files_false_on_clean_repo() {
    let repo = std::env::temp_dir().join(format!(
        "agend-conflict-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&repo).unwrap();
    // #1748: drive fixture git through the REAL git binary, not the bare
    // `git` PATH entry which resolves to this agend-git shim — whose #1463
    // ChdirPass strips the `-C <tempdir>` and redirects the op onto the
    // caller's bound worktree, corrupting it. `resolve_real_git()` is the
    // same resolver the shim uses to exec real git (excludes $AGEND_HOME/bin).
    let git_bin = resolve_real_git();
    assert!(Command::new(&git_bin)
        .arg("-C")
        .arg(&repo)
        .args(["init", "-b", "main"])
        .output()
        .unwrap()
        .status
        .success());
    Command::new(&git_bin)
        .arg("-C")
        .arg(&repo)
        .args(["config", "user.name", "test"])
        .output()
        .unwrap();
    Command::new(&git_bin)
        .arg("-C")
        .arg(&repo)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .unwrap();
    assert!(Command::new(&git_bin)
        .arg("-C")
        .arg(&repo)
        .args(["commit", "--allow-empty", "-m", "init"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!has_unmerged_files(&git_bin, repo.to_str().unwrap()));
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn has_unmerged_files_true_on_conflict() {
    let repo = std::env::temp_dir().join(format!(
        "agend-conflict-pos-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&repo).unwrap();
    // #1748: real git, not the shim — see has_unmerged_files_false_on_clean_repo.
    let git_bin = resolve_real_git();
    let git = |args: &[&str]| {
        Command::new(&git_bin)
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&["add", "f.txt"]);
    git(&["commit", "-m", "base"]);
    git(&["checkout", "-b", "side"]);
    std::fs::write(repo.join("f.txt"), "side\n").unwrap();
    git(&["commit", "-am", "side"]);
    git(&["checkout", "main"]);
    std::fs::write(repo.join("f.txt"), "main\n").unwrap();
    git(&["commit", "-am", "main"]);
    let merge = git(&["merge", "side", "--no-edit"]);
    assert!(!merge.status.success(), "merge should fail with conflict");
    assert!(has_unmerged_files(&git_bin, repo.to_str().unwrap()));
    git(&["merge", "--abort"]);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn conflict_guidance_contains_resolution_steps() {
    let guidance = format_conflict_guidance();
    assert!(guidance.contains("resolve"), "should mention resolving");
    assert!(guidance.contains("git add"), "should mention git add");
    assert!(guidance.contains("--continue"), "should mention --continue");
    assert!(
        guidance.contains("Do NOT abandon"),
        "should discourage abandoning"
    );
}

// ── #1463: foreign-repo passthrough (A) + target-override strip (B) ──

use std::sync::atomic::{AtomicU32, Ordering};
static TMP_CTR_1463: AtomicU32 = AtomicU32::new(0);

fn uniq_tmp_1463(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "agend-1463-{}-{}-{}",
        tag,
        std::process::id(),
        TMP_CTR_1463.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Fabricate a canonical-style source repo (`<root>/.git/` DIR) — hermetic,
/// no `git` invocation (so the shim is never re-entered by the test).
fn fake_source_repo(root: &Path) {
    let git = root.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("config"), "[core]\n").unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
}

/// Fabricate a linked worktree of `source_root`: a `worktrees/<name>/` entry
/// carrying a `commondir` file, plus the worktree dir's `.git` FILE pointer.
fn fake_worktree(source_root: &Path, wt_root: &Path, name: &str) {
    let entry = source_root.join(".git").join("worktrees").join(name);
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(wt_root).unwrap();
    std::fs::write(
        wt_root.join(".git"),
        format!("gitdir: {}\n", entry.display()),
    )
    .unwrap();
}

// (a) an independent `git init` temp repo is FOREIGN → passthrough.
#[test]
fn foreign_scratch_repo_is_foreign_1463() {
    let src = uniq_tmp_1463("src");
    fake_source_repo(&src);
    let wt = uniq_tmp_1463("wt");
    fake_worktree(&src, &wt, "w1");
    let scratch = uniq_tmp_1463("scratch");
    fake_source_repo(&scratch); // its own, separate object store
    assert!(
        paths_are_foreign(&scratch, &wt),
        "an independent scratch repo has a separate commondir → foreign"
    );
}

// (b) the canonical SOURCE repo shares the worktree's commondir → NOT foreign.
#[test]
fn canonical_source_not_foreign_1463() {
    let src = uniq_tmp_1463("src");
    fake_source_repo(&src);
    let wt = uniq_tmp_1463("wt");
    fake_worktree(&src, &wt, "w1");
    assert!(
        !paths_are_foreign(&src, &wt),
        "canonical source shares the worktree's commondir → must ChdirPass"
    );
}

// (c) a SIBLING worktree (`.git` FILE) shares canonical's commondir → NOT foreign.
#[test]
fn sibling_worktree_not_foreign_1463() {
    let src = uniq_tmp_1463("src");
    fake_source_repo(&src);
    let mine = uniq_tmp_1463("mine");
    fake_worktree(&src, &mine, "mine");
    let sib = uniq_tmp_1463("sib");
    fake_worktree(&src, &sib, "sibling");
    assert!(
        !paths_are_foreign(&sib, &mine),
        "a sibling worktree shares canonical's commondir → must ChdirPass (no sibling-write)"
    );
}

// (e) craft: a `.git` FILE pointing `gitdir: <canonical>/.git` resolves to
// canonical's commondir → NOT foreign (no canonical-write bypass).
#[test]
fn craft_gitfile_pointing_canonical_not_foreign_1463() {
    let src = uniq_tmp_1463("src");
    fake_source_repo(&src);
    let wt = uniq_tmp_1463("wt");
    fake_worktree(&src, &wt, "w1");
    let evil = uniq_tmp_1463("evil");
    std::fs::write(
        evil.join(".git"),
        format!("gitdir: {}\n", src.join(".git").display()),
    )
    .unwrap();
    assert!(
        !paths_are_foreign(&evil, &wt),
        "craft pointing at canonical's .git resolves to canonical commondir → NOT foreign"
    );
}

// (e) a `.git` SYMLINK → fail-closed (NOT foreign → ChdirPass).
#[cfg(unix)]
#[test]
fn symlink_gitfile_fails_closed_1463() {
    let src = uniq_tmp_1463("src");
    fake_source_repo(&src);
    let wt = uniq_tmp_1463("wt");
    fake_worktree(&src, &wt, "w1");
    let evil = uniq_tmp_1463("evilsym");
    std::os::unix::fs::symlink(src.join(".git"), evil.join(".git")).unwrap();
    assert!(
        !paths_are_foreign(&evil, &wt),
        "a `.git` symlink is an irregular shape → fail-closed → NOT foreign"
    );
}

// parse-fail / no-repo cwd → fail-closed (NOT foreign).
#[test]
fn unresolvable_cwd_fails_closed_1463() {
    let src = uniq_tmp_1463("src");
    fake_source_repo(&src);
    let wt = uniq_tmp_1463("wt");
    fake_worktree(&src, &wt, "w1");
    let nonrepo = uniq_tmp_1463("nonrepo"); // no `.git` anywhere up the tree
    let garbage = uniq_tmp_1463("garbage");
    std::fs::write(garbage.join(".git"), "not a gitdir pointer\n").unwrap();
    assert!(
        !paths_are_foreign(&nonrepo, &wt),
        "no resolvable repo → fail-closed NOT foreign"
    );
    assert!(
        !paths_are_foreign(&garbage, &wt),
        "garbage `.git` file (no gitdir:) → fail-closed NOT foreign"
    );
}

// (A) the ChdirPass→Passthrough conversion matrix.
#[test]
fn foreign_passthrough_action_matrix_1463() {
    use Action::*;
    let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // local mutating + foreign → Passthrough
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "commit", &a(&["commit"]), true),
        Passthrough
    );
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "add", &a(&["add"]), true),
        Passthrough
    );
    // local mutating + NOT foreign → unchanged ChdirPass
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "commit", &a(&["commit"]), false),
        ChdirPass("wt".into())
    );
    // push / checkout are NOT local-mutating → stay ChdirPass even if foreign
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "push", &a(&["push"]), true),
        ChdirPass("wt".into())
    );
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "checkout", &a(&["checkout"]), true),
        ChdirPass("wt".into())
    );
    // non-ChdirPass inputs are returned verbatim
    assert_eq!(
        apply_foreign_repo_passthrough(Deny("x".into()), "commit", &a(&["commit"]), true),
        Deny("x".into())
    );
    assert_eq!(
        apply_foreign_repo_passthrough(Passthrough, "commit", &a(&["commit"]), true),
        Passthrough
    );
}

// #2027 (§3.9): a ref-naming `branch`/`tag` in a FOREIGN repo must pass through
// (run against THAT repo), never be ChdirPass'd into the worktree — the
// worktree-redirect is the success-lie (silent no-op + exit 0 for a create; a
// fake `already exists` for a name the worktree already holds). Mirrors the
// issue's deterministic 3-shape repro matrix.
#[test]
fn branch_tag_names_ref_classifier_2027() {
    let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // create: positional ref name → names a ref
    assert!(branch_tag_names_ref("branch", &a(&["branch", "feat-x"])));
    assert!(branch_tag_names_ref("tag", &a(&["tag", "v1.0"])));
    // delete / move / copy flags → name a ref
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "-d", "feat-x"])
    ));
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "-m", "old", "new"])
    ));
    assert!(branch_tag_names_ref("tag", &a(&["tag", "-d", "v1.0"])));
    // #2030 (codex): CURRENT-branch mutators with NO positional token — they
    // write `branch.<cur>.merge` / the description, so a foreign-repo redirect
    // would still lie. All four forms must name a ref.
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "--set-upstream-to=origin/main"])
    ));
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "--set-upstream-to", "origin/main"])
    ));
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "-u", "origin/main"])
    ));
    // codex r2: the GLUED short form `-u<up>` (value attached, one token) —
    // missed by both the exact `-u` match and the positional check.
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "-uorigin/main"])
    ));
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "--unset-upstream"])
    ));
    assert!(branch_tag_names_ref(
        "branch",
        &a(&["branch", "--edit-description"])
    ));
    // bare LIST form (no positional, list/inspect flags only) → does NOT name a ref
    assert!(!branch_tag_names_ref("branch", &a(&["branch"])));
    assert!(!branch_tag_names_ref("branch", &a(&["branch", "-a"])));
    assert!(!branch_tag_names_ref("branch", &a(&["branch", "-v", "-v"])));
    assert!(!branch_tag_names_ref("tag", &a(&["tag", "-l"])));
    // non-branch/tag subcommands are never ref-naming here (handled elsewhere)
    assert!(!branch_tag_names_ref("commit", &a(&["commit", "-m", "x"])));
    assert!(!branch_tag_names_ref("status", &a(&["status"])));
}

// #2027 (§3.9): the end-to-end conversion — bound-agent `git branch <name>` in
// a foreign repo flips ChdirPass→Passthrough (no lie); fleet (non-foreign) and
// the bare LIST form stay ChdirPass byte-identical.
#[test]
fn foreign_passthrough_branch_tag_matrix_2027() {
    use Action::*;
    let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Shape 1 — never-seen create name, foreign → Passthrough (was: silent no-op exit 0)
    assert_eq!(
        apply_foreign_repo_passthrough(
            ChdirPass("wt".into()),
            "branch",
            &a(&["branch", "zz-new"]),
            true
        ),
        Passthrough
    );
    // Shape 2 — name the worktree already holds, foreign → Passthrough
    // (was: fake `already exists` exit 128 from the worktree)
    assert_eq!(
        apply_foreign_repo_passthrough(
            ChdirPass("wt".into()),
            "branch",
            &a(&["branch", "feat-x"]),
            true
        ),
        Passthrough
    );
    // tag create, foreign → Passthrough
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "tag", &a(&["tag", "v1.0"]), true),
        Passthrough
    );
    // #2030 (codex): a no-positional current-branch mutator, foreign → Passthrough
    // (was: ChdirPass wrote branch.<cur>.merge into the worktree, the lie)
    assert_eq!(
        apply_foreign_repo_passthrough(
            ChdirPass("wt".into()),
            "branch",
            &a(&["branch", "--set-upstream-to=origin/main"]),
            true
        ),
        Passthrough
    );
    // #2030 codex r2: the glued short form `-u<up>`, foreign → Passthrough
    assert_eq!(
        apply_foreign_repo_passthrough(
            ChdirPass("wt".into()),
            "branch",
            &a(&["branch", "-uorigin/main"]),
            true
        ),
        Passthrough
    );
    // FLEET (non-foreign) ref-naming branch → unchanged ChdirPass (byte-identical)
    assert_eq!(
        apply_foreign_repo_passthrough(
            ChdirPass("wt".into()),
            "branch",
            &a(&["branch", "feat-x"]),
            false
        ),
        ChdirPass("wt".into())
    );
    // bare LIST form, even foreign → unchanged ChdirPass (read-only, no lie)
    assert_eq!(
        apply_foreign_repo_passthrough(ChdirPass("wt".into()), "branch", &a(&["branch"]), true),
        ChdirPass("wt".into())
    );
}

// (B) GATED to mutating-local: a leading `-C`/`--git-dir`/`--work-tree` is
// stripped ONLY when the real subcommand mutates (so the shim's
// `-C <worktree>` wins and `<elsewhere>` — e.g. canonical — is not touched);
// non-mutating `-C` and a post-subcommand `-C` (reuse-message) are preserved.
#[test]
fn strip_target_overrides_1463() {
    // ── MUTATING-local + leading override → STRIPPED ──
    assert_eq!(
        strip_target_overrides(&s(&["-C", "/tmp/x", "commit", "-m", "z"])),
        s(&["commit", "-m", "z"])
    );
    assert_eq!(
        strip_target_overrides(&s(&["--git-dir", "/g", "add", "."])),
        s(&["add", "."])
    );
    assert_eq!(
        strip_target_overrides(&s(&["--git-dir=/g", "commit"])),
        s(&["commit"])
    );
    assert_eq!(
        strip_target_overrides(&s(&["--work-tree", "/w", "reset", "--hard"])),
        s(&["reset", "--hard"])
    );
    // glued -C<path>
    assert_eq!(
        strip_target_overrides(&s(&["-C/tmp/x", "commit"])),
        s(&["commit"])
    );
    // repeated -C all dropped (the left-to-right override chain)
    assert_eq!(
        strip_target_overrides(&s(&["-C", "/a", "-C", "/b", "commit"])),
        s(&["commit"])
    );
    // value-taking non-target global (-c) kept WITH its value, -C dropped
    assert_eq!(
        strip_target_overrides(&s(&["-c", "k=v", "-C", "/x", "commit"])),
        s(&["-c", "k=v", "commit"])
    );
    // lead matrix: `git -C <canonical> commit` (mutating) → strip → worktree.
    assert_eq!(
        strip_target_overrides(&s(&["-C", "/canonical", "commit"])),
        s(&["commit"])
    );

    // ── NON-mutating + leading -C → PRESERVED (gating) ──
    assert_eq!(
        strip_target_overrides(&s(&["-C", "/tmp/x", "rev-parse", "--is-inside-work-tree"])),
        s(&["-C", "/tmp/x", "rev-parse", "--is-inside-work-tree"])
    );
    assert_eq!(
        strip_target_overrides(&s(&["-C", "/tmp/x", "init", "--quiet"])),
        s(&["-C", "/tmp/x", "init", "--quiet"])
    );
    // lead matrix: `git -C <canonical> rev-parse` (non-mutating) → read in place.
    assert_eq!(
        strip_target_overrides(&s(&["-C", "/canonical", "rev-parse"])),
        s(&["-C", "/canonical", "rev-parse"])
    );

    // ── preserved / no-op forms ──
    // POST-subcommand `-C` (git commit reuse-message) PRESERVED
    assert_eq!(
        strip_target_overrides(&s(&["commit", "-C", "HEAD"])),
        s(&["commit", "-C", "HEAD"])
    );
    // no globals → unchanged
    assert_eq!(
        strip_target_overrides(&s(&["commit", "-m", "x"])),
        s(&["commit", "-m", "x"])
    );
    // globals-only / malformed (no subcommand) → unchanged, no panic
    assert_eq!(strip_target_overrides(&s(&["-C"])), s(&["-C"]));
    assert_eq!(strip_target_overrides(&s(&["-C", "/x"])), s(&["-C", "/x"]));
}

// ── #2234 (C): cwd↔bound-worktree drift detection ──────────────────
// Drive fixture git through the REAL git binary (not the bare `git` PATH
// entry, which IS this shim — its #1463 ChdirPass would hijack the op). See
// the #1748 note on `has_unmerged_files_false_on_clean_repo`.
fn drift_git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let git = resolve_real_git();
    assert!(
        Command::new(&git)
            .arg("-C")
            .arg(dir)
            .args(["init", "-b", "main"])
            .output()
            .unwrap()
            .status
            .success(),
        "git init {dir:?}"
    );
}

fn drift_home(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("agend-2234-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn workspace_clone_is_drift_but_scratch_repo_is_not_2234() {
    let home = drift_home("detect");
    let agent = "ag";
    let h = home.to_str().unwrap();
    // The agent's configured workspace dir — a SEPARATE clone (own store).
    let ws = home.join("workspace").join(agent);
    drift_git_init(&ws);
    // The bound worktree — a different, separate repo (only object-store
    // identity matters for the foreign check).
    let worktree = home.join("wt");
    drift_git_init(&worktree);
    // A legit foreign scratch repo OUTSIDE the workspace dir (#1463 incubator).
    let scratch = home.join("scratch");
    drift_git_init(&scratch);

    // cwd = workspace clone, foreign to worktree → DRIFT.
    assert!(
        is_workspace_clone_drift(h, agent, &ws, &worktree),
        "workspace clone foreign to the bound worktree must be drift"
    );
    // cwd = scratch repo OUTSIDE the workspace dir → NOT drift (no FP).
    assert!(
        !is_workspace_clone_drift(h, agent, &scratch, &worktree),
        "a foreign scratch repo outside the workspace dir must NOT warn"
    );
    // cwd == worktree (aligned, same object store) → NOT drift.
    assert!(
        !is_workspace_clone_drift(h, agent, &worktree, &worktree),
        "cwd aligned with the bound worktree must NOT be drift"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn drift_warns_per_class_then_latches_2234() {
    let home = drift_home("latch");
    let agent = "ag";
    let h = home.to_str().unwrap();
    let ws = home.join("workspace").join(agent);
    drift_git_init(&ws);
    let worktree = home.join("wt");
    drift_git_init(&worktree);

    // First read-class sighting → warns, writes the .read latch + a fleet_events
    // line.
    assert!(
        warn_workspace_drift_once(h, agent, &ws, &worktree, false),
        "first read-class drift sighting must warn"
    );
    let read_marker = home
        .join("runtime")
        .join(agent)
        .join("cwd_drift_warned.read");
    assert!(
        read_marker.exists(),
        "read-class latch marker must be written"
    );
    let events = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("cwd_worktree_drift"),
        "a cwd_worktree_drift event must be logged: {events}"
    );

    // Second read-class call, same drifted cwd → latched, no second warn.
    assert!(
        !warn_workspace_drift_once(h, agent, &ws, &worktree, false),
        "standing read-class drift must warn only once (latched)"
    );

    // The FIRST mutating-class op on the SAME cwd is a distinct latch class →
    // warns again (so the agent sees the hint before its first write). ≤2/cwd.
    assert!(
        warn_workspace_drift_once(h, agent, &ws, &worktree, true),
        "first mutating-class drift sighting must warn (independent latch class)"
    );
    let mut_marker = home
        .join("runtime")
        .join(agent)
        .join("cwd_drift_warned.mut");
    assert!(
        mut_marker.exists(),
        "mutating-class latch marker must be written"
    );

    // Second mutating-class call → latched, no third warn (cadence capped at 2).
    assert!(
        !warn_workspace_drift_once(h, agent, &ws, &worktree, true),
        "standing mutating-class drift must warn only once (latched)"
    );

    // Aligned cwd never warns (guard), in either class.
    assert!(
        !warn_workspace_drift_once(h, agent, &worktree, &worktree, false),
        "aligned cwd must not warn (read class)"
    );
    assert!(
        !warn_workspace_drift_once(h, agent, &worktree, &worktree, true),
        "aligned cwd must not warn (mutating class)"
    );

    std::fs::remove_dir_all(&home).ok();
}

// #2234: a NEW drifted cwd re-warns even after a prior cwd latched — the latch
// keys on the exact cwd string, so moving to a different clone is new info.
#[test]
fn drift_new_cwd_rewarns_2234() {
    let home = drift_home("newcwd");
    let agent = "ag";
    let h = home.to_str().unwrap();
    let ws1 = home.join("workspace").join(agent);
    drift_git_init(&ws1);
    let worktree = home.join("wt");
    drift_git_init(&worktree);

    assert!(
        warn_workspace_drift_once(h, agent, &ws1, &worktree, false),
        "first cwd must warn"
    );
    assert!(
        !warn_workspace_drift_once(h, agent, &ws1, &worktree, false),
        "same cwd latched"
    );
    // A DIFFERENT clone path under the workspace dir → not yet latched → re-warns.
    let ws2 = ws1.join("nested");
    drift_git_init(&ws2);
    assert!(
        warn_workspace_drift_once(h, agent, &ws2, &worktree, false),
        "a new drifted cwd must re-warn (new info)"
    );

    std::fs::remove_dir_all(&home).ok();
}

// #2234: the warning carries the ACTIONABLE recovery hint (operator ruling:
// replace the cd-only message). Routes through the SAME `drift_warning_message`
// producer the emit uses (#1493 — no hand-copied shape) and pins the concrete
// recovery tokens so the message can't silently regress to cd-only.
#[test]
fn drift_message_has_actionable_recovery_hint_2234() {
    let msg = drift_warning_message(Path::new("/home/ws/ag"), Path::new("/home/wt"));
    // The actionable recovery contract: a check command, absolute-path guidance,
    // and the explicit "cd alone does NOT" correction (r2's finding).
    assert!(
        msg.contains("status --short"),
        "must tell the agent how to CHECK what mislanded: {msg}"
    );
    assert!(
        msg.contains("ABSOLUTE paths"),
        "must give the absolute-path recovery action: {msg}"
    );
    assert!(
        msg.contains("`cd` alone does NOT"),
        "must correct the insufficient cd-only advice (r2): {msg}"
    );
    // Names both endpoints so the agent knows which dir is which.
    assert!(
        msg.contains("/home/ws/ag") && msg.contains("/home/wt"),
        "must name both the cwd clone and the worktree: {msg}"
    );
}

// ── #2158: shim-side bypass mutating-op audit ──────────────────────────
/// Option B gate: the stray-worktree / drift / stray-tree-push vector IS
/// audited; read-only ops and the high-frequency `commit`/`add` (agent
/// self-worktree, ~zero forensic value) are NOT.
#[test]
fn bypass_op_is_audited_option_b_gate_2158() {
    let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Audited: worktree-lifecycle + drift + destructive + stray-tree push.
    for op in ["worktree", "checkout", "switch", "reset", "clean", "push"] {
        assert!(
            bypass_op_is_audited(op, &a(&[op])),
            "bypass `{op}` must be audited (Option B vector)"
        );
    }
    // `branch` audited ONLY in its ref-mutating form, not the bare list.
    assert!(
        bypass_op_is_audited("branch", &a(&["branch", "-D", "feat/x"])),
        "bypass `branch -D` (ref delete) must be audited"
    );
    assert!(
        !bypass_op_is_audited("branch", &a(&["branch"])),
        "bare `branch` (list, read) must NOT be audited"
    );
    // NOT audited: high-frequency self-worktree mutators (Option B exclusion).
    for op in ["commit", "add"] {
        assert!(
            !bypass_op_is_audited(op, &a(&[op])),
            "bypass `{op}` must NOT be audited (Option B: floods, ~0 value)"
        );
    }
    // NOT audited: read-only ops.
    for op in ["status", "log", "diff", "rev-parse", "show", "tag"] {
        assert!(
            !bypass_op_is_audited(op, &a(&[op])),
            "bypass read-only `{op}` must NOT be audited"
        );
    }
}

/// The audit record carries the forensic fields (event type, subcommand, argv,
/// process ancestry, bypass layer) so a stray-worktree culprit is traceable.
#[test]
fn build_bypass_audit_event_shape_2158() {
    let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let argv = a(&["worktree", "add", "/tmp/stray", "origin/main"]);
    let ancestry = vec!["100 1 git".to_string(), "1 0 launchd".to_string()];
    let ev = build_bypass_audit_event("dev-2", "worktree", &argv, "/cwd/x", 4242, &ancestry, "env");
    assert_eq!(ev["event"], "bypass_mutating_op");
    assert_eq!(ev["agent"], "dev-2");
    assert_eq!(ev["subcommand"], "worktree");
    assert_eq!(ev["ppid"], 4242);
    assert_eq!(ev["cwd"], "/cwd/x");
    assert_eq!(ev["bypass_layer"], "env");
    assert_eq!(ev["argv"][2], "/tmp/stray");
    assert_eq!(ev["process_ancestry"][0], "100 1 git");
    assert!(ev["timestamp"].is_string(), "must carry a timestamp");
}

// ── #2379 ③ denylist-core tests ───────────────────────────────

/// Pure matcher table: trust-root basenames (at any depth) + `*.jsonl` are
/// denied; normal repo files and near-misses are allowed. Includes the
/// abs-prefix counter-example proving we match the repo-relative BASENAME, not
/// a `$AGEND_HOME` filesystem prefix (which would false-block every file in a
/// managed worktree under `$AGEND_HOME/worktrees/...`).
#[test]
fn trust_root_basename_denied_table_2379() {
    for p in [
        ".config-integrity-key",
        "policy.toml",
        "fleet.yaml",
        "config/policy.toml", // basename-anywhere (sub-dir dodge)
        "stash/sub/fleet.yaml",
        "deep/.config-integrity-key",
        "event-log.jsonl",
        "logs/fleet_events.jsonl", // *.jsonl glob, nested
        "a/b/c/state-transitions.jsonl",
    ] {
        assert!(trust_root_basename_denied(p), "{p:?} must be DENIED");
    }
    for p in [
        "src/main.rs",
        "Cargo.toml",
        "README.md",
        "src/policy.rs",    // not policy.toml
        "fleet.yaml.bak",   // basename ≠ fleet.yaml
        "config/fleet.yml", // .yml ≠ .yaml
        "data/notes.json",  // .json ≠ .jsonl
        "policy.toml.example",
        ".config-integrity-key.txt",
    ] {
        assert!(!trust_root_basename_denied(p), "{p:?} must be ALLOWED");
    }
    // ⚠ abs-prefix counter-example: a normal file whose ABSOLUTE path sits
    // under `$AGEND_HOME/.agend-terminal/worktrees/...` is NOT denied — only
    // its basename matters. Proves basename-match ≠ `$AGEND_HOME`-prefix match
    // (the bug the lead flagged: a prefix test would block 100% of pushes).
    assert!(!trust_root_basename_denied(
        "/Users/x/.agend-terminal/worktrees/dev/feat/x/src/foo.rs"
    ));
    // …and a genuine trust-root basename in such a path IS denied — by basename.
    assert!(trust_root_basename_denied(
        "/Users/x/.agend-terminal/worktrees/dev/feat/x/fleet.yaml"
    ));
}

fn git_run_2379(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .expect("git spawn")
}

/// Build a temp repo with a real `origin` remote, seed `origin/main` with one
/// commit, and check out a fresh `feat/test` branch — so `origin/main..HEAD`
/// resolves. Returns the worktree path (caller removes `worktree.parent()`).
fn build_repo_with_origin_main_2379(tag: &str) -> std::path::PathBuf {
    let id = format!(
        "{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let base = std::env::temp_dir().join(format!("agend-2379-{id}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let origin_bare = base.join("origin.git");
    let worktree = base.join("worktree");
    assert!(git_run_2379(
        &[
            "init",
            "--bare",
            "-b",
            "main",
            origin_bare.to_str().unwrap()
        ],
        &base
    )
    .status
    .success());
    assert!(git_run_2379(
        &[
            "clone",
            origin_bare.to_str().unwrap(),
            worktree.to_str().unwrap()
        ],
        &base
    )
    .status
    .success());
    git_run_2379(&["config", "user.name", "test"], &worktree);
    git_run_2379(&["config", "user.email", "test@test.local"], &worktree);
    git_run_2379(&["config", "commit.gpgsign", "false"], &worktree);
    std::fs::write(worktree.join("README.md"), "initial\n").unwrap();
    assert!(git_run_2379(&["add", "README.md"], &worktree)
        .status
        .success());
    assert!(git_run_2379(&["commit", "-m", "initial"], &worktree)
        .status
        .success());
    assert!(git_run_2379(&["push", "origin", "main"], &worktree)
        .status
        .success());
    assert!(git_run_2379(&["checkout", "-b", "feat/test"], &worktree)
        .status
        .success());
    worktree
}

/// CONTRACT RED→GREEN (real-git): a clean push range is allowed, but one that
/// carries a force-added trust-root file (`git add -f` bypasses `.gitignore` —
/// the actual threat) is DENIED with an actionable reason naming the file. RED
/// if `trust_root_basename_denied` always returns false (the deny disappears).
/// Joins the nextest `git-subprocess` group (spawns real git; #1893).
#[test]
fn denylist_blocks_force_added_trust_root_in_push_range_2379() {
    let wt = build_repo_with_origin_main_2379("blocks");
    // Clean commit — must be allowed.
    std::fs::write(wt.join("feature.txt"), "real work\n").unwrap();
    assert!(git_run_2379(&["add", "feature.txt"], &wt).status.success());
    assert!(git_run_2379(&["commit", "-m", "feat: real"], &wt)
        .status
        .success());
    assert_eq!(
        push_trust_root_denylist_violation(wt.to_str().unwrap()),
        None,
        "a clean push range must be allowed"
    );

    // Force-add a trust-root file (simulates the gitignore-bypass threat).
    std::fs::write(wt.join("fleet.yaml"), "stolen\n").unwrap();
    assert!(git_run_2379(&["add", "-f", "fleet.yaml"], &wt)
        .status
        .success());
    assert!(git_run_2379(&["commit", "-m", "sneak in trust-root"], &wt)
        .status
        .success());
    let violation = push_trust_root_denylist_violation(wt.to_str().unwrap());
    assert!(
        violation
            .as_deref()
            .is_some_and(|r| r.contains("fleet.yaml")),
        "force-added trust-root file must be denied with an actionable reason naming it, \
             got: {violation:?}"
    );

    let _ = std::fs::remove_dir_all(wt.parent().unwrap());
}

/// A trust-root blob added in an INTERMEDIATE commit then deleted in a later
/// one is still in the pushed history — the per-commit `--name-only` union
/// catches it (a net `diff` would miss it). Joins `git-subprocess`.
#[test]
fn denylist_catches_trust_root_added_then_deleted_in_range_2379() {
    let wt = build_repo_with_origin_main_2379("addremove");
    std::fs::write(wt.join("policy.toml"), "x\n").unwrap();
    assert!(git_run_2379(&["add", "-f", "policy.toml"], &wt)
        .status
        .success());
    assert!(git_run_2379(&["commit", "-m", "add trust-root"], &wt)
        .status
        .success());
    // Later commit removes it — but the blob remains in the pushed range.
    assert!(git_run_2379(&["rm", "policy.toml"], &wt).status.success());
    assert!(git_run_2379(&["commit", "-m", "remove it"], &wt)
        .status
        .success());
    let violation = push_trust_root_denylist_violation(wt.to_str().unwrap());
    assert!(
        violation
            .as_deref()
            .is_some_and(|r| r.contains("policy.toml")),
        "trust-root added-then-deleted in range must still be denied, got: {violation:?}"
    );
    let _ = std::fs::remove_dir_all(wt.parent().unwrap());
}

/// FAIL-CLOSED: when the range can't be computed (`origin/main` absent), the
/// denylist refuses the push rather than allowing it unverified — STRICTER than
/// `cleanup_init_pile_pre_push`, which no-ops on the same error. Joins
/// `git-subprocess`.
#[test]
fn denylist_fails_closed_when_origin_main_missing_2379() {
    let base = std::env::temp_dir().join(format!(
        "agend-2379-failclosed-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // A repo with NO remote / no origin/main ref → origin/main..HEAD errors.
    assert!(
        git_run_2379(&["init", "-b", "main", base.to_str().unwrap()], &base)
            .status
            .success()
    );
    git_run_2379(&["config", "user.name", "test"], &base);
    git_run_2379(&["config", "user.email", "test@test.local"], &base);
    git_run_2379(&["config", "commit.gpgsign", "false"], &base);
    std::fs::write(base.join("a.txt"), "x\n").unwrap();
    assert!(git_run_2379(&["add", "a.txt"], &base).status.success());
    assert!(git_run_2379(&["commit", "-m", "c"], &base).status.success());
    let violation = push_trust_root_denylist_violation(base.to_str().unwrap());
    assert!(
        violation
            .as_deref()
            .is_some_and(|r| r.contains("fail-closed")),
        "missing origin/main must FAIL CLOSED with an actionable reason, got: {violation:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Persistent guard (lead decision b): the denylist patterns must produce ZERO
/// hits on the REAL repo's tracked tree. If a legitimate tracked `*.jsonl`
/// fixture / `policy.toml` / etc. is ever committed, this RED-flags so the deny
/// surface is reviewed (allowlist) instead of silently blocking real pushes —
/// replaces the spike's one-shot manual `git ls-tree` probe. Joins
/// `git-subprocess` (spawns real git).
#[test]
fn tracked_tree_has_zero_trust_root_hits_persistent_guard_2379() {
    let out = Command::new("git")
        .args(["ls-files"])
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git ls-files spawn");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hits: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .filter(|p| trust_root_basename_denied(p))
        .collect();
    assert!(
        hits.is_empty(),
        "tracked tree must have ZERO trust-root denylist hits; found {hits:?}. If one is a \
             legitimate repo file, the #2379 denylist needs an allowlist entry — do NOT let it \
             through silently."
    );
}
