[English](FEATURE-quickstart.md)

# Quickstart — 互動式首次設定

## 使用情境

> **適用對象：** Operator——透過 CLI 使用。

**首次安裝。** 你剛裝好 agend-terminal，想以最少的步驟取得可運作的設定。`quickstart` 會自動偵測 `$PATH` 上的 AI backend、引導你設定 Telegram bot，並產生一份可以直接使用的 `fleet.yaml`。

**團隊新成員加入。** 新成員需要自己的 agent fleet。不用手寫 fleet.yaml 或摸索 Telegram 設定，只要執行 `agend-terminal quickstart`，5 分鐘內就能取得可用的設定。

**換機重新設定。** 搬到新機器後需要重新設定。Quickstart 偵測剛安裝的 backend 並重新產生 fleet.yaml，不需要記住 YAML schema 的細節。

## 設計初衷

AgEnD 整合了多種 AI coding backend（Claude Code、Kiro CLI、Codex、OpenCode、Gemini、
Agy），每個 backend 有不同的安裝路徑、CLI 參數和環境變數。新使用者面對的第一個
問題是：「我裝了什麼？我要怎麼把它跑起來？」

`quickstart` 就是為了回答這個問題。它自動偵測已安裝的 backend，引導使用者
設定 Telegram 通知頻道，然後產生一份可以直接使用的 `fleet.yaml`。整個過程
大約 2–5 分鐘。

```
agend-terminal quickstart
```

---

## 互動流程

### 第一步：偵測已安裝的 Backend

quickstart 啟動後會掃描 `$PATH` 中的可執行檔，比對所有支援的 backend 命令名稱：

| Backend | 偵測的命令 |
|---------|-----------|
| Claude Code | `claude` |
| Kiro CLI | `kiro-cli` |
| Codex | `codex` |
| OpenCode | `opencode` |
| Gemini | `gemini` |
| Agy (Antigravity) | `agy` |

偵測到的 backend 會顯示版本號。如果偵測到多個，使用者可以選擇預設要用哪一個。
如果只偵測到一個，就自動選取。

如果沒有偵測到任何 backend，quickstart 會列出所有支援的 backend 及安裝提示，
然後結束。

**範例輸出（偵測到多個 backend）：**

```
偵測到 2 個 AI coding backend：

  1. Claude Code (v1.2.3)
  2. Kiro CLI (v0.4.1)

選擇預設 backend [1]:
```

### 第二步：Telegram 設定

Telegram 是 AgEnD 目前主要的通知頻道。quickstart 會引導使用者完成 bot 設定：

#### 2a. 取得 Bot Token

quickstart 會顯示 BotFather 的操作步驟：

1. 在 Telegram 搜尋 `@BotFather`
2. 發送 `/newbot`
3. 依提示設定 bot 名稱
4. 複製產生的 token

使用者貼上 token 後，quickstart 會驗證格式（`<8位以上數字>:<30字元以上英數字>`）。
格式不對會提示重新輸入。

#### 2b. 驗證 Bot

通過格式檢查後，quickstart 會呼叫 Telegram `getMe` API 確認 token 有效，
並取回 bot 的 username。

#### 2c. 偵測 Telegram 群組

quickstart 會請使用者把 bot 加入一個 Telegram 超級群組（supergroup），
然後在群組裡發送任意訊息。quickstart 會以 3 分鐘的 long-polling（`getUpdates`）
等待偵測群組。

偵測條件：
- 必須是 **supergroup** 類型（一般群組不支援 topic 模式）
- bot 必須擁有 **管理員權限**（topic 模式需要管理 topic 的權限）

如果超時，使用者可以選擇：
- **重試**：再等 3 分鐘
- **跳過**：先產生 fleet.yaml，之後再手動設定 Telegram
- **結束**：放棄 quickstart

重試次數上限為 3 次，之後會建議跳過。

#### 2d. 儲存 Token

成功偵測群組後，quickstart 會將 `AGEND_BOT_TOKEN` 寫入 `~/.env` 檔案。

安全措施：
- 檔案權限設為 `0600`（僅擁有者可讀寫，Unix only）
- 檢查 `.gitignore` 是否涵蓋 `.env`，避免意外提交到 git

