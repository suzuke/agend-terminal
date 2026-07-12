# SPIKE #2737 — Typed / Call-Level Owner-Service Invariants — Decision Manifest **v2**

- **Task:** t-20260712023025086467-46182-2 (branch `spike/2737-typed-owner-invariants`)
- **Source of truth:** decision `d-20260712023005493110-0` + PR #2737 POST-MERGE audit + codex-125550 REJECT-v1 (m-20260712032940580882-20).
- **Freshness:** origin/main @ `c4206950619e748c4e641f6076c762e0ee88d916` (= worktree HEAD; guards @ merge `6f90572d`).
- **v2 changelog (addresses codex REJECT):** I4 no longer claimed behavioral — closed **structurally by a private-constructor permit**. App I2 no longer reviewer-only — closed **by-construction via an `AppBootstrap` witness at `setup_app_bootstrap`**. I1 claim narrowed (acknowledgement of a *seam-routed* service, does NOT catch off-seam spawn — that is the permit's job). Migration is now **RED-first** (failing tests precede the seam). P1 explicitly out of scope. A1 deferred.

---

## 0. Recommendation (TL;DR)

**Approach 2 (injectable seam) + a private-constructor permit + a bootstrap witness.** This is the smallest design that closes all four invariants by *construction or real call-level test* (no source scan, no threads), letting the ~295-LOC masker + 3 static guards + 3 scan pins be deleted:

| invariant | closed by | mechanism |
|---|---|---|
| I1 completeness (seam-routed) | call-level test | `OwnerServiceStarters` 5 required fields + `real()` + exact-set test → a 6th seam service must be acknowledged in all three |
| I2 dual-host reach | **by construction** | daemon: `OwnerServicesStarted` witness in `TickInfrastructure` return; app: witness in `AppBootstrap` returned by `setup_app_bootstrap` (witness ctor private to the seam) |
| I3 attached exclusion | **by construction + test** | seam takes `OwnerRole`; app passes role derived from `attached_mode`; test drives real seam with `Attached` → starts none |
| I4 no host-local copy | **by construction** | `&OwnerServicePermit` (private ctor) required at all 5 spawn entry points → a host-body direct spawn **fails to compile** |

Not A1 full type-state (defers I2 through run_app's ~900-line body — over-heavy for a reversible stage). Not A3 syn-AST (lateral: same source-scan class, zero runtime coverage — the thing `d-…493110-0` says to leave).

---

## 1. Exact current guard inventory (@ HEAD c4206950)

### 1a. Production wiring
- `owner_services.rs:26` `start_shared_monitoring_services` → `instance_monitor::spawn_monitor_tick` (:27) + `api_activity_probe::spawn` (:31)
- `owner_services.rs:39` `start_shared_stream_observers` → `shadow::rollout::spawn` (:42) + `shadow::opencode::spawn` (:44) + `shadow::kiro::spawn` (:46)
- App host `run_app` (`app/mod.rs:405`): helpers at :469 / :487, inside `if !attached_mode {` (:463).
- Daemon host `build_tick_infrastructure` (`daemon/mod.rs:1232`): helpers at :1250 / :1255 (always owner).
- Each inner spawn = real `std::thread::Builder` fire-and-forget (`instance_monitor.rs:66`, `api_activity_probe.rs:76`, `shadow/{rollout:238,opencode:372,kiro:155}`).

### 1b. Guards / pins (all `app/mod.rs` `mod tests`)
| # | fn | line | mechanism | invariant | v2 fate |
|---|---|---|---|---|---|
| G1 | `owner_services_called_by_both_hosts` | 2850 | masker, `contains(helper)` both hosts | I2 | delete (→ witnesses) |
| G2 | `owner_services_spawns_absent_from_hosts_present_at_wiring_site` | 2866 | masker, `!contains`/`contains` | I4+I1 | delete (→ permit makes "absent" a compile guarantee) |
| G3 | `owner_services_calls_inside_attached_mode_guard` | 2890 | masker + brace-match | I3 | delete (→ seam gate + test) |
| P1 | `run_app_wires_shadow_socket_server_2413` | 2386 | **raw** `contains(shadow::start(&home))`, NO masker | shadow::start (EXCLUDED, host-local) | **KEEP — out of scope** (§8.5) |
| P2 | `run_app_wires_codex_rollout_tailer_2413` | 2412 | masker | rollout present | delete |
| P3 | `run_app_wires_opencode_sse_observer_2413` | 2442 | masker | opencode present | delete |
| P4 | `run_app_wires_kiro_session_tailer_2413` | 2794 | masker | kiro present | delete |

### 1c. Masker machinery (the handwritten Rust the decision names — all deletable)
`strip_rust_comments` :2474 (~130 LOC) · `blank_string_contents` :2605 (~100 LOC) · `strip_comments_and_blank_strings` :2711 · self-tests :2719/:2747 (~60 LOC) · `owner_wiring_prod` :2837 · `OWNER_HELPERS` :2820 · `OWNER_MOVED_SPAWNS` :2824.

> ⚠️ **Naming drift:** PR #2737's *body* named guards `both_hosts_call_the_two_shared_helpers` etc.; merged code uses the `owner_services_*` names above. Impl targets the merged names.

---

## 2. Invariants (the real spec)
- **I1 Completeness** — owner-service set = exactly {monitor_tick, api_activity_probe, rollout, opencode, kiro}; a new *seam-routed* service is wired once.
- **I2 Dual-host reach** — BOTH `run_app` (owned TUI = live fleet daemon) and `build_tick_infrastructure` reach the composition (root class #982/#1002/#1720/#2434).
- **I3 Attached exclusion** — attached TUI starts none.
- **I4 No host-local copy** — no host body spawns a service directly.

---

## 3. Approaches compared (corrected coverage)

✅ by-construction · ◑ call-level test · ⚠️ last-mile/convention · ❌ none

| | I1 | I2 | I3 | I4 | no threads | deletes masker | runtime coverage | KISS |
|---|---|---|---|---|---|---|---|---|
| **A2 seam + permit + witness (v2 recommended)** | ◑ | ✅ | ✅+◑ | ✅ | ✅ | ✅ | yes | low |
| A2-plain (v1, seam only) | ◑ | ⚠️ run_app | ✅+◑ | ⚠️ **convention** | ✅ | ✅ | yes | low |
| A1 full type-state | ❌(type≠svcs) | ✅✅ | ⚠️(type≠gate) | ✅ | ✅ | ✅ | partial | **high** (run_app body) |
| A3 syn-AST scan | ✅ | ✅ | ✅ | ✅ | ✅ | replaces w/ AST | ❌ | med |

**v1's two overclaims, now fixed:**
- *I4 was ⚠️ not ✅.* v1 rested I4 on "real spawns live only in `RealStarters`" — that is **convention**: all 5 spawn fns stay crate-callable, so a host could reintroduce a direct spawn and the fake-seam tests still pass. **Fix: the permit (§4a) makes a direct host spawn a compile error.**
- *App I2 was ⚠️ reviewer-only.* v1 left run_app's single seam call to source review — a source-review last mile the charter forbids. **Fix: the `AppBootstrap` witness (§4c) makes it compile-forced.**

**A3 rejected** as primary: the decision targets "typed construction OR real call-level runtime invariants"; AST is neither — it hardens the *matcher* but keeps the no-runtime-coverage debt and adds parser-walk code we'd delete masker to avoid. (A syn pass is the right tool only if a source scan must survive — here it need not.)

---

## 4. Recommended design (concrete)

### 4a. Permit → structural I4 (in `owner_services.rs`)
```rust
/// Capability token. Public field is `()` and PRIVATE to this module, so only
/// `owner_services` code can mint one. Other modules may NAME `&OwnerServicePermit`
/// but cannot construct it → a host-body direct spawn cannot satisfy the arg.
pub(crate) struct OwnerServicePermit(());
```
The **five spawn entry points gain a `_permit: &OwnerServicePermit` parameter** (they need not use it — its presence in the signature is the gate). Exact 5 signature edits:
| fn | today | v2 |
|---|---|---|
| `instance_monitor::spawn_monitor_tick` | `(home: PathBuf, registry: AgentRegistry)` | `(…, permit: &OwnerServicePermit)` |
| `api_activity_probe::spawn` | `(registry: AgentRegistry)` | `(…, permit: &OwnerServicePermit)` |
| `shadow::rollout::spawn` | `(registry, home: PathBuf)` | `(…, permit: &OwnerServicePermit)` |
| `shadow::opencode::spawn` | `(registry, _home: PathBuf)` | `(…, permit: &OwnerServicePermit)` |
| `shadow::kiro::spawn` | `(registry, home: PathBuf)` | `(…, permit: &OwnerServicePermit)` |

**Blast radius (verified, not assumed):** `git grep` of all five symbols → the ONLY callers are the 5 lines in `owner_services.rs`; the app/mod.rs hits are string literals in the masker, and every same-file `.spawn(move||…)` is `thread::Builder::spawn` (not these fns). **No existing direct-call test exists → zero test breakage.** (If a future module wants to unit-test its own spawn, add a `#[cfg(test)]` permit minter — none needed today.) The permit is `&`-borrowed so it never enters the spawned thread.

### 4b. Injectable seam (mints the permit, drives the 5 starters)
```rust
#[derive(Clone, Copy, PartialEq, Eq)] pub(crate) enum OwnerRole { Owned, Attached }

pub(crate) struct OwnerServiceStarters<'a> {   // 5 REQUIRED fields — struct-literal forces all 5
    pub monitor_tick:       &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
    pub api_activity_probe: &'a dyn Fn(&OwnerServicePermit, &AgentRegistry),
    pub rollout:            &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
    pub opencode:           &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
    pub kiro:               &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
}
impl OwnerServiceStarters<'static> { pub(crate) fn real() -> Self { /* each field forwards to the real spawn */ } }

#[must_use] pub(crate) struct OwnerServicesStarted(());  // witness; ctor PRIVATE to this module

pub(crate) fn start_owner_services(
    role: OwnerRole, home: &Path, reg: &AgentRegistry, s: &OwnerServiceStarters<'_>,
) -> OwnerServicesStarted {
    if role == OwnerRole::Owned {
        let permit = OwnerServicePermit(());       // minted here, nowhere else
        (s.monitor_tick)(&permit, home, reg);
        (s.api_activity_probe)(&permit, reg);
        (s.rollout)(&permit, home, reg);
        (s.opencode)(&permit, home, reg);
        (s.kiro)(&permit, home, reg);
    }
    OwnerServicesStarted(())
}
```
- Matches repo `dyn Fn` seam convention (`api/handlers/external.rs:18`, `ci_watch/provider.rs:117`) → closures, not a trait.
- I3 is **the real production path**: app calls with `Owned`/`Attached` from `attached_mode`, so the tested branch IS production (not "the call is textually inside the guard").

### 4c. Witnesses → by-construction I2 (compare vs v1 reviewer-only)
- **Daemon:** `build_tick_infrastructure` already returns a tuple / `TickKeepalive`. Add `OwnerServicesStarted` to it (e.g. field on `TickKeepalive`). Since the witness ctor is private to the seam, run_core **cannot build its return value without calling the seam** → compile-forced.
- **App:** replace `setup_app_bootstrap`'s tuple (`app/mod.rs:1305`, returns `(ApiGuard, Option<Arc<dyn Channel>>, TelegramStatus, Option<PathBuf>)`) with a struct:
  ```rust
  struct AppBootstrap { api_guard: ApiGuard, telegram: Option<Arc<dyn Channel>>,
                        telegram_status: TelegramStatus, attached_run_dir: Option<PathBuf>,
                        owner_services: OwnerServicesStarted }
  ```
  `setup_app_bootstrap` already receives `home` + `registry` and makes the Owned/Attached decision (`bootstrap::prepare` outcome). It calls `start_owner_services(role, home, registry, &OwnerServiceStarters::real())` and stores the witness. Because the witness ctor is private to the seam, **`setup_app_bootstrap` cannot return a valid `AppBootstrap` without calling the seam**, and `run_app` cannot skip `setup_app_bootstrap` (it needs `api_guard`). → app I2 compile-forced, **without touching run_app's 900-line body**.
  - **vs v1 reviewer-only:** strictly better — turns a source-review last mile into a compiler error.
  - ⚠️ **Ordering delta to assess (reviewer-verify):** today the app seam runs at :469/:487 *after* `supervisor::spawn` (:464) and *after* `shadow::start` (:480); moving it into `setup_app_bootstrap` starts owner services slightly **earlier**. Assessed **safe**: all five are independent fire-and-forget detached threads sharing only the `registry` Arc; none observes supervisor/shadow-socket state at startup, and `setup_app_bootstrap` already performs startup side-effects (API server, signal handler). This IS a deviation from #2737's "identical order" and must be called out in the PR + reviewer-confirmed. `supervisor::spawn` and `shadow::start` stay in run_app's `if !attached_mode` block (both EXCLUDED from the owner-service set — separate forks).

---

## 5. I1 — exactly what it does and does NOT catch (per codex point 3)

**Catches (a seam-routed 6th service):** adding an owner service forces THREE edits or the build/tests fail: (a) a new `OwnerServiceStarters` field → every struct literal (incl. `real()`) fails to compile until populated; (b) `real()` must forward it; (c) the exact-set test `owned_role_starts_exactly_the_five` (set-equality) goes RED until its expected set is updated. So a service *routed through the seam* cannot be added silently.

**Does NOT catch (must not be claimed):** a future author wiring a brand-new service *directly into a host, bypassing the seam entirely*. I1's set-test only observes what the seam starts. That off-seam case is caught only if the new spawn fn also takes `&OwnerServicePermit` (the convention the permit establishes) — otherwise it is a review concern. The permit closes I4 for the **five current** spawns un-bypassably; extending the guarantee to future services is a one-line convention (new owner spawns take the permit), not an automatic property. Stated plainly so we do not repeat v1's overclaim.

---

## 6. Migration — RED-first, one PR (per codex point 4 / §3.10)

**One PR, three commits** (RED precedes seam; delete after green):

- **C1 — RED (test-first):** add the new behavioral test module driving `start_owner_services` with recording fakes (§7) + the *minimal unwired* seam declarations (`OwnerRole`, `OwnerServicePermit`, `OwnerServiceStarters`, `OwnerServicesStarted`, and `start_owner_services` with a **stub body that starts nothing**). Result: new tests **compile and FAIL** (stub → started-set empty ≠ 5; attached test passes trivially, so make the RED explicit on the owned-set test). Hosts untouched, old guards still green, build green. ← the failing-test-before-impl commit.
- **C2 — GREEN (impl + rewire):** real `start_owner_services` body; add `&OwnerServicePermit` to the 5 spawn signatures; `OwnerServiceStarters::real()` routes the 5 with the permit; daemon host → seam + witness in `TickKeepalive`; app host → seam moved into `setup_app_bootstrap` returning `AppBootstrap` witness. Remove the now-orphaned `start_shared_*` helpers **and the guards coupled to the old wiring** (G1 pins old helper names at hosts; G3 pins the `if !attached_mode` block that no longer holds the calls) — these go RED on rewire, so they leave with it. New tests green.
- **C3 — CLEANUP (after green):** delete P2–P4 + G2 (permit makes "absent from hosts" a compile guarantee; set-test covers presence) + the entire masker (`strip_rust_comments`, `blank_string_contents`, `strip_comments_and_blank_strings`, both self-tests) + `owner_wiring_prod` + `OWNER_HELPERS`/`OWNER_MOVED_SPAWNS`.

**Why one PR (not split):** the host rewire, old-guard removal, and masker deletion are causally coupled (rewiring the hosts turns G1/G3 RED); splitting would land a RED intermediate. C1→C2→C3 keeps every commit green except the deliberate C1 RED test. Split only if C2's rewire proves riskier than expected (then C3 → follow-up PR after C2 merges green).

**Deletion total ≈ 450 LOC** (masker ~290 + guards/pins/consts ~140) out; **~110 LOC in** (permit + seam + witnesses + tests) + 5 one-arg signature edits.

---

## 7. RED tests (land in C1; drive the REAL seam with recording fakes — no threads)

```rust
// recording fake: closures push a label; permit arg ignored. Zero threads.
fn recording<'a>(log: &'a RefCell<Vec<&'static str>>) -> OwnerServiceStarters<'a> {
    OwnerServiceStarters {
        monitor_tick:       &|_p,_h,_r| log.borrow_mut().push("monitor_tick"),
        api_activity_probe: &|_p,_r|    log.borrow_mut().push("api_activity_probe"),
        rollout:            &|_p,_h,_r| log.borrow_mut().push("rollout"),
        opencode:           &|_p,_h,_r| log.borrow_mut().push("opencode"),
        kiro:               &|_p,_h,_r| log.borrow_mut().push("kiro"),
    }
}

#[test] // I1: owner mode starts EXACTLY the five (exact-set → a 6th prod service goes RED here)
fn owned_role_starts_exactly_the_five_owner_services() {
    let log = RefCell::new(vec![]);
    start_owner_services(OwnerRole::Owned, tmp(), &empty_registry(), &recording(&log));
    let mut got = log.into_inner(); got.sort();
    assert_eq!(got, ["api_activity_probe","kiro","monitor_tick","opencode","rollout"]);
}

#[test] // I3: attached mode starts NONE
fn attached_role_starts_no_owner_services() {
    let log = RefCell::new(vec![]);
    start_owner_services(OwnerRole::Attached, tmp(), &empty_registry(), &recording(&log));
    assert!(log.into_inner().is_empty());
}
```
**Compile-time invariants (I2 witnesses, I4 permit)** are verified by reverse-mutation, per §3.20 SOP3 (a runtime test can't assert "fails to compile"; a `trybuild` compile-fail fixture is optional if the dep is present — it is not currently):
- I4: reviewer re-adds a direct `instance_monitor::spawn_monitor_tick(h,r,??)` into a host body → **build fails** (no permit obtainable).
- I2-daemon: delete the seam call from `build_tick_infrastructure` → `TickKeepalive` can't be built → **build fails**.
- I2-app: delete the seam call from `setup_app_bootstrap` → `AppBootstrap` can't be built → **build fails**.
- I1: remove a closure from `start_owner_services` → `owned_role_starts_exactly_the_five` RED. Flip its gate to run under `Attached` → `attached_role_starts_no_owner_services` RED.

Tests drive the same `start_owner_services` production calls → production-path-coupled (§3.9), no helper-mimic, no mid-pipeline inject.

---

## 8. Open items / decisions for codex

1. **I2 for run_app:** v2 adopts the `AppBootstrap` witness (compile-forced) per your point 2 — replacing v1's reviewer-only option. Confirm the **ordering delta** in §4c (owner services start earlier, assessed safe) is acceptable, or require preserving exact order (then the witness must instead be threaded to a mandatory post-bootstrap consumer in run_app — more invasive; I do not recommend).
2. **Permit param on the 5 spawns:** confirm the `&OwnerServicePermit` signature change (5 fns, 0 test breakage) is in scope.
3. **I1 scope:** confirmed narrowed — catches seam-routed additions, NOT off-seam spawns (permit handles the 5 current; off-seam-new stays a convention/review concern). Acceptable?
4. **A1 full host typestate:** deferred (revisit if #2453 later unifies host bodies).
5. **P1 `run_app_wires_shadow_socket_server_2413`:** **left out of this implementation scope** — `shadow::start` is host-local and a separate fork (per d-20260711201257672833-2), and you note #2738/#2739 provide its runtime ordering coverage. Not claiming "all app static scans are gone" — only the owner-service masker scans (G1–G3, P2–P4) are removed; P1 (raw `contains`, no masker) stays as the host-local shadow-socket pin.

---

## 9. Evidence
- `git rev-parse HEAD` → `c4206950…` (freshness match).
- `git grep` 5 spawn symbols → sole callers = `owner_services.rs:27/31/42/44/46`; app/mod.rs matches are masker string literals; every same-file `.spawn(move||…)` is `thread::Builder::spawn`. → permit = 5 sig + 5 call edits, **zero test breakage**.
- `setup_app_bootstrap` `app/mod.rs:1305` returns `(ApiGuard, Option<Arc<dyn Channel>>, TelegramStatus, Option<PathBuf>)`; receives `home`+`registry`; makes Owned/Attached decision → natural witness home.
- masker 30/30 usages in `app/mod.rs`, zero other consumers → deletable.
- inner spawns = real `std::thread::Builder`: `instance_monitor.rs:66`, `api_activity_probe.rs:76`, `shadow/rollout.rs:238`, `shadow/opencode.rs:372`, `shadow/kiro.rs:155`.
- DI seam prior art: `api/handlers/external.rs:18`, `ci_watch/provider.rs:117`, `event_bus.rs:229`.
- Call sites: app `app/mod.rs:463/469/487`; daemon `daemon/mod.rs:1232/1250/1255`.
