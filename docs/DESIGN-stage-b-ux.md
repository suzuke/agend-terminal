# Stage B-UX — Fleet visibility (Q2) implementation

**Status:** design doc for Wave 3 / Stage B-UX。對應 `PLAN-channel-ux-layer.md`
§9 Stage B-UX、§6 Q2 (S2c + S2d)、§7 `fleet_binding`、§4 `FleetEvent`。
派工前 inline-reviewed by general，非 PR 流程。

## 1. Scope

四塊：

1. **Config parsing** — `channels.<name>.fleet_binding`（plan §7）
2. **FleetEvent producer** — 4 個 MCP handler emit 到 in-process `UxEventSink`
   registry，包成 `UxEvent::Fleet(FleetEvent)`（plan §4 已鎖 enum 形狀）
3. **Fleet binding renderer** — TelegramChannel 吃 `UxEvent::Fleet(..)` →
   format 成 one-liner → 送到 `fleet_binding`（plan §6 S2c）
4. **S2d provenance injection** — delegate 的 **receive 端**，agent B 的
   primary binding 收到 `⬅️ from A — DELEGATE brief` system message

**非 scope**（plan §8 + §9 Stage D 排除）：
- `send_to_instance` / `request_information` mirror（plan §4 + §8 排除）
- per-event / per-agent / per-kind 過濾、auto-create fleet binding
- `AgentThinking / Idle / RateLimited / Crashed / Restarted`（Q1 剩餘）
- typing_indicator action、max_msg_bytes 的 "view full" hook（Stage D-UX）
- Discord / Slack adapters（本 doc 只寫 Telegram，但 `UxEventSink` trait 對外不鎖 adapter）

## 2. PR split

| PR | Scope | Depends on | LOC budget |
|---|---|---|---|
| **PR-A** | §3 config + §4 FleetEvent 線路到 registry + producer tests | main (origin) | ~350 |
| **PR-B** | §5 TelegramChannel fleet renderer + S2c format + snapshot test | PR-A | ~250 |
| **PR-C** | §6 S2d provenance injection in delegate_task receive path | main (並行 PR-B) | ~200 |

**依賴順序**：A → B 必須序列（B consume A 建的 UxEventSink registry + FleetEvent）。
C 正交，走 delegate_task 的 *receive* 端，跟 A/B 的 producer→sink 路徑不重疊，
可與 B 並行 review。

**PR-A 可驗證性**：PR-A 落地時 sink registry 沒有 fleet renderer（只有 NoopUxSink
+ T3 的 TelegramChannel Q1 路徑），但 unit test 拿一個 recording sink（`Arc<dyn UxEventSink>`
存 received events）掛進 registry、invoke 每個 MCP handler、assert bus 有對應 FleetEvent。
不需要 PR-B 才能觀察。

## 3. Config schema

plan §7 already sketched。實作：擴 `ChannelConfig::Telegram` 加 `fleet_binding: Option<FleetBindingConfig>`。

```yaml
# fleet.yaml
channels:
  telegram:
    type: telegram
    token_env: TELEGRAM_BOT_TOKEN
    group_id: -100...
    fleet_binding:          # 可省略；省略 = 不 mirror
      type: topic
      name: "fleet-activity"
```

**支援兩種 YAML 形狀**（plan §7 exemplar）：
- struct：`{ type: topic, name: "..." }` → TG topic / Discord channel / Slack thread
- string shorthand：`"#agend-ops"` → Slack / Discord 用的 channel name（TG 不接受 shorthand，會 warn）

Telegram 的 `fleet_binding` resolved 後產生一個 `BindingRef`（kind="telegram",
payload=`TelegramBindingPayload { topic_id }`, display_tag=「fleet」）存在
`TelegramState` 加一欄 `fleet_binding: Option<BindingRef>`。

**Resolution 時機**：bootstrap 時，跟 `instance_to_topic` 一起 resolve。topic 不存在
就 bail（跟其他 instance binding 一致）。

## 4. FleetEvent producer — 4 個 MCP handler hook

全程走 plan §4 鎖定的 `UxEvent::Fleet(FleetEvent)` wrapper，共用 T3 的 `UxEventSink` trait。

