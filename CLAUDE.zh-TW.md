[English](CLAUDE.md)

# agend-terminal——Claude 工作備忘

## Rust 工作流程

提交任何 Rust 變更之前，**一律**執行：

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
```

CI 會在 `ci.yml` 的前兩個步驟執行這些命令。本機略過它們，代表下一次 push 會失敗，還得多跑一輪「修 fmt／修 clippy」。

### `git push` 前：執行完整的 CI-parity preflight

```bash
scripts/preflight.sh          # 完整 matrix；--quick 會略過 Windows 檢查
```

這是 CI `check` job 的一次性鏡像，也是避免 local-green → CI-red 往返的最佳方法。它會執行 `cargo fmt --check`、`cargo clippy --all-targets --features tray -- -D warnings`、`cargo test --tests --features tray`（unit + integration + invariant），以及關鍵的 **Windows cross-check**（`x86_64-pc-windows-msvc`）。Windows-only 程式碼（`libc::getppid`、`/bin/sh` spawn、`UnixStream`）在 Unix 開發機上可以順利編譯，卻會讓 CI 的 `windows-latest` runner 失敗。

Windows 步驟需要 MSVC C toolchain，因為 transitive C dependency（`ring`）在 macOS/Linux 上缺少它就無法 cross-compile。只需安裝一次：

```bash
cargo install cargo-xwin && rustup target add x86_64-pc-windows-msvc
```

若沒有 `cargo-xwin`，Windows 步驟會附提示並 SKIP（絕不 false-fail）；CI 的 `windows-latest` runner 仍是後援。Preflight 刻意*不是* git hook——完整 matrix 需要幾分鐘，請手動執行。

`.git/hooks/pre-commit` 的 pre-commit hook 會自動格式化 staged `.rs` 檔並重新 stage。它不會執行 clippy——clippy 對 pre-commit 路徑而言太慢。`git push` 前請自行執行 clippy。

Pre-push hook（`scripts/hooks/pre-push`）會執行**兩道 gate**：

1. **CI-parity**（#t-ci-parity-prepush-guard）——若 push range 觸及 `src/` / `tests/` / `Cargo.*` / `build.rs`，便執行 `scripts/preflight.sh --quick`（與 CI `check` 完全相同的命令：`cargo fmt --check`、`cargo clippy --all-targets --features tray -- -D warnings`、`cargo test --tests --features tray`），並在失敗時**阻擋 push**。這可防止一再發生的遺漏：agent 只執行 `cargo test --bin`——會略過 `tests/` integration target——便宣稱 CI-ready，接著被 CI 拒絕（#1734 stale-string integration test、#1735 block_on invariant）。Docs-only push 會略過 build。`--quick` 不執行 Windows cross-check（由 CI 的 `windows-latest` 後援）。
2. **claim-verify**——依實際 diff 驗證 `Claim:` trailer。

緊急情況可用 `git push --no-verify` 覆寫任一 gate（daemon-side gate 與 CI 仍會套用，所以 `--no-verify` 不是免死金牌）。

當 `src/` 檔案變更時，post-merge hook 會在背景觸發 `cargo build --release`，完成後顯示桌面通知。它不會自動重啟 daemon——由 operator 決定重啟時機。只有 operator 擁有的獨立 clone 才可用 `git config core.hooksPath /dev/null` 停用，且必須理解這會停用每一個 tracked hook。絕不要在 daemon-managed agent worktree 中變更 `core.hooksPath`；該路徑也承載 provenance 與 pre-push safety hook。

### 若尚未安裝 hook

Hook 是 per-clone 的。Fresh `git clone` 後請安裝：

```bash
scripts/install-hooks.sh   # idempotent——可安全重複執行
```

**與 fleet agent 共存**：daemon 會將每個 managed worktree 的 `core.hooksPath` 設為 `$AGEND_HOME/hooks`（`src/binding.rs::install_hooks`），並只在其中寫入 `prepare-commit-msg`——它*不會*清空目錄。因此 `install-hooks.sh` 會把 tracked `pre-push` *複製*到 `$AGEND_HOME/hooks/pre-push`，讓它共存並在 agent push 時觸發（因所有目前與未來的 worktree 共用該目錄，一份 copy 即可涵蓋全部）。Agent push 確實會執行 hook：active agent git guard 最終呼叫真正的 `git`，而 git 會遵守 `core.hooksPath`。此共存機制仰賴 daemon 不刪除 `$AGEND_HOME/hooks/pre-push`；若行為改變，請重新執行 `install-hooks.sh`。

## 測試忠實度：將 producer 的真實輸出餵給 consumer（#1493）

測試某種 wire format 的 consumer——例如 parser、matcher，或對另一個 function 建立的 string/struct 所做的 predicate——時，**請呼叫 producer 來建構 input，不要手寫資料形狀。** Review 檢查題：*「這份 fixture 是否與 production 實際送出的內容完全相同？」*

這是 #1483 的 false-green 類型：`notification_is_actionable_wake` 會比對 real `[AGEND-MSG-PENDING]` pointer 從未包含的 bracketed body marker，而測試手刻了那個從未發出的形狀——所以 matcher 是 dead code，測試卻仍通過（#1487 加入手刻字串缺少的 `now=` 欄位後又再度 drift）。修正模式：抽出單一 builder（例如 `build_pending_pointer`），讓 production 與測試都走它。測試 parser 的 *malformed/edge* handling 時仍可手刻 input——只要把 happy-path contract 固定在 real producer 上。

## sync→async bridge：禁止直接對 shared runtime 呼叫 `block_on`（#1476）

**硬性規則。** Sync→async bridge 絕不能直接對長生命週期的 shared runtime accessor（`telegram_runtime()`、`discord_runtime()`、`shared_ci_runtime()`……）呼叫 `block_on`。這些是 `current_thread` runtime，所以 caller 一旦從 tokio runtime 內到達 `<name>_runtime().block_on(...)`，就會以 *「Cannot start a runtime from within a runtime」* panic。

這正是 telegram 遇到的 bug（#1474；teloxide 0.17 讓路徑變得 reachable，daemon 下一次重啟便 panic），discord 也複製了同樣錯誤（#1476）。它會潛伏數週，因為只有 *caller* context 改變時才觸發，而非 bridge 本身改變時。

**必要模式**：每個會回傳值的 shared-runtime call 都要透過 `block_on_value` 類型的 helper。Helper 以 `tokio::runtime::Handle::try_current().is_ok()` 防護；若已在 runtime 內，便在 `std::thread::scope` thread 上以*全新* runtime 執行 future（絕不 nested）。參見 `src/channel/telegram/state.rs::block_on_value` 與 `src/channel/discord/adapter.rs::block_on_value`。

本機新建的 runtime（`let rt = Builder::…build()?; rt.block_on(…)`）不受此限——非 shared runtime 不會 nested。只有 shared-accessor 形式有危險。

`tests/block_on_runtime_guard_invariant.rs` 會強制此規則：任何不在 Handle-guarded / scoped-thread helper 內的 `<name>_runtime().block_on` 都會讓 CI 失敗。新增 channel/bridge 時，請新增 guarded helper，絕不要直接 `block_on`。

## Worktree 分支政策

Fleet agent 必須讓 daemon provision 並 bind worktree。絕不要直接執行 `git worktree add/remove/move`、絕不要切換 canonical repo，也絕不要在 shim deny 後使用 bypass。請透過 MCP lifecycle provision 專用的 non-protected branch：

```text
repo({action: "checkout", repository_path: "<canonical>",
      branch: "feat/short-name", from_ref: "origin/main",
      bind: true, task_id: "<task-id>"})
