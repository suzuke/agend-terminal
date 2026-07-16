# Chat Platform API 參考 — AgEnD Channel 擴充用

各平台的具體接入方式，供實作 `src/channel/` adapters 時參考。

---

## 0. Telegram（已實作 ✅ — 現有參考實作）

AgEnD 目前唯一完整實作的 channel。以下記錄其架構作為其他 adapter 的參考模板。

### 連接方式：Long Polling（getUpdates）

不需要 public URL。Daemon 用 `teloxide` crate 主動 polling Telegram API。

### 實作架構（`src/channel/telegram/`）

```
telegram/
├── mod.rs               # 子模組匯出
├── adapter.rs           # Channel trait 實作 + TelegramBindingPayload
├── bootstrap.rs         # init_from_config() + attach_registry()
├── inbound.rs           # start_polling() → handle_message()
├── reply.rs             # agent → user 回覆邏輯
├── send.rs              # send_with_topic / send_media
├── bot_api.rs           # 底層 Telegram Bot API 封裝 (edit/react)
├── creds.rs             # token 讀取 (bot_token_env fallback)
├── error.rs             # 錯誤處理 + topic deletion cleanup
├── state.rs             # TelegramState (Mutex-wrapped shared state)
├── topic_registry.rs    # Forum topic 自動建立/刪除
├── notify.rs            # 通知推送 (silent/normal)
└── ux_sink.rs           # UxEventSink 實作 (fleet activity mirror)
```

### 關鍵設計模式

1. **獨立 thread + tokio runtime** — `start_polling()` 在專屬 thread 跑自己的 tokio runtime（不與 TUI 的 main runtime 衝突）
2. **Supervisor loop** — panic/disconnect 自動 5s 重連
3. **Topic-based routing** — 每個 agent 有自己的 Forum Topic，訊息路由靠 `topic_id → instance_name`
4. **User allowlist** — 空 = 拒絕全部（fail-closed）；explicit IDs 才放行
5. **Fleet binding** — fleet activity（delegate/report/decision）mirror 到獨立 topic

### Telegram Bot API 重點

```
# Long Polling (teloxide 內部處理):
GET https://api.telegram.org/bot{token}/getUpdates?offset={last+1}&timeout=30

# 發訊息:
POST https://api.telegram.org/bot{token}/sendMessage
{ "chat_id": group_id, "message_thread_id": topic_id, "text": "..." }

# 編輯訊息 (streaming update):
POST https://api.telegram.org/bot{token}/editMessageText
{ "chat_id": ..., "message_id": ..., "text": "updated..." }

# Forum Topic 操作:
POST .../createForumTopic { "chat_id": ..., "name": "agent-name" }
POST .../deleteForumTopic { "chat_id": ..., "message_thread_id": ... }
```

### 現有 fleet.yaml schema

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_TELEGRAM_BOT_TOKEN  # env var containing the token
  group_id: -1001234567890                  # Telegram group (supergroup) chat ID
  mode: topic                               # "topic" for Forum topics
  user_allowlist:                           # Telegram user IDs (not usernames)
    - 123456789
  fleet_binding:                            # optional: where fleet events go
    type: topic
    name: "fleet-activity"
