[English](RECOVERY-STAGES.md)

# Recovery Stage

`#685` staged auto-recovery dispatcher 的權威來源。

> **目前狀態（已於 `main@1d83b423`、2026-07-16 重新驗證）：** live
> dispatcher 只保留 Stage 1，且預設仍為 shadow。在 canonical per-tick
> 順序中，它位於 `HangDetectionHandler` 與
> `BackendExitDetectionHandler` 之後。Shadow mode 不做 PTY I/O，但仍會
> 記錄 `Stage1Pending` 與 `last_stage1_fired_at`，作為 one-shot re-fire
> guard。§RS.9 與 §RS.10 只屬歷史紀錄。

**#2549 P2 更新（operator decision `d-20260703021554626467-13`）：**
Stage 2（auto-restart）與 dispatcher 驅動的 Stage 3 escalation path
已**移除**——收斂為**只有 Stage 1**。下方 `§RS.9`（Stage 2）與
`§RS.10`（Stage 3 dispatcher side）保留作為歷史 decision record，記錄
sub-task 7b/7c 曾交付的內容及理由，但不再描述 live code——請看各 section
的 banner。`HealthState::Paused` / `HealthTracker::enter_paused` /
`RecoveryStageState::Stage3Pending` 本身**仍在使用**（`§RS.7`、
`§10.2`-`§10.7` mechanics）：它們是共用的 terminal-escalation machinery，
目前也由 `RespawnWatchdogHandler` 獨立使用（另一種 failure mode），不再
專屬於此 dispatcher ladder。完整理由請見
`src/daemon/per_tick/recovery_dispatcher.rs` 的 module doc（為何在 #2549
前 default gate 已關閉的條件下，此刪除是 behavior-preserving，而非擴大
scope）。

原始 `#685` sub-task 7a 交付 **Stage 1 ESC interrupt** 與完整
infrastructure（state machine、env-var gate、anti-thrash cooldown、
telemetry pattern）。

Decision：`d-20260514030404021793-1`（三方共識：lead-claude +
dev-claude + reviewer-opencode）。

Sibling chain：sub-task 1（PR #750）+ 2（#752）+ 3（#763）+ 4（#766）+
5（#769）+ 6（#770）。Stage 2（7b）與 3（7c）曾是 `#685` 的後續
sub-task，在重用本 module infrastructure 的基礎上新增 dispatch arm——
之後已依 #2549 移除（見上方）。

維護規則：section ID（`§RS.1`-`§RS.10`）是穩定 contract anchor，遵循
sub-task 1 建立的 M1/M2/M3 紀律。

## §RS.1 — 為何採 staged auto-recovery

在此 sub-task 前，`check_hang`（sub-task 1）偵測到 `Hung` 時，daemon
只會發出一次 `tracing::warn!("hang detected")`，不採取任何行動。
Operator 必須在 agent 的 TUI pane 手動按 ESC 才能 recovery。Issue
`#685` Phase 2 要求 staged automation：

- **Stage 1**：daemon 把 ESC byte 寫入 PTY（模擬 operator ESC）
- ~~**Stage 2**：Stage 1 未 recovery → auto-restart agent + telegram
  警告 operator~~——**已於 #2549 移除**，見上方 banner。
- ~~**Stage 3**：Stage 2 失敗 N 次 → pause + telegram escalate +
  標記待人工調查~~——作為 dispatcher-driven arm 已於 **#2549 移除**；
  `Paused` / `enter_paused` 本身仍作為共用 machinery 存在（`§RS.7`）。

每個 stage 都由 env var gate 控制，operator 預設為 `warn-only`。

## §RS.2 — Lifecycle（state machine）

**目前狀態（#2549 後）：**

```rust
pub enum RecoveryStageState {
    None,
    Stage1Pending { entered_at: Instant },
    Stage3Pending { entered_at: Instant },
}
```

它由 `HealthTracker` 內部攜帶，讓 dispatcher 能在一個 per-agent lock 下
同時讀取 `HealthState` 與 stage progression。`Stage3Pending` 只會透過
`HealthTracker::enter_paused` 到達——目前由 `RespawnWatchdogHandler`
獨立觸發，而不是經由已移除的 `Stage3Eligible` waiting-room state。

