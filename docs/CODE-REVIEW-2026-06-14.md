# agend-terminal 完整 Code Review

- **日期**：2026-06-14
- **範圍**：`main` @ `eaf65a9`，整棵 `src/`（約 227k 行 Rust / 375 檔）
- **方法**：多代理對抗式評審 — 18 個模組範圍 reviewer + 4 個跨切面全樹掃描（安全 / 併發 / panic / 持久化），每一條發現再由一個獨立驗證者開檔到 `file:line` 嘗試「反駁」，只有經得起反駁的才列入。
- **結果**：105 條發現 → **99 confirmed、1 uncertain、5 refuted**（駁回的誤報未列入），再加 panic/持久化補掃出的 1 critical + 5 條。

> ⚠️ 這是「附加」的評審交付文件，未修改任何程式碼。修復前請各自再驗證一次。

---

## 1. 執行摘要

| 等級 | 數量 |
|------|------|
| 🔴 Critical | 1 |
| 🟠 High | 15 |
| 🟡 Medium | 26 |
| 🟢 Low | 53 |
| ⚪ Info | 8 |

**依類別**（99 條 confirmed）：correctness 36、concurrency 15、resource-leak 13、error-handling 11、security 10、maintainability 9、design 2、performance 2、test-fidelity 1。

**一句話結論**：這是一個**成熟、工程紀律極高、但已逼近以「紀律」壓制複雜度上限**的 daemon supervisor。99 個 bug 不是 99 個獨立問題，而是 **約 7 個結構性模式的重複實例**（見 §3）。治本之道是把這些模式「變成不可能犯」，而非逐一熱修補。

---

## 2. 架構評估

agend-terminal 本質是一個**單主（single-primary）行程監督器**：用 `$AGEND_HOME/.daemon.lock` 的 flock 保證全機唯一 daemon，再用 PID 隔離的 `run/<pid>/` 目錄與 `.daemon`（`pid:boot_unix`）身份檔做 PID 回收偵測。在這之上長出 PTY 隔離的 agent 生命週期、35 工具的 MCP 表面、fleet 協調、CI 監看、Telegram/Discord channel、JSON loopback 控制 API、TUI、git-worktree 池化管理。技術選型稱職：`parking_lot` 記憶體鎖、per-file flock 跨行程協調、`atomic_write`（temp+fsync+rename）持久化、JSONL event log 審計軌跡。

工程紀律展現出**異常高的問題意識密度**——幾乎每個非顯而易見的決策旁都引用 issue 編號、解釋 race window、記錄「為什麼不那樣做」。這不是初稿，而是歷經 60+ sprint、被大量 incident 驅動修補的系統。`run_core` 的 self-respawn handoff（commit→exit 間的 successor liveness 重檢）就是被反覆對抗式打磨的範例。

**但這也是問題所在。** 發現分佈（36 correctness / 15 concurrency / 13 resource-leak）揭示的不是「不會寫 Rust」，而是**架構已無法用紀律繼續壓制複雜度**。bug 形態高度一致 → 它們是結構性、會持續再生產的。目前靠密集註解與個別熱修補維持正確性，這是會隨成長崩潰的維持方式。

### 真正的優點
- **單主租約身份模型嚴謹**：`.daemon.lock` flock + `run/<pid>/.daemon` 雙重身份檔，正確區分「互斥鎖」與「PID 回收防護」。
- **app/daemon 模式共用單一真相來源**：`register_event_subscribers` / `build_default_handlers` 兩模式共用同一份清單，並用 allowlist + invariant 測試防漂移（直接針對 #1002/#982/#1719 一整類歷史 bug）。
- **`atomic_write` 細節到位**：per-call 唯一 tmp 檔名消除共享 inode 競爭（#965），`TmpGuard` RAII 失敗自動 unlink 孤兒檔。
- **損壞檔「不靜默」**：`handle_corrupt_store` fallback 前先 rename 備份、ERROR log、發 event。
- **狀態偵測穩定性閘門成熟**：AuthError/AwaitingOperator/ServerRateLimit 都有 content-FP 去抖閘門 + tiered backoff，且抽成純函數可測。

### 結構性疑慮
- **`supervisor.rs` 是典型 god object（5,344 行）**：檔頭塞滿二十幾個跨 issue 的調校常數，聚合了 usage-limit 解析、auth-error 去抖、rate-limit 重試、pane-input 偵測等不相關決策權威於同一 tick 迴圈。
- **per-tick 模型是核心選擇也是核心成長風險**：`build_default_handlers` 現有 30+ handler 每 ~10s tick 跑一遍。正確性押在「每個 handler 自己 self-throttle 且不持鎖跨 IO」上，缺乏單 tick 的全域工作量/鎖佔用預算管控。
- **鎖紀律是反覆危險，且缺可強制的抽象**：三層鎖（全域 registry mutex、~50 個 `.core.lock()` 站點、14 處 per-file flock）。同一份狀態有些路徑走 flock、有些走 lock-free RMW；持鎖跨 blocking IO 出現在多個高層。鎖協定沒被型別系統或單一存取層封裝，全靠呼叫端記得用對。
- **跨行程快取/序號假設了單行程世界**：`SEQ_CACHE`、per-process `ID_SEQ` 在「CLI + daemon 多行程並存」設計下做跨行程唯一性來源 → 重複 seq/id。

