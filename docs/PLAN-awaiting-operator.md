# PLAN: AwaitingOperator — 啟動卡住的泛用救援機制

**Branch:** `feat/awaiting-operator`
**Worktree:** `/Users/suzuke/Documents/Hack/agend-terminal-awaiting-op`
**Date:** 2026-04-19

---

## 1. 背景與問題

agend-terminal 以 PTY 包裝 CLI agent（claude、codex、gemini 等）。部分 CLI 啟動時會丟互動式 prompt（例如 codex v0.120.0 → v0.121.0 的更新提示：`1. Update now / 2. Skip / 3. Skip until next version`）。

**症狀：** agent 卡在等 stdin，永遠達不到 `ready_pattern`。`health.rs::check_hang` 要 120 秒才把 `Starting → Hang`，期間 orchestrator 無法判斷是「CLI 在等人」還是「CLI 死了」。

**通病本質：** 這不是 codex 特有，是「orchestrator 無法預料每個 CLI 的所有互動 prompt」的架構問題。未來每出現一家新 CLI 就要加一套 preflight workaround，不可持續。

---

## 2. 已否決的替代方案

| 方案 | 問題 |
|------|------|
| 寫 `~/.codex/version.json` 的 `dismissed_version` | 污染使用者本機 codex，使用者也不會看到更新提示 |
| 隔離 `CODEX_HOME` per agent | 每家 CLI 要研究 config 格式、要 sync auth，每個新 backend 要新邏輯 |
| agent spawn 時送 `--no-update-check` | codex 不支援這類 flag（已驗證 `codex --help`、`codex features list`、config.toml） |
| pattern match + 自動送 `3\n` | fragile，每家 CLI prompt 格式不同，容易誤觸其他 prompt |
| 把 codex 換成 `codex exec` 非互動模式 | 失去 TUI 能力，非使用情境 |

**共同缺陷：** 都是「per-CLI workaround」，累積 CLI 特定知識在 orchestrator。

---

## 3. 設計

### 3.1 核心觀念

**把「啟動卡住」視為「需要 human 介入的對話」。** Orchestrator 不需要認識每家 CLI 的 prompt 格式，只需要偵測到「卡住了」並把畫面轉送給 operator。

### 3.2 新增狀態

在 `AgentState` enum 加 `AwaitingOperator` variant，定義：

- **何時進入：** `Starting` 狀態 + 最後一次 stdout 輸出後靜默超過閾值（預設 3 秒）
- **何時離開：** `ready_pattern` 匹配 → 正常轉 `Ready`（現有邏輯）
- **何時不離開：** 靜默不再觸發（已離開 `Starting`）；使用者無回覆也保持此狀態（不自動 Crashed）

### 3.3 資料流

```
spawn agent
   ├─ PTY stdout → state.feed() 持續偵測 ready_pattern
   ├─ PTY stdout → health.last_output = now (現有)
   └─ daemon tick loop (500ms):
        └─ state == Starting && (now - last_output) > silence_threshold
             → state := AwaitingOperator
             → telegram: 推 vterm tail (40 行) + 提示「回覆將寫入 stdin」
             → (可選) TUI 狀態列顯示 ⚠️

operator 在 Telegram topic 回覆 "3"
   └─ telegram::handle_message
        ├─ 查目標 agent 當前狀態
        ├─ if state == AwaitingOperator:
        │     → API call INJECT_RAW { data: "3\n" }
        │     → daemon handler → agent::write_to_agent (raw PTY write, 無 inbox 包裝)
        └─ else: 現有 inbox 路徑

agent 收到 "3\n" → 處理 → 繼續 boot → 觸發 ready_pattern
   └─ state.feed() → AwaitingOperator → Ready
   └─ telegram: 推「✅ agent 已就緒」
```

### 3.4 已 resolve 的設計辯論

| 議題 | 結論 | 理由 |
|------|------|------|
| 超時邏輯做在哪 | `health.rs` tick loop（不是 `state.rs::feed`） | `feed()` 事件驅動；agent 靜默時根本不會觸發，永遠測不到超時 |
| 超時判準 | 「最後 stdout 後靜默 N 秒」 | 語意正確（agent 已講完話在等），比「絕對 timeout」對慢 boot 的 agent 更友善。`health.last_output` 欄位已存在 |
| 寫入格式 | 新增 `INJECT_RAW` API method | 現有 `INJECT` 經 `inbox::notify_agent` 包成 `[source] text<reply_hint><submit_key>`，codex 的 `3\n` 會變 `[telegram:@user] 3...\r`，無法解析 |
| tail 節流 | 進入時推一次；operator 回覆後若仍 `AwaitingOperator` 再推一次 | 避免 agent 定期 heartbeat 洗版 Telegram；給 operator 穩定快照 |
| 退出條件 | 僅 `ready_pattern match` | 不加 idle timeout；operator 離開 30 分鐘回來還能接手。需手動 `/kill` 清理 |

