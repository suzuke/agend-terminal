[繁體中文](DETECTION-RULES-SPIKE-3647.zh-TW.md)

# Spike: Data-Driven Screen-Detection Rules (issue #3647)

> Design spike for issue #3647. This is a decision document only: nothing here
> changes production code. The proposal externalises the per-backend screen
> detection patterns that today live as hardcoded Rust regexes into a
> per-backend data manifest, while the state machine and the structural
> false-positive guards stay in code. This document answers the nine spike
> questions, inventories every current pattern, and sequences the work so the
> first cut is behaviour-preserving.

## 0. Status, scope, and non-goals

The screen layer classifies a rendered pane into an `AgentState`
(`src/state/mod.rs:36`). Every backend's patterns are Rust literals today: the
co-located `BackendProfile` bundles in `src/backend_profile.rs`, the shared
network-error alternation in `src/state/patterns.rs:42`, and the global hint
tokens in `src/state/mod.rs:781`. When a CLI rewords a banner, a pinned regex
quietly stops matching. Issue #3534 is the motivating case: the UsageLimit
regex was pinned to `You've reached your Fable 5 limit`, the live banner read
`Fable limit`, and an instance idled for 82 minutes undetected.

The model to borrow from herdr is `rules as data` plus `match screen structure`
plus `explain`; its state model and its unsigned remote-update path are not
worth copying. This spike decides the shape of that externalisation.

- Goals: answer the nine questions below; produce the full inventory; sequence
  the implementation so the first cut is a byte-identical migration.
