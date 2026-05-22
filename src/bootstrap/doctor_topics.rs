//! Sprint 59 Wave 2 PR-IMPL (F2 — γ-reduced 4-class) — operator-
//! callable diagnostic + cleanup surface for telegram topic state.
//!
//! Backs the `agend-terminal doctor topics [--cleanup] [--format
//! human|json]` CLI subcommand. Reads `topics.json` (registry) +
//! `fleet.yaml` (instance list) and classifies every observable
//! (topic_id, instance_name) pair into one of 4 mutually-exclusive
//! classes via precedence-ordered first-match-wins assignment.
//!
//! Post-#994 Phase 1: topics.json is the single source of truth for
//! topic_id. The `fleet_topic_ids` comparison also reads topics.json,
//! making DriftFleet and StaleRegistry unreachable from `classify()`.
//! These classes remain in the enum for manual report construction.
//!
//! ## Why 4 classes (not 5)
//!
//! The Sprint 59 Wave 2 PR-1 RCA originally proposed a 5-class
//! taxonomy that included `stale_chat` (topic exists in chat but
//! not in topics.json). Detecting `stale_chat` requires
//! enumerating live forum topics in the chat — the Telegram Bot
//! API + teloxide-core 0.11.2 do NOT expose any "list forum
//! topics" method. Per surface-block #1 + (F2) decision (lead
//! m-20260509165526589860-288 + general
//! m-20260509165440054018-286), `stale_chat` is dropped from the
//! taxonomy.
//!
//! Operators encountering chat-side topics that are NOT tracked
//! by `topics.json` must verify via the Telegram UI directly —
//! the `--cleanup` flag cannot detect or act on that class.
//! Sprint 60+ candidate: teloxide upgrade evaluation if a future
//! Bot API version exposes forum-topic enumeration.
//!
//! ## Classification algorithm (4 classes, precedence-ordered)
//!
//! For each `(topic_id, instance_name)` candidate observed across
//! topics.json + fleet.yaml instance list, apply rules in order;
//! assign first-match:
//!
//! 1. `live` — present in topics.json AND instance in fleet.yaml.
//! 2. `drift_fleet` — (unreachable post-#994: same-source comparison)
//! 3. `stale_registry` — (unreachable post-#994: same-source comparison)
//! 4. `orphan` — present in topics.json mapping to instance name
//!    NOT in fleet.yaml. Instance retired without registry cleanup.

use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::Path;

/// 4-class taxonomy (per F2 reduced from RCA's original 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicClass {
    /// topics.json entry matches (post-#994: always the case).
    Live,
    /// topics.json entries differ (unreachable post-#994).
    DriftFleet,
    /// topics.json says it should exist; chat-side unverifiable.
    StaleRegistry,
    /// topics.json maps to instance NOT in fleet.yaml.
    Orphan,
}

impl TopicClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TopicClass::Live => "live",
            TopicClass::DriftFleet => "drift_fleet",
            TopicClass::StaleRegistry => "stale_registry",
            TopicClass::Orphan => "orphan",
        }
    }
}

/// One classified topic entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedTopic {
    pub topic_id: i32,
    pub instance_name: String,
    pub class: TopicClass,
    /// `Some(fleet_id)` from topics.json lookup for the instance.
    /// `None` when not in topics.json.
    pub fleet_topic_id: Option<i32>,
}

/// Classification report — sorted by topic_id for stable output.
#[derive(Debug, Clone, Default)]
pub struct TopicReport {
    pub entries: Vec<ClassifiedTopic>,
    /// `can_manage_topics` permission status; `None` when the
    /// telegram channel is unconfigured/unavailable. `--cleanup`
    /// chat-mutating operations gated on `Some(true)`.
    pub can_manage_topics: Option<bool>,
}

impl TopicReport {
    pub fn count_by_class(&self, class: TopicClass) -> usize {
        self.entries.iter().filter(|e| e.class == class).count()
    }
}

