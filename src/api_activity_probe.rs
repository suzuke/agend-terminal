//! #2413 Phase 1 — out-of-path API-activity probe.
//!
//! Answers "is this agent actually mid-LLM-call, or genuinely idle?" by reading
//! the kernel socket table (`lsof`) once per tick — it **never touches** the
//! agent↔LLM connection (no proxy, no decrypt, zero in-path risk). The signal
//! feeds `AgentCore::api_activity`, which `list_instances` surfaces so an
//! operator (or the daemon) can spot a *false-idle*: pattern-state says `idle`
//! but a live LLM socket proves the agent is mid-turn.
//!
//! ## Why a socket probe works (empirically, 2026-06-23, this fleet)
//! An idle managed agent holds **zero** ESTABLISHED `:443` sockets (verified
//! claude / codex / agy, whole PID-tree). A turn opens a socket to the LLM at
//! turn-start, holds it through the turn, closes on idle. So socket-to-LLM
//! present ⟺ mid-turn.
//!
//! ## Precise vs graceful (the LLM-IP table)
//! `lsof -nP` yields raw IPs, not hostnames (reverse-DNS fails — Anthropic /
//! OpenAI are CDN-fronted, PTR = NXDOMAIN). So we forward-resolve a small table
//! of LLM hostnames → IP set and match by IP (precise: ignores npm / git-https /
//! telemetry). When DNS is stale/unavailable the match **degrades** to "any
//! public `:443`" (raw-:443 fallback) — never worse than the simple signal.
//!
//! ## Known limitation (accepted, #2413)
//! When an LLM endpoint shares a CDN IP range with non-LLM telemetry (e.g.
//! Statsig/Sentry over the same Cloudflare block), pure IP-match cannot perfectly
//! separate them. For false-idle this errs **toward "active"** — the safe
//! direction (we never wrongly call a busy agent idle).
//!
//! ## Platform
//! macOS/Linux (operator runs darwin). Windows / any host without `lsof` →
//! the probe thread no-ops once and exits; `api_activity` stays default (`None`/
//! `false`), the daemon is never broken (best-effort + graceful).

use crate::agent::{AgentCore, AgentRegistry};
use crate::sync_audit::CoreMutex;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// Probe cadence. Aligned with the instance-monitor tick (5 s). LLM turns
/// typically run >5 s, so a 5 s probe catches them; a brief sub-tick turn that
/// is missed is, by definition, not a stall — false-idle is about *long* idles.
const PROBE_TICK: Duration = Duration::from_secs(5);

/// Re-resolve the LLM-IP table every N ticks (60 s). CDN IPs rotate slowly; a
/// minute of staleness is harmless (a stale-but-nonempty table stays precise on
/// the IPs it knows, and any miss just degrades that one socket to the public
/// `:443` rule, not to wrong-attribution).
const REFRESH_TICKS: u64 = 12;

/// Timeout for one `lsof`/`ps` spawn. Generous (a batched `lsof` measures
/// ~0.07 s) — the bound only guards the rare `lsof`-wedged-on-a-stale-mount
/// failure mode so the probe thread can never hang the process.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// LLM API endpoints, by backend (empirically observed remote hosts, #2413 spike).
/// Everything NOT on this list (npm / github / datadog telemetry / auth) is noise.
const LLM_HOSTS: &[&str] = &[
    "api.anthropic.com",                 // claude
    "chatgpt.com",                       // codex (observed)
    "api.openai.com",                    // codex / openai
    "cloudcode-pa.googleapis.com",       // agy
    "daily-cloudcode-pa.googleapis.com", // agy (observed)
];

/// One ESTABLISHED outbound socket: the owning PID and its remote endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Socket {
    pid: u32,
    remote_ip: IpAddr,
    remote_port: u16,
}

