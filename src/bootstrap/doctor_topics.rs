//! Sprint 59 Wave 2 PR-IMPL (F2 — reduced 2-class) — operator-
//! callable diagnostic + cleanup surface for telegram topic state.
//!
//! Backs the `agend-terminal doctor topics [--cleanup] [--format
//! human|json]` CLI subcommand. Reads `topics.json` (registry) +
//! `fleet.yaml` (instance list) and classifies every observable
//! (topic_id, instance_name) pair into one of 2 mutually-exclusive
//! classes.
//!
//! Post-#994: topics.json is the single source of truth for
//! topic_id. The former 4-class taxonomy (live / drift_fleet /
//! stale_registry / orphan) collapsed to 2 classes (live / orphan)
//! because drift_fleet and stale_registry were only possible when
//! fleet.yaml carried an independent topic_id copy.
//!
//! ## Why not `stale_chat`
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
//! ## Classification algorithm (2 classes)
//!
//! For each `(topic_id, instance_name)` in topics.json:
//!
//! 1. `live` — instance exists in fleet.yaml.
//! 2. `orphan` — instance NOT in fleet.yaml (retired without
//!    registry cleanup).

use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::Path;

/// 2-class taxonomy (post-#994; formerly 4 classes before single-source-of-truth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicClass {
    /// Instance in topics.json AND in fleet.yaml.
    Live,
    /// Instance in topics.json but NOT in fleet.yaml.
    Orphan,
}

impl TopicClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TopicClass::Live => "live",
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
}

/// #991 PR-D: a fleet.yaml instance eligible for the `bind_topic` MCP
/// action — no topics.json entry yet, and not `skip` mode (which
/// `bind_topic_for_instance`, topic_registry.rs:154, permanently refuses).
/// Structurally separate from [`ClassifiedTopic`] (sourced from fleet.yaml,
/// not topics.json — there is no `topic_id` to report yet, that's the
/// point) rather than a third [`TopicClass`] variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundEntry {
    pub instance_name: String,
    /// Verbatim `topic_binding_mode` from fleet.yaml — `Some("deferred")`,
    /// `Some("auto")`, or `None` (unset, defaults to auto behavior). Lets an
    /// operator distinguish a deliberately-deferred instance from an
    /// `auto`-mode one that missed its topic at spawn (e.g. the ~6s
    /// post-boot window) — the latter may be a symptom worth investigating,
    /// not just a state to retrofit.
    pub topic_binding_mode: Option<String>,
}

/// Classification report — sorted by topic_id for stable output.
#[derive(Debug, Clone, Default)]
pub struct TopicReport {
    pub entries: Vec<ClassifiedTopic>,
    /// #991 PR-D: bind_topic-eligible fleet.yaml instances with no topic
    /// yet. Sorted by instance name for stable output.
    pub unbound: Vec<UnboundEntry>,
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
    let fleet_instance_names: std::collections::HashSet<String> = fleet
        .as_ref()
        .map(|c| c.instances.keys().cloned().collect())
        .unwrap_or_default();
    // Snapshot BEFORE `registry.into_iter()` consumes it below.
    let bound_instance_names: std::collections::HashSet<String> =
        registry.values().cloned().collect();

