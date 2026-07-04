[English](SOURCE-OF-TRUTH.md)

# Source-of-Truth Matrix — 真相源矩陣

**狀態**：ACTIVE — 工程規範。新增狀態、新增儲存、或替既有狀態新增一個讀取者，
合併前都必須在此分類。

**由來**：把 `workspace/fugu-0acdd8/agend-terminal-solutions.md` §3.2–§3.6
（source of truth 分散）正式化。動機：真相分散是實證性的失效類型——2026-07 的
branch 堆積根因就是一個「沒人知道已死的 stale 欄位」（`worktree_source`，task
`…67777-3`）；post-#994 的 `topics.json` single-source 規則、`binding.json`
真相源修法，都是個案式各自確立的。本文件把它們系統化。

**已核實**：以下每一個 `path:line` 都對照 `origin/main` @ `15ab267f`
（2026-07-04）逐條核對，不是憑印象寫。當你改動列出的入口時，請同步更新此處的行號。

---

## 三種資料類型

每一份狀態只能屬於其中一種。

| 類型 | 定義 | 規則 |
|---|---|---|
| **Source（真相源）** | 唯一可寫的真相，mutation 發生在此。 | 只有這個 store 可以被當作權威來源。可以是檔案，也可以是 in-memory。 |
| **Projection / Cache（投影／快取）** | 由 Source 衍生、可重建、可能 stale。 | 絕不能作為「不可逆」mutation 的依據。讀取者可以用它，但須遵守下方 fail-open 規則。 |
| **Side-effect（副作用）** | 對外送出（PTY 一行、Telegram/Discord 訊息、CI 通知）。 | 送出即消失，永遠不要把它讀回來當狀態真相。 |

**Projection 讀取者的 fail-open 規則**（見下方 `snapshot`）：decider *可以* 讀
projection，但若 projection 缺失或 stale，decider 必須退回保守、冪等的 fallback
——絕不做毀資料或其他不可逆的動作。

---

## 總覽矩陣

| 狀態 | 唯一真相源 | 真相源類型 | 非真相源副本 |
|---|---|---|---|
| Instance 宣告式設定 | `fleet.yaml` | Source（檔案） | in-mem `FLEET_CACHE` = 投影 |
| Teams | `fleet.yaml` `teams:` 區塊 | Source（檔案） | runtime team view = 投影 |
| Live agent 行程 | in-memory `AgentRegistry` | Source（in-mem） | `.port` 檔 = discovery index（投影） |
| Daemon discovery | 活體 daemon 行程 + run dir | Source（行程） | `api.port`、`api.cookie` = 投影 |
| Task 狀態 | task event log（`task_events.jsonl`） | Source（append-only 檔） | rendered task list = 投影 |
| Inbox 狀態 | inbox storage（`inbox/<name>.jsonl`） | Source（append-only 檔） | PTY 注入訊息 = side-effect |
| Decision 狀態 | decision store（`decisions/*.json`） | Source（檔案） | channel 通知 = side-effect |
| Worktree lease/binding | `binding.json` | Source（檔案） | git worktree 目錄 = materialized state；`worktree_source` = **死欄位** |
| Agent runtime 狀態 | in-memory `StateTracker` | Source（in-mem） | `snapshot.json` = fail-open 投影 |
| Channel / topic 綁定 | `topics.json` | Source（檔案） | `fleet.yaml` `topic_id` = fallback |
| CI watch | `ci-watches/<hash>.json` sidecar | Source（檔案） | CI 通知 = side-effect |
| pr_state | *（無——快取）* | Projection / Cache | GitHub 才是終局真相 |

---

## 各狀態細節（讀寫入口）

### Instance 宣告式設定 — 真相源：`fleet.yaml`
- **寫**：`src/fleet/persist.rs:18` `mutate_fleet_yaml()`（取鎖 `:13`，再
  `atomic_write_yaml` `:5`）；如 `add_instances_to_yaml:43`。
- **讀**：`src/fleet/mod.rs:618` `FleetConfig::load()` → `load_arc:585` →
  `load_uncached:621`。
