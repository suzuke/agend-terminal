[English](HUNG-STATE-TRANSITIONS.md)

# Hung 狀態轉換稽核

> **目前狀態說明（`main@1d83b423`，2026-07-16）。** 本文件保留
> #685 Phase 1 的轉換稽核與穩定 section anchor。Live source 才是權威：
> state tracking 已從 `src/state.rs` 移至 `src/state/mod.rs` 加上
> `src/state/patterns.rs` / `src/backend_profile.rs`；Gemini 已退役
> （Agy 是其後繼者，且目前也支援 Grok）；舊有「只警告、沒有 recovery
> consumer」結論也已被取代。目前 `check_hang` 的 wiring 位於
> `src/daemon/per_tick/hang_detection.rs`，同一條 canonical handler
> pipeline 隨後接上僅含 Stage 1 的 recovery dispatcher。Dispatcher 的
> Stage 2/3 已在 #2549 移除；請參閱
> [RECOVERY-STAGES.zh-TW.md](RECOVERY-STAGES.zh-TW.md)。下方歷史 Gemini
> pattern 與 hypothesis 僅保留作為 provenance，不是目前 backend tuning
> 的指引。

本文件是 `src/health.rs` 中 `HealthState::Hung` 與
`HealthState::IdleLong` 轉換語意的 contract baseline，並與每個 mutation
site 的 inline structured comment 及 `check_hang` function-level rustdoc
相互配套。