- Non-goals: sidebar UI (#3645); toast or audio notifications; the
  implementation of remote hot-update (only the go/no-go and its preconditions);
  any change to state semantics or any new state.
- Deliverable: this document. No production-code change.

## 1. Inventory and split

Every current detection pattern is listed in Appendix A. The split principle is
simple: **regex literals and hint-token lists move into data; anything that is
control flow, timing, or a cross-field structural check stays in code.** Data
can select which named guards apply to a rule, but data cannot define new guard
logic, so a data-only edit can never weaken a guard.

The layer-by-layer split:

| Layer | Location today | Decision |
|---|---|---|
| Per-backend `AgentState` regexes | `src/backend_profile.rs` `*_profile()` | Move to data, order preserved |
| Shared net-error alternation | `src/state/patterns.rs:42` | Move to data as a named fragment, referenced by rule id |
| Tail-anchored update menu | `src/backend_profile.rs:178` | Move to data, attach the code-side tail-anchor guard |
| Context percent regex | `src/backend_profile.rs:108,113` | Move to data, region fixed to bottom status rows |
| Throttle hint tokens | `src/state/mod.rs:781` | Move to data as a token list |
| Input-line markers | `BackendProfile.input_line_markers` | Move to data (already per-backend) |
| State machine and priority | `src/state/mod.rs:148,2368` | Stay in code |
| Latch, expiry, oscillation guard | `src/state/mod.rs:979-1012,2402` | Stay in code |
| `HIGH_FP` predicate and anchor choice | `src/state/mod.rs:563,603` | Stay in code |
| Red anchor and content anchor | `src/state/mod.rs:1648` | Stay in code |
| Position gate (live tail) | `src/state/mod.rs:1691` | Stay in code |
| Working-marker override | `src/state/mod.rs:1863` | Stay in code |
| Hard-wrap flatten rescue | `src/state/mod.rs:833-963,1751,1780` | Stay in code, guard semantics fixed |
| Proximity guards | `src/state/mod.rs:912,944` | Stay in code, referenced by name |
| Hash-dedup and heartbeat gate | `src/state/mod.rs:1515,1186` | Stay in code |
| Startup prompt structural fallback | `src/state/patterns.rs:368` | Stay in code |
| Productive markers and behavioral timings | `src/behavioral.rs:26,111,179-244` | Out of scope for this spike, unchanged |
| Dismiss patterns and re-arm hints | `src/backend.rs:321,390` | Adjacent bucket, unchanged for now |

The pattern-level decision is therefore almost uniformly `data`; the
exceptions are called out in Appendix A and are limited to `data+anchor` (the
regex moves but a named code guard is attached) and `code` (the structural
startup-prompt recognizer stays in Rust). No state, priority, latch, clear, or
gate behaviour moves into data.

## 2. Rule schema and structural guards

The schema must make the existing structural guards expressible without moving
their semantics into data. Each rule declares a region, a matcher, and a list of
**named guards**. Guards are a closed vocabulary implemented in Rust; data
names them and supplies only bounded parameters that the loader clamps. An
unknown guard name is a load error, not a silent no-op, so a manifest can never
silently drop a guard.

```toml
schema = 1
backend = "claude"
source = "builtin"

[[rules]]
id = "claude.usage_limit.model_tier"
state = "usage_limit"
priority = 60
region = "whole_screen"
matcher = { kind = "regex", value = "..." }
guards = ["input_line_excluded", "usagelimit_banner_adjacent"]

[[rules]]
id = "claude.context_full.bounded"
state = "context_full"
priority = 30
region = "error_tail"
matcher = { kind = "regex", value = "..." }
guards = ["red_anchor"]
```

The guard vocabulary that preserves today's strength:

| Guard | Semantics | Location today |
|---|---|---|
| `red_anchor` | At least one red cell across an on-screen occurrence | `src/state/mod.rs:1648` |
| `error_line_content` | Match sits on an error-shaped line, input lines excluded | `src/state/patterns.rs:121` |
| `input_line_excluded` | Match does not sit on the backend input line | `src/state/patterns.rs:153` |
| `usagelimit_banner_adjacent` | Box-draw before plus reset stamp after, within 40 chars | `src/state/mod.rs:944` |
| `throttle_indicator_adjacent` | Error indicator within 80 chars before the match | `src/state/mod.rs:912` |
| `tail_anchored` | Whole block sits at end of screen (`\z`) | `src/backend_profile.rs:178` |
| `position_error_tail` | Match still inside the live bottom 15 rows | `src/state/mod.rs:1691` |
| `status_rows` | Scan only the bottom status rows for context percent | `src/state/mod.rs:530` |

The proximity guards keep their numeric constants in Rust and accept no
data-supplied value, so `usagelimit_banner_adjacent` cannot be loosened by a
local or remote manifest. `region` selects a screen-slicing primitive
(`whole_screen`, `error_tail`, `hard_wrap_tail`, `status_rows`) whose windows
are existing code constants. Matching is a matcher tree of `all`, `any`, and
`not` over leaves `literal`, `regex`, and `line_regex`, so a rule that today is
a single regex stays a single regex and the first cut is exact.

## 3. Loading and local override

Built-in manifests ship inside the binary with `include_str!`, so they are
versioned with the release. A local override lives at
`~/.config/agend-terminal/detection/<backend>.toml`.

- Override granularity: **whole-file replacement**, not per-rule merge. A
  per-rule merge makes "what actually ran" hard to reconstruct and can hide a
  dropped guard; whole-file replacement plus `explain` keeps the running rule
  set auditable. This matches the herdr precedent.
- Fail-closed: any load failure — TOML parse, schema mismatch, regex that does
  not compile, unknown guard name, or a missing rule that a health invariant
  requires — falls back to the built-in manifest and emits a visible warning
  (a log line, a health field, and an `explain` banner). A broken override
  never silently disables detection.
- Replacement still cannot weaken guards: the override is subject to the same
  closed guard vocabulary and the same Rust-side proximity constants.
- The loader records the source (`builtin` or `local`) and a content hash, both
  surfaced by `explain` and in telemetry, so the active manifest is always
  identifiable.

## 4. Remote updates

Decision: **do not ship remote hot-update in this work.** herdr's remote path
checks once at startup and has no signature or hash verification, which turns
detection rules into a remotely writable control channel over daemon behaviour;
that is not a risk worth taking for a rule file that changes rarely.

If it is revisited, the preconditions are, in order: (a) a signature over the
manifest verified against a public key pinned in the binary; (b) a
monotonically increasing version with rollback refusal; (c) the same fail-closed
load validation as local overrides; (d) an explicit operator kill-switch. Until
all four exist, the only inputs are the built-in manifest and the local
override, both of which are already operator-controlled.

## 5. explain interface

Ship both surfaces, because they serve different callers:

- CLI: `agend-terminal state explain [target] [--file SCREEN_FILE] [--json]`.
- MCP tool: `explain_state`, for agents and orchestrators that want the same
  data programmatically.

The `--file` form is the regression-test entry point: it feeds a captured raw
screen (or an already-rendered pane dump) through the same classifier and prints
a stable record, so a fixture can assert a golden output. The fields:

| Field | Meaning |
|---|---|
| `backend` | Backend whose manifest was evaluated |
| `manifest_source` | `builtin` or `local` |
| `manifest_hash` | Content hash of the active manifest |
| `region` | Screen region each rule was evaluated against |
| `rules` | Every rule id with matched true or false, and its evidence snippet |
| `guards` | Per-rule guard decisions with the reason for any rejection |
| `winner` | Winning rule id, state, and priority, or none |
| `fallback_reason` | Why nothing latched: no match, anchor fail, stale position, dedup skip, or latch held |
| `throttle_hint` | Whether the cheap hint pre-filter fired |
| `hard_wrap_rescue` | Whether the flatten rescue ran and what it found |

`explain` is read-only and never touches `State::current`, matching the
cycle-proof invariant the shadow observer already respects
(`src/daemon/shadow/mod.rs:88`).

## 6. Relationship with hook authority

The precedence does not change. Screen detection produces the `raw` state; the
Shadow Observer's hook or stream evidence can override it only through the
existing shared gate (`src/daemon/shadow/gate.rs:95`). That gate fires only when
authority is `Hook` or `Stream` with `Confirmed` or `Strong` confidence, the raw
screen is not an authoritative gate screen, and the observed state disagrees at
the coarse level. Hook signals that never carry rate-limit information are
explicit in the evidence contract (`src/daemon/shadow/evidence.rs:38,152`).

| Screen state | Can a hook or stream override it | Why |
|---|---|---|
| `Approval`, `PermissionPrompt` | No | A human gate is always authoritative |
| `UsageLimit`, user `RateLimit` | No | The operator must always see the wall |
| `ServerRateLimit` | Only by a fresh post-SRL Active episode | A stale banner must not pin a resumed agent |
| `Active`, `Idle`, `AwaitingOperator` | Yes | The live lifecycle plane is stronger |
| `ApiError`, `ContextFull`, `ModelUnsupported`, `AuthError`, `GitConflict` | In practice screen-only | No hook or stream plane emits them reliably |

The externalisation does not move this line: it changes how `raw` is computed,
never how `raw` and `observed` are combined. This is also why UsageLimit and
RateLimit remain screen-only states and must keep strong fixtures (section 7).

## 7. Test strategy

Two layers already exist and should be extended, not replaced.

- Unit fixtures: inline positive and negative panes in `src/state/tests.rs`,
  such as the #3534 pair at `src/state/tests.rs:2952-3028`.
- Corpus replay: `tests/fixtures/state-replay/*.raw` with expected transitions
  in `MANIFEST.yaml`, driven through vterm and `StateTracker` by
  `replay_manifest_regression` (`src/state/tests.rs:1778`); presence is enforced
  by `tests/state_pattern_coverage.rs`.

New enforcement for the externalised rules:

- Every rule id must declare at least one positive fixture and one negative
  fixture in the corpus index. A new invariant test enumerates manifest rules in
  CI and fails when a rule has no positive or no negative fixture, and when a
  fixture references no rule.
- Changing a rule without updating its fixtures fails that test, because the
  rule id carries the fixture requirement with it.
- Banner-class rules (`UsageLimit`, `RateLimit`, `AuthError`) must have at least
  one real-capture positive fixture, and must key on structure as well as
  wording.

Can this root out the #3534 class of failure? Partly, and it is worth being
honest about the limit. CI cannot know that a vendor changed its banner text.
What the corpus gate guarantees is that a changed rule arrives with evidence and
that a Structural-key rule keeps a negative quote fixture. The real defence
against #3534 is the structural key itself (box-draw chrome plus the
`/usage-credits` remedy, model name free), and the corpus gate forces that
discipline on every future banner rule. An optional follow-up is a periodic
re-capture canary comparing live banner shapes against the corpus; that needs
the live CLI and belongs outside CI.