---

## 3. 反覆出現的結構性模式（最重要的結論）

這 99 個 bug 是 **~7 個模式的重複實例**。修個案治標，修模式才治本：

1. **持鎖跨 IO（lock-held-across-IO）** — #38 registry mutex 跨磁碟 IO、core lock 跨 file IO、PTY writer 鎖跨無界 write、registry 鎖內取 core 鎖。**根因**：無「鎖內只做記憶體運算、IO 移出鎖外」的強制慣例。
2. **lock-free RMW vs flocked sibling（同狀態雙協定）** — decision_timeout、record_dispatch、watch-file、inbox pickup-id、release soft-mark。**根因**：sidecar/watch/marker 檔無單一存取層，每個呼叫端各自決定要不要 flock。
3. **跨行程快取/序號陳舊** — SEQ_CACHE、per-process ID_SEQ 撞 id、dedup 只查原名的 TOCTOU、bridge 挑首個 api.port run dir 不驗 liveness。**根因**：行程內狀態當跨行程唯一性來源。
4. **fire-and-forget without driven runtime** — Telegram notify 派到沒被 driven 的 current_thread runtime、detached CI poll task 每 tick spawn、裸 block_on 違反硬規。**根因**：sync→async 橋接沒有統一、保證被推進的 runtime bridge（121 個 block_on 站點只有部分走 safe helper）。
5. **stale-state-on-redeploy（同名重部署繼承陳舊狀態）** — pending_auth 繼承陳舊 pane_tail、next_after_ci handoff 殘留、topic-recreate 不 re-key dedup。**根因**：in-memory 追蹤 map 以「名字」為鍵但名字會被回收，且刪除/重部署時不主動清掃。
6. **無界成長的 map / append-only 檔** — idle_watchdog/waiting_on_stale 的 last_alerted map 從不 prune、board_router 每 miss 重複 append、unclassified_errors.jsonl 每 tick append。**根因**：去抖 map 從不在實體刪除時 prune；多個 JSONL 從不壓實。
7. **byte-offset / UTF-8 panic（把 PTY/外部位元組當 ASCII 切）** — parse_unlock_at、claim_verifier、scan_context_pct caps[1]、user 整數無 checked 算術。**根因**：解析不可信文字時假設 ASCII 邊界與有界整數。

**治本的五件事**（做完，多數發現會整類消失）：
1. 一個**強制 flock** 的 sidecar/watch/marker 存取層 → 消滅模式 2。
2. 一個**保證被 driven** 的 async runtime bridge，收斂全部 block_on → 消滅模式 4。
3. **集中強制**的 `validate_name`/`validate_branch`/`ensure_not_protected` 入口層 → 消滅一整批安全/校驗漏呼叫。
4. 一條「**鎖內不做 IO**」的 lint/review gate（`sync_audit.rs` 已有 core-lock 進出計數基礎，可擴成持鎖時長告警）→ 削弱模式 1。
5. in-memory 追蹤 map 一律掛 **agent-deletion prune hook** → 消滅模式 5、6。

---

## 4. Critical / High 詳述（含修法）

### 🔴 C1 — `claim_verifier.rs:116-119` 跨字串 byte-offset slice，非 ASCII claim 文字 panic
`let lower = s.to_lowercase(); let idx = lower.find(marker)? + marker.len(); let rest = s[idx..]` — `idx` 由小寫副本算出卻切原字串 `s`；`to_lowercase()` 不保留 byte 長度（如 `İ`→`i̇`、`ß`→`ss`），非 ASCII 時 `s[idx..]` 在非字元邊界 panic。經 `verify_push` 流程（`api/handlers/verify_push.rs:20`、`main.rs:1237`）以 agent/operator 供給的 claim 文字觸達。**修**：用同字串 offset 或 `.get(..)`（回傳 Option）取代 `[..]`。

### 🟠 H1 — `supervisor.rs:226-241` `parse_unlock_at` 同樣的跨字串/非邊界 byte slice panic
`idx` 來自 `line.to_lowercase()` 卻切原 `line`；另 `rest[..5]` 固定 byte index 可切穿多位元組字元。`pane_tail` 是完全內容可控的 PTY 輸出（`tail_lines(10)`）。per-tick `catch_unwind` 雖吞掉 panic，但**整個 tick（所有 agent 的 reaction/recovery/pane-input 掃描）會在該內容顯示的每個週期被中止**。**修**：一致地對 `lower` 搜尋並切片，用 char-aware 擷取（`chars()` 或 `\d{2}:\d{2}` regex）取代固定 byte index。

