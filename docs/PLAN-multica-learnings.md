# Multica 借鑑 — 分析與實作計劃

> Date: 2026-04-17
> Status: Planning (not started)
> Source: multica-ai/multica @ 2026-04-17 (Go backend + Next.js 前端, 14.6k stars)
> Scope: 挑出值得抄的設計，忽略與 AgEnD 定位衝突的

---

## 0. TL;DR

兩邊定位根本不同：
- **Multica** 是 Linear/Jira + 託管 agent 的 SaaS 產品，核心資產是 Postgres 資料模型與 Next.js 前端，agent 被降級為「讀 JSON 的 subprocess 工人」。
- **AgEnD** 是 tmux 的 agent-aware 取代品，核心資產是 PTY + VTerm + MCP 匯流排，零設定、本機優先。

所以不是競品。技術底子上 AgEnD 在多 agent 協作這塊（first-class MCP delegate、PTY 狀態機、健康監控）其實比 Multica 強。值得借鑑的是**產品/runtime 層的 5 個技術模式**，與**1 個可選的架構路徑**。

---

## 1. 程式碼比較摘要

### 1.1 規模與語言

| | AgEnD Terminal | Multica |
|---|---|---|
| 語言 | Rust 100%（~20,600 行） | Go 42.9% + TypeScript 55.8% |
| 架構 | 單一 CLI binary + daemon | Monorepo：server (Go) + apps/web (Next.js 16) + desktop + packages/ |
| 後端依賴 | 無（fs2 file lock + JSON） | PostgreSQL 17，47 個 migrations，sqlc |

### 1.2 Agent 執行模型（最大分歧）

| | AgEnD | Multica |
|---|---|---|
| IPC | **PTY**（portable-pty + alacritty_terminal VTerm） | **純 subprocess** `exec.CommandContext` + `StdoutPipe/StdinPipe`，無 PTY |
| 輸入注入 | Mutex-guarded atomic PTY write；對 bubbletea TUI 有 typed_inject 逐位元組 | 讀各 CLI 的 JSON stream（`claude --output-format stream-json`、`hermes acp` ACP JSON-RPC） |
| 自動 dismiss | 內建 trust dialog pattern + 300ms 延遲注入 | N/A |
| 後端數 | 5（Claude、Kiro、Codex、OpenCode、Gemini） | 9（Claude、Codex、Copilot、OpenCode、OpenClaw、Hermes、Gemini、Pi、Cursor） |
| 狀態偵測 | regex + hysteresis，追 Thinking/ToolUse/RateLimit/ContextFull/AuthError/Crashed | 依賴 CLI 自己吐 JSON event |

### 1.3 Daemon / Client 通訊

| | AgEnD | Multica |
|---|---|---|
| Daemon 與誰通訊 | 本機 CLI / TUI attach（Unix socket JSON-RPC + framed stream） | 遠端 server（HTTP poll loop，30s timeout） |
| WebSocket | 不用 | 僅 browser ↔ server（realtime/hub.go） |
| 自動更新 | 無 | Heartbeat response 回 PendingUpdate → server-push |

### 1.4 Inter-agent 協作

- **AgEnD**：first-class。MCP 工具 `delegate_task` / `broadcast` / `send_to_instance` / `report_result` / `request_information` / `inbox`。短訊息直接注 PTY，長訊息 JSONL 排隊。
- **Multica**：**不是 first-class**。真正的 agent→agent 交接走 @mention：`EnqueueTaskForMention(issue, mentionedAgent, triggerCommentID)`。這是 Jira 流程，不是 IPC。整個 server/ 裡 grep `delegate` 只在 `pkg/agent/hermes.go` 把 `"delegate"` alias 成 `execute`。

### 1.5 持久化

- **AgEnD**：全部 JSON/JSONL + fs2 exclusive lock。`snapshot.json`、`tasks.json`、`decisions/*.json`、`inbox/*.jsonl`。
- **Multica**：PostgreSQL + sqlc，47 migrations。關鍵表：`user / workspace / member / agent / issue / agent_task_queue / skill + skill_file + agent_skill / chat / projects / autopilot / activity_log`。
- **pgvector**：Multica Docker image 用 `pgvector/pgvector:pg17`，但 schema 裡**沒有 `CREATE EXTENSION vector` 或 `embedding vector(...)` 欄位**。實際搜尋用 Postgres FTS 索引（migrations 032/033/036/039）。README 寫的是預留，目前等於沒用。

### 1.6 Skills