```
                      ┌──────┐
                      │ None │◄────── spontaneous recovery
                      └──┬───┘        (HealthState::Healthy)
                         │
        HealthState::Hung + alive-stuck branch
                         ▼
              ┌─────────────────┐
              │ Stage1Pending   │── Stage 1 timeout / dead-likely / cooldown:
              └─────────────────┘   log-only, terminal (#2549 — see below)

              ┌─────────────────┐
              │ Stage3Pending   │── reached via a DIFFERENT handler
              │   { entered_at }│   (RespawnWatchdogHandler's own
              └─────────────────┘   enter_paused call), not from
                                     Stage1Pending in this dispatcher.
```

Sub-task 7a（Stage 1）實作後，**#2549 後的目前形狀**：
- `None → Stage1Pending`（alive-stuck branch，包括 PTY-write-failure——
  停在 one-shot「已嘗試，停止」marker，不重試）
- `None → None`（dead-likely branch 或 cooldown skip——只寫 log，不轉換；
  #2549 前是 `None → Stage2Eligible`，現已移除）
- `Stage1Pending → Stage1Pending`（Stage 1 timeout 到期——只寫 log，
  每個 tick 都重新記錄；#2549 前是 `→ Stage2Eligible`）
- `* → None`（在 `Healthy` 上 spontaneous recovery）

**Sub-task 7b（Stage 2）——已於 #2549 移除。** 曾經實作的內容保留在
下方 `§RS.9` 歷史紀錄（banner 標示為非現行）。

**Sub-task 7c（Stage 3）的 dispatcher-driven arm——已於 #2549 移除。**
歷史紀錄保留於下方 `§RS.10`。底層
`HealthTracker::enter_paused` / `Stage3Pending` / `HealthState::Paused`
mechanics 未變且仍 live——見 `§RS.7` 與 `§10.2`-`§10.7`。

## §RS.3 — Tick 順序與 dispatcher 位置

Dispatcher 是 `src/daemon/per_tick/mod.rs::build_default_handlers` 的
**第三個** entry，位於 `HangDetectionHandler` 與
`BackendExitDetectionHandler` 之後。順序很重要：

1. `HangDetectionHandler` 執行 `check_hang` → 可能把
   `core.health.state` 轉換為 `Hung`（sub-task 1 §Invariants 5b——唯一
   mutator）。
2. `BackendExitDetectionHandler` 檢查 foreground/backend identity。Backend
   一致時 `Hung` 不變；identity mismatch 持續存在時，agent 會重新分類為
   `Unhealthy`，因此 recovery dispatcher 會正確跳過這個不同的 failure mode。
3. `RecoveryDispatcherHandler` 隨後讀取最新的 `core.health.state` 值。
   Agent 持續 `Hung` 時，後續 tick 讀取同一 state；dispatcher **不會**
   subscribe `check_hang` 的 `bool` return（依 sub-task 1 audit，它只在
   transition edge 觸發）。

位置：`src/daemon/per_tick/recovery_dispatcher.rs`。這是 modular per-tick
handler，沿用 sub-task 5 / #694 BLOCK 1 idiom。

## §RS.4 — Combined-gate 三個 branch

Decision §1.4 Delta 2——dispatcher 直接檢查 raw silence + productive
silence elapsed time（**不是**透過
[HUNG-STATE-TRANSITIONS.zh-TW.md](HUNG-STATE-TRANSITIONS.zh-TW.md) §F9.5
classification gate），讓 Stage 1 的價值不受 productive-gate promotion
時程影響：

| Branch | 條件 | 行動 |
|---|---|---|
| **alive-stuck** | `productive_silence > threshold` && `silence < threshold` | 發出 Stage 1 ESC（agent process 仍在讀 PTY，只是沒有 productive）。State → `Stage1Pending`。 |
| **dead-likely** | `silence > threshold` | 跳過 Stage 1；ESC 無法幫助不再讀取的 process。只寫 log，state 維持 `None`（#2549 前是 `→ Stage2Eligible`，現已移除）。 |
| **anomaly** | 兩個條件都不成立 | 記錄 warning，state 不變。Agent 理應不是 `Hung`。 |