### 4.1 UxEventSink registry

新檔 `src/channel/sink_registry.rs`：

```rust
use std::sync::{Arc, OnceLock, RwLock};
use super::ux_event::{UxEvent, UxEventSink};

/// Global registry of UxEventSinks. Follows origin/main's established
/// singleton pattern — crate-level `static` with `OnceLock<T>`, read by
/// name at call sites. Examples on main: `src/mcp/mod.rs::ACL`,
/// `src/schedules.rs::DETECTED_TZ`, `src/channel/telegram.rs::RT`.
///
/// We deliberately do NOT introduce a DI framework / service-locator
/// abstraction — registry singleton is a small thing, matching existing
/// style beats over-engineering. See general's feedback on PR-A draft.
static REGISTRY: OnceLock<UxSinkRegistry> = OnceLock::new();

pub fn registry() -> &'static UxSinkRegistry {
    REGISTRY.get_or_init(UxSinkRegistry::default)
}

#[derive(Default)]
pub struct UxSinkRegistry {
    sinks: RwLock<Vec<Arc<dyn UxEventSink>>>,
}

impl UxSinkRegistry {
    pub fn register(&self, sink: Arc<dyn UxEventSink>) {
        self.sinks.write().unwrap().push(sink);
    }
    pub fn emit(&self, event: &UxEvent) {
        for s in self.sinks.read().unwrap().iter() { s.emit(event); }
    }
}
```

**為何不掛既有 state**：grep 顯示 `resolve_channel()` 每次 call 重讀 `fleet.yaml`，
沒有 TelegramState singleton。新 `OnceLock<UxSinkRegistry>` 是獨立 resource，
對齊 `ACL` / `DETECTED_TZ` / `RT` 的 crate-level static 做法。

**註冊時機**：bootstrap TelegramChannel 建構後 `registry().register(channel_arc.clone())`。
MCP handler 從 `crate::channel::sink_registry::registry()` 拿 `&'static UxSinkRegistry` emit。

### 4.2 FleetEvent enum — 依 plan §4 但 task_id 收窄為 Option

```rust
// src/channel/ux_event.rs — 新增
pub enum FleetEvent {
    // 偏離 plan §4：task_id 從 `TaskId` 改成 `Option<TaskId>`。
    // 理由：delegate_task / report_result handler 沒有強制 TaskId，
    // correlation_id 是 ad-hoc string。Option 是對實況的誠實反映，
    // renderer 有 id 就顯示沒就略。decided by general on design-doc review.
    DelegateTask { from: String, to: String, summary: String, task_id: Option<TaskId> },
    ReportResult { from: String, to: String, summary: String, task_id: Option<TaskId> },
    PostDecision { by: String, title: String, decision_id: DecisionId },
    Broadcast    { from: String, recipients: Vec<String>, summary: String },
}

// UxEvent 新增 variant
pub enum UxEvent {
    UserMsgReceived { .. }
    AgentPickedUp { .. }
    AgentReplied { .. }
    Fleet(FleetEvent),  // 新增
}
```

### 4.3 Hook 點（照 plan §4）

`handlers.rs` 4 個 arm，每個在原 handler 末尾 emit：

| MCP handler | FleetEvent | `from` 來源 | summary 來源 | ID |
|---|---|---|---|---|
| `delegate_task` | `DelegateTask` | `sender.as_str()`（既有 gate） | `args["task"]` 截斷 | `None` |
| `report_result` | `ReportResult` | `sender.as_str()`（既有 gate） | `args["summary"]` | `args["correlation_id"]` parse 成 `Option<TaskId>` |
| `post_decision` | `PostDecision` | `sender`（見下） | `decisions::post` 回傳 title | `decisions::post` 回傳 id |
| `broadcast` | `Broadcast` | `sender.as_str()`（既有 gate） | `args["message"]` 截斷 | N/A |

**`post_decision` 的 Sender：skip-emit，不 gate**。
handler 目前無 `err_needs_identity` gate（anonymous tolerated）。修法：
`if let Some(sender) = .. { registry.emit(UxEvent::Fleet(PostDecision{..})); }`，
anonymous 模式不 emit。**anonymous post_decision 不會出現在 fleet_binding — 這是
intentional：決定 provenance 需要身份。** decided by general on design-doc review
(不 break decisions 的 anonymous contract)。

