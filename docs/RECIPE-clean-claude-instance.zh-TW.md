[English](RECIPE-clean-claude-instance.md)

# Recipe：啟動不繼承既有 context 的 Claude Code instance

有時候你會想要一個**不會**讀取 operator 全域 `~/.claude/CLAUDE.md` 指令、也不會
讀取任何累積的 auto-memory 的 Claude Code instance——例如要 A/B 比較 agent 行為、
進行隔離實驗，或是把一個乾淨的 instance 交給別人，又不想洩漏你的個人偏好設定。

這份 recipe 整理了 Claude Code 實際上會跨 session 繼承哪些東西，以及官方提供的
退出（opt out）開關。

## 會被繼承的內容

| 來源 | 範圍 | 預設 |
|--------|-------|---------|
| `~/.claude/CLAUDE.md` | 機器上的所有 session | 一律載入 |
| `~/.claude/projects/<pwd-slug>/memory/MEMORY.md`（以及 `memory/` 中的檔案） | 每個工作目錄 | 存在時載入 |
| `<pwd>/CLAUDE.md` 和 `<pwd>/.claude/CLAUDE.md` | 每個專案 | 存在時載入 |

agend-terminal 早已替每個受管 instance 配置自己的工作目錄，位於
`~/.agend-terminal/workspace/<instance-name>/`（或在指派分支時，位於專屬的 worktree
`~/.agend-terminal/worktrees/<…>/`）。由於 auto-memory 路徑是從 pwd slug 推導而來，
**每個 instance 都會自動取得自己的空白 auto-memory 目錄**——在這個面向上 instance
之間不會互相洩漏。

真正*會*跨 instance 洩漏的，是全域的 `~/.claude/CLAUDE.md`，以及先前執行時寫在該
instance 自己 pwd slug 底下的任何 auto-memory。

## 退出開關

### 1. 停用 auto-memory 載入（官方做法，推薦）

以下任一即可：

- 環境變數：`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`
- 設定檔：在 `<workspace>/.claude/settings.json` 中加入 `"autoMemoryEnabled": false`

這會停用整個 auto-memory 系統——同時關掉 session 開始時的載入*以及*寫入端，所以
instance 也不會再附加新的 memory 檔案。磁碟上既有的 memory 檔案不會被刪除，只是不
再被讀取或寫入。

### 2. 排除全域 CLAUDE.md（基於設定檔，部分有效）

在 `<workspace>/.claude/settings.json` 中：

```json
{
  "autoMemoryEnabled": false,
  "claudeMdExcludes": ["~/.claude/CLAUDE.md"]
}
```

Claude Code 目前沒有一級的 `--no-user-claude-md` 旗標；用明確路徑搭配
`claudeMdExcludes` 才是官方文件記載的逃生門。

### 3. 透過 `HOME` 隔離（最徹底，也最具破壞性）

在一個拋棄式的 `HOME` 底下啟動 instance：

```bash
HOME=/tmp/clean-claude-home claude
```

你真正的 `~/.claude/` 底下的任何東西都不會被讀取。代價是：你必須在這個拋棄式的
home 裡面重新配置所有 Claude Code 設定（MCP server、auth、佈景主題），instance 才能
派上用場。一般來說只有在進行對安全性敏感的測試，或要從零驗證預設行為時才值得。

## 在 agend-terminal 中套用

`create_instance` 目前不會注入環境變數，但它確實會在
`~/.agend-terminal/workspace/<name>/` 替每個 instance 配置一個 workspace。實務上的
做法如下：

1. 先選好你打算啟動的 instance 名稱——比方說 `clean-agent`。
2. 預先建立 workspace 並放入一個設定檔：
   ```bash
   mkdir -p ~/.agend-terminal/workspace/clean-agent/.claude
   cat > ~/.agend-terminal/workspace/clean-agent/.claude/settings.json <<'JSON'
   {
     "autoMemoryEnabled": false,
     "claudeMdExcludes": ["~/.claude/CLAUDE.md"]
   }
   JSON
   ```
3. 照常用 `create_instance(name="clean-agent", backend="claude")` 啟動。
   Claude Code 開機時會讀取 workspace 本地的 `settings.json`，並同時略過全域指令
   以及任何 auto-memory 載入。

如果你需要完整的 `HOME` 隔離，目前那必須在 agend-terminal 的啟動層級處理（在執行
`agend-terminal start` 之前設定 `HOME`）——`create_instance` 並沒有 per-instance 的
`env` 注入機制。

## 這份 recipe *不是*什麼

- 它不會移除 daemon 注入到每個 instance system prompt 中的 agend-terminal fleet
  協定。那是來自 daemon，不是來自 `~/.claude/`。
- 它不會阻止 MCP server 被掛載——那些是在 workspace 的 `mcp-config.json` 層級設定，
  而非由 CLAUDE.md 決定。
- 它不會回溯清除既有的 auto-memory；如果你想清除，請刪除對應 slug 的
  `~/.claude/projects/<pwd-slug>/memory/`。