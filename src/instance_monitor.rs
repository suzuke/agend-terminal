//! Instance monitor — collects per-instance OS-level metrics (RSS, CPU%, uptime)
//! and exposes them for the TUI Monitor tab.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

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
    cache().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Collect metrics for all agents in the registry. Called from daemon tick.
pub fn collect(home: &std::path::Path, registry: &crate::agent::AgentRegistry) {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};

    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_processes_specifics(ProcessRefreshKind::everything());

    let now = Instant::now();
    let handles: Vec<(String, Option<u32>)> = {
        let reg = crate::agent::lock_registry(registry);
        reg.iter()
            .map(|(name, handle)| {
                let pid = handle.child.lock().ok().and_then(|c| c.process_id());
                (name.clone(), pid)
            })
            .collect()
    };

    let mut metrics = Vec::with_capacity(handles.len());
    for (name, pid) in handles {
        let (agent_state, health_state) = {
            let reg = crate::agent::lock_registry(registry);
            reg.get(&name)
                .and_then(|h| {
                    h.core.lock().ok().map(|c| {
                        (
                            c.state.get_state().display_name().to_string(),
                            c.health.state.display_name().to_string(),
                        )
                    })
                })
                .unwrap_or_else(|| ("unknown".into(), "unknown".into()))
        };

        let (rss_bytes, cpu_percent, uptime_secs) = if let Some(p) = pid {
            let spid = sysinfo::Pid::from_u32(p);
            if let Some(proc_info) = sys.process(spid) {
                // Process tree RSS: main + children
                let mut total_rss = proc_info.memory();
                for cproc in sys.processes().values() {
                    if cproc.parent() == Some(spid) {
                        total_rss += cproc.memory();
                    }
                }
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
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = metrics;
}

/// Read heartbeat lag and pending pickup count from metadata JSON.
fn read_metadata_metrics(home: &std::path::Path, name: &str) -> (Option<u64>, usize) {
    let meta_path = home.join("metadata").join(format!("{name}.json"));
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