/// Spawn the daemon-owned probe thread. Fire-and-forget.
pub fn spawn(registry: AgentRegistry) {
    // fire-and-forget: the probe loop is a read-only sampler that terminates on
    // process exit. Losing the thread on shutdown is harmless (next boot
    // re-samples); it owns no state another thread must join. (§10.5)
    let _ = std::thread::Builder::new()
        .name("api_activity_probe".into())
        .spawn(move || {
            if !tool_available("lsof") {
                tracing::info!(
                    "api_activity_probe: `lsof` not found — out-of-path API-activity \
                     probe disabled (api_activity stays None/false). Daemon unaffected."
                );
                return;
            }
            let mut llm_ips: HashSet<IpAddr> = HashSet::new();
            let mut tick: u64 = 0;
            loop {
                if tick.is_multiple_of(REFRESH_TICKS) {
                    let fresh = resolve_llm_ips(LLM_HOSTS);
                    // Keep the last good table on a failed/empty refresh — only an
                    // empty table (never resolved) drops us to the raw-:443 rule.
                    if !fresh.is_empty() {
                        llm_ips = fresh;
                    }
                }
                probe_once(&registry, &llm_ips);
                tick = tick.wrapping_add(1);
                std::thread::sleep(PROBE_TICK);
            }
        });
}

/// One probe cycle: snapshot agent roots (brief registry lock) → run `lsof`/`ps`
/// OUT of the lock → attribute sockets to agents → write each agent's
/// `api_activity`. On a tool failure this cycle is skipped (no write), so a
/// transient `lsof` hiccup leaves the prior signal intact rather than flipping
/// every agent to "idle".
fn probe_once(registry: &AgentRegistry, llm_ips: &HashSet<IpAddr>) {
    // Snapshot (name, root_pid, core) under the registry lock, then release it
    // BEFORE the subprocess spawns — the lock must never be held across I/O.
    let agents: Vec<(String, Arc<CoreMutex<AgentCore>>)> = {
        let reg = crate::agent::lock_registry(registry);
        reg.values()
            .map(|h| (h.name.to_string(), Arc::clone(&h.core)))
            .collect()
    };
    if agents.is_empty() {
        return;
    }
    // root_pid → agent name. A PID read failure (process gone) just drops that
    // agent from attribution this tick.
    let roots: HashMap<u32, String> = {
        let reg = crate::agent::lock_registry(registry);
        reg.values()
            .filter_map(|h| {
                h.child
                    .lock()
                    .process_id()
                    .map(|pid| (pid, h.name.to_string()))
            })
            .collect()
    };

    let sockets = match probe_sockets() {
        Some(s) => s,
        None => return, // lsof failed/timed out — keep the prior signal.
    };
    let ppid = probe_ppid_map(); // empty on failure → only pid==root attributes.

    let active = active_agents(&roots, &ppid, &sockets, llm_ips);

    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    for (name, core) in &agents {
        let in_flight = active.contains(name.as_str());
        let mut c = core.lock();
        c.api_activity.in_flight = in_flight;
        if in_flight {
            c.api_activity.last_active_epoch_ms = Some(now_ms);
        }
    }
}

/// Run `lsof` and parse its ESTABLISHED-TCP sockets. `None` on spawn failure /
/// non-zero exit (graceful — caller skips the tick).
fn probe_sockets() -> Option<Vec<Socket>> {
    let mut cmd = std::process::Command::new("lsof");
    // -nP: no DNS / port-name resolution (fast, raw IPs). -i/-s: only ESTABLISHED
    // TCP. -Fpn: machine-readable field output — `p<pid>` then `n<addr>` lines.
    cmd.args(["-nP", "-iTCP", "-sTCP:ESTABLISHED", "-Fpn"]);
    let out = crate::git_helpers::spawn_group_bounded(cmd, "api_probe_lsof", SPAWN_TIMEOUT).ok()?;
    // lsof exits non-zero when *some* fds are unreadable even though it printed
    // usable output — so parse stdout regardless of status (don't gate on exit).
    Some(parse_lsof(&String::from_utf8_lossy(&out.stdout)))
}