```

### Rust Crate

- `teloxide` — 成熟的 Telegram Bot framework，內建 long polling + dispatcher

---

## 1. Discord（已實作 ✅ — `discord` feature gate）

### 現有實作狀態

**已完成（`src/channel/discord/`）：**
- `DiscordChannel` struct + `DiscordState` + `DiscordBindingPayload`
- `ChannelCapabilities` 設定（markdown dialect、mention style）
- `twilight-http::Client` 整合
- outbound send/edit/delete 與 binding lifecycle
- live Gateway WebSocket、`MESSAGE_CREATE` mapping、heartbeat/reconnect supervisor
- daemon/app bootstrap 與 inbound `poll_event()` → agent inbox dispatcher
- allowlist fail-closed 與 inbound/outbound/reconnect 測試矩陣

**目前邊界：** Discord 尚未提供 Telegram 的 `UxEventSink`／`fleet_binding`
等價功能；fleet activity mirror 仍是 Telegram-only。編譯時也必須啟用
`discord` feature。

**Feature gate:** `discord` feature in Cargo.toml → 啟用 `twilight-gateway` 0.16 + `twilight-http` 0.16 + `twilight-model` 0.17

### 連接方式：Gateway WebSocket（長連接）

不需要 public URL。Bot 主動連接 Discord Gateway。

### 設置步驟

1. **Discord Developer Portal** → 建立 Application → Bot
2. **Privileged Gateway Intents** 必須勾選：
   - ✅ `MESSAGE_CONTENT` — 讀取訊息內容（2022 年後必須明確開啟）
   - ✅ `GUILD_MEMBERS` — 讀取成員資訊（optional，只有查 member info 時需要）
3. **Bot Permissions** (OAuth2 URL Generator)：
   - Send Messages, Embed Links, Attach Files
   - Read Message History, Add Reactions
   - Create Public Threads, Send Messages in Threads

### 關鍵 API 概念

```
wss://gateway.discord.gg/?v=10&encoding=json

認證流程：
1. Connect WebSocket
2. Receive HELLO { heartbeat_interval }
3. Send IDENTIFY { token, intents: GUILDS | GUILD_MESSAGES | MESSAGE_CONTENT, ... }
4. Receive READY { session_id, user, guilds }
5. Start heartbeat loop (interval from HELLO)
6. Receive MESSAGE_CREATE events

斷線重連：
- Receive RECONNECT → close + reconnect with resume
- Receive INVALID_SESSION → re-identify
- twilight-gateway 內建處理這些
```

### Rust Crate

已選用 **twilight** 生態系（Cargo.toml 已有）：

| Crate | Version | 用途 |
|-------|---------|------|
| `twilight-gateway` | 0.16 | WebSocket shard lifecycle（connect/heartbeat/resume） |
| `twilight-http` | 0.16 | REST API（send message/create thread/add reaction） |
| `twilight-model` | 0.17 | 型別定義（Message, Channel, Guild 等） |

選 twilight 而非 serenity 的原因：模組化、低層控制、不帶 cache 開銷。

### Runtime wiring

`channel::discord::init_from_config` resolves the token, starts the gateway
supervisor, and returns a registered `DiscordChannel`. The bootstrap layer then
starts the inbound dispatcher, attaches the agent registry when ready, and routes
authorized gateway events into the target agent inbox.

### Rate Limit

- REST API：50 requests/second（global）
- Gateway：120 commands/60s
- 10,000 invalid requests/10min → Cloudflare ban

### fleet.yaml schema

```yaml
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_BOT_TOKEN
  guild_id: 123456789012345678        # Discord server (guild) snowflake ID
  user_allowlist:                     # Discord user snowflakes; omit/empty = deny all
    - 111222333444555666
```

---

## 2. Slack

### 連接方式：Socket Mode（WebSocket，不需 public URL）✅

Slack Socket Mode 是 outbound WebSocket — app 主動連接 Slack，完美適合 local daemon。

### 設置步驟

1. **api.slack.com/apps** → Create New App → From Scratch
2. **Socket Mode** → Enable Socket Mode ✅
3. **Basic Information** → App-Level Tokens → Generate：
   - Name: `agend-socket`
   - Scope: `connections:write`
   - 產出 `xapp-...` token
4. **OAuth & Permissions** → Bot Token Scopes：
   - `app_mentions:read`
   - `channels:history`
   - `chat:write`
   - `files:read`（optional: 讀取上傳的文件）
   - `reactions:write`（optional: emoji reactions）
5. **Event Subscriptions** → Enable Events → Subscribe：
   - `app_mention`
   - `message.channels`
6. Install to Workspace → 取得 `xoxb-...` Bot User OAuth Token

### 關鍵 API 概念

```
# 1. 用 App-Level Token 取得 WebSocket URL:
POST https://slack.com/api/apps.connections.open
Authorization: Bearer xapp-...
→ { "ok": true, "url": "wss://wss-primary.slack.com/link/?ticket=..." }