- **投影**：`FLEET_CACHE` mtime/size 快取（`src/fleet/mod.rs:585-599`），寫入後
  由 `invalidate_cache()`（`src/fleet/persist.rs:9`）主動清除。
- ⚠ **寫入口尚未完全收斂**：`src/quickstart.rs:814` 用裸 `std::fs::write` 寫
  `fleet.yaml`，繞過 lock + atomic 路徑。此處僅限互動式 quickstart 的覆寫確認分支
  （`:743-758`），非常駐 runtime 路徑。這裡登記為例外，不是第二個真相源。

### Teams — 真相源：`fleet.yaml` `teams:` 區塊
`src/teams.rs:1-10` 明文「operator-edited fleet.yaml is the source of truth」。
- **寫**：`src/fleet/persist.rs:327` `add_team_to_yaml()` / `:370`
  `remove_team_from_yaml()` / `:387` `update_team_in_yaml()`，由
  `src/teams.rs:222` `create()` 呼叫。
- **讀**：`src/teams.rs:61` `load_fleet()` → `FleetConfig::load`。
- **投影**：`src/teams.rs:117` `project_team()`。`list()`（`src/teams.rs:399`）
  交叉活體 registry（`src/runtime.rs:28` `list_live_agents`）標記
  `stale_members`——但從不 mutate `fleet.yaml`，證實 registry 是獨立的活體真相。

### Live agent 行程 — 真相源：in-memory `AgentRegistry`
`src/agent/mod.rs:129` `Arc<Mutex<HashMap<InstanceId, AgentHandle>>>`，非持久化。
- **寫**：`src/daemon/mod.rs:1758-1763` 插入（spawn）；刪除經
  `lifecycle::delete_transaction`。
- **讀**：`agent::lock_registry`（如 `src/daemon/mod.rs:1775`）。
- **投影**：`.port` 檔。`src/ipc.rs:47` `write_port`（atomic），由
  `src/daemon/tui_bridge.rs:61`（每 agent 一個，於 registry 插入*之後*才寫）。
  讀：`src/ipc.rs:49` `read_port`。`src/ipc.rs:1-4` 自述 port 檔為 discovery
  index。回滾佐證：prep 失敗時 `src/daemon/mod.rs:1786-1791` 跑
  `delete_transaction`，回滾 registry 並清 port——registry 為主、port 為投影。

### Daemon discovery — 真相源：活體 daemon 行程 + run dir
- **Run dir**：`src/daemon/mod.rs:292` `run_dir()` / `:299` `run_dir_for_pid()`
  （`home/run/<pid>`，PID 即鍵）。身分戳記由 `:440` `write_daemon_id()` 寫入
  （atomic `:451`）。讀取由 `:323` `find_active_run_dir()`（掃描 + PID 存活 +
  `.daemon` 身分核對，`:338-370`）。
- **api.port**（投影）：`src/api/mod.rs:241` → `crate::ipc::write_port`；讀
  `src/ipc.rs:84` `connect_run_dir_api`。
- **api.cookie**（投影）：`src/auth_cookie.rs:29` `issue()`（0600、tmp+rename），
  由 `src/daemon/mod.rs:546`；讀 `src/auth_cookie.rs:70` `read_cookie`。檔名
  `api.cookie`（`src/auth_cookie.rs:23`）。

### Task 狀態 — 真相源：task event log
`src/task_events.rs:3-4`——「Source-of-truth storage for task board state」。
- **寫**：`src/task_events.rs:1062` `append`；`:1093` `append_batch_at`（實際落盤）。
- **讀／投影**：`src/tasks/mod.rs:383-388` `list_all_at` →
  `task_events::replay_at`（`src/task_events.rs:1636`）。rendered task list 是
  replay 投影。
- **反繞過不變量（已強制）**：`tests/task_events_invariant.rs:5-7`——只有
  `src/task_events.rs` 可引用 `task_events.jsonl` 字串；其餘所有生產呼叫者都必須經
  `append` / `append_batch` 公開 API。許多模組*確實*直接呼叫 `append*`（如
  `src/schedules.rs:931`、`src/daemon/idle_watchdog.rs:976`、
  `src/api/handlers/messaging.rs:206`）——那是允許的；直接*開檔*才是禁止的。

