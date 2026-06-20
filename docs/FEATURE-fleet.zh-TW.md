[English](FEATURE-fleet.md)

# Fleet Management — 統一管理所有 Agent

## 使用情境

> **適用對象：** Operator——透過 CLI 或 TUI 使用。

**定義你的 agent 團隊。** 你需要一個 lead 負責任務拆解、一個 dev 負責實作、一個 reviewer 負責 code review——三者都在同一個 repo 上工作但使用獨立的 worktree。fleet.yaml 讓你在一個檔案中宣告這三個 agent，分別指定角色、backend 和工作目錄。

**管理共用設定。** 所有 agent 需要相同的環境變數和 ready pattern，但其中一個使用不同的 backend。`defaults` 區段處理共用設定，個別 instance 只覆蓋需要不同的部分。

**擴大團隊。** 專案成長後需要第二個 dev 或專屬的 tester。在 fleet.yaml 加幾行、重啟 daemon，新 agent 就會以正確的設定啟動——包括團隊歸屬、worktree 和 Telegram topic。

## 設計初衷

在沒有 fleet.yaml 之前，啟動多個 AI agent 需要為每一個分別開終端、設定環境
變數、指定工作目錄。Agent 之間無法協作，也沒有統一的生命週期管理。

fleet.yaml 解決了這個問題：用一個 YAML 檔描述所有 agent 的配置——使用哪個
backend、在哪個目錄工作、屬於哪個團隊、用什麼通訊頻道。`agend-terminal start`
讀取 fleet.yaml 後自動啟動所有 agent，daemon 負責監控健康狀態、自動重啟、
以及跨 agent 通訊。

---

## fleet.yaml 結構

fleet.yaml 位於 `$AGEND_HOME/fleet.yaml`（預設 `~/.agend-terminal/fleet.yaml`）。

### 完整範例

```yaml
# 預設配置（所有 instance 繼承）
defaults:
  backend: claude
  ready_pattern: "bypass permissions|❯"
  env:
    AGEND_PRODUCTIVE_GATE: "1"

# 通訊頻道
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456789
  mode: topic
  user_allowlist:
    - 12345

# 顯示時區（IANA 格式）
display_timezone: Asia/Taipei

# Agent 實例
instances:
  lead:
    role: "Team lead — task decomposition and dispatch"
    backend: claude
    model: opus
    working_directory: ~/Projects/my-app
    source_repo: ~/Projects/my-app
    worktree: false

  dev:
    role: "Primary developer"
    working_directory: ~/Projects/my-app
    source_repo: ~/Projects/my-app
    github_login: my-github-user

  reviewer:
    role: "Code reviewer"
    role_kind: reviewer   # 具型別的角色 → 裁切對外公告的 MCP 工具面（opt-in；詳見下方）
    backend: kiro-cli
    working_directory: ~/Projects/my-app
    source_repo: ~/Projects/my-app

# 團隊
teams:
  core:
    members: [lead, dev, reviewer]
    orchestrator: lead
    description: "Core development team"
    source_repo: ~/Projects/my-app
```

### 各區塊說明

#### `defaults` — 預設配置

所有 instance 會繼承 defaults 中的設定。Instance 可以覆蓋任何欄位。

| 欄位 | 型別 | 說明 |
|------|------|------|
| `backend` | string | Backend 名稱（claude / kiro-cli / codex / opencode / gemini / agy / shell） |
| `command` | string | 自訂執行命令（覆蓋 backend 預設命令） |
| `args` | [string] | CLI 參數列表 |
| `model` | string | 模型名稱（如 opus、sonnet） |
| `ready_pattern` | string | 正規表達式，用來判斷 agent 何時準備就緒 |
| `env` | map | 環境變數（key-value 對） |
| `cols` | int | 終端寬度（預設 200） |
| `rows` | int | 終端高度（預設 50） |

#### `instances` — Agent 實例

每個 key 是 agent 的名稱（必須符合 `[a-zA-Z0-9_-]`），value 是該 agent 的配置。