- **Multica**：真有 skills 系統。`008_structured_skills` migration 建 `skill / skill_file / agent_skill (M:N)`，workspace-scoped；`execenv.SkillContextForEnv` 把 skills 物化到每個 task 的 workdir（例如對 Claude 注入 CLAUDE.md）。
- **AgEnD**：沒有 skills 系統。只有 `instructions.rs` + 根目錄的 `claude-settings.json` / `skills-lock.json`。

### 1.7 多租戶 / Auth

- **Multica**：JWT + PAT（`mul_<40-hex>`）+ Daemon token（`mdt_<40-hex>`），SHA-256 hash 存。`middleware/workspace.go` 強制 workspace scope。
- **AgEnD**：單人單機，file lock + PID 檔，無認證層。

### 1.8 隔離

- **AgEnD**：per-agent git worktree（`{repo}/.worktrees/{instance}/`，branch `agend/{name}`）。
- **Multica**：per-task workdir（`{workspacesRoot}/{task_id_short}/workdir/`），agent 自己呼 `multica repo checkout`。顆粒度不同：AgEnD agent-級，Multica task-級。

### 1.9 Health & Crash Recovery

- **AgEnD**：完整。10 分鐘 crash window、5s→300s 指數退避、最多 5 次、30 分鐘 stability decay、30s hang detection、5 分鐘通知節流、respawn 時剝掉 `--resume` 避免 loop、注入 `[system] Agent restarted…` 訊息。
- **Multica**：task 層級的 fail/retry，沒看到 agent 程序層級類似監控。

---

## 2. 借鑑的實作項目（依優先序）

每項包含：**是什麼 / 原因 / 期望增益 / Trade-off / 粗估工作量**。

---

### Item 1 — Skills 系統（最高優先）

**是什麼**：引入 reusable、可組合、per-agent 授權的 skill 機制。借 Multica 的 `skill / skill_file / agent_skill` 三實體概念，但不用 DB。

**提案目錄結構**：
```
~/.agend-terminal/skills/
  git-review/
    SKILL.md                    # 說明 + 使用情境
    resources/
      checklist.md
      common-smells.md
  telegram-triage/
    SKILL.md
    resources/
      priority-matrix.md
```

`fleet.yaml` 增加：
```yaml
instances:
  reviewer:
    role: "Code reviewer"
    skills: [git-review, telegram-triage]
```

`backend.rs` preset 裡增加 `skill_mount` 策略（per-backend）：
- Claude → 附加到 `CLAUDE.md` 或 `.claude/skills/`
- Codex → `$CODEX_HOME/skills/`
- OpenCode → `.opencode/skills/`
- Gemini → `.gemini/` 或 prompt prefix
- Kiro → 系統 prompt 注入

spawn 時由新模組 `skills.rs`（或併入 `worktree.rs`）把被授權的 skill 物化到 worktree 對應位置。

**原因**：
- 目前使用者要重用「一套 code review 習慣」只能複製貼上 prompt，或把東西硬塞進 `instructions.rs`，沒有組合性。
- Multica 證明 skill 抽象有市場需求；他們強制所有人上 DB 是因為 SaaS 性質，我們可以用純檔案。
- AgEnD 已經有 `skills-lock.json` 這個檔案名但沒有配套機制，這個 lock 就是在等對應的 feature。

**期望增益**：
- 「code review agent」「PR writer agent」「translator agent」變成分享給社群的原子單位，形成 ecosystem。
- `fleet.yaml` 大幅縮短（skill 取代長 role prompt）。
- 同一個 skill 被多個 agent 共用時改一次就好。
- 差異化：Multica 的 skill 綁 workspace，離不開 SaaS；AgEnD 版 skill 是可攜的本機檔案，使用者可以版控、用 git submodule 分享。

**Trade-off**：
- 多一個抽象層，使用者要學 skill 概念（但已經熟悉 role，遷移平滑）。
- Per-backend skill mount 策略要對每個後端維護一份（目前 5 個，可控）。
- `skills-lock.json` 需要定義鎖定語意（版本、hash），不做的話 agent 在不同時間收到不同 skill 內容，行為會漂。
- 安全：載入第三方 skill 等於信任它的 prompt 內容。需要 warning。

**工作量**：中（~800-1200 行）。核心是 `skills.rs` + backend preset 更新 + `fleet.yaml` schema 擴充 + 少量 tests。

---

### Item 2 — Broadcast channel backpressure

**是什麼**：把 `agent.rs` 的 PTY output broadcast 從 **unbounded crossbeam channel** 改成 **bounded + drop-on-full**，參考 Multica `realtime/hub.go` 對慢 WS client 的處理。