### 4.4 dispatch 分流

```rust
impl UxEventSink for TelegramChannel {
    fn emit(&self, event: &UxEvent) {
        match event {
            UxEvent::UserMsgReceived { .. }
            | UxEvent::AgentPickedUp { .. }
            | UxEvent::AgentReplied { .. } => {
                // Q1 path: select_action + apply to origin binding
                let action = select_action(event, &self.caps);
                self.apply_q1_action(action);
            }
            UxEvent::Fleet(fe) => {
                // Q2 path: 不走 select_action；fleet_binding 專用 renderer
                self.apply_fleet_action(fe);
            }
        }
    }
}
```

**為何不走 `select_action`**：fleet 事件沒有 cap-degradation ladder（永遠是 send;
target 是 configured binding 不是 origin msg）。把 fleet 混進 `select_action` 會污染
Q1 cap-ladder 語意。未來若真的要 FleetEvent 上 degradation（譬如 fleet_binding 是
SMS），再新增 `select_fleet_action`；現在不做 speculative。

## 5. PR-B — TelegramChannel fleet renderer

`apply_fleet_action(&self, fe: &FleetEvent)`：
1. 拿 `self.state.fleet_binding: Option<BindingRef>`。`None` 時直接 return（plan §7
   "absent block = no fleet sink for that channel"）。
2. `format_fleet_oneliner(fe, self.caps.max_msg_bytes)` → `String`。
3. 解析 `fleet_binding` 的 topic_id → `try_telegram_reply_to_topic(topic_id, &text)`
   （複用現有 send path；不新增 Bot API helper）。

### 5.1 Format（plan §6 S2c）

```
[at-dev-1 → at-dev-2] DELEGATE  task #9 Option C scoping
[at-dev-2 → at-dev-1] REPORT    DONE  src/utils.rs consolidation landed (#21)
[at-dev-3 → *]         BROADCAST  CI green post-rebase
[at-dev-1 solo]         DECISION  task-board-ownership rules (D-42)
```

Pure fn `format_fleet_oneliner(fe, max_bytes) -> String`：
- 截斷 summary 到 max_bytes - 固定 prefix 長度；加 `…` suffix。
- BROADCAST recipients 多時顯示 `[from → *N]` 或 `[from → a,b,…+2]`（>3 recipients）。
- DECISION 有 id 就接 `(D-{id})`；DELEGATE/REPORT 有 task_id 就接 `(#{id})`。

Snapshot test 覆蓋 4 variant、截斷邊界、recipients 0/1/3/5 case。

## 6. PR-C — S2d provenance injection

**Trigger 點**：agent B 收到 delegate 訊息時。

**設計選擇**：inject 不在 `inbox::deliver` 裡做（通用 inbox，會污染所有 message 路徑），
而是在 `delegate_task` handler success 後、於 daemon 本地 call `inject_provenance(target, sender, task)` —
只 DELEGATE 路徑觸發。

```rust
// src/channel/telegram.rs
pub fn inject_provenance(
    target_instance: &str,
    from: &str,
    brief: &str,
) -> anyhow::Result<()> {
    // 送到 target_instance 的 primary topic（既有 instance_to_topic lookup）
    let message = format!("⬅️ from {from} — DELEGATE\n   (brief: \"{brief}\")");
    try_telegram_reply(target_instance, &message)
}
```

handler arm 結尾：
```rust
"delegate_task" => {
    ..
    let result = send_to(&home, sender, target, &msg, "task");
    if let Err(e) = crate::channel::telegram::inject_provenance(target, sender.as_str(), task) {
        tracing::warn!(
            %e, %target, from = %sender.as_str(),
            "S2d provenance injection failed — routing may be broken"
        );
    }
    registry.emit(&UxEvent::Fleet(FleetEvent::DelegateTask { .. }));
    result
}
```

