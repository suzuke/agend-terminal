# SPIKE #2737 — Typed / Call-Level Owner-Service Invariants — Decision Manifest

- **Task:** t-20260712023025086467-46182-2 (branch `spike/2737-typed-owner-invariants`)
- **Source of truth:** decision `d-20260712023005493110-0` ("#2737 source-scan/masker guards remain temporary until a spike replaces them with typed construction or real call-level runtime invariants; do not invest further in handwritten Rust masking") + PR #2737 POST-MERGE audit (`gapfix-reviewer7`).
- **Freshness:** origin/main @ `c4206950619e748c4e641f6076c762e0ee88d916` (= my worktree HEAD; guards live @ merge `6f90572d`).
- **Charter:** analysis only. No production code. Deliver guard inventory + non-vacuous invariant design + migration/deletion slices + KISS comparison + RED tests.

---

## 0. Recommendation (TL;DR)

**Adopt Approach 2 — an injectable owner-service seam — as the primary replacement**, because it is the smallest design that gives *real call-level* coverage of composition + attached-exclusion **without starting threads**, and it lets the entire ~295-LOC handwritten masker + 3 static guards + 3 masker-scan pins be **deleted**.

- I1 (composition = exactly the 5 services), I3 (attached excludes), I4 (no host-local copy) → covered **behaviorally** by driving the real seam with recording fake starters.
- I2 (both hosts reach the seam) → close cheaply with a `#[must_use]` witness the daemon host's return tuple already carries (compile-forced for run_core) + one reviewer-verified call in `run_app`. **Full type-state (Approach 1, PreparedOwner→RunningOwner) is NOT recommended now** — it forces restructuring run_app's ~900-line body (`src/app/mod.rs:405`→`1304`) for a *reversible Stage 1a*, exceeding KISS.
- **Approach 3 (syn AST scan) is rejected as the primary**: it is a lateral move — a better *matcher* for the same source-scan class the decision explicitly wants to leave ("do not invest further in handwritten Rust masking"), and it still delivers **zero runtime/behavioral coverage**.

Net: delete ~450 LOC of test scaffolding, add ~40 LOC prod seam + ~70 LOC behavioral tests.

---

## 1. Exact current guard inventory (@ HEAD c4206950)

### 1a. Production wiring under guard
- `src/daemon/owner_services.rs:26` `start_shared_monitoring_services(home,registry)` → `instance_monitor::spawn_monitor_tick` (:27) + `api_activity_probe::spawn` (:31)
- `src/daemon/owner_services.rs:39` `start_shared_stream_observers(home,registry)` → `shadow::rollout::spawn` (:42) + `shadow::opencode::spawn` (:44) + `shadow::kiro::spawn` (:46)
- **App host** `run_app` (`src/app/mod.rs:405`): both helpers called at :469 / :487, inside `if !attached_mode {` (:463).
- **Daemon host** `build_tick_infrastructure` (`src/daemon/mod.rs:1232`): both helpers called at :1250 / :1255 (always owner; no attached concept).
- Each inner `spawn*` starts a **real `std::thread::Builder`** fire-and-forget thread returning `()` (`instance_monitor.rs:66`, `api_activity_probe.rs:76`, `shadow/{rollout:238,opencode:372,kiro:155}`). ← this is *why* helpers can't run in unit tests today.

### 1b. Guards / pins (all in `src/app/mod.rs`, `mod tests`)
| # | test fn | line | mechanism | invariant |
|---|---|---|---|---|
| G1 | `owner_services_called_by_both_hosts` | 2850 | masker via `owner_wiring_prod`, `contains(helper)` on both hosts | **I2** dual-host reach |
| G2 | `owner_services_spawns_absent_from_hosts_present_at_wiring_site` | 2866 | masker, `!contains` in hosts + `contains` in owner_services | **I4** no host copy + **I1** presence |
| G3 | `owner_services_calls_inside_attached_mode_guard` | 2890 | masker + brace-matching on `if !attached_mode {` block | **I3** attached exclusion |
| P1 | `run_app_wires_shadow_socket_server_2413` | 2386 | **raw** `contains("crate::daemon::shadow::start(&home)")`, NO masker | shadow::start (EXCLUDED/host-local) present in app prod |
| P2 | `run_app_wires_codex_rollout_tailer_2413` | 2412 | masker, `contains(rollout::spawn()` in owner_services | rollout present |
| P3 | `run_app_wires_opencode_sse_observer_2413` | 2442 | masker | opencode present |
| P4 | `run_app_wires_kiro_session_tailer_2413` | 2794 | masker | kiro present |

