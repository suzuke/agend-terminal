[English](architecture.md)

# AgEnD Terminal — 架構地圖

> 當前狀態的結構地圖，整理自 #2050 架構盤點
> （六份子系統調查，所有論述都已對照 `main` @ `65d9ad8` 驗證，
> 2026-06-12）。搭配文件：[REFACTOR-PLAN.md](REFACTOR-PLAN.md)——由本地圖
> 衍生出的分階段計畫。
>
> 新進人員：請先閱讀 [ARCHITECTURE-QUICK-START.md](ARCHITECTURE-QUICK-START.md)。
> 重寫時期的原始設計文件已封存於
> [archived/architecture-design-doc-2026-05.md](archived/architecture-design-doc-2026-05.md)。
> Lock 紀律記載於 [DAEMON-LOCK-ORDERING.md](DAEMON-LOCK-ORDERING.md)，
> 此處只做摘要（不重複內容）。

## 1. 這是什麼

一個長時間運行的 daemon，將多個 AI coding agent
（claude / codex / kiro / agy / opencode）以 PTY 子程序的形式加以 supervise，
並提供：agent 之間的 MCP 通訊、fleet-as-code 配置、每個 agent 獨立的
git-worktree 隔離、附帶自動重生（auto-respawn）的健康監控，以及 TUI +
Telegram 遠端控制。核心價值在於 **multi-agent orchestration**；
其餘一切都是為它服務。

約 198K 行 Rust 程式碼，分布於 286 個檔案。三個 binary：`agend-terminal`（daemon +
TUI + CLI）、`agend-mcp-bridge`（每個 agent 的 stdio↔TCP MCP 中繼）、
`agend-git`（PATH-shim 的 `git` 政策閘門）。

## 2. 子系統地圖

六個子系統，依規模排列。每個子系統在 #2050 盤點中都有專屬的調查軌跡；
下方的 file:line 證據是承重的核心子集。

### 2.1 Daemon 核心 / 生命週期（約 52K LOC）

核心所在：觀察每個 agent 的狀態機、做出反應、進行復原。

| Module | LOC | 角色 |
|---|---|---|
| `daemon/supervisor.rs` | 5408 | 每個 agent 狀態的 OBSERVATION + 反應發送 + 錯誤復原（SRL/ApiError）。原本 12 個 inline slow-tracker 掃描已在 W1.1（#2065）移至 `PerTickHandler` |
| `daemon/mod.rs` | 2764 | 進入點/初始化/關閉；建構 per-tick handler 集合（`build_default_handlers` → 32 個 handler） |
| `daemon/per_tick/` | — | `PerTickHandler` trait + 32 個 handler 實作、panic-guarded dispatch、boot-grace 閘門 |
| `daemon/crash_respawn.rs` | 568 | Crash→respawn 決策：health budget、escalation 持久化、respawn worker |
| `health.rs` | 2385 | 每個 agent 的 hang/crash budget、blocked-reason、escalation 持久化+rehydrate |
| `state/mod.rs` | 2176 | 每個 agent 的 `StateTracker`——screen-heuristic 狀態機（見 2.4） |

週期性工作現在是單一 pipeline（W1.1 / #2065，見 §6.1）：daemon
迴圈會 dispatch 全部 32 個 `PerTickHandler`，每個 handler 都有 `catch_unwind` +
計時（per_tick/mod.rs:182-214）。原本在 supervisor `run_loop` 中 inline 執行的
12 個 tracker，已包裝成 handler 並依其原本的相對順序附加；supervisor `run_loop`
如今只負責 `tick()` /
`process_error_recovery()` / 一個 boot-time sidecar GC（仍是它自己的 10s 執行緒）。

### 2.2 MCP layer + agent 間通訊（約 22K LOC）

宏觀架構上是健全的：單一不可變的 36 筆 registry
（`mcp/registry.rs:20`）將 JSON-schema 定義與 handler
fn-pointer 配對，並透過一個經過驗證的 chokepoint
（`mcp/handlers/dispatch.rs:77-117`）來 dispatch。壓力是微觀層面的：兩個 handler 檔案
卡在 750-LOC 的 `file_size_invariant` 上限，而目前為止的拆分都是 size-driven 而非
concept-driven（見 §6.4）。