Issue：[#685](https://github.com/suzuke/agend-terminal/issues/685) Phase 1
交付項目 #1。Decision：`d-20260513154400110972-2`。範圍嚴格受限——請見
下方 `§Scope`。

維護規則：section ID（`§Entry.E1`、`§Exit.X1` 等）是 **contract**
anchor——重新命名任何 heading 都會破壞 PR scope，且必須同步更新 inline
comment 與 decision reference。本文件對 source 的 cross-reference 使用
`rg <pattern>` grep hint，而不是檔案行號，因此重構造成換行變動時不會讓
文件失效；下方 prose 中的行號只供說明，反映的是 HEAD `2f24376`。

F9 productive-output contract 已整併至下方 §F9.1–§F9.5。這些 section
是 productive-output gate 的維護中權威來源；原本獨立的 F9 文件只作為
本次整併的輸入。

## Lifecycle 概覽

```
                      ┌──────────────┐
                      │   Healthy    │◄────────────────┐
                      └──────┬───────┘                 │
                             │ record_crash            │ §Exit.X1
                             ▼                         │ silence drops
                      ┌──────────────┐                 │
                      │  Recovering  │                 │
                      └──────┬───────┘                 │
                             │ recent ≥ 3              │
                             ▼                         │
                      ┌──────────────┐                 │
                      │   Unstable   │                 │
                      └──────┬───────┘                 │
                             │ total_crashes ≥ max     │
                             ▼                         │
                      ┌──────────────┐                 │
                      │    Failed    │                 │
                      └──────────────┘                 │
                                                       │
   ┌───────────────────────────────────────────────────┴──────────────┐
   │                                                                  │
   │  check_hang mutator monopoly (§Invariants 5b)                    │
   │                                                                  │
   │  silence > threshold ──┬── input pending past hb ──► §Entry.E1   │
   │                        │                                          │
   │                        ├── heartbeat fresh ──────► §Entry.E2     │
   │                        │                                          │
   │                        └── neither ──────────────► §IdleLong.E1  │
   │                                                                  │
   │  state ∈ {Hung, IdleLong}, silence drops below ─► §Exit.X1       │
   │                                                                  │
   └──────────────────────────────────────────────────────────────────┘

   ErrorLoop (separate state) — see §Open questions
```

## 範圍

範圍內的 state mutation（稽核如下）：

- `HealthState::Hung` Entry（E1、E2）與 Exit（X1）
- `HealthState::IdleLong` Entry（E1）與 Exit（X1，與 Hung 共用 predicate）

明確排除的範圍：

- `HealthState::Healthy / Recovering / Unstable / Failed / ErrorLoop`
  轉換——它們不是由 `check_hang` 驅動（見 §Invariants 5b），另有其他稽核。
- `AgentState`（位於 `src/state.rs`）——F39 evidence 位於該處，但本 scope
  只透過下方 §F39 cross-reference table 引用，不會修改它。

## 不變量

下列條件在 HEAD `2f24376` 成立，並由 decision
`d-20260513154400110972-2` 向前鎖定：

- **5a（entry 完整列舉）**——`HealthState::Hung` 恰有 **兩個** entry site；
  `rg "self\.state = HealthState::Hung" src/health.rs` 恰好回傳兩筆 match
  （`§Entry.E1` 與 `§Entry.E2`，兩者都在 `check_hang` 內）。不存在第三條
  entry path。

- **5b（mutator 獨占）**——`HealthState::Hung` 的每個 read/write 都位於
  `check_hang`。`maybe_decay`（`rg "fn maybe_decay" src/health.rs`）只修改
  `Failed → Recovering` 與 `Unstable → Healthy`；F10 已驗證。這表示稽核
  Hung 語意的 reader 只需要閱讀一個 function。

- **5c（wire-compatible external surface）**——Hung state 的 external
  consumer 是 `check_hang` 回傳的 bool（由
  `rg "check_hang" src/daemon/mod.rs` 驅動，唯一 consumer 是
  `tracing::warn!`），以及 `display_name()` 字串；後者由
  `rg "health_state" src/api/handlers/query.rs` 與
  `rg "health_state" src/mcp/handlers/instance.rs` 序列化。**沒有 external
  code 對 `HealthState::Hung` variant 做 pattern match。** 這表示後續
  sub-task（F9 / F10 / F39）只要維持 `check_hang -> bool` 與
  `display_name()` contract，就可以 wire-compatible 地調整 Hung 的內部語意。

- **5d（負面不變量——`maybe_decay` 不碰 Hung）**——F10 稽核已確認：
  `maybe_decay` 讀取 `last_crash.elapsed()`，而不是
  `last_output.elapsed()`。其 state mutation 僅限
  `Failed → Recovering` 與 `Unstable → Healthy`。**它絕不會退出 Hung。**
  Hung agent 會一直保持 Hung，直到 `check_hang` 本身觀察到 silence 降到
  threshold 以下（`§Exit.X1`）。這個負面不變量也重複寫入 `check_hang`
  function-level rustdoc augmentation，讓關心它的 audience 能就近看到。

## Entry 轉換

### §Entry.E1 — input pending 超過 heartbeat

- **在 source 中尋找**：`rg "Hung Entry \(E1\)" src/health.rs`
- **PRE**：
  - `self.current_reason` 是 `None`，或不屬於
    `{RateLimit, QuotaExceeded, AwaitingOperator}`（race mutex 未持有）
  - `silence_exceeds_threshold` 為 `true`（threshold 依 `AgentState`
    而異：預設 120s；`Thinking | ToolUse` 為 600s；`Idle` 永不觸發；
    `Starting` 為 120s）
  - `input_pending_past_response` 為 `true`：
    `last_input_at_ms > last_heartbeat_at_ms + INPUT_RESPONSE_GRACE_MS`
    （grace = 5_000 ms）
  - `self.state != HealthState::Hung`（第一次偵測會 latch state flip；
    後續 tick 直接 short-circuit）
- **POST**：
  - `self.state = HealthState::Hung`
  - `check_hang` 回傳 `true`（只在第一次偵測時——caller 會 escalate）
  - `tracing::warn!` 帶有 structured field
    `last_input_at_ms / last_heartbeat_at_ms / input_response_delta_ms / silent_secs / agent_state`
- **FP vector**——Operator 輸入使 `last_input_at_ms` 增加，但 agent 實際上
  正在產生由 MCP drain、卻尚未 flush 成可見 PTY output 的 keystroke。
  Heartbeat 語意對此設有界線：任何 MCP tool call 都會更新
  `last_heartbeat_at_ms`，把 delta 拉回 5s grace 以下。
- **FN vector**——F9 grey failure：agent 產生 1-byte output
  （spinner / log line / partial token）會重設 `StateTracker` 上游 silence
  timer，因此即使沒有實質工作，`silent` 也永遠不會越過 threshold。
  Productive-output detection 屬於 F9 sub-task；本稽核只記錄此缺口。

### §Entry.E2 — heartbeat fresh 但 PTY silent（F1 cross-check）

- **在 source 中尋找**：`rg "Hung Entry \(E2\)" src/health.rs`
- **PRE**：
  - `self.current_reason` race mutex 與 §Entry.E1 相同
  - `silence_exceeds_threshold` 為 `true`（threshold 與 §Entry.E1 相同）
  - `input_pending_past_response` 為 `false`（沒有 pending input；§Entry.E1
    未觸發）
  - `heartbeat_fresh` 為 `true`：`last_heartbeat_at_ms > 0` 且
    `heartbeat_age_ms < silent.as_millis()`——也就是 agent 最近呼叫過 MCP
    tools（更新 heartbeat），但沒有產生 PTY output
  - `self.state != HealthState::Hung`
- **POST**：
  - `self.state = HealthState::Hung`
  - `check_hang` 回傳 `true`
  - `tracing::warn!` 帶有 structured field
    `last_heartbeat_at_ms / heartbeat_age_ms / silent_ms / agent_state`
- **FP vector**——F39：vterm scrollback 中殘留的
  `AgentState::Thinking` pattern（regex 針對 rendered screen text，比對到
  已捲出畫面的文字後仍可能 latch）。`src/state.rs` 的
  `LATCHED_STATE_EXPIRY`（30s）會限制影響，但並不完美。見 §F39
  cross-reference。
- **FN vector**——與 §Entry.E1 相同的 F9；低於 threshold 的 output 會讓
  `silent` 保持在 trigger 以下。

## Exit 轉換

### §Exit.X1 — silence 降至 threshold 以下（recovery）

- **在 source 中尋找**：`rg "Hung Exit \(X1\)" src/health.rs`
- **PRE**：
  - `self.state in {HealthState::Hung, HealthState::IdleLong}`（共用
    predicate；一個 mutation site 同時服務兩種 state）
  - `!silence_exceeds_threshold`（任何 output，包括單一 byte，都會讓
    `silent` 降到該 `AgentState` 的 threshold 以下）
- **POST**：
  - `self.state = HealthState::Healthy`
  - `check_hang` 回傳 `false`
- **FP vector——F10 的旁支疑慮**——沒有 productive-work evidence
  requirement。**單一 byte 的 PTY output 就會把 Hung 翻回 Healthy**，
  即使它只是 TTY spinner tick，而不是進度。F10 sub-task 是 doc-only
  確認；會收緊此 exit predicate 的 productive-output gate 屬於 F9 sub-task。
- **FN vector**——沒有直接 FN；這是 recovery path。間接風險是：若
  §Exit.X1 誤觸發（F10），operator 可能基於過時的「Healthy」分類而認定
  真正 stuck 的 agent 沒有問題。

## IdleLong 轉換

`IdleLong` 用來區分「agent 因沒有人要求它做事而 silent」與「agent 因停止
回應 input 而 silent」（Hung）。04:00 UTC 的 false-alarm 模式促成此拆分。

### §IdleLong.Entry.E1 — silent 超過 threshold，沒有 pending input

- **在 source 中尋找**：`rg "IdleLong Entry \(E1\)" src/health.rs`
- **PRE**：
  - `self.current_reason` race mutex 與 §Entry.E1 相同
  - `silence_exceeds_threshold` 為 `true`
  - `input_pending_past_response` 為 `false`（沒有 input pending 超過 heartbeat）
  - `heartbeat_fresh` 為 `false`（heartbeat 比 silent duration 更舊）
  - `self.state != HealthState::IdleLong`
- **POST**：
  - `self.state = HealthState::IdleLong`
  - `check_hang` 回傳 `false`（依
    `rg "Returns .true. ONLY when transitioning" src/health.rs` 的 rustdoc
    contract，escalation consumer 只對 `Hung` 採取行動）
  - `tracing::debug!`（不是 `warn!`——不 escalation）
- **FP vector**——真正 idle、等待下一個 operator prompt 的 agent；此分類正確。
- **FN vector**——F9：與 §Entry.E1 / §Entry.E2 相同的形狀。

### §IdleLong.Exit.X1 — 與 §Exit.X1 共用

- **在 source 中尋找**：同一個 `rg "Hung Exit \(X1\)" src/health.rs`
  （`matches!(state, Hung | IdleLong)` predicate 是單一 mutation site）
- **PRE**：與 §Exit.X1 相同，但 `state` precondition 是
  `HealthState::IdleLong`
- **POST**：與 §Exit.X1 相同（`state = HealthState::Healthy`、
  `check_hang` 回傳 `false`）
- **FP / FN**：與 §Exit.X1 相同

## Productive-output 補充路徑（F9）

這是 F9 sub-finding（`#685` Phase 1 交付項目 #2 + #3）的維護中 contract：
用來補充 silence-based Hung detection 的 dual-path。本節與
`src/state/mod.rs`、`src/behavioral.rs` 及 `src/health.rs` 中的 F9 inline
structured comment 相互配套。

**目前 baseline**：已於 `main@1d83b423`（2026-07-16）重新驗證。Gate
預設仍為 shadow（`AGEND_PRODUCTIVE_GATE=1` 會啟用 classification）。Gemini
退役後，其 calibration 已重新命名為 Agy。Grok 是受支援的 backend，但目前
使用 generic marker/cache path；Grok 專屬的 F9 calibration 仍未驗證。

Issue：[#685](https://github.com/suzuke/agend-terminal/issues/685) F9
sub-finding。Sibling sub-task：1（Hung audit，PR #750）、2（F39 audit，PR
#752）、3（F39 speculative narrow，PR #763）。

Decision chain：
- `d-20260513154400110972-2`（sub-task 1 base——Hung invariants）
- `d-20260513161542381785-0`（sub-task 2 audit——F39 hypotheses）
- `d-20260513231713506833-1`（sub-task 3 speculative——F39 Gemini narrow）
- `d-20260513235514013631-0`（sub-task 4——F9 productive-output gate）

維護規則：section ID（`§F9.1`-`§F9.5`）是 contract anchor。重新命名時
必須在同一個 PR 傳播更新。沿用 Hung audit 的 M1/M2/M3 紀律：inline
comment 使用 `§F9.<n>` reference，本文件使用 `rg <pattern>` grep hint，
且 section heading 必須保持穩定。

### §F9.1 — 架構理由（dual-path supplement）

「以 productive-output detection 取代 silence_exceeds_threshold」這種直覺
說法是**錯的**。它會讓 `#659` 的 silent-stuck-in-thinking detection 退化——
若 process 真的完全停止輸出 PTY byte，classification 若特別等待
*productive* byte（它永遠不會來），就永遠不會觸發 Hung，agent 會一直留在
Hung 之前的 state。

F9 以 **dual-path supplement** 形式交付：

- **既有 silent path**（有任何 output 就視為 alive，採 threshold-based）
  保留在 `check_hang` 中。它會捕捉 agent 真正完全 silent 的 `#659` 情境。
- **Productive path** 作為補充：當 silence 低於 threshold（agent 正在產生
  *某些* output），但另一個 threshold 期間內都沒有 *productive* output，
  就把 agent 標為 Hung candidate。它捕捉 F9 grey failure：1-byte spinner
  output 不斷重設上游 silence timer，卻沒有任何實際工作。
- 任一條 path 觸發時，`check_hang` 都會回傳 `true`；兩者的 union 只會
  **增加** coverage，不會移除既有 coverage。

F9 運作於 **HealthState** classification 層。Sibling F39(c) hypothesis
（§F39.4）則運作於 **AgentState** 層（`Thinking` pattern stickiness）——
它們是不同 concern，也有不同 bug surface。

### §F9.2 — Productive signal 設計

下列任一條件成立時，signal 即為 **productive**：

1. **MCP heartbeat** 最近有更新（`use_heartbeat: true` config）。Heartbeat
   refresh 表示 agent 呼叫過 MCP tool，是具體的 forward-progress evidence。
   此訊號適用所有 managed backend。
2. **Structural marker** 與 rendered screen text match。Marker 使用
   **line-start anchor 與特定格式**，而不是 bare keyword。Bare-keyword
   作法（`Saved` / `Wrote`）會遇到與 F39 audit Scenario A/B/C taxonomy
   相同的 scrollback FP surface。

所有 backend 共用的 generic structural anchor（file save banner）：
- `^Saved to \S+`——file save banner
- `^Wrote \d+ bytes`——明確 byte count
- `^Created file: \S+`——structured creation

Per-backend completion marker 由 `#685` sub-task 6（交付項目 #4，decision
`d-20260514022917793418-0`）交付。每個 backend 的 `MARKERS` const 都列出
上述 generic anchor，再加上自己的 completion-glyph + tool-vocabulary
regex。可用 `rg "<BACKEND>_PRODUCTIVE_MARKERS" src/behavioral.rs` 查找：

| Backend | Completion regex（加入 generic anchor） | Source | 驗證狀態 |
|---|---|---|---|
| Claude | `^[✓●⏺]\s+(Read\|Bash\|Edit\|Write\|Grep\|Glob\|Listing\|Reading\|Writing\|Searching\|Editing)\b` | `src/behavioral.rs` 的 `CLAUDE_PRODUCTIVE_MARKERS` | F685 fixture `f685-f9-positive-savedfile.raw`（synthetic）。Real capture 尚待 corpus growth。 |
| Kiro | `^●\s+(Read\|Write\|Edit\|Bash\|Grep\|Glob\|Task\|List\|Search)\b` 加上 `\[(fs_read\|fs_write\|execute_bash)\]` | `src/behavioral.rs` 的 `KIRO_PRODUCTIVE_MARKERS` | 只有 synthetic unit test——`src/behavioral.rs` tests 中的 `kiro_markers_*`。**尚未用 real capture 驗證**——請使用 fixture capture playbook。 |
| Codex | `^•\s+(Explored\|Edited\|Ran)\b` 加上 `apply_patch` | `src/behavioral.rs` 的 `CODEX_PRODUCTIVE_MARKERS` | **只有 synthetic——尚未用 real capture 驗證。** 請使用 fixture capture playbook。 |
| Agy | `^✓\s+(ReadFile\|WriteFile\|ReadManyFiles\|Edit\|Shell\|WebFetch\|Glob\|GoogleSearch\|MemoryTool\|ReadFolder)\b` | `src/behavioral.rs` 的 `AGY_PRODUCTIVE_MARKERS` | 繼承已退役 Gemini engine 的 calibration；目前 Agy 的 real-capture validation 仍不完整。 |
| OpenCode | `^→\s+(Read\|Write\|Edit\|Glob\|Grep\|Bash\|List\|Task)\b` | `src/behavioral.rs` 的 `OPENCODE_PRODUCTIVE_MARKERS` | **只有 synthetic——尚未用 real capture 驗證。** 請使用 fixture capture playbook。 |
| Grok | 只有 generic save-banner anchor | `src/backend_profile.rs` 的 `grok_profile()` 透過 `GENERIC_PRODUCTIVE_MARKERS` | **Backend-specific coverage 未驗證**；沒有 Grok-labelled corpus fixture。 |

**F9 marker 明確排除**：
- 所有 in-progress / spinner glyph（Braille `[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]`、
  OpenCode `✱`、Codex `◦ Working`、歷史 Gemini `⠦ Thinking`）。F9 的
  productivity 只認 completion；這些 glyph 會在工作完成前出現。
- Agy/Gemini-engine 的 `tool.*call` / `MCP.*tool` literal——heartbeat path
  已涵蓋 MCP signal，重複計入會造成 evidence double counting。
- Bare keyword marker（例如沒有 line-start anchor 的 `Saved` / `Wrote`）。
  像「I saved your time」這類 prose 不得 match。此 contract 由舊有
  `infer_productivity_rejects_bare_keyword_scrollback` 與各 backend 的
  `<backend>_markers_reject_*` test 固定。

#### Cache routing

Per-backend marker **不會**以 pointer equality routing。那會落入每次呼叫
都執行 `Regex::new()` compile 的路徑——正是造成 PR #766 Ubuntu/Windows
CI failure 的 bug。Sub-task 6 在 `ProductivityConfig` 上引入
`MarkerCacheId` enum：

```rust
pub enum MarkerCacheId { Generic, Claude, Kiro, Codex, Agy, OpenCode }

pub struct ProductivityConfig {
    pub markers: &'static [&'static str],
    pub use_heartbeat: bool,
    pub heartbeat_fresh_window_ms: u64,
    pub cache_id: Option<MarkerCacheId>,
}
```

`infer_productivity` 對 `cache_id` 做 match，把它 routing 到相應的
per-backend `LazyLock<Vec<Regex>>` static（`CLAUDE_PRODUCTIVE_REGEXES`
等）。Compile-time exhaustive match 能避免遺漏 backend 的 bug。`None`
保留給 ad-hoc test config，會退回每次呼叫都執行 `Regex::new()`；Phase 1
production code 永遠不會進入此路徑。

未來的 per-backend `heartbeat_fresh_window_ms` tuning 與 per-backend
silence threshold tuning，在 corpus measurement data 足以支持 calibration
前都不在範圍內（見 §F9.5）。

### §F9.3 — Dual-path decision table

| `silent` | `silent_productive` | `agent_state` | 預設模式 | `AGEND_PRODUCTIVE_GATE=1` |
|---|---|---|---|---|
| ≤ threshold | ≤ threshold | any | 非 Hung | 非 Hung |
| > threshold | any | non-Idle | discriminator（既有） | discriminator（既有） |
| ≤ threshold | > threshold | non-Idle | 非 Hung + telemetry | discriminator（新路徑） |
| any | any | `Idle` | 非 Hung（Idle 永不 hang） | 非 Hung（Idle 永不 hang） |

「discriminator」是 `check_hang` 既有的 input-pending-past-heartbeat /
heartbeat-fresh / IdleLong branch。任一 path 觸發後，都會進入同一個
discriminator。F9 新增的是 entry condition，不是新的 discriminator branch。

Source 中的 threshold mapping 可用
`rg "silence_exceeds_threshold" src/health.rs`（silent path）與
`rg "productive_silence_exceeds" src/health.rs`（F9 path）尋找。兩者目前
使用相同的 per-`AgentState` threshold。

### §F9.4 — 已知限制（須由 fixture corpus 測量）

#### 4.1 Heartbeat-as-productive 缺口

長時間的 pure-reasoning session（例如 Claude 沒有 MCP tool call 的內部
thinking）既不會更新 heartbeat，也沒有 productive marker。一旦設定
`AGEND_PRODUCTIVE_GATE=1`，這類 session 即使仍在進行合法工作，也會在
threshold 後被標記為 Hung。

**延後的 mitigation**：
- 後續整合 spinner-cycling-as-productive（F39 hypothesis (e) 的 variant——
  pattern-source-line tracking，但反向用來把 spinner-glyph activity 視為
  evidence）。
- Operator override mechanism（不在 F9 scope）。

**風險已被限制**：shadow mode 加 env-var opt-in 代表 rollout 期間只有主動
opt in 的使用者會遇到此 FN。啟用受 fixture-corpus measurement gate
（§F9.5）約束。

#### 4.2 Generic marker 的 FP residual

即使有 structural anchor，仍存在 edge case。使用者可能在 chat message
貼上 literal `Saved to /tmp/foo`，而 agent echo 該 input，因而 match marker。
Negative test `infer_productivity_rejects_bare_keyword_scrollback` 固定了
anchor 作法；它大幅縮小 surface，但無法完全消除。

**已套用的 mitigation**：line-start anchor（`^`）加上特定格式。Test
同時固定 positive（real marker 會 match）與 negative（bare prose 不會
match）contract。

**已確認的 residual risk**：持續用 real captured fixture 測量。

#### 4.3 Cross-backend pattern 一致性

Phase 1 對所有 backend 交付同一組 generic marker。後續 calibration
加入 Kiro `[fs_read]` 與 Agy tool banner（由已退役的 Gemini engine
重新命名）。Grok 目前仍使用 generic profile，因此若 Grok 專屬進度訊號
不同於這些 anchor，在 real-capture calibration 落地前，其 F9 sensitivity
會較低。

### §F9.5 — 啟用 gate（shadow → opt-in → promotion）

F9 productive-silence telemetry 一律觸發
（`rg "F9 dual-path candidate" src/health.rs`）。**Classification** 由
`AGEND_PRODUCTIVE_GATE` env var 控制：

```
unset / not "1"   → shadow mode (default): telemetry collected,
                    no Hung classification from productive path
"1"               → active mode: productive-silence path can flag Hung
```

**防止 dead infra 條款**：Sprint 27 PR-A behavioral telemetry 以 shadow
mode 交付後從未 promotion。因此 productive-output path 保留明確的 promotion
criteria：

1. **Fixture corpus 測得 FP rate < 1%，且 non-stuck fixture 的 N ≥ 300**
   （Rule-of-Three statistical minimum；原始 `#685` wording 使用較小的
   3+ case floor）。使用
   [PTY fixture capture playbook](../tests/fixtures/state-replay/CAPTURE-RECIPES.zh-TW.md)
   capture 並擴充 corpus，再執行 `tests/fixture_corpus_measurement.rs`
   的 measurement，採 per-transition、source-separated reporting。
2. **為期 2 週的 shadow-mode telemetry 顯示 behavioral divergence 穩定**
   （Sprint 27 PR-A divergence-dashboard pattern，可用
   `rg "behavioral_shadow" src/behavioral.rs` 找到）。
3. **Operator decision 決定翻轉 env-var default**，另開 PR 將
   `check_hang` 的預設值從 unset 改為 `"1"`，或在 promotion 後完全移除 gate。

三項未全數達成前，gate 維持預設關閉。若超過 6 週仍未進行 measurement，
F9 path 本身會成為 removal candidate；dead shadow infrastructure 比完全沒有
infrastructure 更糟。

#### 5.1 Cross-reference

- §F39.4 hypothesis (c) footnote——F39(c) `AgentState` 與 F9
  `HealthState` 的 layer distinction。
- §Invariants 5b/5c——F9 保留 `check_hang -> bool` return contract 與
  `display_name()` string contract。新增的 `silent_productive: Duration`
  parameter 是 internal-API refactor；唯一 production caller 是
  `src/daemon/per_tick/hang_detection.rs`（`rg "check_hang(" src/daemon`）。
- `src/behavioral.rs`——`ProductivitySignal`、`ProductivitySource`、
  `ProductivityConfig`、`config_for_productivity`、`infer_productivity` 與
  `log_productivity_telemetry`。歷史上它們與 Sprint 27 PR-A 的 silence-side
  對應物（`BehavioralSignal` / `infer_from_silence` /
  `log_shadow_telemetry`）平行；後者已在 #2547 因 dead code 移除，而
  `BehavioralConfig` 因 `backend_profile.rs` 仍在使用所以保留。
- [RECOVERY-STAGES.zh-TW.md](RECOVERY-STAGES.zh-TW.md)——目前僅含 Stage 1
  的 recovery dispatcher 直接讀取 `productive_silence_exceeds`，用來選擇
  Stage 1 alive-stuck 或 dead-likely branch。無論 F9 promotion state 為何，
  recovery 對所有 `Hung` source 一視同仁；見 §RS.4。

## §F39 — AgentState Thinking Pattern Stickiness（cross-audit：AgentState，不是 HealthState）

本節是 **cross-audit boundary**：§F39 記錄 `src/state.rs` 中
`AgentState::Thinking` pattern 的語意。這些 pattern 會作為 input signal
餵給 `check_hang`，但本身不是 `HealthState` mutator。之所以把 F39 納入
Hung-state audit，是因為 `AgentState::Thinking` pattern 會影響 §Entry.E2
的 precondition path（heartbeat-fresh + PTY-silent classification），因此
stale-pattern false positive 會傳播至 Hung detection。

Scope：pattern stickiness audit；可能的 mitigation 只列為 **hypothesis**
（沒有 FP-rate data——仍待 `#685` 交付項目 5 的 fixture-corpus validation）。
任何 mitigation 的 implementation 都嚴格不在範圍內。

Sibling decision：`d-20260513161542381785-0`（N 個 sub-task 中的第 2 個）。

### §F39.1 — 各 backend 的 pattern

`AgentState::Thinking` 透過 `src/state.rs` 中的 regex pattern catalog，
依 backend 分別 match。Pattern 只屬於單一 backend（`StateTracker::new`
期間，以 `Backend` enum variant 作為 state pattern lookup 的 key），因此
cross-backend contamination 必須先發生 backend detection 錯誤——見 §F39.5
的 cross-backend overlap。

| Backend | Pattern | 在 source 中尋找 | Source evidence | 歷史 |
|---|---|---|---|---|
| Kiro (kiro-cli) | `r"Kiro is working\|esc to cancel"` | `rg "Kiro is working" src/state.rs` | pattern line 上方的 `[measured]` comment | Sprint 34 PR-1（generation 期間顯示 `Kiro is working`） |
| Gemini (gemini-cli) | `r"esc to cancel"` | `rg "esc to cancel" src/state.rs` | pattern 附近的 `[measured]` comment | 最初是 bare `r"Thinking"`——已縮窄至 `esc to cancel` 以減少 stale match。進一步縮窄（例如要求 leading Braille spinner `⠦`）是待另一個後續 PR 評估的 quick-win candidate，**不在**本 audit 內。 |

Cross-backend overlap：literal substring `"esc to cancel"` 同時出現在 Kiro
與 Gemini pattern。因為 pattern catalog 依 backend 分隔，只要 backend
detection 正確，這就是 benign。若 `Backend::from_command` routing 錯誤
（例如不熟悉的 binary name），所用 catalog 會是 `None`（Shell/Raw
fallback），它**沒有 Thinking pattern**；因此 cross-contamination 必須是
主動誤 route 到另一個 managed backend。本 audit 不處理此範圍——見 §F39.5。

### §F39.2 — LATCHED_STATE_EXPIRY 語意

```rust
const LATCHED_STATE_EXPIRY: Duration = Duration::from_secs(30);  // src/state.rs
```

Expiry 透過 `maybe_expire_latched_state`
（`rg "fn maybe_expire_latched_state" src/state.rs`）與 active-state
hysteresis 互動：當 `current` 是會自行 expiry 的 active state
（`Thinking | ToolUse`），且
`since.elapsed() >= LATCHED_STATE_EXPIRY`，tracker 會轉換至 `Ready`。
Fallback 由兩個 call site 觸發：

1. `feed()` non-match branch（`rg "maybe_expire_latched_state" src/state.rs`——
   第一個 call site，約在 line 759）——screen 已改變但沒有 pattern match 時，
   fallback 會移除 stale latched state。
2. `tick()` periodic supervisor call（`rg "fn tick" src/state.rs`——第二個
   call site，約在 line 843）——即使沒有 PTY output 也會執行，涵蓋先前
   incident「`dev-reviewer 卡在互動 prompt`」中的「screen frozen at
   dismissed prompt」情境。

兩個 call site 都依賴 `since` 確實經過 LATCHED_STATE_EXPIRY。Scenario C
bug（§F39.3）的原因是 `since` 會在越過 threshold 前持續被 priority
oscillation 重設。

### §F39.3 — Scenario taxonomy A/B/C（核心）

「scrollback pattern 重新 match → `since` 重設 → expiry 永不觸發」這個直覺
說法是**錯的**。兩個既有 guard 會阻止單純 re-match path 破壞 expiry：

- `feed()` hash-dedup（`rg "last_screen_hash" src/state.rs`）——若 rendered
  screen hash 未改變，`feed()` 會在進入 `detect()` 前 short-circuit。
  相同 hash ⇒ 看得到相同 pattern ⇒ 不會 spurious re-detect。
- `transition(same_state)` early return
  （`rg "if new_state == self.current" src/state.rs`）——若 `detect()` 回傳
  目前已經處於的相同 state，`transition()` 會 short-circuit，不動 `since`。

這兩個 guard 正確處理 Scenario A 與 B。Scenario C 才是真正的 bug surface。

**Scenario A——pattern 在 scrollback、screen static（正常）**

Agent 處於 `Thinking`。Active spinner 停止 rendering，但 `esc to cancel`
文字仍留在 frozen screen。各 tick 的 screen hash 都沒有改變，因此
`feed()` 在 hash-dedup gate short-circuit。`detect()` 不會執行；`since`
仍是最初 transition 的 timestamp。經過 `LATCHED_STATE_EXPIRY` 後，
`tick()` 觸發 `maybe_expire_latched_state` → transition 至 `Ready`。
**沒有 bug。**

註——screen resize：terminal resize 會強制 vterm buffer realloc，即使語意
沒變也會改變 screen hash。這會重新觸發 `detect()`；但若 pattern 仍 match
（相同文字內容，不同 layout），結果是 Scenario B——同樣已正確處理。
沒有 pattern-text 變動的 resize 仍等同 Scenario A。

**Scenario B——screen 改變，state pattern 不變（正常）**

Agent 處於 `Thinking`。新內容捲入，但 `esc to cancel` 仍可見。Hash 改變，
`detect()` 執行並回傳 `Thinking`，但 `transition(Thinking)` 因
`new_state == self.current` early-return。`since` 不變，`tick()` 最終仍會
觸發 expiry。**沒有 bug。**

**Scenario C——conflicting pattern 下的 priority oscillation（故障）**

順序如下（數字只供說明；任何會 oscillate 的 priority pair 都有相同行為）：

```
t=0s   agent enters Thinking (priority 6); since=0
t=10s  spinner clears, shell prompt `❯` becomes visible
       detect() returns Idle (priority 4)
       transition(Idle): priority-down + held >= 2s active min_hold
         → state=Idle; since=10s
t=15s  screen scrolls; `esc to cancel` re-enters viewport
       detect() returns Thinking (priority 6)
       transition(Thinking): higher priority always wins, instant
         → state=Thinking; since=15s   ← `since` reset by bounce
t=25s  agent action clears spinner; `❯` again
       transition(Idle) → state=Idle; since=25s
t=40s  scroll triggers `esc to cancel` re-detection
       transition(Thinking) → state=Thinking; since=40s
...
```

每次轉換到不同 state 都會重設 `since`。因為連續 bounce 不斷讓 `since`
保持在近期，30s 的 `LATCHED_STATE_EXPIRY` predicate
`since.elapsed() >= 30s` 永遠無法維持足夠久而觸發。對 upstream consumer
而言，agent 會無限期顯示為 Thinking，包括 `check_hang` 的 §Entry.E2 path。

**精確機制**：每次 bounce 都由 priority oscillation 重設 `since`。
（本文刻意不使用「afterglow」一詞；該詞暗示 signal 逐漸 decay，但真正的
bug 是 `since=now` 被重設，不是 decay。）

### §F39.4 — 待驗證的可能 mitigation

**沒有 FP-rate data。這些只是交給 fixture corpus validation（`#685`
sub-task 5）的 hypothesis，不是 recommendation。**

| Hypothesis | 說明 | 所需 measurement |
|---|---|---|
| (a) Cursor-anchored / viewport-only | 只對最後 N rows 或可見 viewport 比對 pattern；完全排除 scrollback row | 計算 corpus 中 mitigation 前後的 Scenario C bounce 數；**feasibility check**：macOS / Linux / Windows ConPTY 上 portable-pty / vterm cursor-position API surface（open question §F39.5） |
| (b) Recent-output-bytes gate | 對最近 K 次 `feed()` call 收到的 byte（累積 buffer slice）比對，而不是完整 rendered screen | 測量各 backend 的 output rate distribution；選擇 K，讓合法 Thinking match 維持在 threshold 以上 |
| (c) Co-required negative pattern¹ | 只有 `esc to cancel` 存在且 prompt indicator（例如 `❯`）不存在時，`Thinking` 才有效 | 計算 corpus 中 spinner 可見時的 Thinking→Idle transition |
| (d) Oscillation-detection min-hold extension | Counter 偵測 N 秒內有 ≥2 次觸及同一 state 的 transition → 在允許後續 transition 前，把 `min_hold` 延長為 N × K 秒 | 測量 corpus 中的 oscillation frequency |
| (e) Pattern-source-line tracking | `detect()` 回傳 match row index；scrollback row（viewport top 以上）產生「stale」verdict 並跳過 `transition()` | 測量各 pattern 的 scrollback-vs-viewport match rate |
| (f) Per-pattern / dynamic `LATCHED_STATE_EXPIRY` | 每個 pattern 使用自己的 expiry 值（`Thinking` 較短），或在 current state 持續 > 2× typical duration 時動態縮短 | 測量各 backend 的 typical Thinking duration；找出 outlier |

**不同 lever——(d) 與 (f)**：(d) 延長 `min_hold`（位於
`rg "min_hold" src/state.rs` 的 priority transition gate），讓 oscillation
更難發生；(f) 縮短 `LATCHED_STATE_EXPIRY`，讓 latched state 更早 expiry。
兩者可以彼此獨立組合。

¹ Hypothesis (c) variant——把 Gemini Thinking pattern 從
`r"esc to cancel"` 縮窄為 `r"\(esc to cancel,"`——曾在 PR #763
（decision `d-20260513231713506833-1`）speculatively 套用。這會減少
scrollback 中 stale `esc to cancel` 文字造成的 FP，且不需要 co-pattern
gate。待 fixture corpus data 可用時，重新評估完整的 (c) hypothesis。

**F9 layer distinction**：此 hypothesis 位於 `AgentState` layer
（`src/state.rs` 的 `Thinking` pattern stickiness）。F9 productive-output
gate（§F9.1–§F9.5，decision `d-20260513235514013631-0`）則位於
`HealthState` layer（`src/health.rs::check_hang` 中的 `Hung`
classification）。兩者**不重疊**：F39 mitigation 調整哪些
`AgentState::Thinking` transition 會觸發；F9 則新增與 `AgentState`
無關的平行 `HealthState::Hung` classification path。修正其中一層不會
涵蓋另一層的修正。

**已拒絕**：screen-hash change 時由 tick 強制 recheck——`tick()` 已經
定期呼叫 `maybe_expire_latched_state`（`rg "fn tick" src/state.rs`），
且無論 caller 為何，底層的
`since.elapsed() >= LATCHED_STATE_EXPIRY` check 都完全相同。這無法處理
Scenario C 的 `since` reset 機制。

### §F39.5 — Open question

- **F9 / F39 interaction warning**：F9 productive-output signal（另一個
  sub-task）若使用 PTY pattern matching 作為 evidence，也會繼承
  Scenario A/B/C surface。F9 sub-task 從第一天起就必須考慮 scrollback
  staleness；同一套 A/B/C taxonomy 也適用。

- **Fixture corpus Scenario C capture acceptance criteria**：fixture corpus
  sub-task（`#685` 交付項目 5）必須包含 `AgentState` 在 `Thinking` 與
  非 `Thinking` state（`Idle`、`Ready` 等）之間交替的 trace，而且在
  **30 秒內至少 3 次**，同時 `esc to cancel`（或其他 Thinking-pattern
  substring）全程都在 scrollback 中可見。若沒有 Scenario-C-specific
  capture，hypothesis (a)–(f) 的 FP-rate measurement 就無法區分「真正
  處理此 bug 的 fix」與「只遮蔽 Scenario A、B 的 fix」。

  **更新（sub-task 5 已交付）**：corpus infrastructure 已落地。Scenario C
  measurement 本身仍 deferred——replay test 在 microseconds 內完成，因此
  wall-clock-based `min_hold` threshold 在 byte-only replay 中永遠不會
  跨過。仍需擴充 time-injection harness；請以
  [PTY fixture capture playbook](../tests/fixtures/state-replay/CAPTURE-RECIPES.zh-TW.md)
  收集新 trace。

- **Cross-backend pattern overlap**：Kiro
  `r"Kiro is working|esc to cancel"` 與 Gemini `r"esc to cancel"` 共用
  literal `"esc to cancel"`。State pattern 依 backend 分隔
  （`StateTracker::new(Some(&backend))` → backend-specific catalog），
  因此只要 `Backend::from_command` routing 正確，這就是 benign。若 backend
  mis-detect 而 routing 到錯誤 catalog，可能發生 silent latching 到錯誤
  state。此驗證不在範圍內，但對 F9 / mitigation design 值得記錄。

- **缺少 Scenario C unit test**：
  `rg "oscillation|bounce" src/state.rs` → tests 中 0 hits。現有 test
  涵蓋 happy-path `LATCHED_STATE_EXPIRY`
  （`rg "fn feed_fallback_expires_thinking" src/state.rs`），但沒有涵蓋
  priority oscillation。任何 mitigation sub-task 落地時，都要加入
  Scenario-C-specific unit test。

- **Cursor-anchored feasibility check**：hypothesis (a) 依賴所有支援平台
  （macOS、Linux、Windows ConPTY）上 `portable-pty` 與 `VTerm` abstraction
  的 cursor-position API surface。任何 implementation PR 前都先驗證
  availability；若 Windows ConPTY 無法可靠暴露 cursor position，
  hypothesis (a) 就無法跨平台實作。

## F9 / F10 後續 scope cross-reference

F9 row 現由本 contract 的 §F9.1–§F9.5 維護。此表保留作為
transition-to-finding map。

| Finding | 受影響的 transition | Sub-task scope |
|---|---|---|
| F9（productive-output gate） | §Entry.E1 FN、§Entry.E2 FN、§IdleLong.Entry.E1 FN、§Exit.X1 FP | §F9.1–§F9.5 維護 dual-path productive-output signal 與 activation gate；變更仍限於 `StateTracker` 及／或 `check_hang` predicate 的內部。 |
| F10（doc-only confirmation） | §Exit.X1 FP | 確認 `maybe_decay` 確實不影響 Hung（evidence 是本 audit 的 §Invariants 5b/5d），且 §Exit.X1 是唯一 recovery path。Doc-only sub-task。 |

## Open question（供 Phase 2 / 未來 sub-task）

- **ErrorLoop entry 沒有 exit**——
  `rg "HealthState::ErrorLoop" src/health.rs` 回傳一個 entry site
  （位於 `record_error`），但未觀察到
  `HealthState::ErrorLoop → Healthy` exit transition。值得另作稽核；
  不在 Hung audit scope。
- **Fixture corpus design**——Phase 1 交付項目 #5（重播 #659 及其他
  stuck-thinking incident 的 captured fixture）是另一個 sub-task。
  Acceptance criterion：依 issue，FP < 1% / FN < 10%。
- **Backend-specific tuning hook**——Phase 1 交付項目 #4
  （kiro/gemini 可能需要不同於 claude 的 threshold）屬另一個 sub-task。
- **Stage-1 / Stage-2 / Stage-3 recovery design**——#685 Phase 2，依 issue
  受 feature flag 與 operator 預設「warn-only」約束。
  **更新（sub-task 7a 已交付）**：Stage 1 ESC interrupt infrastructure
  已交付——`src/daemon/per_tick/recovery_dispatcher.rs` +
  `RecoveryStageState` state machine + `HealthState::Paused` variant +
  env-var-gated、預設 shadow mode。Stage 2、3 的後續 sub-task 曾重用同一個
  dispatcher tick 與 state machine。完整 lifecycle 與 promotion criteria
  請見 [RECOVERY-STAGES.zh-TW.md](RECOVERY-STAGES.zh-TW.md)。
  **更新（#2549）**：Stage 2/3（以及 dispatcher 驅動的 Stage 3 arm）
  後來已移除——收斂為只有 Stage 1。理由請見
  [RECOVERY-STAGES.zh-TW.md](RECOVERY-STAGES.zh-TW.md) 的 header banner。

## Consumer 稽核

在原始 `2f24376` audit baseline，§Invariants 5c 記錄了下列 surface。
為避免把歷史結論誤認成 live behavior，先列目前 consumer：

- **目前 transition consumer**：`HangDetectionHandler::run` 呼叫
  `check_hang` 並記錄 transition；對符合條件的 self-orchestrator，也會
  persist Hung-entry/exit escalation anchor
  （`src/daemon/per_tick/hang_detection.rs`）。
- **目前 state consumer**：`RecoveryDispatcherHandler::run` 在同一 tick
  與後續 tick 讀取 `core.health.state == Hung`。只有
  `AGEND_AUTO_RECOVERY_STAGE1=1` 時才能送出 Stage-1 ESC；預設 shadow mode
  只記錄 decision，不做 PTY I/O。`RespawnWatchdogHandler` 擁有另一條
  resume-spawn failure path，且可以進入 `Paused`。
- **Display projection**：`health.state.display_name()` 會序列化給 API、
  MCP、snapshot 與 UI consumer。應把此字串視為 projection，不是 mutation
  authority。

舊有 grep 結果（在 `2f24376`，`src/health.rs` 外沒有
`HealthState::Hung` consumer）只保留作為 **pre-recovery baseline**；在目前
source 上，預期它不成立。