binding_state({instance: "<self>"})
```

只在 `binding_state` 回傳的 worktree 中工作。工作安全 push 或 handoff 後，以 `release_worktree({instance: "<self>"})` 釋放。Daemon 會拒絕 `main`/`master`、擁有 marker 與 binding transaction，並保留 canonical checkout 供 operator 與 tooling 使用。

## Daemon logging（#914）

Daemon tracing 透過 `tracing_appender::rolling` 寫入 `$AGEND_HOME/daemon.log.<YYYY-MM-DD>`，每日輪替。預設保留設定：

- `AGEND_LOG_RETAIN_DAYS=N`（預設 3）——`max_log_files` 上限
- `AGEND_LOG_MAX_BYTES=2G`（或純 integer / `K`/`M`/`G` 後綴）——目錄大小的硬性後援；總量超過時，每小時 tick 會修剪最舊檔案

Operator 的 tail target 維持 `$AGEND_HOME/daemon.log`——在 Unix 上它是指向最新 rotated file 的 symlink（同一個 hourly tick 會重新指向）；Windows operator 則使用 `glob daemon.log.*`（symlink support 需要 Developer Mode）。

**相較 pre-#914，已接受的 regression**：

- Log 不再包含 ANSI color code（`with_ansi(false)`）——operator script grep plain text 時不再需要 `sed 's/\x1b\[[0-9;]*m//g'`。
- systemd / `journalctl -u agend-terminal` 不再包含完整 daemon trace；請改用 `tail -F $AGEND_HOME/daemon.log`。（Unit template 的 stdout/stderr 現在只擷取 panic + migration-failure message。）
- macOS launchd plist 的 `StandardOutPath` / `StandardErrorPath` 會導向 `/dev/null`；同樣請改用上述 `tail`。

#914 binary 落地後第一次開機時，任何既有的 `daemon.log` 檔案（legacy unbounded）都會重新命名為 `daemon.log.migration.<unix-epoch>`，rolling appender 則接管新路徑。Migration 具 idempotency——修正後重新執行舊 binary 不會 double-rotate。

## Daemon lifecycle 檔案（#922）

啟動後的 daemon 會在 `$AGEND_HOME/run/<pid>/` 發布四個檔案：

| 檔案 | 寫入者 | 用途 | Tier（#879v4 spike 詞彙） |
|---|---|---|---|
| `.daemon` | `bootstrap::prepare` 前期 | 供 `find_active_run_dir` liveness check 使用的 PID identity | daemon-pid-published |
| `api.cookie` | `.daemon` 之後的 `auth_cookie::issue` | Daemon API socket 上 cookie handshake 使用的 32-byte shared secret | daemon-pid-published（auth-ready） |
| `api.port` | `bind_loopback` 之後的 `api::serve` thread | JSON control API 的 TCP loopback port | daemon-api-ready |
| `.ready` | agent spawn loop 完成後的 daemon main thread | 開機完成訊號：spawn loop 完成，本次開機的 agent count 已確定 | daemon-init-complete |

`.ready` 存在 ⟹ daemon 的 agent spawn loop 已完成，`agend-terminal list`（或 `/api/list`）會回傳本次開機的**最終** agent set。它**不**保證 `count == fleet.size`——daemon 採 log-and-continue policy，個別 agent 可失敗而不 abort loop，所以最終 count 可能較少。

**Lifecycle file 單一訊號政策**（來自 #922 dialectic 中 dev-2 的發現）：`.ready` 是**唯一**開機完成訊號。未來 sub-stage signal 必須擴充 `.ready` 的內容（帶有 per-subsystem ready flag 的 JSON payload），不得新增檔案。上方四檔表格就是完整 surface——不會有第五個檔案。

### Race condition 的區別

最近的 PR 已修復兩種 timing race——它們位於**不同** surface：

- **#908** 修復 per-agent `.port` file race：`spawn_and_register_agent` 現在會 block，直到 per-agent TUI thread 完成 `bind_loopback` + `write_port`。Function 回傳後，該 agent 的 `.port` 檔案一定已在磁碟上。
- **#922** 修復 API-level partial-results race：看到 `api.port` 的外部 prober 不能假設 registry 已填滿，因為 `api.port` 由 `api::serve` 在 agent spawn loop **之前**寫入（依 #906 reorder）。外部 prober 應等待 daemon-init-complete 訊號 `.ready`。

### Stale marker 安全性

Crash daemon 舊 `run/<pid>/` 目錄中殘留的 `.ready`，在 cleanup 前仍會存在磁碟上。在 crash-residue 情境中，只用 `until [ -f .ready ]` 輪詢並**不充分**——它可能因 stale marker 而回傳 true。正確的 operator idiom 會結合 `.ready` 與 PID liveness：

```bash
# Idiom A（建議）：使用 `agend-terminal doctor`
until agend-terminal doctor 2>&1 | grep -q "Active agents:"; do sleep 0.1; done

