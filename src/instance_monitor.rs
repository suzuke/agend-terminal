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

/// Last time a consumer read `latest_metrics()`. The only consumer is the TUI
/// Monitor view, which reads once per frame it draws — so a recent read means a
/// subscriber is actively watching. Drives the collection gate (`should_collect`)
/// so a headless daemon (or a TUI not on the Monitor tab) skips the whole sysinfo
/// sweep instead of sampling every 5s for nobody.
static LAST_METRICS_READ: Mutex<Option<Instant>> = Mutex::new(None);

/// How many tick intervals a subscriber's last read keeps collection warm. >1 so
/// a brief render hiccup doesn't drop a frame's worth of freshness; the Monitor
/// view reads every frame, so a live viewer keeps this perpetually fresh.
const SUBSCRIBER_GRACE_TICKS: u32 = 3;

/// Collect only when a subscriber read `latest_metrics()` within `grace`. `None`
/// (never read → headless daemon) ⇒ skip. Pure, for deterministic testing.
fn should_collect(last_read: Option<Instant>, now: Instant, grace: Duration) -> bool {
    match last_read {
        Some(t) => now.duration_since(t) <= grace,
        None => false,
    }
}

/// Read the latest metrics snapshot. Returns empty vec if no collection has run.
/// Records the read so the collection tick knows a subscriber is watching (see
/// `should_collect`); the Monitor view's "waiting for first collection" placeholder
/// covers the ≤1-tick bootstrap window after the first view.
pub fn latest_metrics() -> Vec<InstanceMetrics> {
    *LAST_METRICS_READ.lock() = Some(Instant::now());
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
        .spawn(move || {
            use sysinfo::{ProcessRefreshKind, RefreshKind, System};
            // R5: own ONE System for the thread's lifetime and refresh it in
            // place each tick, instead of allocating a fresh process table every
            // 5s. Bonus: reusing the System gives accurate CPU% (sysinfo derives
            // it from the delta between consecutive refreshes).
            let mut sys = System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
            );
            loop {
                std::thread::sleep(MONITOR_TICK);
                collect(&home, &registry, &mut sys);
            }
        });
}

/// One agent's registry-side snapshot, captured under a single registry lock.
struct AgentSnap {
    name: String,
    pid: Option<u32>,
    agent_state: String,
    health_state: String,
}

