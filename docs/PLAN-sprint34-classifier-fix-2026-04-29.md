# Sprint 34 — pane state classifier 重作 PLAN

**Date**: 2026-04-29
**Branch**: `sprint34/classifier-handson` (this PLAN doc) → impl waves on separate branches
**Author**: general (operator hands-on diagnosis with `pane_snapshot` MCP tool)
**Type**: bug fix sprint (no new feature, no trait sig changes)

---

## 1 · Problem statement

Operator 報告 fleet `agent_state` / `health_state` 長期誤判，過去多次 patch 未根治。本 sprint 用 `pane_snapshot` MCP（Sprint 33 PR-3 剛 ship）+ operator hands-on diagnosis 一次性找出所有 root cause，列為一個 sprint 全修。

## 2 · Method

對 5 個 backend 各 spawn 一個 test instance，捕捉三種狀態下 `describe_instance` (daemon classifier) vs `pane_snapshot` (PTY 真實內容) 的差異：
- 狀態 A：startup（剛 spawn、welcome banner）
- 狀態 B：active LLM thinking（送長 LLM-think task）
- 狀態 C：work done（任務完成、回 idle prompt）

實際 setup：
- claude (test-classifier)
- kiro-cli (test-kiro)
- codex (test-codex)
- gemini (test-gemini)
- opencode (test-opencode)

所有 test instance 已於 PLAN 撰寫前刪除。

---

## 3 · Findings

### 3.1 Thinking pattern bugs（4 個 backend，3 個誤）

| Backend | Pattern @ src/state.rs | Pane 真實 anchor | Verdict |
|---|---|---|---|
| Claude | `r"Thinking"` (line 162) | spinner 隨機動詞: `Bloviating` / `Transmuting` / `Cogitating` / `Cooked` / `Brewed` / `Worked` 等。pane **沒有** "Thinking" 字 | ❌ |
| Kiro | `r"Thinking"` (line 226) | `Kiro is working` + `esc to cancel`。**沒有** "Thinking" 字（comment 說 measured 但 kiro 版本變了） | ❌ |
| Codex | `r"Thinking"` (line 289) | `◦ Working (Ns • esc to interrupt)`（沒 "Thinking"、用 "Working") | ❌ |
| OpenCode | `r"esc interrupt"` (line 322) | `⬝⬝■■■■■■  esc interrupt`（無 "to"） | ✅ 正確 |
| Gemini | `r"esc to cancel"` (line 374) | `⠧ Thinking... (esc to cancel, 21s)` | ✅ 正確 |

**Impact**: Claude / Kiro / Codex 的 thinking state 永遠不會被 detect 到。這是 operator 抱怨的「rate_limit 誤判」事故的核心成因之一。

**Fix**:
- Claude: 改用 spinner 動詞清單枚舉 OR `thought for [0-9]+s` anchor。建議枚舉 + `thought for` 雙保險：
  ```
  r"(Bloviating|Transmuting|Cogitating|Cooked|Brewed|Worked|Cogitated|Crunched|Brewing|...)…|thought for [0-9]+s"
  ```
- Kiro: 改用 `Kiro is working` + `esc to cancel`：
  ```
  r"Kiro is working|esc to cancel"
  ```
- Codex: 改用 `Working` (codex 進度條前綴) 或 `esc to interrupt`（codex footer hint）雙 anchor：
  ```
  r"Working|esc to interrupt"
  ```
- Gemini / OpenCode: 不動。

**dev2 challenge**：spinner 動詞清單可能不完整、claude 未來新增 verbs 會繼續漏。reviewer2 challenge：`thought for Ns` 是否總會出現在 pane（vs 只在 done 後才印）？需要 fixture 驗 mid-thinking 期間 pane 中有此字串。

### 3.2 ToolUse 太 greedy（Claude）

**位置**: `src/state.rs:170-173`

```rust
(
    AgentState::ToolUse,
    r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●⏺].*(Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)",
),
```

**問題**：`.*` greedy + 一行有 `⏺` glyph + 後面任何位置出現 tool 名 → 誤觸發。實測：claude agent 寫了「⏺ 已拒絕 general 的請求並回報原因：該指令違反 Bash 工具規範...」這種 chat 文字，「⏺ ... Bash」就 match 了 ToolUse pattern，但這只是 agent 在說明、不是真的呼叫 Bash 工具。