# Idiom B：glob `.ready` + 驗證 run_dir 的 `.daemon` PID 仍存活
until for d in "$AGEND_HOME"/run/*/; do
  [ -f "$d/.ready" ] && pid=$(cat "$d/.daemon" 2>/dev/null) && kill -0 "$pid" 2>/dev/null && exit 0
done; do sleep 0.1; done
```

`.github/workflows/ci.yml` 中的 CI smoke harness 會直接輪詢 `.ready`；它也在每次 loop iteration 檢查 `kill -0 $DAEMON_PID`（daemon-died gate）。由於 smoke daemon 的 PID 在 spawn 時已擷取，這提供與 Idiom B 同等的 liveness coverage。

### 與 `runtime::list_agents_with_fallback` 的互動（#910）

`runtime::list_agents_with_fallback`（#910 PR1 引入的 canonical「列舉 live agent」helper）不會等待 `.ready`。需要保證列舉完整的 caller 應先等待 `.ready`；開機視窗中的直接 caller 在 spawn loop 尚在進行時，合理地可能回傳 partial set。

## Bootstrap instrumentation（#945 Phase 0）

`bootstrap::prepare` 會為每個 instrumented step 發出一行 `bootstrap-step` tracing，帶有 `step=<name>` + `elapsed_ms=<n>` field。Operator 不需外部 instrumentation 就能擷取 cold-boot timing breakdown：

```bash
# 最近一次開機最慢的 5 個 bootstrap step
grep "bootstrap-step" $AGEND_HOME/daemon.log \
  | sort -t= -k3 -n -r \
  | head -5

# 單次開機的完整有序 timeline
grep "bootstrap-step" $AGEND_HOME/daemon.log
```

目前有 13 個以上 step 已 instrument（`load_fleet_yaml`、`stop_managed_agents`、`prune_stale_worktrees`、`bind_loopback`、`migrate_legacy_watch_filenames`、`start_telegram_init`……）。Phase 0 audit 發現 `telegram_init` 佔 cold-boot wall time 的 92.5%（約 6.6 秒中的 6.1 秒）；Phase 1 將該步驟移至背景（見下一節）。

## Telegram init 背景化（#945 Phase 1）

`bootstrap::telegram_init::init` 現在會立即回傳 `None`，並 spawn 一條 fire-and-forget thread 執行實際初始化（5–10 個依序執行的 `bot.create_forum_topic` HTTP call + fleet-binding resolve）。Cold-boot wall time 從約 6.6 秒降至約 0.5 秒。影響如下：

- **`api.cookie` + `api.port` 會在幾毫秒內落地。** 外部 prober / `agend-terminal list` 在看到 reachable daemon 前少 race 幾秒。
- **背景初始化完成前，`active_channel()` 會回傳 `None`。** Caller 都在 >10 秒 tick cadence 上，因此實務上看不出 channel 延遲。如果新增需要同步 channel 的 caller，請在 30 秒上限內以 poll loop 查詢 `active_channel()`。
- **Registry attachment 使用 `PENDING_REGISTRY` bridge。** `bootstrap::prepare`（caller）透過 `crate::agent::set_pending_registry` 發布 agent registry；背景 init thread 在 `register_active_channel` 後讀取它，再呼叫 `attach_registry`。以 100 ms cadence、上限 30 秒的 bounded poll 可涵蓋背景 thread 早於 caller 發布完成的罕見 race。
- **失敗透過 `tracing::error!` 顯示。** 不 panic，也不 abort boot。`topic_registry` orphan sweep 會在下次開機自我修復。Operator 可用 `tail -F daemon.log | grep telegram_init` 找出反覆發生的 failure。

## State detection 的 red anchor（#919 Phase A）

標記為 `HIGH_FP`（高 false-positive 風險——例如 `"Error"`、`"failed"` 等泛用字串）的 state-detection pattern，現在要求 captured byte stream 中距 match 200 bytes、30 秒內出現 red SGR escape（`\x1b[31m` family）。此 anchor 修復了一類 false transition：backend 從 user prompt echo `Error: ...` 字串（無紅色）後，daemon 將 agent 分類為 failed。

`Backend::has_red_anchor()`（`src/backend.rs`）會宣告每個 backend 是否在真實 error 上可靠發出 red SGR。若為 `false`，HIGH_FP gate 會 **fail open**（pattern 本身即可觸發 transition），以免沒有一致 color signal 的 backend 靜默失效。

Telemetry gate（Phase B，另有獨立 gate）：Phase A 上線時即會輸出 FP-rate sample；待 operator telemetry 證實此 gate 利大於弊後，Phase B 才會加嚴 enforcement。在此之前，缺少 red anchor 的 `HIGH_FP` match 會記錄 debug line，指出 pattern + missing-anchor reason，可供 fixture collection 使用。

## Operator 診斷方法

下列是一組整合過的 `grep` 方法，用於最常見的「daemon 健康嗎？」／「時間花在哪裡？」問題。每項都可獨立複製、貼上、執行。

```bash
# Zombie debugging——#932 已透過 #941 observability 關閉；
# 驗證 zombie 是否仍連到 stale $AGEND_HOME
grep "shutting down (signal received)" $AGEND_HOME/daemon.log
cat /proc/<zombie-pid>/environ | tr '\0' '\n' | grep AGEND_HOME

# Bootstrap timing——最近一次開機最慢的 5 個 step（#945 Phase 0）
grep "bootstrap-step" $AGEND_HOME/daemon.log \
  | sort -t= -k3 -n -r \
  | head -5

# Live thread state dump——daemon 看似 wedged 時很有用（#941）
AGEND_DAEMON_THREAD_DUMP_SECS=60 ./agend-terminal start
# 後續 dump 每 60 秒出現在 daemon.log，包含
# `thread-dump` line + per-thread state summary。

# CI-watch correlation——尋找指定 branch 的每則 notification（#946）
grep '"correlation_id":"owner/repo@branch"' $AGEND_HOME/inbox/*.jsonl

# Dispatch-idle correlation fallback（#947）——尋找 upstream 沒有
# correlation_id 的 watchdog firing；合成 id 的格式為
# `disp-<unix_micros>-<seq>`
grep '"correlation_id":"disp-' $AGEND_HOME/inbox/*.jsonl | head -5

# 啟用 destructive mode 前，先 preview boot sweep dry-run（#933）
AGEND_DAEMON_BOOT_SWEEP_DRY_RUN=1 AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS=14 \
  ./agend-terminal start
grep "boot-sweep" $AGEND_HOME/daemon.log
```

## 透過非隱藏 workspace link 提供 agy fleet MCP（#1547）

agy（Antigravity CLI）會從 `<workspace>/.agents/mcp_config.json`（官方 Customization Roots）載入 project-scoped fleet MCP，該檔案由 `configure_agy`（`src/mcp_config.rs`）寫入。但 agy **會拒絕 path 中任何 ancestor 以 dot 開頭（隱藏）的 workspace folder**——`addWorkspaceFolder` 會記錄 `is hidden: ignore uri`（`root.go:132`），且完全不讀取 `.agents/`。每個 daemon workspace 都位於 `$AGEND_HOME` 下（通常是 `~/.agend`，或 legacy `~/.agend-terminal`；兩者都以 dot 開頭）→ hidden → 沒有 fleet `send`/`inbox`/`task` tool。

**機制（經 operator e2e 驗證）：** agy 依 **`$PWD` env var** 判定「hidden」，而非 `getcwd()`/realpath。因此 daemon 會以真正的隱藏 workspace 作為 CWD（已驗證的 allowed root）來 spawn agy，但讓 `$PWD` 指向同一目錄的**非隱藏 link**。Hidden check 會通過；project discovery 仍解析 realpath（透過 `.antigravitycli`），所以它是**同一個** antigravity project——不會重複——並會載入 `.agents/`。

`src/agy_workspace.rs` 擁有此 link：`ensure_link`（spawn）建立 `<base>/<instance>` → `$AGEND_HOME/workspace/<instance>`，並回傳 `agent::build_command` 設為 `$PWD` 的 path；`remove_link`（teardown）由 `agent_ops::cleanup_working_dir` 呼叫，只會移除 managed link，絕不移除真實 workspace。Base 預設為 `<user_home>/agend-ws`，可透過 fleet.yaml 的 `agy_workspace_link_base` 覆寫。**Unix：** symlink。**Windows：** directory **junction**（`junction` crate）——不是需要 Developer Mode / admin privilege 的 `symlink_dir`；junction 兩者皆不需要。

Recovery safety（M2）：agy 會將 MCP discovery cache 在 `~/.gemini/antigravity-cli/mcp/<server>/`，因此 `configure_agy` 每次（重新）設定時都會 bust 該 cache，而 Stage-2 respawn path 會重新執行 `configure()`——略過它的 respawn 會在沒有 fleet tool 的情況下回來（recovery 正是最需要這些 tool 的時候）。

## Release

符合 `v*` 的 tag 會觸發 `.github/workflows/release.yml`，建置 5 個 target（macOS x64/arm64、Linux x64/arm64、Windows x64），並上傳 tarball
+ `SHA256SUMS` 至 GitHub release。

Tag 前請確認 `main` 最新的 `ci.yml` run 為 green。
