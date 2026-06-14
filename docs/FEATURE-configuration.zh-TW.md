[English](FEATURE-configuration.md)

# 設定層：`AGEND_HOME`、`fleet.yaml`、runtime config、MCP config

這份文件說明 AgEnD 的設定是怎麼分層的、誰能寫哪一層、
以及你應該在哪裡改哪一種設定。

## 使用情境

> **Target audience:** Operators — used through CLI or TUI.

操作者想知道目前的行為到底是從哪裡來的。`AGEND_HOME`、`.env`、`fleet.yaml` 與 `runtime-config.json` 分別控制不同層級，這個 section 的目的就是幫你判斷該改哪個檔案。

當 instance 需要長期策略變更時，通常應該改 `fleet.yaml`。如果只是暫時調整 live 數值，`runtime-config.json` 更適合，因為 daemon 每個 tick 都會重新載入。

如果 backend 的 project-local MCP config 看起來不對，應該把它視為派生檔，而不是主來源。通常重新產生會比手動直接改 backend artifact 更安全。

核心原則只有一句：

- **來源要單一，派生檔要可重建。**

也就是說：

- `fleet.yaml` 是 fleet 的主要人類可編輯來源。
- `runtime-config.json` 是可熱更新的執行期數值。
- 各 backend 的 `mcp.json` / `settings.local.json` 是派生設定，不是主來源。
- service manager artifact 也是派生設定，不是主來源。

## 設定層次總覽

| 層級 | 典型檔案 / env | 誰寫 | 作用時間 |
|---|---|---|---|
| 進程環境 | `AGEND_HOME`, `AGEND_POINTER_ONLY_INJECT`, `AGEND_CAPTURE_FIXTURES` | 操作員 / 啟動器 | 啟動時讀取 |
| Fleet 主設定 | `$AGEND_HOME/fleet.yaml` | 操作員 + daemon | 啟動 / 重新載入時 |
| 執行期設定 | `$AGEND_HOME/runtime-config.json` | MCP `config` 工具 | 每個 daemon tick |
| MCP 派生設定 | `.claude/settings.local.json`, `.kiro/settings/mcp.json`, `mcp-config.json` | daemon / 生成器 | backend 啟動前 |
| 服務管理器 artifact | plist / unit / task xml | `service install` | OS 登入時 |
| 診斷輸出 | `bugreport-*.txt`, `captures/*` | operator / capture 工具 | 需要時 |

## `AGEND_HOME`

`AGEND_HOME` 是最重要的根目錄。

### 解析規則

程式優先順序是：

1. 如果環境變數 `AGEND_HOME` 存在，就直接用它。
2. 否則回到使用者家目錄：
   - 優先 `~/.agend`
   - 相容舊路徑 `~/.agend-terminal`

### 為什麼這個根目錄重要

幾乎所有可持久化狀態都掛在這裡：

- `fleet.yaml`
- `runtime-config.json`
- `captures/`
- `service/`
- `skills/`
- `bin/`
- `protocol/`
- `workspace/`
- `worktrees/`

換句話說，搬家或備份時，先看 `AGEND_HOME`。

## `.env`

daemon 會從 `AGEND_HOME/.env` 讀取環境變數。

### 支援格式

- `KEY=value`
- `export KEY=value`
- single quoted / double quoted value

### 注意事項

- `#` 在 quoted value 中會被保留。
- unquoted 值的 inline comment 會被剝掉。
- `.env` 不是唯一來源；它只是啟動期環境補充。

### 適合放什麼

- bot token 的 env 名稱
- backend API key 的 env 名稱
- local-only feature flag

### 不適合放什麼

- 大量結構化 fleet 定義
- runtime 可調 threshold
- 使用者 global CLI 設定

## `fleet.yaml`

`fleet.yaml` 是 fleet 的主設定檔。

### 預設位置

```text
$AGEND_HOME/fleet.yaml
```

### 你會在這裡設定什麼

這個檔案描述每個 instance 的來源、角色與啟動方式。
常見欄位包含：

- `backend`
- `working_directory`
- `role`
- `instructions`
- `source_repo`
- `repo`
- `github_login`
- `args`
- `model`
- `env`
- `ready_pattern`
- `command`
- `worktree`
- `skills`
- `topic_binding_mode`
- `topic_id`
- `id`

### 哪些欄位是 daemon-managed

目前明確歸為 daemon-managed 的欄位有：

- `id`
- `topic_id`
- `git_branch`
- `source_repo`