/// Build the classification report by reading the on-disk sources.
/// Pure function — no chat-side network calls beyond the optional
/// `can_manage_topics` permission probe done elsewhere.
pub fn classify(home: &Path) -> TopicReport {
    let registry = load_topic_registry(home);
    let fleet = FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    let fleet_topic_ids: HashMap<String, i32> = {
        let reg = load_topic_registry(home);
        reg.into_iter().map(|(tid, name)| (name, tid)).collect()
    };
    let fleet_instance_names: std::collections::HashSet<String> = fleet
        .as_ref()
        .map(|c| c.instances.keys().cloned().collect())
        .unwrap_or_default();

    let mut entries: Vec<ClassifiedTopic> = registry
        .into_iter()
        // Skip the fleet_binding sentinel + `general` topic_id=1
        // pseudo-instance — these are reserved markers, not user-
        // facing topics that operators classify.
        .filter(|(tid, name)| *tid != 1 && name != crate::channel::telegram::FLEET_BINDING_SENTINEL)
        .map(|(topic_id, instance_name)| {
            let fleet_topic_id = fleet_topic_ids.get(&instance_name).copied();
            let in_fleet = fleet_instance_names.contains(&instance_name);
            // Precedence-ordered first-match-wins assignment per RCA §4.1
            // (4-class reduction per F2):
            // 1. orphan — instance not in fleet.yaml (regardless of
            //    fleet topic_id state, which would be None for
            //    missing instances anyway).
            let class = if !in_fleet {
                TopicClass::Orphan
            } else if let Some(fleet_id) = fleet_topic_id {
                if fleet_id == topic_id {
                    // 2. live — registry + fleet agree.
                    // Note: chat-side unverifiable per F2 API gap;
                    // class is "registry says live, chat unverified".
                    TopicClass::Live
                } else {
                    // 3. drift_fleet — registry + fleet differ.
                    TopicClass::DriftFleet
                }
            } else {
                // 4. stale_registry — instance in fleet but no
                //    fleet topic_id (or it's been cleared);
                //    registry is the only anchor.
                TopicClass::StaleRegistry
            };
            ClassifiedTopic {
                topic_id,
                instance_name,
                class,
                fleet_topic_id,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.topic_id);

    TopicReport {
        entries,
        // can_manage_topics is set externally by callers that
        // probe the permission via a network call — cli.rs
        // populates it before display.
        can_manage_topics: None,
    }
}

/// Load topic registry from `topics.json`. Returns empty map on
/// missing/corrupt file — operators get an empty report rather
/// than a crash.
fn load_topic_registry(home: &Path) -> HashMap<i32, String> {
    let path = home.join("topics.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| k.parse::<i32>().ok().map(|id| (id, v)))
                .collect()
        })
        .unwrap_or_default()
}

/// Render the report as a human-readable multi-line string.
pub fn render_human(report: &TopicReport) -> String {
    let mut out = String::new();
    out.push_str("Telegram topic state:\n");
    let total = report.entries.len();
    if total == 0 {
        out.push_str("  (no tracked topics in topics.json)\n");
    } else {
        for class in [
            TopicClass::Live,
            TopicClass::DriftFleet,
            TopicClass::StaleRegistry,
            TopicClass::Orphan,
        ] {
            let count = report.count_by_class(class);
            if count == 0 {
                continue;
            }
            out.push_str(&format!("  {count} {} ", class.as_str()));
            let names: Vec<String> = report
                .entries
                .iter()
                .filter(|e| e.class == class)
                .map(|e| {
                    let drift_note =
                        if let (TopicClass::DriftFleet, Some(fid)) = (e.class, e.fleet_topic_id) {
                            format!(" (registry={}, fleet={})", e.topic_id, fid)
                        } else {
                            format!(":{}", e.topic_id)
                        };
                    format!("{}{}", e.instance_name, drift_note)
                })
                .collect();
            out.push_str(&format!("({})\n", names.join(", ")));
        }
    }
    out.push('\n');
    match report.can_manage_topics {
        Some(true) => {
            out.push_str("Bot can_manage_topics: ✓ enabled (cleanup operations available)\n")
        }
        Some(false) => out.push_str(
            "Bot can_manage_topics: ✗ DISABLED — chat-mutating cleanup will be skipped. \
             Grant via Telegram → Chat → Manage admins → bot name → enable 'Manage topics'.\n",
        ),
        None => {
            out.push_str("Bot can_manage_topics: ? (telegram channel unconfigured/unavailable)\n")
        }
    }
    out.push('\n');
    out.push_str(
        "F2 limitation note: `stale_chat` class detection is unavailable — Telegram Bot API \
         does not expose forum-topic enumeration. Operators must verify chat-side state via \
         the Telegram UI directly. Sprint 60+ candidate tracks teloxide upgrade evaluation.\n",
    );
    if total > 0 {
        out.push_str(
            "\nRun with --cleanup to act on stale_registry / drift_fleet / orphan entries.\n",
        );
    }
    out
}

/// Render the report as JSON.
pub fn render_json(report: &TopicReport) -> String {
    let entries: Vec<serde_json::Value> = report
        .entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "topic_id": e.topic_id,
                "instance_name": e.instance_name,
                "class": e.class.as_str(),
                "fleet_topic_id": e.fleet_topic_id,
            })
        })
        .collect();
    let counts = serde_json::json!({
        "live": report.count_by_class(TopicClass::Live),
        "drift_fleet": report.count_by_class(TopicClass::DriftFleet),
        "stale_registry": report.count_by_class(TopicClass::StaleRegistry),
        "orphan": report.count_by_class(TopicClass::Orphan),
        "total": report.entries.len(),
    });
    let payload = serde_json::json!({
        "schema_version": 1,
        "entries": entries,
        "counts": counts,
        "can_manage_topics": report.can_manage_topics,
        "limitation_note": "stale_chat class unavailable per Telegram Bot API forum-topic enumeration gap (Sprint 59 Wave 2 PR-IMPL F2)",
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

/// Action taken by `--cleanup` per classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupAction {
    /// Removed from `topics.json`; no chat operation.
    UnregisteredOnly {
        topic_id: i32,
        instance_name: String,
    },
    /// Called `delete_topic` on chat side + unregistered.
    DeletedFromChatAndRegistry {
        topic_id: i32,
        instance_name: String,
    },
    /// Skipped because the bot lacks `can_manage_topics`.
    SkippedNoPermission {
        topic_id: i32,
        instance_name: String,
    },
    /// Skipped per operator's `--prefer-fleet` / `--prefer-registry`
    /// resolution choice not matching this entry's class needs.
    SkippedNeedsResolution {
        topic_id: i32,
        instance_name: String,
        reason: &'static str,
    },
    /// API error during cleanup; entry left as-is.
    SkippedApiError {
        topic_id: i32,
        instance_name: String,
        error: String,
    },
}

/// Cleanup preference for `drift_fleet` resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftResolution {
    /// Take fleet.yaml as authoritative; update topics.json to
    /// match. (Default if operator doesn't pick.)
    PreferFleet,
    /// Take topics.json as authoritative; update fleet.yaml to
    /// match.
    PreferRegistry,
    /// Don't resolve; operator wants to see the drift surfaced
    /// without acting.
    LeaveDrift,
}