傳輸主幹（一次 agent 對 agent 的 `send`）：

```
agent LLM → MCP tools/call {request_id: UUID}     bin/agend-mcp-bridge.rs:327
  → content-dedup (500ms double-fire guard)        agend-mcp-bridge.rs:146
  → cookie handshake → loopback TCP api socket     ipc.rs / api/mod.rs:229
  → operator_gate.check_operation_allowed          api/operator_gate.rs:121
  → request_dedup.dispatch (idempotent retry)      api/request_dedup.rs:165
  → messaging::handle_send (5 phases)              api/handlers/messaging.rs:610
      validate → team/quota gates → build message
      → route_and_deliver → inbox::enqueue (flock + atomic rename)
      → post side-effects (provenance/verdict/dispatch-tracking)
  → target's next poll: inbox::drain (48KB byte-cap, keeps the
    response dedup-cacheable so a lost-transport retry serves the
    cached batch)                                   inbox/storage.rs:286-407
  → daemon wake: compose_aware_inject → PTY        inbox/notify.rs:269-361
```

Idempotency 主幹：bridge 只產生一次 `request_id`，並在 retry 時原封不動沿用；
`DedupCache`（Fresh/InProgress/Cached/Oversized）的界限為：
TTL 10min、64KB/entry、64MB 總量、每個 id 8 個 waiter。

task board 是 event-sourced（`task_events.rs`，約 20 種 event variant，
full-replay 讀取以 file-len/mtime/generation 做快取），並具備 fail-closed 的
forward-compat。

### 2.3 Channels / API / TUI / render（約 24K LOC）

兩套重疊的遞送架構（見 §6.5）：agent 對 agent 的 send
流經 `messaging.rs`；operator/channel 通知流經 channel
adapter + UX sink + 一個持久化的 deferred-notification 佇列
（`notification_queue.rs`），由 TUI 迴圈或 daemon 的 `notification_flush` handler
其中之一恰好一次（exactly-once）drain 完（每個 agent 一個 OS file lock + 一個唯一的
claim 檔——僅靠 rename 仲裁，過去在 CI 下曾發生重複遞送）。

TUI：`Layout`/`Tab`/`PaneNode` 擁有拓樸；`render/core_render.rs` 擁有
繪製；`VTerm`（alacritty wrapper）擁有 terminal emulation。在 #2048
之後有兩個刻意設計的 resize chokepoint——layout 預先計算尺寸
（`layout/mod.rs:294`），render 對最終內層內容
rect 具有權威（`render/core_render.rs:437`）。render 刻意不是 pure 的：它會
drain pane 輸出，並可能在繪製前 resize VTerm/PTY。

### 2.4 State detection / backend profiles（約 13K LOC）

公認最脆弱的子系統——orchestration 的雙眼。

Screen-heuristic 分類器：PTY bytes → vterm grid → 每個 backend 依序排列的
regex first-match（`state/patterns.rs:244`）→ 一連串對順序敏感的閘門
（anchor/position/working-marker/recovery/phantom-probe/
UsageLimit-release/heartbeat 閘門）→ 單一 transition funnel
`record_set`（state/mod.rs:2038）。Pattern 優先序僅由 Vec
位置決定——first-match-wins，沒有 compile-time precedence invariant。

對於支援 hook 的 backend（目前是 claude）存在第二條、具權威性的路徑：
backend hook 會 POST event → `daemon/hook_shadow.rs`；
`authoritative_state()` 會讓 Fresh 的 hook state 凌駕於 heuristic 之上，
受 flag 控制，且**僅限 snapshot 範圍**（#1523 phase-1，即
`per_tick/snapshot.rs:51` chokepoint）。仍有五個 per-tick decider 讀取
原始 heuristic state（見 §6.2——計畫中的 phase-2 收斂）。

每個 backend 的設定乾淨地集中在 `BackendProfile`
（每個 backend 的 patterns/behavioral/markers）；`backend.rs` 擁有 spawn、
model-args、resume，以及一個 6-arm preset factory。

### 2.5 Worktree / git / fleet（約 9K LOC）