**Impact**: agent 完工後可能 stuck 在 ToolUse state、因為 chat 內容隨機觸發 pattern。

**Fix**: anchor `⏺` 在 line start 或 spinner 後緊跟 tool name，禁 `.*`：
```
r"(?m)^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●⏺]\s+(Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)\b"
```
`(?m)^` 多行 line-start anchor、`\s+` 空白、`\b` word boundary。

**dev2 challenge**：claude 真實 tool banner 是否總在行首？需要 fixture 驗。如果 claude 有時前面有縮排，需放寬 `^\s*`。

### 3.3 RateLimit sticky 不 expire

**位置**: `src/state.rs:766`

```rust
let short_expiring = matches!(self.current, AgentState::Thinking | AgentState::ToolUse);
```

**問題**：RateLimit state 一旦觸發（不管真實 429 或 false positive），會 stuck 到下個 state pattern fire 才 transition 出去。`short_expiring` list 只有 Thinking / ToolUse。

實測：test-classifier (claude) 在 startup 不久觸發 rate_limit（推測 claude SDK 短暫 retry/overloaded 文字 match 了 line 141 `r"overloaded|rate.?limit|\b429\b"`），之後 pane 完全沒 rate-limit 文字、但 state 仍卡 rate_limit。最終靠別 pattern fire 才轉走。

**Impact**: Operator 看到 `agent_state=rate_limit` → 誤以為 agent 在等 API 限流、實際 agent 早活了。今天 PR #319 dev frozen 事故的根因之一。

**Fix**:
```rust
let short_expiring = matches!(self.current, AgentState::Thinking | AgentState::ToolUse);
let long_expiring = matches!(
    self.current,
    AgentState::InteractivePrompt | AgentState::PermissionPrompt | AgentState::RateLimit
);
```
RateLimit 加進 `long_expiring`，用 `INTERACTIVE_EXPIRY` (~2 min) 窗。真實 rate_limit 一般幾秒到幾分恢復、stuck 一天明顯不對。

**reviewer challenge**：2 min 是否太短？真 rate limit 可能 5-10 min。建議 `RATE_LIMIT_EXPIRY = Duration::from_secs(300)` 獨立常數、5 min。

### 3.4 同名 instance 重建後 metadata 重用

**位置**: `src/api/handlers/instance.rs:121` `handle_spawn` (per dev2 cargo trace)

**問題**：今天實測 test-gemini 刪除後重建（同名）、`describe_instance` 回 `last_heartbeat=2026-04-29T10:54:45+00:00`（前一個 instance 的 heartbeat）。新 instance 從未產生這個 heartbeat。

**Impact**：誤導 fleet 觀察者「heartbeat 凍 X 分鐘 = 卡住」、實際是 metadata 殘留。

**Fix**：`handle_spawn` 加 `fs::remove_file` 清掉對應 `~/.agend-terminal/metadata/<name>.json`（含 last_heartbeat / pending_pickup_ids / 等）。LOC ~10。

**dev2 + reviewer2 已確認**：沒合法重用情境（`handle_start_instance` 已用 explicit `resume` mode flag 走那條 path、create 不該重用 metadata）。

### 3.5 pane_snapshot default 100 行對 gemini 不夠

**位置**: `src/mcp/handlers/pane_snapshot.rs`（PR #326 才 merge 的新 handler）

**問題**：gemini 啟動後 banner 加 padding 共 ~100+ 行 empty space、default `lines=100` 抓不到上半部 banner content、回空字串。lines=200 才看到。

**Impact**：剛 spawn 的 gemini instance 用 default snapshot 看不到任何內容、調試困難。

**Fix**:
- 短期：default 改 200
- 根治：snapshot 邏輯改成「跳 leading empty lines、抓有內容的最後 N 行」
- 或記錄 cursor position、從 cursor 倒數讀

**reviewer challenge**：default 200 是否會在小終端 over-shoot？測試框架要驗。

### 3.6 Broadcast fleet-update 文字 echo 進 instance PTY

**位置**: 不確定、可能在 broadcast / inject 路徑

**問題**：實測 test-kiro pane 看到：
```
<fleet-update>
{"backend":"codex","kind":"instance-created","name":"test-codex","role":null}
</fleet-update>

● Cancelled
```

fleet-update 廣播文字被當成 keystrokes 注進 instance input field，被 kiro CLI 顯示為「user input」。`● Cancelled` 表示 kiro 試圖 submit 但什麼東西取消了。