/// Plan + execute the cleanup pass. The planning + execution split
/// makes it testable without mocking the telegram API: pure
/// classification + plan in this function; chat-side mutation
/// happens via the existing `delete_topic` / `unregister_topic`
/// surfaces.
///
/// `can_manage_topics` short-circuits chat-mutating ops. When
/// `false` or `None`, only `UnregisteredOnly` actions fire (registry-
/// only cleanup of `stale_registry` entries).
pub fn execute_cleanup(
    home: &Path,
    report: &TopicReport,
    drift_resolution: DriftResolution,
) -> Vec<CleanupAction> {
    let mut actions = Vec::new();
    let can_manage = report.can_manage_topics.unwrap_or(false);
    for entry in &report.entries {
        match entry.class {
            TopicClass::Live => {
                // No action — agreement state.
            }
            TopicClass::StaleRegistry => {
                // Registry says it should exist; chat-side
                // unverifiable. Operator's recovery path is manual
                // verify; we surface the entry but don't auto-clean
                // (would risk losing a real-but-unverified topic).
                actions.push(CleanupAction::SkippedNeedsResolution {
                    topic_id: entry.topic_id,
                    instance_name: entry.instance_name.clone(),
                    reason: "stale_registry: chat-side unverifiable per teloxide API gap; \
                             operator must manually verify via Telegram UI",
                });
            }
            TopicClass::DriftFleet => match drift_resolution {
                DriftResolution::PreferFleet => {
                    // Update topics.json to match fleet.yaml.
                    if let Some(fid) = entry.fleet_topic_id {
                        let mut reg = registry_load(home);
                        reg.remove(&entry.topic_id);
                        reg.insert(fid, entry.instance_name.clone());
                        registry_save(home, &reg);
                        actions.push(CleanupAction::UnregisteredOnly {
                            topic_id: entry.topic_id,
                            instance_name: entry.instance_name.clone(),
                        });
                    }
                }
                DriftResolution::PreferRegistry => {
                    tracing::info!(
                        instance = %entry.instance_name,
                        topic_id = entry.topic_id,
                        "doctor: PreferRegistry is no-op post-refactor (topics.json is canonical)"
                    );
                    actions.push(CleanupAction::SkippedNeedsResolution {
                        topic_id: entry.topic_id,
                        instance_name: entry.instance_name.clone(),
                        reason: "prefer_registry: no-op (topics.json is already canonical source)",
                    });
                }
                DriftResolution::LeaveDrift => {
                    actions.push(CleanupAction::SkippedNeedsResolution {
                        topic_id: entry.topic_id,
                        instance_name: entry.instance_name.clone(),
                        reason: "drift_fleet: operator chose to leave drift unresolved",
                    });
                }
            },
            TopicClass::Orphan => {
                if !can_manage {
                    actions.push(CleanupAction::SkippedNoPermission {
                        topic_id: entry.topic_id,
                        instance_name: entry.instance_name.clone(),
                    });
                    continue;
                }
                use crate::channel::telegram::topic_registry::{delete_topic, DeleteTopicOutcome};
                match delete_topic(home, entry.topic_id) {
                    DeleteTopicOutcome::Deleted => {
                        actions.push(CleanupAction::DeletedFromChatAndRegistry {
                            topic_id: entry.topic_id,
                            instance_name: entry.instance_name.clone(),
                        });
                    }
                    DeleteTopicOutcome::PermissionDenied => {
                        actions.push(CleanupAction::SkippedNoPermission {
                            topic_id: entry.topic_id,
                            instance_name: entry.instance_name.clone(),
                        });
                    }
                    DeleteTopicOutcome::ApiError(e) => {
                        actions.push(CleanupAction::SkippedApiError {
                            topic_id: entry.topic_id,
                            instance_name: entry.instance_name.clone(),
                            error: e,
                        });
                    }
                    DeleteTopicOutcome::ChannelUnavailable => {
                        actions.push(CleanupAction::SkippedApiError {
                            topic_id: entry.topic_id,
                            instance_name: entry.instance_name.clone(),
                            error: "channel unavailable".to_string(),
                        });
                    }
                }
            }
        }
    }
    actions
}

