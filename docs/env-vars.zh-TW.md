[English](env-vars.md)

# Environment Variables Reference — 環境變數參考

本文件分門別類列出 codebase 讀取的每一個 `AGEND_*` 環境變數，外加會被尊重的
外部／標準變數，以及僅供測試使用的 fixture。

每一條目都是透過閱讀**實際的讀取位置**（`std::env::var` /
`var_os` / `has_env`）及其預設值解析邏輯推導而來——並非從名稱去臆測。`file:line`
指向主要讀取位置；行號相對於 crate root，反映撰寫當下的 `origin/main`。

## 慣例

- **以存在判定（Presence-based）**的旗標：只要變數被*設定*就啟用
  （`var_os(name).is_some()` / `var(name).is_ok()`），值會被忽略。
- **以值判定（Value-based）**的旗標：只有當值符合特定字串時才啟用
  （通常剛好是 `"1"`）；任何其他值——包含空字串——都視為「關閉」。
- **Default** 描述變數**未設定**時的行為（`unwrap_or` 的 fallback 或「功能關閉」）。
  無法解析的數值通常會 fallback 到同一個預設值。
- 🔒 **Secret（機密）**——絕對不要記錄、列印、echo 或 commit 其值。這類變數不會
  顯示任何範例值。
- ⚠️ **Security-sensitive（影響安全）**——更動它會削弱某個強制執行的邊界，或是
  觸發破壞性／不可逆的行為。

---

## 1. Core / identity

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_HOME` | 覆寫核心的 agend home 目錄（state / runtime / `.env` 的根）。也會被 git shim、mcp-bridge、agent 和 claim_verifier 使用。 | 若 `~/.agend` 存在則用它（否則為了向後相容退回舊版 `~/.agend-terminal`）。在 git shim 中，未設定 → 為空 → shim 直接 exec 未修改的真實 git。 | 絕對目錄路徑。 | `src/main.rs:111`（主要）；另有 `src/bin/agend-git.rs:66`、`src/bin/agend-mcp-bridge.rs:486`、`src/agent/mod.rs:1963` | 面向 operator 的核心設定。測試中大量用來建立隔離的 home。 |
| `AGEND_INSTANCE_NAME` | agent 的身分名稱。Daemon 會把它注入每個 spawn 出來的 agent 的環境；之後讀回來，用來在跨 instance 訊息上蓋上「from」欄位，並授權 bind / CI 動作。 | 未設定／為空 → `None` = 匿名／standalone 模式；以身分為閘的 handler 會拒絕匿名呼叫者。沒有字面上的預設名稱。 | 字串，限制為 `[A-Za-z0-9_:-]`，且不可為空。 | `src/identity.rs:29`（正式讀取位置）；另有 `src/bin/agend-git.rs:107`、`src/mcp/handlers/ci/mod.rs:776` | ⚠️ Security-sensitive（控管 bind/CI 的授權）。列在 `SENSITIVE_ENV_KEYS` 中，因此 template 無法覆寫它。是否存在可區分「agent 呼叫者」與「operator shell」。 |

---

## 2. Channels & tokens

所有 bot token 都是**間接**讀取的：fleet.yaml 的 `bot_token_env` 欄位指定變數
名稱，接著再透過 `std::env::var(bot_token_env)` 取得其值。下列變數是這套間接
機制的**預設名稱**。

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_TELEGRAM_BOT_TOKEN` | 存放 Telegram bot token 的環境變數的預設名稱。 | 設定欄位預設指向名稱 `AGEND_TELEGRAM_BOT_TOKEN`；若讀取當下該變數未設定，則退回舊版 `AGEND_BOT_TOKEN`（並發出 deprecation 警告）。 | Telegram bot token 字串。 | `src/fleet/mod.rs:227`（預設名稱）；於 `src/channel/telegram/creds.rs:23` 讀取 | 🔒 Secret。面向 operator。 |
| `AGEND_DISCORD_BOT_TOKEN` | 存放 Discord bot token 的環境變數的預設名稱。 | token 變數未設定 → Discord channel 不啟用（走「無憑證」分支）。 | Discord bot token 字串。 | `src/fleet/mod.rs:230`（預設名稱）；於 `src/channel/telegram/creds.rs:23` 解參考讀取 | 🔒 Secret。面向 operator。與 telegram channel 共用同一套 token 間接機制。 |
| `AGEND_BOT_TOKEN` | **舊版／fallback** 的 Telegram bot token，只有在設定的 `bot_token_env` 變數未設定時才讀取；會發出 deprecation 警告，引導 operator 改用 `bot_token_env`。 | 兩者都未設定 → 出現「bot token env not set」錯誤；telegram verify 測試會被略過。 | Telegram bot token 字串。 | `src/channel/telegram/creds.rs:25`；`src/channel/telegram/bootstrap.rs:39` | 🔒 Secret。**已棄用（Deprecated）**——僅作為讀取時的 fallback。`quickstart` 現在會寫入正式的 `AGEND_TELEGRAM_BOT_TOKEN`（並在重新執行時把舊版那一行遷移掉）；建議在 fleet.yaml 改用 `bot_token_env`。 |
| `AGEND_TELEGRAM_GROUP_ID` | `quickstart` 讀取的 Telegram supergroup id（若有設定），用來在產生的 fleet.yaml 中填入 `group_id`，讓 onboarding 可以從環境預先帶入 channel 綁定。 | 未設定 → `quickstart` 不會填入 `group_id`（由 operator 在 fleet.yaml／透過 topic 綁定自行填寫）。 | Telegram chat/supergroup id 字串（例如 `-100…`）。 | `src/quickstart.rs:174` | 面向 operator，只在 `quickstart` onboarding 時讀取（不在 hot path 上）。非機密。 |