### Inbox 狀態 — 真相源：inbox storage
`src/inbox/mod.rs:1-3`——append-only JSONL，每 agent 一檔
（`{home}/inbox/{name}.jsonl`）。
- **寫**：`src/inbox/storage.rs:170` `enqueue`（flock + append + fsync，
  `:178-191`）。
- **讀**：`src/inbox/storage.rs:421` `drain` / `:662` `ack`（storage 本身*即*真相，
  無另建投影）。
- **Side-effect**：`src/daemon/delivery_worker.rs:116-128` `dispatch()` 處理
  `PtyWake` job → `src/inbox/notify.rs:675-696` `inject_with_submit_direct` 把該行
  寫進 agent 的 PTY。注入的那行是 delivery side-effect，不是真相。
- 註：inbox 無反繞過不變量測試（不像 task_events）；`enqueue` 被多個模組直接呼叫。

### Decision 狀態 — 真相源：decision store
`src/decisions.rs:1`——CRUD over JSON files in `{home}/decisions/`。
- **寫**：`src/decisions.rs:170` `save`（經 `store::save_atomic`），由 `:191`
  post / `:416` update / `:505` answer。
- **讀**：`src/decisions.rs:127` `load_all`、`:363` `list_all`、`:395` `list`。
- **Side-effect**：`src/mcp/handlers/task.rs:13-24` `handle_post_decision` 呼叫
  `decisions::post` 後，emit `UxEvent::Fleet(FleetEvent::PostDecision{…})`
  （`src/channel/ux_event.rs:113,240-249`）→ Telegram/Discord 通知。
- 邊界例外：`src/daemon/retention/decisions.rs:56-66` 的 GC/歸檔用
  `std::fs::rename` 直接搬檔，但仍在 `decisions::with_decision_lock`
  （`src/decisions.rs:180`）之下協調。

### Worktree lease/binding — 真相源：`binding.json`
Daemon-only writer；`agend-git` shim 與 hooks 是唯讀消費者（`src/binding.rs:1-4`）。
- **寫**：`src/binding.rs:266-390` `bind_full`（寫 `:373-375`）；清除經 `unbind`
  `src/binding.rs:566-596`（移除 `:586`）。
- **讀**：`src/binding.rs:720-743` `read`（先查 in-memory index，再讀盤）。
- **Materialized state（非真相源）**：git worktree 目錄本身，建於
  `src/worktree.rs:81`。`src/worktree.rs:5-10,48`——canonical layout 下
  「production code reads `binding.source_repo` directly」（`binding.rs:371`）。

### Agent runtime 狀態 — 真相源：in-memory `StateTracker`
`src/state/mod.rs:241`——per-agent 狀態，持於 agent core lock 之下。
- **投影寫入**：`src/daemon/per_tick/snapshot.rs:29-90`
  `SnapshotRotationHandler::run` 從 `handle.core.lock()`（`:40-63`）讀取，組成
  `AgentSnapshot`，再呼叫 `crate::snapshot::save`（`:87`；落盤
  `src/snapshot.rs:48-57`）。

### snapshot — Projection（fail-open）
`snapshot.json` 是 `StateTracker` 的 read-optimized、file-based 投影，每 tick 覆寫
以供 lock-free 讀取（`src/daemon/per_tick/snapshot.rs:30-37`）。它**確實**被
decider 讀取，所以 fail-open 規則是強制的。現行四個讀取者，全部核實為保守：

| Decider | 讀取 | snapshot 缺失/stale 時的 fallback |
|---|---|---|
| dispatch idle | `src/daemon/dispatch_idle/mod.rs:1041` `snapshot::load` | `target_is_working` false → 照樣觸發（可逆的）nudge（`:942-946`；測試 `no_snapshot_falls_back_to_firing_1516` `:3559-3575`） |
| inbox inject | `src/inbox/notify.rs:326` `agent_state_of` | `None` → 不 defer → 直接 inject（`:360-397`、`:423`） |
| handoff timeout | `src/daemon/handoff_timeout_watchdog.rs:192,213` `agent_is_busy` | 缺失 → 非忙碌 → 重新 nudge（可逆） |
| reply ledger | `src/reply_ledger.rs:374` `agent_is_busy` | 缺失 → `emit_warn` + `NudgeAgent`（`:386-392`）——絕不做不可逆刪除 |

