# Track 1 — `waiting_on` annotation + heartbeat

**Status:** design doc for Track 1。解決 A2 (silent wait) + A5 (state detector
false positive) friction。派工前 inline-reviewed by general，非 PR 流程。

## 1. Scope

**In scope：**

1. MCP tool `set_waiting_on` — agent 宣告自己在等什麼（review、delegation
   result、human input 等）
2. Implicit heartbeat — 每次 MCP tool call 到達 daemon 即為一次 heartbeat
3. State detector gate — heartbeat 新鮮時，`PermissionPrompt` latch 被
   override 為 `Thinking`（agent 活著在做事，PTY regex 是 false positive）
4. Stale decay — heartbeat 超時後 `waiting_on` 自動清除，state detector
   恢復正常 PTY-based 判定
5. `list_instances` / `describe_instance` 暴露 `waiting_on` 欄位

**Out of scope（明列以免 scope creep）：**

- TUI 渲染變更（TUI 已經顯示 `agent_state`，`waiting_on` 只是 metadata
  merge 進 API response，TUI 自然拿到；渲染美化另立）
- Telegram / Discord channel rendering（Stage B-UX infra 已有 UxEvent
  pipeline，waiting_on 的 fleet visibility 是 Stage D-UX 範疇）
- Cross-agent blocking graph / task dependency visualization（B1+B2 friction，
  另立 track）
- AgentState enum 新 variant（見 §9 Decision D1）
- 重構 state machine（leverage 是解 A2+A5，不是重構）

## 2. Problem statement

### A5 — PermissionPrompt false positive（50 分空轉 incident）

Wave 3 期間，前任 at-dev-2 (claude) 等待 at-dev-4 (codex) review。agent 的
PTY 畫面殘留 `Allow this action` 或 `y/n/t` 等 pattern，state detector 偵測
為 `PermissionPrompt`（priority 8）。

根因：`PermissionPrompt` 在 `maybe_expire_latched_state()` 中被排除——只有
`Thinking` / `ToolUse` 會在 30 秒後 auto-expire。設計意圖是 permission prompt
需要 operator 明確動作才能 dismiss，但這個假設在 multi-agent 場景下失效：agent
可能在等另一個 agent 回覆，PTY 畫面靜止但 agent 並非真的在等 operator。

結果：operator 看到 `permission` 標籤 50 分鐘，以為 agent 卡住需要人工介入，
實際上 agent 只是在等 review 結果。

### A2 — Silent wait（不知道 agent 在等什麼）

Agent 等待 delegation result / review / human input 時，`list_instances`
只顯示 `agent_state: thinking` 或 `permission`。Operator 無法區分：

- Agent 正在思考（正常）
- Agent 在等另一個 agent 回覆（正常但需要知道等誰）
- Agent 卡住了（需要介入）

缺乏 intent declaration 讓 operator 的 situational awareness 降級。

## 3. PR split + LOC budget

| PR | Scope | Depends on | LOC budget |
|---|---|---|---|
| **PR-1** | `set_waiting_on` MCP tool + metadata persistence + heartbeat timestamp tracking + `list_instances`/`describe_instance` 暴露 | main | ~200 |
| **PR-2** | State detector heartbeat gate — `PermissionPrompt` override + stale decay | PR-1 | ~150 |

PR-1 可獨立驗證：tool 設定 `waiting_on`，`list_instances` 回傳欄位，
heartbeat timestamp 被記錄。不需要 PR-2 就能觀察。

PR-2 是 A5 的 actual fix：state detector 在 heartbeat 新鮮時 override
`PermissionPrompt`。

## 4. Code design

### 4.1 MCP tool surface

新增一個 tool `set_waiting_on`，不需要 `clear_waiting_on`——設 `condition`
為空字串即為 clear。

```rust
// src/mcp/tools.rs — instance_tools() 新增
json!({
    "name": "set_waiting_on",
    "description": "Declare what this instance is currently waiting for. \
        Set to empty string to clear. Automatically cleared when stale.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "condition": {
                "type": "string",
                "description": "What you are waiting for, e.g. 'review from at-dev-4', \
                    'delegation result from at-dev-1'. Empty string to clear."
            }
        },
        "required": ["condition"]
    }
})
```

