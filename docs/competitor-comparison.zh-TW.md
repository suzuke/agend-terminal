[English](competitor-comparison.md)

# Competitor Comparison：AgEnD-Terminal vs Multica vs OpenAB

## 總覽

| 專案 | 定位 | Stars | 語言 |
|---------|-------------|-------|----------|
| **AgEnD-Terminal** | Operator 即時指揮系統——TUI 多 agent fleet 協調，具備即時控制與 review chain | — | Rust |
| **[Multica](https://github.com/multica-ai/multica)** | Agent HR 管理平台——看板式任務分派、進度追蹤、多使用者共享 agent pool | 32.2k | Go + TypeScript |
| **[OpenAB](https://github.com/openabdev/openab)** | 聊天優先的 ACP 橋接器——透過 stdio JSON-RPC 將 Discord/Slack 訊息路由到任何相容 ACP 的 coding CLI | 515 | Rust |

## 架構

```
AgEnD-Terminal:
┌─────────────┐
│  Operator   │ (TUI / Telegram)
└──────┬──────┘
       │ direct PTY control + MCP tools
┌──────┴──────┐
│   Daemon    │ (single process, file-based state)
├─────────────┤
│ agent-1..N  │ PTY sessions + inter-agent messaging
└─────────────┘

Multica:
┌──────────┐     ┌──────────┐     ┌────────────┐
│ Web UI   │────►│ Go Server│────►│ PostgreSQL │
│ Next.js  │     │ REST+WS  │     └────────────┘
└──────────┘     └────┬─────┘
                      │ poll/heartbeat
                 ┌────┴─────┐
                 │  Daemon  │ (per machine, PTY spawn)
                 └──────────┘

OpenAB:
┌──────────┐  WebSocket/webhook  ┌──────────┐  ACP (JSON-RPC/stdio)  ┌─────────────┐
│ Discord  │◄───────────────────►│  openab  │◄──────────────────────►│ coding CLI  │
│ Slack    │                     │  (Rust)  │                        │ (any ACP)   │
│ Telegram │                     └──────────┘                        └─────────────┘
└──────────┘                     Session Pool
```

## 功能比較表

| 功能 | AgEnD-Terminal | Multica | OpenAB |
|---------|---------------|---------|--------|
| **通訊協定** | PTY + MCP tools | REST + WebSocket | ACP（透過 stdio 的 JSON-RPC） |
| **聊天平台** | Telegram | 僅限 Web UI | Discord、Slack、Telegram、LINE、Feishu、Google Chat、WeCom |
| **多 agent 協調** | Fleet protocol（lead/dev/reviewer chain） | Squads + Leader 路由 | Bot 對 bot 訊息（基本） |
| **任務管理** | Task board + event sourcing | Issue board + 完整生命週期 | ❌ 無（僅 session pool） |
| **Git/分支管理** | 深度整合（worktree bind/release/gc） | ❌ 無 | ❌ 無 |
| **CI 整合** | CI watch + 自動通知 chain | 未提及 | ❌ 無 |
| **Review Chain** | ✅ 結構化（VERIFIED/REJECTED） | ❌ 無 | ❌ 無 |
| **即時控制** | ✅ interrupt/pane_snapshot/replace | ❌（分派後等待） | ❌（發出後不管） |
| **TUI 儀表板** | ✅ 多 pane、tab/split 佈局 | ❌ | ❌ |
| **部署模式** | 本地 daemon（零基礎設施） | Docker 自架 / SaaS | Kubernetes（雲原生） |
| **Session 管理** | 每個 agent 一個 worktree | 每個任務一個 workspace 目錄 | Pool（max_sessions、TTL） |
| **Cron/排程** | ✅ Cron + 單次 | ✅ Autopilots | ✅ 設定驅動的 cron |
| **支援的 Backend CLI** | 5-6（透過 PTY） | 11（透過 PTY） | 12+（透過 ACP） |
| **語音/多媒體** | 圖片（Telegram） | ❌ | STT、圖片、檔案 |
| **Skill 系統** | ✅ SKILL.md + skills-lock.json | ✅ .agents/skills/ | ❌ 無 |

## 設計理念比較

| | AgEnD-Terminal | Multica | OpenAB |
|---|---|---|---|
| **agent 是什麼？** | 特種部隊成員 | 員工 | 聊天機器人 |
| **控制模型** | 指揮官驅動 | 任務分派驅動 | 對話驅動 |
| **複雜度** | 高（operator → fleet → review chain → 結果） | 中（團隊 → agent pool → 回報） | 低（1 位使用者 → 1 個 session → 回應） |
| **目標使用者** | 單一進階使用者 | 團隊 PM + 工程師 | 任何 Discord/Slack 使用者 |
| **擴展方向** | 更深的 fleet 智慧 | 更多人、更多 agent | 更多聊天平台 |

## 獨特優勢

### AgEnD-Terminal

1. **即時 TUI 控制**——同時以多 pane 檢視所有 agent。`pane_snapshot` 即時取得終端輸出，`interrupt` 中止錯誤的工作，`replace_instance` 立即重置。沒有其他工具能提供這種程度的 operator-in-the-loop 控制。
2. **結構化 review chain**——VERIFIED/REJECTED 裁決協定、自動 dispatch reviewer、對 operator 透明的內部重試。
3. **深度 git/worktree 整合**——每個 agent 獨立分支隔離、daemon 管理的 bind/release 生命週期、自動 GC。
4. **agent 間直接訊息**——agent 之間可互相查詢/回報，無需經過中央伺服器。適合複雜協作（review chain、fixup loop）。
5. **零基礎設施**——不需要 PostgreSQL、不需要 Docker、不需要 web server。單一 binary daemon 搭配 file-based state。

### Multica

1. **產品完整度**——完整的 web UI、issue board、評論串、執行歷史、workspace 隔離。3,236 次 commit、75 個 release。
2. **多使用者 / 多 workspace**——為團隊而設計。多人可以將工作分派到共享的 agent pool。
3. **非技術人員易用性**——web UI 讓 PM 和非工程師也能與 agent 互動。
4. **生態系規模**——32k stars、活躍社群、完整文件。
5. **桌面 + 行動 + Web**——三平台覆蓋，共用元件架構。

### OpenAB

1. **ACP 協定標準化**——使用 Agent Client Protocol（透過 stdio 的 JSON-RPC）取代 PTY 取巧手法。結構化的 tool call、thinking 與 permission。
2. **聊天平台廣度**——透過統一的 gateway 架構支援 7 個平台（Discord/Slack/Telegram/LINE/Feishu/Google Chat/WeCom）。
3. **雲原生（K8s）**——Helm charts、PVC、各 backend 專屬的 Dockerfile。為多租戶擴展而設計。
4. **Edit-streaming**——隨著 token 抵達，每 1.5 秒即時更新 Discord 訊息。
5. **最低進入門檻**——在 Discord 中 @mention 一個 bot 就能使用 coding agent。終端使用者零設定。

## AgEnD 可以學習的地方

### 向 Multica 學習

- **Autopilot（schedule → auto-task → auto-assign）**——已部分完成；schedule 可以觸發訊息，但無法自動建立帶有生命週期追蹤的任務。
- **Workspace GC（僅清理 artifact）**——✅ 已實作。
- **每個 agent 的 timeout**——✅ 已實作。
- **Task metadata KV**——✅ 已實作。
- **開機孤兒清掃**——針對 daemon 已死亡的 in_progress 任務做 crash 復原。
- **最大並行 agent 數**——資源保護機制。

### 向 OpenAB 學習

- **ACP 協定支援**——隨著 coding CLI 逐漸收斂到 ACP（`--acp` 模式），AgEnD 僅依賴 PTY 的做法可能會錯失結構化輸出（tool call、thinking token、permission）。可考慮在 PTY 之外增加選用的 ACP backend 模式。
- **Session pool TTL + 最大 session 數**——比無上限地 spawn agent 更安全。
- **聊天平台的 gateway 架構**——OpenAB 獨立的 Custom Gateway 比把 Telegram 寫死在 daemon 裡更乾淨。AgEnD 已有 `channel/` 抽象層，但採用更可插拔的設計會更好。

## AgEnD 不該照抄的地方

| 來自 Multica | 為何不該 |
|---|---|
| Web UI | TUI 才是核心差異點；加上 web 會稀釋焦點 |
| PostgreSQL | 對單一 operator 使用情境來說，file-based state 才是正確選擇 |
| 多使用者驗證 | 設計上就是單一 operator |
| REST API | YAGNI——沒有第二個網路用戶端存在 |

| 來自 OpenAB | 為何不該 |
|---|---|
| K8s 部署 | 本地工具，不需要雲端基礎設施 |
| 以 Discord/Slack 作為主要 UX | AgEnD 不是聊天機器人 |
| 發出後不管的執行方式 | 違背了 operator 控制的初衷 |

## 總結

這三個專案各自佔據不同的抽象層：

- **OpenAB** = 通訊層（聊天 ↔ agent 橋接）
- **Multica** = 管理層（任務 → agent → 回報）
- **AgEnD-Terminal** = 控制層（operator → fleet → 品質關卡 → 結果）

AgEnD-Terminal 的護城河是 **即時 TUI 控制 + 結構化 fleet 協調**。這需要深度的 daemon-PTY 整合（`vterm.rs`、`pane_factory.rs`、`layout/`、`keybinds.rs`、`render/`），無法事後硬塞進其他架構——它從第一天起就是為此目的而設計的。