# Reviewer Contract v0.1

> AgEnD multi-agent cross-review 協議。首次試跑於 Wave 2（2026-04-22），
> PR #47 與 PR #49 各走完一輪 REJECTED → fix → VERIFIED → merge。
> 本文件固化該協議供新 reviewer agent 加入時引用。

---

## 目錄

1. [三態 Verdict](#三態-verdict)
2. [Reviewer 必做動作](#reviewer-必做動作)
3. [Metadata Schema](#metadata-schema)
4. [Implementer 責任](#implementer-責任)
5. [Orchestrator 責任](#orchestrator-責任)
6. [為什麼 Reviewer 要跨 Backend](#為什麼-reviewer-要跨-backend)
7. [Anti-patterns](#anti-patterns)
8. [Wave 2 案例](#wave-2-案例)
9. [適用範圍](#適用範圍)
10. [版本演進](#版本演進)

---

## 三態 Verdict

| Verdict | 意義 | 後續動作 |
|---------|------|----------|
| **VERIFIED** | Claims 皆可 cross-check；scope 無問題 | Orchestrator merge |
| **REJECTED** (= REQUEST_CHANGES) | 發現實質問題 | 附 findings + fix direction → implementer 修 |
| **UNVERIFIED** | 某 claim 無法獨立驗證（缺 tooling / 環境 / access） | 不 block merge，但顯式聲明 epistemic gap；escalate 給 orchestrator 決定 |

### 為什麼不是二態

二態 pass/fail 讓 reviewer 在「不確定」時預設 pass，製造橡皮圖章。
三態留中性出口——reviewer 可以誠實說「這部分我沒辦法驗」，
把決策權交回 orchestrator，而不是被迫在 pass 和 fail 之間賭一個。

---

## Reviewer 必做動作

### 1. 先讀 diff，再形成意見

打開 PR 後第一件事是 `git diff <base>...<head>`。
不要先讀 PR description、不要先讀 prior conversation。
先看 code，形成獨立判斷，再對照 author 的 claim。

**為什麼：** PR description 是 framing。先讀 framing 會讓你用 author 的視角看 code，
而不是用 code 的事實看 claim。PR #47 的 `receives_edit_events: true` 就是
PR body 沒提、但 diff 裡靜悄悄改了語意的例子。

### 2. 至少一個獨立 verification command

不能只靠 PR body / prompt 內 claim 相信。必須自己跑至少一個驗證指令：

```bash
# 典型 verification commands
git diff <base>...<head>
rg -n "<claim-keyword>" src/
cargo test <module> -q
grep -n "<symbol>" src/**/*.rs
```

PR #49 案例：reviewer 用 `rg -n "display_tag" src/` 發現
`SendText` arm 傳的是人眼標籤而非 instance name，
單靠讀 PR body 不會發現這個 wiring 錯誤。

### 3. Scope guard

Re-review 時明確聲明：

> "Only re-audited the N prior findings; did not broaden review."

防止 round 2 不斷擴大範圍。reviewer 的工作是確認 fix 有效，
不是趁機做 full review。Scope creep 會拖慢迭代、消耗 implementer 信任。

### 4. 回傳 structured metadata

每次 review 結果必須包含下節定義的 metadata schema。
非結構化的「看起來沒問題」不算 review。

---

## Metadata Schema

每次 review 回 `report_result` 時必須包含以下結構：

```yaml
reviewed_files:
  - <path>
  - <path>

verification_commands:
  - `<command>`
  - `<command>`

verdict: VERIFIED | REQUEST_CHANGES | UNVERIFIED

stale_if:
  - <condition that would invalidate this review>
  - <condition>

# 僅 REQUEST_CHANGES 時
findings:
  - [high|medium|low] <finding description + fix direction>
  - ...

# 僅 re-review 且 VERIFIED 時
findings_audited:
  - <original finding>: addressed. <evidence path:line>
  - ...

# 選填
notes:
  - <scope boundary statement, etc.>
```

### 欄位說明

- **reviewed_files** — 實際看過的檔案。沒列的就是沒看，不要假裝全看了。
- **verification_commands** — 跑過的指令。這是 audit trail，也是讓下一個 reviewer 能重現你的驗證。
- **verdict** — 三態之一，不接受其他值。
- **stale_if** — 什麼條件下這份 review 失效。例:「若 `src/ux/sink.rs` 在 merge 前被其他 PR 改動」。讓 orchestrator 知道何時需要 re-review。
- **findings** — 每條標 severity（high/medium/low）+ fix direction。不只說「這裡有問題」，要說「建議怎麼修」。
- **findings_audited** — re-review 時逐條對照原 finding，附 evidence（檔案路徑 + 行號）。
- **notes** — scope boundary、已知限制、或其他 reviewer 想留的備註。

---

## Implementer 責任

### Fix 時加 regression pin

不只修 bug，要加測試 pin 住 wiring 邏輯——「值從哪來 / 誰呼叫誰」。

**案例（PR #49）：** `UxAction::SendText` 的 `instance` 欄位來源是 `event.agent`，
不是 `binding.display_tag()`。Fix 加了 3 個 value-source regression pin：

```rust
// 顯式 assert SendText.instance 來源是 agent 不是 display_tag
assert_eq!(action.instance, "test-agent");  // not "TG#229"
```

這種 pin 的價值：未來 refactor 動到這個欄位的 source 會立刻炸，
不會像原本那樣單測 pass 但 integration 斷層。

### Fix 後 ping orchestrator，不要直接 ping reviewer

Push-notify 的 ownership 在 orchestrator。Implementer fix 完後：

```
report_result → orchestrator (general)
```

由 orchestrator 決定何時、派誰做 re-review。
直接 ping reviewer 會繞過 orchestrator 的排程，製造混亂。

---

## Orchestrator 責任

### Dispatch 時傳完整 context

Re-review dispatch 必須包含：
- 原 findings 列表
- Fix commit hash
- Re-review scope 限定（「只看這 N 個 finding 是否 addressed」）

不要讓 reviewer 自己去翻歷史拼湊 context。

### Verdict 後的動作

| Verdict | Orchestrator 動作 |
|---------|-------------------|
| VERIFIED | Merge |
| REJECTED | 轉 implementer，附 findings |
| UNVERIFIED | 視 claim 性質：補證據 or 升級給人類 |

### 不兼任 reviewer

Orchestrator 分配任務後傾向認為任務被正確完成（confirmation bias）。
獨立 reviewer agent 是必要的，不是奢侈品。

---

## 為什麼 Reviewer 要跨 Backend

Wave 2 的配對是 claude implementer（at-dev-2）+ codex reviewer（at-dev-4）。
這個配對不是隨機，而是針對 **systematic blind spots**：同一 backend 的兩個 agent
容易在相同類型的錯誤上同時失手（譬如同款 prompt 結構下誤讀 cap 語意、
或同款訓練資料產生的 framing bias）。跨 backend 讓 reviewer 的盲點跟 implementer
不重疊。

不是說 same-backend review 無效，而是 cross-backend 對 systematic error
更有抗性。Wave 2 兩次 REQUEST_CHANGES（`receives_edit_events` 語意 drift、
`display_tag` wiring bug）都是 implementer 自己跟 orchestrator（都是 claude）
一起漏掉、codex reviewer 抓到的——這是具體 evidence。

---

## Anti-patterns

### 1. Rubber-stamp review

「看起來沒問題」，無 verification command。
**偵測：** metadata 裡 `verification_commands` 為空或只有 `git diff`。
**後果：** PR #47 的 `receives_edit_events` 語意 drift 就是這樣漏掉的。

### 2. Snapshot decay

Review 基於記憶中的 code 狀態，而非當前 main。
**偵測：** `stale_if` 條件已觸發但沒有 re-review。
**防範：** merge 前檢查 `stale_if` 條件。

### 3. Scope creep

Re-review 時擴大到 non-prior-finding 的範圍。
**後果：** 迭代無限延長，implementer 信任崩潰。
**防範：** re-review 開頭必須聲明 scope guard。

### 4. Silent UNVERIFIED

不說哪裡沒驗就回 VERIFIED。
**後果：** 假裝全驗了，但其實有 epistemic gap 被藏起來。
**防範：** metadata schema 強制列 `reviewed_files`——沒列的就是沒看。

---

## Wave 2 案例

以下兩個案例是本協議首次完整試跑的實證記錄。

### Case A — PR #47：Telegram UX Capabilities

**Round 1：REQUEST_CHANGES**

- **[high]** `receives_edit_events: true` 過度聲明。
  Telegram adapter ingress 只做 `Update::filter_message()`，
  edited messages 未被消費。PR 靜悄悄把欄位語意從
  「adapter 實際 emits」改成「platform 理論能」。
- **[comment]** 48-hour Bot API edit window 措辭錯誤
  （該限制適用 business messages，非 bot-sent 一般 edits）。

**Fix `d356ecc`：**
- `receives_edit_events: false`
- Comment 指名缺的 ingress handler
- Revert caps.rs doc 改動
- 移除 48h 措辭

**Round 2：VERIFIED**（scope held）→ Merge `9da4371`

**Learning：** capability 值 vs adapter 實作的 gap 是典型
「平台理論 vs adapter 實況」drift。需要跨 module grep 驗證才能抓到。

### Case B — PR #49：UxEventSink

**Round 1：REQUEST_CHANGES**

- **[high]** `UxAction::SendText` arm 傳 `binding.display_tag()`
  （格式 `TG#<topic_id>`，人眼標籤）給 `try_telegram_reply`，
  但該函式期待 `instance_name` 去 `config.instances.get()` 查 topic_id。
  Runtime 會 bail `No topic_id for TG#229`。
  `select_action` 單測 pass 但 integration 層沒 cover 這個 wiring。
- **[medium]** Plan §6 Q1 table 的 `typing_indicator` column
  在實作中 silently dropped，doc 未更新。
- **[comment]** PR body 測試數 12/13 與實際 10 不符。

**Fix `993c609`：**
- `UxAction` 三個 variant 加 `instance: String` 欄位
- `select_action` 從 `event.agent` 填入
- 3 個 value-source regression pin
  （顯式 assert `SendText.instance` 來源是 agent 不是 display_tag）

**Round 2：VERIFIED**（scope held）→ Merge `4b7e4a2`

**Learning：** 單元測試 vs integration 斷層是典型盲點。
Reviewer 從 field source 語意切入（display_tag = 人眼標籤不是 lookup key）
發現的 bug。Value-source pin 寫法讓未來 refactor 動到這個欄位會立刻炸。

---

## 適用範圍

### 適用

- Multi-agent 協作的 PR，特別是跨 agent 產出需要 cross-check 的場景
- 涉及 capability 聲明、interface contract、跨 module wiring 的變更
- 任何 implementer 和 reviewer 是不同 agent 的情況

### 不適用

- **Hotfix：** 緊急修復可以 orchestrator 直接 merge，事後補 review
- **Single-author trivial PR：** 純 typo fix、comment 更新、CI config 調整
  不需要走完整 review protocol
- **文件類 PR（docs-skip-PR）：** 如本文件本身，由 orchestrator 審過直接 commit

### 灰色地帶

不確定是否需要 full review 時，orchestrator 決定。
寧可多 review 一次，不要事後才發現該 review 沒 review。

---

## 版本演進

**v0.1 是 minimal viable protocol。**

基於 Wave 2 兩個 PR 的實戰經驗制定，覆蓋了最基本的：
- 三態 verdict
- Reviewer / implementer / orchestrator 三方責任
- Structured metadata
- Anti-patterns 警告

**未來迭代方向（不鎖死）：**

- **Reviewer 品質追蹤：** 定期交叉審核同一份產出，比對兩個 reviewer 的 findings
- **Max iteration cap：** 連續 REJECTED 超過 N 次自動 escalate 人類（目前靠 convention）
- **stale_if 自動化：** 讓 CI 檢查 review 的 stale_if 條件是否被觸發
- **reply_to convention：** `delegate_task` / `request_information` context 裡明確寫 `reply_to: <instance>`，解決臨時 review team 的 routing 問題（side-tab reviewer discussion incident 引出的議題；`c68bf4c` 已部分修 empty `from:` symptom，middleman routing 仍待解）

每次迭代基於實戰反饋，不做預測性設計。
協議改動需要至少一個實際案例支撐，不接受純理論修訂。

---

*v0.1 — 2026-04-22 · 基於 Wave 2 (PR #47, #49) 實戰經驗*