Handler（`src/mcp/handlers.rs`）：

```rust
"set_waiting_on" => {
    let condition = args["condition"].as_str().unwrap_or("");
    if condition.is_empty() {
        // Clear: remove waiting_on + waiting_on_since from metadata
        save_metadata(&home, instance_name, "waiting_on", json!(null));
        save_metadata(&home, instance_name, "waiting_on_since", json!(null));
        json!({"cleared": true})
    } else {
        let now = chrono::Utc::now().to_rfc3339();
        save_metadata(&home, instance_name, "waiting_on", json!(condition));
        save_metadata(&home, instance_name, "waiting_on_since", json!(now));
        json!({"waiting_on": condition, "since": now})
    }
}
```

跟 `set_display_name` / `set_description` 完全同 pattern：
`save_metadata()` → `~/.agend/metadata/{name}.json` → `merge_metadata()`
自動 merge 進 `list_instances` / `describe_instance` response。零 API 層
改動。

### 4.2 Heartbeat infra — implicit, not explicit

每次 MCP `handle_tool()` 被呼叫，就是一次 heartbeat。Agent 活著在做事
（call tool = alive）。不需要 explicit heartbeat tool。

```rust
// src/mcp/handlers.rs — handle_tool() 開頭，在 match 之前
pub fn handle_tool(tool: &str, args: &Value, instance_name: &str) -> Value {
    let home = crate::home_dir();
    let sender: Option<Sender> = Sender::new(instance_name).or_else(Sender::from_env);
    let instance_name: &str = sender.as_ref().map(Sender::as_str).unwrap_or("");

    // Implicit heartbeat: any MCP tool call = agent is alive
    if !instance_name.is_empty() {
        save_metadata(
            &home,
            instance_name,
            "last_heartbeat",
            json!(chrono::Utc::now().to_rfc3339()),
        );
    }

    match tool { .. }
}
```

**為何不做 explicit heartbeat tool：**

- Agent 已經在做 MCP call（send_to_instance、inbox、delegate_task 等），
  每次 call 自然帶 heartbeat。額外 tool 增加 agent 的 prompt 負擔且
  浪費 token。
- 如果 agent 真的完全靜默（不做任何 MCP call），那它確實可能卡住了——
  heartbeat 超時是正確行為。

**PTY output 不算 heartbeat：** PTY output 只代表 terminal 有輸出，不代表
agent 在做有意義的事。Cursor blink、spinner animation 都會產生 PTY output
但不代表 agent alive。MCP call 是 agent 的 intentional action，語意更準確。

### 4.3 State detector heartbeat gate（PR-2）

`StateTracker` 新增方法，由 `feed()` 呼叫：

```rust
impl StateTracker {
    /// Heartbeat freshness threshold. If the last MCP heartbeat is within
    /// this window, the agent is considered alive and PermissionPrompt
    /// detection is suppressed (overridden to Thinking).
    const HEARTBEAT_FRESH_WINDOW: Duration = Duration::from_secs(120);

    /// Called by feed() after pattern detection. If the detected state is
    /// PermissionPrompt but a fresh heartbeat exists, override to Thinking.
    fn gate_on_heartbeat(&self, detected: AgentState) -> AgentState {
        if detected != AgentState::PermissionPrompt {
            return detected;
        }
        if self.is_heartbeat_fresh() {
            AgentState::Thinking
        } else {
            detected
        }
    }

    fn is_heartbeat_fresh(&self) -> bool {
        self.last_heartbeat
            .map(|t| t.elapsed() < Self::HEARTBEAT_FRESH_WINDOW)
            .unwrap_or(false)
    }
}
```

`StateTracker` 新增欄位：

```rust
pub struct StateTracker {
    // ... existing fields
    /// Last MCP heartbeat instant. Updated by daemon when MCP tool call
    /// arrives. None before first heartbeat.
    last_heartbeat: Option<Instant>,
}
```

**Heartbeat 怎麼從 MCP handler 到 StateTracker：**

MCP handler 寫 metadata file（`last_heartbeat` 欄位）。Daemon supervisor
tick 讀 metadata file、parse timestamp、更新 `StateTracker::last_heartbeat`。