**失敗處理：`tracing::warn!`（非 silent debug）**。decided by general on design-doc
review, overriding initial silent-log bias。理由：provenance 失敗靜默有個風險 —
實際 routing bug（譬如 B 的 topic_id 錯）會無聲劣化 user 體驗而沒 signal。
`warn!` level 讓問題浮到 log 面，但不 propagate error 所以不擋主路徑
（send_to 成功送給 B，user 不受影響；operator 看 log 能察覺 provenance 壞了）。
Regression 成本低：只是 log level 而非邏輯變化。

## 7. Test strategy

### PR-A
- `ux_event` 單測：新增 `FleetEvent` 4 variant 建構 test（smoke）。
- `sink_registry` 單測：register 多個 sink、emit 全體收到、move semantics。
- `handlers` 接線測試：每個 MCP handler invoked → recording sink 收到對應 FleetEvent
  + 欄位值（from=sender, summary=args.*, etc）。Recording sink 是 test-only 的
  `struct Recorder(Arc<Mutex<Vec<UxEvent>>>)`。
- **Negative pin**：`send_to_instance` handler 不 emit FleetEvent（rg-assert not-called）。
  plan §10 明列此 pin。
- **Anonymous pin**：`post_decision` 無 Sender 時 registry 不收到 FleetEvent（§4.3 決定）。

### PR-B
- `format_fleet_oneliner` pure-fn snapshot test：4 variant × 邊界長度。
- `apply_fleet_action` unit test：mock Bot API（走 seam 或 env-guard），`fleet_binding=Some`
  → 觸發 send；`None` → no-op。
- emit 層 integration：餵 `UxEvent::Fleet(..)` 進 TelegramChannel、assert
  mock sink 收到正確 payload。

### PR-C
- `inject_provenance` call 後 `try_telegram_reply` 被以 target + S2d 格式文呼叫（seam 測試）。
- handler-level：`delegate_task` → recording sink 同時觀察到 FleetEvent::DelegateTask +
  provenance side-call（順序無關）。
- **Failure-visibility pin**：`inject_provenance` 失敗路徑被 `tracing::warn!` 記錄
  （§6 決定），用 `tracing-test` 或 `log_capture` 驗證 warn record 存在。

## 8. Non-goals（明列以免 scope creep）

- `send_to_instance` / `request_information` fleet mirror（plan §4 + §8 排除）
- Per-event / per-agent / per-kind routing knobs（plan §7 明列）
- Discord / Slack fleet renderer（Stage B 只寫 Telegram）
- `max_msg_bytes` "view full" hook（defer Stage D-UX）
- typing_indicator / AgentThinking / AgentRateLimited（defer T12）
- Q3 TUI-as-channel（Stage C-UX）
- Auto-create fleet binding（plan §7 排除）
- `FleetEvent` degradation ladder（speculative，未來才加）

## 9. Decisions log（design-doc round 決定點）

| # | Question | Decision | By |
|---|---|---|---|
| Q1 | `task_id: Option<TaskId>` vs plan §4 `TaskId` | **Option** — plan §4 小偏離，誠實反映 correlation_id 非強制 | general |
| Q2 | `post_decision` anonymous 模式 | **skip-emit** — `if let Some(sender)..`，不 break decisions anonymous contract | general |
| Q3 | Registry storage | **crate-level `OnceLock<UxSinkRegistry>`** — 對齊 `ACL`/`DETECTED_TZ`/`RT` 既有 pattern，不發明 DI abstraction | general |
| Q4 | `inject_provenance` 失敗處理 | **`tracing::warn!`** — silent 會讓 routing bug 無 signal（override 初版 silent-log bias） | general |
| — | MCP handler 範圍 | **4 個（delegate/report/post_decision/broadcast）**，`send_to_instance` + `request_information` 照 plan §8 排除 | general |

## 10. Open for PR-A（非 blocker，實作時處理）

- `args["correlation_id"]` parse 成 `Option<TaskId>` 的 parse 失敗：silent `None`（bias），
  或 warn-log？等實作時看 `correlation_id` 實況 format（應該是 "AGD-123" 之類的 string）
  再決定。PR-A commit 會明註選擇。
- Registry `RwLock` 順序：現在只 write 於 bootstrap，read 於 emit。若未來 hot-register
  才需檢查 contention。bootstrap-only write 不會 contend 所以不 block 當前 PR-A。
