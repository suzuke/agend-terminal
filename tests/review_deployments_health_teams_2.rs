//! Repro batch `deployments-health-teams` — Finding 2
//! ("create_deployment_team treats ok-false as success").
//!
//! In `src/deployments.rs::create_deployment_team`, the CREATE_TEAM
//! `crate::api::call` result is matched as:
//!
//! ```ignore
//! Ok(_) => {}                 // ok-false is a NO-OP -> no team created
//! Err(_) => { let _ = crate::teams::create(home, &team_args); }
//! ```
//!
//! A daemon rejection comes back as `Ok(v)` with `v["ok"] == false`, which
//! the bare `Ok(_) => {}` arm silently swallows: no team is created via the
//! fallback, yet `deploy()` still records `team: Some(deploy_name)`. The
//! sibling `spawn_instances` path correctly inspects
//! `v.get("ok").and_then(|b| b.as_bool()) == Some(false)`.
//!
//! The ok-false runtime path can only be produced by a live daemon, so this
//! is verified as a SOURCE INVARIANT (mirrors tests/core_mutex_invariant.rs):
//! the `create_deployment_team` body must NOT contain the bare `Ok(_) => {}`
//! no-op arm AND must inspect the `ok` field. RED now (bad arm present, no
//! ok-check), GREEN once the Ok arm treats ok-false as failure.

use std::path::PathBuf;

/// Slice out the `create_deployment_team` function body: from its `fn`
/// header to the start of the next top-level `pub fn deploy`. Returns the
/// raw text so we can assert on the match arms inside.
fn create_deployment_team_body(src: &str) -> String {
    let start = src
        .find("fn create_deployment_team(")
        .expect("create_deployment_team must exist in src/deployments.rs");
    let rest = &src[start..];
    // The function immediately preceding `pub fn deploy(` in the file.
    let end = rest
        .find("pub fn deploy(")
        .expect("pub fn deploy must follow create_deployment_team");
    rest[..end].to_string()
}

#[test]
#[ignore = "deployments-health-teams F2: red until fix; remove #[ignore] after fix to confirm"]
fn create_deployment_team_handles_ok_false_deployments_health_teams() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/deployments.rs");
    let src = std::fs::read_to_string(&path).expect("read src/deployments.rs");
    let body = create_deployment_team_body(&src);

    // BAD pattern: the CREATE_TEAM Ok arm is a bare no-op, so an ok-false
    // daemon rejection creates no team while the deployment records one.
    let has_noop_ok_arm = body.contains("Ok(_) => {}");

    // GUARD: after the fix the Ok arm must inspect the `ok` field to detect
    // a daemon rejection (mirroring spawn_instances' ok-false handling).
    let inspects_ok_field = body.contains(".as_bool()") && body.contains("\"ok\"");

    assert!(
        !has_noop_ok_arm && inspects_ok_field,
        "create_deployment_team must treat a CREATE_TEAM ok-false daemon rejection as a \
         FAILURE (fall back to teams::create / skip recording the team), not as success. \
         Found bare `Ok(_) => {{}}` no-op arm = {has_noop_ok_arm}; inspects `ok` field = \
         {inspects_ok_field}. Inspect v.get(\"ok\").and_then(|b| b.as_bool()) == Some(false) \
         like spawn_instances does."
    );
}