Threshold 與 `check_hang` 中的 `silence_exceeds_threshold` 一致：
- `AgentState::Idle`：永不觸發（等待 input）
- `AgentState::Starting`：120s
- `AgentState::Thinking | ToolUse`：600s
- 其他 state：120s

Productive-silence threshold 透過 `health::productive_silence_exceeds`
helper 抽取（decision §1.4 Delta 2 Option a——DRY、single source of truth）。

## §RS.5 — Shadow-mode 預設值與 env var gate

| Env var | 預設 | 用途 |
|---|---|---|
| `AGEND_AUTO_RECOVERY_STAGE1` | unset（shadow） | `"1"` 會啟用：dispatcher 把 ESC byte 寫入 PTY。Unset：相同 telemetry，不做 I/O。 |

Dispatcher 每個 tick 都重新讀取 gate env var——operator 不必重啟 daemon
即可翻轉 `AGEND_AUTO_RECOVERY_STAGE1=1`。這對 shadow→active promotion
workflow 很重要。

兩種 gate mode 都會轉換至 `Stage1Pending` 並 stamp
`last_stage1_fired_at`。在 shadow mode，這些欄位只作 bookkeeping：它們
會抑制重複的 would-have-fired decision，並驅動 timeout/cooldown telemetry；
它們**不表示** ESC 已成功 delivery。

Stage 1 timeout（10 s，`STAGE1_TIMEOUT_DEFAULT_MS`）與 cooldown（60 s，
`STAGE1_COOLDOWN_DEFAULT_MS`）是 **fixed const，不可由 env 設定**
（#env-cleanup：`AGEND_AUTO_RECOVERY_STAGE1_TIMEOUT_MS` /
`_COOLDOWN_MS` override 已降級）。

### Promotion criteria（operator action）

本流程仿照
[HUNG-STATE-TRANSITIONS.zh-TW.md §F9.5](HUNG-STATE-TRANSITIONS.zh-TW.md)
中維護的 productive-output promotion SOP，並使用
[PTY fixture capture playbook](../tests/fixtures/state-replay/CAPTURE-RECIPES.zh-TW.md)
擴充 real evidence：

1. Operator 在整個 agent fleet 讓 daemon 維持
   `AGEND_AUTO_RECOVERY_STAGE1` unset（shadow）至少 2 週。
2. Operator 檢視 `recovery_shadow` tracing target output，分類每次 shadow fire：
   - **Would-have-helped**：agent 是 alive-stuck，且在 timeout 內後續 recovery
     顯示 ESC 很可能可以解除阻塞。
   - **Would-have-hurt**：agent 正在產生有用 output，而 ESC 會取消它。
3. Operator 信心足夠後（例如 N ≥ 30 次 shadow fire 中 ≥95% 為
   would-have-helped），在 production env 設定
   `AGEND_AUTO_RECOVERY_STAGE1=1`。

防止 dead infra 條款：若 6 週後仍未 measurement，Stage 1 會成為 removal
candidate。沿用 sub-task 4「dead shadow infra 比沒有 infra 更糟」的紀律。

## §RS.6 — Anti-thrash cooldown

Decision §1.4 Refinement B——若 agent 在最近一次 Stage 1 fire 後的
`STAGE1_COOLDOWN_DEFAULT_MS` 內再次進入 `Hung`，dispatcher 會跳過重送
Stage 1；只寫 log，也沒有後續 stage 可 escalation（#2549 前是轉換至
現已移除的 `Stage2Eligible`）。這可防止快速連發 ESC，避免掩蓋 infinite
loop 或持續 backend bug 等底層問題。

`HealthTracker` 上的 `last_stage1_fired_at: Option<Instant>` 會 stamp
clock。依 linear-escalation 紀律，只在 spontaneous recovery
（HealthState::Healthy）時清除。

## §RS.7 — `HealthState::Paused` guard

