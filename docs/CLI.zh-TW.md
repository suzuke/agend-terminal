[English](CLI.md)

# CLI 指令參考

所有指令都透過 `src/main.rs` 中的 `clap`（enum `Commands`）派發。執行 `agend-terminal --help` 取得簡要說明，或對個別子指令執行 `<cmd> --help`。

資料根目錄由 `AGEND_HOME` 控制（預設為 `~/.agend`，為了向後相容會退回 `~/.agend-terminal`）。日誌遵循 `AGEND_LOG`（例如 `AGEND_LOG=agend_terminal=debug`）。

## 不帶參數執行

```
agend-terminal
```

印出 `--help` 後結束。若要使用互動式的多 pane TUI，請用 `agend-terminal app`。

## 指令

### `app`
啟動具備 in-process agent 管理功能的多分頁／pane TUI。這是 `0.3.0` 之後主要的使用者進入點。

```
agend-terminal app [--fleet <path>]
```
- `--fleet <path>` — 覆寫 fleet 檔案。預設：`$AGEND_HOME/fleet.yaml`。

按鍵繫結：見 `src/keybinds.rs`。前綴 `Ctrl+B`，接著 `c` 開新分頁、`n`／`p` 下一個／上一個、`"` / `%` 分割、`o` 切換焦點到下一個 pane、`x` 關閉、`z` 縮放、`[` 捲動模式、`:` 指令選擇盤、`d` 卸離、`?` 說明。大寫 `D` / `T` 開啟 decisions / task 浮層。`Space` 循環切換版面預設配置。

### `start`
使用 `fleet.yaml` 或明確指定的 `--agents` 啟動 daemon。

```
agend-terminal start [--foreground] [--fleet <path>]
agend-terminal start --agents <name:cmd>...        # ad-hoc, no fleet.yaml
```
- 預設為 detached service mode。`--foreground` 讓 stdio 保持連接並阻塞呼叫端 shell，適合除錯或交由 OS service manager 管理。
- `--fleet <path>` — 覆寫 fleet 檔案。預設：`$AGEND_HOME/fleet.yaml`。
- `--agents <NAME:CMD>...` — 以明確的 agent 規格啟動，而非使用 `fleet.yaml`。與 `--fleet` 互斥，並隱含 `--foreground`。涵蓋了先前的 `daemon` 子指令。

範例：`agend-terminal start --agents dev:claude reviewer:claude shell:/bin/bash`

以 fleet.yaml 啟動時：清除過時的 git worktree、若有設定 Telegram channel 則自動建立一個 `general` instance、初始化 Telegram，並依 `HealthTracker` 重新 spawn 任何已 crash 的 agent。

### `attach`
連接到單一 agent 的 PTY（終端檢視）。`Ctrl+B d` 卸離，daemon 會持續讓該 agent 運行。

```
agend-terminal attach [<name>]      # default: shell
```

### `inject`
將任意文字寫入 agent 的 PTY（若需要換行請附加 `\r`）。

```
agend-terminal inject <name> <text...>
```

### `list` / `ls` / `status`
列出運行中的 agent。單純的 `list` 透過 `runtime::list_agents_with_fallback` 查詢 daemon 的 in-memory registry；當 daemon API 暫時無回應時（例如重啟途中），會退回掃描 run-dir 的 `.port` 檔案，讓此指令仍能盡力回傳結果。傳入 `--detailed`／`-d`（或 `--json`，它隱含 detailed）可透過 daemon API 取得 state / health / backend 資訊（無 fallback——`--detailed` 需要 daemon 可連線）。

daemon 的 in-memory registry 是判斷「哪些 agent 存在」的權威真實來源；`.port` 檔案是 TUI-bridge 的 per-agent socket 產物，只會在離線 fallback 中出現。需要權威輸出的 operator 腳本應透過管線使用 `--json`，而非去解析單純的 `list`。

```
agend-terminal list [--detailed] [--json] [--legacy-json]
agend-terminal ls   [--detailed] [--json] [--legacy-json]   # alias
agend-terminal status                                       # alias of `list` (kept for back-compat; use --detailed for state/health/cmd)
```

