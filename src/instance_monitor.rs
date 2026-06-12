//! Instance monitor — collects per-instance OS-level metrics (RSS, CPU%, uptime)
//! and exposes them for the TUI Monitor tab.

use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Collection interval — operator Q7 ack'd 5 seconds.
const MONITOR_TICK: Duration = Duration::from_secs(5);

/// Per-instance metrics snapshot.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InstanceMetrics {
    pub name: String,
    pub pid: Option<u32>,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f32>,
    pub uptime_secs: Option<u64>,
    pub agent_state: String,
    pub health_state: String,
    pub heartbeat_lag_secs: Option<u64>,
    pub pending_pickup_count: usize,
    pub collected_at: Instant,
}

/// Process-wide metrics cache. Written by daemon tick, read by TUI render.
static METRICS_CACHE: OnceLock<Arc<Mutex<Vec<InstanceMetrics>>>> = OnceLock::new();

fn cache() -> &'static Arc<Mutex<Vec<InstanceMetrics>>> {
    METRICS_CACHE.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

/// Read the latest metrics snapshot. Returns empty vec if no collection has run.
pub fn latest_metrics() -> Vec<InstanceMetrics> {
    cache().lock().clone()
}

/// Spawn a dedicated monitor collection thread at 5s interval.
/// Independent from the supervisor 10s tick to meet operator Q7 spec.
pub fn spawn_monitor_tick(home: std::path::PathBuf, registry: crate::agent::AgentRegistry) {
    // fire-and-forget: monitor tick loop terminates on process exit. Sysinfo
    // collection is read-only sampling — losing one tick on shutdown is
    // harmless (next daemon start re-samples). Self-acknowledged Sprint 20
    // Track B finding (PR-AZ author = dev-impl-2).
    let _ = std::thread::Builder::new()
        .name("monitor_tick".into())
        .spawn(move || loop {
            std::thread::sleep(MONITOR_TICK);
            collect(&home, &registry);
        });
}

/// Collect metrics for all agents in the registry. Called from daemon tick.
pub fn collect(home: &std::path::Path, registry: &crate::agent::AgentRegistry) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let now = Instant::now();
    // #1441: registry is UUID-keyed; carry the id for the re-lock lookup and
    // the display name for metadata / metrics / sort.
    let handles: Vec<(crate::types::InstanceId, String, Option<u32>)> = {
        let reg = crate::agent::lock_registry(registry);
        reg.iter()
            .map(|(id, handle)| {
                let pid = handle.child.lock().process_id();
                (*id, handle.name.to_string(), pid)
            })
            .collect()
    };

    let mut metrics = Vec::with_capacity(handles.len());
    for (id, name, pid) in handles {
        let (agent_state, health_state) = {
            let reg = crate::agent::lock_registry(registry);
            reg.get(&id)
                .map(|h| {
                    let c = h.core.lock();
                    (
                        c.state.get_state().display_name().to_string(),
                        c.health.state.display_name().to_string(),
                    )
                })
                .unwrap_or_else(|| ("unknown".into(), "unknown".into()))
        };

        let (rss_bytes, cpu_percent, uptime_secs) = if let Some(p) = pid {
            let spid = sysinfo::Pid::from_u32(p);
            if let Some(proc_info) = sys.process(spid) {
                // Process tree RSS: recursive walk (main + all descendants)
                let total_rss = tree_rss(&sys, spid);
                let cpu = proc_info.cpu_usage();
                let uptime = proc_info.run_time();
                (Some(total_rss), Some(cpu), Some(uptime))
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        let (heartbeat_lag_secs, pending_pickup_count) = read_metadata_metrics(home, &name);

        metrics.push(InstanceMetrics {
            name,
            pid,
            rss_bytes,
            cpu_percent,
            uptime_secs,
            agent_state,
            health_state,
            heartbeat_lag_secs,
            pending_pickup_count,
            collected_at: now,
        });
    }

    metrics.sort_by(|a, b| a.name.cmp(&b.name));
    *cache().lock() = metrics;
}

/// One node of the process table used for the descendant-RSS walk.
struct ProcNode {
    mem: u64,
    parent: Option<u32>,
}

/// Sum RSS for `root` and all its descendants, cycle-safe and iterative.
///
/// Windows reuses PIDs aggressively, so the parent-PID graph can contain a
/// cycle (A's parent is B, B's parent is A). The previous recursive walk would
/// then recurse forever and overflow the stack (monitor_tick thread crash,
/// 2026-06-12 15:13, ~18k frames). The `visited` set breaks cycles and also
/// guarantees each process is counted at most once.
fn sum_tree_rss(root: u32, table: &std::collections::HashMap<u32, ProcNode>) -> u64 {
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![root];
    let mut total = 0u64;
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue; // already counted / cycle guard
        }
        if let Some(node) = table.get(&pid) {
            total = total.saturating_add(node.mem);
        }
        for (&cpid, node) in table {
            if node.parent == Some(pid) && !visited.contains(&cpid) {
                stack.push(cpid);
            }
        }
    }
    total
}