**#2549 後仍 live**——`Paused` 是要求 operator action 的 terminal state，
不同於 crash counter 耗盡的 `Failed`。它最初只能由此 dispatcher 現已移除
的 Stage 3 arm 進入；`enter_paused` 是唯一 writer，目前也由
`RespawnWatchdogHandler` 獨立呼叫（另一個 failure mode：stuck `resume`
spawn），使 `Paused` 成為共用 terminal-escalation machinery，而非 Hung
ladder 專屬：

| State | Trigger | Recovery |
|---|---|---|
| `Failed` | `record_crash` counter ≥ `max_retries`（window 內 5 次 process crash） | Operator action，或 `maybe_decay` 緩慢清除 counter |
| `Paused` | `HealthTracker::enter_paused`（`RespawnWatchdogHandler` retry-cap escalation；過去也包含此 dispatcher Stage 3 arm，已於 #2549 移除） | Operator unpause command（另一個 sub-task） |

Phase 1 已實作以下 guard（decision §5），目前仍有效：

- `check_hang` 在 `Paused` 上 short-circuit（回傳 `false`——沒有
  auto-recovery dispatcher 工作；operator 已收到警示，更多 warning 只是
  noise）。
- `maybe_decay` **不會**碰 `Paused`（crash decay 不得退出 Paused；只有
  operator unpause 才能）。
- `display_name() -> "paused"`，供 telegram visibility 與 JSON API
  consumer（`api/handlers/query.rs`）。

## §RS.8 — Cross-reference 與範圍外事項

### Cross-reference

- [HUNG-STATE-TRANSITIONS.zh-TW.md §F39.5](HUNG-STATE-TRANSITIONS.zh-TW.md)
  ——open question list 會引用本文的 staged-recovery 細節。
- [HUNG-STATE-TRANSITIONS.zh-TW.md §F9.5](HUNG-STATE-TRANSITIONS.zh-TW.md)
  ——recovery 對所有 `Hung` source 一視同仁；productive-gate promotion
  不需要另外的 recovery wiring。
- [PTY fixture capture playbook](../tests/fixtures/state-replay/CAPTURE-RECIPES.zh-TW.md)
  ——recovery shadow telemetry 會提供未來 corpus growth 的資訊；operator
  可以 capture Stage 1 shadow fire 前後的 PTY trace 作為 fixture collection。
- `src/daemon/per_tick/recovery_dispatcher.rs`——module implementation。
- `src/health.rs::RecoveryStageState`——state machine variant（#2549 後為
  `None` / `Stage1Pending` / `Stage3Pending`）。
- `src/health.rs::HealthState::Paused`——terminal state，目前可由
  `RespawnWatchdogHandler` 進入；歷史上也曾由此 dispatcher 進入。

### 範圍外（sub-task 7a baseline）

- Operator unpause command（CLI 或 MCP tool）——另一個 sub-task。
- Per-backend stage timing tuning——需要 corpus measurement，後續工作
  類似 sub-task 6 的 per-backend marker calibration。
- Stage 1 的 Telegram notify——decision §6 Refinement A：Stage 1 成功時
  silent（只有 info-level log）。
- F39 mitigation selection / productive-gate promotion——受 fixture-corpus-N
  gate 約束。

## §RS.9 — Stage 2 細節（sub-task 7b）

> ⚠ **歷史紀錄——已於 #2549 移除。** `§RS.9` 下方所有內容描述
> sub-task 7b 曾建置的項目與理由，只保留作為 decision record，不再描述
> live code：`AgentExitEvent::Stage2Restart`、
> `RecoveryStageState::{Stage2Eligible,Stage2Pending}`、
> `recovery_restart_count`、`daemon/mod.rs::handle_stage2_restart` 與
> `AGEND_AUTO_RECOVERY_STAGE2*` env var 都已刪除。理由請見文件 header banner。