## 8. Debounce

herdr confirms Working to Idle three times over about 700 ms. This codebase
already has stronger and cheaper equivalent machinery: the screen hash dedup
gate skips identical redraws (`src/state/mod.rs:1515`), the transition function
applies a 5 s passive or 2 s active minimum hold before a priority-down move
(`src/state/mod.rs:2381`), the oscillation guard suppresses an Active bounce
(`src/state/mod.rs:2402`), and latched states expire on their own timers
(`src/state/mod.rs:979-1012`).

Decision: do not add a blanket multi-sample confirmation. It would add latency
to error detection, where instant latching is deliberate, and would duplicate
the hysteresis that already exists. The one legitimate gap is a specific rule
whose idle marker is known to flicker; if that appears, the schema gains an
optional per-rule `confirmations` count, applied only where a fixture proves the
need. That is a follow-up, not part of the first cut.

## 9. Migration path

The first cut is a pure data extraction and must be behaviour-identical.

1. Generate one manifest per backend by transcribing the existing
   `*_profile()` pattern vectors in order, mapping array order to strictly
   descending priorities. Keep the shared alternation as a named fragment.
2. Make `StatePatterns::for_backend` compile from the manifest through the same
   `Regex::new` pipeline, and keep the guard call sites unchanged.
3. Prove identity with a temporary parity test: keep the legacy pattern vectors
   frozen in test code and assert the loaded manifest yields the same
   `(state, regex source)` list, in the same order, per backend. The profile
   migration train (#1683 and its successors) used exactly this byte-identity
   harness before deleting the legacy source.
4. Only then change individual rules, each with its own positive and negative
   fixtures.

Because the manifest is compiled through the existing path and the guards are
untouched, the verdict on any screen is unchanged after cut one.

## Appendix A — Complete pattern inventory

Classes: `data` moves verbatim into the manifest; `data+anchor` moves and
attaches a named code guard; `data(field)` is an existing per-backend data
field; `code` stays in Rust. Source lines are in `src/backend_profile.rs`
unless noted.

```text
BACKEND  STATE             RULE-ID                            CLASS        PATTERN / SOURCE
grok     PermissionPrompt  grok.perm.trust                    data         Run Grok Build in a project directory\?|Do you trust
grok     Active            grok.active.stop                   data         \[stop\]|Ctrl\+c:cancel|Esc:cancel|Ctrl\+;:queue
grok     Idle              grok.idle.legacy                   data         Turn completed in \d|Space:prompt|Enter:open
grok     Idle              grok.idle.footer                   data         (?m)^[ \t]*Shift\+Tab:mode[ \t]*│[ \t]*Ctrl\+\.:shortcuts[ \t]*$
agy      UsageLimit        agy.usage_limit.quota              data         Individual quota reached|Contact your administrator to enable overages
agy      RateLimit         agy.rate_limit.capacity            data         exhausted your capacity on this model
agy      ApiError          agy.api_error.high_traffic         data         servers are experiencing high traffic
agy      PermissionPrompt  agy.perm.request                   data         Requesting permission for:|Do you trust the contents of this project|tab Amend · e edit command
agy      GitConflict       agy.git_conflict.merge             data         Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in
agy      Active            agy.active.tool_bullet             data         ●\s+[A-Z][a-zA-Z]+\(
agy      Active            agy.active.esc_cancel              data         esc to cancel
agy      Idle              agy.idle.shortcuts                 data         \? for shortcuts
agy      Idle              agy.idle.chrome                    data         Antigravity CLI|Type your message
kiro     AuthError         kiro.auth.not_authenticated        data         Not authenticated|AccessDenied|denied access
kiro     UsageLimit        kiro.usage_limit.service_quota     data         ServiceQuotaExceeded|InsufficientModelCapacity|you have reached the limit
kiro     RateLimit         kiro.rate_limit.throttle           data         Too Many Requests|ThrottlingError|ThrottlingException|Rate exceeded|\b429\b
kiro     ServerRateLimit   kiro.server_rate_limit.net_errors  data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
kiro     ContextFull       kiro.context_full.overflow         data         context window overflow|compacting context
kiro     ModelUnsupported  kiro.model_unsupported.sentence    data         (?m)^\s*The\s+model\s+'[^'\r\n]+'\s+is\s+not\s+available\.\s+Please\s+use\s+'/model'\s+to\s+select\s+a\s+different\s+model\s+and\s+try\s+again\.
kiro     PermissionPrompt  kiro.perm.approval                 data         requires approval|ESC to close \| Tab to edit
kiro     GitConflict       kiro.git_conflict.merge            data         shared merge literal (see agy.git_conflict.merge)
kiro     Active            kiro.active.tools                  data         execute_bash|fs_read|fs_write
kiro     Active            kiro.active.working                data         Kiro is working|esc to cancel
kiro     Idle              kiro.idle.pct                      data         ◔\s*\d+(?:\.\d+)?%\s*$|ask a question or describe a task
kiro     Idle              kiro.idle.trust_all                data         Trust All Tools active|/quit to exit
opencode RateLimit         opencode.rate_limit.api            data         API rate limited \(429\)|Rate limited\. Quick retry|API rate limit exceeded
opencode ServerRateLimit   opencode.server_rate_limit.net     data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
opencode UsageLimit        opencode.usage_limit.quota         data         Quota Limit Exceeded|monthly usage limit reached
opencode AuthError         opencode.auth.invalid_key          data         Invalid API key
opencode ApiError          opencode.api_error.provider        data         Error from provider:|request validation errors
opencode ContextFull       opencode.context_full.overflow     data         ContextOverflow
opencode PermissionPrompt  opencode.perm.required             data         Permission required|Allow once\s+Allow always\s+Reject
opencode GitConflict       opencode.git_conflict.merge        data         shared merge literal (see agy.git_conflict.merge)
opencode Active            opencode.active.tool_marker        data         ✱\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b|~\s+(Reading|Writing|Editing|Searching|Listing|Globbing|Grepping)\b
opencode Active            opencode.active.esc_interrupt      data         esc interrupt
opencode PermissionPrompt  opencode.perm.update               data         Update Available|Skip\s+Confirm
opencode Idle              opencode.idle.ask_anything         data         Ask anything
opencode Idle              opencode.idle.ask_anything_tab     data         Ask anything|tab agents
opencode Idle              opencode.idle.statusline           data         ctrl\+p commands
codex    AuthError         codex.auth.api_key                 data         OPENAI_API_KEY|Incorrect API key|invalid_api_key
codex    UsageLimit        codex.usage_limit.hit_limit        data         hit your usage limit|try again at
codex    RateLimit         codex.rate_limit.hit_rate          data         rate_limit_exceeded|RateLimitError|hit your rate limit
codex    ServerRateLimit   codex.server_rate_limit.net        data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
codex    ContextFull       codex.context_full.overflow        data         ContextOverflow
codex    ModelUnsupported  codex.model_unsupported.invalid    data         invalid_request_error|model is not supported|Model metadata for .*? not found
codex    PermissionPrompt  codex.perm.run_command             data         Would you like to run the following command\?|Press enter to confirm or esc to cancel|No, and tell Codex what to do differently
codex    PermissionPrompt  codex.perm.update_menu             data+anchor  CODEX_UPDATE_MENU_LIVE (src/backend_profile.rs:178), guard tail_anchored
codex    GitConflict       codex.git_conflict.merge           data         shared merge literal (see agy.git_conflict.merge)
codex    Active            codex.active.working               data         Working|esc to interrupt
codex    Idle              codex.idle.prompt                  data         ›
codex    Idle              codex.idle.chrome                  data         OpenAI Codex|gpt-.*left
claude   AuthError         claude.auth.api_key                data         Invalid API key|invalid x-api-key|authentication_error|authentication failed|OAuth token has expired|Please run /login|API Error: 40[13]\b
claude   ServerRateLimit   claude.server_rate_limit.temp      data         Server is temporarily limiting requests|temporarily limiting.*not your usage|API Error: 5\d{2}\b|server-side issue.*temporary|API Error: Repeated 529 Overloaded|overloaded_error|api_error|timeout_error
claude   ServerRateLimit   claude.server_rate_limit.net       data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
claude   RateLimit         claude.rate_limit.429              data         API Error: Request rejected \(429\)|rate_limit_error|hit a rate limit
claude   UsageLimit        claude.usage_limit.model_tier      data+anchor  ⎿[ \t]+You've reached your [^\n]{0,32}? limit\.[ \t]+Run /usage-credits|You've hit your session limit|You've hit your weekly limit|You've hit your Opus limit|Credit balance is too low|credit_balance_too_low; guard usagelimit_banner_adjacent
claude   ContextFull       claude.context_full.bounded        data         compacting context|context.{0,16}(full|limit)
claude   PermissionPrompt  claude.perm.amend                  data         Esc to cancel · Tab to amend|allow all edits during this session|Enter to confirm · Esc to cancel
claude   GitConflict       claude.git_conflict.merge          data         shared merge literal (see agy.git_conflict.merge)
claude   Active            claude.active.spinner              data         (?m)^(?:[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+(?:Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)|[✓●⏺]\s+(?:Listing|Reading|Writing|Searching|Editing))\b
claude   Active            claude.active.sparkle              data         (?i)[✻✢✶✳✽*·]\s*\w+\x{2026}|\w+\x{2026}\s*\((?:\d+[smh]|running )|thought for [0-9]+s
claude   Idle              claude.idle.prompt                 data         ❯
claude   Idle              claude.idle.bypass                 data         bypass permissions
claude   telemetry         claude.context.pct                 data(field)  CLAUDE_CONTEXT_PATTERN (src/backend_profile.rs:108), region status_rows
kiro     telemetry         kiro.context.pct                   data(field)  KIRO_CONTEXT_PATTERN (src/backend_profile.rs:113), region status_rows
all      global            global.throttle_hint_tokens        data         THROTTLE_HINT_TOKENS (src/state/mod.rs:781)
all      Starting          generic.startup_prompt             code         (?i)\(y/n\)|\(yes/no\)|\[y/n\]|press\s+(enter|return|any\s+key) (src/state/patterns.rs:368)
```

## Appendix B — Implementation cuts and dependencies

| Cut | Change | Depends on | Parity evidence |
|---|---|---|---|
| 1 | Manifest schema plus loader; transcribe all patterns into built-in manifests | none | Frozen legacy vectors equal loaded `(state, regex)` lists per backend |
| 2 | Route `StatePatterns::for_backend` through the loader; attach named guards | cut 1 | Full fixture replay unchanged; all state unit tests green |
| 3 | Add the rule-to-fixture corpus index and the CI coverage invariant | cut 1, cut 2 | The new invariant test fails on a rule with no positive or negative fixture |
| 4 | Add local override loading with fail-closed fallback and warning | cut 2 | Load-failure test falls back to built-in and warns |
| 5 | Add `state explain` CLI and `explain_state` MCP tool | cut 2 | Golden `--file` output on corpus fixtures |
| 6 | Change individual rules, one manifest edit each | cut 3, cut 5 | Each change carries its own positive and negative fixtures |

Cut 1 is the behaviour-preserving migration the spike requires. Cuts 4 to 6 are
independent of each other once cut 2 lands.

## Appendix C — Decision summary

| Question | Decision |
|---|---|
| 1 Inventory and split | Regexes and token lists to data; state machine, latch, guards, and rescue logic stay in code |
| 2 Schema | Closed named-guard vocabulary; data selects guards, Rust defines their semantics and proximity constants |
| 3 Loading and override | Built-in via `include_str!`; whole-file local replacement; fail-closed to built-in with a visible warning |
| 4 Remote updates | Not now; if ever, signature plus monotonic version plus fail-closed load plus kill-switch are preconditions |
| 5 explain | Both CLI and MCP, sharing one classifier; `--file` is the regression entry point |
| 6 Hook authority | Unchanged; hooks override `raw` only through the existing gate, and UsageLimit and user RateLimit stay screen-only |
| 7 Test strategy | Rule-to-fixture coverage gate plus real-capture banner fixtures; structural keys are the true #3534 defence |
| 8 Debounce | Do not add blanket confirmation; reuse hash dedup, min-hold, and oscillation guard, add per-rule confirmation only if a fixture proves need |
| 9 Migration | First cut is a byte-identical extraction proven by a frozen-vector parity harness |