| 欄位 | 型別 | 說明 |
|------|------|------|
| `role` | string | Agent 的角色描述，自由文字（別名：`description`） |
| `role_kind` | enum | **選用、opt-in。** 具型別的角色選擇器，用來裁切此 agent 對外公告的 MCP 工具面（縱深防禦，#2300）。可為 `reviewer` / `planner` / `explorer`（讀取/回報類——移除 instance/worktree 生命週期與協調類工具）或 `orchestrator` / `implementer` / `utility` / `proxy`（完整工具面）。**未設定 → 全部工具**（不更動既有 fleet 行為）。與上方自由文字的 `role` 不同。strict 且 fail-closed：未知值會使 fleet 載入失敗，格式錯誤的 fleet 也絕不會悄悄放寬對外公告的工具面（#2367）。 |
| `backend` | string | 覆蓋 defaults 的 backend |
| `command` | string | 覆蓋 defaults 的命令 |
| `args` | [string] | 附加的 CLI 參數（與 defaults 合併） |
| `working_directory` | string | 工作目錄（支援 `~/` 展開）。若未設定，預設為 `$AGEND_HOME/workspace/<name>/` |
| `source_repo` | string | Git 倉庫路徑，用於自動建立 worktree。與 `working_directory` 分離，讓 worktree 可以放在不同位置 |
| `repo` | string | GitHub `owner/repo` 格式。用於 CI watch、PR 操作等。自動從 `source_repo` 的 git remote 推導，此欄位為手動覆蓋 |
| `worktree` | bool | `true`（預設）= 自動建立 git worktree；`false` = 不建立 |
| `git_branch` | string | 自訂 worktree 分支名稱（別名：`worktree_source`） |
| `model` | string | 模型覆蓋 |
| `env` | map | 環境變數（與 defaults 合併，instance 優先） |
| `cols` / `rows` | int | 終端尺寸覆蓋 |
| `ready_pattern` | string | 就緒判斷正規表達式覆蓋 |
| `display_name` | string | 在 UI 和 Telegram 中顯示的名稱 |
| `instructions` | string | 額外指令檔案路徑（相對於 fleet.yaml 所在目錄） |
| `github_login` | string | GitHub 使用者名稱，用於 task sweep 的作者驗證 |
| `skills` | [string] | 該 agent 可使用的 skills 白名單 |
| `topic_id` | int | Telegram topic ID（daemon 自動管理，通常不需手動設定） |
| `topic_binding_mode` | string | Topic 建立模式：`auto`（預設）/ `skip` / `deferred` |

#### `channel` — 通訊頻道

目前支援 Telegram 和 Discord 兩種頻道類型。

**Telegram：**

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN    # 環境變數名稱（非 token 明文）
  group_id: -100123456789           # 超級群組 ID
  mode: topic                       # topic（論壇模式）或 flat
  user_allowlist: [12345, 67890]    # 允許操作的 Telegram user ID
  fleet_binding:                    # 選填：agent-topic 綁定
    dev: 42
    reviewer: 43
```

**Discord：**

```yaml
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_TOKEN
  guild_id: "123456789"