在 Wave 1 的 CLI 整併之後，`status` 保留為 `list` 的 clap 別名；新程式碼應優先使用 `list --detailed`。

#### JSON 結構 (#938)

`list --json` 會輸出一個帶有 `mode` 判別欄位的封套，讓 operator 腳本能區分權威輸出與離線 fallback 輸出：

```json
{
  "mode": "live" | "fallback_daemon_stuck" | "fallback_daemon_absent",
  "agents": [ ... ]
}
```

- `live` — daemon API 有回應；`agents` 為完整的 registry 回應（`state` / `health` / `backend` 欄位皆已填入）。
- `fallback_daemon_stuck` — `.daemon` 的 PID 仍存活但 API 未回應（重啟途中、主迴圈卡住）。`agents` 攜帶來自 run-dir 掃描、僅含 `{name}` 的物件。可能是暫時性的；發出警報前請先重跑一次。若持續發生 → `agend-terminal admin cleanup-zombies`。
- `fallback_daemon_absent` — 沒有 `.daemon` 檔案，或 PID 已死。請以 `agend-terminal app` / `agend-terminal start` 啟動一個 daemon。

`--legacy-json` 可切回 #938 之前的結構（`{"agents": [...], ...}`，原樣透傳 API 回應，無 `mode` 欄位）。這是給硬編碼舊結構的 operator 解析器一個橫跨一個 release cycle 的棄用緩衝期；遷移後即移除。在沒有 `--json` 時此旗標無作用。

當 `mode != live` 時，單純（非 JSON）的 `list` 會在 stderr 加上一行提示，讓互動式執行此指令的 operator 不必重跑 `--json` 也能看到 fallback 狀態。

### `admin`

Operator 端的維護子指令。具破壞性的路徑會提示 `[y/N]`，除非提供 `--yes`（供腳本化的復原任務使用）。

```
agend-terminal admin cleanup-branches [--yes]
agend-terminal admin cleanup-zombies [--age <DURATION>] [--yes]
agend-terminal admin task-sweep-config [--repository <SLUG>] [--pause|--resume] [--dry-run|--no-dry-run] [--api-base-url <URL>]
agend-terminal admin gc-dry-run [--format human|json]
agend-terminal admin tokens [--action summary|by_instance] [--group-by instance|task] [--since <WINDOW>] [--instance <NAME>]
agend-terminal admin watchdog <snooze|resume|status|ack> [--duration <DURATION>]
agend-terminal admin config-set <KEY> <VALUE>
```

#### `admin cleanup-zombies` (#927)

終止仍持有 `<home>/run/<pid>/` 目錄的長時間運行 zombie daemon process。列出每個 mtime 早於 `--age`（預設 `14d`）的 `.daemon`，印出候選集合，然後在發出訊號前要求確認。

- `--age <DURATION>` — 接受 `14d`、`3h`、`30m` 等。比此年齡新的 daemon 會被略過。
- `--yes` — 非互動式；跳過 `[y/N]` 提示，並輸出一行「non-interactive destructive mode」稽核日誌。

終止語意是**刻意**設計成跨平台不對稱的（#936 的結案分析）：

- **Unix** — `SIGTERM` → 5 秒寬限 → `SIGKILL`。這 5 秒窗口涵蓋 daemon 自身的 `SHUTDOWN_GRACE=2s` agent 拆解，加上約 3 秒的 cleanup hook 與 log-worker flush。
- **Windows** — `TerminateProcess` 單階段。此 CLI 目前使用的 Win32 介面沒有 SIGTERM 的對等機制。未來的改進可能會加入 `CTRL_BREAK_EVENT` 路徑以達成兩階段的對等。

Exit code：

- `0` — 所有候選皆已回收（或未找到任何候選）。
- 非零 — 至少有一個 process 在寬限窗口內拒絕死亡（kernel 卡住／不可中斷睡眠／kernel module 持有）。Operator 必須手動調查。

