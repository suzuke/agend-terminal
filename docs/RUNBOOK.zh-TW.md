[English](RUNBOOK.md)

# Incident Runbook — 事故復原手冊

以症狀為導向的復原指南。這裡的每一條命令都在真實 deployment 上實際跑過才寫下來。`$AGEND_HOME` 預設為 `~/.agend-terminal`；請全文替換成你自己的路徑。

**先找到 log。** Log 檔以 **UTC** 日期命名並每日輪替——跑 TUI 時（`agend-terminal app`，最常見的 deployment）是 `app.<YYYY-MM-DD>.log`，跑 headless 的 `agend-terminal start` 時則是 `daemon.<YYYY-MM-DD>.log`。當天的檔案要等到該 UTC 日期第一次寫入後才會出現，所以剛過 UTC 午夜時「今天」的檔案可能還不存在。最穩當的抓法是抓最新的那一個：

```sh
ls -t ~/.agend-terminal/app.*.log ~/.agend-terminal/daemon.*.log 2>/dev/null | head -1
```

另外兩個你會反覆遇到的檔案：

- `event-log.jsonl` — 每行一筆 operator 可見的事件（daemon 的「你應該知道的事」頻道）。用 `grep <kind>` 來查。
- `state-transitions.jsonl` — 每一次 agent 狀態變更，附帶時間戳和一段畫面片段（見 §2）。

---

## 1. Daemon 起不來 / 反覆 crash

**症狀**：`agend-terminal app`（或 `start`）一啟動就立刻退出，或是 agent 不斷死掉又重生。

**診斷**

```sh
LOG=$(ls -t ~/.agend-terminal/app.*.log ~/.agend-terminal/daemon.*.log 2>/dev/null | head -1)
grep -E " ERROR " "$LOG" | tail -20
agend-terminal doctor          # home dir / .env / fleet.yaml / live agents, all checked
```

crash 處理機制在 log 裡長這樣：每次 agent crash 會記下 `crashed`，重生會帶 backoff 延遲，累積 **5 次 crash** 之後該 agent 的健康狀態變成 `Failed`——不再重生，只發一則通知。如果是 orchestrator，那會是像這樣的一行：

```
self-orchestrator PERMANENTLY FAILED (respawn budget exhausted) — escalating terminal P0
```

crash 預算的狀態**會跨 daemon 重啟保留**（這樣就不會有人不小心用重啟來重置一陣 respawn 風暴）。如果連保存這個狀態本身都失敗了（例如磁碟滿了），你會在 ERROR 等級看到 `escalation_persist: write FAILED`，外加一筆 `escalation_persist_failed` 的 event-log 紀錄——這種就當作「先把磁碟修好」來處理。

**復原**

```sh
agend-terminal stop            # graceful: suppresses crash handling during shutdown
agend-terminal app             # or: agend-terminal start (headless)
# service installs instead:
agend-terminal service status  # then restart through your init system
```

如果在乾淨地重啟 daemon 之後仍有某一個 agent 一直 crash，那問題出在該 agent 的 backend／工作目錄，而不是 daemon：用 `agend-terminal attach <agent>` 看它的畫面，先修好根本原因再重生（`agend-terminal kill <agent>` + 重啟）。

---

## 2. 某個 agent 看起來卡住 / 顯示奇怪的狀態

**症狀**：badge 顯示 `awaiting_operator` / `hung` / `starting`，但 agent 看起來好好的——或者該 agent 真的卡死了。

**診斷**

```sh
# What changed, when, and what the screen looked like at that moment:
grep '"agent":"<name>"' ~/.agend-terminal/state-transitions.jsonl | tail -5
# Look at the live screen (read-only enough — detach by closing the viewer):
agend-terminal attach <name>
agend-terminal doctor
```

`state-transitions.jsonl` 的每一行是 `{"agent","from","to","ts","pty_snippet"}`——snippet 是狀態轉換當下 pane 底部的內容，通常能直接回答「它為什麼會這樣判斷」。

歷史紀錄：曾有一整類 **daemon 重啟後出現的假 `awaiting_operator`** 在 #2020/#2021 被修掉了（閒置的 opencode pane 在恢復 session，以及略過了乾淨 ready-prompt 的忙碌 agent）。如果你在比這些修正更舊的版本上看到這種樣態，請升級；在當前版本上，就把 `awaiting_operator` 當真，並去看被擷取下來的 pane。

**復原**

- agent 真的卡死了 → `agend-terminal kill <name>`；daemon 會把它重生（在 crash 預算範圍內，見 §1）。
- 一個 worktree binding 在它的 agent 之後還活著（例如你刪掉／重建了一個 instance，而 `bind_self` 現在拒絕綁定）：用 `force_release_worktree` 這個 MCP 工具（從任何已連線的 agent 或 lead 都可以呼叫）。
  **警告：它會刪掉磁碟上的 worktree 目錄——任何在 `~/.agend-terminal/worktrees/<agent>/...` 裡未提交的 WIP 都會不見。** 如果那些工作重要，請先 commit／push。它會拒絕 daemon worktree pool 以外的路徑，而且是冪等的。

---

## 3. Task board 凍住 / 載入不出來

**症狀**：`task` 查詢回傳錯誤，或是 board 就是一直不變。

這通常是**刻意設計的 fail-closed 閘門**（#1992）：task event log 裡有一筆這個 daemon 版本看不懂的紀錄（最常見的是：這份 log 是由一個比較**新**的 daemon 寫的——也就是你降級了）。daemon 會繼續跑，但寧可不推進 board 也不要亂猜。