```

`user_allowlist` 是安全機制——不在白名單中的 Telegram 使用者無法透過 bot
向 agent 發送指令。此欄位為必填。

#### `teams` — 團隊

將多個 agent 組成團隊，啟用跨 agent 協作（任務分配、code review dispatch 等）。

| 欄位 | 型別 | 說明 |
|------|------|------|
| `members` | [string] | 團隊成員的 instance 名稱 |
| `orchestrator` | string | 團隊的協調者（接收任務分配和進度回報） |
| `description` | string | 團隊描述 |
| `source_repo` | string | 團隊共用的 git 倉庫路徑 |

#### `display_timezone` — 顯示時區

設定 daemon 在人類可讀的時間戳中使用的時區。接受 IANA 時區名稱
（如 `Asia/Taipei`、`America/New_York`）。未設定時使用系統時區。

#### `templates` — 部署模板

定義可重複使用的 agent 配置模板，供 `fleet deployment deploy` 動態建立
instance 使用。

#### `watchdog` — Watchdog 拓撲

控制 idle watchdog 監看哪個 agent，以及由誰接收各種 watchdog／
anti-stall／decision-timeout 通知。這些是 agent／接收者**名稱**
（fleet 拓撲），所以放在這裡而非 env vars。每個欄位皆為
選填；省略整個區塊（或某個欄位）時會退回到舊版 `AGEND_*` env var
（已棄用），再退回到內建預設值。解析優先順序：

**`watchdog:` 值 > `AGEND_*` env var（已棄用，僅警告一次）> 內建預設值。**

```yaml
watchdog:
  # Legacy SINGLE-AGENT mode for the dev-vantage idle watchdog. When set, the
  # watchdog watches ONLY this agent (with the global dev_idle_threshold_secs)
  # instead of iterating every fleet instance. Omit it (default) to keep the
  # modern per-instance iteration. Mirrors AGEND_IDLE_WATCHDOG_AGENT.
  idle_watchdog_agent: dev
  # Recipient for dev-vantage idle alerts. Default: lead.
  dev_recipient: lead
  # Recipient for fleet-vantage idle alerts ("the whole fleet is quiet").
  # Default: lead (#1563: was general, which over-pinged the general assistant).
  fleet_recipient: lead
  # Recipients for task-stall warnings. Default: [general, lead].
  task_stall_recipients:
    - general
    - lead
  # Recipient for the decision-timeout auto-default (operator-proceed) emission.
  # Default: general.
  decision_timeout_recipient: general
```

對應的 env var（`AGEND_IDLE_WATCHDOG_AGENT`、`AGEND_IDLE_WATCHDOG_DEV_RECIPIENT`、
`AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT`、`AGEND_TASK_STALL_RECIPIENTS`、
`AGEND_DECISION_TIMEOUT_RECIPIENT`）皆已**棄用**——它們還能再運作一個
版本週期，但之後會被移除；參見 `docs/env-vars.md` §8。

### 環境變數

#### 合併優先順序

環境變數依以下順序解析（優先順序最高者勝出）：

1. **執行期 SPAWN 參數**——在 MCP `start_instance` / SPAWN API 呼叫中傳入的 `env` 物件
2. **Instance `env`**——fleet.yaml 中的 `instances.<name>.env`
3. **Defaults `env`**——fleet.yaml 中的 `defaults.env`
4. **Daemon 預設值**——由 daemon 自動注入（見下文）

在 fleet.yaml 內，會先套用 `defaults.env`，再由 `instances.<name>.env` 擴充／覆蓋。如果執行期 SPAWN 呼叫包含 `env` 物件，會完全取代 fleet.yaml 解析出來的 env。

#### Daemon 注入的變數

daemon 會在套用 fleet.yaml env 之前，將以下變數注入每個 agent process：

| 變數 | 值 | 用途 |
|------|------|------|
| `AGEND_INSTANCE_NAME` | Agent 名稱 | 向 MCP 工具和 daemon IPC 標識 agent |
| `TERM` | `xterm-256color` | PTY 渲染所用的終端類型 |
| `COLORTERM` | `truecolor` | 啟用 24-bit 色彩支援 |
| `FORCE_COLOR` | `1` | 強制會檢查此變數的工具輸出彩色 |
| `GIT_EDITOR` | `true` | 避免 git 開啟互動式編輯器 |
| `GIT_SEQUENCE_EDITOR` | `true` | 避免 `git rebase -i` 開啟編輯器 |
| `EDITOR` | `true` | 後備編輯器抑制 |
| `VISUAL` | `true` | 後備編輯器抑制 |
| `LANG` | `en_US.UTF-8` | 僅在環境中尚未有 `LANG` 時設定 |

`GIT_EDITOR` 系列變數可避免 agent 在 git 操作時卡在互動式編輯器（例如 Vim）。fleet.yaml 的 env 項目會覆蓋這些預設值——設定 `instances.dev.env.GIT_EDITOR: vim` 即可為該 agent 恢復互動式編輯。

#### 敏感金鑰拒絕清單

以下環境變數名稱在 fleet.yaml `env` 中會被**封鎖**，以防止意外洩漏憑證或執行期劫持。嘗試設定這些變數會被靜默丟棄並記錄一筆警告日誌：

| 類別 | 被封鎖的金鑰 |
|------|-------------|
| AI backend 憑證 | `ANTHROPIC_API_KEY`、`ANTHROPIC_AUTH_TOKEN`、`OPENAI_API_KEY`、`GOOGLE_API_KEY`、`GEMINI_API_KEY` |
| 雲端憑證 | `AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、`AWS_SESSION_TOKEN` |
| Git forge token | `GITHUB_TOKEN`、`GITLAB_TOKEN`、`NPM_TOKEN` |
| 動態連結器注入 | `LD_PRELOAD`、`LD_LIBRARY_PATH`、`LD_AUDIT`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`、`DYLD_FALLBACK_LIBRARY_PATH` |
| AgEnD 執行期內部變數 | `AGEND_HOME`、`AGEND_INSTANCE_NAME` |

這些憑證應改在主機 shell 環境或 `.env` 檔中設定——daemon process 會繼承它們並自動傳遞給 agent 子程序。

#### 範例

**Proxy 設定：**

```yaml
defaults:
  env:
    HTTP_PROXY: "http://proxy.corp:8080"
    HTTPS_PROXY: "http://proxy.corp:8080"
    NO_PROXY: "localhost,127.0.0.1"