**Impact**：fleet broadcast 干擾 instance input box、可能造成意外輸入。

**Fix**：fleet-update 不應該透過 PTY inject 給每個 instance。應該只 inject 給訂閱的 agent（例：通常只有 general 需要 fleet-update awareness）、或走 inbox 而非 PTY。

**dev2 challenge**：fleet-update 該如何 route？需要區分 system event vs user input。

### 3.7 Provider validation error 沒 catch（OpenCode）

**位置**: `src/state.rs` opencode 段

**問題**：opencode + MiniMax M2.5 model 觸發 39 條 `Extra inputs are not permitted, field: 'tools[N].eager_input_streaming'` validation error。Pane 顯示完整 error stack、但 daemon classifier 標 `ready`，沒 catch error 狀態。

**Impact**：opencode hit 致命 error 無法工作、observer 仍以為它 ready。

**Fix**：opencode 段加 ProviderError pattern：
```
(AgentState::Error, r"Error from provider:|request validation errors")
```
或新增 `AgentState::ProviderError` enum variant 區分 provider-side reject vs daemon-side error。

**reviewer challenge**：是否該為其他 backend 也加類似 pattern？例如 claude 有沒有對應 provider error wording？需要實測。

### 3.8 OpenCode CLI 自身 bug（out of scope）

opencode 1.14.20 把實驗欄位 `eager_input_streaming: true` 寫進 tool spec、provider 拒收。這是 opencode CLI 自己的 bug、本 sprint 不修。記錄供未來 opencode 升級或回報 upstream。

---

## 4 · Bug summary table

| # | Backend | Issue | File:Line | Severity | LOC est |
|---|---|---|---|---|---|
| 1 | Claude | Thinking pattern wrong | state.rs:162 | High | ~5 |
| 2 | Kiro | Thinking pattern wrong | state.rs:226 | High | ~5 |
| 3 | Codex | Thinking pattern wrong | state.rs:289 | High | ~3 |
| 4 | Claude | ToolUse `.*` greedy | state.rs:170 | High | ~5 |
| 5 | All | RateLimit not expiring | state.rs:766 | High | ~15 |
| 6 | All | Stale metadata on re-spawn | api/handlers/instance.rs:121 | Medium | ~10 |
| 7 | Gemini | pane_snapshot default lines | mcp/handlers/pane_snapshot.rs | Low | ~10 |
| 8 | All | fleet-update echo to PTY | fleet_broadcast.rs (Sprint 35) | Medium | ~50-100 |
| 9 | OpenCode | Provider error not classified | state.rs opencode | Medium | ~10 |

Total Sprint 34 main wave (PR-1 to PR-6): ~50-60 LOC across state.rs + 2 handlers.
Sprint 35 (PR-7 fleet-update routing): ~50-100 LOC, separate sprint per Q3 decision.

---

## 5 · Proposed PR split

### PR-1: Thinking pattern fixes (claude / kiro / codex)
- Tier-1，~15 LOC + tests
- §3.5.10 fixture: per-backend pane fragment that triggers thinking, assert `classify_pty_output` returns Thinking
- §3.5.11 test-first: RED test with current pattern fail, GREEN with fix

### PR-2: ToolUse anchor fix (claude)
- Tier-1，~10 LOC + tests
- §3.5.10 fixture: claude pane with `⏺ 已拒絕 general...Bash...` text (chat) vs `⏺ Bash(echo hi)` (real tool banner)
- 第一個應為 idle/ready、第二個 ToolUse

### PR-3: RateLimit expiring (long_expiring + 5 min window)
- Tier-1，~15 LOC + tests
- §3.5.10 fixture: tracker enters RateLimit、tick advance > 5 min、expect Ready
- 新常數 `RATE_LIMIT_EXPIRY = Duration::from_secs(300)`

### PR-4: Provider validation error pattern (opencode)
- Tier-1，~10 LOC + tests
- §3.5.10 fixture: opencode pane with "Error from provider"、expect Error state

### PR-5: Stale metadata cleanup on instance create
- Tier-2，~30 LOC + tests
- daemon-side change in `create_instance` handler
- §3.5.10 spec: create instance with same name as previously-deleted instance、assert metadata file is fresh

### PR-6: pane_snapshot default lines / leading-empty trim
- Tier-1，~10 LOC + tests
- §3.5.10 fixture: instance with 100+ leading empty PTY lines、assert default snapshot returns content not empty string