欄位層級基座：`src/snapshot.rs:21-38`（`#[serde(default)]`）+
`src/snapshot.rs:44-46` `default_silent_secs() → i64::MAX`——缺欄位一律讀成「非常
安靜」而非「忙碌」，逼每個 decider 走「繼續動作」（絕不靜默吞掉）的路徑。沒有任何
snapshot 讀取者對缺失/stale 做不可逆 mutation（刪 worktree/branch 等）。

### Channel / topic 綁定 — 真相源：`topics.json`
`src/bootstrap/doctor_topics.rs:10`——「topics.json is the single source of
truth」。`src/channel/telegram/inbound.rs:142`——「topics.json is the canonical
source for topic_id → instance mapping」。
- **儲存**：`src/channel/telegram/topic_registry.rs:15-17`
  （`home.join("topics.json")`）。
- **寫**：`topic_registry.rs:42-60` `register_topic`（flock read-modify-write），
  經 `create_topic_for_instance:111`。
- **讀**：`src/channel/telegram/inbound.rs:130-153` `resolve_topic`；
  `src/fleet/resolve.rs:159-163`。
- **Fallback（非真相源）**：`fleet.yaml` `topic_id`（`src/fleet/mod.rs:456`、
  `:824`）。經 `.or(inst.topic_id)`（`src/fleet/resolve.rs:159-163`）在*每次*
  resolve 都讀——非僅 bootstrap——但只在 `topics.json` 無該 instance 項時才生效。
- **治理欄位**：`topic_binding_mode`（#2606，`src/fleet/mod.rs:512`、`:895`；由
  `list_instances` `src/mcp/handlers/instance_queries.rs:49-55` 曝露；gate 在
  `topic_registry.rs:161`）決定*要不要*綁——它不與 `topic_id` 競爭權威性。

### CI watch — 真相源：`ci-watches/<hash>.json` sidecar
- **儲存**：`src/daemon/ci_watch/registry.rs:4-6`（`home/ci-watches/`）。
- **寫**：`src/mcp/handlers/ci/watch.rs:7` `handle_watch_ci`（`atomic_write`
  `:188`）；unwatch `:251` / `:322` / `:360`。
- **讀**：`src/daemon/ci_watch/poller.rs:468` `check_ci_watches_with_provider`
  （`read_dir:473`），由 `src/daemon/per_tick/ci_watch_poll.rs:28` 每 tick 驅動。
- **Side-effect**：`src/daemon/ci_watch/poller.rs:1983` `deliver_ci_watch`（通知）。

### pr_state — Projection / Cache（無本機真相源）
PR verdict/CI 狀態的可重建快取；GitHub 才是終局真相。
- **儲存**：`src/daemon/pr_state/mod.rs:458-460`（`home/pr-state/*.json`，key 為
  repo+branch，非 PR number，`:484-488`）。可重建
  （`src/daemon/pr_state/scanner.rs:14-19`）。
- **寫**：`src/daemon/pr_state/mod.rs:917` `record_verdict`（由
  `src/api/handlers/messaging.rs:480,489,497`）；`record_ci_result:836`（由
  `src/daemon/ci_watch/poller.rs:2160`）。
- **讀**：`src/daemon/pr_state/mod.rs:492` `load`；`:528` `with_pr_state`
  （生產路徑）。消費者：`src/daemon/handoff_timeout_watchdog.rs:52-61`。

---

## 歷史個案（本文件為何存在）

以下每一件都是真實事故：非真相源副本被誤當真相，或真相源欄位悄悄死掉。它們是本矩陣
的實證基礎。

### 1. `topics.json` vs `fleet.yaml` `topic_id`（#2598）
兩者一度都像是 topic→instance 的真相。#2598（`a0bf79e6`）定調：`topics.json`
才是權威；`fleet.yaml` `topic_id` 是 best-effort 鏡寫。在
`bind_topic_for_instance`（`src/channel/telegram/topic_registry.rs:172-186`）中
`topics.json` **先**寫，而 `fleet.yaml` 鏡寫失敗只 warn（`:182`）——不擋 `Bound`
結果。**教訓**：一個「可以失敗而不使操作失敗」的鏡像，是投影，不是真相源。

