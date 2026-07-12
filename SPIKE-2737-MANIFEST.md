# SPIKE #2737 — Typed / Call-Level Owner-Service Invariants — Decision Manifest **v3 (FINAL)**

- **Task:** t-20260712023025086467-46182-2 (branch `spike/2737-typed-owner-invariants`)
- **Source of truth:** decision `d-20260712023005493110-0` + PR #2737 POST-MERGE audit + codex-125550 REJECT-v1 (m-…32940-20) + REJECT-v2 ordering (m-…37722-34).
- **Freshness:** origin/main @ `c4206950619e748c4e641f6076c762e0ee88d916` (= worktree HEAD; guards @ merge `6f90572d`).
- **Status:** design holds under source inspection → **READY FOR DECISION**. Analysis only, no code.
- **v3 changelog:** adopts codex's **two-phase typed** design. Seam NOT moved into `setup_app_bootstrap` — exact app/daemon order preserved. Monitoring-before-stream is compile-enforced by a `&OwnerMonitoringStarted` token. App I2 via `&OwnerServicesStarted` required at existing owned-only `app_maintenance_tick`. Permit + RED-first + P1-out + A1-deferred retained.

---

## 0. Recommendation (TL;DR)

Two typed phases (matching today's two helpers, same positions) + private-ctor permit + a witness threaded only from the seam to the existing owned-only tick consumer. Closes all invariants by construction / real call-level test, **preserves exact order**, deletes the ~295-LOC masker + 3 static guards + 3 scan pins.

| invariant | closed by | mechanism |
|---|---|---|
| I1 completeness (per phase, seam-routed) | call-level test | 2+3 required starter fields + `real()` + exact-ordered-set test |
| **phase order** (monitoring → stream) | **by construction + test** | phase 2 requires `&OwnerMonitoringStarted` (can't run stream first / omit monitoring — won't compile); recorded-order test as belt-and-suspenders |
| I2 dual-host reach | **by construction** | daemon: `OwnerServicesStarted` in `TickKeepalive`; app: `&OwnerServicesStarted` required by owned-only `app_maintenance_tick` |
| I3 attached exclusion | structural + test | production: calls stay inside the unchanged `if !attached_mode` wrapper; behavioral: seam `Attached` role starts none |
| I4 no host-local copy | **by construction** | `&OwnerServicePermit` (private ctor) at all 5 spawn entry points → host-body direct spawn fails to compile |

Not A1 full typestate (defers). Not A3 syn-AST (lateral, no runtime coverage).

---

## 1. Guard inventory (@ HEAD c4206950)

### 1a. Production wiring + EXACT ORDER (the contract codex is protecting)
- **App** `run_app` `if !attached_mode {` (`app/mod.rs:463`): `supervisor::spawn` :464 → `start_shared_monitoring_services` **:469** → `shadow::start(&home)` **:480** → `start_shared_stream_observers` **:487**.
- **Daemon** `build_tick_infrastructure` (`daemon/mod.rs:1232`): `TaskSweep`/`supervisor`/`router` :1240-1247 → `start_shared_monitoring_services` **:1250** → `start_shared_stream_observers` **:1255**.
- Helpers in `owner_services.rs:26/39`; 5 inner spawns each a real `std::thread::Builder` (`instance_monitor.rs:66`, `api_activity_probe.rs:76`, `shadow/{rollout:238,opencode:372,kiro:155}`).
- ⛔ A single merged `start_owner_services` would move monitoring later OR stream earlier, crossing `shadow::start` — **rejected**. Two phases keep the exact interleave.

### 1b. Guards / pins (all `app/mod.rs` `mod tests`)
| # | fn | line | invariant | v3 fate |
|---|---|---|---|---|
| G1 | `owner_services_called_by_both_hosts` | 2850 | I2 | delete → witnesses |
| G2 | `owner_services_spawns_absent_from_hosts_present_at_wiring_site` | 2866 | I4+I1 | delete → permit + set test |
| G3 | `owner_services_calls_inside_attached_mode_guard` | 2890 | I3 | delete → wrapper unchanged + Attached test |
| P1 | `run_app_wires_shadow_socket_server_2413` | 2386 | shadow::start (EXCLUDED) | **KEEP — out of scope** |
| P2/P3/P4 | rollout/opencode/kiro `_2413` | 2412/2442/2794 | presence | delete |

### 1c. Masker (deletable): `strip_rust_comments` :2474 (~130) · `blank_string_contents` :2605 (~100) · `strip_comments_and_blank_strings` :2711 · self-tests :2719/:2747 (~60) · `owner_wiring_prod` :2837 · `OWNER_HELPERS` :2820 · `OWNER_MOVED_SPAWNS` :2824.

> ⚠️ Naming drift: PR body names ≠ merged `owner_services_*` names; impl targets merged names.

---

## 2. Design (concrete, exact-order-preserving)

### 2a. Permit → structural I4 (unchanged from v2)
```rust
pub(crate) struct OwnerServicePermit(());   // private () field → ctor private to owner_services
```
5 spawn entry points gain `_permit: &OwnerServicePermit`. **Blast radius (git grep, verified):** sole callers are `owner_services.rs:27/31/42/44/46`; every same-file `.spawn(move||…)` is `thread::Builder`; app/mod.rs matches are masker string-literals → **5 sig + 5 call edits, ZERO test breakage**. `&`-borrow never enters the thread.

### 2b. Two typed phases (in `owner_services.rs`)
```rust
#[derive(Clone,Copy,PartialEq,Eq)] pub(crate) enum OwnerRole { Owned, Attached }
#[must_use] pub(crate) struct OwnerMonitoringStarted(());   // ctor private to seam
#[must_use] pub(crate) struct OwnerServicesStarted(());     // ctor private to seam

pub(crate) struct OwnerMonitoringStarters<'a> {             // 2 required fields
    pub monitor_tick:       &'a dyn Fn(&OwnerServicePermit,&Path,&AgentRegistry),
    pub api_activity_probe: &'a dyn Fn(&OwnerServicePermit,&AgentRegistry),
}
pub(crate) struct OwnerStreamStarters<'a> {                 // 3 required fields
    pub rollout:  &'a dyn Fn(&OwnerServicePermit,&Path,&AgentRegistry),
    pub opencode: &'a dyn Fn(&OwnerServicePermit,&Path,&AgentRegistry),
    pub kiro:     &'a dyn Fn(&OwnerServicePermit,&Path,&AgentRegistry),
}
impl OwnerMonitoringStarters<'static> { pub(crate) fn real() -> Self { /* → the 2 real spawns */ } }
impl OwnerStreamStarters<'static>     { pub(crate) fn real() -> Self { /* → the 3 real spawns */ } }

pub(crate) fn start_owner_monitoring(
    role: OwnerRole, home:&Path, reg:&AgentRegistry, s:&OwnerMonitoringStarters<'_>,
) -> OwnerMonitoringStarted {
    if role == OwnerRole::Owned { let p=OwnerServicePermit(());
        (s.monitor_tick)(&p,home,reg); (s.api_activity_probe)(&p,reg); }
    OwnerMonitoringStarted(())
}
pub(crate) fn start_owner_stream_observers(
    role: OwnerRole, _mon:&OwnerMonitoringStarted,           // ← forces monitoring-BEFORE-stream at compile time
    home:&Path, reg:&AgentRegistry, s:&OwnerStreamStarters<'_>,
) -> OwnerServicesStarted {
    if role == OwnerRole::Owned { let p=OwnerServicePermit(());
        (s.rollout)(&p,home,reg); (s.opencode)(&p,home,reg); (s.kiro)(&p,home,reg); }
    OwnerServicesStarted(())
}
```
- `dyn Fn` closures match repo seam convention (`api/handlers/external.rs:18`, `ci_watch/provider.rs:117`).
- **Phase order is compile-enforced**: you cannot call `start_owner_stream_observers` without an `OwnerMonitoringStarted`, which only `start_owner_monitoring` mints → can't swap phases or omit monitoring.

### 2c. Host rewire at EXACT positions
**App** (`run_app`, calls stay at :469/:487, `shadow::start` untouched at :480):
```rust
let owner_services: Option<OwnerServicesStarted> = if !attached_mode {
    crate::daemon::supervisor::spawn(...);                                          // :464 unchanged
    let mon = start_owner_monitoring(OwnerRole::Owned,&home,&registry,&OwnerMonitoringStarters::real()); // :469
    crate::daemon::shadow::start(&home);                                            // :480 unchanged (between phases)
    let svc = start_owner_stream_observers(OwnerRole::Owned,&mon,&home,&registry,&OwnerStreamStarters::real()); // :487
    /* telegram wiring … */
    Some(svc)
} else { None };
```
**App I2 compile-force** (no body threading): the existing owned-only `app_maintenance_tick` (`app/mod.rs:1365`, called `:1156` — owned-only: the tick arm's `tick_rx` is `None`→`never_rx` in attached, comment `:1148-1149`) gains a required `witness: &OwnerServicesStarted` param, passed `owner_services.as_ref().expect("owned tick ⇒ seam ran")`. Deleting the seam call ⇒ nothing can construct `OwnerServicesStarted` ⇒ `owner_services` can't be `Some(_)` ⇒ **build fails**. (Residual, honest: a hand-edit forcing `None` in owned mode panics on first owned tick — loud + test-covered, far tighter than the deleted source-scan. Optional: wrap in a tiny `AppOwnerKeepalive { owner_services }` held by the tick host.)

**Daemon** (`build_tick_infrastructure`, calls at :1250/:1255):
```rust
let mon = start_owner_monitoring(OwnerRole::Owned, home, &ctx.registry, &OwnerMonitoringStarters::real());   // :1250
let owner_services = start_owner_stream_observers(OwnerRole::Owned,&mon,home,&ctx.registry,&OwnerStreamStarters::real()); // :1255
...
(TickKeepalive { _task_sweep, _owner_services: owner_services }, handlers, tick_rx)   // :1291
```
`TickKeepalive` gains an `OwnerServicesStarted` field → can't be built without the seam → **run_core compile-forced** (held at `:849`).

### 2d. I3 attached exclusion
- **Production:** the two calls remain inside the **unchanged** `if !attached_mode` wrapper (real `if`, not a scan) → attached never reaches them. Order + gating both untouched.
- **Behavioral:** the `role` param lets the I3 test drive `Attached` → both phases start none. (Production hardcodes `Owned` inside the wrapper; `Attached` is the tested contract — G3's source-scan is thereby replaced by a real behavioral assertion + the unchanged structural `if`.)

---

## 3. I1 — precise claim (per codex REJECT-v1 pt.3)
Catches a **seam-routed** new service: a new starter field ⇒ every struct literal incl. `real()` fails to compile until populated; the exact-ordered-set test goes RED until updated. Does **NOT** catch an **off-seam** direct spawn — that is the permit's job (structural for the 5 current spawns; a one-line convention "new owner spawns take `&OwnerServicePermit`" for future ones + review). No overclaim.

---

## 4. Migration — RED-first, ONE PR, 3 commits (per codex pt.6 / §3.10)
- **C1 RED:** new behavioral tests (§5) + minimal **unwired** seam decls (enums/tokens/starter structs + both phase fns with **stub bodies that start nothing**). Owned-set test **FAILS** (stub → empty set). Hosts untouched, old guards green, build green. ← failing-test-before-impl.
- **C2 GREEN (rewire at EXACT positions):** real phase bodies; `&OwnerServicePermit` on the 5 spawns; `real()` starters; app rewire at :469/:487 + `app_maintenance_tick` witness param; daemon rewire at :1250/:1255 + `TickKeepalive` witness. Remove `start_shared_*` helpers + G1/G3 (coupled — rewire turns them RED). New tests green.
- **C3 CLEANUP (after green):** delete P2–P4 + G2 + entire masker + `owner_wiring_prod` + `OWNER_*` consts.

One PR (host rewire, old-guard removal, masker deletion are causally coupled — splitting lands a RED intermediate). **≈450 LOC out / ~110 in** + 5 one-arg sig edits. P1 stays.

---

## 5. RED tests (land C1; drive REAL phases with recording fakes — no threads; cover set + attached-none + phase order per codex pt.5)
```rust
fn rec_mon<'a>(log:&'a RefCell<Vec<&'static str>>) -> OwnerMonitoringStarters<'a> { /* 2 closures push labels, permit ignored */ }
fn rec_stream<'a>(log:&'a RefCell<Vec<&'static str>>) -> OwnerStreamStarters<'a>   { /* 3 closures push labels */ }

#[test] // I1 + phase ORDER: monitoring pair before stream trio, exact ordered list
fn owned_two_phases_start_all_five_in_order() {
    let log = RefCell::new(vec![]);
    let m = start_owner_monitoring(OwnerRole::Owned, tmp(), &empty_reg(), &rec_mon(&log));
    start_owner_stream_observers(OwnerRole::Owned, &m, tmp(), &empty_reg(), &rec_stream(&log));
    assert_eq!(log.into_inner(),
        ["monitor_tick","api_activity_probe","rollout","opencode","kiro"]); // ordered ⇒ phase swap/omit is RED
}
#[test] // I3: attached starts none (both phases)
fn attached_two_phases_start_none() {
    let log = RefCell::new(vec![]);
    let m = start_owner_monitoring(OwnerRole::Attached, tmp(), &empty_reg(), &rec_mon(&log));
    start_owner_stream_observers(OwnerRole::Attached, &m, tmp(), &empty_reg(), &rec_stream(&log));
    assert!(log.into_inner().is_empty());
}
```
**Reverse-mutation (§3.20 SOP3):**
- omit a monitoring closure / reorder → `owned_two_phases_start_all_five_in_order` RED.
- swap phase calls / omit monitoring entirely → **won't compile** (no `OwnerMonitoringStarted` for phase 2).
- run under `Attached` in phase body → `attached_two_phases_start_none` RED.
- I4: re-add a direct `instance_monitor::spawn_monitor_tick(h,r,?)` to a host → **build fails** (no permit).
- I2: delete seam call in `build_tick_infrastructure` → `TickKeepalive` unbuildable → build fails; delete in `run_app` → `app_maintenance_tick` arg unsatisfiable → build fails.

Tests drive the same phase fns production calls → production-path-coupled (§3.9).

---

## 6. Decisions for codex (spike ready)
1. Two-phase design confirmed to preserve EXACT app/daemon order (§1a/§2c) — no `setup_app_bootstrap` move.
2. App I2 witness consumed at owned-only `app_maintenance_tick` (§2c) — confirm, or prefer the `AppOwnerKeepalive` wrapper variant.
3. Permit on 5 spawns (0 breakage) — in scope.
4. I3: production = unchanged `!attached_mode` wrapper + behavioral Attached test (§2d); no automated "calls-stay-in-wrapper" guard (would require role-driven unconditional call → reorders → forbidden). Acceptable trade for exact order?
5. P1 shadow::start out of scope (host-local; #2738/#2739 runtime coverage). Not claiming all app scans gone.
6. A1 deferred.

---

## 7. Evidence
- `git rev-parse HEAD` → `c4206950…`.
- exact order cited: `app/mod.rs:463-487`, `daemon/mod.rs:1240-1255`.
- `app_maintenance_tick` def `app/mod.rs:1365`, call `:1156`, owned-only (tick arm `None`→`never_rx` in attached; comment `:1148-1149`, `:751`/`:776`).
- `TickKeepalive` built `daemon/mod.rs:1291`, held `:849`.
- 5 spawn symbols sole callers = `owner_services.rs:27/31/42/44/46` (git grep); same-file `.spawn` = `thread::Builder`; 0 test breakage on permit.
- masker 30/30 usages in `app/mod.rs`, no other consumers.
- DI seam prior art: `api/handlers/external.rs:18`, `ci_watch/provider.rs:117`, `event_bus.rs:229`.