### 🟠 H2 — `decision_timeout.rs:119-123` 用無鎖 `remove_file` 取消 pending decision → resurrection / lost-fire race
`record_pending_decision`（API 執行緒）用裸 `remove_file` 刪除前一個 sidecar，未取 `{decision_id}.lock`（而 `scan_and_emit`/`mark_resolved_for_sender` 都取）。若 scan 讀檔→record 刪檔→scan 的 `atomic_write` 落地，被刪的 sidecar 會以 `status='timeout'` **被復活並永久殘留**，且為操作者正在取代的 decision 誤發 timeout 事件。**修**：刪除前取同一把 flock（仿 `dispatch_idle::delete_sidecar_locked`）。

### 🟠 H3 — `decision_timeout.rs:282` 持久化失敗被吞 → 每 tick 重複 timeout 通知
flip 為 `"timeout"` 後 `let _ = write_decision(...)` 丟棄結果，但通知**照發**；磁碟仍是 `"pending"`，下個 tick 再讀再 timeout 再發 → 無界重複通知 + 狀態永久不一致。同檔 209 行的兄弟路徑有正確檢查 `if write_decision(...)`。**修**：`if write_decision(...) { Some(current) } else { None }` 並記錄失敗。

### 🟠 H4 — `mcp/handlers/ci/mod.rs:819-854` `ci unwatch` 忽略已驗證的呼叫者身份，經 daemon-env fallback 退訂**所有** agent
`handle_unwatch_ci` 用 `ha` adapter（丟棄 `ctx.instance_name`），改 fallback 讀 `std::env::var("AGEND_INSTANCE_NAME")`——但這跑在 daemon 行程，其環境沒有呼叫 agent 的名字 → `caller` 為空 → 空呼叫者分支 `subscribers.clear()` 退訂**該分支所有其他 agent** 的 CI，與模組文件「只移除 caller」矛盾。`handle_watch_ci` 用 `hai`（正確）；unwatch 是不對稱的漏網。**修**：改 `hai` 形狀傳入 `instance_name`，刪掉 env fallback。

### 🟠 H5 — `mcp/handlers/ci/mod.rs:633-750` MCP watch handler 對 watch 檔做無 flock RMW
兩個 MCP handler 讀 watch JSON→記憶體改→`atomic_write` 寫回，**無 file lock**；而所有其他寫者（poll loop、cleanup、reassign）都持 `<hash>.lock`（#692）。MCP `ci watch/unwatch` 可與 poll loop 交錯 → 還原 `last_notified_head_sha`/`last_run_id` → 重複 `[ci-pass]` 通知、poll cursor 遺失、subscriber 被覆蓋。`atomic_write` 只保證單寫原子，**不防 read→write 間的 lost update**。**修**：從初讀到 `atomic_write` 全程持 `watch_path.with_extension("lock")`。

### 🟠 H6 — `mcp/handlers/instance_state/spawn.rs:96-107` `create_instance(branch="main")` 繞過 E4.5 保護分支閘門
spawn 路徑只呼 `validate_branch`（允許 main/master），未呼 `ensure_not_protected`；`worktree::create` 也不檢查 → `git worktree add -b main` / fallback `add <dir> main` 成功在 agent worktree checkout 受保護分支，違反「worktree 永不取 main」的全系統 invariant（`bind_self` 工具明載「Rejects main/master (E4.5)」）。**修**：`validate_branch` 後立即加 `ensure_not_protected(branch)`。

### 🟠 H7 — `mcp/handlers/instance_state/mod.rs:43-69` 未驗證的 team name → 成員名 + workspace 目錄 → path traversal 逃出 `workspace/`
team 模式直接把 `args["team"]` 轉給 `CREATE_TEAM`，未 `validate_name`（單實例路徑有驗）。下游 `create_team` 以 `format!("{team}-{n}")` 建成員名並 `workspace_dir(home).join(&inst_name)`，`PathBuf::join` 保留 `..` → `team="../../tmp/evil"` 在 workspace 外建立並註冊 fleet 條目。**修**：兩個 team 分支頂端都加 `validate_name_or_err!(team_name)`。

### 🟠 H8 — `channel/telegram/notify.rs:64` Telegram 通知派到**沒被 driven** 的 current_thread runtime → 通知可能永遠不送出
`telegram_runtime()` 是 `new_current_thread`，spawn 的 task 只有當某執行緒正在該 runtime 的 `block_on()` 內才會推進；但沒有持久 driver 執行緒（polling thread 自建另一個 runtime）。public wrapper 又 `let _ =` 丟棄 JoinHandle。於是 daemon stall/recovery/crash/CI 通知只在後續恰好有 sync-context `block_on_value` 時機會性送出，否則**永遠卡在佇列且 dedup claim 仍記著**（抑制 TTL 內重發）。MED-1 測試只因明確 `block_on_value(h1)` 推進才通過——正是 production 沒有 driver 的證據。**修**：在 `notify_telegram_inner` 內用 `block_on_value` 同步送出（仿 `reply.rs::send_reply`），或起一條長駐執行緒持有並持續 drive 該 runtime。