# 2. Connect WebSocket，收到事件 envelope:
{
  "type": "events_api",
  "envelope_id": "dbbc1fda-...",
  "payload": {
    "event": {
      "type": "app_mention",
      "text": "<@U0BOT> fix this bug",
      "channel": "C0123456789",
      "user": "U0SENDER",
      "ts": "1234567890.123456",
      "thread_ts": null
    }
  }
}

# 3. ACK（必須 3 秒內）:
→ send: { "envelope_id": "dbbc1fda-..." }

# 4. 回覆（Web API）:
POST https://slack.com/api/chat.postMessage
Authorization: Bearer xoxb-...
{ "channel": "C0123456789", "text": "Working on it...", "thread_ts": "1234567890.123456" }

# 5. 更新訊息 (streaming):
POST https://slack.com/api/chat.update
{ "channel": "...", "ts": "reply_ts", "text": "Updated content..." }
```

### 斷線重連

Socket Mode WebSocket 會定期斷開（Slack 主動 close）。需要：
1. 收到 `disconnect` event 或 WebSocket close
2. 重新 call `apps.connections.open` 拿新 URL
3. 重新連接

### Rust Crate

無成熟 Rust Slack Socket Mode crate。建議直接用：
- `tokio-tungstenite` — WebSocket 連接
- `reqwest` — Web API calls
- `serde_json` — envelope 解析

Protocol 簡單（一個 WS + envelope ACK + REST），不值得引入重量級 framework。

### Rate Limit

- Web API：大部分 Tier 2（20+ req/min）或 Tier 3（50+ req/min）
- `chat.postMessage`：1 msg/sec per channel（burst OK）
- Socket Mode：需 3 秒內 ACK，否則 Slack 認為失敗

### fleet.yaml schema

```yaml
channel:
  type: slack
  bot_token_env: AGEND_SLACK_BOT_TOKEN    # xoxb-... (Bot User OAuth Token)
  app_token_env: AGEND_SLACK_APP_TOKEN    # xapp-... (App-Level Token for Socket Mode)
  allowed_channels:
    - "C0123456789"
  # allowed_users:
  #   - "U0123456789"
```

---

## 3. Feishu / Lark（飛書）

### 連接方式：WebSocket 長連接（不需 public URL）✅

飛書支援 WebSocket 長連接模式 — 不需要 public URL，適合 local daemon。

### 設置步驟

1. **open.feishu.cn**（飛書）或 **open.larksuite.com**（Lark）→ 建立「自建應用」
2. **憑證與基本資訊** → 記下 App ID + App Secret
3. **應用能力** → 開啟「機器人」
4. **事件與回調** → 配置方式選「長連接」（WebSocket）
5. 訂閱事件：
   - `im.message.receive_v1` — 收到訊息

### 關鍵 API 概念

```
# 1. 取得 tenant_access_token:
POST https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal
{ "app_id": "cli_xxx", "app_secret": "xxx" }
→ { "tenant_access_token": "t-xxx", "expire": 7200 }

# 2. WebSocket 長連接:
# 官方 SDK 提供 WSClient 實作
# Rust 手動: 連接 wss://open.feishu.cn/ws/... 帶 token 握手
# 具體 WS URL 需透過 SDK 或逆向取得（文檔較少）

# 3. 收到事件:
{
  "header": {
    "event_id": "xxx",
    "event_type": "im.message.receive_v1",
    "create_time": "1234567890"
  },
  "event": {
    "message": {
      "message_id": "om_xxx",
      "chat_id": "oc_xxx",
      "message_type": "text",
      "content": "{\"text\":\"fix this bug\"}"
    },
    "sender": {
      "sender_id": { "open_id": "ou_xxx" },
      "sender_type": "user"
    }
  }
}