/// Build a pid → ppid map from `ps`. Empty on failure (walk-up then only matches
/// a socket owned directly by an agent root PID).
fn probe_ppid_map() -> HashMap<u32, u32> {
    let mut cmd = std::process::Command::new("ps");
    cmd.args(["-axo", "pid=,ppid="]);
    match crate::git_helpers::spawn_group_bounded(cmd, "api_probe_ps", SPAWN_TIMEOUT) {
        Ok(out) => parse_ps_ppid(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => HashMap::new(),
    }
}

/// Is `tool` runnable? (`<tool> -v`/exit). Used once to gate the whole probe.
fn tool_available(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Forward-resolve the LLM hostnames to their current IP set via the system
/// resolver (no extra crate). Best-effort: hosts that fail to resolve are skipped.
fn resolve_llm_ips(hosts: &[&str]) -> HashSet<IpAddr> {
    let mut ips = HashSet::new();
    for host in hosts {
        if let Ok(addrs) = (*host, 443u16).to_socket_addrs() {
            for a in addrs {
                ips.insert(a.ip());
            }
        }
    }
    ips
}

// ── pure helpers (unit-tested) ──────────────────────────────────────────────

/// Parse `lsof -Fpn` output → sockets. Format: a `p<pid>` line opens a process,
/// then `f<fd>` (ignored) + `n<local>-><remote>` lines for its files.
fn parse_lsof(stdout: &str) -> Vec<Socket> {
    let mut out = Vec::new();
    let mut cur_pid: Option<u32> = None;
    for line in stdout.lines() {
        let Some((tag, rest)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => cur_pid = rest.trim().parse().ok(),
            "n" => {
                if let (Some(pid), Some((ip, port))) = (cur_pid, parse_name_remote(rest)) {
                    out.push(Socket {
                        pid,
                        remote_ip: ip,
                        remote_port: port,
                    });
                }
            }
            _ => {} // f<fd>, etc. — ignored
        }
    }
    out
}

/// Extract `(remote_ip, remote_port)` from an lsof `n` address `LOCAL->REMOTE`.
/// Only ESTABLISHED sockets (which carry `->`) yield a value.
fn parse_name_remote(name: &str) -> Option<(IpAddr, u16)> {
    let (_local, remote) = name.split_once("->")?;
    parse_endpoint(remote)
}

/// Parse `ip:port`, handling bracketed IPv6 (`[2001:db8::1]:443`) vs bare IPv4.
fn parse_endpoint(addr: &str) -> Option<(IpAddr, u16)> {
    let (ip_str, port_str) = if let Some(rest) = addr.strip_prefix('[') {
        rest.split_once("]:")? // "2001:db8::1", "443"
    } else {
        addr.rsplit_once(':')? // "1.2.3.4", "443"
    };
    Some((ip_str.parse().ok()?, port_str.parse().ok()?))
}

/// Parse `ps -axo pid=,ppid=` (whitespace-padded `<pid> <ppid>` per line).
fn parse_ps_ppid(stdout: &str) -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let mut it = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (it.next(), it.next()) {
            if let (Ok(pid), Ok(ppid)) = (pid.parse(), ppid.parse()) {
                map.insert(pid, ppid);
            }
        }
    }
    map
}

/// Set of agent names with an in-flight API socket. For each socket that counts
/// as API activity, walk its PID up the parent chain to the owning agent root.
fn active_agents(
    roots: &HashMap<u32, String>,
    ppid: &HashMap<u32, u32>,
    sockets: &[Socket],
    llm_ips: &HashSet<IpAddr>,
) -> HashSet<String> {
    let mut active = HashSet::new();
    for s in sockets {
        if !counts_as_api(s.remote_ip, s.remote_port, llm_ips) {
            continue;
        }
        if let Some(name) = owning_agent(s.pid, ppid, roots) {
            active.insert(name.to_string());
        }
    }
    active
}

/// Does this socket count as LLM API activity?
/// - precise (table non-empty): remote `:443` AND remote IP ∈ LLM-IP table.
/// - degraded (table empty — DNS failed): any remote `:443` to a public IP.
fn counts_as_api(remote: IpAddr, port: u16, llm_ips: &HashSet<IpAddr>) -> bool {
    if port != 443 {
        return false;
    }
    if llm_ips.is_empty() {
        is_public_remote(remote) // raw-:443 fallback
    } else {
        llm_ips.contains(&remote)
    }
}

