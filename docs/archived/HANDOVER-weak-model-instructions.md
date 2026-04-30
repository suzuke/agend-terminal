# Handover: 弱模型 Instruction Following 問題

## 問題描述

agend-terminal 透過 CLI 指令讓 AI coding agents 互相通訊：

```bash
agend-terminal agent reply "text"        # 回覆 Telegram 用戶
agend-terminal agent send TARGET "text"  # 發訊息給其他 agent
agend-terminal agent inbox               # 查看收件匣
agend-terminal agent delegate/report/ask/broadcast/list/spawn/delete/...
```

**核心問題**：強模型（Claude Opus、Gemini 2.5 Pro）能正確使用這些 CLI 指令，但弱模型（Gemini 2.5 Flash、GPT-4o-mini、OpenCode 預設模型）無法穩定遵循 instructions，常見失敗模式：

1. **直接輸出純文字回覆**，不執行任何 shell command
2. **用錯指令**：收到 Telegram 訊息用 `send` 而非 `reply`
3. **格式錯誤**：描述「我會執行這個指令」但不實際執行
4. **只學會 reply**，其他 30+ 指令完全不使用

## 目前的 Instruction 設計（v6-cli）

位置：`src/instructions.rs` → `AGEND_RULES` 常數

目前策略：
- 6 個 `<example>` few-shot 範例（input → command → output）
- XML 標籤結構化
- 明確路由規則：`[user:... via telegram]` → reply、`[from:AGENT]` → send
- Command reference 列表
- 反覆強調「ALWAYS run a shell command, NEVER plain text」

這個版本是從 v3-mcp 經過多次迭代的結果，已經顯著改善了弱模型的 reply 能力，但其他指令仍然不被使用。

## 已探索的方向

### 1. Native Tool Registration（已放棄）

**想法**：為每個 CLI 注冊原生 tool/skill/extension，讓模型用 tool calling 而非讀 instructions。

| CLI | 機制 |
|-----|------|
| Claude Code | `.claude/skills/` |
| Gemini CLI | Extensions (MCP server) |
| OpenCode | Plugins (JS/TS) |
| Codex | Plugin packages |
| Kiro | Hooks only（不支援） |

**放棄原因**：
- 全域安裝污染用戶環境（不用 agend 時也被影響）
- Project-local 安裝跟寫 instruction markdown 本質無差別
- 如果只注冊 5 個高頻 tool，其他 30 個指令弱模型還是不會
- 如果注冊全部 37 個 tool，回到 MCP 的 context window 爆炸問題（72%）
- 弱模型的問題本質是 instruction following 能力不足，不是交付機制問題

### 2. API + CLI + Skills 三層架構

參考：https://blog.wu-boy.com/2026/04/api-cli-skills-architecture-for-ai-agents-zh-tw/

**架構**：
```
API (daemon socket) → CLI (agend-terminal agent) → Skills (workflow markdown)
```

**結論**：agend-terminal 已經有前兩層。第三層 "Skills" 對我們來說就是更好的 instructions，不需要額外框架。

### 3. AutoCrucible 自動優化（進行中，暫停）

**想法**：用 AutoCrucible（自動實驗平台）迭代優化 instruction 內容，以弱模型的 CLI 使用正確率作為 metric。

**專案位置**：`/Users/suzuke/Documents/Hack/optimize-agend-instructions/`

```
.crucible/config.yaml   # 實驗配置
.crucible/program.md    # 優化 agent 的指示
prompt.txt              # 可編輯的 instruction（被優化目標）
scenarios.json          # 15 個測試場景
evaluate.py             # 評估腳本：跑弱模型 → 檢查 CLI 使用正確性
examples.txt            # 範例（optimizer agent 的參考資料）
```

**暫停原因**：Gemini CLI 的 OAuth (Code Assist) 被 rate limit（429），無法批量呼叫做自動評估。

**恢復條件**：
- 等 Gemini API rate limit 恢復，或
- 設定 `GOOGLE_API_KEY`（Google AI Studio 免費版），改 evaluate.py 用 SDK 直呼 API，或
- 改用其他弱模型（Anthropic Haiku via API）做評估目標

## 未探索的方向

### A. 分層指令策略

不同能力的模型給不同複雜度的 instructions：
- **弱模型**：只教 `reply` 和 `send`（2 個指令），其他需求 delegate 給強模型
- **強模型**：完整 37 個指令

在 `instructions.rs` 可以根據 backend 產生不同內容。

### B. 結構化 prompt 研究

已知對弱模型有效的技巧（from mini-SWE-agent research）：
- Few-shot 範例比規則描述有效 10x
- 模型從 pattern 學習，不是從 description 學習
- Output format 要極度明確（包含完整 shell command + expected output）
- 用 XML tags 分隔結構

可以進一步研究：
- Chain-of-thought prompting（讓模型先判斷訊息類型再選指令）
- 更多 few-shot 覆蓋更多指令類型
- 強制 output format（「你的回覆必須以 `agend-terminal` 開頭」）

### C. Wrapper script 簡化介面

建立極簡 wrapper 降低弱模型的認知負擔：

```bash
# 不用記 agend-terminal agent reply，只要：
reply "text"
send dev "text"
inbox
```

在 agent 的 shell 環境注入 aliases 或 wrapper scripts。模型只需要記住單字指令。

### D. 強制機制

在 agent 的 PTY 輸出中檢測純文字回覆，自動注入提醒：
```
[system] You must use agend-terminal commands. Your plain text response was not delivered.
```

## 建議優先順序

1. **C (Wrapper script)** — 最小改動，可能最有效。弱模型記住 `reply "text"` 比 `agend-terminal agent reply "text"` 容易得多
2. **A (分層指令)** — 弱模型少學點，靠 delegate 補
3. **B (AutoCrucible)** — 恢復後跑，系統性找最佳 prompt
4. **D (強制機制)** — 作為 safety net