/// Collect metrics for all agents in the registry. Called from the monitor tick,
/// which owns the persistent `sys` (refreshed in place each call — see R5).
pub fn collect(
    home: &std::path::Path,
    registry: &crate::agent::AgentRegistry,
    sys: &mut sysinfo::System,
) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};

    // R5 ⑤ subscriber gate: skip the whole sweep (the sysinfo refresh below is
    // the dominant cost) when nobody has read `latest_metrics()` recently — a
    // headless daemon, or a TUI not on the Monitor tab, has no consumer to serve.
    if !should_collect(
        *LAST_METRICS_READ.lock(),
        Instant::now(),
        MONITOR_TICK * SUBSCRIBER_GRACE_TICKS,
    ) {
        return;
    }

    // R5 ③: refresh the persistent System in place (no per-tick allocation).
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let now = Instant::now();
    // R5 ④: one registry lock captures everything — id is no longer needed since
    // state/health are read here too (was a per-agent re-lock). #1441: registry
    // is UUID-keyed; name carries through for metadata / metrics / sort.
    let snaps: Vec<AgentSnap> = {
        let reg = crate::agent::lock_registry(registry);
        reg.values()
            .map(|handle| {
                let pid = handle.child.lock().process_id();
                let core = handle.core.lock();
                AgentSnap {
                    name: handle.name.to_string(),
                    pid,
                    agent_state: core.state.get_state().display_name().to_string(),
                    health_state: core.health.state.display_name().to_string(),
                }
            })
            .collect()
    };

    // R5 ①②: build the process memory table + parent→children index ONCE per
    // collect (was rebuilt per agent inside `tree_rss`). Each agent's tree-RSS is
    // then O(descendants) via the index, not O(all machine processes) per agent.
    let (mem, children) = build_proc_index(sys);

    let mut metrics = Vec::with_capacity(snaps.len());
    for snap in snaps {
        let (rss_bytes, cpu_percent, uptime_secs) = if let Some(p) = snap.pid {
            let spid = sysinfo::Pid::from_u32(p);
            if let Some(proc_info) = sys.process(spid) {
                let total_rss = sum_tree_rss(p, &mem, &children);
                (
                    Some(total_rss),
                    Some(proc_info.cpu_usage()),
                    Some(proc_info.run_time()),
                )
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        let (heartbeat_lag_secs, pending_pickup_count) = read_metadata_metrics(home, &snap.name);

        metrics.push(InstanceMetrics {
            name: snap.name,
            pid: snap.pid,
            rss_bytes,
            cpu_percent,
            uptime_secs,
            agent_state: snap.agent_state,
            health_state: snap.health_state,
            heartbeat_lag_secs,
            pending_pickup_count,
            collected_at: now,
        });
    }

    metrics.sort_by(|a, b| a.name.cmp(&b.name));
    *cache().lock() = metrics;
}

/// Process memory table (pid → RSS bytes).
type ProcMem = std::collections::HashMap<u32, u64>;
/// Parent pid → its direct child pids.
type ChildIndex = std::collections::HashMap<u32, Vec<u32>>;

/// Build the process memory table + parent→children index ONCE per collect from a
/// refreshed `System`. The children index turns each agent's descendant walk into
/// O(descendants) instead of the old O(all machine processes) full-table scan per
/// popped pid (which `tree_rss` also paid to rebuild once per agent).
fn build_proc_index(sys: &sysinfo::System) -> (ProcMem, ChildIndex) {
    let procs = sys.processes();
    let mut mem: ProcMem = std::collections::HashMap::with_capacity(procs.len());
    let mut children: ChildIndex = std::collections::HashMap::new();
    for p in procs.values() {
        let pid = p.pid().as_u32();
        mem.insert(pid, p.memory());
        if let Some(parent) = p.parent() {
            children.entry(parent.as_u32()).or_default().push(pid);
        }
    }
    (mem, children)
}

/// Sum RSS for `root` and all its descendants, cycle-safe and iterative, walking
/// the prebuilt `children` index (O(descendants), no full-table scan).
///
/// Windows reuses PIDs aggressively, so the parent-PID graph can contain a
/// cycle (A's parent is B, B's parent is A). The previous recursive walk would
/// then recurse forever and overflow the stack (monitor_tick thread crash,
/// 2026-06-12 15:13, ~18k frames). The `visited` set breaks cycles and also
/// guarantees each process is counted at most once.
fn sum_tree_rss(root: u32, mem: &ProcMem, children: &ChildIndex) -> u64 {
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![root];
    let mut total = 0u64;
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue; // already counted / cycle guard
        }
        if let Some(&m) = mem.get(&pid) {
            total = total.saturating_add(m);
        }
        if let Some(kids) = children.get(&pid) {
            for &cpid in kids {
                if !visited.contains(&cpid) {
                    stack.push(cpid);
                }
            }
        }
    }
    total
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
    fn latest_metrics_does_not_panic_before_collection() {
        // `latest_metrics()` reads a process-global cache, so its emptiness is
        // test-ordering dependent and can't be asserted (the previous name
        // `latest_metrics_empty_on_startup` over-promised). What IS guaranteed:
        // reading the cache before/without any collection must not panic and
        // must return a well-formed Vec.
        let m = latest_metrics();
        // Touch the result so the call can't be optimized away; any panic in
        // latest_metrics() fails the test.
        assert!(m.len() <= m.capacity());
    }

    /// Build a (mem, children) index from `(pid, mem, parent)` rows — mirrors
    /// `build_proc_index` so the walk tests exercise the production index shape.
    fn index(rows: &[(u32, u64, Option<u32>)]) -> (ProcMem, ChildIndex) {
        let mut mem = ProcMem::new();
        let mut children = ChildIndex::new();
        for &(pid, m, parent) in rows {
            mem.insert(pid, m);
            if let Some(par) = parent {
                children.entry(par).or_default().push(pid);
            }
        }
        (mem, children)
    }

    #[test]
    fn sum_tree_rss_sums_root_and_descendants() {
        let (mem, children) = index(&[
            (1, 10, None),
            (2, 20, Some(1)),
            (3, 30, Some(2)),
            (9, 99, None), // unrelated tree, must not be counted
        ]);
        assert_eq!(sum_tree_rss(1, &mem, &children), 60);
    }

    #[test]
    fn sum_tree_rss_survives_parent_cycle() {
        // Windows PID reuse: A's parent is B, B's parent is A. The old recursive
        // walk overflowed the stack here; this must terminate and count each once.
        let (mem, children) = index(&[(1, 100, Some(2)), (2, 200, Some(1))]);
        assert_eq!(sum_tree_rss(1, &mem, &children), 300);
    }

    #[test]
    fn sum_tree_rss_survives_self_parent() {
        let (mem, children) = index(&[(1, 50, Some(1))]);
        assert_eq!(sum_tree_rss(1, &mem, &children), 50);
    }

    /// R5 ①②: the parent→children index scopes each agent's walk to its OWN
    /// subtree. 50 agent trees embedded among 10_000 unrelated processes: each
    /// root's tree-RSS must equal only its subtree, regardless of the machine's
    /// total process count (proves no full-table-scan pollution).
    #[test]
    fn sum_tree_rss_isolates_subtree_among_many_unrelated_procs() {
        let mut rows: Vec<(u32, u64, Option<u32>)> = Vec::new();
        for pid in 100_000u32..110_000 {
            rows.push((pid, 7, None)); // unrelated, unlinked to any agent root
        }
        let roots: Vec<u32> = (0..50u32).map(|i| i * 10 + 1).collect();
        for &r in &roots {
            rows.push((r, 100, None)); // root
            rows.push((r + 1, 200, Some(r))); // child
            rows.push((r + 2, 300, Some(r + 1))); // grandchild → 600 total
        }
        let (mem, children) = index(&rows);
        for &r in &roots {
            assert_eq!(
                sum_tree_rss(r, &mem, &children),
                600,
                "root {r}: must sum its own subtree only, not the 10k unrelated procs"
            );
        }
    }

    /// R5 ①②③: the per-agent term is O(descendants), independent of the machine's
    /// process count. Summing 50 agent subtrees over a 10_000-process table is
    /// bounded work — a generous ceiling (not a tight micro-benchmark) that the old
    /// O(agents×procs) full-table-scan-per-pid would blow past. Prints the measured
    /// per-agent term so the "microsecond-level" claim is visible.
    #[test]
    fn per_agent_sum_is_bounded_independent_of_proc_count() {
        let mut rows: Vec<(u32, u64, Option<u32>)> = Vec::new();
        for pid in 100_000u32..110_000 {
            rows.push((pid, 7, None));
        }
        let roots: Vec<u32> = (0..50u32).map(|i| i * 10 + 1).collect();
        for &r in &roots {
            rows.push((r, 100, None));
            rows.push((r + 1, 200, Some(r)));
        }
        let (mem, children) = index(&rows); // built ONCE, outside the timed region
        let start = Instant::now();
        let mut sink = 0u64;
        for &r in &roots {
            sink = sink.wrapping_add(sum_tree_rss(r, &mem, &children));
        }
        let elapsed = start.elapsed();
        assert_eq!(
            sink,
            300 * roots.len() as u64,
            "sanity: each tree sums to 300"
        );
        let per_agent_us = elapsed.as_micros() as f64 / roots.len() as f64;
        eprintln!("R5 per-agent tree-RSS term: {per_agent_us:.3}µs over a 10k-proc table");
        assert!(
            elapsed < Duration::from_millis(200),
            "50 agent sums over a 10k-proc table must be fast (O(descendants), not \
             O(agents×procs)); got {elapsed:?}"
        );
    }

    /// R5 ⑤: collection gates on a recent subscriber read. Deterministic via
    /// constructed Instants (no wall-clock dependence).
    #[test]
    fn should_collect_gates_on_recent_subscriber_read() {
        let grace = Duration::from_secs(15);
        let t0 = Instant::now();
        assert!(
            !should_collect(None, t0, grace),
            "never read (headless) ⇒ skip"
        );
        assert!(should_collect(Some(t0), t0, grace), "just read ⇒ collect");
        assert!(
            should_collect(Some(t0), t0 + Duration::from_secs(10), grace),
            "within grace ⇒ collect"
        );
        assert!(
            !should_collect(Some(t0), t0 + Duration::from_secs(16), grace),
            "past grace ⇒ skip (subscriber went away)"
        );
    }

    /// R5 ⑤: reading `latest_metrics()` records the subscriber read that keeps
    /// collection warm (bootstraps a freshly-opened Monitor view on the next tick).
    #[test]
    fn latest_metrics_records_subscriber_read() {
        let _ = latest_metrics();
        assert!(
            LAST_METRICS_READ.lock().is_some(),
            "latest_metrics() must record a subscriber read"
        );
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