fn registry_load(home: &Path) -> HashMap<i32, String> {
    load_topic_registry(home)
}

fn registry_save(home: &Path, reg: &HashMap<i32, String>) {
    let map: HashMap<String, &String> = reg.iter().map(|(k, v)| (k.to_string(), v)).collect();
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = crate::store::atomic_write(&home.join("topics.json"), json.as_bytes());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-doctor-topics-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_registry(home: &Path, entries: &[(i32, &str)]) {
        let map: HashMap<String, String> = entries
            .iter()
            .map(|(tid, name)| (tid.to_string(), name.to_string()))
            .collect();
        let json = serde_json::to_string_pretty(&map).unwrap();
        std::fs::write(home.join("topics.json"), json).unwrap();
    }

    fn write_fleet(home: &Path, instances: &[(&str, Option<i32>)]) {
        let mut yaml = String::from("instances:\n");
        for (name, tid) in instances {
            yaml.push_str(&format!("  {name}:\n"));
            yaml.push_str("    backend: claude\n");
            if let Some(t) = tid {
                yaml.push_str(&format!("    topic_id: {t}\n"));
            }
        }
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    #[test]
    fn classify_live_when_registry_and_fleet_agree() {
        let home = tmp_home("live");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", Some(42))]);
        let report = classify(&home);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Live);
        assert_eq!(report.entries[0].topic_id, 42);
        assert_eq!(report.entries[0].instance_name, "alpha");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn classify_live_even_when_fleet_yaml_differs() {
        let home = tmp_home("drift-now-live");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", Some(99))]);
        let report = classify(&home);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(
            report.entries[0].class,
            TopicClass::Live,
            "post-refactor: fleet_topic_ids reads topics.json, not fleet.yaml — always agrees with registry"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn classify_live_when_fleet_yaml_lacks_topic_id() {
        let home = tmp_home("stale-now-live");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", None)]);
        let report = classify(&home);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(
            report.entries[0].class,
            TopicClass::Live,
            "post-refactor: topics.json is canonical — fleet.yaml topic_id irrelevant"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn classify_orphan_when_instance_missing_from_fleet() {
        let home = tmp_home("orphan");
        write_registry(&home, &[(42, "retired-agent")]);
        write_fleet(&home, &[("alpha", Some(1))]); // alpha exists, retired-agent doesn't
        let report = classify(&home);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Orphan);
        assert_eq!(report.entries[0].instance_name, "retired-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn precedence_orphan_beats_other_classes_when_instance_absent() {
        // Even when topic_id matches fleet.yaml entry value (which
        // wouldn't happen in practice for an orphan, but pin the
        // precedence semantic): orphan wins because instance is
        // not in fleet.yaml.
        let home = tmp_home("precedence-orphan");
        write_registry(&home, &[(42, "ghost")]);
        write_fleet(&home, &[("alpha", Some(42))]); // ghost not in fleet
        let report = classify(&home);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Orphan);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_binding_sentinel_excluded_from_classification() {
        let home = tmp_home("sentinel-exclude");
        write_registry(
            &home,
            &[
                (42, "alpha"),
                (1, "general"),
                (99, crate::channel::telegram::FLEET_BINDING_SENTINEL),
            ],
        );
        write_fleet(&home, &[("alpha", Some(42))]);
        let report = classify(&home);
        // Only alpha should appear; general (tid=1) + fleet_binding sentinel filtered out.
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].instance_name, "alpha");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn classify_handles_missing_registry_file() {
        let home = tmp_home("missing-registry");
        write_fleet(&home, &[("alpha", Some(42))]);
        let report = classify(&home);
        assert!(report.entries.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn classify_handles_missing_fleet_yaml() {
        let home = tmp_home("missing-fleet");
        write_registry(&home, &[(42, "alpha")]);
        // No fleet.yaml — instance absent everywhere.
        let report = classify(&home);
        assert_eq!(report.entries.len(), 1);
        // No fleet.yaml means instance not in fleet → orphan
        assert_eq!(report.entries[0].class, TopicClass::Orphan);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn render_human_includes_class_counts_and_permission_status() {
        let home = tmp_home("render-human");
        write_registry(&home, &[(42, "alpha"), (43, "beta")]);
        write_fleet(&home, &[("alpha", Some(42)), ("beta", Some(99))]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(true);
        let out = render_human(&report);
        // Post-refactor: fleet_topic_ids comes from topics.json (same source as registry),
        // so both entries are Live — DriftFleet is unreachable from classify().
        assert!(
            out.contains("2 live"),
            "human output must show live count: {out}"
        );
        assert!(
            out.contains("can_manage_topics: ✓"),
            "permission status must be visible: {out}"
        );
        assert!(
            out.contains("F2 limitation note"),
            "limitation note must be present: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn render_human_warns_when_permission_disabled() {
        let home = tmp_home("render-no-perm");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", Some(42))]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(false);
        let out = render_human(&report);
        assert!(out.contains("can_manage_topics: ✗"), "must warn: {out}");
        assert!(out.contains("Manage topics"), "actionable hint: {out}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn render_json_structured_payload() {
        let home = tmp_home("render-json");
        write_registry(&home, &[(42, "alpha"), (43, "ghost")]);
        write_fleet(&home, &[("alpha", Some(42))]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(true);
        let json_str = render_json(&report);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["counts"]["live"], 1);
        assert_eq!(parsed["counts"]["orphan"], 1);
        assert_eq!(parsed["counts"]["total"], 2);
        assert!(parsed["limitation_note"].is_string());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn execute_cleanup_orphan_skipped_without_permission() {
        let home = tmp_home("cleanup-no-perm");
        write_registry(&home, &[(42, "ghost")]);
        write_fleet(&home, &[("alpha", Some(1))]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(false);
        let actions = execute_cleanup(&home, &report, DriftResolution::PreferFleet);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            CleanupAction::SkippedNoPermission { .. }
        ));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn execute_cleanup_former_stale_registry_now_live() {
        // Post-refactor: fleet_topic_ids comes from topics.json, so a registry
        // entry always matches itself → Live (StaleRegistry unreachable).
        let home = tmp_home("cleanup-stale");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", None)]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(true);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Live);
        let actions = execute_cleanup(&home, &report, DriftResolution::PreferFleet);
        assert!(
            actions.is_empty(),
            "Live entries produce no cleanup actions"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn execute_cleanup_former_drift_now_live_prefer_fleet() {
        // Post-refactor: fleet_topic_ids comes from topics.json, so registry
        // entry always agrees with itself → Live (DriftFleet unreachable).
        let home = tmp_home("cleanup-drift-fleet");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", Some(99))]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(true);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Live);
        let actions = execute_cleanup(&home, &report, DriftResolution::PreferFleet);
        assert!(
            actions.is_empty(),
            "Live entries produce no cleanup actions"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn execute_cleanup_former_drift_now_live_leave() {
        // Post-refactor: fleet_topic_ids comes from topics.json, so registry
        // entry always agrees with itself → Live (DriftFleet unreachable).
        let home = tmp_home("cleanup-drift-leave");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", Some(99))]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(true);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Live);
        let actions = execute_cleanup(&home, &report, DriftResolution::LeaveDrift);
        assert!(
            actions.is_empty(),
            "Live entries produce no cleanup actions"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn class_as_str_round_trips_to_taxonomy_names() {
        assert_eq!(TopicClass::Live.as_str(), "live");
        assert_eq!(TopicClass::DriftFleet.as_str(), "drift_fleet");
        assert_eq!(TopicClass::StaleRegistry.as_str(), "stale_registry");
        assert_eq!(TopicClass::Orphan.as_str(), "orphan");
    }
}