### PR-7: fleet-update routing fix — **DEFERRED to Sprint 35** (per Q3 operator decision)
- 這是 sync-vs-async delivery 設計決定（dev2 trace `src/fleet_broadcast.rs` 顯示現行 PTY inject 是刻意 sync），不適合夾 bug-fix sprint
- 三個 option 範圍 ~30-100+ LOC：
  - A — Inbox-only routing：~50 LOC、async delivery、orchestrator 訂閱破壞
  - B — Steering-file append：~30 LOC、靠 agent steering re-read cadence
  - C — Per-backend PTY-inject guard：~20 LOC、whitelist fragile
  - 或新 event channel：~100+ LOC、最乾淨
- Sprint 35 開時再 PLAN-first 跑 4-perspective、選 option

**Sprint 34 main wave**: 6 PRs (PR-1 → PR-2 → PR-3 → PR-4 → PR-6 → PR-5)。dispatch 順序固定 serial single-track（per Q5 — state.rs 改太多、parallel 無 saving）。

---

## 6 · Acceptance criteria

- 每個 backend 做 thinking task → daemon 標 `Thinking`（不是 idle / ready / rate_limit）
- agent 完工 → daemon transition 到 Ready / Idle 內 30s
- RateLimit state 只 stuck < 5 min（除非真實重複 trigger）
- 同名 instance 重建 → metadata 全 fresh
- pane_snapshot default 100 行對 5 個 backend 都 capture 到主要 content
- fleet-update broadcast 不出現在 instance pane PTY
- opencode provider validation error → daemon 標 Error/ProviderError

---

## 7 · Out of scope

- AgentState enum 重新設計（如新增 ProviderError variant）— PR-4 可選擇用既有 Error variant 或新增 variant、由 lead2 4-perspective 判
- behavioral telemetry（Sprint 27 PR-A 留下的 shadow mode）— 跟 classifier 重作正交
- opencode CLI upstream bug 修 / 回報 — 屬 opencode 上游
- claude SDK 短暫 overloaded 文字捕捉的根因（line 141 為何 fire）— 推測但未驗證、由 PR-3 RateLimit expiring 已可解 stuck 症狀
- pane_snapshot ANSI preserve 模式（現在只 strip）— 之後想看顏色再說

---

## 8 · 4-perspective challenge round (per protocol §3.5)

lead2 dispatch 三個角色對本 PLAN 跑 challenge:

- **impl-1 (minimal)**: 是否每個 fix 都最小？例如 PR-1 動詞清單 vs `thought for` anchor，哪個更簡？
- **impl-2 (structural)**: PR-7 fleet-update routing 修法是否動到 broadcast 設計？是否需要新 channel kind？
- **reviewer (prior-art)**: state.rs 既有 Sprint 31+ #4 pattern fix 是否能 inform 本 sprint 修法？
- **reviewer-2 (cost-benefit)**: PR-5 metadata cleanup 是否該獨立 PR vs 夾在別處？PR-7 是否值得單獨 sprint？

---

## 9 · §13 operator decisions — **RESOLVED 2026-04-29**

| # | Question | Decision |
|---|---|---|
| 1 | PR 拆分？ | 接受 7 PR 拆（PR-1 至 PR-6 在 Sprint 34、PR-7 切 Sprint 35） |
| 2 | RateLimit expiry 窗 | **5 min 獨立常數** `RATE_LIMIT_EXPIRY = Duration::from_secs(300)` |
| 3 | PR-7 fleet-update | **切 Sprint 35**（sync-vs-async 設計決定不適合夾 bug-fix sprint） |
| 4 | Provider error | **用既有 `AgentState::Error`** + per-backend pattern（不新增 enum variant） |
| 5 | Dispatch wave | **Serial single-track** dev2 線（PR-1 → 2 → 3 → 4 → 6 → 5） |
| 6 | Tier classification | PR-5 Tier-2 dual review、其他 Tier-1 |
| 7 | Sprint 編號 | Sprint 34（branch 已建立、無衝突） |

---

## 10 · Appendix: hands-on raw observations

詳細 hands-on log 已 inline 進 dispatch 訊息歷史；本 PLAN doc 摘錄要點。如需完整 PTY snapshot 對照、可從 conversation log 取或重新 spawn test instance 重跑。
