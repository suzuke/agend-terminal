[English](FEATURE-channels.md)

# Channels — Telegram / Discord 整合

Channels 系統讓操作者直接在 Telegram 或 Discord 中與 agent 互動，不需要開啟終端。每個 agent 對應一個 Telegram forum topic，訊息自動雙向同步。

## 使用情境

> **Target audience:** Both operators and agents.

操作者出門在外，想快速確認某個 agent 的狀態、送出一則指令，或直接讀到最新回覆時，Telegram 就變成主要操作面。daemon 會把 topic 狀態維持同步，不需要回到 terminal 才能做事。

agent 完成工作後回覆訊息，系統會把回覆同步回同一個 topic，讓操作者可以在原本的對話脈絡裡直接追蹤進度，而不是分散在不同工具裡找訊息。

當 fleet 規模變大時，每個 agent 對應一個 topic 的設計能維持對話隔離。操作者可以同時看多個 agent，但不會把不同上下文混在一起。

## 設計理念

多 agent 團隊工作時，操作者需要一個隨時隨地都能觸及 agent 的管道。Channels 提供：

- **雙向通訊**：在 Telegram topic 中發訊息，agent 收到；agent 回覆，Telegram 中看到
- **一對一映射**：每個 agent 有自己的 topic，對話不會混在一起
- **平台無關抽象**：核心程式碼不依賴特定平台，透過 Channel trait 適配不同服務
- **自動設定**：daemon 啟動時自動建立缺少的 topic，不需手動設定

---

## 快速開始

### 1. 設定 Telegram Bot

在 `fleet.yaml` 中加入 channel 設定：

```yaml
channel:
  telegram:
    bot_token: "123456:ABC-DEF..."
    group_id: -1001234567890
    user_allowlist:
      - 12345678    # 你的 Telegram user ID
```

- `bot_token`：從 @BotFather 取得
- `group_id`：Telegram 群組 ID（必須是已啟用 Topics 的超級群組）
- `user_allowlist`：允許與 agent 互動的 Telegram user ID 列表

### 2. 啟動 daemon

```bash
agend-terminal start
```

Daemon 啟動時會自動：
1. 讀取 `fleet.yaml` 的 channel 設定
2. 載入 `topics.json`（topic ID ↔ agent 名稱的映射）
3. 為缺少 topic 的 agent 自動建立 Telegram forum topic
4. 建立 fleet binding topic（跨 agent 的團隊事件通知）
5. 開始輪詢收取訊息

### 3. 開始對話

在 Telegram 群組中找到對應 agent 名稱的 topic，直接打字即可。Agent 會在該 topic 中回覆。

---

## 核心概念

### Channel Trait

所有 channel 實作共用同一個 trait 介面：

| 方法 | 說明 |
|------|------|
| `send` | 發送訊息到指定 binding |
| `edit` | 編輯已發送的訊息 |
| `delete` | 刪除訊息 |
| `create_binding` | 為 agent 建立頻道綁定 |
| `remove_binding` | 移除綁定 |
| `create_topic` | 建立新的 forum topic |
| `poll_event` | 輪詢收取新訊息 |

核心程式碼只透過 trait 操作，不直接呼叫 Telegram API。新增 Discord 或 Slack 支援只需實作這個 trait。

### Binding（綁定）

Binding 是 agent 與 channel 之間的連結。每個 binding 包含平台特定的定址資訊（例如 Telegram topic ID），但核心程式碼不需要知道這些細節——binding 是不透明的（opaque）。

```
agent "dev" ← binding → Telegram topic #42
agent "reviewer" ← binding → Telegram topic #43
```

### Capabilities（能力矩陣）

每個 channel 宣告自己支援的功能，核心程式碼據此決定降級行為：

| 能力 | Telegram | Discord |
|------|----------|---------|
| 原生討論串 | 是（forum topics） | 是（threads） |
| Markdown | MarkdownV2 | Discord Markdown |
| 附件上傳 | 是 | 是 |
| 訊息編輯 | 是 | 是 |
| Emoji 反應 | 是 | 是 |
| 打字指示器 | 是 | 是 |
| 訊息長度上限 | 4096 bytes | 2000 chars |
| 刪除事件 | 否 | 是 |

不支援的功能會靜默降級，不會報錯。

### Topics 與 Registry

daemon 維護一個 topic 的 registry，用來把 Telegram 中實際存在的狀態與磁碟上記錄的狀態對齊。這個 registry 是 topic 路由的事實來源，而 topic 本身則是操作者可見的對話面。

### 收訊與發訊（Inbound vs Outbound）