### 1c. Masker machinery (handwritten Rust — the debt the decision names)
- `strip_rust_comments` :2474 (~131 LOC, string/char/raw-string-aware)
- `blank_string_contents` :2605 (~101 LOC)
- `strip_comments_and_blank_strings` :2711 (composition)
- self-tests `strip_comments_and_blank_strings_masks_string_and_comment_needles` :2719, `strip_rust_comments_is_string_literal_aware` :2747 (~60 LOC)
- helpers/consts `owner_wiring_prod` :2837, `OWNER_HELPERS` :2820, `OWNER_MOVED_SPAWNS` :2824

> ⚠️ **Naming drift:** PR #2737's *description* named the guards `both_hosts_call_the_two_shared_helpers` / `moved_spawns_live_only_in_owner_services` / `app_helper_calls_are_inside_the_attached_mode_owner_guard`. The **merged code** uses the `owner_services_*` names in the table above. Impl must target the merged names.

---

## 2. Invariants the guards protect (the real spec)

- **I1 Composition completeness** — the owner-service set is exactly {monitor_tick, api_activity_probe, rollout, opencode, kiro}; a *new* owner service is wired once and reaches both hosts.
- **I2 Dual-host reach** — BOTH `run_app` (owned TUI, = the live fleet daemon) and `build_tick_infrastructure` (headless run_core) reach that composition. (Root bug class #982/#1002/#1720/#2434: wired in one host, silently dead in the other — the live daemon is app mode, so run_core-only wiring is dead in prod.)
- **I3 Attached exclusion** — an attached TUI (another daemon owns the fleet) starts NONE of them.
- **I4 No host-local copy** — neither host body re-spawns a service directly (drift reintroduction).

---

## 3. Three approaches compared

Coverage (✅ by-construction/behavioral · ⚠️ partial/last-mile · ❌ none):

| | I1 comp | I2 dual-host | I3 attached | I4 no-copy | no threads | deletes masker | runtime/behavioral? | KISS cost |
|---|---|---|---|---|---|---|---|---|
| **A1 typed (PreparedOwner→RunningOwner)** | ❌ (type ≠ which svcs) | ✅✅ compile | ⚠️ (type ≠ gate logic) | ✅ (only ctor path) | ✅ | ✅ | partial | **high** — restructure run_app body + run_core |
| **A2 injectable seam (fake starters)** | ✅ set-equality test | ⚠️ last-mile (1 call/host) | ✅ real gate path tested | ✅ real spawns only in `RealStarters` | ✅ | ✅ | **yes** | **low** — ~40 LOC prod seam, matches existing `dyn Fn` seam convention |
| **A3 syn AST scan** | ✅ path-resolved | ✅ AST both hosts | ✅ AST gate | ✅ | ✅ | ✅ (replaces w/ AST) | ❌ still source-scan | med — adds `syn` walk test code; **same class the decision leaves** |

**Why A2 primary, not A1:** A1's strength is *only* I2, and it does NOT by itself cover I1 or I3 (a type witness proves "startup happened", not "which 5 services / attached-gated correctly") — so A1 still needs A2's behavioral test underneath. A1's full form threads owner-lifecycle types through both hosts' bodies: invasive against run_app's ~900-line body for a step billed reversible.

**Why not A3:** the decision names the target as "typed construction OR real call-level runtime invariants." AST scanning is neither — it hardens the matcher but keeps the "no runtime coverage" debt the audit flagged, and adds a fresh chunk of parser-walking test code right after we delete one. A `syn` pass is the right tool if we ever must keep a source-scan, but here we can delete the scan entirely.

**I2 residual after A2 (honest):** with a single seam call per host, the I2 failure mode degrades from *silent single-service drift* (dangerous) to *host omits the one owner-setup call → that host has NO monitoring AND NO observers at all* (loud, immediately visible). Closed further at near-zero cost by the witness in §4.

---

## 4. Recommended design (A2 + light witness)

### 4a. Seam (in `src/daemon/owner_services.rs`, replaces the two helpers)
```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerRole { Owned, Attached }

/// Injected starters — production spawns real threads; tests record labels.
/// Matches the repo's existing `dyn Fn` seam convention (api/handlers/external.rs,
/// ci_watch/provider.rs). Boxed closures, not a trait, to stay minimal.
pub(crate) struct OwnerServiceStarters<'a> {
    pub monitor_tick:      &'a dyn Fn(&Path, &AgentRegistry),
    pub api_activity_probe:&'a dyn Fn(&AgentRegistry),
    pub rollout:           &'a dyn Fn(&Path, &AgentRegistry),
    pub opencode:          &'a dyn Fn(&Path, &AgentRegistry),
    pub kiro:              &'a dyn Fn(&Path, &AgentRegistry),
}
impl OwnerServiceStarters<'static> { pub(crate) fn real() -> Self { /* forwards to the 5 real spawns */ } }

/// Witness that owner-service startup was decided. `#[must_use]` so a host that
/// forgets to bind it warns; carried in build_tick_infrastructure's return tuple
/// makes the daemon host compile-forced.
#[must_use]
pub(crate) struct OwnerServicesStarted(());

pub(crate) fn start_owner_services(
    role: OwnerRole, home: &Path, reg: &AgentRegistry, s: &OwnerServiceStarters<'_>,
) -> OwnerServicesStarted {
    if role == OwnerRole::Owned {
        (s.monitor_tick)(home, reg);
        (s.api_activity_probe)(reg);
        (s.rollout)(home, reg);
        (s.opencode)(home, reg);
        (s.kiro)(home, reg);
    }
    OwnerServicesStarted(())
}
```

### 4b. Host call sites (behavior-preserving)
- **app** `run_app` — call **unconditionally** with role derived from `attached_mode` (moves the exclusion decision INTO the tested seam; the `if !attached_mode {` block keeps supervisor/`shadow::start`/telegram):
  `let _owner = start_owner_services(if attached_mode { Attached } else { Owned }, &home, &registry, &OwnerServiceStarters::real());`
- **daemon** `build_tick_infrastructure` — `start_owner_services(Owned, home, &ctx.registry, &OwnerServiceStarters::real())`; add its `OwnerServicesStarted` to the returned tuple (`TickKeepalive` is a natural home) → **compile-forced** for run_core.

This makes **I3 by-construction** (production gate = the code the test drives) rather than "source-scan says the call is textually inside the guard."

---

## 5. Migration slices (ordered, each reversible, test-first)

1. **S1 (seam, no behavior change):** add `OwnerRole`, `OwnerServiceStarters`, `start_owner_services`, `OwnerServicesStarted` to `owner_services.rs`; keep the two old helpers delegating to `OwnerServiceStarters::real()` temporarily so the existing guards stay green. Land the new RED behavioral tests (§7) here — they pass on the real seam.
2. **S2 (rewire hosts):** switch `run_app` (:469/:487) and `build_tick_infrastructure` (:1250/:1255) to the single seam call; thread the witness into the daemon tuple. Old static guards (G1–G3, P2–P4) still pass (helpers still present via delegation).
3. **S3 (delete masker + scans):** remove the two old helpers, G1–G3, P2–P4, and the entire masker block (§6). P1 (`shadow::start`, raw scan, EXCLUDED service) is out of core scope — see §8.
4. Each slice is independently revertable; behavior identical throughout (same 5 spawns, same order, same attached gate).

---

## 6. Deletion slices (what S3 removes, all `src/app/mod.rs`)
- Masker: `strip_rust_comments` (~131) + `blank_string_contents` (~101) + `strip_comments_and_blank_strings` (3) + 2 self-tests (~60) ≈ **295 LOC**
- Static guards G1–G3 (~70) + `owner_wiring_prod` (~8) + `OWNER_HELPERS`/`OWNER_MOVED_SPAWNS` (~11)
- Scan pins P2/P3/P4 (~57)
- Old helpers `start_shared_*` in owner_services.rs (~22)
- **Total ≈ 450 LOC deleted**, replaced by ~40 LOC prod + ~70 LOC behavioral tests. Masker becomes dead on last usage removed (confirmed: 30/30 masker usages are these guards — zero other consumers).

---

## 7. RED tests (proposed; land in S1, drive the seam with fakes — NO threads)

```rust
// Recording fake: pushes a label instead of spawning. Zero threads.
fn recording() -> (RefCell<Vec<&'static str>>, /* starters built from it */) { ... }

#[test] // I1: owner mode starts EXACTLY the five, once each (set-equality → adding a
        // 6th prod service without updating this test goes RED)
fn owned_role_starts_exactly_the_five_owner_services() {
    let rec = RefCell::new(vec![]);
    let s = recording_starters(&rec);   // 5 closures each push their label
    start_owner_services(OwnerRole::Owned, tmp(), &empty_registry(), &s);
    assert_eq!(sorted(rec), ["api_activity_probe","kiro","monitor_tick","opencode","rollout"]);
}

#[test] // I3: attached role starts NONE
fn attached_role_starts_no_owner_services() {
    let rec = RefCell::new(vec![]);
    start_owner_services(OwnerRole::Attached, tmp(), &empty_registry(), &recording_starters(&rec));
    assert!(rec.borrow().is_empty());
}

#[test] // I2 (daemon, by-construction): compile-enforced — build_tick_infrastructure's
        // return type carries OwnerServicesStarted. This test asserts the tuple field
        // exists / is produced (a run_core that skips the seam fails to compile).
fn build_tick_infrastructure_yields_owner_services_witness() { /* type-level: destructure the witness */ }
```

**Non-vacuity / reverse-mutation (for reviewer RED→GREEN, §3.20 SOP3):**
- Delete one closure call from `start_owner_services` → `owned_role_starts_exactly_the_five…` RED.
- Flip the gate to run under `Attached` → `attached_role_starts_no_owner_services` RED.
- Add a 6th real service to `OwnerServiceStarters::real()` but not the expected set → set-equality RED (forces the wiring to be acknowledged — the I1 completeness ratchet).
- Remove the witness from the daemon tuple → daemon host fails to compile.

Because the tests drive the **real** `start_owner_services` (the same fn production calls), they are production-path-coupled (§3.9): no helper-mimic, no mid-pipeline inject.

---

## 8. Open forks / decisions for orchestrator (codex-125550)

1. **I2 for `run_app`:** accept the `#[must_use]` witness + reviewer-verified single call (KISS, recommended), OR invest in threading the witness into run_app's teardown to make it compile-forced too? (I recommend the former for a reversible stage.)
2. **P1 `run_app_wires_shadow_socket_server_2413` (shadow::start):** it is an app::tests **static string scan** (so nominally in the charter's "replace app::tests static string scans") BUT targets `shadow::start`, an **EXCLUDED host-local** service (per d-20260711201257672833-2, separate fork). Options: (a) leave P1 as-is (out of scope — recommended, it's raw `contains`, not masker), (b) fold shadow::start into a host-local injectable seam in a later stage. Not core to "delete the masker."
3. **Approach A1 (full type-state):** confirmed *deferred*, not rejected forever — revisit if #2453 later unifies the host bodies (then PreparedOwner→RunningOwner becomes cheap and gives full compile-time I2 for both hosts).

---

## 9. Evidence
- `git rev-parse HEAD` → `c4206950…` (matches freshness boundary).
- `git grep` masker usages → 30/30 in `src/app/mod.rs`, all the guards above; zero other consumers (deletable).
- Inner spawns start real `std::thread::Builder` threads: `instance_monitor.rs:66`, `api_activity_probe.rs:76`, `shadow/rollout.rs:238`, `shadow/opencode.rs:372`, `shadow/kiro.rs:155`.
- DI seam prior art (justifies closures-not-trait): `src/api/handlers/external.rs:18`, `src/daemon/ci_watch/provider.rs:117`, `src/daemon/event_bus.rs:229`.
- Call sites cited: app `src/app/mod.rs:463/469/487`; daemon `src/daemon/mod.rs:1232/1250/1255`.