每個 agent 的 git 隔離。dispatch→bind→worktree 串接鏈：

```
dispatch_auto_bind_lease_* (mcp/handlers/dispatch_hook/)
  → BindGuard (per-agent in-flight gate)
  → per-branch flock lease (binding.rs:92) — serializes same-branch races
  → scan_existing_branch_binding — reject if another agent holds it
  → ensure_branch_exists (4-tier repo resolve; #2010 remote resolution;
    #869 stale-ref refresh)
  → worktree_pool::lease → create + .agend-managed marker + bind_full
  → auto-arm ci-watch (+ next_after_ci chain target)
```

`binding.json` 只有單一 writer（`binding::bind_full`），而 reader 都經過
HMAC 驗證（git shim 不信任任何未簽章的東西）。所有
daemon 內部的 git 都經過 `git_helpers::git_bypass`（timeout +
process-group kill，因此 timeout-kill 永遠不會拖垮 daemon 自己的
process group）——但程式碼庫中仍有 209 個 call site 直接建構原始的
`Command::new("git")`（見 §6.3）。

`agend-git` 的 PATH shim 是權威邊界：`classify()`
（bin/agend-git.rs:679）會根據 agent 的 binding，把每個 subcommand 閘成
passthrough/chdir-pass/deny/exempt。Branch GC
（`branch_sweep` + `worktree_cleanup`）共用同一個 squash-merge 偵測器，並在
delete transaction 內執行 `git worktree prune`（#2011）。

### 2.6 Ops / 進入點（bins、service、deployments、schedules、skills）

`main.rs`（1338 LOC）是 CLI 總機：app/daemon/attach/admin/
service/doctor/skills/quickstart/verify。Service 生命週期委派給
OS supervisor（launchd / systemd user / Task Scheduler）——daemon
不會 supervise 自己。`deployments.rs`（2697）擁有 template
deploy/teardown 生命週期，並刻意採用範圍很窄的 store flock（self-IPC
一律在 lock 之外）。`schedules.rs`（1480）把儲存與
runtime executor（`daemon/cron_tick.rs`）分開。Release pipeline：annotated tag →
gate（version/changelog/MSRV/semver）→ 5-target build + AppImage → GH
Release → crates.io publish（見 RELEASING.md）。

## 3. Concurrency model

完整階層與理由：[DAEMON-LOCK-ORDERING.md](DAEMON-LOCK-ORDERING.md)。
以下是承重的紀律，每一條都由 runtime 或 CI 強制：

| 紀律 | 規則 | 強制方式 |
|---|---|---|
| Lock order | registry (L0) → per-agent core (L1) → side channels (L2) → heartbeat snapshot (L3)；釋放時反向 | `sync_audit::assert_lock_tier` runtime assert |
| #1492 | 持有 registry/core lock 時不得 self-IPC | `assert_no_registry_lock_for_self_ipc`（api/mod.rs:784）——fail-fast 回傳 Err，而非 deadlock；90s socket-read backstop |
| #1530 | 在 `core.lock()` 之下收集 reaction intent，在 drop 之後才發送 | 即 `let action = { … }` 的區塊邊界（supervisor.rs:1282-1293）；CI source-pin `tick_emitters_run_after_core_lock_drops`（#1644） |
| Inbox | 每次 enqueue/drain/sweep 都用 flock + atomic rename；每個 agent 一個 `.jsonl.lock` | 由結構保證（inbox/storage.rs:99-139） |
| Deferred notifications | 每個 agent 一個 OS file lock + 一個唯一的 process claim 檔 | 由結構保證（notification_queue.rs:401-424） |
| Deployment store | 僅在 load-modify-save 周圍上 flock；self-IPC API 呼叫在 lock 之外 | 由結構保證（deployments.rs:434-445） |
| git subprocesses | process-group 隔離（Unix `process_group(0)` / Windows `CREATE_NEW_PROCESS_GROUP`），讓 timeout-kill 解決 child pgid | git_helpers.rs |
| UX sinks | mutex 只保護 sink vec；clone Arc，釋放後才發送；發送為 fire-and-forget | sink_registry.rs:67-74 |
| Per-tick handlers | 每個 `run()` 都包在 `catch_unwind` 內——一個 panic 永遠不會跳過其他兄弟 | per_tick/mod.rs:182-214 |