---

## 4. 改動地圖（驗證過的錨點）

| 檔案 | 動作 | 錨點 | 估計行數 |
|------|------|------|---------|
| `src/state.rs` | 加 `AwaitingOperator` enum variant | `:17-32` enum / `:36-53` priority / `:56-58` is_error / `:65-82` display_name | ~15 |
| `src/health.rs` | 加 `check_awaiting_operator(state, silent)` 方法（與 `check_hang` 並列） | `:140` `check_hang` 旁邊 | ~20 |
| `src/daemon.rs` | tick loop 加超時觸發 → 推 Telegram + 狀態轉換 | `:397` 現有 `check_hang` 呼叫點旁邊 | ~40 |
| `src/api.rs` | 新增 `INJECT_RAW` method 常數 | `:78-92` `pub mod method` | ~2 |
| `src/daemon.rs` (api handler) | 新增 `INJECT_RAW` 分支 → `agent::write_to_agent(data)`（bytes 原樣） | 現有 `INJECT` handler 旁 | ~15 |
| `src/telegram.rs` | `handle_message` 分岔：查狀態決定走 `INJECT_RAW` 或現有 inbox | `:214` `handle_message` | ~30 |
| `src/telegram.rs` | 新增 `notify_awaiting_operator(agent, tail)` 推 tail + 提示文案 | 新函式 | ~30 |
| `src/vterm.rs` | 新增 `tail_lines(n: usize) -> String` helper | 檔案新增 | ~30 |
| `tests/` | 新增狀態轉換 / INJECT_RAW / tail 單元測試 | 依現有 test 風格 | ~80 |

**合計估計：** ~260 行新增、7 檔修改 + 新測試、0 破壞性改動。

---

## 5. 實作順序（small commits，每步可獨立跑測試）

1. **[test-first] AwaitingOperator enum variant + 所有 match arm 補齊**
   - 4 個 match 補完（priority、is_error、display_name、現有測試）
   - `priority = 1` 或 `1.5`（插在 `Hang=1` 和 `Ready=2` 之間 → 需要重排 0..14）
   - 決定：是否 `is_error() = false`（不是錯，只是等 human）→ 傾向 false
   - Commit: `feat(state): add AwaitingOperator variant`

2. **[test-first] `health::check_awaiting_operator`**
   - 簽名 `(state: AgentState, silent: Duration) -> bool`
   - 邏輯：`state == Starting && silent > threshold`
   - 閾值：常數 `AWAITING_OP_SILENCE = Duration::from_secs(3)`（之後可做成 fleet.yaml 可覆寫）
   - 測試：`Starting` + 2s → false；`Starting` + 4s → true；其他 state 不管多久都 false
   - Commit: `feat(health): detect AwaitingOperator via stdout silence`

3. **vterm `tail_lines`**
   - 從 vt100 grid 讀最近 N 行，剝除 ANSI escape，join `\n`
   - 測試：餵已知 frame，assert output
   - Commit: `feat(vterm): expose tail_lines(n) helper`

4. **`INJECT_RAW` API method**
   - `api.rs` 加 const
   - `daemon.rs` handler 加分支，直呼 `write_to_agent`
   - 測試：透過 socket 送 INJECT_RAW，assert PTY 收到的 bytes 完全等於 payload（無 prefix/suffix）
   - Commit: `feat(api): add INJECT_RAW method for bypass-inbox writes`

5. **`telegram::handle_message` 分岔**
   - 查 agent 狀態
   - `AwaitingOperator` → `INJECT_RAW` + append `\n`
   - 否則 → 現有路徑（零回歸）
   - 測試：mock 狀態 = AwaitingOperator，assert API call 是 INJECT_RAW
   - Commit: `feat(telegram): route raw keystrokes when agent awaits operator`

