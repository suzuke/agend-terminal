use std::fs;

fn protocol() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/FLEET-DEV-PROTOCOL.md"
    ))
    .expect("read fleet development protocol")
}

#[test]
fn protocol_examples_match_the_live_mcp_contract() {
    let body = protocol();

    for stale in [
        "send(kind",
        "binding_state(agent",
        "release_worktree(agent",
        "release_worktree(force:true)",
        "describe_instance",
        "repo action=checkout source=",
    ] {
        assert!(
            !body.contains(stale),
            "protocol still teaches stale MCP syntax: {stale}"
        );
    }

    for current in [
        "request_kind",
        "binding_state({instance:",
        "release_worktree({instance:",
        "list_instances({instance:",
        "repository_path:",
    ] {
        assert!(
            body.contains(current),
            "protocol must teach the live MCP contract: {current}"
        );
    }
}

#[test]
fn protocol_post_merge_ci_is_pinned_to_the_merge_head() {
    let body = protocol();

    assert!(
        !body.contains("gh run list -b main --limit 1"),
        "latest-main polling can falsely validate an unrelated newer commit"
    );
    for exact_head_field in ["head_sha:", "next_after_ci:", "<full-merge-sha>"] {
        assert!(
            body.contains(exact_head_field),
            "post-merge CI must teach exact-head watch field: {exact_head_field}"
        );
    }
}

#[test]
fn protocol_dispatch_does_not_invent_delivery_receipts() {
    let body = protocol();

    for unavailable in ["returned message ID", "`delivery_mode`"] {
        assert!(
            !body.contains(unavailable),
            "protocol must not promise unavailable send receipt: {unavailable}"
        );
    }
    assert!(
        body.contains("review_class: \"single\" | \"dual\""),
        "PR-producing branch tasks must set review_class before dispatch"
    );
}

#[test]
fn protocol_separates_worktree_release_from_branch_deletion() {
    let body = protocol();

    assert!(
        !body.contains("remote tracking ref is gone (squash-merge)"),
        "a missing remote ref alone is not branch-deletion proof"
    );
    for preservation_rule in [
        "structural squash proof",
        "24-hour age floor",
        "may be released before merge",
    ] {
        assert!(
            body.contains(preservation_rule),
            "protocol must preserve unmerged work: {preservation_rule}"
        );
    }
}

#[test]
fn protocol_red_green_and_worktree_recipes_are_daemon_managed() {
    let body = protocol();

    for unsafe_recipe in [
        "git checkout <test-sha>",
        "git checkout <pre-fix-base>",
        "git checkout <fix-head>",
        "git checkout <anchor-sha>",
        "git checkout <impl-sha>",
        "git worktree add -b review/<N>-r0",
        "Full rule + escape hatch: §10.4",
    ] {
        assert!(
            !body.contains(unsafe_recipe),
            "protocol still teaches an unsafe or dangling recipe: {unsafe_recipe}"
        );
    }

    assert!(
        body.contains("daemon-managed named worktree"),
        "RED→GREEN verification must use daemon-managed named worktrees"
    );
    assert!(
        body.contains("Full rule + exceptions: §12.4 and §13"),
        "worktree guidance must point at the current sections"
    );
}