**診斷**

```sh
LOG=$(ls -t ~/.agend-terminal/app.*.log | head -1)
grep "FAIL-CLOSED" "$LOG" | tail -3
grep "task_replay_fail_closed" ~/.agend-terminal/event-log.jsonl
```

那行 ERROR 會把這份合約講清楚：

```
task-board replay FAIL-CLOSED — the board will not advance until resolved
(upgrade the daemon to a version that understands this log, or quarantine
the offending record)
```

（這筆 event-log 紀錄**每次開機只觸發一次**，所以 per-tick 的重試不會洗版你；但可被 grep 到的那行 ERROR 會重複出現。）

**復原**

- 來自未來版本的紀錄 → **升級 daemon**。這是保護機制，不是 bug：見 `docs/COMPATIBILITY.md` 的 tier (b)。
- 真正壞掉的行（crash 造成的寫入撕裂）daemon 會幫你處理：每次啟動時，daemon 會把非 JSON 的行隔離到 `~/.agend-terminal/task_events.recovery/<timestamp>/`，並保留好的那些。檢查那個目錄就能看到被抽出來的內容。合法 JSON 但來自未來版本的紀錄則**刻意不會**被自動丟棄（它們屬於更新的 daemon——升級後就會恢復）。

---

## 4. 某個 store 檔案自己被重置了（「我的 X 跑去哪了？」）

**症狀**：某些被持久化的狀態（schedules、runtime config 等）在重啟後突然變成空的／預設值。

store loader 發現了壞掉的 JSON，**把壞檔案改名擱在一旁**，然後用預設值啟動（#2017）：備份就在原檔案旁邊，名稱是

```
<store-file>.corrupt.<YYYYMMDDHHMMSS>
```

**診斷**

```sh
ls ~/.agend-terminal/*.corrupt.* 2>/dev/null
LOG=$(ls -t ~/.agend-terminal/app.*.log | head -1)
grep "store load: corrupt JSON" "$LOG"
grep "store_corrupt" ~/.agend-terminal/event-log.jsonl   # once per boot per file
```

**復原**

```sh
agend-terminal stop
cp ~/.agend-terminal/<store>.corrupt.<ts> /tmp/inspect.json   # look at it; often a truncated tail
# fix the JSON (usually: delete the torn last record), then:
mv /tmp/inspect.json ~/.agend-terminal/<store>
agend-terminal app
```

如果你不需要舊內容，什麼都不用做——daemon 已經帶著乾淨的預設值在跑，下次寫入時就會覆蓋。備份會一直留著，直到你自己刪掉。

---

## 5. 通知沒送達（Telegram 安靜、agent badge 卡住）

**症狀**：某個 agent 顯示有待處理的通知，或 Telegram 訊息不再送達。

被延後的訊息存在 `~/.agend-terminal/notification-queue/`（每個 agent 一個 `.jsonl` 檔；行數＝待送訊息數）。它們是**刻意**在 agent 正在生成、或你正在打字的當下被扣住的，並由 TUI 迴圈和 daemon 端的 per-tick flusher 一起釋放（包含 headless 的 deployment），有防飢餓上限——actionable 訊息約 1 秒、ambient 訊息約 7 秒。從 #2029 起，遇到爭用的 queue 會重試，絕不會被誤報成空的。

**診斷**

```sh
wc -l ~/.agend-terminal/notification-queue/*.jsonl 2>/dev/null
LOG=$(ls -t ~/.agend-terminal/app.*.log | head -1)
grep "telegram notify failed" "$LOG" | tail -3   # network/token class
grep "requeue FAILED" "$LOG"                     # disk class — a queued message was LOST
```

**復原**

- `telegram notify failed` → 問題出在網路或 bot token。檢查 token 環境變數（`~/.agend-terminal/.env` 裡的 `AGEND_TELEGRAM_BOT_TOKEN`）、bot 的群組成員資格／管理權限，以及連線狀況。daemon 會持續重試；本地端沒有東西要清理。
- agent 明明閒置，queue 檔案卻無止盡地長大 → attach 到該 agent（見 §2）看看 flusher 為什麼認為它在忙，並檢查 log 裡的 `#1944-draftgate-decision` 那幾行（它們記下了每一次扣住／釋放的決定）。
- 持續出現的 `requeue FAILED` 是磁碟問題——先把那個修好；受影響的那一行會連同遺失文字的開頭一起被記下來。

---

## 6. 安全地升級／降級

先讀 `docs/COMPATIBILITY.md`——它定義了三個 on-disk 層級：(a) 手動編輯的公開檔案，例如 `fleet.yaml`（僅可增添，帶有 `schema_version`）；(b) daemon 內部持久化的狀態（有版本控管；比支援版本更新的紀錄會被警告或 fail-closed）；(c) 可重新生成的檔案（沒有任何承諾）。

- **升級**：stop、install、start。由任何同一 major 版本的較舊版本寫下的狀態都讀得進來。
- **降級**：要預期 tier-(b) 的摩擦——由較新版本寫下的 task event log 會**刻意把 board fail-close 掉**（見 §3）。那是保護機制在運作，不是 bug；回到較新版本就會把一切恢復。來自較新版本的 `fleet.yaml` 載入時會帶一則警告（未知欄位會被忽略——在信任其行為之前，先看那則 WARN）。
- 升級**以 service 安裝**的部署後，重新跑一次 `agend-terminal service install`，讓 unit/plist 帶上當前設定，然後重啟 service（細節見 `docs/RELEASING.md` 和 service 相關文件）。