Sub-task 7b（decision `d-20260514034230950032-2`）曾在 7a infrastructure
上實作 Stage 2。Stage 2 是**受控 auto-restart**：當 agent 無法從 Stage 1
ESC recovery（或一開始就是 dead-likely），dispatcher 會向 `crash_tx`
發出 `AgentExitEvent::Stage2Restart` event；
`src/daemon/mod.rs::handle_stage2_restart` 的 respawn worker Stage 2 arm
會執行 `spawn_agent`，並選擇性保留欄位。

### 9.1 Cumulative restart cap

`HealthTracker.recovery_restart_count: u32` 仿照 `total_crashes` 紀律。
每次 Stage 2 成功 fire，都在 respawn worker 端增加 counter（**不是**
dispatcher——避免 channel send 成功但 spawn 失敗時重複計數）。預設 cap
`STAGE2_MAX_RESTARTS_DEFAULT = 3`（依 decision §Q1/Q2——issue body
「fails N times → Stage 3」）。Operator 可用 env var
`AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS` override。

當 `recovery_restart_count >= cap`，dispatcher 的 Stage 1 entry arm 會
short-circuit cycle，**直接** escalation 至 `Stage3Eligible`——要求 operator
介入，而不是讓 automation 繼續 thrash。

### 9.2 Spawn 間的選擇性欄位保留

Decision §1 critical wrinkle（dev round 1）：`spawn_agent` 在
`rg "reg.insert" src/agent.rs` 建立一個**全新、使用預設 `HealthTracker`
的 `AgentCore`**。既有 Crash path 透過 `daemon/mod.rs` 的
`saved_health.clone()` 保留所有 health；Stage 2 需要不同語意：

| Field | Stage 2 行為 |
|---|---|
| `state` | 重設為全新的 `Healthy`——recovery success seed |
| `recovery_stage_state` | 重設為全新的 `None`——linear escalation reset |
| `last_stage1_fired_at` | 重設為全新的 `None`（Stage 2 表示 Stage 1 已 fire 或被 skip，但下一個 cycle 重新開始） |
| `crash_times` | **保留**——不可因 recovery restart 丟失 crash history |
| `total_crashes` | **保留**——理由相同 |
| `last_notification` | **保留**——notify cooldown discipline |
| `recovery_restart_count` | **保留並 +1**——counter 必須跨越它所驅動的 restart |
| `last_stage2_fired_at` | 設為 `Some(now)`——驅動 decay clock |

**不會**呼叫 `record_crash`（Stage 2 ≠ crash）。**不會**呼叫
`respawn_ok`（state 已是全新的 `Healthy`）。

### 9.3 1-second backoff

Decision §1.4 Delta 2：在 Stage 2 arm 執行 `spawn_agent` 前，預設 backoff
1s。這是針對 transient spawn error（filesystem / network / PTY allocation）
tight-loop 的 defensive padding。Crash path 使用 exponential 5s+
backoff；Stage 2 是 controlled action，因此允許較短 delay。Fixed const
`STAGE2_BACKOFF_DEFAULT_MS`（1 s），不可由 env 設定
（#env-cleanup：`AGEND_AUTO_RECOVERY_STAGE2_BACKOFF_MS` override 已降級）。

### 9.4 Stage 2 failure criteria（3 種 mode）

Dispatcher 的 `Stage2Pending` monitor 在下列任一情況下 escalation 至
`Stage3Eligible`：

1. **`spawn_agent` 回傳 `Err`**——Stage 2 無法完成；agent 從 registry
   移除，dispatcher 下一個 tick 沒有工作可做。Operator 已在 emit 前收到
   telegram。Phase 1 限制：需要 manual respawn 或未來的 operator-unpause
   command。
2. **30s timeout window 到期**，且沒有 recovery（
   `entered_at.elapsed() >= STAGE2_TIMEOUT_DEFAULT_MS` 時
   `state != Healthy`）。Fixed const（30 s），不可由 env 設定
   （#env-cleanup：`AGEND_AUTO_RECOVERY_STAGE2_TIMEOUT_MS` override 已降級）。
3. **Agent 在 Stage 2 window 內重新 Hung**——`Stage2Pending` 且
   state == Hung 表示短暫 Healthy 後又回到 Hung，因此要更積極 escalation。
   （Phase 1 implementation：timeout check 已涵蓋；re-Hung 只是「timeout
   後仍非 Healthy」的具體情況。）