### 第三步：產生 fleet.yaml

quickstart 會在 `$AGEND_HOME/` 下產生 `fleet.yaml`。如果已有現成檔案，
會詢問是否覆蓋。

產生的 fleet.yaml 包含：

```yaml
# 預設 backend
defaults:
  backend: claude  # 或使用者選擇的 backend

# Telegram 通知頻道（如果有設定）
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456789
  mode: topic
  user_allowlist:
    - 12345  # ← 請填入你的 Telegram user ID

# Agent 實例
instances:
  general:
    role: "General-purpose coding assistant"
    working_directory: ~/workspace
```

如果跳過了 Telegram 設定，channel 區塊會以註解形式保留，方便之後手動填入。

`user_allowlist` 欄位永遠會產生（即使是空的），這是 Sprint 21 的 fail-closed
安全設計——沒有在白名單中的 Telegram 使用者無法操作 agent。

### 第四步：後續步驟提示

quickstart 完成後會列出接下來需要做的事：

```
=== 開始之前 ===

1. 編輯 fleet.yaml 中的 user_allowlist，填入你的 Telegram user ID
2. 確認 bot 在群組中擁有管理員權限
3. 確認群組已升級為超級群組（支援 topic 模式）

=== 啟動 ===

$ agend-terminal start        # 啟動 daemon
$ agend-terminal list          # 確認 agent 狀態
$ agend-terminal attach general  # 連接到 agent 終端
```

---

## 常見問題

### Q: 我沒有 Telegram，可以跳過嗎？

可以。在 Telegram 設定步驟選擇「跳過」，quickstart 會產生不含 channel
設定的 fleet.yaml。AgEnD 仍然可以正常運作，只是不會有 Telegram 通知。

### Q: 我想用 Discord 而不是 Telegram

quickstart 目前只支援 Telegram 的自動設定。Discord 需要手動在 fleet.yaml
中設定：

```yaml
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_TOKEN
  guild_id: "123456789"
```

### Q: 已經有 fleet.yaml 了，quickstart 會覆蓋嗎？

quickstart 偵測到現有 fleet.yaml 時會詢問是否覆蓋。選擇「否」會保留現有
檔案，quickstart 結束。

### Q: Token 格式驗證失敗怎麼辦？

合法的 Telegram bot token 格式是 `<8位以上的數字>:<30字元以上的英數字、底線或
連字號>`。請從 BotFather 完整複製整段 token，不要漏掉冒號前的數字部分。

### Q: 為什麼群組必須是超級群組？

AgEnD 使用 Telegram 的 topic（論壇主題）功能為每個 agent 建立獨立的對話串。
Topic 功能只有超級群組才支援。將一般群組轉換為超級群組的方式：
群組設定 → 啟用「Topics」。

---

## 技術細節

### 支援的 Backend

| Backend | 命令 | 預設參數 | Resume 支援 |
|---------|------|---------|------------|
| Claude Code | `claude` | `--dangerously-skip-permissions` | `--continue` |
| Kiro CLI | `kiro-cli` | `--dangerously-skip-permissions` | `--resume` |
| Codex | `codex` | （依版本） | 內建 |
| OpenCode | `opencode` | （無） | `--continue` |
| Gemini | `gemini` | （無） | `--resume latest` |
| Agy | `agy` | （無） | `--continue` |

### 檔案寫入位置

| 檔案 | 路徑 | 說明 |
|------|------|------|
| fleet.yaml | `$AGEND_HOME/fleet.yaml` | Agent 設定檔 |
| .env | `~/.env` | Bot token 環境變數 |

`$AGEND_HOME` 預設為 `~/.agend-terminal`。

### Token 安全

- Token 以環境變數名稱（`AGEND_BOT_TOKEN`）儲存在 fleet.yaml 中，
  而非明文 token 值
- 實際 token 值只存在 `~/.env` 檔案中
- `~/.env` 權限為 `0600`（Unix）
- quickstart 會檢查 `.gitignore` 涵蓋範圍，從工作目錄往上搜尋到根目錄，
  確保 `.env` 不會被 git 追蹤