意思是：

- daemon 會覆寫它們。
- operator 不應把它們當成永久手改欄位。
- 若手改與 daemon 內容衝突，merge 會以 daemon 為準，或直接報 conflict。

### 哪些欄位是 operator hand-edit

常見 operator 管欄位：

- `backend`
- `working_directory`
- `role`
- `instructions`
- `repo`
- `github_login`
- `args`
- `model`
- `env`
- `ready_pattern`
- `command`
- `worktree`
- `skills`
- `topic_binding_mode`

### `None` 與 `Some(empty)` 的差別

這個差異很重要，尤其在 `args`、`env`、`skills` 這類欄位。

- `None`：不要覆寫預設。
- `Some(vec![])` / 空集合：明確 opt-out。

例子：

- `args: null` → 使用 backend 預設參數。
- `args: []` → 明確要求空參數列。
- `skills: null` → 安裝全部 skills。
- `skills: []` → 明確不安裝任何 skills。

### `skills`

`skills` 是 per-instance allowlist。

語意如下：

- `null`：安裝所有共享 skills。
- `[]`：不安裝任何 skills。
- `["foo", "bar"]`：只安裝指定的 skills。

這個欄位會影響 backend 工作目錄底下的技能安裝內容。

### `topic_binding_mode`

這個欄位控制 Telegram topic 是否在 spawn 時就建立。

- `auto` / `null`：目前預設行為。
- `skip`：永遠不建立 topic。
- `deferred`：spawn 時不建，之後可補綁。

### `repo` / `source_repo`

這兩個欄位都跟來源 repo 有關，但語意不同：

- `source_repo`：daemon 會把它當成綁定來源的一部分。
- `repo`：GitHub `owner/name` 層級的覆寫。

如果你只是在做 operator hand-edit，避免把 daemon-managed 欄位當成永久真相。

### merge 行為

fleet merge 不是純覆蓋，而是有欄位分類。

- daemon-managed 欄位：daemon 值覆寫 operator 值。
- operator-hand-edit 欄位：
  - 既有欄位缺失 → 寫入 daemon 值
  - 既有欄位相同 → no-op
  - 既有欄位不同 → 產生 conflict
- daemon 未提供的欄位：保留 operator 原值

這樣做的目的，是避免 daemon 和 operator 互相把對方的資訊洗掉。

## `runtime-config.json`

這是執行期設定。

### 預設位置

```text
$AGEND_HOME/runtime-config.json
```

### 讀取方式

daemon 會在每個 tick 重新載入一次，所以它是 live tunable 的。

### 可調欄位

| key | 預設值 | 意義 |
|---|---|---|
| `dev_idle_threshold_secs` | `3600` | 單一 agent 的 idle 閾值 |
| `fleet_idle_threshold_secs` | `1800` | 整體 fleet 的 idle 閾值 |
| `hang_auto_recovery_enabled` | `false` | 是否啟用 hang auto-recovery |

### 如何修改

透過 MCP `config` 工具：

- `config get`
- `config set`
- `config list`

也就是說，這不是一個通常要手改的檔案。

### 失敗語意

- key 不存在 → error
- 整數 / 布林 parse 失敗 → error
- JSON 解析失敗 → 預設值

這意味著：runtime config 壞掉時，daemon 不會停；它會回到預設值。

### 什麼時候適合用它

- 調整 watchdog 閾值。
- 暫時拉高 fleet idle 閾值。
- 開關 hang auto-recovery shadow/active。

### 不適合放什麼

- fleet 結構
- agent 身分
- backend 路徑
- 任何需要審核的長期政策

## `DaemonConfig`：進程內旗標

`src/daemon_config.rs` 的設定是 process-wide，但不是持久化檔。

### 現有欄位

- `pointer_only_inject`

### 來源

- `AGEND_POINTER_ONLY_INJECT=1` 會開啟
- 沒有就預設 `false`

### 用途

這類設定是 daemon startup 時讀一次，
適合控制特定注入策略或暫時性實驗旗標。

### 注意事項

- 它不會寫回磁碟。
- 它不會自動跟 fleet.yaml 同步。
- 如果你要長期保存，應該把需求落到正式設定檔或 MCP config。

## MCP config：各 backend 的派生設定

`src/mcp_config.rs` 負責為 backend 生成對應的 MCP 設定檔。

### 設定範圍

這裡有一條硬規則：