當 `agend-terminal list` 偵測到卡住的 daemon 時，會在其 fallback 訊息中浮現 `cleanup-zombies` 提示。這個提示刻意保守——fallback 也可能在重啟途中暫時觸發，所以在呼叫 `cleanup-zombies` 前請等一個週期。

#### `admin cleanup-branches`

刪除其 PR 已被 merge 的本地分支（對 squash-merge 安全）。預設為 dry-run（僅預覽）；`--yes` 才會實際刪除。squash-merge 偵測的啟發式做法見 `docs/RCA-*` 筆記。

#### `admin task-sweep-config` (#2547)

檢視或設定 GitHub-PR 自動關閉的 sweep daemon（輪詢已 merge 的 PR，為 `Closes t-XXX-N` 標記發出 `Done` 事件）。從 `task_sweep_config` MCP 工具移至此處——這是 operator 專屬設定，20 天內零 agent 呼叫。不帶任何 flag 時，僅印出目前設定，不做變更。

- `--repository <owner/repo>` — 要 sweep 的 GitHub slug。空字串停用。
- `--pause` / `--resume` — 暫停／恢復 sweep tick（互斥）。
- `--dry-run` / `--no-dry-run` — 僅記錄決策不發事件，或實際發出（互斥）。
- `--api-base-url <URL>` — 自架 GitHub Enterprise 的 REST API base。空字串重設為 `https://api.github.com`。

#### `admin gc-dry-run` (#2548)

列出 Phase 4 GC 的候選項目（已釋放、過了 grace period、daemon 管理的 worktree）但不刪除。非破壞性。從 `gc_dry_run` MCP 工具移過來（20 天內零呼叫）。

- `--format human|json` — 輸出格式（預設 `human`）。

#### `admin tokens` (#2548)

依需求統計 token 用量與估計的 USD 成本，來源為 Claude Code／Codex 的 session transcript。從 `tokens` MCP 工具移過來（20 天內零呼叫）。成本為估計值；OpenCode／Kiro／Gemini 尚未涵蓋。

- `--action summary|by_instance` — `summary`（預設）為 fleet 總計＋per-instance 表格；`by_instance` 需搭配 `--instance`。
- `--group-by instance|task` — `instance`（預設）為 per-instance/per-model；`task` 會把每筆訊息時間對應到當時的 active task。
- `--since <WINDOW>` — 回溯窗口，例如 `24h`（預設）、`7d`、`90m`、`all`。
- `--instance <NAME>` — `--action by_instance` 必填；`summary` 時為可選過濾器。

#### `admin watchdog` (#2548)

Fleet idle watchdog 控制。從 `watchdog` MCP 工具移過來（20 天內零呼叫）。`ack` 會抑制 fleet 警示，直到偵測到 ack 之後的 agent 活動後自動解除。

- `<snooze|resume|status|ack>` — 位置參數，指定動作。
- `--duration <DURATION>` — snooze 時長，例如 `2h`、`30m`、`1h30m`。上限為 4h。預設 `1h`。

#### `admin config-set` (#2548)

設定一個執行期可變更的 daemon 設定 key。從 `config` MCP 工具的 `set` 動作移過來（20 天內零 MCP 呼叫）——agent 仍可透過 `config` MCP 工具讀取設定（`get`／`list`），但只有 operator 能變更它。目前可用的 key 列表見 `config` 工具的即時描述（`docs/MCP-TOOLS.zh-TW.md`）。

- `<KEY> <VALUE>` — 位置參數，key 與新值。

### `connect`
依外部要求在 daemon 註冊下執行 backend。`connect` 會註冊 instance、spawn backend、等待它退出後再解除註冊；它不是用來接管已在執行的 process。

```
agend-terminal connect <name> --backend <backend> [--working-dir <dir>] [-- <extra-args>...]
```
- `--backend` — `claude`、`kiro-cli`、`codex`、`opencode`、`antigravity-cli`（二進位檔 `agy`）或 `grok`。別名 `agy` / `antigravity` / `antigravity-cli` 解析到同一 backend；Gemini CLI 已退役。
- `--working-dir` — 預設為目前目錄。
- `--` 之後的額外參數會傳遞給 backend。