### 9.5 Channel-full safety（try_send）

`crash_tx` 在 `daemon/mod.rs:438` 是 `bounded::<>(64)`。極端負載下
（例如許多 agent 同時 crash），send 可能回傳 `TrySendError::Full`。
Dispatcher 使用 `try_send`：

- **`Ok`**：state 轉換至 `Stage2Pending`，並 stamp
  `last_stage2_fired_at`。Counter increment 位於 respawn worker 端，因此
  event delivery 成功但 spawn 未完成時不會錯誤增加。
- **`Err`**：state 維持 `Stage2Eligible`，counter 不增加。Dispatcher
  下一個 tick 重試。

這就是 decision §extras 提到的 **race coverage**：Stage 2 spawn 期間若有
crash 到達同一 channel，不會 double-count，因為 dispatcher 的 `try_send`
使用不同的 `Stage2Restart` variant；crash 會獨立走自己的 path。

### 9.6 Spawn failure 的 Phase 1 限制

若 `handle_stage2_restart` 中的 `spawn_agent` 回傳 `Err`，agent 會從
registry **移除**。Dispatcher 下一個 tick 找不到它，recovery sequence
到此結束。Stage 2 telegram 在 spawn attempt 前就**預先發出**，因此仍保留
operator visibility。

完整 lifecycle（operator 驅動的 re-spawn 或 unpause）原定由 sub-task 7c
加上另一個 operator-unpause command sub-task 交付。Phase 1 可接受：
spawn-failure 是 edge case；operator 可透過既有 `start` CLI 或 MCP
`agent spawn` tool 手動 re-spawn。

### 9.7 Telegram notify 內容

```
[recovery] {agent_name}: Stage 2 auto-restart triggered.
Hung silence: {silent_ms}ms (productive silence: {prod_ms}ms)
Recovery restart count: {count}
Next: monitoring 30s for recovery; Stage 3 (pause + operator action)
on continued failure.
```

這段訊息可供 operator 採取行動：它顯示 trigger 原因（silence 或
productive silence——區分 alive-stuck 與 dead-likely）、目前 restart-count
距離 cap 的進度，以及預期下一個 escalation step 的時間。

### 9.8 Activation gate（仿照 §RS.5 Stage 1 pattern）

| Env var | 預設 | 用途 |
|---|---|---|
| `AGEND_AUTO_RECOVERY_STAGE2` | unset（shadow） | `"1"` 會啟用：dispatcher 發出 `Stage2Restart` event。Unset：相同 telemetry，不 emit。 |
| `AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS` | 3 | Cumulative cap → 直接 escalation 至 `Stage3Eligible`。 |

Stage 2 monitoring window（30 s，`STAGE2_TIMEOUT_DEFAULT_MS`）與 respawn
backoff（1 s，`STAGE2_BACKOFF_DEFAULT_MS`）是 **fixed const，不可由 env
設定**（#env-cleanup：`AGEND_AUTO_RECOVERY_STAGE2_TIMEOUT_MS` /
`_BACKOFF_MS` override 已降級）。

使用與 Stage 1 相同的 shadow-mode promotion workflow：operator 先在
shadow 執行 ≥2 週，透過 `recovery_shadow` tracing target 分類
would-have-fire，信心足夠後翻轉為 active。防止 dead infra 條款：6 週未
measurement → Stage 2 成為 removal candidate。

### 範圍外（sub-task 7b）

- Stage 3 dispatcher arm + `HealthState::Paused` activation——sub-task 7c。
- Operator unpause command——另一個 sub-task（Stage 3 production 前必要）。
- Per-backend Stage 2 timeout / backoff tuning——需要 corpus measurement，
  由後續處理。
- Variant-split spawn 的完整 PTY-backed integration test——unit test 涵蓋
  state machine + counter discipline；除非 shadow telemetry 顯示 edge
  case，完整 integration 先 deferred。

## §RS.10 — Stage 3 細節（sub-task 7c）