**目前程式碼位置**：`src/agent.rs` 的 broadcast subscriber 表。

**改動**：
1. subscriber 的 channel 容量改為 1024 frames（或以 bytes 計的 1MB 上限）。
2. `try_send` 失敗（channel 滿）→ 標記該 subscriber 為 `lagged`，關 socket，從 subscriber 表移除。
3. attach 端收到意外 EOF 時印出明確訊息：`[agend] detached: subscriber fell behind`。

**原因**：
- 目前如果某個 TUI attach client 被 SIGSTOP、機器睡眠、網路斷但 TCP 半開，unbounded channel 會一直累積。
- 你在 `core.lock()` 裡 send 到 broadcast，雖然 crossbeam unbounded 不阻塞，但累積記憶體是真的。
- 一個 agent 一天可以產出數百 MB 輸出，十個 agent × 一個掛掉的 attach client = 幾 GB RSS。

**期望增益**：
- 記憶體上限可預測（agent 數 × subscriber 數 × 1MB）。
- 當機的 attach client 不會拖垮整個 daemon。
- 與未來 GUI frontend（Tauri WebView、遠端 attach）本來就需要這個。

**Trade-off**：
- 慢 client 會被主動踢掉，使用者要重新 attach。但這比 daemon 吃爆記憶體好。
- 1024 frames 的數字需要調：太小會誤踢，太大沒意義。先用 1024 + metric。

**工作量**：小（~150 行 + test）。單檔改動，influence 半徑小。

---

### Item 3 — Per-agent concurrency limit

**是什麼**：在 agent 層級加 `max_concurrent_tasks`（預設 1），claim task 時原子檢查，對齊 Multica 的 `ClaimAgentTask` SQL 語意。

**目前程式碼位置**：`src/tasks.rs` 的 claim 路徑，`src/fleet.rs` 的 agent 定義。

**改動**：
1. `fleet.yaml` agent 定義加 `max_concurrent_tasks: u32`（預設 1）。
2. `tasks.json` 新增欄位（或 agent 狀態表）追蹤每個 agent 的 `in_flight` 計數。
3. `claim_task` 在 `store::mutate` 的原子段裡檢查 `in_flight < max_concurrent_tasks`，否則拒絕 claim。
4. task `done / failed / cancelled` 時遞減。

**原因**：
- 現在 `delegate_task` 可以不斷塞給同一個 agent，變成 PTY 裡的對話排隊，語意不清楚。
- 使用者（或 scheduler、broadcast）很容易意外把 N 個任務同時派給同一 agent。
- Multica 用 SQL transaction 做這件事；我們已有 `store::mutate` 的 fs2 exclusive lock，加欄位就好。

**期望增益**：
- `delegate_task` 回應有明確語意：接受 / 拒絕（原因：忙碌）。
- 派發端（人類、其他 agent）可以做 fallback（改派另一個 agent）。
- 為未來 load balancer 鋪路（round-robin 到 team 裡空閒的 agent）。

**Trade-off**：
- 使用者可能會抱怨「我明明想排隊」。解法：預設 1 改 `unlimited` 就是目前行為，不破壞 backward compat。
- `in_flight` 計數可能在 crash 後不準。需要 daemon 啟動時 reconcile（掃 running tasks 重算）。

**工作量**：小-中（~300 行）。主要是 `tasks.rs` + `fleet.rs` schema + reconcile 邏輯 + integration test。

---

### Item 4 — Token usage 上報與成本追蹤

**是什麼**：借 Multica daemon 的 `POST /usage` 概念，在本機記錄每個 agent 的 token 用量。

**改動**：
1. 每個 backend preset 定義 `usage_extract`（regex 或 state machine），從 PTY 輸出抽出 input/output tokens 與成本。
2. 新增 `$HOME/.agend-terminal/usage.jsonl`，append-only，一行一次詢問：`{timestamp, instance, model, input_tokens, output_tokens, cost_usd}`。
3. 新 MCP 工具 `usage_summary(instance?, since?)` 讓 agent 自己查。
4. Telegram 可選的 daily digest：「今天 fleet 花了 $X.YY，dev $A.BB，reviewer $C.DD」。

**原因**：
- 目前 `state.rs` 有 `RateLimit` / `UsageLimit` 偵測但沒數字，使用者不知道實際花了多少錢。
- 長跑 fleet 的使用者最關心的就是成本，沒這個資料很難判斷是否值得。
- Claude Code 的 `/cost`、Codex 的 usage 行都有輸出，抽取不難。