- 寫入只能落在 `$AGEND_HOME` 或專案工作目錄。
- 不碰使用者全域的 CLI 設定目錄。

也就是說，不要讓程式去改 `~/.claude`、`~/.codex`、`~/.gemini` 之類的個人設定。

### 生成內容

產出的 MCP config 會帶上：

- `AGEND_HOME`
- 某些情況下的 `AGEND_INSTANCE_NAME`
- bridge binary 的路徑

### 常見輸出位置

視 backend 而定，常見包括：

- `.claude/settings.local.json`
- `mcp-config.json`
- `.kiro/settings/mcp.json`

其他 backend 也會有對應的 project-local 設定檔。

### 備援與錯誤處理

- JSON 破損時，會先備份成 `.corrupt.<timestamp>`。
- 然後從空物件重新開始。
- 這跟 `runtime-config.json` 的 silent default 不同；MCP config 會盡量保留一份備份。

### 為什麼是派生檔

MCP config 的真相其實是：

- `fleet.yaml` 裡的 instance 定義
- `AGEND_HOME`
- 當前 binary 路徑

MCP config 只是把這些來源轉成 backend 需要的格式。

## `service` artifact 也是派生設定

service 產物不是來源檔，而是由 install 命令根據目前 binary 與 `AGEND_HOME` render 出來的。

| 平台 | 產物 |
|---|---|
| macOS | plist |
| Linux | systemd user unit |
| Windows | Task Scheduler XML |

如果 binary moved / repathed，重跑 `service install` 才會更新。

## 設定錯誤時的典型反應

### `fleet.yaml` 壞掉

- `doctor` 會報 parse error。
- daemon 可能無法正常啟動。
- 先用 `bugreport` 取快照，再修檔。

### `runtime-config.json` 壞掉

- daemon 會回到 default values。
- 不一定會立刻看到 crash。
- 這類問題通常要看 log 或 `doctor` 的結果。

### `mcp` 設定檔壞掉

- 會備份舊檔。
- 重新產生新的派生檔。
- 如果 backend 行為怪異，先確認是不是 config 被重寫。

### `service` artifact 壞掉

- `service status` 可能顯示 `stopped`。
- 重新 `service install` 通常比手修 artifact 快。

## 建議的操作順序

### 新機器 / 新安裝

1. 設好 `AGEND_HOME` 或接受預設。
2. 準備 `.env`。
3. 編輯 `fleet.yaml`。
4. 跑 `service install`。
5. 跑 `doctor`。

### 想調整 idle / watchdog 閾值

1. 用 `config set` 改 `runtime-config.json`。
2. 等下一個 tick。
3. 用 `doctor` 或看 event / log 驗證。

### 想改某個 agent 的行為

1. 改 `fleet.yaml` 的 instance 欄位。
2. 若涉及 backend 設定，讓 MCP config 重新生成。
3. 必要時重裝 service。

### 想追問題

1. 先跑 `doctor`。
2. 再跑 `bugreport`。
3. 若是 backend 互動問題，再考慮 `capture`。

## 常見誤區

### 把 MCP config 當主設定

錯。它是派生檔，會被重寫。

### 把 runtime-config 當 fleet 配置

錯。它只管 live threshold 類的數值。

### 手改 daemon-managed 欄位後期待永久保留

錯。那些欄位會被 daemon 重寫。

### 把空集合和 `null` 當同一件事

錯。很多欄位的語意剛好相反。

## 對應原始碼

- `src/main.rs`：`AGEND_HOME`、CLI default path、`Doctor` / `Service`
- `src/fleet.rs`：`FleetConfig`、`InstanceYamlEntry`、欄位 merge 規則
- `src/runtime_config.rs`：runtime-config 讀寫與 tick reload
- `src/daemon_config.rs`：process-wide runtime flags
- `src/mcp_config.rs`：backend MCP config 生成
- `src/store.rs`：lock、atomic write、corrupt backup
- `src/service/*`：service manager artifact 生成
- `src/bugreport.rs`：diagnostic report 打包
- `src/capture.rs`：capture 與 promote 產物

## 實務建議

1. 長期政策放 `fleet.yaml`，不要塞進 runtime config。
2. 短期 tuning 放 `runtime-config.json`，不要硬改 source code。
3. 任何會被派生的設定，都要保留原始來源。
4. 看到疑似設定污染，先看 `bugreport`，再看派生檔是不是被重寫。
5. 記住一句話：**主來源可編輯，派生檔可重建。**