---

## 3. Supervision & restart

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_WRAPPED` | restart-supervisor 標記，由 `scripts/agend-wrapper.sh` 在每次 daemon 啟動前設定；作為一個訊號，表示有 supervisor 會在 `exit(42)` 時重新 spawn daemon，讓 `restart_daemon` 得以進行。 | 不存在 → 不提供任何 supervised 訊號；若沒有其他訊號存在，`is_restart_supervised()` 為 false，restart 會 fail closed（保守拒絕）。 | **以存在判定**（`var_os(...).is_some()`）；任何值，甚至空字串，都算數。 | `src/daemon/restart.rs:55`（`has_env`，第 62–63 行） | ⚠️ Security-relevant：在破壞性的 `restart_daemon` 路徑上是 fail-closed 的閘門。另見下方外部的 `XPC_SERVICE_NAME` / `INVOCATION_ID`，以及下方的正向 sentinel `AGEND_SUPERVISED` 與 `AGEND_RESTART_HANDOFF` / `AGEND_SUCCESSOR_HANDOFF` 各列。 |
| `AGEND_SUPERVISED` | 正向的 supervisor sentinel，由 `service install` 寫入產生的 launchd plist / systemd unit；`is_restart_supervised()` 接受它作為「有 supervisor 會在 `exit(42)` 時重新 spawn daemon」的證明。取代了那個會誤判的環境變數 `XPC_SERVICE_NAME`。 | 不存在 → 不提供任何 supervised 訊號。 | **以存在判定**（`has_env`）；template 會寫入 `=1`。 | `src/daemon/restart.rs`（`SUPERVISED_ENV`、`is_restart_supervised`） | #1812。⚠️ Security-relevant（與 `AGEND_WRAPPED` 相同的 fail-closed 閘門）。 |
| `AGEND_RESTART_HANDOFF` | #1814 自癒式 successor-handoff restart 路徑（spawn 一個 successor、用 health 閘門檢查、失敗時 abort 並保持原 process 存活）相對於舊版 `exit(42)` + 外部重新 spawn fallback 的開關。 | **未設定 → 開啟**（自我 respawn），自 Stage 4 起。`=0` → 走舊版 `exit(42)` 路徑（與 #1814 之前 byte-identical）。 | **預設開啟（DEFAULT ON）**：只有字面上的 `"0"` 才退出；未設定 / `"1"` / 其他任何值 ⇒ 自我 respawn。 | `src/daemon/restart.rs`（`self_respawn_enabled`、`RESTART_HANDOFF_ENV`） | #1814 Stage 4 把預設從 opt-in 翻轉為 opt-OUT（在 Stage 2 將 launchd `KeepAlive` 對齊到 `SuccessfulExit=false` 之後）。**#2098**：與此旗標無關，`restart_daemon` 在 `agend-terminal app`（TUI+daemon 合併）／任何非 `run_core` 擁有模式下都會 fail-closed——該 process 沒有 in-process 的 `RESTART_PENDING` 消費端，所以 in-process 的自我 respawn 會把控制平面弄壞。在那種情況下：請 quit 後重新啟動 app，或 SIGTERM + restart。由正向標記 `RUN_CORE_ACTIVE`（`src/daemon/mod.rs`）控管。 |
| `AGEND_SUCCESSOR_HANDOFF` | 內部的 handoff token（`<old_pid>:<token>`），由前任 process 設定在它 spawn 出來的 successor 上，讓 successor 走最小化的 pre-lock handoff 開機流程（繞過 singleton「another daemon is already running」防護，延後 flock 與 reconcile）。並非 operator 旋鈕。 | 未設定 → 正常開機（完整 `prepare`）。 | `<u32 pid>:<non-empty token>`；格式錯誤 → 忽略（正常開機）。 | `src/daemon/restart.rs`（`successor_handoff_marker`、`SUCCESSOR_HANDOFF_ENV`） | #1814——內部使用；只由 `spawn_successor_handoff` 設定。 |

---

## 4. Auto-recovery

各 Stage 閘門都是**以值判定**（必須等於 `"1"`）；關閉時它們會以
「shadow mode」運行（只做 telemetry／logging，不做實際動作）。另有一個 runtime-config
的總開關（`hang_auto_recovery_enabled`）也能啟用 Stage 1–3。

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_AUTO_RECOVERY_STAGE1` | Stage 1 閘門：對 hung 的 agent 的 PTY 寫入 ESC byte。 | 除非總開關開啟，否則不啟用（shadow mode）。 | `"1"` 啟用；否則關閉。 | `src/daemon/per_tick/recovery_dispatcher.rs:193` | Operator 旗標；會更動一個運行中的 PTY。 |
| `AGEND_AUTO_RECOVERY_STAGE2` | Stage 2 閘門：發出 `Stage2Restart` 事件（重啟該 agent）。 | 除非總開關開啟，否則不啟用（shadow mode）。 | `"1"` 啟用；否則關閉。 | `src/daemon/per_tick/recovery_dispatcher.rs:155` | Operator 旗標；會觸發 agent 重啟。 |
| `AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS` | Stage 2 的最大重啟嘗試次數。 | `3`（`STAGE2_MAX_RESTARTS_DEFAULT`）。 | `u32`。 | `src/daemon/per_tick/recovery_dispatcher.rs:161` | 對重啟迴圈的安全上限。 |
| `AGEND_AUTO_RECOVERY_STAGE3` | Stage 3 閘門：藉由寫入 `HealthState::Paused` 來升級處理。 | 除非總開關開啟，否則不啟用（shadow mode：只有 telegram + tracing）。 | `"1"` 啟用；否則關閉。 | `src/daemon/per_tick/recovery_dispatcher.rs:114` | Operator 升級閘門。 |
| `AGEND_PRODUCTIVE_GATE` | 啟用 F9 的「productive-silence」hang 偵測路徑（可把 agent 標記為 Hung）。關閉 → 只做 shadow telemetry。 | `false`（不啟用）。 | `"1"` 啟用；否則關閉。 | `src/health.rs:753` | Rollout 功能閘門。 |