替代方案：MCP handler 直接寫 `StateTracker`（需要 `AgentCore` 的 `Arc<Mutex>`
穿透到 MCP handler）。但 MCP handler 目前是 stateless function（只拿
`tool, args, instance_name`），引入 `AgentCore` 依賴會破壞這個 boundary。
走 metadata file 是 zero-coupling 路徑，跟 `set_display_name` 一致。

**Supervisor tick 讀 heartbeat：**

```rust
// src/daemon/supervisor.rs — existing tick loop
// 在 feed() 之前讀 metadata 更新 heartbeat
let meta_path = home.join("metadata").join(format!("{name}.json"));
if let Ok(meta) = std::fs::read_to_string(&meta_path)
    .and_then(|c| serde_json::from_str::<Value>(&c).map_err(std::io::Error::other))
{
    if let Some(ts) = meta["last_heartbeat"].as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
            let elapsed = chrono::Utc::now().signed_duration_since(dt);
            if let Ok(std_dur) = elapsed.to_std() {
                core.state.update_heartbeat(std_dur);
            }
        }
    }
}
```

`update_heartbeat` 把 file-based timestamp 轉成 `Instant`：

```rust
impl StateTracker {
    pub fn update_heartbeat(&mut self, age: Duration) {
        self.last_heartbeat = Some(Instant::now() - age);
    }
}
```

### 4.4 Stale decay

`waiting_on` 自動清除條件：heartbeat 超過 `HEARTBEAT_FRESH_WINDOW`（120s）
且 `waiting_on` 非空。

清除不在 state detector 裡做（state detector 不該有 side effect）。
在 supervisor tick 裡做：

```rust
// supervisor tick — after heartbeat read
if !core.state.is_heartbeat_fresh() {
    let meta_path = home.join("metadata").join(format!("{name}.json"));
    // Clear waiting_on if stale
    if let Ok(meta) = std::fs::read_to_string(&meta_path)
        .and_then(|c| serde_json::from_str::<Value>(&c).map_err(std::io::Error::other))
    {
        if meta.get("waiting_on").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
            save_metadata(&home, name, "waiting_on", json!(null));
            save_metadata(&home, name, "waiting_on_since", json!(null));
            tracing::info!(%name, "waiting_on cleared — heartbeat stale");
        }
    }
}
```

### 4.5 Type changes — orthogonal annotation, not AgentState variant

`waiting_on` 是 metadata（file-based JSON），不是 `AgentState` variant。

見 §9 Decision D1 的完整 trade-off 分析。

## 5. Telegram fleet rendering

**本 track 不做 fleet rendering。**

`waiting_on` 已經透過 `merge_metadata()` 暴露在 `list_instances` /
`describe_instance` API response 中。Fleet rendering（把 `waiting_on`
emit 成 `UxEvent` 送到 fleet binding）屬於 Stage D-UX 範疇，跟
`AgentThinking` / `Idle` / `RateLimited` 等 event 一起做。

理由：Track 1 的 leverage 是解 A2+A5 friction，不是做 fleet visibility。
Fleet visibility 需要 `UxEvent` 新 variant + renderer 改動，scope 膨脹
不值得。Operator 透過 `list_instances` MCP tool 或 TUI 已經能看到
`waiting_on`。

## 6. A5 specific fix 路徑

### 現狀

1. PTY screen 含 `Allow this action` / `y/n/t` → `StatePatterns::detect()`
   回傳 `PermissionPrompt`
2. `StateTracker::feed()` → `transition(PermissionPrompt)`
3. `PermissionPrompt` priority 8 > 任何 active state → 立即 transition
4. `maybe_expire_latched_state()` 排除 `PermissionPrompt`（設計意圖：
   需要 operator 明確動作）
5. 結果：`PermissionPrompt` 永遠 latch 直到 PTY screen 變化

### Fix 路徑

在 `feed()` 的 `patterns.detect()` 之後、`transition()` 之前插入
`gate_on_heartbeat()`：