/// Walk `pid` up the parent chain; return the first ancestor (incl. itself) that
/// is an agent root. Depth-capped (cycle / PID-reuse guard).
fn owning_agent<'a>(
    pid: u32,
    ppid: &HashMap<u32, u32>,
    roots: &'a HashMap<u32, String>,
) -> Option<&'a str> {
    let mut cur = pid;
    for _ in 0..64 {
        if let Some(name) = roots.get(&cur) {
            return Some(name.as_str());
        }
        match ppid.get(&cur) {
            Some(&p) if p != cur && p != 0 => cur = p,
            _ => return None,
        }
    }
    None
}

/// Public (internet-routable) remote? Excludes loopback / private / link-local /
/// ULA so the raw-:443 fallback never counts loopback IPC or LAN traffic.
fn is_public_remote(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            !a.is_loopback()
                && !a.is_private()
                && !a.is_link_local()
                && !a.is_unspecified()
                && !a.is_broadcast()
        }
        IpAddr::V6(a) => {
            let seg0 = a.segments()[0];
            let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
            let ula = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
            !a.is_loopback() && !a.is_unspecified() && !link_local && !ula
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().expect("test ipv4 literal"))
    }

    // Real-shaped `lsof -Fpn` output (see the live capture in the PR notes):
    // `p<pid>` opens a process; `f<fd>` delimits files; `n<local>-><remote>`.
    // pid 100 = a claude root (socket to api.anthropic.com + a Cloudflare
    // telemetry IP); pid 200 = a codex Rust child (socket to chatgpt.com), its
    // parent 150 is the codex agent root; pid 999 = unrelated noise on :443.
    const LSOF_FIXTURE: &str = "\
p100
f14
n192.168.1.5:54321->160.79.104.10:443
f15
n192.168.1.5:54322->104.16.9.34:443
p200
f33
n192.168.1.5:60001->104.18.32.47:443
p300
f9
n[fe80:e::1c12:5a10:30e:a686]:54592->[fe80:e::47a:ee1f]:50231
p999
f7
n192.168.1.5:51000->93.184.216.34:443
";

    #[test]
    fn parse_lsof_extracts_pid_and_remote_endpoints() {
        let socks = parse_lsof(LSOF_FIXTURE);
        // 100: two :443 sockets; 200: one; 300: one (link-local, non-443); 999: one.
        assert_eq!(socks.len(), 5);
        assert!(socks.contains(&Socket {
            pid: 100,
            remote_ip: v4("160.79.104.10"),
            remote_port: 443
        }));
        assert!(socks.contains(&Socket {
            pid: 200,
            remote_ip: v4("104.18.32.47"),
            remote_port: 443
        }));
        // IPv6 bracketed remote parsed; its port is the link-local 50231.
        assert!(socks.iter().any(|s| s.pid == 300
            && s.remote_port == 50231
            && matches!(s.remote_ip, IpAddr::V6(_))));
    }

    #[test]
    fn parse_endpoint_handles_ipv4_and_bracketed_ipv6() {
        assert_eq!(
            parse_endpoint("160.79.104.10:443"),
            Some((v4("160.79.104.10"), 443))
        );
        assert_eq!(
            parse_endpoint("[2001:db8::1]:443"),
            Some((
                IpAddr::V6(
                    "2001:db8::1"
                        .parse::<Ipv6Addr>()
                        .expect("test ipv6 literal")
                ),
                443
            ))
        );
        assert_eq!(parse_endpoint("garbage"), None);
    }

    #[test]
    fn parse_ps_ppid_parses_padded_pairs() {
        let map = parse_ps_ppid("    1     0\n  150     1\n  200   150\n  100     1\nbad line\n");
        assert_eq!(map.get(&200), Some(&150));
        assert_eq!(map.get(&150), Some(&1));
        assert_eq!(map.len(), 4); // "bad line" (single token) skipped
    }

    fn roots() -> HashMap<u32, String> {
        // claude root owns its socket directly (pid 100); codex root is the node
        // wrapper (150) whose Rust child (200) owns the socket.
        HashMap::from([(100, "claude".to_string()), (150, "codex".to_string())])
    }
    fn ppid() -> HashMap<u32, u32> {
        HashMap::from([(100, 1), (150, 1), (200, 150), (999, 1)])
    }

    #[test]
    fn precise_mode_matches_only_llm_ips_and_attributes_via_pid_tree() {
        // Table = the two LLM IPs in the fixture; the Cloudflare telemetry IP
        // (104.16.9.34) and the noise host (93.184.216.34) are NOT in it.
        let llm: HashSet<IpAddr> = HashSet::from([v4("160.79.104.10"), v4("104.18.32.47")]);
        let socks = parse_lsof(LSOF_FIXTURE);
        let active = active_agents(&roots(), &ppid(), &socks, &llm);
        // claude: direct LLM socket on its root. codex: LLM socket on child 200 →
        // walked up to root 150. Noise pid 999 (not an agent) ignored.
        assert!(active.contains("claude"));
        assert!(
            active.contains("codex"),
            "child socket must attribute to the root via ppid walk-up"
        );
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn precise_mode_ignores_non_llm_443() {
        // A claude root holding ONLY a Cloudflare-telemetry :443 socket (not in
        // the LLM table) is NOT flagged active — the whole point of precise mode.
        let llm: HashSet<IpAddr> = HashSet::from([v4("160.79.104.10")]);
        let socks = vec![Socket {
            pid: 100,
            remote_ip: v4("104.16.9.34"),
            remote_port: 443,
        }];
        let active = active_agents(&roots(), &ppid(), &socks, &llm);
        assert!(active.is_empty());
    }

    #[test]
    fn degraded_mode_counts_any_public_443_but_not_private_or_loopback() {
        let empty: HashSet<IpAddr> = HashSet::new(); // DNS failed → raw-:443 fallback
        let socks = vec![
            Socket {
                pid: 100,
                remote_ip: v4("93.184.216.34"),
                remote_port: 443,
            }, // public
            Socket {
                pid: 150,
                remote_ip: v4("127.0.0.1"),
                remote_port: 443,
            }, // loopback IPC
            Socket {
                pid: 150,
                remote_ip: v4("192.168.1.9"),
                remote_port: 443,
            }, // LAN
        ];
        let active = active_agents(&roots(), &ppid(), &socks, &empty);
        assert!(
            active.contains("claude"),
            "public :443 counts in degraded mode"
        );
        assert!(
            !active.contains("codex"),
            "loopback/LAN :443 must NOT count"
        );
    }

    #[test]
    fn non_443_never_counts() {
        let empty = HashSet::new();
        assert!(!counts_as_api(v4("160.79.104.10"), 80, &empty));
        assert!(!counts_as_api(v4("160.79.104.10"), 50231, &empty));
    }

    #[test]
    fn owning_agent_caps_depth_on_parent_cycle() {
        // PID-reuse cycle 5↔6 with no agent root above → must terminate (not hang).
        let cyclic = HashMap::from([(5u32, 6u32), (6u32, 5u32)]);
        let r = HashMap::from([(1u32, "a".to_string())]);
        assert_eq!(owning_agent(5, &cyclic, &r), None);
    }

    #[test]
    fn owning_agent_matches_self_and_ancestor() {
        let r = roots();
        let p = ppid();
        assert_eq!(owning_agent(100, &p, &r), Some("claude")); // self is a root
        assert_eq!(owning_agent(200, &p, &r), Some("codex")); // parent is a root
        assert_eq!(owning_agent(999, &p, &r), None); // unrelated
    }

    #[test]
    fn is_public_remote_classifies_ranges() {
        assert!(is_public_remote(v4("160.79.104.10")));
        assert!(!is_public_remote(v4("127.0.0.1")));
        assert!(!is_public_remote(v4("10.0.0.1")));
        assert!(!is_public_remote(v4("192.168.1.1")));
        assert!(!is_public_remote(IpAddr::V6(
            "fe80::1".parse::<Ipv6Addr>().expect("test ipv6 literal")
        )));
        assert!(!is_public_remote(IpAddr::V6(
            "::1".parse::<Ipv6Addr>().expect("test ipv6 literal")
        )));
        assert!(is_public_remote(IpAddr::V6(
            "2606:4700::1"
                .parse::<Ipv6Addr>()
                .expect("test ipv6 literal")
        )));
    }
}