Channels 處理兩個方向：

- **Inbound**：操作者或外部事件流入 daemon，再進入 agent。
- **Outbound**：agent 的回覆被同步回同一個 channel 面。

兩側的程式碼路徑對稱，所以無論除錯哪一側，通常都從同一筆 topic 條目與 binding 紀錄開始。

---

## Topics 映射

### topics.json

Agent 與 Telegram topic 的映射關係持久化在 `topics.json`：

```json
{
  "42": "dev",
  "43": "reviewer",
  "100": "general",
  "500": "__fleet__"
}
```

- Key 是 Telegram forum topic ID（字串化的數字）
- Value 是 agent 的 instance name
- `__fleet__` 是保留的 sentinel，用於跨 agent 的團隊事件通知

### 自動建立 Topic

Daemon 啟動時，對每個在 `fleet.yaml` 中定義但在 `topics.json` 中沒有對應 topic 的 agent，自動建立 forum topic。

TUI 中透過 `Ctrl+B c` 新增 agent 時，也會自動建立 topic 並註冊。

### 孤兒 Topic 清理

使用 `doctor topics` 命令檢查和清理孤兒 topic：

```bash
# 檢查 topic 狀態
agend-terminal doctor topics

# 清理孤兒 topic
agend-terminal doctor topics --cleanup
```

Topic 分為兩類：
- **Live**：在 `topics.json` 和 `fleet.yaml` 中都存在
- **Orphan**：在 `topics.json` 中存在但 `fleet.yaml` 中沒有（agent 已刪除但 topic 未清理）

---

## 訊息流程

### 收訊（Telegram → Agent）

```
1. 使用者在 Telegram forum topic 中發送訊息
2. 輪詢執行緒偵測到新訊息，取得 topic_id
3. 透過 topic_to_instance 映射解析目標 agent
4. 訊息寫入 agent 的收件匣
5. Agent 呼叫 inbox 工具讀取完整訊息
```

### 發訊（Agent → Telegram）

```
1. Agent 呼叫 reply MCP 工具
2. 去重檢查：相同內容在 5 秒內不重複發送
3. 查詢 instance_to_topic 取得目標 topic_id
4. 透過 Telegram Bot API 發送訊息
5. 記錄 message_id 供後續編輯/刪除使用
```

### Mirror Skip（防重複）

Agent 的回覆會同時出現在 Telegram topic 和 PTY 終端輸出中。為避免 PTY mirror 把 agent 自己的回覆又轉發一次，系統在發送前設定 `mirror_skip_until_next_turn` 旗標。這個旗標會在下一輪使用者輸入時自動重置。

---

## 去重機制

防止以下 race condition 造成重複訊息：

- App 和 daemon 同時輪詢 CI watch，各發一次通知
- PTY mirror 和 reply 工具同時發送相同內容
- 重試邏輯未充分保護的發送路徑

去重使用內容雜湊 + TTL 視窗（預設 5 秒）：

```yaml
# fleet.yaml 可調整 TTL
channel:
  dedup_ttl_secs: 5
```

相同的 (instance, topic, content hash) 在 TTL 內只會發送一次。記憶體上限 1024 筆，使用插入順序的 LRU 策略。

---

## 通知閘門

Agent 的對外通知（CI 狀態、任務完成等）預設關閉（fail-closed）。必須在 `fleet.yaml` 中設定 `user_allowlist` 才會啟用：

```yaml
channel:
  telegram:
    user_allowlist:
      - 12345678
```

未設定時，所有對外通知靜默丟棄，不會報錯。這防止未設定的 bot 意外將資訊洩漏到未授權的群組。

---

## Fleet Binding Topic

fleet binding 是一個特殊的 topic，用於顯示跨 agent 的團隊事件：

| 事件類型 | 格式 |
|----------|------|
| 任務委派 | `[lead → dev] DELEGATE 修復 #1177 (#t-...)` |
| 結果回報 | `[dev → lead] REPORT PR 已建立 (#t-...)` |
| 決策發布 | `[lead] DECISION 使用 prefix match (#d-...)` |
| 廣播 | `[lead → 3 agents] BROADCAST merge freeze` |

操作者可以在一個 topic 中看到整個團隊的活動概覽，不需要逐一檢查每個 agent 的 topic。

---

## 自我修復

### Topic 被刪除

如果 Telegram topic 被意外刪除（管理員操作或 API 錯誤），系統會自動：

1. 偵測到 topic-deleted 錯誤
2. 清除無效的 topic 映射
3. 重新建立 topic
4. 重試發送