> ⚠ **歷史紀錄——本節所述 dispatcher-driven arm 已於 #2549 移除。**
> `RecoveryStageState::Stage3Eligible`、
> `recovery_dispatcher.rs::handle_stage3_escalate` /
> `notify_stage3_escalate` / `format_stage3_body`，以及此 dispatcher
> 本身對 `enter_paused` 的呼叫都已刪除。**`HealthTracker::enter_paused` /
> `HealthState::Paused` / `RecoveryStageState::Stage3Pending` 本身仍 live**——
> `§10.2`-`§10.5` 的 atomic-invariant / no-op-arm mechanics 仍逐字適用，
> 只是目前由 `RespawnWatchdogHandler` 觸發，而不是此 dispatcher。
> `§10.4` 的 `recovery_restart_count` 已不存在（與 Stage 2 一起刪除，
> 不是在 unpause 時重設，因為已沒有可重設的欄位）。理由請見文件 header
> banner。

Stage 3 是 auto-recovery state machine 的 terminal stage。Stage 1 ESC
失敗、Stage 2 auto-restart 已嘗試至 cumulative cap
（`recovery_restart_count >= STAGE2_MAX_RESTARTS`）後，dispatcher 會把
agent escalation 至 `HealthState::Paused`，並通知 operator 必須人工介入。

### 10.1 Stage 3 目的

Auto-recovery 已耗盡；繼續 unattended retry 只會讓 agent thrash。Stage 3
的工作是**停止嘗試**、把 agent 的 `HealthState` 鎖定在不採取行動的
terminal value，並透過 Error-level telegram 向 operator 顯示情況。
Escalation 在每個 Hung cycle **只執行一次**——進入 `Stage3Pending` 後，
dispatcher 的 `Stage3Pending` arm 是明確 no-op（見 §10.5）。

### 10.2 `enter_paused` atomic invariant

`src/health.rs::HealthTracker::enter_paused(&mut self, now: Instant)` 是
codebase 中 `HealthState::Paused` 的**唯一 writer**（§F39.5 invariant——
單一 grep target）。此 method 在 caller lock 下寫入三個 invariant：

1. `state = HealthState::Paused`
2. `recovery_stage_state = RecoveryStageState::Stage3Pending { entered_at: now }`
3. `last_stage3_fired_at = Some(now)`

`Stage3Pending` variant 攜帶 `entered_at`，讓 dispatcher no-op debug log
能回報 Paused-since duration，不必回頭讀取 `HealthTracker`。
`last_stage3_fired_at` 保留給未來 operator-unpause sub-task（UX「Paused
since N minutes」），在該 sub-task 讀取前使用 `#[allow(dead_code)]`。

DI-friendly signature 與 `maybe_decay_at(now)` 平行：production 傳入
`Instant::now()`；test 傳入 deterministic base，確保 cross-platform-safe
arithmetic（PR #775 v2 lesson——`Instant::add` 在所有平台都 saturate；
`Instant::now() - Duration` 在低 uptime 的 Windows CI VM 可能 underflow）。

### 10.3 `NotifySeverity::Error` + telegram format

`NotifySeverity` enum 有三個 level：`Info`、`Warn`、`Error`。Stage 2
使用 `Warn`；crash notification 使用 `Error`。Stage 3 表示「auto-recovery
已耗盡，operator 必須採取行動」，所以 severity 必須 ≥ crash level →
`Error`。`silent=false`，讓 operator channel 與 crash notification 一起
顯示它。

Telegram body（由 `format_stage3_body(name, count)` 建立，unit test 會固定
operator-facing wording）：

```
[recovery ESCALATION] {name}: PAUSED — manual intervention required.
  Stage 2 auto-restart fired {count} time(s), all exhausted.
  Final state: Paused (no further auto-recovery).
  Action: investigate root cause + manual unpause (CLI command pending sub-task).
```

Telegram 在 shadow 與 active mode 都會觸發，讓 operator 在翻轉 gate 前先
看到 escalation pattern。訊息在 state write 前**預先 emit**，因此即使在
telegram 與 `enter_paused` 間 crash，decision 仍可見。