### `kill`
停止特定的 agent。daemon 持續運行。

```
agend-terminal kill <name>
```

### `stop`
停止 daemon（同時終止所有受管理的 agent）。

```
agend-terminal stop
```

### `agend-mcp-bridge`（獨立的二進位檔）

為目前的 instance 啟動 MCP stdio server。這是設計給 agent 的 backend 呼叫的，而非由人類直接執行——相關的 backend 設定會由 `mcp_config.rs` 自動寫入該 agent 的工作目錄。此 bridge 會把所有 tool 呼叫代理到運行中 daemon 的 TCP API；不存在本地 handler 的 fallback。

```
AGEND_INSTANCE_NAME=<name> agend-mcp-bridge
```

不帶 `AGEND_INSTANCE_NAME` 執行是允許的，但會進入 standalone 模式並發出警告。Sprint 56 Track I (#531) 已退役先前的 `agend-terminal mcp` 子指令；舊 Phase 1 RCA 仍可依[歷史紀錄還原規則](README.zh-TW.md#歷史紀錄)取得。

### `capture`
擷取 backend 輸出，或把 passive capture 提升到 state-replay corpus。

```
agend-terminal capture backend --backend <name> [--seconds <N>]    # default 15s
agend-terminal capture promote <capture.cap> <scenario> --scenario-kind <kind>
```

### `verify`
跨 backend 的完整端對端驗證（spawn 每個已設定的 backend，驗證 PTY + VTerm + MCP 接線）。

```
agend-terminal verify [--json] [--backend <name>] [--quick]
```

- `--quick` — 跳過 per-backend 測試與 daemon-spawning 測試；只執行 4 個 in-process 探測（attach、inbox、mcp framing、api）。在 30 秒內完成。涵蓋了先前的 `test` 子指令。

### `doctor`
健康檢查：home 目錄、`.env`、`fleet.yaml` 解析、活躍的 socket、backend 二進位檔是否存在及其版本（若安裝的 backend 版本與用於 state detection 的已校準版本不同，還會附上一則註記）。

```
agend-terminal doctor
```

### `quickstart`
互動式設定精靈：偵測已安裝的 backend、選擇性設定 Telegram、寫入 `fleet.yaml` + `.env`。會妥善處理既有設定而不覆蓋它。

```
agend-terminal quickstart [--unattended]
```

`--unattended` 不讀 stdin、不等待網路，供 CI 與腳本化安裝使用。

### `mode`
設定 operator availability 與選用的 sleep-mode delegation。此為 operator-only authority control。

```
agend-terminal mode <active|away|sleep> [--delegate <instance>] [--scope <op,...>]
```

### `service`
安裝、移除或查詢使用者層級的 OS service：

```
agend-terminal service <install|uninstall|status>
```

### `skills`
管理統一 skills 來源並安裝到各 backend 路徑：

```
agend-terminal skills <add|remove|list|update|install> ...
```

### `verify-push`
用實際 diff 驗證 semantic push claim：

```
agend-terminal verify-push --base <commit> [--head <commit>] (--claim <text>|--claim-from-stdin) [--json]
```

### `bugreport`
產生一個單一文字檔，內含診斷資訊、近期日誌與已遮蔽（redacted）的設定。輸出到 `AGEND_HOME/bugreports/`。

```
agend-terminal bugreport
```

### `completions`
將 shell 補全腳本印到 stdout。

```
agend-terminal completions <shell>
# shell ∈ bash | zsh | fish | elvish | powershell
```

---

## 環境變數

| 變數 | 用途 | 預設 |
|----------|---------|---------|
| `AGEND_HOME` | 資料／設定根目錄 | `~/.agend`（fallback：`~/.agend-terminal`） |
| `AGEND_LOG` | `tracing-subscriber` env filter | `agend_terminal=info`（見下方優先順序註記） |
| `AGEND_LOG_RETAIN_DAYS` | 每日輪替的保留數量 (#914) | `3` |
| `AGEND_LOG_MAX_BYTES` | 目錄大小的硬性上限 (#914)；支援 `K`／`M`／`G` 後綴 | `2G` |
| `AGEND_INSTANCE_NAME` | 向 MCP server 標識該 instance | *（由 spawner 設定）* |
| `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` | 開機時對過時 `run/<pid>/` 做 GC，清除超過 N 天的項目 (#933)。`0`／未設定時停用。具破壞性——請謹慎使用。 | *（停用）* |
| `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN` | 設為 `1` 時，開機 sweep 會記錄將被刪除的集合而非實際解除連結 (#933)。可與 `AGE_DAYS` 搭配，在啟用破壞性模式前進行安全試跑。 | *（停用）* |
| `AGEND_DAEMON_THREAD_DUMP_SECS` | 每 N 秒做一次週期性的 in-process thread state 傾印 (#941)。`0`／未設定時停用；任何正整數即啟用。輸出出現在 `daemon.log`。未設定時零開銷。 | *（停用）* |
| Telegram env | `TELEGRAM_BOT_TOKEN`、`TELEGRAM_CHAT_ID` | *（選填；從 `$AGEND_HOME` 下的 `.env` 讀取）* |

**`AGEND_LOG` 優先順序 (#927 PR-A)** — 當此環境變數有設定時，它會勝過程式碼內的預設（`agend_terminal=info`）。預設只在此變數未設定或為空時才套用。先前這被記載為「預設」，但實作偶爾會覆寫呼叫端設定的 env 值；現在優先順序已明確化並有測試覆蓋。

**破壞性環境變數的安全須知** — `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` 會直接刪除 `run/<pid>/` 目錄（不做封存）。在開啟它之前，請先以 `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN=1` 執行並用 `grep "boot-sweep" $AGEND_HOME/daemon.log` 來核對候選集合是否符合預期。

## 磁碟上的佈局

```
$AGEND_HOME/
    .env                          # optional; key=value, supports `export` prefix and quoted values
    fleet.yaml                    # agent definitions
    decisions/                    # decision JSON files
    tasks/                        # task board state
    inbox/<agent>.jsonl           # per-agent message queue
    metadata/                     # miscellaneous state
    downloads/                    # Telegram attachment downloads
    snapshot.json                 # fleet snapshot
    event-log.jsonl               # event log
    workspace/<agent>/            # default working dir when none set
    run/<daemon-pid>/
        .daemon                   # pid:start_time — identity for liveness checks (early)
        api.cookie                # 32-byte auth cookie for api.port (0600 on Unix)
        api.port                  # daemon control API TCP port (loopback)
        .ready                    # boot-completion sentinel (#922); daemon-init-complete signal
        <agent>.port              # per-agent TUI bridge TCP port (loopback, cookie-auth)
```

`.ready` 存在 ⟹ daemon 的 agent spawn 迴圈已完成，且 `list` / `/api/list` 會回傳本次開機的最終 agent 集合。單一訊號政策——未來的子階段 readiness 必須擴充 `.ready` 的內容，而非引入新檔案。完整的對照表與裸輪詢（bare-poll）的注意事項見 `CLAUDE.md` 的「Daemon lifecycle files (#922)」（從已 crash daemon 殘留的 `.ready` 需要結合 PID-liveness 檢查；`agend-terminal doctor` 是推薦的慣用做法）。

`$AGEND_HOME` 下的所有東西（包括 `fleet.yaml`、`session.json`）在變更期間都透過 `fs2::FileExt` 加鎖——對並行的 daemon／CLI 使用是安全的。

## Exit Code

- `0` — 成功。
- `1` — 輸入無效或指令失敗。
- 其他非零代碼來自 `inject` / `attach` 等指令中的子 process。

## 另見

- `docs/MCP-TOOLS.md` — 暴露給每個 agent 的 MCP tools。
- `docs/architecture.md` — daemon 設計與模組地圖。
- `CHANGELOG.md` — 版本歷史。
- `CONTRIBUTING.md` — 如何開發與測試。