# 4. 回覆:
POST https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/reply
Authorization: Bearer t-xxx
{ "content": "{\"text\":\"Working on it...\"}", "msg_type": "text" }

# 或主動發到群組:
POST https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id
{ "receive_id": "oc_xxx", "content": "...", "msg_type": "text" }
```

### 注意事項

- `tenant_access_token` 2 小時過期，需要定時刷新
- WebSocket 長連接的文檔比較少，建議參考：
  - OpenAB `gateway/` 目錄（有飛書 WebSocket 實作）
  - `ConnectAI-E/Feishu-Webhook-Proxy`（Node 實作）
- 飛書和 Lark 是同一平台不同域名（`open.feishu.cn` vs `open.larksuite.com`）

### Rust Crate

- 無成熟 Rust crate
- 用 `tokio-tungstenite` + `reqwest` 手動實作
- JSON content 是 stringify 過的（`content` 欄位是 JSON string 不是 object）

### Rate Limit

- API：大部分 50-100 QPS
- 訊息發送：50 次/分鐘/應用

### fleet.yaml schema

```yaml
channel:
  type: feishu
  app_id_env: AGEND_FEISHU_APP_ID
  app_secret_env: AGEND_FEISHU_APP_SECRET
  # allowed_users:
  #   - "ou_xxx"
```

---

## 4. LINE

### 連接方式：Webhook（需 public URL）⚠️

LINE 只支援 webhook push。需要 Cloudflare Tunnel 或類似方案暴露 local port。

### 設置步驟

1. **LINE Developers Console** → Create Provider → Messaging API Channel
2. 記下：Channel ID、Channel Secret、Channel Access Token
3. 設定 Webhook URL（需要 public HTTPS）
4. LINE Official Account Manager → 關閉 auto-reply

### 關鍵 API 概念

```
# LINE 推送到你的 webhook:
POST https://your-server/webhook/line
X-Line-Signature: <HMAC-SHA256(channel_secret, body)>
{
  "events": [{
    "type": "message",
    "replyToken": "abc123",
    "source": { "type": "user", "userId": "U123" },
    "message": { "type": "text", "text": "fix this bug" }
  }]
}

# 回覆 (replyToken 限時):
POST https://api.line.me/v2/bot/message/reply
Authorization: Bearer {channel_access_token}
{ "replyToken": "abc123", "messages": [{ "type": "text", "text": "..." }] }

# Push (任何時候，有配額):
POST https://api.line.me/v2/bot/message/push
{ "to": "U123", "messages": [{ "type": "text", "text": "..." }] }
```

### 對 AgEnD 的挑戰

需要 public URL。選項：
- `cloudflared tunnel` — 免費但需額外 setup
- 不做 LINE — 除非有明確需求

### fleet.yaml schema

```yaml
channel:
  type: line
  channel_secret_env: AGEND_LINE_CHANNEL_SECRET
  channel_access_token_env: AGEND_LINE_ACCESS_TOKEN
  webhook_port: 8443
```

---

## 優先級總結

| 平台 | 連接方式 | 需要 Public URL | 狀態 | 優先級 |
|------|---------|----------------|------|--------|
| **Telegram** | Long Polling | ❌ | ✅ 已完成（13 個 rs 文件） | — |
| **Discord** | Gateway WebSocket | ❌ | ✅ 已完成（feature-gated；無 fleet activity sink） | — |
| **Slack** | Socket Mode WS | ❌ | 🆕 未開始 | P1 |
| **Feishu/Lark** | WebSocket 長連接 | ❌ | 🆕 未開始 | P2 |
| LINE | Webhook | ⚠️ 需要 | 🆕 未開始 | P3 |
| Google Chat | HTTP/Pub/Sub | ⚠️ 需要 GCP | — | Skip |

Discord 和 Slack 不需要 public URL，完美適配 AgEnD 的本地 daemon 定位。
