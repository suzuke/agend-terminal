# 更新日誌

本文件記錄本專案所有重要變更。
格式基於 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)；專案遵循 [SemVer](https://semver.org/spec/v2.0.0.html)。

## [Unreleased]

### Changed

- **重啟監督偵測:改用正向 `AGEND_SUPERVISED` sentinel,移除 `XPC_SERVICE_NAME`(#1812)** — `is_restart_supervised()`(`restart_daemon` 的 #851 fail-closed 防呆)不再信任 `XPC_SERVICE_NAME`。macOS 會把該變數注入 GUI 登入工作階段中的*每一個* process(包含在 Terminal.app 裡裸跑 `agend-terminal start`),導致 macOS 上此防呆永遠回 true,`restart_daemon` 可能 `exit(42)` 後沒有任何程序重啟 daemon。現在改以 `agend-terminal service install` 寫入 launchd plist(`EnvironmentVariables`)與 systemd unit(`Environment=`)的 `AGEND_SUPERVISED=1` 明確 sentinel 判斷;`AGEND_WRAPPED` 與 systemd 的 `INVOCATION_ID` 仍接受。**macOS/Linux 升級遷移:升級後請重跑 `agend-terminal service install`,再重啟 daemon 一次** —— 舊版安裝的 service 設定檔早於此 sentinel,在重新產生前 `restart_daemon` 會 fail-closed 並回傳可操作的錯誤訊息(這是安全方向 —— 拒絕而非把 daemon 卡死)。Windows Task Scheduler 無法攜帶此 sentinel(task XML 沒有環境變數元素),維持裸啟動 fail-closed。

## [0.7.0] — 2026-05-28

自 `0.6.1` 以來超過 200 個 commit，橫跨 Sprint 55–69（2026 年 5 月 7 日至 5 月 28 日）。三大主題：
**(1) Task board 可靠性** — 根因分析並預防 ghost-owner 問題（`teams::delete` 級聯刪除、啟動時孤兒掃描、`force` 旗標）+ 新的清理工具 + 營運者可見的健康快照；**(2) Bridge ↔ daemon 冪等重試** — 消除暫時性傳輸失敗下有副作用的 MCP 呼叫重複執行問題（UUID request_id + DedupCache + Condvar 阻塞等待）；**(3) MCP handler 重構 #694 完成** — 30+ 個工具分支從行內 `match` 遷移至 dispatch table，為工具註冊表熱重載 (#776) 鋪路。另有 Hung 偵測影子模式基礎設施（F9 Stage 1–3）、速率限制恢復自動提示，以及約 50 個較小的 bug 修復/加固 PR。

### 新增

- **`notify_system` helper (#1335)** — `crate::inbox::notify_system()` 封裝常見的 daemon 通知模式。七個 daemon 模組完成遷移，每個通知點從約 8 行減少到 1 行。
- **Event bus (#1336)** — 全域事件匯流排，透過 `AGEND_EVENT_BUS=1` 啟用。`event_bus::emit_lazy(kind, || payload)` 在停用時延遲序列化；停用路徑零成本（無配置、無序列化）。
- **`with_pr_state` flock helper (#1342)** — `pr_state::with_pr_state()` 和 `with_pr_state_or_create()` 透過 `fs4` 檔案鎖序列化所有 `pr-state/*.json` 的讀取-修改-寫入操作。消除 gh-poll 覆寫 scanner 的 `ready_emitted_for_sha` 旗標導致重複 `[pr-merged]` 通知的 lost-update race。全部 6 個生產環境 `save()` 呼叫點已遷移。
- **pr-merged 時自動釋放 worktree (#1344)** — Scanner 的 `MergeState::Merged` 分支現在在發出 `[pr-merged]` 前呼叫 `auto_release_for_merged_branch()`。防止 dev 的 worktree 仍持有本地分支時 `gh pr merge --delete-branch` 失敗。
- **`/setup-telegram` skill + `skill add` URL#subdir 支援 (#1351, PR #1354)** — 導引式 Telegram 設定的 per-channel 安裝 skill。`skill add <url>#<subdir>` 可克隆 skill repo 並安裝子目錄。
- **CI 程式碼覆蓋率（cargo-llvm-cov + Codecov, #686）** — CI 新增 `coverage` job，在 Ubuntu 上執行 `cargo llvm-cov` 並上傳至 Codecov。維護者專用基礎設施。
- **Bridge ↔ daemon 冪等重試 (#842, PR #843)** — bridge 為每個 JSON 封包產生 UUID v4 `request_id`；傳輸失敗重試時重用同一 id。新的 `DedupCache`（TTL 10 分鐘、64KB/條目、64MB 上限、Condvar 阻塞等待）快取已完成的回應並阻擋進行中的重複請求。
- **`task action=health` (#830, PR #838)** — 營運者自助式看板健康快照。回傳總數 + 按狀態統計 + ghost_owners + 過期 claims + 年齡分布 + 建議陣列。
- **`task action=sweep` (#806, PR #810)** — 營運者觸發的過期任務清理。4 個類別搭配預設 dry-run + `confirm_ids` + `audit_reason` 審計。
- **`repo action=cleanup_merged_branches` (#817, PR #820)** — 營運者觸發的本地分支清理。4 個類別，`min_age_days` 可設定（預設 90）。
- **`force_release_worktree` GC 模式 (#826, PR #837)** — 對已解散的 agent 額外掃描孤兒 git-level worktree metadata 並強制清理。
- **Daemon 啟動時孤兒擁有者掃描 (#829, PR #835)** — 啟動時掃描所有 assignee 不在 fleet registry 中的任務，嚴格模式自動設為孤兒。
- **`teams::delete` 級聯刪除 (#828, PR #834)** — 解散路徑現在遍歷團隊成員並呼叫 `full_delete_instance`，級聯至 `orphan_tasks_for_owner`。修復產生 ghost-owner 的根因。
- **`task force=true` 旗標 (#808, PR #809)** — 繞過 ACL 清理歷史遺留的 ghost-owned 任務。需要 `force_reason`（記入審計日誌）。
- **`cleanup_init_commits` trailer 感知 body 檢查 (#833, PR #839)** — `KNOWN_TRAILER_KEYS` 白名單剝離後再判斷 commit body 是否為空。daemon 產生的 heartbeat 現在正確分類為空 commit 並可清理。
- **Daemon 速率限制恢復自動提示 (#841, PR #844)** — 偵測到 `ServerRateLimit`/`RateLimit`/`ApiError` 狀態閒置後，自動注入單次恢復提示。`fleet.yaml` 可 per-instance 停用。
- **`ci_watch` 衝突 PR 偵測 (#813, PR #816)** — daemon 現在查詢 PR `mergeable` 狀態；若為 `CONFLICTING`/`DIRTY`，立即發出 `[ci-conflict-detected]` 警報。
- **Dispatch 測試名稱驗證 (#812, PR #815)** — daemon 驗證 send body 中的 `cargo test ... <test_name>` 是否存在於 PR HEAD tree。
- **通知去重 race 修復 (#836, PR #840)** — 基於 `msg_id` 的抑制機制，防止接收者命中速率限制時同一通知最多觸發 3 次。
- **MCP dispatch table 重構 — #694 完成** — 30+ 個工具分支遷移至 dispatch table，啟用未來工具註冊表熱重載。
- **Hung 偵測 F9 影子模式基礎設施 (#685)** — 生產力輸出閘道、fixture 語料庫、per-backend 標記、Stage 1-3 自動恢復/重啟/升級排程。僅影子模式。
- **時區顯示設定 (#790, PR #797)** — 透過 `fleet.yaml` 設定顯示時區（預設 Asia/Taipei）。

### 變更

- **comms.rs `request_id` 傳播 (#1341)** — `comms.rs` 中的 3 個 `api::call` 呼叫點現在包含 UUIDv4 `request_id`，啟用 daemon 的 `DedupCache` 去重。
- **`dispatch_idle` flock (#1340, PR #1347)** — `mark_resolved` 和 `scan_and_emit` 現在使用 flock 序列化，防止並行解析和掃描之間的 race condition。
- **`agy` backend (#987)** — Google Antigravity CLI 作為第六個一級 backend。因 Gemini CLI 2026-06-18 停止服務而新增。
- **`doctor topics` 分類從 4 類減少為 2 類 (#994)** — 移除 `drift_fleet` 和 `stale_registry`。僅保留 `live` 和 `orphan`。
- **`agy` backend 顯示名稱 + workspace-trust 自動關閉 + `fleet_mcp_supported: false` (#995)** — TUI 顯示 `antigravity-cli`；workspace-trust 提示自動關閉；MCP 設定寫入為 no-op。
- **`agend-terminal list --json` 封包 (#938)** — JSON 輸出現在用帶有 `mode` 欄位的 discriminated envelope 包裝 agent 陣列。
- **`AGEND_LOG` 優先級 (#927 PR-A)** — 環境變數設定值現在確實優先於程式內預設值。
- **`telegram_init` 非同步背景化 (#945 Phase 1)** — 冷啟動時間從約 6.6 秒降至約 0.5 秒。
- **CI-watch `correlation_id` 格式 (#946)** — 每個 `system:ci` inbox 通知現在攜帶 `correlation_id = "{repo}@{branch}"`。
- **`dispatch_idle` watchdog fallback `correlation_id` (#947)** — 合成的 fallback 使用規範的 `disp-<unix_micros>-<seq>` 格式。
- **`ci_watch` 檔案身分加固 (#942 / #943)** — watch 檔名現在使用 sha256。舊格式檔案在啟動時遷移。
- **`ci_watch` 跨 bind/release 移交存活 (#931)** — `release_worktree` 不再銷毀 watch 檔案。
- **`agend-terminal admin cleanup-zombies` (#927 PR-B)** — 終止持有過期 run 目錄的殭屍 daemon。
- **啟動掃描環境變數 (#933)** — `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` + `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN`。
- **執行緒狀態傾印環境變數 (#941)** — `AGEND_DAEMON_THREAD_DUMP_SECS`。
- **`.ready` 啟動完成信號 (#922)** — daemon 初始化完成的單一信號策略。
- **`bootstrap-step` 儀表化 (#945 Phase 0)** — daemon log 中的每步驟計時分解。
- **狀態偵測紅色 SGR 錨點 (#919 Phase A)** — `HIGH_FP` 模式現在要求 200 bytes 內出現紅色轉義序列。
- **Telegram inbound 長度分流 (#1352, PR #1353)** — 短訊息（<200 字元）僅 PTY，長訊息 inbox+hint。消除雙路徑重複投遞。
- **Replay cache mtime→generation counter (#1355, PR #1357)** — 使用單調遞增的 generation counter + mtime 四元組修復 mtime 碰撞導致的假快取命中。
- **`task action=list` 預設過濾 (#806)** — 預設只回傳可操作狀態；`include_history=true` 可查看 `done`/`cancelled`。
- **`task action=create` 回應格式 (#807, PR #811)** — 回傳完整 task 物件而非僅 `{id, status}`。

### 修復

- **過濾 pr_state scanner 的 `.lock` 檔案 (#1349, PR #1350)** — pr_state scanner 嘗試將 `.json.lock` sidecar 檔案解析為 JSON，導致每 10 秒 tick 產生 WARN 日誌。
- **TUI 滑鼠選取捲動凍結 (#1356, PR #1358)** — 活動選取期間新輸出到達時，選取座標會漂移。修復在 MouseDown 時快照 `max_scroll()`，並補償 grid 成長以將 viewport 固定在相同內容上。
- **`WIDE_CHAR_SPACER` ratatui buffer cell 洩漏 (#819, PR #823)** — 全形字元過渡到半形字元時，過期字元跨 frame 洩漏。
- **Bridge ↔ daemon 暫時性傳輸失敗下的重複執行 (PR #804 RCA → PR #843)** — bridge 的 `is_retriable_io` 將 `TimedOut` 分類為可重試，導致慢 handler 觸發重試和重複執行。根本修復是 L1 冪等重試架構。
- **`teams::delete` 未清理成員任務 (#828)** — 遺漏的 `full_delete_instance` 呼叫是所有 ghost-owner 累積的根因。
- **`force_release_worktree` 未清理 git-level metadata (#826)** — binding 已移除時未檢查 git-level worktree。新增 GC 模式。
- **生產環境 `task action=list` 回傳 500KB+ (#806)** — 見「變更」中的預設過濾。
- **`cleanup_init_commits` 對 daemon 產生的 heartbeat 無效 (#833)** — 見「新增」中的 trailer 感知 body 檢查。
- **CI 審計 job 自 2026-05-15 起在 main 上持續失敗 (PR #831)** — `permissions: checks: write + issues: write` 修復。
- **通知在消費+重試時三次觸發 (#836)** — 見「新增」中的通知去重。
- **WAF-stage / macOS rustup-init 暫時性問題 (#772 v3, PR #800)** — `cache-bin: false` 防止過期 rustup-init 汙染。
- **PTY 寫入超時防止鏈路死鎖 (#659, PR #679)** — `PTY_WRITE_TIMEOUT = 5s`。
- **`kill_process_tree` PID 0 防護 (#681, PR #687)** — 拒絕向 PID 0 發送信號。
- **`flock` 保護 ci-watch RMW race (#692, PR #731)** — 防止兩個 daemon tick 互相覆寫。

### 移除

- **`requires_daemon_state` 欄位 (#672 / #674)** — 已移除無用欄位。

### 建置/依賴

- **`sysinfo` 0.32 → 0.39** — API 遷移。
- **`rustls-webpki` 升級** — 安全公告修復。
- **`twilight-model` 0.16 → 0.17.1**（Discord channel 依賴）。
- **`libc`、`uuid`、`which`** — 例行 dependabot 升級。

### 測試基礎設施

- **`admin::cleanup_zombies::poll_until_dead` (#934)** — 確定性輪詢原語，取代基於 sleep 的等待。
- **`api::handlers::instance::await_sentinel_nonempty` (#949)** — 重新命名以釐清合約：等待檔案有內容，而非僅存在。