---

## 5. Worktree & GC

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_WORKTREE_AUTO_CLEANUP` | 控管 runtime 的 worktree 自動清理掃描（移除已合併的 worktree、清掉孤兒分支）。 | **開啟**（`unwrap_or(true)`）。 | 除 `"0"` 以外的任何值 → 啟用；只有 `"0"` 才停用。 | `src/worktree_cleanup.rs:17` | opt-**out**。⚠️ Module doc 註解寫「`=1` opt-in」但程式碼是 opt-out——以程式碼為準。 |
| `AGEND_WORKTREE_ENFORCEMENT` | 在 messaging handler 中，決定 task target 是否必須先綁定到受管理的 worktree 才能投遞。 | `"warn"`（記錄警告但仍允許投遞）。 | `"off"`（略過）、`"enforce"`（拒絕並回 `worktree_not_managed`），其餘（含 `"warn"`）→ 警告後放行。 | `src/api/handlers/messaging.rs:282` | ⚠️ Security-sensitive（控管對未綁定 agent 的訊息傳遞）。是三態，不是 bool。 |
| `AGEND_WORKTREE_GC` | worktree GC 掃描的總閘門（把乾淨的孤兒 worktree 封存到 `.trash`，並清除舊的 trash）。 | **關閉**（no-op）。 | `"1"` 啟用；否則關閉。 | `src/daemon/retention/worktrees.rs:391` | ⚠️ 控管 worktree 刪除。嚴格比對 `=="1"`。 |
| `AGEND_WORKTREE_GC_TRASH_DAYS` | `.trash/worktrees/*` 的保留視窗（天數）；更舊的條目會在 GC 掃描時被清除。 | `7`。 | `u64` 天數；`0` = 同一次掃描就清除；不過濾正負值。 | `src/daemon/retention/worktrees.rs:49` | 調校旋鈕。 |
| `AGEND_WORKTREE_FORCE_RECLAIM_DAYS` | 強制回收（force-reclaim）後援機制的年齡上限（天數）：一個從未被釋放、且沒有 agent liveness、又比此值更舊的 lease，會被強制回收。 | `7`（`<=0` 時也是）。 | `i64` 天數，過濾為 `>0`。 | `src/worktree_pool.rs:534` | ⚠️ 控管破壞性的回收行為。 |

---

## 6. Git-shim & bypass

這些變數位於 `agend-git` shim 二進位檔（`src/bin/agend-git.rs`）中。三個
`AGEND_GIT_BYPASS*` 控制項是層層疊加的緊急覆寫——全部都是 ⚠️ security-sensitive。

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_GIT_BYPASS` | Layer-1 一次性覆寫：若有設定，`should_bypass()` 回傳 true 並 exec 真實 git，跳過所有強制執行。 | 不 bypass。 | **以存在判定**（`is_ok()`）；任何值，甚至空字串。 | `src/bin/agend-git.rs:178` | ⚠️ 緊急覆寫。Daemon 內部的呼叫者會設 `=1` 來跳過 shim。 |
| `AGEND_GIT_BYPASS_AGENT` | Layer-2 特定 agent 的豁免：當值等於目前的 `AGEND_INSTANCE_NAME` 時 bypass。 | 無 agent 豁免。 | Agent/instance 名稱字串。 | `src/bin/agend-git.rs:181` | ⚠️ 會與 `AGEND_INSTANCE_NAME` 做值比對。 |
| `AGEND_GIT_BYPASS_UNTIL` | Layer-3 有時限的豁免：在 now < 指定 epoch 之前 bypass。 | 無時間豁免。 | **Unix epoch 秒數**（`u64`，非 ISO）。 | `src/bin/agend-git.rs:188` | ⚠️ 過期／無法解析 → 不 bypass。 |
| `AGEND_GIT_SHIM_DEPTH` | 遞迴防護，會傳遞到 spawn 出來的 git；在 `MAX_SHIM_DEPTH = 3` 時 hard-fail。 | `0`。 | 非負 `u32`；無法解析 → 0。 | `src/bin/agend-git.rs:33`（讀取）；於 `:1279`、`:1310` 設定 | 內部 sentinel；正常情況下不由使用者設定。`>= 3` 時 exit 70（#1504）。 |
| `AGEND_REAL_GIT` | escape hatch，存放真實 git 二進位檔的路徑，讓 shim 能 exec git 而不遞迴。Daemon 在 agent spawn 時注入。 | Shim：未設定 → 退回字面 `"git"`，再走 PATH-exclude 解析。Daemon：只在尚未設定時注入。 | 指向 git 可執行檔的絕對路徑；只有檔案存在時才接受。 | `src/bin/agend-git.rs:1338`（讀取）；於 `src/agent/mod.rs:835` 注入 | ⚠️ Correctness-sensitive：錯誤／缺漏的值有遞迴 spawn 風暴的風險（#1504）。 |

---

## 7. Logging & diagnostics

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_LOG` | `EnvFilter` 指令，控制 CLI 與滾動式 daemon/app subscriber 的 log 詳盡程度。 | CLI → `agend_terminal=info`；滾動式 → 由呼叫端提供的預設 filter。 | `tracing_subscriber::EnvFilter` 語法，例如 `agend_terminal=debug`、`info,agend_terminal::daemon=trace`。 | `src/logging.rs:76`（CLI）、`:156`（滾動式） | 面向 operator。 |
| `AGEND_LOG_MAX_BYTES` | 目錄大小的後援上限：每小時的 handler 會修剪最舊的 `daemon.log.*`，直到整體佔用低於上限。 | `2 GiB`（`DEFAULT_MAX_BYTES`）。 | 純位元組數，或加 `K`/`M`/`G` 後綴（不分大小寫），例如 `2G`、`500M`。 | `src/daemon/per_tick/log_rotation.rs:43`；parser 於 `src/logging.rs:198` | 僅 daemon 的調校項。 |
| `AGEND_LOG_RETAIN_DAYS` | daily rolling appender 上的 `max_log_files`（保留的每日輪替檔數量）。 | `3`（`DEFAULT_RETAIN_DAYS`；`<=0` 時也是）。 | 正的 `usize`。 | `src/logging.rs:62` | 與 byte cap 互不相干。 |
| `AGEND_DAEMON_THREAD_DUMP_SECS` | 每個 tick 的 thread-dump 間隔（秒）；`N>=1` 會啟用定期 dump。 | `0` / 停用。 | `u64` 秒；`0` 停用。 | `src/sync_audit.rs`（`thread_dump_interval_secs`） | 透過 `OnceLock` 快取一次——要切換需重啟。同一個 accessor 同時餵給 handler 間隔與 `thread_dump_enabled` 閘門。 |
| `AGEND_DEBUG_PTY_READ` | 在 PTY 讀取迴圈中，啟用對讀取次數／位元組總量的詳盡 debug logging。僅供 debug 的接縫。 | 關閉。 | `"1"` 啟用；其他任何值（含 `0`）皆關閉。 | `src/agent/mod.rs` | 內部 debug 旗標。 |
| `AGEND_LOCK_AUDIT` | 在 **release** build 中啟用 lock-ordering 稽核（記錄 tier 違規，而非變成 no-op）。 | Release build → no-op；debug/test build 不論如何一律稽核。 | **以存在判定**（`is_err()` 檢查）。 | `src/sync_audit.rs:43` | 開發／診斷用；只影響 release build。 |
| `AGEND_TUI_SIZE_DEBUG` | #2057 instrument：在指定的 app 啟動里程碑記錄控制端 TTY 的 kernel winsize（用來追查 TUI 自身的 render 區域在哪裡縮小）。 | 關閉（不做 size tracing）。 | 以值判定：剛好 `"1"` 啟用；否則關閉。 | `src/app/mod.rs:270` | 內部診斷（僅 `app` 模式）。在啟動時讀取一次到 local，因此沒有每幀的 env 查詢成本。 |

---

## 8. Watchdog & recipients

> **Deprecation（watchdog 拓樸 → `fleet.yaml`）。** 下列五個 `AGEND_IDLE_WATCHDOG_*`
> / `AGEND_TASK_STALL_RECIPIENTS` / `AGEND_DECISION_TIMEOUT_RECIPIENT` 變數都是
> agent / recipient **名稱**——屬於 fleet *拓樸*，而非 env 調校。它們的歸屬地是
> `fleet.yaml` 頂層的 `watchdog:` 區塊（見 `docs/FEATURE-fleet.md`）。這些 env 變數
> 暫時保留作為**棄用的 fallback，僅維持一個過渡視窗**，讓既有設定還能運作；
> 解析優先序為 **fleet.yaml `watchdog:` 值 > env 變數（已棄用，警告一次）>
> 內建預設**。請把它們搬到 `fleet.yaml` 並移除 env 變數：
>
> ```yaml
> watchdog:
>   idle_watchdog_agent: dev          # AGEND_IDLE_WATCHDOG_AGENT (single-agent mode)
>   dev_recipient: lead               # AGEND_IDLE_WATCHDOG_DEV_RECIPIENT
>   fleet_recipient: lead             # AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT
>   task_stall_recipients: [general, lead]   # AGEND_TASK_STALL_RECIPIENTS
>   decision_timeout_recipient: general      # AGEND_DECISION_TIMEOUT_RECIPIENT
> ```

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_IDLE_WATCHDOG_AGENT` | **已棄用** → `watchdog.idle_watchdog_agent`。dev-vantage idle watchdog 的單一 agent 模式（只監看這一個 agent）。 | `"dev"`（僅作為載入失敗時的 fallback）。 | Agent 名稱；空白／純空格會被忽略。 | `src/fleet/watchdog.rs` | Fleet 設定優先；env 是棄用的 fallback。 |
| `AGEND_IDLE_WATCHDOG_DEV_RECIPIENT` | **已棄用** → `watchdog.dev_recipient`。dev-vantage idle 警示的收件者。 | `"lead"`。 | Recipient 名稱；空白／純空格會被忽略。 | `src/fleet/watchdog.rs` | Fleet 設定優先；棄用的 fallback。 |
| `AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT` | **已棄用** → `watchdog.fleet_recipient`。fleet-vantage idle 警示（「整個 fleet 都靜悄悄」）的收件者。 | `"lead"`。 | Recipient 名稱；空白／純空格會被忽略。 | `src/fleet/watchdog.rs` | Fleet 設定優先；棄用的 fallback。 |
| `AGEND_TASK_STALL_RECIPIENTS` | **已棄用** → `watchdog.task_stall_recipients`。task-stall 警告的收件者。 | `["general", "lead"]`。 | 以逗號分隔的名稱；每項會去除前後空白，空項會被過濾。 | `src/fleet/watchdog.rs` | Fleet 設定（清單）優先；棄用的 fallback。 |
| `AGEND_DECISION_TIMEOUT_RECIPIENT` | **已棄用** → `watchdog.decision_timeout_recipient`。decision-timeout 自動採預設（operator-proceed）發送時的收件者。 | `"general"`。 | 非空的 recipient 名稱；空白視為未設定。 | `src/fleet/watchdog.rs` | Fleet 設定優先；棄用的 fallback。 |
| `AGEND_WATCHDOG_DRY_RUN` | 讓每個 tick 的 watchdog 把分類後的 PTY 錯誤只記錄到 event log，而不去更動 agent 的 health 狀態。 | `false`（套用 health 更動）。 | `"1"`/`"true"`/`"TRUE"`/`"True"` → dry-run；否則關閉。 | `src/daemon/watchdog.rs:21` | Operator 安全開關。 |

---

## 9. MCP & tools

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_HOOK_STATE_POC` | Lifecycle-hook 狀態閘門（#1523 epic / #2016 推廣）。開啟時：(a) MCP-config writer 會把 hook 狀態回報器注入該 agent 的 per-workspace `.claude/settings`（尊重 scope；user-global 的 `~/.claude` 不動），且 (b) 對於**有 hook instrument 的（strong）backend**，一份*新鮮*的 hook 衍生 `AgentState` 會在 daemon 的每個 tick snapshot 中**被推升為權威**——勝過 screen heuristic。 | 關閉（不注入回報器；hook 狀態永不被推升；一切由 screen heuristic 驅動——byte-identical）。 | 以值判定：剛好 `"1"` 啟用；否則關閉。 | `src/mcp_config.rs:193`（注入）；`src/daemon/hook_shadow.rs:115`（`promotion_enabled`）、`:148`（`authoritative_state`）；`src/daemon/per_tick/snapshot.rs:51`（snapshot 採用它） | 內部功能閘門，預設關閉。**推升是 phased-v1、以 SNAPSHOT 為範圍**（#2014）：它驅動 snapshot 的消費端——`dispatch_idle`、pane-state badge、`agent_state_of`/`snapshot.json`（#1985 的介面）。一個過期/未知的 hook 視窗、旗標關閉、或非 hook backend ⇒ 回退到 heuristic（不變）。直接讀取 RAW screen heuristic 的每個 tick 決策者——supervisor、hang 偵測、recovery dispatcher、idle/anti-stall watchdog、`conflict_notify`、`query`/`list` API——在 v1 中**不**被推升（epic phase-2，soak 之後）。 |

---

## 10. Env-isolation & security

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_ENV_ISOLATION` | agent-backend env 隔離的閘門（#1440 分階段推廣）。 | 停用。 | `"1"` 啟用；否則關閉。 | `src/agent/mod.rs:179` | 預設關閉的功能旗標。開啟時，只有 allowlist 中的 env 會被轉發給 backend（見 [external env](#12-honored-external-env)）。 |
| `AGEND_ALLOWED_ROOTS` | `working_directory` 驗證時額外允許的根目錄（附加到 home、workspace、cwd 之後）。 | 無額外根目錄；只允許 home + workspace + cwd。 | OS 路徑分隔符的清單（`:` Unix、`;` Windows）；空段會被略過。 | `src/api/mod.rs:156` | ⚠️ 控管 agent 工作目錄的 path-traversal allowlist。 |
| `AGEND_BIND_STRICT_MODE` | 在 dispatch_hook 中：當 `source_repo` 解析為 stub（tier 4）且此值為 `"1"` 時，拒絕 stub fallback，強制在 fleet.yaml 中明確指定 `source_repo`。 | strict mode 關閉；允許 stub fallback。 | `"1"` 啟用；否則關閉。 | `src/mcp/handlers/dispatch_hook/mod.rs:343` | 正式環境（production）安全閘門。 |

---

## 11. State-detection, injection & timing/tuning

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_POINTER_ONLY_INJECT` | 開啟時，PTY inbox 注入會使用 header-only（「pointer」）格式，強迫 agent 呼叫 `inbox` 才能取得內文。 | `false`。 | `"1"` 啟用；否則關閉。 | `src/daemon_config.rs:27`（用來初始化 `DaemonConfig::default`）；於 `src/inbox/notify.rs:14` 消費 | env 只在 default 建構時讀取；runtime 值存在 `DaemonConfig` 中。 |
| `AGEND_CONTEXT_ALERT_PCT` | context-window 用量百分比，達到此值時每個 tick 的 context-alert watchdog 會發出通知（含遲滯與重新警示節奏）。 | `80.0`（`DEFAULT_ALERT_PCT`）。 | 浮點百分比；無法解析 → 預設值。 | `src/daemon/per_tick/context_alert.rs:36` | 調校旋鈕。面向 operator。 |
| `AGEND_CONTEXT_HANDOFF_PCT` | context-window 用量百分比，達到此值時 context-handoff watchdog 會向 agent 注入一個 `SESSION-HANDOFF.md` 請求。 | `85.0`（`DEFAULT_HANDOFF_PCT`）。 | 浮點百分比；無法解析 → 預設值。 | `src/daemon/per_tick/context_handoff.rs:51` | 調校旋鈕。應高於 alert pct。 |
| `AGEND_CONTEXT_HANDOFF_ESCALATE_PCT` | 更高的 context-window 百分比，達到此值時 handoff watchdog 會升級通報給 operator。 | `92.0`（`DEFAULT_ESCALATE_PCT`）。 | 浮點百分比；無法解析 → 預設值。 | `src/daemon/per_tick/context_handoff.rs:58` | 調校旋鈕。應高於 handoff pct。 |
| `AGEND_LOW_DISK_THRESHOLD` | 可用空間下限（位元組）；inbox 寫入時，可用空間低於此值就視為「low disk」。 | `1 GiB`（`1024³`，`DEFAULT_LOW_DISK_FLOOR_BYTES`）。 | `u64` 位元組；無法解析 → 預設值。 | `src/inbox/disk.rs:13` | 調校旋鈕。純位元組數（不接受 `K`/`M`/`G` 後綴，與 `AGEND_LOG_MAX_BYTES` 不同）。 |

---

## 12. Daemon lifecycle / retention / capture

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` | 啟用開機時的破壞性 zombie-daemon 掃描，並設定年齡門檻（天數）；比 N 更舊的候選會被 kill。 | 僅 telemetry（不 kill）；門檻常數 `DEFAULT_AGE_DAYS = 14`。 | 正整數 `>=1` 天；格式錯誤 → 警告 + 視為未設定。 | `src/daemon/boot_sweep.rs:36` | ⚠️ **破壞性**（對 zombie daemon 送 SIGTERM/SIGKILL）。設定一個有效值就會切換到破壞性模式。 |
| `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN` | 次級閘門：當值為 `"1"` 且有設定 age-days 時，把破壞性掃描降級為只記錄 log。 | 非 dry-run（若有設定 age-days 則為破壞性）。 | `"1"` 啟用 dry-run；否則關閉。 | `src/daemon/boot_sweep.rs:40` | 對掃描的安全覆寫。 |
| `AGEND_RETENTION_CUTOVER` | **pending-dispatch** retention 掃描（刪除超過 14 天的 dispatch sidecar）的 kill-switch。 | 除非 `=="0"` 否則**開啟**。 | `"0"` 停用；未設定 / 其他任何值 → 啟用。 | `src/daemon/retention/mod.rs:41` | opt-**out**。#env-cleanup：已解耦——這現在僅針對 pending-dispatch（decisions 已移到下方它自己的旗標）。`=="1"` 仍**同時**啟用 decisions 掃描，作為舊版 fallback（已棄用）。 |
| `AGEND_RETENTION_DECISIONS_CUTOVER` | **decisions** retention 掃描（封存已過期的 decision）的 opt-in 閘門。 | **關閉**。 | `"1"` 啟用；否則關閉。 | `src/daemon/retention/decisions.rs`（`decisions_cutover_enabled`） | opt-**in**。#env-cleanup 解耦：獨立旗標，讓「pending-OFF + decisions-ON」這個組合可達。舊版 `AGEND_RETENTION_CUTOVER=1` 仍會啟用它（棄用過渡視窗）。 |
| `AGEND_FLEET_NO_AUTO_MIGRATE` | 停用載入 `fleet.yaml` 時對缺漏 instance ID 的自動回填／遷移。 | 執行自動遷移（回填 ID 並重寫 fleet.yaml）。 | `"1"` 略過遷移；否則關閉。 | `src/fleet/mod.rs:544` | 讓你 opt out 自動重寫。 |
| `AGEND_CAPTURE_FIXTURES` | 啟用 PTY-capture fixture sink：原始 PTY bytes 會寫入 `$AGEND_HOME/captures/<agent>/`。開機路徑會發出隱私警告。 | `NoOpCapture`（不擷取，零開銷）。 | `"1"` 啟用；否則關閉。 | `src/capture.rs:56`；`src/bootstrap/mod.rs:224` | ⚠️ fixture 擷取工具，在真實開機路徑上可讀。擷取到的 bytes 可能含有**機密／prompt**——commit 前請先檢視。另見 [test-only](#14-test-only-fixtures)。 |

---

## 13. Pending / in-flight（尚未進 `main`）

這些已設計好但尚未合併。在此記錄供前瞻參考；待其 PR 落地後再對照程式碼驗證。

_目前沒有。_（先前列出的 `AGEND_SUPERVISED`（#1812）以及
`AGEND_RESTART_HANDOFF` / `AGEND_SUCCESSOR_HANDOFF`（#1814）皆已合併——見
[§3. Supervision & restart](#3-supervision--restart) 中它們的正式條目。）

---

## 14. Test-only fixtures & seams

⚠️ **請勿在正式環境設定這些。** 它們是測試 harness 的慣例／fixture，或是
**僅供測試的接縫（seam）**——其 env 讀取存在的唯一目的，是讓某個跨 process 的
整合測試（會把一個 agend 二進位檔當成子程序 spawn）能控制該子程序的時序——
正式環境一律使用固定的預設值。它們**不是**正式環境的可調項。

| Name | Purpose | Source | Notes |
|------|---------|--------|-------|
| `AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS` | **僅供測試的接縫。** 縮短 `agend-mcp-bridge` 的 `tools/list` retry-timeout 預算，讓跨 process 測試能快速看到 daemon-unreachable 錯誤。正式環境一律使用固定的 **30 s** 預設值。 | `src/bin/agend-mcp-bridge.rs:271`（讀取）；由 `tests/attached_path_mcp_invariants.rs` 設定 | 非正式環境可調項（#env-cleanup reclassify）。`u64` ms；格式錯誤 → 預設值。 |
| `AGEND_SPAWN_STAGGER_MS` | **僅供測試的接縫。** 在 spawn 出來的 daemon 中設定 multi-agent 交錯 spawn 的延遲，讓跨 process 測試取得一個確定性的 startup-race 視窗。正式環境一律使用固定的 **500 ms** 預設值。 | `src/daemon/mod.rs:1091`（讀取）；由 `tests/ready_marker_invariants.rs`、`tests/attached_path_mcp_invariants.rs` 設定 | 非正式環境可調項（#env-cleanup reclassify）。`u64` ms；無法解析 → 預設值。 |
| `AGEND_SELF_RESPAWN_SETTLE_SECS` | **僅供測試的接縫。** #1814 自我 respawn 在最後一次 recover-as-primary 重新檢查之前的沉澱（settle）視窗。正式環境一律使用固定的 **1 s** 預設值；它存在的唯一目的，是讓跨 process 的整合測試能確定性地拉寬這個視窗（好讓 successor 的死亡落在重新檢查之內）。 | `src/daemon/mod.rs`（`self_respawn_settle`） | 非正式環境可調項。`u64` 秒；無法解析 → 1s。由 `tests/self_respawn_handoff.rs` 拉寬。 |
| `AGEND_FORCE_SUCCESSOR_FAIL` | **僅供測試的接縫。** #1814：讓 spawn 出來的自我 respawn successor 在啟動時 crash（沒通過 Phase-1 閘門）。 | `src/daemon/mod.rs`（`run_successor_handoff`） | 驅動「successor 失敗 → 前任保持存活（ok:false）」的整合測試。 |
| `AGEND_FORCE_SUCCESSOR_FAIL_AFTER_CONTROL_READY` | **僅供測試的接縫。** #1814：successor 通過 Phase-1（回答 STATUS）後，在 flock 之前死亡——驗證前任的 commit→exit liveness 重新檢查。 | `src/daemon/mod.rs`（`run_core` handoff 分支） | 驅動 FIX2 的 abort-stay-alive 整合測試。 |
| `AGEND_FORCE_SUCCESSOR_FAIL_DURING_TEARDOWN` | **僅供測試的接縫。** #1814：successor 撐過 Phase-1 + loop-break 重新檢查，接著在前任的 teardown 視窗期間死亡——驗證最後的 recover-as-primary 閘門。 | `src/daemon/mod.rs`（`run_core` handoff 分支） | 驅動 recover-as-primary 整合測試（搭配 `AGEND_SELF_RESPAWN_SETTLE_SECS`）。 |

---

## 15. Honored external env

codebase 實際會讀取的標準／第三方變數（僅確認過讀取位置的）。當
`AGEND_ENV_ISOLATION=1` 時，會有一份更廣的 locale / proxy / platform 變數
**allowlist** 被轉發給 spawn 出來的 backend（透過 `std::env::vars()` 批次傳遞，
而非逐一讀取）。

### Directly read

| Name | Purpose | Default (unset) | Source | Notes |
|------|---------|-----------------|--------|-------|
| `GITHUB_TOKEN` | GitHub API 認證；位於 token-discovery 鏈最前端，在 `gh auth token` 之前；CI/PR 輪詢用的 `Bearer` header。 | 退回 `gh` CLI，否則未認證（60/hr）。 | `src/github_token.rs:166`；`src/daemon/ci_watch/provider.rs:745` | 🔒 Secret。（`GH_TOKEN` **不**被尊重——只有 `GITHUB_TOKEN`。） |
| `GITLAB_TOKEN` | GitLab API 認證（`PRIVATE-TOKEN` header）。 | 退回 `~/.config/glab-cli/config.yml`。 | `src/daemon/ci_watch/provider.rs:776` | 🔒 Secret。 |
| `BITBUCKET_TOKEN` | Bitbucket 認證（`user:app_password`）。 | 退回 `~/.config/bb/config`。 | `src/daemon/ci_watch/provider.rs:1001` | 🔒 Secret。 |
| `HOME` | Home 目錄：`~` 展開、XDG fallback base、service unit 路徑、token 的 CLI-config fallback、backend session 目錄。 | service/`~` 路徑會 hard error；其他地方則略過。 | `src/service/macos.rs:17`；`src/connect.rs:62`；`src/agent/mod.rs:630` | 以 Unix 為主。 |
| `XDG_CONFIG_HOME` | systemd user unit 路徑的 base 目錄。 | `$HOME/.config`。 | `src/service/linux.rs:17` | Linux。 |
| `XDG_DATA_HOME` | 解析 opencode 正式的 `auth.json`。 | `$HOME/.local/share`。 | `src/agent/mod.rs:627` | XDG 語意。 |
| `PATH` | 把 agend bin / shim 目錄前置；定位真實的 `git`/`gh`；foreign-repo 偵測。 | `unwrap_or_default()` / harness fallback `"/usr/bin:/bin:/usr/local/bin"`。 | `src/connect.rs:120`；`src/bin/agend-git.rs:1353` | 跨平台。 |
| `SHELL` | `Shell` backend / 終端 spawn 時啟動的命令。 | `crate::default_shell()`。 | `src/backend.rs:144`；`src/app/mod.rs:354` | Unix。 |
| `LANG` | 若未設定，daemon 會把 `LANG=en_US.UTF-8` 注入 spawn 出來的 agent env。 | 未設定時注入預設值；已設定則保持不動。 | `src/agent/mod.rs:762` | 跨平台。 |
| `TZ` | schedule 評估用的 IANA 時區（在 `iana-time-zone` 之前的第一個來源）。 | 平台 TZ，再來 `"UTC"`。 | `src/schedules.rs:33` | 跨平台。 |
| `COLORTERM` | 偵測 24-bit truecolor（`truecolor`/`24bit`）以供 render。 | `unwrap_or_default()` → 無 truecolor。 | `src/vterm.rs:71` | 跨平台。 |
| `TERMINAL` | tray「open terminal」動作偏好的終端模擬器。 | 先試 `x-terminal-emulator`，再走 fallback 鏈。 | `src/tray/terminal/linux.rs:23` | Linux。 |
| `USERNAME` | scheduled-task XML 用的 Windows 當前使用者識別碼（`DOMAIN\USER`）。 | `unwrap_or_default()` → 空。 | `src/service/windows.rs:22` | Windows。 |
| `USERDOMAIN` | 使用者識別碼的 Windows domain 前綴。 | 裸的 `USERNAME`。 | `src/service/windows.rs:23` | Windows。 |
| `XPC_SERVICE_NAME` | macOS launchd 的 supervisor 偵測訊號，控管 `restart_daemon`。 | 未偵測到（fail-closed）。 | `src/daemon/restart.rs:55`（`has_env`） | macOS。⚠️ 已知的誤判來源——見 #1812 與 `AGEND_SUPERVISED`（Pending）。 |
| `INVOCATION_ID` | systemd 的 supervisor 偵測訊號，控管 `restart_daemon`。 | 未偵測到（fail-closed）。 | `src/daemon/restart.rs:55`（`has_env`） | Linux。 |
| `GIT_DIR` / `GIT_COMMON_DIR` / `GIT_WORK_TREE` | 只要存在就讓 git shim fail-closed（略過 foreign-repo 保護），因為它們會獨立於 cwd 重新指向 git。 | 正常的 `.git` 探索。 | `src/bin/agend-git.rs:443`–`445`（`var_os`，僅判定存在） | 跨平台。 |

### Forwarded to backends when `AGEND_ENV_ISOLATION=1` (allowlist passthrough)

與 `BASE_ENV_ALLOWLIST` 比對，存在的就注入（不存在的 key 就不轉發）。透過
`std::env::vars()` 在 `src/agent/mod.rs:716` 批次讀取；allowlist 在
`src/agent/mod.rs:124`。

- **Locale / session：** `USER`、`LOGNAME`、`LANGUAGE`、`LC_ALL`、`LC_CTYPE`、`LC_MESSAGES`、`SSH_AUTH_SOCK`、`XDG_CACHE_HOME`、`XDG_RUNTIME_DIR`、`TMPDIR`、`TMP`、`TEMP`
- **Proxy：** `http_proxy`、`https_proxy`、`all_proxy`、`no_proxy`（+ 大寫變體）
- **Windows platform：** `SYSTEMROOT`、`SystemDrive`、`windir`、`PATHEXT`、`COMSPEC`、`USERPROFILE`、`HOMEDRIVE`、`HOMEPATH`、`APPDATA`、`LOCALAPPDATA`、`ProgramData`、`ProgramFiles`、`ProgramFiles(x86)`、`NUMBER_OF_PROCESSORS`、`PROCESSOR_ARCHITECTURE`
- **Backend credentials**（🔒 轉發給偵測到的 backend 的子程序；key 在 `src/backend.rs:68`）：`ANTHROPIC_API_KEY`、`ANTHROPIC_AUTH_TOKEN`、`CLAUDE_CODE_OAUTH_TOKEN`、`OPENAI_API_KEY`、`GEMINI_API_KEY`、`GOOGLE_API_KEY`、`GOOGLE_APPLICATION_CREDENTIALS`、`KIRO_API_KEY`、`OPENCODE_CONFIG`、`OPENCODE_API_KEY`

### Searched but NOT read in `src/`

`GH_TOKEN`、`RUST_LOG`（由 `tracing-subscriber` 內部消費，沒有明確讀取）、`RUST_BACKTRACE`、`NO_COLOR`、`COLUMNS`/`LINES`（尺寸來自 PTY）、`TERM`（只寫入，從不讀取），以及 `XDG_RUNTIME_DIR`/`TMPDIR`/`USER` 作為直接讀取（僅 allowlist passthrough）。

---

## 16. Appendix：不是實際 env 變數的 `AGEND_*` 識別字

`grep -rhoE 'AGEND_[A-Z0-9_]+' src/` 會撈出一些**並非** runtime 環境變數的識別字，
因此本參考刻意把它們從上面的表格中排除。在此列出，讓盤點得以可證地完整（每一個
grep 命中都有交代）。

**已降級為固定 const（`#env-cleanup`，single-user-dev YAGNI）。** 曾經可由 env
覆寫，現在硬寫死；名稱僅存活於解釋性的程式碼註解中，**沒有 `env::var` 讀取**：

- `AGEND_API_CALL_TIMEOUT_SECS`（`src/api/mod.rs:880`）——現為固定 30 s。
- `AGEND_API_MAX_CONNS`（`src/api/mod.rs:270`）——現為固定 const。
- `AGEND_DRAFT_ESCAPE_SECS`（`src/notification_queue.rs:103`）。
- `AGEND_FRAME_LIMIT`（`src/framing.rs:14`）。
- `AGEND_OSCILLATION_GUARD_WINDOW_SECS`（`src/state/mod.rs:453`）。
- `AGEND_PANE_INPUT_THRESHOLD_SECS`（`src/daemon/supervisor.rs:431`）。
- `AGEND_PR_STATE_REPLAY_AGE_HOURS`（`src/daemon/pr_state/mod.rs:651`）。
- `AGEND_WORKTREE_FORCE_RECLAIM_BOOT_GRACE_SECS`（`src/worktree_pool.rs:631`）。

**字串常數／標記（從來不是 env 變數）：**

- `AGEND_BLOCK_START` / `AGEND_BLOCK_END`（`src/instructions.rs:104`–105）——即
  `<!-- agend:start -->` / `<!-- agend:end -->` 指令區塊標記。
- `AGEND_GITIGNORE`（`src/instructions.rs:53`）——一個 `.gitignore` 本文常數。

**僅存在於註解（未接線）：** `AGEND_RENDER_DEBUG`（`src/render/core_render.rs`）——
在 render 診斷註解中被提及；沒有 `env::var` 讀取。

**測試內部 fixture：** `AGEND_TEST_ENV_UTIL_FIXTURE`（`src/env_util.rs:88`）——
只有 `env_util` 自己的單元測試會用到的 key。