一個已稽核的殘留問題：在 supervisor.rs:1857 附近，registry-lock 範圍內有一個
`handle.core.lock()`——已在 #1530/F2 標記；任何 supervisor 重構前都應先讀過它。

## 4. 承重的 invariant（lock 之外）

| Invariant | 位置 | 違反時會壞掉什麼 |
|---|---|---|
| State pattern 順序：error 在 Thinking/Idle 之前，first-match wins | `BackendProfile.patterns` 的 Vec 順序 | 誤分類 → 錯誤的 idle/hang 反應 |
| `feed_with_fg` 中的 gate-gauntlet 順序（position gate 在 working-marker override 之前等等） | state/mod.rs:1208-1757 | FP 抑制機制停止組合 |
| Hook 升級只能透過 `authoritative_state()`（把 freshness 與 `has_state_hooks()` 耦合）——絕不直接走 `resolved_state_for` | hook_shadow.rs:113-123 | 在非 hook backend 上信任了過期的 hook |
| Hook ToolUse 是 event-pair-closed，而非 clock-bounded（刻意如此——保護長時間執行的工具，即 #1985 那一類） | hook_shadow.rs:79-98 | 加上 clock backstop 會重新弄壞 #1523 當初要修的東西 |
| `request_id` 在 bridge 處只產生一次，retry 時沿用 | agend-mcp-bridge.rs:329-340 | transport retry 造成重複的 side effect |
| Inbox drain 回應 ≤ 48KB，使其維持 dedup-cacheable（< 64KB） | inbox/storage.rs:270 | lost-transport retry 會漏掉剩餘部分 |
| Fleet allowlist 解析是 fail-closed：一筆格式錯誤的 entry 就讓整份清單失敗 → 下游 auth 全部拒絕 | fleet load / `is_authorized_recipient` | 靜默的部分授權 |
| binding.json 單一 writer + HMAC sidecar | binding.rs | shim 信任了偽造的 binding |
| Task event log：在 lock 之下 append 並 re-replay（`append_checked`），對未知的未來 event 採 fail-closed | task_events.rs:1034,1538 | TOCTOU 造成 board 損毀 / 靜默丟失 event |
| Boot grace（180s）在 restart 後抑制 notification watchdog | per_tick/mod.rs:118 | restart 時爆出一連串假警報（每個 handler 手工接線——見 REFACTOR-PLAN W2） |
| Per-tick handler 順序須與 pre-extraction 的呼叫順序一致 | daemon/mod.rs:577-579 | 細微的反應重新排序 |
| `spawn` 處須附帶 fire-and-forget 理由，或保存 JoinHandle | protocol §10.4、Phase-5b invariant test | 關閉時留下 orphan task |

## 5. 橫切（cross-cutting）模式

- **Per-agent latch map**：帶有客製化 prune 的 `Mutex<HashMap<String, T>>`
  出現約 10 次（supervisor notify/retry track、context_handoff/alert
  state、inbox_stuck、handoff_timeout 等）。
- **Cadence counter**：`AtomicU64` + `is_multiple_of(N)` 的手刻寫法，在 per-tick
  handler 中出現約 15 次。
- **Schema-version guard**：三個幾乎相同的實作
  （`FLEET_SCHEMA_VERSION`、`BINDING_SCHEMA_VERSION`，以及 store 層級的
  `SchemaVersioned` trait）。Fleet 是 warn-not-refuse（一個手工編輯的 public
  interface，見 COMPATIBILITY.md）；store 則是 fail-closed。
- **變成承重的 instrumentation**：#1808 SRL phantom-probe
  欄位如今驅動了跨週期的 SRL→Idle 修正（state/mod.rs:1547）——
  那裡的 telemetry 與 classification 已不再可分離。

## 6. 已知的架構張力

這些是刻意或日積月累形成的雙路徑。每一項都是 REFACTOR-PLAN 的
條目；沒有一項能在不顧及所列注意事項的情況下「隨手清乾淨」。