### 10.4 `recovery_restart_count` 不在 `enter_paused` 重設

Stage 3 entry 會**保留** counter。理由：Paused 代表「automated retry
已耗盡；必須處理 root cause」。若未來 operator-unpause sub-task 把 agent
帶回 `Healthy`，但 root cause 未修而再次 Hung，dispatcher cap check 應
立即再次 escalation 至 `Stage3Eligible`，而非重新消耗 auto-restart
budget。Operator 語意是：pause 具有 stickiness；counter 是否在 unpause
重設，屬 unpause sub-task 的 design space。

### 10.5 `Stage3Pending` idempotent no-op

Dispatcher 的 `Stage3Pending` arm 是明確 no-op：

```rust
RecoveryStageState::Stage3Pending { entered_at } => {
    tracing::debug!(
        target: "recovery_shadow",
        agent = %name,
        paused_for_ms = entered_at.elapsed().as_millis() as u64,
        "stage3_pending: awaiting operator unpause"
    );
}
```

不修改 state、不重新發 telegram、沒有 timeout escalation。`run()` 中
top-level `HealthState::Paused` early-`continue`（§RS.7 的 7a guard）提供
第二層保護。兩者合併保證：dispatcher 無法從 Paused 向外 escalation，
後續 tick 不會重送 Stage 3 telegram，且 `maybe_decay_at` 遵守 Paused
short-circuit，因此 operator 看到的 counter 忠實反映進入 Paused 的時刻。

### 10.6 Promotion criteria（`AGEND_AUTO_RECOVERY_STAGE3=1`）

Hybrid template（round 2 convergence）：

1. Operator 在整個 agent fleet 讓 daemon 維持
   `AGEND_AUTO_RECOVERY_STAGE3` unset（shadow）≥2 週。
2. Operator 檢視 `recovery_shadow` tracing target output，重點放在
   **每週 trigger rate**，而不是 FP-per-trigger。Terminal stage 的 FP
   語意未定義——Stage 3 只在 Stage 2 retry 已可證明耗盡後觸發，因此不當
   action 的風險在結構上接近零。Observation target 是「fleet 到底多常
   觸及 auto-recovery exhaustion？」
3. Trigger-rate baseline 穩定，且 operator 確信 paused agent 確實 stuck
   （而不是 threshold tuning 錯誤等情況）後，設定
   `AGEND_AUTO_RECOVERY_STAGE3=1`。

防止 dead infra 條款沿用 Stage 1 / Stage 2：6 週未 measurement → Stage 3
promotion infrastructure 成為 removal candidate。

### 10.7 Paused 解除限制（Phase 1）

`enter_paused` 寫入後，agent 會維持 `Paused`，直到以下任一情況：

- **Operator 透過既有 CLI agent-restart surface 手動 restart**——完全重設
  agent（全新 `HealthTracker`），也會重設 `recovery_restart_count`。這是
  Phase 1 operator workflow。
- **未來 operator-unpause sub-task** 會提供專用 `unpause` CLI / MCP
  command，在不做完整 restart 的情況下轉換 `Paused → Healthy`。該 scope
  包含 unpause 時是否重設 `recovery_restart_count` 的 design space；7c
  不會預先決定。

`Paused` 沒有 automatic exit。`maybe_decay_at` 遵守 body 第一行的
short-circuit（7a guard）——operator 看到的 counter 忠實反映 Paused-entry
時刻。

### 範圍外（sub-task 7c）

- Operator unpause command（另一個 sub-task）
- Per-backend Stage 3 customization（Phase 3）
- Multiple-Pause aggregation（只追蹤單一 Paused）
- unpause 時重設 `recovery_restart_count`（defer 至 unpause sub-task）
- `last_stage3_fired_at` consumer code（保留給 unpause sub-task；
  `#[allow(dead_code)]` 讓欄位通過 7c）
- 透過 registered agent 執行 `enter_paused` 的完整 PTY-backed integration
  test——unit test 在 `HealthTracker` boundary 固定 atomic invariant +
  idempotency；除非 shadow telemetry 顯示 edge case，integration 先 deferred。
