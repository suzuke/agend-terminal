---
name: setup-telegram
description: 以互動方式引導設定 AgEnD 的 Telegram channel——建立 bot、偵測群組並寫入 fleet.yaml
---

[English](SKILL.md)

# /setup-telegram——Telegram Channel 設定

引導使用者為 AgEnD 設定 Telegram bot channel。選擇題使用 `AskUserQuestion`、API 驗證使用 `Bash`（curl），config file 則使用 `Read`/`Edit`/`Write`。

## 前置條件

開始前，先找出 AgEnD home directory：

```bash
echo "${AGEND_HOME:-$HOME/.agend-terminal}"
```

將結果儲存為 `AGEND_HOME`，供後續所有步驟使用。

## 步驟 1：透過 BotFather 建立 Bot

告訴使用者：

> 1. 開啟 Telegram，與 **@BotFather** 對話
> 2. 傳送 `/newbot`，依指示為 bot 命名
> 3. 複製 bot token（外觀如 `123456789:ABCdef...`）

使用 `AskUserQuestion` 詢問 token：
- 問題：「貼上 BotFather 提供的 bot token」
- 選項：提供「略過——稍後設定」選項

若略過，告訴使用者日後可再次執行 `/setup-telegram`，並在此停止。

## 步驟 2：驗證 Token 格式

檢查 token 是否符合 `<digits>:<35+ alphanumeric chars>` pattern：

```bash
echo "$TOKEN" | grep -qE '^[0-9]{8,}:[A-Za-z0-9_-]{30,}$'
```

若格式無效，警告使用者，並詢問要重新輸入、仍要繼續，或略過。

## 步驟 3：透過 Telegram API 驗證 Bot

```bash
curl -s "https://api.telegram.org/bot${TOKEN}/getMe"
```

檢查 `result.is_bot` 為 `true`。成功時顯示 bot username；失敗時提供重新輸入或略過選項。

## 步驟 4：設定群組

告訴使用者：

> 1. 建立 Telegram **supergroup**（或使用既有群組）
> 2. 在群組設定中啟用 **Topics**（Group → Edit → Topics）
> 3. 將 bot 加入群組並設為 **admin**
> 4. 在群組中傳送任意訊息

接著使用 `getUpdates` 輪詢群組：

```bash
curl -s "https://api.telegram.org/bot${TOKEN}/getUpdates?timeout=30&allowed_updates=[\"message\"]"
```

解析 response，找出 supergroup chat（type == "supergroup"），並擷取 `chat.id` 與 `chat.title`。最多重試 6 次（總計約 3 分鐘）。若未偵測到群組，請使用者手動輸入 group_id 或略過。

## 步驟 5：驗證 Bot 是 Admin

```bash
curl -s "https://api.telegram.org/bot${TOKEN}/getChatMember?chat_id=${GROUP_ID}&user_id=${BOT_ID}"
```

檢查 `result.status` 是 `"administrator"` 或 `"creator"`。若不是，警告使用者：

> Bot 必須是群組 admin，topic mode 才能運作。請前往群組設定，將 bot 提升為 admin。

這只是警告，不是 blocker——仍繼續執行。

## 步驟 6：使用者 Allowlist

詢問使用者的 Telegram user ID：

> 在 Telegram 傳送訊息給 **@userinfobot**，即可取得你的 user ID（例如 `123456789`）。

收集一或多個 user ID。它們會寫入 fleet.yaml 的 `user_allowlist`。

## 步驟 7：安全儲存 Token

將 token 儲存到 `$AGEND_HOME/.env`：

```bash
# 讀取既有 .env，取代或附加 AGEND_BOT_TOKEN
```

規則：
- 若 `.env` 已有 `AGEND_BOT_TOKEN`，覆寫前先詢問（預設：保留既有值）
- 寫入後設定權限：`chmod 600 "$AGEND_HOME/.env"`
- 檢查 `.gitignore` 是否涵蓋 `.env`——若沒有則警告使用者

**絕不要將 token value 直接寫入 fleet.yaml 或任何 YAML config。** 一律使用 `bot_token_env: AGEND_BOT_TOKEN` 參照 environment variable。

## 步驟 8：更新 fleet.yaml

讀取既有的 `$AGEND_HOME/fleet.yaml`。若已存在 `channel:` section，覆寫前先詢問（預設：保留既有內容，並備份為 `fleet.yaml.bak`）。

加入或更新 channel section：

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: <detected or entered group_id>
  mode: topic
  user_allowlist: [<user_ids>]
```

可以的話使用 `Edit` 修改既有檔案；若從頭建立則使用 `Write`。

## 步驟 9：最終檢查清單

顯示摘要：

> **設定完成！**
> - Bot：@<bot_username>
> - 群組：<group_title>（<group_id>）
> - Token：儲存在 `$AGEND_HOME/.env`（env var：`AGEND_BOT_TOKEN`）
> - Config：已更新 `$AGEND_HOME/fleet.yaml`
>
> **後續步驟：**
> 1. 重新啟動 daemon：`agend restart`
> 2. 在 Telegram 群組傳送訊息，確認 delivery

## 安全防護

下列規則為強制要求，不得 bypass：

1. **Token 只能以 env var 參照**——fleet.yaml 必須使用 `bot_token_env: AGEND_BOT_TOKEN`，絕不能 inline token string
2. **chmod 600**——`.env` 檔案必須只有 owner 可讀寫
3. **gitignore 檢查**——若 `.gitignore` 未涵蓋 `.env`，必須警告
4. **Token 格式驗證**——發出 API call 前先驗證格式
5. **輸出不得包含 token**——向使用者顯示 token 時須遮罩，只顯示前 4 與後 4 個字元（例如 `1234...wxyz`）