### 2. `binding.json` vs `worktree_source` — 一個死欄位（task `…67777-3`）
2026-07 的 branch 堆積根因是 `AgentConfig.worktree_source`，它在現行架構下
**恆為 `None`**：
- `src/worktree.rs:52-60` `source_repo_of` 只認 legacy `{repo}/.worktrees/{name}`
  版面（`.any(|c| c == ".worktrees")`），對 canonical 的
  `$AGEND_HOME/worktrees/…` 版面一律回 `None`。它自己的 docstring（`:44-51`）就寫
  「retained for legacy-layout detection only」。
- `src/daemon/mod.rs:1676-1678` 在 spawn 時經 `source_repo_of` 從 working dir 設一次
  `worktree_source` → 因此恆為 `None`。
- `src/daemon/per_tick/worktree_registry_sweep.rs:54-71` 把該欄位餵給
  `worktree_cleanup::sweep_from_registry`，後者用它當 repo scope
  （`src/worktree_cleanup.rs:229-232`）。

串聯：canonical layout → `source_repo_of` = `None` → `worktree_source` =
`None` → registry 衍生的 repo 集合恆為空。worktree 來源 repo 的真相其實是
`binding.json.source_repo`（`src/binding.rs:371`），生產程式碼直接讀它
（`src/worktree.rs:48`）。**教訓**：由「不再符合現實」的 helper 衍生出來的欄位，會變
成沉默的死欄位——當 helper 假設的版面改變時，要稽核衍生 helper。

### 3. snapshot fail-open（solutions.md §3.4）
`snapshot.json` 不是無用快取——decider 會讀它（見 snapshot 段）。正確規則不是「決策
永遠不讀 snapshot」，而是「讀 snapshot 的決策必須 fail-open、冪等，且不能因
stale/缺失的 snapshot 造成不可逆破壞」。現行四個讀取者都已依此規則核實。**教訓**：
餵給 decider 的投影是允許的，但前提是每個讀取者都能安全降級。

### 4. pr_state `record_verdict`（#2603）
`src/daemon/pr_state/mod.rs:917` `record_verdict` 是 pr_state 快取唯一的 verdict
寫入者。#2603（`4d13b2f3`）讓 handoff watchdog
（`src/daemon/handoff_timeout_watchdog.rs:44-61`）改讀 branch 的*快取* pr_state
快照——「independent of ci_watch … no GitHub call」。這之所以安全，正因為 pr_state
是投影：stale 讀取最壞只是延遲一次 handoff nudge；終局的 merge/CI 真相仍在 GitHub，
由 `gh_poll` 路徑校正。**教訓**：讀快取以省一次網路呼叫是可以的——只要該快取被明確
定義為投影、且讀取者容忍 stale。

---

## 寫入口紀律

solutions.md §3.5 提議：每個核心狀態檔只有其擁有模組能寫。這在今天是**部分強制**的：
- **已強制**：task event log——`tests/task_events_invariant.rs` 是活的反繞過測試。
- **尚未強制**：inbox 與 decisions 無對應的不變量測試；其 `enqueue` / `save` API 是
  預期入口，但沒有測試防止直接開檔。
- **已知例外**：`src/quickstart.rs:814` 直接寫 `fleet.yaml`（僅互動式覆寫分支）。

`agend-git` shim（§3.6）：獨立 binary，無法 link daemon internal API。它是
`binding.json` 與 protected refs 的**唯讀**消費者，不經 daemon 公開 API；共享邏輯靠
source include 或未來的 `agend-core` crate。「所有狀態都要走 domain API」這條規則明確
豁免 shim。

---

## 維護本文件

- 新增狀態、或替既有狀態新增讀取者？合併前補上/標註其列，附 `path:line`。
- 改動列出的入口？在同一個 PR 內同步更新此處行號。
- 發現兩個 store 各自聲稱是某狀態的真相源？那是 **bug**，不是文件缺口——停手回報。