1. **兩套週期性工作機制**（§2.1）：✅ 已由 W1.1（#2065）解決。原本 12 個
   inline supervisor `maybe_scan` tracker，如今都是同一條
   `build_default_handlers` pipeline 中的 `PerTickHandler`（32 個 handler）；一個
   completeness invariant 釘住整組集合，使這套雙機制的分裂無法靜默重新出現。
2. **Heuristic vs hook state，雙 reader**（§2.4）：snapshot 路徑已被
   升級（hook-aware），但 hang/watchdog/recovery/supervisor escalation
   仍讀取原始 heuristic——在單一個 tick 內，dispatch_idle 可能抑制
   一次 nudge（snapshot=ToolUse），同時 hang_detection 卻在 escalate
   （raw=heuristic-Idle）。目前由 #1999 escalation throttle 限制其範圍。
   收斂 = #1523 phase-2（W3；需要 lock-ordering 設計——一個
   天真的共享快取會反轉 registry⨯core 順序）。
3. **原始 `Command::new("git")` vs `git_bypass`**：約 150 個 daemon 原始 git
   site，分布於約 25 個 module；任何忘了 `AGEND_GIT_BYPASS=1` 的 site 都會靜默撞上
   shim（一整類的 flaky-test）。W1.2（#2068）導入了 `git_cmd`/`git_ok`，
   並把它最先的 4 個 module（`branch_sweep`、`worktree_cleanup`、
   `worktree_pool`、`binding`）封印在 `tests/daemon_git_helper_invariant.rs` 之後，這是一個
   per-slice 的 `MODULE_SCOPE` 掃描器，會隨著每個後續的 slice
   migrate 其 module 而單調成長。其餘 module 是待辦事項
   （`t-…766-17`）——這道封印只讓已 migrate 的 module 在結構上不可能 regression，
   絕不宣稱尚未掙得的覆蓋率。
4. **MCP 上限處的 size-driven 拆分**：`instance.rs`/`comms.rs` 卡在
   750-LOC 上限，帶有「為了 file_size_invariant 而拆分」的 cross-file
   接縫；`handle_delegate_task` 是一個 317-LOC 的 god-fn，gate 都 inline 在裡頭。
   concept-driven 的重新拆分是 W2。
5. **Channel trait vs 舊式 notify 函式**：`Channel::notify`
   委派給較舊的具體 `notify_telegram*` 進入點；該 trait
   尚未成為唯一的 production 路徑。任何 adapter 工作前都需先做歸屬決策（W4 設計決議）。
6. **#2048 之後的兩個 resize chokepoint**：刻意如此（layout pre-pass +
   render last-mile 權威），但在此之前一直未被記錄為 invariant；
   兩者都保留，並把這份合約命名（W2）。

## 7. 測試與強制執行的全貌

CI：3-platform Check + LOC-overrun gate + cargo audit（required）；
Coverage + daemon-boot flake-gate（non-required）。架構層級的
invariant 由專屬測試釘住：`file_size_invariant`（750-LOC
handler 上限）、`tick_emitters_run_after_core_lock_drops`（#1644）、
heartbeat-pair atomicity 稽核、spawn-rationale invariant（Phase 5b）、
`daemon_git_helper_invariant`（#2068——per-slice 的 `MODULE_SCOPE` 封印：已 migrate 的
module 中不得有未標記的原始 `Command::new("git")`）。
Worktree 端的測試執行需要 `AGEND_GIT_BYPASS=1`（shim 會在受管 worktree 中攔截
原始 git subprocess）。Review 紀律 §3.9：測試須透過真實進入點
搭配具代表性的 fixture 進入——合成的 unit-inject fixture 一再隱藏了 production 接線的漏洞。

## 8. 出處

整理自六份 #2050 調查文件（daemon-core、mcp-comms、
channels-tui、state-detection、worktree-git-fleet、ops-entry），由
fixup-dev、fixup-dev-2 與 fixup-reviewer 對照 `main` @ `65d9ad82` 撰寫，
另加上 2026-06-10 的 production-readiness 稽核。行號會漂移；
那些命名過的 anchor（函式名、invariant test 名、issue 編號）才是
穩定的參照。請在 REFACTOR-PLAN 的某一波 wave 落地時更新本地圖，
而非每個 PR 都更新。