6. **`telegram::notify_awaiting_operator` + tick loop 串接**
   - `daemon.rs` tick loop：偵測到 `Starting → AwaitingOperator` 轉換 → 呼叫 telegram 推 tail
   - operator 回覆後若仍 AwaitingOperator → 再推一次
   - 文案：`⚠️ {agent} 啟動後 {N}s 無回應，可能在等互動\n\n<pre>{tail}</pre>\n\n💬 回覆直寫 stdin`
   - 測試：手動煙霧測試（沒有 Telegram 單元測試 harness）
   - Commit: `feat(telegram): push vterm tail on AwaitingOperator entry`

7. **煙霧驗證腳本 `scripts/verify-awaiting-operator.sh`**
   - 起一個 shell backend agent，故意不送 ready_pattern
   - assert 3 秒後 daemon `status` API 回傳 `awaiting_operator`
   - 送 INJECT_RAW 一個 "\n"，assert PTY 收到
   - Commit: `test: verification script for AwaitingOperator flow`

8. **（可選）fleet.yaml `ready_silence_secs` 覆寫**
   - defaults 和 per-instance 都支援
   - 預設 3s
   - Commit: `feat(config): per-backend ready_silence_secs override`

**合併策略：** 全部累積在 `feat/awaiting-operator` 分支，最後本地 merge 回 main（按 `feedback_local_merge_no_pr.md`，純本地 / 無 CI 特徵）。不開 PR。

---

## 6. 測試策略

- **單元：** state priority 排序、check_awaiting_operator 邊界、vterm tail_lines、handle_message 分岔邏輯
- **整合：** 透過 API socket 的 INJECT_RAW round-trip（起 dummy agent → 送 bytes → 讀 PTY 輸出）
- **煙霧：** `scripts/verify-awaiting-operator.sh`（shell agent + 延遲 ready）
- **回歸：** 現有 `scripts/verify-*.sh` 全部 pass、`cargo test` 全綠、`cargo clippy -- -D warnings` 乾淨
- **手動：** 用真實 codex v0.120.0 啟動時重現（若本機已 dismiss 更新可改 `version.json` 重現）

---

## 7. 風險與開放問題

| 風險 | 影響 | 緩解 |
|------|------|------|
| 3 秒閾值對真的慢啟動的 agent 誤觸 | operator 收到誤警 | fleet.yaml 可覆寫；預設偏保守可拉到 5s |
| vterm grid 在 CLI 用 alt-screen 時讀到舊內容 | tail 顯示不準 | 先讀 primary screen；不行再讀 alt screen（vt100 crate 應都有） |
| Telegram 中「回覆覆蓋到其他 topic」誤送到錯 agent | 使用者送錯字到錯 stdin | 現有 topic routing 已處理，`handle_message` 已綁定 agent_name |
| INJECT_RAW 被濫用繞過未來可能加的 authz | 安全風險 | Stage 3 的 Telegram allowlist 已擋；INJECT_RAW 不新增攻擊面 |
| operator 打的字含 emoji / unicode → 進 PTY 亂碼 | bytes 轉 PTY 格式錯 | Telegram API 本來就 UTF-8，PTY 接受 UTF-8，應無問題；加 test case |

**開放問題（寫 code 時 resolve）：**
- `AwaitingOperator` 的 `priority()` 應該是 1.5 還是插進 ordering 重排？傾向重排（乾淨）
- 第一次進 `AwaitingOperator` 時是否也要推 Telegram 給 `general` topic（全域告警）？傾向不要，單 topic 就夠

---

## 8. 不在範圍內（明確排除）

- **per-CLI 的更新 / 登入 / trust prompt 自動處理**：本方案取代它們
- **non-PTY / headless / CI 情境**：用 `codex exec` 這類非互動 subcommand，不走 PTY TUI
- **GUI frontend 的 AwaitingOperator UX**：本次只做 Telegram + TUI 狀態列；GUI 之後單獨跟進
- **Windows 支援的平台差異**：共用同一套邏輯（state machine 不分平台）
- **Plan doc 以外的文件更新（README、architecture.md）**：合併前最後一步再補

---

## 9. 預計驗收清單

- [ ] codex 更新提示情境：10 秒內 Telegram 收到 tail + 回 `3` 後 agent 正常 boot
- [ ] 正常啟動（無 prompt）情境：agent 在閾值內 Ready，從不進 AwaitingOperator
- [ ] 多 agent 並發卡住：各自的 topic 獨立推送，互不干擾
- [ ] 全套 `scripts/verify-*.sh` 通過
- [ ] `cargo test` 全綠（預期 ~285+ 個通過）
- [ ] `cargo clippy -- -D warnings` 無 warning