**期望增益**：
- 成本可觀測。
- 可做「predicted cost 超過 $X 就暫停」這種 budget guard（進階）。
- 社群可以用來做「哪個 agent 性價比最高」的分析。

**Trade-off**：
- 每個後端的 usage 格式不一樣，regex/parser 需要維護。當 CLI 改輸出格式時會默默失準。解法：加單元測試固定 fixture，升級 CLI 版本時 CI 會炸。
- 抽取不到時要 fail silently，不能讓 agent 因為 log parsing bug 就卡住。

**工作量**：中（~500 行）。核心是 extractor + jsonl writer + MCP tool。Telegram digest 是 optional 加值。

---

### Item 5 — 統一 Event Bus

**是什麼**：把 `event_log.rs` 升級成真正的 event bus，所有領域事件（task claimed、agent crashed、schedule fired、ci failure、delegate 完成、usage updated）丟 bus，多個 subscriber 並行消費。參考 Multica `events.Bus` + `cmd/server/*_listeners.go` 模式。

**改動**：
1. 定義 enum `DomainEvent { AgentCrashed, TaskClaimed, TaskDone, ScheduleFired, CiFailed, DelegateCompleted, UsageRecorded, ... }`。
2. Daemon 啟動一個 bus（crossbeam broadcast channel）。
3. 現有 subscribers 重構成獨立 thread：
   - `TelegramNotifier`（吃 AgentCrashed / CiFailed）
   - `SnapshotWriter`（吃任何 state 變更）
   - `UsageAggregator`（吃 UsageRecorded）
   - 未來的 `GuiStreamer`（吃所有，推給 Tauri WebView）
4. 所有 producer（health.rs、tasks.rs、schedules.rs）改為 publish 到 bus，而非直接呼 notifier。

**原因**：
- 現在 `health.rs` → `telegram.rs` 是直接函式呼叫。加 CI notifier 時又要改 `health.rs`。加 GUI streaming 時每處都要改。
- `docs/PLAN-gui-frontend.md` 已規劃 GUI frontend，如果現在不重構，屆時 GUI 整合會非常痛。
- 這是經典的 observer pattern，Multica 的 `events.Bus` 直接證明了在 agent runtime 上下文可行。

**期望增益**：
- 加新 subscriber 不需改 producer（例如「PR 建立時 git push」「agent 狀態變更時 WebSocket push」都只是 new subscriber）。
- 單元測試 producer 只需斷言 `bus.published == [...]`，不需 mock notifier。
- GUI frontend 落地成本大幅下降。

**Trade-off**：
- 重構既有程式碼，PR 會比較大（但可以分階段：先建 bus 框架並新舊並存，再逐個 producer 遷移）。
- broadcast channel 同樣要處理 lagged subscriber（參考 Item 2 同類型處理）。
- 事件定義需要版本管理（enum 加欄位不破壞，刪欄位要小心）。

**工作量**：中-大（~1500 行含重構）。建議排在 Item 1-4 後面做，因為那些功能會變成第一批 event producer/consumer，需求跑過一輪再重構。

---

## 3. 可以選做（中等價值）

### Item 6 — Headless JSON mode 作為 PTY 的並行路徑

**是什麼**：對支援 headless JSON 輸出的後端（Claude `--output-format stream-json`、Hermes ACP），提供 PTY 以外的第二條執行路徑，用於 **one-shot delegate_task / schedule 觸發 / 短任務**。

**改動**：
1. `backend.rs` preset 增加 `headless_mode: Option<HeadlessBackend>`。
2. `delegate_task` / schedule 的 one-shot 任務走 headless：`Command::new(cli).args(["--output-format", "stream-json", "-p", prompt]).spawn()` → 讀 stdout JSON。
3. 互動式 `attach` 仍走 PTY。

**原因**：
- PTY 有啟動 overhead、要處理 dismiss pattern、要解析螢幕文字。對於「丟一個 prompt 收一個答案」的純 worker 任務是 overkill。
- Multica 整個 runtime 就是走這條路，證明可行。
- AgEnD 的優勢是能跑**任何** CLI，包含沒有 headless mode 的；但對有 headless mode 的後端，讓使用者可選走更快的路徑不矛盾。

**期望增益**：
- One-shot 任務延遲降低（省掉 PTY startup 與 dismiss 等待）。
- 解析更穩定（結構化 JSON vs regex screen scraping）。
- 同時保留 PTY 路徑給互動式 / 無 headless mode 的後端。