    let mut entries: Vec<ClassifiedTopic> = registry
        .into_iter()
        .filter(|(tid, name)| *tid != 1 && name != crate::channel::telegram::FLEET_BINDING_SENTINEL)
        .map(|(topic_id, instance_name)| {
            let class = if fleet_instance_names.contains(&instance_name) {
                TopicClass::Live
            } else {
                TopicClass::Orphan
            };
            ClassifiedTopic {
                topic_id,
                instance_name,
                class,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.topic_id);

    // #991 PR-D: Unbound — mirrors `bind_topic_for_instance`'s real
    // eligibility rule (topic_registry.rs:154): not `skip` mode AND no
    // topic yet. Deliberately broader than PRERESEARCH's original
    // "deferred only" wording — an `auto`-mode instance that missed its
    // topic at spawn is equally bind_topic-eligible.
    let mut unbound: Vec<UnboundEntry> = fleet
        .as_ref()
        .map(|c| {
            c.instances
                .iter()
                .filter(|(name, inst)| {
                    inst.topic_binding_mode.as_deref() != Some("skip")
                        && !bound_instance_names.contains(name.as_str())
                })
                .map(|(name, inst)| UnboundEntry {
                    instance_name: name.clone(),
                    topic_binding_mode: inst.topic_binding_mode.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    unbound.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));

    TopicReport {
        entries,
        unbound,
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
        for class in [TopicClass::Live, TopicClass::Orphan] {
            let count = report.count_by_class(class);
            if count == 0 {
                continue;
            }
            out.push_str(&format!("  {count} {} ", class.as_str()));
            let names: Vec<String> = report
                .entries
                .iter()
                .filter(|e| e.class == class)
                .map(|e| format!("{}:{}", e.instance_name, e.topic_id))
                .collect();
            out.push_str(&format!("({})\n", names.join(", ")));
        }
    }
    if !report.unbound.is_empty() {
        out.push_str(&format!("  {} unbound (", report.unbound.len()));
        let names: Vec<String> = report
            .unbound
            .iter()
            .map(|e| {
                format!(
                    "{}:{}",
                    e.instance_name,
                    e.topic_binding_mode.as_deref().unwrap_or("auto")
                )
            })
            .collect();
        out.push_str(&names.join(", "));
        out.push_str(
            ") — bind_topic-eligible, run `bind_topic instance_name=<name>` to retrofit\n",
        );
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
        out.push_str("\nRun with --cleanup to act on orphan entries.\n");
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
            })
        })
        .collect();
    let unbound: Vec<serde_json::Value> = report
        .unbound
        .iter()
        .map(|e| {
            serde_json::json!({
                "instance_name": e.instance_name,
                "topic_binding_mode": e.topic_binding_mode,
            })
        })
        .collect();
    let counts = serde_json::json!({
        "live": report.count_by_class(TopicClass::Live),
        "orphan": report.count_by_class(TopicClass::Orphan),
        "unbound": report.unbound.len(),
        "total": report.entries.len(),
    });
    let payload = serde_json::json!({
        "schema_version": 3,
        "entries": entries,
        // #991 PR-D: bind_topic-eligible fleet.yaml instances with no topic
        // yet — see `UnboundEntry` doc comment. NOT part of `entries`
        // (sourced from fleet.yaml, not topics.json — no topic_id exists),
        // and excluded from `counts.total` (which still only counts the
        // topics.json-derived live/orphan taxonomy).
        "unbound": unbound,
        "counts": counts,
        "can_manage_topics": report.can_manage_topics,
        "limitation_note": "stale_chat class unavailable per Telegram Bot API forum-topic enumeration gap (Sprint 59 Wave 2 PR-IMPL F2)",
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

/// Action taken by `--cleanup` per classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupAction {
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
    /// API error during cleanup; entry left as-is.
    SkippedApiError {
        topic_id: i32,
        instance_name: String,
        error: String,
    },
}

/// Plan + execute the cleanup pass. Only orphan entries are actionable
/// post-#994 (live entries need no cleanup; drift_fleet / stale_registry
/// no longer exist).
///
/// `can_manage_topics` short-circuits chat-mutating ops. When
/// `false` or `None`, orphan entries are skipped with a permission warning.
pub fn execute_cleanup(home: &Path, report: &TopicReport) -> Vec<CleanupAction> {
    let mut actions = Vec::new();
    let can_manage = report.can_manage_topics.unwrap_or(false);
    for entry in &report.entries {
        match entry.class {
            TopicClass::Live => {}
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
        let actions = execute_cleanup(&home, &report);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            CleanupAction::SkippedNoPermission { .. }
        ));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn execute_cleanup_live_produces_no_actions() {
        let home = tmp_home("cleanup-live");
        write_registry(&home, &[(42, "alpha")]);
        write_fleet(&home, &[("alpha", None)]);
        let mut report = classify(&home);
        report.can_manage_topics = Some(true);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Live);
        let actions = execute_cleanup(&home, &report);
        assert!(
            actions.is_empty(),
            "Live entries produce no cleanup actions"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn class_as_str_round_trips_to_taxonomy_names() {
        assert_eq!(TopicClass::Live.as_str(), "live");
        assert_eq!(TopicClass::Orphan.as_str(), "orphan");
    }

    // ── #991 PR-D: Unbound (bind_topic-eligible) classification ──

    /// Sibling of `write_fleet` with an explicit `topic_binding_mode`.
    fn write_fleet_with_binding(home: &Path, instances: &[(&str, Option<i32>, Option<&str>)]) {
        let mut yaml = String::from("instances:\n");
        for (name, tid, mode) in instances {
            yaml.push_str(&format!("  {name}:\n"));
            yaml.push_str("    backend: claude\n");
            if let Some(t) = tid {
                yaml.push_str(&format!("    topic_id: {t}\n"));
            }
            if let Some(m) = mode {
                yaml.push_str(&format!("    topic_binding_mode: {m}\n"));
            }
        }
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    /// A `deferred`-mode instance with no topics.json entry is the
    /// textbook Unbound case (PR-D's original motivating example).
    #[test]
    fn classify_unbound_deferred_instance_without_topic_991() {
        let home = tmp_home("unbound-deferred");
        write_registry(&home, &[]);
        write_fleet_with_binding(&home, &[("beta", None, Some("deferred"))]);
        let report = classify(&home);
        assert_eq!(report.unbound.len(), 1, "report={report:?}");
        assert_eq!(report.unbound[0].instance_name, "beta");
        assert_eq!(
            report.unbound[0].topic_binding_mode.as_deref(),
            Some("deferred")
        );
    }

    /// Real `bind_topic_for_instance` eligibility (topic_registry.rs:154) is
    /// "not skip AND no topic yet" — broader than PRERESEARCH's original
    /// "deferred only" wording. An `auto`-mode instance that ended up
    /// without a topic (e.g. the ~6s post-boot window) is equally
    /// bind_topic-eligible and must appear in Unbound too, or the class
    /// undercounts exactly the actionable set it exists to surface.
    #[test]
    fn classify_unbound_includes_auto_mode_missing_topic_991() {
        let home = tmp_home("unbound-auto");
        write_registry(&home, &[]);
        // No topic_binding_mode at all == "auto" (the documented default).
        write_fleet_with_binding(&home, &[("gamma", None, None)]);
        let report = classify(&home);
        assert_eq!(report.unbound.len(), 1, "report={report:?}");
        assert_eq!(report.unbound[0].instance_name, "gamma");
        assert_eq!(
            report.unbound[0].topic_binding_mode, None,
            "unset mode recorded as None (\"auto\" is the display default, not a stored value)"
        );
    }

    /// `skip` mode is explicitly refused by `bind_topic_for_instance`
    /// ("skip's literal promise is no topic, ever") — must NEVER appear in
    /// Unbound regardless of missing a topics.json entry.
    #[test]
    fn classify_skip_mode_never_unbound_991() {
        let home = tmp_home("unbound-skip-excluded");
        write_registry(&home, &[]);
        write_fleet_with_binding(&home, &[("delta", None, Some("skip"))]);
        let report = classify(&home);
        assert!(
            report.unbound.is_empty(),
            "skip-mode instances must never be Unbound: {report:?}"
        );
    }

    /// An instance that already has a topics.json entry is bound, not
    /// Unbound — regardless of its topic_binding_mode.
    #[test]
    fn classify_already_bound_instance_not_unbound_991() {
        let home = tmp_home("unbound-already-bound");
        write_registry(&home, &[(42, "epsilon")]);
        write_fleet_with_binding(&home, &[("epsilon", Some(42), Some("deferred"))]);
        let report = classify(&home);
        assert!(
            report.unbound.is_empty(),
            "an already-bound instance must not double-count as Unbound: {report:?}"
        );
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, TopicClass::Live);
    }

    /// Adding Unbound must not perturb the existing Live/Orphan taxonomy —
    /// a mixed fleet classifies each instance into exactly the right bucket.
    #[test]
    fn classify_unbound_coexists_with_live_and_orphan_991() {
        let home = tmp_home("unbound-mixed");
        // tid=1 is reserved (General/fleet-binding topic, excluded from
        // `entries` entirely) — use non-reserved ids for real test instances.
        write_registry(&home, &[(10, "live-one"), (20, "orphan-one")]);
        write_fleet_with_binding(
            &home,
            &[
                ("live-one", Some(10), None),
                ("deferred-one", None, Some("deferred")),
            ],
        );
        let report = classify(&home);
        assert_eq!(report.count_by_class(TopicClass::Live), 1);
        assert_eq!(report.count_by_class(TopicClass::Orphan), 1);
        assert_eq!(report.unbound.len(), 1);
        assert_eq!(report.unbound[0].instance_name, "deferred-one");
    }

    #[test]
    fn render_human_shows_unbound_section() {
        let home = tmp_home("render-unbound");
        write_registry(&home, &[]);
        write_fleet_with_binding(&home, &[("beta", None, Some("deferred"))]);
        let report = classify(&home);
        let out = render_human(&report);
        assert!(out.contains("unbound"), "must show unbound section: {out}");
        assert!(
            out.contains("beta:deferred"),
            "must name + tag the mode: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn render_json_includes_unbound_array_and_count() {
        let home = tmp_home("render-json-unbound");
        write_registry(&home, &[]);
        write_fleet_with_binding(&home, &[("beta", None, Some("deferred"))]);
        let report = classify(&home);
        let json_str = render_json(&report);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["counts"]["unbound"], 1);
        assert_eq!(parsed["unbound"][0]["instance_name"], "beta");
        assert_eq!(parsed["unbound"][0]["topic_binding_mode"], "deferred");
        std::fs::remove_dir_all(&home).ok();
    }
}