/// Process-tree RSS: total memory of a process plus all descendants.
fn tree_rss(sys: &sysinfo::System, root: sysinfo::Pid) -> u64 {
    let table: std::collections::HashMap<u32, ProcNode> = sys
        .processes()
        .values()
        .map(|p| {
            (
                p.pid().as_u32(),
                ProcNode {
                    mem: p.memory(),
                    parent: p.parent().map(|pp| pp.as_u32()),
                },
            )
        })
        .collect();
    sum_tree_rss(root.as_u32(), &table)
}

/// Read heartbeat lag and pending pickup count from metadata JSON.
fn read_metadata_metrics(home: &std::path::Path, name: &str) -> (Option<u64>, usize) {
    let meta_path = crate::agent_ops::metadata_path_resolved(home, name);
    let content = match std::fs::read_to_string(meta_path) {
        Ok(c) => c,
        Err(_) => return (None, 0),
    };
    let meta: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (None, 0),
    };

    let hb_lag = meta["last_heartbeat"]
        .as_str()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .and_then(|dt| {
            chrono::Utc::now()
                .signed_duration_since(dt)
                .to_std()
                .ok()
                .map(|d| d.as_secs())
        });

    let pickup_count = meta["pending_pickup_ids"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);

    (hb_lag, pickup_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_metrics_empty_on_startup() {
        // Before any collection, cache is empty.
        let m = latest_metrics();
        // May or may not be empty depending on test ordering,
        // but should not panic.
        let _ = m;
    }

    fn node(mem: u64, parent: Option<u32>) -> ProcNode {
        ProcNode { mem, parent }
    }

    #[test]
    fn sum_tree_rss_sums_root_and_descendants() {
        let mut t = std::collections::HashMap::new();
        t.insert(1, node(10, None));
        t.insert(2, node(20, Some(1)));
        t.insert(3, node(30, Some(2)));
        t.insert(9, node(99, None)); // unrelated tree, must not be counted
        assert_eq!(sum_tree_rss(1, &t), 60);
    }

    #[test]
    fn sum_tree_rss_survives_parent_cycle() {
        // Windows PID reuse: A's parent is B, B's parent is A. The old recursive
        // walk overflowed the stack here; this must terminate and count each once.
        let mut t = std::collections::HashMap::new();
        t.insert(1, node(100, Some(2)));
        t.insert(2, node(200, Some(1)));
        assert_eq!(sum_tree_rss(1, &t), 300);
    }

    #[test]
    fn sum_tree_rss_survives_self_parent() {
        let mut t = std::collections::HashMap::new();
        t.insert(1, node(50, Some(1)));
        assert_eq!(sum_tree_rss(1, &t), 50);
    }

    #[test]
    fn read_metadata_metrics_missing_file() {
        let (hb, pickup) =
            read_metadata_metrics(std::path::Path::new("/tmp/nonexistent-agend-test"), "ghost");
        assert!(hb.is_none());
        assert_eq!(pickup, 0);
    }

    #[test]
    fn read_metadata_metrics_with_data() {
        let home = std::env::temp_dir().join(format!("agend-monitor-test-{}", std::process::id()));
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        let meta = serde_json::json!({
            "last_heartbeat": "2020-01-01T00:00:00Z",
            "pending_pickup_ids": [{"kind": "telegram", "msg_id": "1"}]
        });
        std::fs::write(
            meta_dir.join("test-agent.json"),
            serde_json::to_string(&meta).expect("json"),
        )
        .ok();
        let (hb, pickup) = read_metadata_metrics(&home, "test-agent");
        assert!(hb.is_some());
        assert!(hb.expect("hb") > 0);
        assert_eq!(pickup, 1);
        std::fs::remove_dir_all(&home).ok();
    }
}