**Trade-off**：
- 雙路徑意味著 state detection、inbox delivery、usage tracking 在 headless 模式要分別實作。增加維護面。
- 使用者可能困惑「為什麼有時候 attach 看不到 agent 的工作」（因為走了 headless）。需要清楚文件。
- 定位稀釋：AgEnD 的 brand 就是 PTY 玩家，這會變成「小 Multica」。需要主觀判斷是否值得。

**工作量**：大（~2000 行）。建議 Item 1-5 完成後再評估，非必要。

---

### Item 7 — Sanitize 模組

**是什麼**：集中處理外部輸入的清理（ANSI escape 剝除、長度限制、控制字元過濾），借 Multica `server/internal/sanitize/`。

**改動**：新增 `src/sanitize.rs`，統一 `send_to_instance` 訊息、Telegram 轉發到 PTY 的內容、`delegate_task` 的 prompt。

**原因**：
- 目前 MCP handler 對訊息內容基本 trust-by-default。
- 惡意訊息可能藏 ANSI escape（改 terminal title、游標操控）或長 payload 塞爆 PTY buffer。
- Telegram bot 模式下，外人可以傳訊息給 bot，間接注入到 agent PTY。

**期望增益**：
- 安全性提升。
- 為將來 AgEnD 作為 multi-user 或公開 bot 鋪路。

**Trade-off**：
- 過度 sanitize 可能讓合法訊息（code block 裡的 ANSI escape、長 diff）失真。需要 allowlist 而非 denylist。

**工作量**：小（~200 行 + tests）。

---

## 4. 不值得抄（明列原因）

| 項目 | 為何不抄 |
|---|---|
| PostgreSQL / sqlc / 47 migrations | 違反 AgEnD 的 zero-config、本機優先。JSON + fs2 lock 就夠，上 DB 是三倍複雜度。 |
| Workspace / JWT / PAT / daemon token 多租戶 | AgEnD 是單使用者工具，加多租戶等於改寫整個產品定位。 |
| Autopilot | 跟 `schedules.rs` 功能重疊 ≥80%，沒必要。 |
| pgvector | **Multica 自己都沒實際用**（schema 裡找不到 vector 欄位，在用 Postgres FTS）。README 寫假的。 |
| Mention-driven delegation 當主軸 | 這是因為 Multica 沒有真的 agent-to-agent RPC 才變成這樣。AgEnD 有 first-class MCP `delegate_task`，這是**你的優勢**，別退化成 @mention。 |
| Next.js 前端 | `docs/PLAN-gui-frontend.md` 已評估過 Tauri 方向，不需要 web。 |
| HTTP poll loop 取代 Unix socket | 本機場景 Unix socket 更快更簡單。跨機場景才值得引入，目前不是優先級。 |

---

## 5. 推薦實作順序

依「風險低 × 增益高」排序：

1. **Item 2（Broadcast backpressure）** — 單檔改動、修潛在記憶體問題，先做。
2. **Item 3（Per-agent concurrency）** — 小改動、語意補齊。
3. **Item 4（Token usage）** — 使用者感知最強的 feature，差異化明顯。
4. **Item 1（Skills 系統）** — 生態價值最高，但工作量中，建議有上面三項收斂後動。
5. **Item 5（Event Bus）** — 重構項，放在 Item 1-4 產生新 producer 之後再做，避免重構完發現還要再改。
6. **Item 7（Sanitize）** — 跟 Telegram app mode（已在 handover 清單）同時做。
7. **Item 6（Headless JSON mode）** — 最後評估，可能不做。

---

## 6. AgEnD 的不對稱優勢（相對 Multica，不應放棄）

讀完 Multica code 後確認，以下是 AgEnD 目前比 Multica 強、**不應為了學 Multica 而稀釋**的地方：

- **PTY + VTerm 讓 AgEnD 能跑任何 interactive CLI**，不限有 headless mode 的。
- **`delegate_task` / `broadcast` / `inbox` 是 first-class MCP 工具**，Multica 的 agent 只能透過 issue comment 間接 handoff。
- **健康監控（exp backoff、crash window、hang detection、stability decay）**更完整。
- **零設定、零依賴、單二進位**。
- **Per-agent git worktree isolation** 語意清晰（Multica 是 per-task workdir，agent 每次 task 都要重新 checkout）。

Multica 強在**產品面**（資料模型、Web UI、workspace、team、activity log），那些大多不值得抄。上述 7 項借鑑都是**技術底子的補強**，不改變 AgEnD 的 runtime 定位。