### 🟠 H9 — `api/handlers/query.rs:11-51` `handle_list` 持全域 registry mutex 跨 per-agent blocking 磁碟 IO
在 `.map()` over `reg.values()` 內、仍持 tier-1 registry 鎖時，對每個 agent 呼 `pending_for_instance` → 整個 pending 目錄 `read_dir` + 每個 `.json` sidecar `read_to_string`+parse。N agent × M sidecar 的磁碟 IO 全在鎖內，磁碟一慢就卡住所有 API handler、supervisor tick、crash-respawn、TUI render。**修**：持鎖時只 snapshot 每 agent 欄位到 owned Vec，`drop(reg)` 後再做磁碟讀；或先 `list_pending` 一次傳入純函數過濾。

### 🟠 H10 — `task_events.rs:1341-1366` `SEQ_CACHE` 跨行程陳舊 → 重複 seq 靜默丟事件
`max_seq_for_instance` 在 tail-scan 前短路於 process-local `SEQ_CACHE`，但 flock 是跨行程、cache 不是。`tasks::handle()`（MCP 行程）與 `auto_close`/`sweep`/`lifecycle`（daemon 行程）寫同一 `task_events.jsonl`。另一行程的 cache 是陳舊高水位 → 新 envelope 被指派 seq ≤ 已持久化值 → replay 時 `apply` 跳過 `seq <= last_seen` → **真實 task transition 靜默遺失**，違反 doc「concurrent appenders observe a totally-ordered seq stream」。**修**：在鎖內勿信記憶體 cache，永遠 tail-scan，或用 file len/mtime 驗證 cache 新鮮度（仿 REPLAY_CACHE）。

### 🟠 H11 — `tasks/lifecycle.rs:114-135` archive 把已完成（Done）task 翻成 Cancelled，污染終態
`archive_done_tasks`（boot 時跑）對逾期 Done task 發 `TaskEvent::Cancelled`；`apply_cancelled` 無條件設 `status=Cancelled` 且走裸 `append()` 繞過 `can_transition_to`（其本身禁 Done→Cancelled）。→ 成功完成的 task 過 7 天被改寫為 Cancelled，任何區分 done/cancelled 的 reader/metric/audit 都把已完成工作誤計為取消。**修**：別用 Cancelled 表達歸檔，引入明確 Archived 事件/狀態，或以 compaction 移出 active replay 而保留 Done 歷史。

### 🟠 H12 — `inbox/storage.rs:1006-1010` #911 JSONL dedup fallback 對 id-native 實例是永久 no-op
`msg_already_drained_in_jsonl` 用 `inbox_path`（原名路徑）解析，但 `drain`（寫 `read_at`）用 `inbox_path_resolved`（UUID 路徑）。id-native 實例無 `<name>.jsonl` → `read_to_string` 失敗 → 一律回 `false`。此為 in-memory ledger（`OnceLock`，重啟即失）MISS 後的唯一 dedup 來源。→ **重啟後已 drain 的訊息可被重新注入/重複投遞**。**修**：改用 `inbox_path_resolved`（與 `drain` 一致）。

### 🟠 H13 — `agent/dismiss.rs:145-166` dismiss 執行緒持 raw PTY writer 鎖跨無界 blocking write → 可永久卡死對某 agent 的所有 inject
auto-dismiss 直接 `pty_writer.lock()` 後無 timeout `write_all`/`flush`（其他路徑都走 `write_with_timeout` 的 spawned-thread+timeout）。若 backend 停止排空 PTY 輸入緩衝（卡住的 agent 的常態，正是 dismiss 觸發時），`write_all` 無限阻塞且持鎖；其他 `write_with_timeout` 呼叫者 5s 超時但其 worker 永卡 `writer.lock()`，stuck thread 無界累積，**該 agent 再也收不到任何訊息直到 daemon 重啟**。**修**：dismiss 走 `write_with_timeout` 或對每次 `write_all` 加 deadline。

### 🟠 H14 — `deployments.rs:408-457` deploy 在取鎖前就 spawn（TOCTOU），同名併發 deploy 都會 spawn+寫 fleet.yaml
spawn 與 fleet.yaml 寫入在 store flock（451 行）之前，無 existing-name 檢查。**修**：以 deploy name 為鍵上鎖或在 flock 內拒絕重名。

### 🟠 H15 — `deployments.rs:397-405` `create_deployment_team` 把 ok=false 當成功
api 呼叫的 Ok 臂是 no-op，只有 Err 臂 fallback 到 teams create；daemon 拒絕（ok=false）時不建 team，但 deploy 卻把 team 記成 `Some`。**修**：ok=false 視為失敗，不記錄 team。

---

## 5. 重點 Medium（安全相關全列；其餘見 §7 表）