```rust
pub fn feed(&mut self, screen_text: &str) {
    // ... hash dedup ...
    if let Some(ref patterns) = self.patterns {
        match patterns.detect(screen_text) {
            Some(detected) => {
                let gated = self.gate_on_heartbeat(detected);
                self.transition(gated);
            }
            None => { /* existing fallback logic */ }
        }
    }
}
```

**語意：** 如果 agent 在過去 120 秒內做過 MCP call，PTY 上的
`PermissionPrompt` pattern 被視為 false positive（agent 活著在做事，
不可能同時在等 operator 按 Allow）。Override 為 `Thinking`。

**120 秒後：** heartbeat stale → `gate_on_heartbeat()` 不 override →
`PermissionPrompt` 正常 latch。如果 agent 真的卡在 permission prompt
且停止做 MCP call，120 秒後 state detector 恢復正確判定。

**為何 override 到 Thinking 而非 Ready：** agent 有 fresh heartbeat 代表
它在做事（MCP call），`Thinking` 比 `Ready` 更準確反映 agent 狀態。
`Ready` 暗示 agent 閒置等待 input，但 agent 可能正在等 delegation result
同時做其他事。

## 7. Test strategy

### PR-1 tests

- `set_waiting_on` handler test：設定 condition → metadata file 有
  `waiting_on` + `waiting_on_since`；設空字串 → metadata 清除
- `list_instances` integration：設定 `waiting_on` 後 `list_instances`
  response 含 `waiting_on` 欄位（走 `merge_metadata` 路徑）
- Heartbeat recording：任意 MCP tool call → metadata file 有
  `last_heartbeat` timestamp

### PR-2 tests — A5 regression pin（bug-repro validated）

**Pin 1 — A5 incident scenario：**

```rust
#[test]
fn heartbeat_fresh_overrides_permission_prompt() {
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    // Simulate fresh heartbeat
    t.update_heartbeat(Duration::from_secs(10)); // 10s ago = fresh
    // Feed permission prompt pattern
    t.feed("Allow this action y/n/t");
    // Should NOT be PermissionPrompt — heartbeat is fresh
    assert_ne!(t.get_state(), AgentState::PermissionPrompt);
    assert_eq!(t.get_state(), AgentState::Thinking);
}
```

**Pin 2 — stale heartbeat restores normal detection：**

```rust
#[test]
fn stale_heartbeat_allows_permission_prompt() {
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    // Simulate stale heartbeat (200s ago > 120s threshold)
    t.update_heartbeat(Duration::from_secs(200));
    t.feed("Allow this action y/n/t");
    // Should be PermissionPrompt — heartbeat is stale
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}
```

**Pin 3 — no heartbeat = normal detection：**

```rust
#[test]
fn no_heartbeat_allows_permission_prompt() {
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    // No heartbeat ever set (last_heartbeat = None)
    t.feed("Allow this action y/n/t");
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}
```

**Bug-repro validation protocol：**

1. Neuter fix body（comment out `gate_on_heartbeat` call in `feed()`）
2. Run Pin 1 → assert FAIL（`PermissionPrompt` not overridden）
3. Restore fix body
4. Run Pin 1 → assert PASS
5. Report 中明說 validation 結果

### Stale decay test

```rust
#[test]
fn waiting_on_cleared_when_heartbeat_stale() {
    // Setup: metadata has waiting_on + last_heartbeat 200s ago
    // Supervisor tick runs
    // Assert: waiting_on cleared from metadata
}
```

## 8. Non-goals

- **Task dependency graph visualization** — B1+B2 friction，另立 track
- **Cross-agent blocking graph** — 需要 agent 間的 dependency 資訊，
  `waiting_on` 只是 free-text annotation，不是 structured dependency
- **Explicit heartbeat MCP tool** — implicit heartbeat 足夠（§4.2）
- **AgentState::WaitingOn variant** — orthogonal annotation（§9 D1）
- **Fleet rendering of waiting_on** — Stage D-UX（§5）
- **Auto-set waiting_on on delegate_task** — tempting 但 agent 可能
  delegate 後繼續做其他事，auto-set 會 false positive。Agent 自己
  知道什麼時候在等，讓 agent 自己 set。
- **PermissionPrompt auto-expire** — 不改 `maybe_expire_latched_state()`
  的排除邏輯。Heartbeat gate 是更精確的 fix：只在有 evidence（MCP call）
  時 override，不是盲目 expire。