### 超級群組遷移

Telegram 群組升級為超級群組時，`group_id` 會改變。系統偵測到 `MigrateToChatId` 錯誤後：

1. 讀取新的 chat ID
2. 更新 `fleet.yaml` 中的 `group_id`
3. 重試發送

這兩種情況都不需要操作者介入。

---

## 多 Channel 支援

目前支援 Telegram，Discord 為預留介面（feature gate）。架構設計支援同時使用多個 channel：

- 每個 channel 獨立註冊在全域 registry 中
- 每個 agent 可以綁定到不同的 channel
- 收訊事件由 dispatcher 統一合併
- 發訊根據 binding 的 channel kind 路由到正確的 adapter

---

## Telegram 行為

### Topic 建立

如果某個 agent 還沒有 topic，啟用 channel 時 daemon 會自動建立一個。這讓 fleet bootstrap 保持簡單：設定 bot、啟動 daemon，然後讓 registry 自行填充。

### Topic 清理

如果 registry 與 Telegram 的實際狀態漂移，`doctor topics` 可以偵測並選擇性清理孤兒條目。操作者面向的診斷流程請見 `docs/FEATURE-diagnostics.md`。

### 權限邊界

會變更 chat 的 topic 清理需要 bot 具備 `can_manage_topics` 權限。如果缺少這個權限，daemon 會把該 topic 保留在報告中，並跳過 chat 側的刪除。

---

## Discord 註記

文件提到 Discord，是因為 channel 抽象在精神上並非只限 Telegram。目前的實作以 Telegram 為核心，但這些抽象在設計上允許透過實作同一組 trait 介面來新增另一個平台。

如果未來新增 Discord backend，主要的預期是一樣的：

- 每個 agent 都需要一個穩定、可定址的對話目標
- 訊息必須往返而不遺失 thread identity
- registry 狀態必須持久化，daemon 才能在重啟後復原

---

## 常見工作流程

### 操作者發送訊息

1. 開啟該 agent 的 topic。
2. 輸入訊息。
3. daemon 收到訊息、把它路由到 agent，並把 agent 的回應同步回同一個 topic。

### Agent 回覆操作者

1. agent 透過 channel 層送出回應。
2. daemon 發送或編輯對應的 Telegram 訊息。
3. 操作者在脈絡中看到回覆，且 topic 歷史被保留。

### 診斷漂移

如果 topic 看起來在 Telegram 中存在但不在 daemon registry 中：

1. 執行 `agend-terminal doctor topics`。
2. 檢查條目是 `live` 還是 `orphan`。
3. 只在你確認動作集之後才使用 `--cleanup`。

---

## 設定範例

一個最小可運作的設定看起來像這樣：

```yaml
channel:
  telegram:
    bot_token_env: AGEND_BOT_TOKEN
    group_id: -1001234567890
    user_allowlist: [123456789]
```

接著匯出 token：

```bash
export AGEND_BOT_TOKEN="123456:abcdef..."
```

如果你想讓操作者在群組聊天中看到 topic map，請確認群組已啟用 Topics，且 bot 具備建立與管理 topic 所需的權限。

---

## 疑難排解

### Bot 收不到訊息

1. 確認 bot 已加入群組且有讀取權限
2. 確認群組已啟用 Topics（設定 → Topics → 開啟）
3. 確認 `user_allowlist` 包含你的 Telegram user ID
4. 檢查 daemon 日誌中的 channel 初始化訊息

### Topic 沒有自動建立

1. 確認 bot 有 `can_manage_topics` 權限（群組管理員設定）
2. 執行 `agend-terminal doctor topics` 檢查狀態
3. 檢查 `topics.json` 中的現有映射

### 訊息重複

調整去重 TTL：

```yaml
channel:
  dedup_ttl_secs: 10    # 加大視窗
```

如果問題持續，檢查是否有多個 daemon 實例同時運行。

---

## 原始碼指引

- `src/channel/telegram.rs`：Telegram channel 實作
- `src/bootstrap/doctor_topics.rs`：topic 分類與清理邏輯
- `src/cli.rs`：`doctor topics` CLI 流程
- `src/main.rs`：subcommand 路由與進入點
- `src/fleet.rs`：fleet 與 instance metadata，含 topic 欄位

---

## 實務建議

1. 把 topic registry 當作 state，而不是 cache。
2. 嘗試清理前永遠先驗證權限。
3. 除非有特定理由，否則維持每個 agent 一個 topic。
4. 當可見的 chat 狀態與 daemon 的狀態不再一致時，使用 `doctor topics`。