- **`tasks/handler.rs:444-451` 〔security〕caller 可控 `done_source` → agent 偽造 PR-merge provenance**：MCP 直接反序列化 `args["done_source"]`，可供 `DoneSource::PrMerged{forged snapshot}`，污染「未來 audit 能重建 daemon 實際所見」的鑑識軌跡。**修**：caller 只准 operator-attestable 變體（OperatorManual）或蓋 `self_reported` 標記；PrMerged/LegacyBackfill 僅由實際觀測 GitHub 的 daemon 路徑構造。
- **`mcp/handlers/dispatch_hook/mod.rs:723-758,883` 〔security〕agent 供給的 branch 未過 `validate_branch` 就進 `git branch <branch>`**：`ensure_branch_exists` 驗了 `from_ref` 卻沒驗 `branch`，`--edit-description`/`-D` 之類選項在任何 guard 前就交給 git（argument-injection，非 shell）。**修**：`ensure_branch_exists` 頂端 `validate_branch(branch)` + `ensure_not_protected`。
- **`quickstart.rs:533-541…` 〔security〕Telegram bot token 在任何網路錯誤時洩漏到 console**：token 內嵌 URL path，reqwest `Error` Display 保留 path，四處 `println!("{e}")` 原樣印出。daemon 側 GitHub 路徑用 Authorization header（正確）；quickstart 是outlier。**修**：token 走 header，或印前用 `mask_token`/`without_url()` 清洗。
- 其餘高價值 Medium：`task_sweep.rs:637` 把 `UNVERIFIED` 當 pass、`instance_state/mod.rs:54` 無上限 count 觸發巨量 Vec 配置（OOM DoS）、`app/overlay.rs:361` 關 shell pane 洩漏 PTY 子行程、`inbox/storage.rs:317` rewrite 靜默刪除前向 schema 版本訊息（downgrade 資料遺失）、`worktree_cleanup.rs:85` `is_remote_gone` 刪掉有未推送 commit 的分支、`mcp_config.rs:129` 備份失敗仍覆寫（「backing up」是謊言+資料遺失）、`token_cost.rs:726` user 可控 i64 無 checked 乘法。

---

## 6. 建議優先修復順序

1. **C1 + H1**（兩處 byte-offset panic，其一 agent-reachable、其一每週期中止整個 supervision tick）。
2. **H8**（Telegram 通知黑洞）+ **H13**（dismiss 永久卡死 agent）+ **H9**（registry 鎖跨 IO）— 三個會讓「控制平面靜默失效」的併發洞。
3. **lock-free RMW 群**（H2 + H5 + Medium 的 record_dispatch）— 一起收斂到強制 flock 存取層（模式 2）。
4. **H10 SEQ_CACHE** + **H11/H12 task/inbox 資料遺失** — event log/inbox 作為真相來源的可信度。
5. **安全校驗群**（H6 + H7 + dispatch_hook branch + done_source 偽造）— 收斂到集中校驗層（§3 治本 #3）。
6. **H14 + H15**（deploy race + ok=false 當成功）。
7. Low/Info 依模式批次處理（prune hook、jsonl 壓實、checked 算術、canonicalize 後刪除）。

---

## 7. 完整發現清單

下表為全部 confirmed 發現（依等級）。C1 與 panic 補掃的另 5 條（`decision_timeout.rs:282`、`operator_mode.rs:207` 非原子寫 authority gate、`telegram/bootstrap.rs:217` topic registry 持久化吞錯→重啟重複建 topic、`quickstart.rs:562` mask_token byte-slice、`app/session.rs:137` session.json 非原子寫+吞錯）已併入上文與下表脈絡。

### HIGH

| 類別 | 標題 | 位置 |
|------|------|------|
| error-handling | parse_unlock_at panics on non-ASCII pane content (cross-string byte-offset slice + non-boundary slice) | `src/daemon/supervisor.rs:226-241` |
| concurrency | decision_timeout cancels existing pending decisions with an UNLOCKED remove_file — resurrection / lost-fire race vs scan_and_emit | `src/daemon/decision_timeout.rs:119-123` |
| correctness | ci unwatch ignores validated caller identity and unsubscribes ALL agents via daemon-env fallback | `src/mcp/handlers/ci/mod.rs:819-854` |
| concurrency | handle_watch_ci / handle_unwatch_ci do watch-file RMW without the per-watch flock used everywhere else | `src/mcp/handlers/ci/mod.rs:633-750` |
| security | create_instance with branch="main" bypasses the E4.5 protected-branch guard and checks out main in a worktree | `src/mcp/handlers/instance_state/spawn.rs:96-107` |
| security | Unvalidated team name in create_instance becomes member names + workspace dirs, enabling path traversal outside workspace/ | `src/mcp/handlers/instance_state/mod.rs:43-69` |
| concurrency | Telegram notify spawned onto an undriven current_thread runtime — notifications can silently never be sent | `src/channel/telegram/notify.rs:64` |
| concurrency | handle_list holds the global registry mutex across per-agent blocking disk I/O | `src/api/handlers/query.rs:11-51` |
| correctness | SEQ_CACHE makes cross-process seq computation stale → duplicate seqs silently drop real events | `src/task_events.rs:1341-1366 (also 1068-1069, 1010-1013)` |
| correctness | lifecycle archive flips completed (Done) tasks to Cancelled, corrupting terminal status | `src/tasks/lifecycle.rs:114-135 (Cancelled emit); 75-138` |
| correctness | #911 JSONL dedup fallback is a permanent no-op for id-native instances (reads unresolved name path) | `src/inbox/storage.rs:1006-1010` |
| concurrency | Dismiss thread holds the raw PTY writer lock across an unbounded blocking write, can permanently wedge all injects to an agent | `src/agent/dismiss.rs:145-166` |
| concurrency | deploy spawns before the lock | `src/deployments.rs:408-457` |
| error-handling | create_deployment_team treats ok-false as success | `src/deployments.rs:397-405` |