## 9. Decisions log

| # | Question | Decision | Rationale |
|---|---|---|---|
| D1 | `waiting_on` 是 `AgentState` variant 還是 orthogonal annotation | **Orthogonal annotation（metadata）** | `waiting_on` 是 agent 的 intent declaration，不是 runtime state。混進 `AgentState` priority ladder 會污染 state detection 語意：`WaitingOn` 該排在哪個 priority？比 `Thinking` 高？那 agent 在 thinking 時設了 `waiting_on` 就會被 override。比 `PermissionPrompt` 低？那 A5 沒解。Orthogonal = 不影響 state machine，只影響 heartbeat gate + API 顯示。 |
| D2 | Implicit heartbeat vs explicit heartbeat tool | **Implicit（MCP call = heartbeat）** | Agent 已經在做 MCP call，額外 tool 浪費 token + 增加 prompt 負擔。如果 agent 完全靜默（不做任何 MCP call），heartbeat 超時是正確行為——agent 可能真的卡住了。 |
| D3 | Stale decay 時限 | **120 秒** | 跟 `HEARTBEAT_FRESH_WINDOW` 一致。Multi-agent 場景下 agent 通常每 30-60 秒做一次 MCP call（inbox check、send_to_instance 等）。120 秒留足夠 margin 避免 false stale。太短（30s）會在 agent 做長時間 thinking 時 false stale；太長（600s）會讓 A5 的 50 分空轉問題只縮短到 10 分。 |
| D4 | `waiting_on` 是否 fleet-visible（emit UxEvent） | **本 track 不做** | Fleet visibility 需要 `UxEvent` 新 variant + renderer 改動。Track 1 的 leverage 是解 A2+A5，fleet visibility 是 Stage D-UX 範疇。Operator 透過 `list_instances` 已能看到。 |
| D5 | Heartbeat 從 MCP handler 到 StateTracker 的路徑 | **File-based（metadata JSON）** | MCP handler 是 stateless function（`fn handle_tool(tool, args, instance_name)`），不持有 `AgentCore` reference。引入 `Arc<Mutex<AgentCore>>` 會破壞 handler 的 stateless boundary。走 metadata file 是 zero-coupling 路徑，supervisor tick 讀 file 更新 `StateTracker`。File I/O 成本：supervisor tick 已經在讀 metadata（`merge_metadata` 路徑），不是新增 I/O。 |
| D6 | Override target state | **Thinking（非 Ready）** | Fresh heartbeat = agent 在做事。`Thinking` 比 `Ready` 更準確。`Ready` 暗示 agent 閒置等待 input。 |

## 10. Open questions for orchestrator review

1. **120 秒 threshold 是否合理？** 基於 multi-agent 場景下 agent 通常
   每 30-60 秒做一次 MCP call 的觀察。如果 orchestrator 有不同的
   empirical data，可以調整。

2. **Supervisor tick 讀 metadata file 的頻率？** 現有 supervisor tick
   間隔是多少？如果 tick 間隔 > 10 秒，heartbeat freshness 判定會有
   延遲。需要確認 tick 間隔是否足夠。

3. **`set_waiting_on` 是否需要 `err_needs_identity` gate？** 目前
   `set_display_name` / `set_description` 不 gate（anonymous tolerated）。
   `waiting_on` 語意上需要 identity（「誰在等」），但 metadata 已經
   按 `instance_name` 分檔，anonymous 設不了。傾向不加 gate，跟
   existing pattern 一致。

4. **PR-2 的 `gate_on_heartbeat` 是否也該 gate `InteractivePrompt`？**
   `InteractivePrompt`（priority 7）也是 operator-action-required state。
   如果 agent 有 fresh heartbeat，`InteractivePrompt` 也可能是 false
   positive。但 `InteractivePrompt` 目前只在 `Starting` state 觸發
   （`is_generic_startup_prompt` + backend patterns），false positive
   風險低。傾向只 gate `PermissionPrompt`，scope 最小。

---

*Track 1 design doc — 2026-04-22 · 基於 Wave 3 A5 incident 實戰經驗*
