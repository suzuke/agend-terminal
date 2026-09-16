//! #3630 regression witness for canonical team/member ordering during hot reload.
//!
//! Kept in a sibling `*tests*.rs` file so the large `app_state.rs` production
//! module stays below the repository's anti-monolith source-size invariant.

use super::*;

#[test]
fn red_3630_hot_reload_preserves_canonical_member_order() {
    let home = std::env::temp_dir().join(format!(
        "agend-test-red-3630-hot-order-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  lead: {}\n  second: {}\n  third: {}\nteams:\n  svc:\n    orchestrator: lead\n    members: [lead, second, third]\n",
    )
    .expect("write fleet.yaml");

    let mut state = AppState::new();
    let mut pane_builder = |name: &str, layout: &mut Layout| test_remote_pane(layout, name);
    state.place_remote_team_grouped(
        &[
            "third".to_string(),
            "lead".to_string(),
            "second".to_string(),
        ],
        &home,
        &mut pane_builder,
    );

    assert_eq!(state.ui.layout.tabs.len(), 1);
    assert_eq!(
        state.ui.layout.tabs[0].root().agent_names(),
        vec!["lead", "second", "third"],
        "hot reload must preserve TeamConfig.members order"
    );
    std::fs::remove_dir_all(&home).ok();
}