### MEDIUM

| 類別 | 標題 | 位置 |
|------|------|------|
| resource-leak | pending_auth map leaks (and inherits stale pane_tail on same-name redeploy) — missing live-agent sweep | `src/daemon/supervisor.rs:1422-1427` |
| concurrency | dispatch_idle record_dispatch in-place dedup refresh writes the sidecar UNLOCKED — lost update vs scan_and_emit's flocked Exceeded flip | `src/daemon/dispatch_idle/mod.rs:271-280` |
| correctness | Compliance review-verdict check matches UNVERIFIED / NOT VERIFIED as a pass | `src/daemon/task_sweep.rs:637-640` |
| security | Agent-supplied branch name reaches `git branch <branch>` without validate_branch — argument-injection / defense-in-depth gap | `src/mcp/handlers/dispatch_hook/mod.rs:723-758, 883` |
| correctness | delegate_task auto-bind can exceed the `send` 30s MCP timeout; agent sees `accepted_in_progress` success while the bind later fails | `src/mcp/handlers/comms.rs:213-234` |
| resource-leak | Unbounded count in create_instance team mode triggers an enormous Vec allocation (OOM/abort DoS) | `src/mcp/handlers/instance_state/mod.rs:54-60` |
| resource-leak | Closing a non-fleet (shell) pane/tab leaks its PTY child process | `src/app/overlay.rs:361-409` |
| resource-leak | Unbounded growth of unclassified_errors.jsonl: full-screen records appended every tick on a throttle-present-but-unclassified pane | `src/state/mod.rs:2100-2147 (append at 2137-2138; bypass at 1460-1478)` |
| resource-leak | lifecycle archive write is non-atomic, unfsynced, and double-archives on crash; both errors swallowed | `src/tasks/lifecycle.rs:79-111, 134` |
| security | caller-controlled done_source lets agents forge PR-merge provenance on Done events | `src/tasks/handler.rs:444-451 (also 656-665)` |
| error-handling | find_message aborts the entire cross-inbox scan on a single unreadable file | `src/inbox/storage.rs:968` |
| correctness | Migrated inboxes are scanned twice via symlink, duplicating thread/find results and corrupting the symlink on rewrite | `src/inbox/storage.rs:929-936` |
| correctness | enqueue_returning_unread_count inflates the pending-hint count by including superseded rows (drifted from unread_count) | `src/inbox/storage.rs:163-166` |
| correctness | drain/clear/sweep silently DELETE forward-schema-version messages on rewrite (downgrade data loss) | `src/inbox/storage.rs:317-324` |
| correctness | resolve_instance returns a fresh random UUID for a fleet instance lacking a parseable id (InstanceId::default() is non-deterministic) | `src/agent/mod.rs:333-340` |
| correctness | Local-branch deletion keyed on default_branch() that silently falls back to "main" | `src/worktree_pool.rs:187-238` |
| correctness | is_remote_gone deletes branches with unpushed local commits when remote ref is absent | `src/worktree_cleanup.rs:85-152, 234-247, 318-327` |
| performance | Squash-merge auto-GC shells out to `gh` over the network inside the per-tick branch prune | `src/branch_sweep.rs:183-260` |
| concurrency | teams create one-agent-one-team is a TOCTOU | `src/teams.rs:174-217` |
| error-handling | parse_since multiplies a user-controlled i64 without checked/saturating arithmetic — silent overflow (release) / panic (debug) | `src/token_cost.rs:726-733` |
| correctness | find_cargo_test_payload only inspects the FIRST 'cargo test' substring — a preceding 'cargo testbed'/'cargo testing' suppresses a real later invocation, letting a hallucinated test name bypass #812 validation | `src/claim_verifier.rs:251-262` |
| correctness | Any claim text mentioning an 'fn name(' pattern is reclassified as FunctionExists and hard-rejected if the fn is absent — classification false-positive that blocks legitimate pushes | `src/claim_verifier.rs:86-91` |
| error-handling | Corrupt MCP config destroyed when backup copy fails — "backing up" warn is a lie (data loss) | `src/mcp_config.rs:129-154` |
| security | Telegram bot token leaks to console on any network error during quickstart | `src/quickstart.rs:533-541, 591-592, 637-638 (leak surfaced at 93, 380, 424, 430)` |

### LOW