```

**透過主機環境設定 API 金鑰（建議）：**

```yaml
# 別把祕密放進 fleet.yaml：
#   env:
#     MY_API_KEY: "sk-abc123"    # BAD — checked into git
#
# 改在你的 shell profile 或 .env 中設定：
#   export MY_SERVICE_API_KEY=sk-abc123
#
# 然後只在需要改名時才在 fleet.yaml 中引用：
defaults:
  env:
    SERVICE_KEY_ALIAS: "${MY_SERVICE_API_KEY}"
```

**Per-instance 覆蓋：**

```yaml
defaults:
  env:
    LOG_LEVEL: info

instances:
  dev:
    env:
      LOG_LEVEL: debug        # overrides defaults
      RUST_BACKTRACE: "1"     # added for this instance only
  reviewer:
    role: "Code reviewer"
    # inherits LOG_LEVEL=info from defaults
```

---

## 啟動流程

### `agend-terminal start`

```
agend-terminal start
```

啟動流程依序執行以下步驟：

1. **Daemon 鎖定**：取得 `$AGEND_HOME/.daemon.lock` 獨佔鎖，確保同一時間
   只有一個 daemon 運行。如果已有 daemon 在執行，會提示使用 `attach` 或
   `app` 連接。

2. **清理殘留**：掃描並清理上次異常結束留下的 run directory 和 zombie process。

3. **載入 fleet.yaml**：讀取並解析 YAML，執行正規化：
   - 如果 fleet.yaml 是空的，自動建立一個 `general` instance
   - 為沒有 `id` 欄位的 instance 自動分配 UUIDv4
   - 將 `channels`（複數形式）正規化為 `channel`（單數）

4. **前置檢查**：執行 doctor 驗證（確認 backend 可執行、端口可用等）。

5. **解析 Agent**：對每個 instance：
   - 合併 defaults 和 instance 配置
   - 展開 `~/` 路徑
   - 驗證 backend 和 ready_pattern
   - 建立工作目錄（如果不存在）
   - 建立 git worktree（如果 `source_repo` 或 `git_branch` 有設定且
     `worktree` 不是 `false`）

6. **初始化 Telegram**：如果有設定 channel，建立 bot 連線並為每個 agent
   建立或綁定 Telegram topic。

7. **設定 Git Shim**：在 `$PATH` 中注入 `agend-git` wrapper，讓 daemon
   可以攔截和管理 agent 的 git 操作。

8. **啟動所有 Agent**：依序 spawn 每個 agent 的 PTY process：
   - 建構命令列（backend preset + 使用者參數 + 環境變數）
   - 開啟 PTY（虛擬終端）
   - 啟動子程序
   - 註冊到 agent registry
   - 啟動 PTY 讀取執行緒
   - 多個 agent 之間會有短暫的交錯延遲，避免同時啟動造成系統負擔

9. **寫入就緒標記**：daemon 初始化完成後寫入 `.ready` 檔案。

### 前景模式

```
agend-terminal start --foreground
```

預設情況下 `start` 會以 detached service 模式運行（背景執行）。加上
`--foreground` 會保持在前景，stdout/stderr 直接輸出到終端——適合除錯或在
process supervisor（systemd / launchd）下運行。

### 直接指定 Agent

```
agend-terminal start --agents dev:claude reviewer:kiro-cli
```

跳過 fleet.yaml，直接以 `name:backend` 格式指定要啟動的 agent。
此模式隱含 `--foreground`。

---

## Resume 模式

當 daemon 重新啟動（crash 後自動重啟或手動 stop/start），agent 可以恢復
上次的對話狀態，而不是從頭開始。

### 各 Backend 的 Resume 行為

| Backend | Resume 旗標 | 說明 |
|---------|------------|------|
| Claude Code | `--continue` | 恢復最近一次在工作目錄中的對話 |
| Kiro CLI | `--resume` | 恢復最近一次對話 |
| Codex | 內建 | 由 Codex 自行管理 session |
| OpenCode | `--continue` | 恢復最近一次對話 |
| Gemini | `--resume latest` | 恢復最近一次對話 |
| Agy | `--continue` | 恢復最近一次對話 |
| Shell | 不支援 | 每次啟動都是新 session |

### 降級機制

如果 daemon 嘗試以 resume 模式啟動 agent，但偵測到沒有可恢復的 session
（例如第一次啟動或 session 檔案已被清除），會自動降級為 fresh 模式啟動，
避免 `--continue` 旗標在空 session 時報錯。

---

## 生命週期管理

### 停止 Daemon

```
agend-terminal stop
```

優雅地停止 daemon 和所有 agent。

### 狀態查詢

```
agend-terminal list              # 簡易列表（agent 名稱）
agend-terminal list --detailed   # 詳細資訊（狀態、健康度、backend）
agend-terminal list --json       # JSON 格式輸出
```

### 健康監控

daemon 持續監控每個 agent 的健康狀態：

- **Healthy**：正常運行
- **Recovering**：crash 後正在恢復
- **Unstable**：短時間內多次 crash
- **Failed**：超過最大重試次數，停止自動重啟
- **Hung**：agent 無回應（有 pending input 但超時未回應）
- **IdleLong**：長時間無活動（但沒有 pending input，非異常）

自動重啟機制使用指數退避（exponential backoff），從 5 秒開始，
最長 5 分鐘，在 10 分鐘窗口內追蹤 crash 次數。

---

## fleet.yaml 欄位合併規則

當 fleet.yaml 被更新（例如透過 `fleet deployment deploy` 或手動編輯）時，
欄位分為兩類：

### Daemon 管理欄位

以下欄位由 daemon 自動管理，合併時 daemon 的值優先：

- `id`：instance UUID
- `topic_id`：Telegram topic ID
- `git_branch`：當前 worktree 分支
- `source_repo`：git 倉庫路徑

### Operator 手動欄位

其他所有欄位（`role`、`backend`、`env`、`working_directory` 等）由 operator
控制。如果合併時發現衝突，daemon 會報錯而非靜默覆蓋。

---

## 常見問題

### Q: fleet.yaml 修改後需要重啟 daemon 嗎？

是的。目前 fleet.yaml 的修改需要 `stop` + `start` 才會生效。

### Q: 一個 agent 可以屬於多個團隊嗎？

fleet.yaml 的 `teams` 結構不限制這一點，但 MCP 通訊工具的團隊路由假設
每個 agent 最多屬於一個團隊。

### Q: 怎麼新增一個 agent？

在 `instances` 區塊下新增一個 key-value 對，然後重啟 daemon：

```yaml
instances:
  # ...existing agents...
  new-agent:
    role: "New agent for feature X"
    working_directory: ~/Projects/feature-x
```