| 類別 | 標題 | 位置 |
|------|------|------|
| concurrency | Blocking file IO performed while holding the per-agent core lock in the supervisor tick | `src/daemon/supervisor.rs:1247-1268` |
| correctness | Router retain-block appends PTY bytes to the mirror buffer without the cap and without an active/reply check | `src/daemon/router.rs:142-150` |
| resource-leak | idle_watchdog last_alerted in-memory map is never pruned on agent deletion — slow unbounded growth | `src/daemon/idle_watchdog.rs:440-470` |
| correctness | Branch (and repo) names interpolated unencoded into CI provider API query strings | `src/daemon/ci_watch/provider.rs:346-351` |
| error-handling | pr-ready-for-merge dedup flag set before deferred enqueue can fail — signal lost on enqueue failure | `src/daemon/pr_state/scanner.rs:178-187` |
| correctness | Stale-snapshot freshness gate compares poll time to created_at but not to the branch's last head advance | `src/daemon/pr_state/scanner.rs:364-372` |
| resource-leak | waiting_on_stale tracker's last_alerted_at map grows unbounded (never pruned) | `src/daemon/waiting_on_stale.rs:29, 122` |
| performance | task_sweep fetches the merged-PR list from GitHub twice per tick | `src/daemon/task_sweep.rs:213, 379, 725` |
| resource-leak | hook_shadow global store accumulates per-agent entries with no eviction | `src/daemon/hook_shadow.rs:47-50, 239-247` |
| correctness | boot_sweep identity guard is bypassed when the .daemon PID is unreadable | `src/daemon/boot_sweep.rs:114-124` |
| maintainability | Doc comment claims the `branch` arg is run through validate_branch, but only `from_ref` is | `src/mcp/handlers/dispatch_hook/mod.rs:681-683` |
| resource-leak | Plain self-dispatch with a branch leases+binds a worktree before the API rejects the self-send (orphan worktree) | `src/mcp/handlers/comms.rs:161-167, 213-234, 276-296` |
| error-handling | parse_duration_secs can integer-overflow on attacker-supplied duration (panic in debug) | `src/mcp/handlers/dispatch.rs:532-541` |
| concurrency | Inbox metadata pickup-id processing mutates state via two non-atomic JSON read/write cycles (lost pickup ids) | `src/mcp/handlers/comms.rs:645-661` |
| resource-leak | handle_release_repo leaks .git/worktrees metadata when worktree .git is not a readable file | `src/mcp/handlers/ci/mod.rs:458-499` |
| error-handling | compute_next_poll_eta can integer-overflow on attacker/buggy interval_secs | `src/mcp/handlers/ci/mod.rs:804-811` |
| correctness | Stale next_after_ci handoff target persists across re-watch unless explicitly overwritten | `src/mcp/handlers/ci/mod.rs:695-708` |
| test-fidelity | reply.rs test writes/reads channel/topics.json but production registry uses topics.json — the 'registry untouched' assertion checks an irrelevant file | `src/channel/telegram/reply.rs:312` |
| correctness | Discord keepalive thread sleeps the full interval before the first PATCH, leaving fresh threads un-refreshed for 30 minutes | `src/channel/discord.rs:369` |
| correctness | notify_telegram_inner topic-recreate retry re-sends to a new topic but does not re-key the dedup claim, so a concurrent dup to the new topic is not suppressed | `src/channel/telegram/notify.rs:120` |
| resource-leak | Dedup cache has no entry-count ceiling; zero-byte (Errored/Oversized/in-flight) entries grow unbounded between 10-min sweeps | `src/api/request_dedup.rs:325-352` |
| correctness | Dedup cache keys only on request_id, allowing a replayed id to return a stale response for a different operation | `src/api/mod.rs:529-548` |
| concurrency | handle_register_external is the only site holding registry+external locks simultaneously, creating an undocumented nested lock order | `src/api/handlers/external.rs:15-49` |
| maintainability | agent_is_alive doc claims poison-safety that parking_lot cannot provide; real risk is deadlock | `src/app/mod.rs:1489-1505` |
| correctness | Terminal Resize event path skips the wide-char ghost-clear the needs_resize path performs | `src/app/mod.rs:707-710` |
| maintainability | #2086-srl-keep-latched WARN re-fires every feed for the full duration a real SRL is stuck (no dedup), reproducing the #1450 14k-lines/incident flood | `src/state/mod.rs:1836-1848` |
| design | Comment claims hash-dedup bounds unclassified-throttle logging 'once per screen' but the throttle-hint bypass and spinner-driven hash churn both defeat it | `src/state/mod.rs:2115` |
| error-handling | scan_context_pct indexes caps[1] on a backend-supplied context_pattern; a pattern without a capture group panics the read-loop | `src/state/mod.rs:1246-1248` |
| maintainability | #1808-probe0-phantom consecutive-rematch WARN re-fires every tick on an in-place static SRL with no dedup | `src/state/mod.rs:1927-1943` |
| security | cross-board task creation has no ACL on the target project; `..` project id escapes the boards subtree | `src/tasks/handler.rs:136-143` |
| correctness | handle_update status check is out-of-lock for non-status fields; emitter identity drift in done arm | `src/tasks/handler.rs:617-632, 655-676` |
| concurrency | sweep apply emits bare Cancelled without an in-lock legality guard (can clobber a concurrently-Done task) | `src/tasks/sweep.rs:408-426` |
| correctness | Cross-process task ID collision: per-process ID_SEQ + microsecond timestamp can mint duplicate ids | `src/tasks/handler.rs:73-77` |
| maintainability | auto_close emitter identity 'system:auto-close' (hyphen) is not in the ACL allow-list (underscore) | `src/tasks/auto_close.rs:63` |
| resource-leak | board_router index repair re-appends duplicate entries on every miss → unbounded task_index.jsonl growth | `src/tasks/board_router.rs:106-122` |
| maintainability | enqueue doc/comment contradicts the actual non-atomic append implementation | `src/inbox/storage.rs:109-140` |
| correctness | wait_for_process_exit None (process never reaped) is classified as Crash, but sweep_child_tree already killed the tree — risks classifying a daemon-kill as a respawnable crash | `src/agent/mod.rs:1595-1614` |
| security | cleanup_working_dir removes the entire directory based on a purely lexical starts_with(workspace) check (no canonicalization) | `src/agent_ops.rs:345-354` |
| error-handling | install_hooks ignores the git config result and all script-write failures, so a worktree can be left with hooks silently not installed | `src/binding.rs:332-360` |
| correctness | release() soft-mark does a non-atomic read-then-rewrite of the managed marker | `src/worktree_pool.rs:77-95` |
| correctness | GC clean-release deletion runs `git worktree remove` with no current_dir when source_repo is unresolved | `src/worktree_pool.rs:1161-1218` |
| correctness | GC agent-name fallback derives the wrong agent for slash-branch worktrees | `src/worktree_pool.rs:913-930` |
| correctness | is_in_use canonicalize fail-open can let an in-use worktree be swept | `src/worktree_cleanup.rs:161-171` |
| maintainability | Module-level and Codex-section comments claim 'take the MAX per file, never summed' but parse_codex_rows sums per-turn deltas — stale comment contradicts implementation | `src/token_cost.rs:16-18` |
| correctness | git_show_and_fmt spawns rustfmt and ignores stdin write failure, then reads stdout — a partial-write/broken-pipe yields a truncated format that can silently flip 'only formatting' verdicts | `src/verify.rs:671-678` |
| error-handling | Hook-state-poc upsert silently discards a corrupt settings file with no backup | `src/mcp_config.rs:240-241` |
| concurrency | Zombie kill primitive has TOCTOU between liveness check and signal — can kill a recycled PID | `src/admin/cleanup_zombies.rs:134-176` |
| correctness | macOS service plist written non-atomically before launchctl load | `src/service/macos.rs:54-66` |
| correctness | Bridge picks the first run dir with api.port without any liveness/identity check | `src/bin/agend-mcp-bridge.rs:507-519` |
| concurrency | Raw block_on on the shared CI runtime violates the CLAUDE.md hard rule and bypasses the nested-runtime-safe block_on_value helper | `src/daemon/ci_watch/provider.rs:277-284` |
| correctness | atomic_write fsyncs the temp file but never fsyncs the parent directory after rename | `src/store.rs:166-193` |
| resource-leak | Detached per-repo CI poll tasks spawned every tick onto the 2-thread shared runtime with no stored handles or overlap guard | `src/daemon/ci_watch/poller.rs:522-523` |
| security | worktree_path joins the agent/instance name into a filesystem path without validating it, relying entirely on callers to pre-validate | `src/worktree.rs:22-24` |

### INFO

| 類別 | 標題 | 位置 |
|------|------|------|
| maintainability | anti_stall module docs reference a non-existent 'dispatched_at' field; code anchors on Task.started_at | `src/daemon/anti_stall.rs:9-20` |
| design | Dispatch auto-watch_ci derives repo from `git remote get-url origin` but watch arm errors only logged, dispatch reported OK | `src/mcp/handlers/dispatch_hook/mod.rs:578-601` |
| correctness | handle_clear_blocked_reason forwards instance arg to the daemon without validate_name | `src/mcp/handlers/instance_metadata.rs:200-208` |
| concurrency | create_instance dedup checks only the original name, not the generated deduped name (TOCTOU + suffix collision) | `src/mcp/handlers/instance_state/spawn.rs:32-49` |
| correctness | Pre-auth read timeout is set on a different fd than the one the handshake reads from | `src/api/mod.rs:459-469` |
| maintainability | Dead code: unsubscribe_all_ci_watches_for_agent retained but unreachable | `src/worktree_pool.rs:512-566` |
| security | Same-user process holding api.cookie gains full operator authority (documented threat model, but worth a defense-in-depth note) | `src/api/operator_gate.rs:136-139` |
| security | validate_branch accepts '.lock' / '.' and other refnames git itself rejects, but does NOT block a leading-dot path component | `src/agent_ops.rs:290-297` |


---

## 8. 方法與限度

- 每條發現都由獨立驗證者開檔到 `file:line` 嘗試反駁；5 條被駁回的誤報未列入。1 條 uncertain（無法從程式碼證實）未列入主表。
- `xcut-panic-io` 全樹掃描首跑因伺服器端速率限制失敗，已單獨補跑（產出 C1 等 6 條）。
- 未執行 `cargo build`/`clippy`（CI 已對 `main` 強制 `clippy -D warnings` 為綠）；本評審聚焦邏輯/結構/併發/安全，非編譯層告警。
- 行號為評審當時（`eaf65a9`）狀態，修復前請再核對。
